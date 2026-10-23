//! Immutable ordered text pairs with list-backed and indexed UTF-8 storage.
//!
//! Both representations have the same value equality, ordering and hashing.
//! Indexed values own their text and ranges; no cursor borrows a connection or
//! mutable parser buffer. The two owned children use the ordinary RC graph walk.

use std::cmp::Ordering;

use crate::{Value, fai_drop, fai_dup, imm_int, is_boxed};

pub(crate) const SOURCE_OFFSET: usize = crate::HEADER_SIZE;
pub(crate) const RANGES_OFFSET: usize = SOURCE_OFFSET + 8;

fn children(value: Value) -> (Value, Value) {
    // SAFETY: typed text-pair operands have this two-child descriptor layout.
    unsafe {
        let p = crate::as_obj(value);
        debug_assert_eq!(crate::object_kind(p), crate::KIND_TEXT_PAIRS);
        (crate::read_i64(p, SOURCE_OFFSET), crate::read_i64(p, RANGES_OFFSET))
    }
}

fn make(source: Value, ranges: Value) -> Value {
    let p = crate::alloc_obj(RANGES_OFFSET + 8, &crate::FAI_TEXT_PAIRS_DESC);
    // SAFETY: the allocated cell has two initialized slots taking these owners.
    unsafe {
        crate::write_i64(p, SOURCE_OFFSET, source);
        crate::write_i64(p, RANGES_OFFSET, ranges);
    }
    crate::from_obj(p)
}

fn data(tag: i64, fields: &[Value]) -> Value {
    // SAFETY: the slice contains initialized owned values transferred to the cell.
    unsafe { crate::fai_make_data(tag, fields.len() as i64, fields.as_ptr()) }
}

unsafe fn ranges<'a>(value: Value) -> &'a [Value] {
    // SAFETY: this helper is used only while a typed, owned Array Int remains
    // live. The returned slice is confined to that owner's native call.
    unsafe {
        std::slice::from_raw_parts(
            crate::as_obj(value).add(crate::ARRAY_ELEMS_OFFSET).cast(),
            crate::array_len(value),
        )
    }
}

#[derive(Clone, Copy)]
struct Text<'a> {
    text: &'a str,
    value: Option<Value>,
}

impl Text<'_> {
    fn owned(self) -> Value {
        self.value.map_or_else(|| crate::make_string(self.text.as_bytes()), |value| fai_dup(value))
    }
}

enum Pairs<'a> {
    List(Value, std::marker::PhantomData<&'a Value>),
    Indexed(&'a str, std::slice::ChunksExact<'a, Value>),
}

fn pairs(owner: &Value) -> Pairs<'_> {
    let (source, index) = children(*owner);
    if is_boxed(index) {
        // SAFETY: indexed storage owns a String and validated ranges for it.
        unsafe { Pairs::Indexed(crate::string_str(source), ranges(index).chunks_exact(4)) }
    } else {
        Pairs::List(source, std::marker::PhantomData)
    }
}

impl<'a> Iterator for Pairs<'a> {
    type Item = (Text<'a>, Text<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::List(cursor, _) => {
                if !is_boxed(*cursor) {
                    return None;
                }
                // SAFETY: the root owner keeps the typed list, tuple and Strings
                // live through iteration. Projections borrow their existing slots.
                unsafe {
                    let cell = crate::as_obj(*cursor);
                    let offset = crate::data_offset(cell);
                    let pair = crate::as_obj(crate::read_i64(cell, offset));
                    *cursor = crate::read_i64(cell, offset + 8);
                    let offset = crate::data_offset(pair);
                    let first = crate::read_i64(pair, offset);
                    let second = crate::read_i64(pair, offset + 8);
                    Some((
                        Text { text: crate::string_str(first), value: Some(first) },
                        Text { text: crate::string_str(second), value: Some(second) },
                    ))
                }
            }
            Self::Indexed(text, index) => index.next().map(|quad| {
                let at = |i: usize| crate::unbox_int(quad[i]) as usize;
                (
                    Text { text: &text[at(0)..at(1)], value: None },
                    Text { text: &text[at(2)..at(3)], value: None },
                )
            }),
        }
    }
}

/// Compare logical pairs, retaining spelling, duplicate order and empty fields.
pub(crate) fn compare(a: Value, b: Value) -> Ordering {
    pairs(&a).map(|(a, b)| (a.text, b.text)).cmp(pairs(&b).map(|(a, b)| (a.text, b.text)))
}

/// Hash logical pair contents without observing storage form or source offsets.
pub(crate) fn hash(value: Value) -> u64 {
    let mut hash = crate::mix64(0x7465_7874_7061_6972);
    let mut count = 0u64;
    for (first, second) in pairs(&value) {
        hash = crate::hash_combine(hash, crate::hash_bytes(first.text.as_bytes()));
        hash = crate::hash_combine(hash, crate::hash_bytes(second.text.as_bytes()));
        count += 1;
    }
    crate::mix64(hash ^ count.rotate_left(17))
}

/// Own an existing list of String pairs without copying its fields.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_from_list(list: Value) -> Value {
    make(list, imm_int(0))
}

/// Own checked UTF-8 ranges, four offsets per pair. All operands are consumed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_from_ranges(text: Value, index: Value) -> Value {
    // SAFETY: the typed operands are a String and an Array Int, owned for the call.
    let input = unsafe { crate::string_str(text) };
    // SAFETY: the typed index array remains owned until validation is complete.
    let offsets = unsafe { ranges(index) };
    let valid = offsets.len().is_multiple_of(4)
        && offsets.chunks_exact(2).all(|range| {
            let start = crate::unbox_int(range[0]);
            let end = crate::unbox_int(range[1]);
            start >= 0
                && end >= start
                && (end as u64) <= input.len() as u64
                && input.is_char_boundary(start as usize)
                && input.is_char_boundary(end as usize)
        });
    if valid {
        data(0, &[make(text, index)])
    } else {
        fai_drop(text);
        fai_drop(index);
        data(1, &[crate::make_string(b"invalid text-pair ranges")])
    }
}

/// Materialize ordered pairs, transferring an existing list when available.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_to_list(value: Value) -> Value {
    let (source, index) = children(value);
    let result = if !is_boxed(index) {
        fai_dup(source)
    } else {
        // SAFETY: the text remains owned by value and construction checked all
        // ranges. Each output String owns its bytes independently of that text.
        let text = unsafe { crate::string_str(source) };
        let mut list = imm_int(crate::NIL_TAG);
        // SAFETY: value retains this index array through the complete traversal.
        for quad in unsafe { ranges(index) }.chunks_exact(4).rev() {
            let at = |i: usize| crate::unbox_int(quad[i]) as usize;
            let name = crate::make_string(&text.as_bytes()[at(0)..at(1)]);
            let field = crate::make_string(&text.as_bytes()[at(2)..at(3)]);
            list = data(crate::CONS_TAG, &[data(0, &[name, field]), list]);
        }
        list
    };
    fai_drop(value);
    result
}

fn name_matches(name: &str, lower: &str) -> bool {
    if name.is_ascii() { name.eq_ignore_ascii_case(lower) } else { name.to_lowercase() == lower }
}

/// Find the first value for a lowercased name, borrowing both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_get_borrowed(value: Value, lower: Value) -> Value {
    // SAFETY: lower is a live, typed String borrowed through this call.
    let lower = unsafe { crate::string_str(lower) };
    for (name, field) in pairs(&value) {
        if name_matches(name.text, lower) {
            return data(1, &[field.owned()]);
        }
    }
    imm_int(0)
}

/// Find the first value for a lowercased name, consuming both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_get(value: Value, lower: Value) -> Value {
    let result = fai_text_pairs_get_borrowed(value, lower);
    fai_drop(value);
    fai_drop(lower);
    result
}

/// Find every matching value in order, borrowing both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_get_all_borrowed(value: Value, lower: Value) -> Value {
    // SAFETY: lower is a live, typed String borrowed through this call.
    let lower = unsafe { crate::string_str(lower) };
    let fields: Vec<_> = pairs(&value)
        .filter(|(name, _)| name_matches(name.text, lower))
        .map(|(_, field)| field.owned())
        .collect();
    crate::cons_list(&fields)
}

/// Find every matching value in order, consuming both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_get_all(value: Value, lower: Value) -> Value {
    let result = fai_text_pairs_get_all_borrowed(value, lower);
    fai_drop(value);
    fai_drop(lower);
    result
}

/// Inspect emptiness without materializing any field or consuming the owner.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_is_empty_borrowed(value: Value) -> Value {
    let (source, index) = children(value);
    // SAFETY: the index, when present, is an array retained by the borrowed value.
    crate::from_bool(if is_boxed(index) {
        unsafe { crate::array_len(index) == 0 }
    } else {
        !is_boxed(source)
    })
}

/// Inspect emptiness and consume the owner.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_is_empty(value: Value) -> Value {
    let result = fai_text_pairs_is_empty_borrowed(value);
    fai_drop(value);
    result
}

fn row_size(first: usize, separator: usize, second: usize, ending: usize) -> Option<usize> {
    first.checked_add(separator)?.checked_add(second)?.checked_add(ending)
}

struct LabelledPairs<'a> {
    cursor: Value,
    selected: u64,
    owner: std::marker::PhantomData<&'a Value>,
}

impl<'a> Iterator for LabelledPairs<'a> {
    type Item = (&'a [u8], &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        while is_boxed(self.cursor) {
            // SAFETY: the borrowed root owns this typed List (Int * String *
            // String), including every tuple and String reached by the cursor.
            unsafe {
                let cell = crate::as_obj(self.cursor);
                let offset = crate::data_offset(cell);
                let row = crate::as_obj(crate::read_i64(cell, offset));
                self.cursor = crate::read_i64(cell, offset + 8);
                let offset = crate::data_offset(row);
                let label = crate::unbox_int(crate::read_i64(row, offset));
                if (0..64).contains(&label) && self.selected & (1 << label) != 0 {
                    return Some((
                        crate::string_bytes(crate::read_i64(row, offset + 8)),
                        crate::string_bytes(crate::read_i64(row, offset + 16)),
                    ));
                }
            }
        }
        None
    }
}

// SAFETY: rows must produce the same immutable fields on both traversals.
unsafe fn encode_rows<'a, I: Iterator<Item = (&'a [u8], &'a [u8])>>(
    prefix: &[u8],
    separator: &[u8],
    ending: &[u8],
    suffix: &[u8],
    reverse: bool,
    rows: impl Fn() -> I,
) -> Value {
    let mut length = prefix
        .len()
        .checked_add(suffix.len())
        .unwrap_or_else(|| crate::fai_allocation_size_panic());
    for (first, second) in rows() {
        let row = row_size(first.len(), separator.len(), second.len(), ending.len())
            .unwrap_or_else(|| crate::fai_allocation_size_panic());
        length = length.checked_add(row).unwrap_or_else(|| crate::fai_allocation_size_panic());
    }
    let size = crate::checked_buffer_size(crate::STRING_BYTES_OFFSET, length, 1)
        .unwrap_or_else(|| crate::fai_allocation_size_panic());
    if length == 0 {
        return crate::make_bytes(&[]);
    }
    let output = crate::alloc_obj(size, &crate::FAI_BYTES_DESC);
    // SAFETY: the checked size covers the header and every output byte. Both
    // traversals borrow the same immutable inputs, so row sizes cannot change.
    // The fresh output overlaps none of them. Reverse order fills complete rows
    // backwards, preserving each row's internal byte order without a scratch list.
    unsafe {
        crate::write_u64(output, crate::STRING_LEN_OFFSET, length as u64);
        let payload = output.add(crate::STRING_BYTES_OFFSET);
        std::ptr::copy_nonoverlapping(prefix.as_ptr(), payload, prefix.len());
        std::ptr::copy_nonoverlapping(
            suffix.as_ptr(),
            payload.add(length - suffix.len()),
            suffix.len(),
        );
        let mut position = if reverse { length - suffix.len() } else { prefix.len() };
        for (first, second) in rows() {
            let size = first.len() + separator.len() + second.len() + ending.len();
            let start = if reverse { position - size } else { position };
            let mut cursor = start;
            for part in [first, separator, second, ending] {
                std::ptr::copy_nonoverlapping(part.as_ptr(), payload.add(cursor), part.len());
                cursor += part.len();
            }
            position = if reverse { start } else { cursor };
        }
        debug_assert_eq!(position, if reverse { prefix.len() } else { length - suffix.len() });
        crate::from_obj(output)
    }
}

/// Encode ordered String pairs as prefix, terminated rows, and suffix. Each row
/// is its first field, separator, second field and ending. All operands are
/// borrowed; nonempty output owns one checked Bytes allocation without row
/// buffers, while empty output uses the shared empty Bytes value.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_render_list_borrowed(
    prefix: Value,
    separator: Value,
    ending: Value,
    suffix: Value,
    fields: Value,
) -> Value {
    // SAFETY: the typed Strings and List (String * String) remain borrowed for
    // both traversals. String readers also support immutable substring views.
    unsafe {
        encode_rows(
            crate::string_bytes(prefix),
            crate::string_bytes(separator),
            crate::string_bytes(ending),
            crate::string_bytes(suffix),
            false,
            || {
                Pairs::List(fields, std::marker::PhantomData)
                    .map(|(first, second)| (first.text.as_bytes(), second.text.as_bytes()))
            },
        )
    }
}

/// Encode ordered String pairs, consuming all five operands after copying them.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_render_list(
    prefix: Value,
    separator: Value,
    ending: Value,
    suffix: Value,
    fields: Value,
) -> Value {
    let output = fai_text_pairs_render_list_borrowed(prefix, separator, ending, suffix, fields);
    for value in [prefix, separator, ending, suffix, fields] {
        fai_drop(value);
    }
    output
}

/// Encode selected labelled rows in forward or reverse order. Label `n` is
/// selected by mask bit `n`; labels outside 0..64 are omitted. Strings and rows
/// are borrowed, while the mask and direction operands are consumed.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_render_rows_borrowed(
    prefix: Value,
    separator: Value,
    ending: Value,
    suffix: Value,
    selected: Value,
    reverse: Value,
    fields: Value,
) -> Value {
    let mask = crate::unbox_int(selected) as u64;
    let backwards = reverse == crate::from_bool(true);
    fai_drop(selected);
    fai_drop(reverse);
    // SAFETY: all Strings and the typed labelled-row list remain borrowed for
    // both traversals. Only copied byte contents escape in the independent result.
    unsafe {
        encode_rows(
            crate::string_bytes(prefix),
            crate::string_bytes(separator),
            crate::string_bytes(ending),
            crate::string_bytes(suffix),
            backwards,
            || LabelledPairs { cursor: fields, selected: mask, owner: std::marker::PhantomData },
        )
    }
}

/// Encode selected labelled rows, consuming all operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_text_pairs_render_rows(
    prefix: Value,
    separator: Value,
    ending: Value,
    suffix: Value,
    selected: Value,
    reverse: Value,
    fields: Value,
) -> Value {
    let output = fai_text_pairs_render_rows_borrowed(
        prefix, separator, ending, suffix, selected, reverse, fields,
    );
    for value in [prefix, separator, ending, suffix, fields] {
        fai_drop(value);
    }
    output
}

#[cfg(test)]
mod tests;
