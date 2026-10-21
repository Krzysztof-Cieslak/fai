//! Scalar/vector agreement, exact stopping positions and slice-boundary reads.

use super::prefix;

const LETTER_A: [u64; 4] = [0, 1 << (b'a' - 64), 0, 0];

#[track_caller]
fn rejected_at(length: usize, index: usize) {
    let mut bytes = vec![b'a'; length];
    bytes[index] = b'b';
    assert_eq!(prefix(&bytes, LETTER_A), index);
}

#[test]
fn empty_input_has_an_empty_prefix() {
    assert_eq!(prefix(&[], [u64::MAX; 4]), 0);
}

#[test]
fn rejection_at_the_scalar_prefix_end_stops_before_vector_setup() {
    rejected_at(160, 31);
}

#[test]
fn rejection_at_the_first_vector_byte_is_exact() {
    rejected_at(160, 32);
}

#[test]
fn rejection_at_the_last_vector_lane_is_exact() {
    rejected_at(128, 127);
}

#[test]
fn rejection_in_the_scalar_tail_is_exact() {
    rejected_at(129, 128);
}

#[test]
fn a_valid_vector_prefix_and_scalar_tail_reach_the_end() {
    assert_eq!(prefix(&[b'a'; 265], LETTER_A), 265);
}

#[test]
fn unaligned_slices_keep_relative_positions() {
    let mut bytes = [b'a'; 200];
    bytes[151] = b'b';
    assert_eq!(prefix(&bytes[5..190], LETTER_A), 146);
}

#[test]
fn an_empty_class_rejects_a_long_input_immediately() {
    assert_eq!(prefix(&[0; 256], [0; 4]), 0);
}

#[test]
fn all_members_of_a_mixed_class_use_the_correct_word_and_bit() {
    let words = [0x8102040810204081, 0x0182442818244281, 0xdeadbeef01234567, 0x09876543fedcba10];
    let observed: [bool; 256] =
        std::array::from_fn(|byte| prefix(&[byte as u8; 160], words) == 160);
    let expected = std::array::from_fn(|byte| words[byte / 64] & (1 << (byte % 64)) != 0);
    assert_eq!(observed, expected);
}

#[test]
fn a_full_class_accepts_every_byte_in_vector_lanes() {
    let bytes: Vec<_> = (0..=255).cycle().take(1024).collect();
    assert_eq!(prefix(&bytes, [u64::MAX; 4]), bytes.len());
}

#[cfg(all(unix, not(miri)))]
mod guarded {
    use super::*;

    struct Mapping {
        pointer: *mut libc::c_void,
        size: usize,
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: this object owns the complete mapping until it is dropped.
            unsafe { libc::munmap(self.pointer, self.size) };
        }
    }

    #[track_caller]
    fn check(length: usize) {
        // SAFETY: sysconf has no pointer arguments; the other calls use a fresh
        // two-page mapping. Only its first page is exposed as an initialized slice.
        unsafe {
            let page = usize::try_from(libc::sysconf(libc::_SC_PAGESIZE)).unwrap();
            assert!(length <= page);
            let pointer = libc::mmap(
                std::ptr::null_mut(),
                page * 2,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            );
            assert_ne!(pointer, libc::MAP_FAILED);
            let mapping = Mapping { pointer, size: page * 2 };
            let end = pointer.cast::<u8>().add(page);
            assert_eq!(libc::mprotect(end.cast(), page, libc::PROT_NONE), 0);
            let start = end.sub(length);
            std::ptr::write_bytes(start, b'a', length);
            assert_eq!(prefix(std::slice::from_raw_parts(start, length), LETTER_A), length);
            end.sub(1).write(b'b');
            assert_eq!(prefix(std::slice::from_raw_parts(start, length), LETTER_A), length - 1);
            drop(mapping);
        }
    }

    #[test]
    fn short_scan_ends_before_an_inaccessible_page() {
        check(127);
    }

    #[test]
    fn exact_vector_end_does_not_load_the_next_page() {
        check(128);
    }

    #[test]
    fn single_byte_tail_does_not_load_the_next_page() {
        check(129);
    }

    #[test]
    fn unaligned_vector_scan_ends_before_an_inaccessible_page() {
        check(257);
    }
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn long_class_prefixes_match_ordered_set_membership(
            allowed in prop::collection::btree_set(any::<u8>(), 1..=256),
            indices in prop::collection::vec(any::<u8>(), 128..1024),
            tail in prop::collection::vec(any::<u8>(), 0..64),
            alignment in 0usize..16,
        ) {
            let members: Vec<_> = allowed.iter().copied().collect();
            let mut words = [0u64; 4];
            for &byte in &members {
                words[usize::from(byte / 64)] |= 1 << (byte % 64);
            }
            let mut storage = vec![0; alignment];
            storage.extend(indices.into_iter().map(|index| members[usize::from(index) % members.len()]));
            storage.extend(tail);
            let bytes = &storage[alignment..];
            let expected = bytes.iter().position(|byte| !allowed.contains(byte)).unwrap_or(bytes.len());
            prop_assert_eq!(prefix(bytes, words), expected);
        }
    }
}
