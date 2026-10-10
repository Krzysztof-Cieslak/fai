//! Compact data-cell metadata alongside the reference count; wide cells retain
//! the descriptor-based layout used by other heap objects.

use super::*;

/// Marks a compact constructor/record/tuple header.
pub const COMPACT_DATA: u64 = 1 << 63;
/// Bit position of a compact cell's field count (four bits).
pub const COMPACT_FIELDS_SHIFT: u32 = 59;
/// Bit position of a compact cell's scalar bitmap (eight bits).
pub const COMPACT_SCALARS_SHIFT: u32 = 51;
/// Bit position of a compact cell's constructor tag (eleven bits).
pub const COMPACT_TAG_SHIFT: u32 = 40;
/// First field of a compact data cell, following its combined header word.
pub const COMPACT_FIELDS_OFFSET: usize = 8;

/// Encodes a bounded data shape, excluding the reference count. Scalar bits
/// beyond the actual field count are immaterial and are discarded.
pub const fn compact_data_metadata(tag: u32, fields: usize, scalars: u64) -> Option<u64> {
    if fields == 0 || fields > 15 || tag > 2047 {
        return None;
    }
    let scalars = scalars & ((1u64 << fields) - 1);
    if scalars > 255 {
        return None;
    }
    Some(
        COMPACT_DATA
            | ((fields as u64) << COMPACT_FIELDS_SHIFT)
            | (scalars << COMPACT_SCALARS_SHIFT)
            | ((tag as u64) << COMPACT_TAG_SHIFT),
    )
}

/// Header bytes for a constructor of this shape (fields follow directly).
pub const fn data_header_size(tag: u32, fields: usize, scalars: u64) -> usize {
    if compact_data_metadata(tag, fields, scalars).is_some() {
        COMPACT_FIELDS_OFFSET
    } else {
        DATA_FIELDS_OFFSET
    }
}

/// Checked physical layout for a new data cell.
pub(super) struct DataLayout {
    /// Inline metadata, or a descriptor-based extended header.
    pub(super) metadata: Option<u64>,
    /// The first field byte offset.
    pub(super) fields_offset: usize,
    /// Complete allocation size.
    pub(super) size: usize,
}

impl DataLayout {
    pub(super) fn new(tag: i64, fields: usize, scalars: u64) -> Self {
        let metadata =
            u32::try_from(tag).ok().and_then(|tag| compact_data_metadata(tag, fields, scalars));
        let fields_offset =
            if metadata.is_some() { COMPACT_FIELDS_OFFSET } else { DATA_FIELDS_OFFSET };
        let size = fields
            .checked_mul(8)
            .and_then(|n| n.checked_add(fields_offset))
            .filter(|n| *n <= MAX_ALLOCATION_SIZE)
            .unwrap_or_else(|| fai_allocation_size_panic());
        Self { metadata, fields_offset, size }
    }

    /// # Safety
    /// `p` is exclusively owned storage of `self.size` bytes; `desc` is the
    /// initialized descriptor for an extended cell.
    pub(super) unsafe fn initialize(&self, p: *mut u8, desc: *const Descriptor, tag: i64) {
        // SAFETY: compact and extended writes stay within their selected header.
        unsafe {
            write_u64(p, RC_OFFSET, self.metadata.unwrap_or(0) | 1);
            if self.metadata.is_none() {
                write_ptr(p, DESC_OFFSET, desc.cast());
                write_u64(p, SIZE_OFFSET, self.size as u64);
                write_u64(p, DATA_TAG_OFFSET, tag as u64);
            }
        }
    }
}

/// Immutable shape metadata read once for a complete structural operation.
pub(super) struct DataShape {
    /// Logical constructor tag.
    pub(super) tag: u64,
    /// Initialized field count.
    pub(super) fields: usize,
    /// Raw Float slots; every other field is a uniform value.
    pub(super) scalars: u64,
    /// Byte offset of the first field.
    pub(super) offset: usize,
}

impl DataShape {
    /// # Safety
    /// `p` is a live initialized data cell, retained throughout the operation.
    #[inline]
    pub(super) unsafe fn read(p: *const u8) -> Self {
        // SAFETY: both layouts describe this live cell's immutable shape. Compact
        // metadata shares the RC word, so take one atomic snapshot of that word.
        unsafe {
            let word = header_word(p);
            if word & COMPACT_DATA != 0 {
                Self {
                    tag: (word >> COMPACT_TAG_SHIFT) & 2047,
                    fields: ((word >> COMPACT_FIELDS_SHIFT) & 15) as usize,
                    scalars: (word >> COMPACT_SCALARS_SHIFT) & 255,
                    offset: COMPACT_FIELDS_OFFSET,
                }
            } else {
                Self {
                    tag: read_u64(p, DATA_TAG_OFFSET),
                    fields: (read_u64(p, SIZE_OFFSET) as usize - DATA_FIELDS_OFFSET) / 8,
                    scalars: desc_scalar_bitmap(obj_descriptor(p)),
                    offset: DATA_FIELDS_OFFSET,
                }
            }
        }
    }
}

/// Reads immutable metadata through the same atomic word as a shared count.
///
/// # Safety
/// `p` is a live or exclusively held reset object with an aligned header.
#[inline]
pub(super) unsafe fn header_word(p: *const u8) -> u64 {
    // SAFETY: a header is aligned and live; shared count changes are atomic.
    unsafe { rc_atomic(p.cast_mut()).load(Ordering::Relaxed) }
}

/// Object kind, including compact data whose second word is already a field.
///
/// # Safety
/// `p` points to an initialized object header.
#[inline]
pub(super) unsafe fn object_kind(p: *const u8) -> u64 {
    // SAFETY: compact metadata is in the header; other objects have a descriptor.
    unsafe {
        if header_word(p) & COMPACT_DATA != 0 { KIND_DATA } else { desc_kind(obj_descriptor(p)) }
    }
}

/// Allocation size of either a compact cell or a descriptor-based object.
///
/// # Safety
/// `p` points to an initialized object or a reset cell retaining its metadata.
#[inline]
pub(super) unsafe fn object_size(p: *const u8) -> usize {
    // SAFETY: the selected size representation belongs to this header form.
    unsafe {
        let word = header_word(p);
        if word & COMPACT_DATA != 0 {
            COMPACT_FIELDS_OFFSET + (((word >> COMPACT_FIELDS_SHIFT) & 15) as usize) * 8
        } else {
            read_u64(p, SIZE_OFFSET) as usize
        }
    }
}

/// First field offset for either data layout.
///
/// # Safety
/// `p` is a data cell or a reset data cell.
#[inline]
pub(super) unsafe fn data_offset(p: *const u8) -> usize {
    // SAFETY: the header remains initialized throughout a reset token's lifetime.
    if unsafe { header_word(p) } & COMPACT_DATA != 0 {
        COMPACT_FIELDS_OFFSET
    } else {
        DATA_FIELDS_OFFSET
    }
}

/// Number of fields in a data cell.
///
/// # Safety
/// `p` is an initialized data cell.
#[inline]
pub(super) unsafe fn data_len(p: *const u8) -> usize {
    // SAFETY: both layouts encode the complete initialized field count.
    unsafe {
        let word = header_word(p);
        if word & COMPACT_DATA != 0 {
            ((word >> COMPACT_FIELDS_SHIFT) & 15) as usize
        } else {
            (read_u64(p, SIZE_OFFSET) as usize - DATA_FIELDS_OFFSET) / 8
        }
    }
}

/// Constructor tag of a data cell.
///
/// # Safety
/// `p` is an initialized data cell.
#[inline]
pub(super) unsafe fn data_tag(p: *const u8) -> u64 {
    // SAFETY: the tag is selected from the cell's actual header form.
    unsafe {
        let word = header_word(p);
        if word & COMPACT_DATA != 0 {
            (word >> COMPACT_TAG_SHIFT) & 2047
        } else {
            read_u64(p, DATA_TAG_OFFSET)
        }
    }
}

/// Scalar-slot bitmap of a data cell.
///
/// # Safety
/// `p` is an initialized data cell.
#[inline]
pub(super) unsafe fn data_scalars(p: *const u8) -> u64 {
    // SAFETY: compact cells encode the bitmap; extended cells retain a descriptor.
    unsafe {
        let word = header_word(p);
        if word & COMPACT_DATA != 0 {
            (word >> COMPACT_SCALARS_SHIFT) & 255
        } else {
            desc_scalar_bitmap(obj_descriptor(p))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::lock;

    static NINTH_FLOAT: Descriptor = Descriptor {
        kind: KIND_DATA,
        scalar_bitmap: 1 << 8,
        name_ptr: std::ptr::null(),
        name_len: 0,
    };
    static NINE_FLOATS: Descriptor = Descriptor {
        kind: KIND_DATA,
        scalar_bitmap: (1 << 9) - 1,
        name_ptr: std::ptr::null(),
        name_len: 0,
    };

    fn data(tag: i64, fields: &[Value]) -> Value {
        // SAFETY: ownership of every supplied field transfers into the cell.
        unsafe { fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
    }

    fn float_field(value: Value, index: i64) -> f64 {
        let field = fai_data_field(value, index);
        let number = read_float(field);
        fai_drop(field);
        number
    }

    #[test]
    fn two_field_cell_occupies_three_words() {
        let _guard = lock();
        let baseline = live_count();
        let value = data(1, &[imm_int(10), imm_int(20)]);
        // SAFETY: this is an initialized, exclusively owned data cell.
        unsafe {
            assert_eq!(object_size(as_obj(value)), 24);
            assert_eq!(data_offset(as_obj(value)), 8);
            assert_eq!(data_len(as_obj(value)), 2);
            assert_eq!(data_tag(as_obj(value)), 1);
            assert_eq!(rc_load(as_obj(value)), 1);
        }
        fai_drop(value);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn maximum_compact_tag_and_field_count_round_trip() {
        let _guard = lock();
        let baseline = live_count();
        let value = data(2047, &[imm_int(1); 15]);
        // SAFETY: the complete initialized shape is encoded in the compact header.
        unsafe {
            assert_eq!(object_size(as_obj(value)), 128);
            assert_eq!(data_offset(as_obj(value)), 8);
            assert_eq!(data_len(as_obj(value)), 15);
            assert_eq!(data_tag(as_obj(value)), 2047);
        }
        fai_drop(value);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn large_tag_retains_the_extended_header() {
        let _guard = lock();
        let baseline = live_count();
        let value = data(2048, &[imm_int(1)]);
        // SAFETY: the tag exceeds the compact range and remains in the full header.
        unsafe {
            assert_eq!(object_size(as_obj(value)), 40);
            assert_eq!(data_offset(as_obj(value)), DATA_FIELDS_OFFSET);
            assert_eq!(data_tag(as_obj(value)), 2048);
        }
        fai_drop(value);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn sixteen_fields_retain_the_extended_header() {
        let _guard = lock();
        let baseline = live_count();
        let value = data(0, &[imm_int(1); 16]);
        // SAFETY: this initialized shape has more fields than the compact count.
        unsafe {
            assert_eq!(object_size(as_obj(value)), 160);
        }
        fai_drop(value);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn record_update_can_expand_a_compact_header() {
        let _guard = lock();
        let baseline = live_count();
        let mut fields = [imm_int(1); 9];
        fields[8] = fai_box_float(1.5f64.to_bits() as i64);
        let value = data(0, &fields);
        let updated = fai_record_update(value, imm_int(8), fai_box_float(2.5f64.to_bits() as i64));
        // SAFETY: the ninth raw Float needs a bitmap bit outside the compact mask.
        unsafe {
            assert_eq!(object_size(as_obj(updated)), 104);
        }
        assert_eq!(float_field(updated, 8), 2.5);
        fai_drop(updated);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn record_update_can_compact_an_extended_header() {
        let _guard = lock();
        let baseline = live_count();
        let mut fields = [imm_int(1); 9];
        fields[8] = 1.5f64.to_bits() as i64;
        // SAFETY: the ninth slot is raw Float bits and the other slots are owned immediates.
        let value = unsafe { fai_make_data_scalar(&NINTH_FLOAT, 0, 9, fields.as_ptr()) };
        let updated = fai_record_update(value, imm_int(8), imm_int(42));
        // SAFETY: no scalar fields remain, so the nine-field cell is compact.
        unsafe {
            assert_eq!(object_size(as_obj(updated)), 80);
        }
        let field = fai_data_field(updated, 8);
        assert_eq!(read_int(field), 42);
        fai_drop(field);
        fai_drop(updated);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn structural_operations_agree_across_header_forms() {
        let _guard = lock();
        let baseline = live_count();
        let raw = [1.25f64.to_bits() as i64; 9];
        let boxed: Vec<_> = raw.iter().map(|&bits| fai_box_float(bits)).collect();
        let compact = data(0, &boxed);
        // SAFETY: all nine slots are raw Float bits under the matching descriptor.
        let extended = unsafe { fai_make_data_scalar(&NINE_FLOATS, 0, 9, raw.as_ptr()) };
        assert!(values_equal(compact, extended));
        assert_eq!(values_compare(compact, extended), std::cmp::Ordering::Equal);
        assert_eq!(values_hash(compact), values_hash(extended));
        fai_drop(compact);
        fai_drop(extended);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn compact_destruction_recycles_root_and_descendant_cells() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let child = data(1, &[imm_int(10), imm_int(20)]);
        let root = data(2, &[child, imm_int(30)]);
        fai_drop(root);
        assert_eq!((live_count(), live_bytes()), baseline);
        let next_child = data(3, &[imm_int(40), imm_int(50)]);
        let next_root = data(4, &[next_child, imm_int(60)]);
        assert_eq!(next_child, child);
        assert_eq!(next_root, root);
        fai_drop(next_root);
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn compact_destruction_skips_float_slots_and_retains_shared_children() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let child = make_str("retained");
        let fields = [(-0.0f64).to_bits() as i64, fai_dup(child)];
        let descriptor = intern_data_descriptor(1);
        // SAFETY: the first field is raw Float bits; the second transfers one
        // owned reference to the string, matching the descriptor bitmap.
        let root = unsafe { fai_make_data_scalar(descriptor, 0, 2, fields.as_ptr()) };
        fai_drop(root);
        assert_eq!(read_string(child), b"retained");
        fai_drop(child);
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn borrowed_uniform_field_adds_no_reference() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let text = make_str("borrowed");
        let cell = data(1, &[text]);
        let field = fai_data_peek(cell, imm_int(0));
        assert_eq!(field, text);
        // SAFETY: the containing cell owns the string throughout this borrow.
        assert_eq!(unsafe { rc_load(as_obj(field)) }, 1);
        assert_eq!(read_string(field), b"borrowed");
        fai_drop(cell);
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn extended_parent_drains_shared_compact_children() {
        let _guard = lock();
        let baseline = (live_count(), live_bytes());
        let child = data(1, &[imm_int(10), imm_int(20)]);
        let root = data(2048, &[child, fai_dup(child)]);
        fai_drop(root);
        assert_eq!((live_count(), live_bytes()), baseline);
    }

    #[test]
    fn unused_scalar_bits_do_not_prevent_compaction() {
        let _guard = lock();
        let baseline = live_count();
        let fields = [f64::NAN.to_bits() as i64; 8];
        // SAFETY: all eight live slots are raw floats. The descriptor's ninth bit
        // is outside the object and does not describe an additional field.
        let value = unsafe { fai_make_data_scalar(&NINE_FLOATS, 0, 8, fields.as_ptr()) };
        // SAFETY: the initialized cell's header encodes only its live slots.
        unsafe {
            assert_eq!(object_size(as_obj(value)), 72);
            assert_eq!(data_scalars(as_obj(value)), 255);
        }
        fai_drop(value);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn an_extended_token_can_rebuild_as_a_compact_cell() {
        let _guard = lock();
        let baseline = live_count();
        let value = data(2048, &[imm_int(1)]);
        let token = fai_drop_reuse(value);
        let fields = [imm_int(2); 4];
        // SAFETY: the token is reset and both layouts occupy forty bytes.
        let rebuilt = unsafe { fai_reuse(token, 1, 4, fields.as_ptr()) };
        assert_eq!(rebuilt, value);
        // SAFETY: the rebuilt object has its new compact metadata.
        unsafe {
            assert_eq!(data_len(as_obj(rebuilt)), 4);
        }
        fai_drop(rebuilt);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn a_compact_token_can_rebuild_as_an_extended_cell() {
        let _guard = lock();
        let baseline = live_count();
        let value = data(1, &[imm_int(2); 4]);
        let token = fai_drop_reuse(value);
        let fields = [imm_int(1)];
        // SAFETY: the token is reset and both layouts occupy forty bytes.
        let rebuilt = unsafe { fai_reuse(token, 2048, 1, fields.as_ptr()) };
        assert_eq!(rebuilt, value);
        // SAFETY: the rebuilt object has its new extended metadata.
        unsafe {
            assert_eq!(data_tag(as_obj(rebuilt)), 2048);
        }
        fai_drop(rebuilt);
        assert_eq!(live_count(), baseline);
    }

    #[test]
    fn large_counts_do_not_change_compact_metadata() {
        let _guard = lock();
        let baseline = live_count();
        let value = data(17, &[imm_int(1)]);
        // SAFETY: the test exclusively owns this cell and restores its real count
        // before dropping it. This models the boundary without allocating edges.
        unsafe {
            let p = as_obj(value);
            let metadata = header_word(p) & !RC_STATE_MASK;
            write_u64(p, RC_OFFSET, metadata | (MAX_REFCOUNT - 1));
            fai_dup(value);
            assert_eq!(rc_load(p), MAX_REFCOUNT);
            assert_eq!(data_tag(p), 17);
            assert_eq!(data_len(p), 1);
            write_u64(p, RC_OFFSET, metadata | 1);
        }
        fai_drop(value);
        assert_eq!(live_count(), baseline);
    }

    #[cfg(not(miri))]
    #[test]
    fn overflow_worker() {
        let Ok(mode) = std::env::var("FAI_RC_LIMIT_TEST") else { return };
        let value = data(17, &[imm_int(1)]);
        // SAFETY: the subprocess exclusively owns the cell and intentionally
        // models the count limit; the following duplicate must abort.
        unsafe {
            let p = as_obj(value);
            let metadata = header_word(p) & !RC_STATE_MASK;
            let flag = if mode == "shared" { MT_FLAG } else { 0 };
            write_u64(p, RC_OFFSET, metadata | flag | MAX_REFCOUNT);
        }
        fai_dup(value);
        std::process::exit(0);
    }

    #[cfg(not(miri))]
    fn assert_overflow(mode: &str) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "data_header::tests::overflow_worker", "--nocapture"])
            .env("FAI_RC_LIMIT_TEST", mode)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("reference count exceeds supported range")
        );
    }

    #[cfg(not(miri))]
    #[test]
    fn local_count_overflow_is_reported_before_metadata_can_change() {
        assert_overflow("local");
    }

    #[cfg(not(miri))]
    #[test]
    fn shared_count_overflow_is_reported_before_metadata_can_change() {
        assert_overflow("shared");
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]
            #[test]
            fn structural_operations_ignore_raw_or_boxed_field_layout(bits in prop::collection::vec(any::<u64>(), 1..21), tag in 0i64..4096) {
                let _guard = lock();
                let baseline = (live_count(), live_bytes());
                let raw: Vec<_> = bits.iter().map(|bits| *bits as i64).collect();
                let boxed: Vec<_> = raw.iter().map(|bits| fai_box_float(*bits)).collect();
                let uniform = data(tag, &boxed);
                let descriptor = intern_data_descriptor((1 << raw.len()) - 1);
                // SAFETY: every initialized field contains raw Float bits, matching
                // the immutable descriptor's scalar bitmap and the field count.
                let scalar = unsafe {
                    fai_make_data_scalar(descriptor, tag, raw.len() as i64, raw.as_ptr())
                };
                prop_assert!(values_equal(uniform, scalar));
                prop_assert_eq!(values_compare(uniform, scalar), std::cmp::Ordering::Equal);
                prop_assert_eq!(values_compare(scalar, uniform), std::cmp::Ordering::Equal);
                prop_assert_eq!(values_hash(uniform), values_hash(scalar));
                fai_drop(uniform);
                fai_drop(scalar);
                prop_assert_eq!((live_count(), live_bytes()), baseline);
            }
        }
    }
}
