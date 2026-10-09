//! Short-literal append agrees with runtime strings across ownership and layout.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    program: fai_driver::CompiledProgram,
    suffix: String,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new(suffix: &str) -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = format!(
            "module M\npublic append : String -> String\nlet append s = s ++ {suffix:?}\npublic main : Runtime -> Unit\nlet main r = ()\n"
        );
        let id = db.add_source("M.fai".into(), source);
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
            .unwrap_or_else(|d| panic!("{d:?}"));
        Self { program, suffix: suffix.into(), _guard: guard }
    }

    fn append(&mut self, input: rt::Value) -> (rt::Value, i64) {
        let mut expected = rt::read_string(input);
        expected.extend(self.suffix.as_bytes());
        let f = self.program.function(Symbol::intern("append")).unwrap();
        rt::reset_allocations();
        let result = rt::apply(f, &[input]);
        let allocations = rt::allocations();
        assert_eq!(rt::read_string(result), expected);
        let expected = rt::make_str(std::str::from_utf8(&expected).unwrap());
        assert_eq!(rt::fai_equal_borrowed(result, expected), rt::make_int(1));
        let actual_hash = rt::fai_hash_borrowed(result);
        let expected_hash = rt::fai_hash_borrowed(expected);
        assert_eq!(rt::read_int(actual_hash), rt::read_int(expected_hash));
        rt::fai_drop(actual_hash);
        rt::fai_drop(expected_hash);
        let actual_len = rt::fai_string_length_borrowed(result);
        let expected_len = rt::fai_string_length_borrowed(expected);
        assert_eq!(rt::read_int(actual_len), rt::read_int(expected_len));
        rt::fai_drop(actual_len);
        rt::fai_drop(expected_len);
        rt::fai_drop(expected);
        (result, allocations)
    }
}

#[track_caller]
fn in_place(left: &str, suffix: &str) {
    let mut h = Harness::new(suffix);
    let original = rt::make_str(left);
    let (result, allocations) = h.append(original);
    assert_eq!(result, original);
    assert_eq!(allocations, 0);
    rt::fai_drop(result);
}

#[test]
fn short_literal_uses_spare_capacity() {
    in_place("a", "ab");
}
#[test]
fn exact_fit_uses_spare_capacity() {
    in_place("abcde", "xyz");
}
#[test]
fn unicode_store_can_be_unaligned() {
    in_place("a", "λ🙂");
}
#[test]
fn embedded_nul_is_preserved() {
    in_place("abc", "\0");
}
#[test]
fn empty_suffix_returns_its_operand() {
    in_place("abc", "");
}

#[test]
fn empty_left_is_valid() {
    let mut h = Harness::new("ab");
    let input = rt::make_str("");
    let (result, _) = h.append(input);
    rt::fai_drop(result);
}

#[test]
fn threshold_literal_fits_the_grown_buffer() {
    let mut h = Harness::new("1234567890123456");
    let input = rt::fai_string_concat(rt::make_str(&"a".repeat(32)), rt::make_str("b"));
    let (result, allocations) = h.append(input);
    assert_eq!(result, input);
    assert_eq!(allocations, 0);
    rt::fai_drop(result);
}

#[test]
fn beyond_threshold_keeps_the_runtime_fallback() {
    let mut h = Harness::new("12345678901234567");
    let (result, _) = h.append(rt::make_str("a"));
    rt::fai_drop(result);
}

#[test]
fn growth_uses_the_runtime_fallback() {
    let mut h = Harness::new("ab");
    let input = rt::make_str("12345678");
    let (result, allocations) = h.append(input);
    assert_ne!(result, input);
    assert_eq!(allocations, 1);
    rt::fai_drop(result);
}

#[test]
fn shared_left_keeps_the_original_bytes() {
    let mut h = Harness::new("ab");
    let original = rt::make_str("x");
    let (result, _) = h.append(rt::fai_dup(original));
    assert_ne!(result, original);
    assert_eq!(rt::read_string(original), b"x");
    rt::fai_drop(result);
    rt::fai_drop(original);
}

#[test]
fn nested_slice_fallback_never_writes_the_base() {
    let mut h = Harness::new("ab");
    let text = "0123456789".repeat(16);
    let original = rt::make_str(&text);
    let slice = rt::fai_string_drop(rt::make_int(20), rt::fai_dup(original));
    let nested = rt::fai_string_take(rt::make_int(80), slice);
    let (result, _) = h.append(nested);
    assert_eq!(rt::read_string(original), text.as_bytes());
    rt::fai_drop(result);
    rt::fai_drop(original);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn text() -> impl Strategy<Value = String> {
        prop::collection::vec(prop_oneof![Just('a'), Just('λ'), Just('🙂'), Just('\0')], 0..20)
            .prop_map(|chars| chars.into_iter().collect())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]
        #[test]
        fn literal_append_matches_rust_bytes(left in text(), right in text(), shared in any::<bool>()) {
            let mut h = Harness::new(&right);
            let baseline = rt::live_count();
            let original = rt::make_str(&left);
            let input = if shared { rt::fai_dup(original) } else { original };
            let (result, _) = h.append(input);
            if shared {
                prop_assert_eq!(rt::read_string(original), left.as_bytes());
                rt::fai_drop(original);
            }
            rt::fai_drop(result);
            prop_assert_eq!(rt::live_count(), baseline);
        }
    }
}
