//! Bounded prefix scans over arbitrary byte classes, with per-call vector tables.

#[inline]
fn scalar(bytes: &[u8], words: [u64; 4]) -> usize {
    bytes.iter().take_while(|&&b| words[usize::from(b >> 6)] & (1 << (b & 63)) != 0).count()
}

/// Count the initial bytes in the class. Short prefixes use the scalar scan
/// without building lookup tables or checking processor features.
#[inline]
pub(super) fn prefix(bytes: &[u8], words: [u64; 4]) -> usize {
    if bytes.len() < 128 {
        return scalar(bytes, words);
    }
    let first = scalar(&bytes[..32], words);
    if first != 32 {
        return first;
    }
    first + long_prefix(&bytes[first..], words)
}

#[inline(never)]
fn long_prefix(bytes: &[u8], words: [u64; 4]) -> usize {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    if std::arch::is_x86_feature_detected!("ssse3") {
        // SAFETY: runtime detection establishes SSSE3; the callee bounds every load.
        return unsafe { x86_prefix(bytes, words) };
    }
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    if std::arch::is_aarch64_feature_detected!("neon") {
        // SAFETY: runtime detection establishes NEON; the callee bounds every load.
        return unsafe { arm_prefix(bytes, words) };
    }
    scalar(bytes, words)
}

// Each low nibble selects a byte whose bits encode the eight high nibbles in
// one half of the class. Arbitrary classes need only these two 16-byte tables.
#[cfg(all(any(target_arch = "x86_64", target_arch = "aarch64"), not(miri)))]
fn tables(words: [u64; 4]) -> ([u8; 16], [u8; 16]) {
    let mut lower = [0u8; 16];
    let mut upper = [0u8; 16];
    for (low, (lower_byte, upper_byte)) in lower.iter_mut().zip(&mut upper).enumerate() {
        for high in 0..8 {
            let byte = (high << 4) | low;
            *lower_byte |= (((words[byte >> 6] >> (byte & 63)) & 1) as u8) << high;
            *upper_byte |= (((words[2 + (byte >> 6)] >> (byte & 63)) & 1) as u8) << high;
        }
    }
    (lower, upper)
}

#[cfg(all(target_arch = "x86_64", not(miri)))]
#[target_feature(enable = "ssse3")]
unsafe fn x86_prefix(bytes: &[u8], words: [u64; 4]) -> usize {
    use std::arch::x86_64::*;

    let (lower, upper) = tables(words);
    // SAFETY: both arrays have exactly 16 initialized bytes; unaligned loads are
    // allowed. Every input load is guarded by at least 16 remaining slice bytes.
    unsafe {
        let lower = _mm_loadu_si128(lower.as_ptr().cast());
        let upper = _mm_loadu_si128(upper.as_ptr().cast());
        let bits = _mm_setr_epi8(1, 2, 4, 8, 16, 32, 64, -128, 1, 2, 4, 8, 16, 32, 64, -128);
        let zero = _mm_setzero_si128();
        let mut index = 0;
        while bytes.len() - index >= 16 {
            let input = _mm_loadu_si128(bytes.as_ptr().add(index).cast());
            let low = _mm_and_si128(input, _mm_set1_epi8(15));
            let high = _mm_and_si128(_mm_srli_epi16::<4>(input), _mm_set1_epi8(7));
            let upper_half = _mm_cmpgt_epi8(zero, input);
            let masks = _mm_or_si128(
                _mm_and_si128(upper_half, _mm_shuffle_epi8(upper, low)),
                _mm_andnot_si128(upper_half, _mm_shuffle_epi8(lower, low)),
            );
            let present = _mm_and_si128(masks, _mm_shuffle_epi8(bits, high));
            let rejected = _mm_movemask_epi8(_mm_cmpeq_epi8(present, zero)) as u32;
            if rejected != 0 {
                return index + rejected.trailing_zeros() as usize;
            }
            index += 16;
        }
        index + scalar(&bytes[index..], words)
    }
}

#[cfg(all(target_arch = "aarch64", not(miri)))]
#[target_feature(enable = "neon")]
unsafe fn arm_prefix(bytes: &[u8], words: [u64; 4]) -> usize {
    use std::arch::aarch64::*;

    let (lower, upper) = tables(words);
    // SAFETY: the tables contain 16 initialized bytes, and each input load is
    // guarded by 16 remaining bytes. NEON permits unaligned byte loads.
    unsafe {
        let lower = vld1q_u8(lower.as_ptr());
        let upper = vld1q_u8(upper.as_ptr());
        let zero = vdupq_n_u8(0);
        let mut index = 0;
        while bytes.len() - index >= 16 {
            let input = vld1q_u8(bytes.as_ptr().add(index));
            let low = vandq_u8(input, vdupq_n_u8(15));
            let high = vandq_u8(vshrq_n_u8::<4>(input), vdupq_n_u8(7));
            let masks = vbslq_u8(
                vcgeq_u8(input, vdupq_n_u8(128)),
                vqtbl1q_u8(upper, low),
                vqtbl1q_u8(lower, low),
            );
            let bit = vshlq_u8(vdupq_n_u8(1), vreinterpretq_s8_u8(high));
            if vmaxvq_u8(vceqq_u8(vandq_u8(masks, bit), zero)) != 0 {
                return index + scalar(&bytes[index..index + 16], words);
            }
            index += 16;
        }
        index + scalar(&bytes[index..], words)
    }
}

#[cfg(test)]
#[path = "byte_class_tests.rs"]
mod tests;
