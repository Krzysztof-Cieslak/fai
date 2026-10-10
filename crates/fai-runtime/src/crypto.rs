//! Audited cryptographic primitives and explicit OS entropy for protocol libraries.

use crate::Value;
use ring::{
    digest, hmac, pbkdf2,
    rand::{SecureRandom, SystemRandom},
};
use std::num::NonZeroU32;
use subtle::ConstantTimeEq;

fn take_bytes(value: Value) -> Vec<u8> {
    // SAFETY: the primitive signature supplies an owned Bytes value.
    let bytes = unsafe { crate::bytes_bytes(value) }.to_vec();
    crate::fai_drop(value);
    bytes
}

/// SHA-256, consuming its buffer.
#[unsafe(no_mangle)]
pub extern "C" fn fai_crypto_sha256(value: Value) -> Value {
    crate::make_bytes(digest::digest(&digest::SHA256, &take_bytes(value)).as_ref())
}

/// HMAC-SHA-256, consuming key and message.
#[unsafe(no_mangle)]
pub extern "C" fn fai_crypto_hmac_sha256(key: Value, message: Value) -> Value {
    let key = hmac::Key::new(hmac::HMAC_SHA256, &take_bytes(key));
    crate::make_bytes(hmac::sign(&key, &take_bytes(message)).as_ref())
}

/// PBKDF2-HMAC-SHA-256. Invalid private calls return an empty buffer; the public
/// wrapper validates counts before invoking the primitive.
#[unsafe(no_mangle)]
pub extern "C" fn fai_crypto_pbkdf2_sha256(
    password: Value,
    salt: Value,
    iterations: Value,
) -> Value {
    let password = take_bytes(password);
    let salt = take_bytes(salt);
    let count = u32::try_from(crate::unbox_int(iterations)).ok().and_then(NonZeroU32::new);
    crate::fai_drop(iterations);
    let Some(count) = count else { return crate::make_bytes(&[]) };
    let work = move || {
        let mut output = [0; 32];
        pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, count, &salt, &password, &mut output);
        output
    };
    let output = if crate::scheduler::in_task() {
        crate::scheduler::run_blocking(Box::new(work))
    } else {
        work()
    };
    crate::make_bytes(&output)
}

/// Constant-time contents comparison for equal-length buffers.
#[unsafe(no_mangle)]
pub extern "C" fn fai_crypto_equal(a: Value, b: Value) -> Value {
    let a = take_bytes(a);
    let b = take_bytes(b);
    crate::from_bool(bool::from(a.ct_eq(&b)))
}

/// PostgreSQL's SASLprep password rule, including raw fallback on prohibited text.
#[unsafe(no_mangle)]
pub extern "C" fn fai_crypto_scram_password(value: Value) -> Value {
    // SAFETY: the primitive signature supplies an owned UTF-8 String.
    let original = unsafe { crate::string_str(value) };
    let normalized = stringprep::saslprep(original).unwrap_or_else(|_| original.into());
    let result = crate::make_string(normalized.as_bytes());
    crate::fai_drop(value);
    result
}

/// OS-backed entropy with a bounded request size, returned as Result Bytes String.
#[unsafe(no_mangle)]
pub extern "C" fn fai_crypto_random(count: Value) -> Value {
    let length = crate::unbox_int(count);
    crate::fai_drop(count);
    let result = if !(0..=1_048_576).contains(&length) {
        Err("entropy count must be in 0..1048576")
    } else {
        let mut bytes = vec![0; length as usize];
        SystemRandom::new()
            .fill(&mut bytes)
            .map(|()| bytes)
            .map_err(|_| "OS entropy is unavailable")
    };
    let (tag, value) = match result {
        Ok(bytes) => (0, crate::make_bytes(&bytes)),
        Err(message) => (1, crate::make_string(message.as_bytes())),
    };
    // SAFETY: the owned field is transferred into the result constructor.
    unsafe { crate::fai_make_data(tag, 1, [value].as_ptr()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn entropy_consumes_the_tagged_count_and_returns_owned_bytes() {
        let _guard = crate::tests::lock();
        let before = crate::live_count();
        let result = fai_crypto_random(crate::fai_box_int(32));
        assert_eq!(crate::data_tag_of(result), 0);
        let bytes = crate::fai_data_field(result, 0);
        // SAFETY: a successful entropy result contains an owned Bytes value.
        assert_eq!(unsafe { crate::bytes_bytes(bytes) }.len(), 32);
        crate::fai_drop(bytes);
        crate::fai_drop(result);
        assert_eq!(crate::live_count(), before);
    }
    #[test]
    fn pbkdf2_matches_the_published_sha256_vector() {
        let mut output = [0; 32];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            NonZeroU32::new(1).unwrap(),
            b"salt",
            b"password",
            &mut output,
        );
        assert_eq!(
            output,
            [
                0x12, 0x0f, 0xb6, 0xcf, 0xfc, 0xf8, 0xb3, 0x2c, 0x43, 0xe7, 0x22, 0x52, 0x56, 0xc4,
                0xf8, 0x37, 0xa8, 0x65, 0x48, 0xc9, 0x2c, 0xcc, 0x35, 0x48, 0x08, 0x05, 0x98, 0x7c,
                0xb7, 0x0b, 0xe1, 0x7b
            ]
        );
    }
}
