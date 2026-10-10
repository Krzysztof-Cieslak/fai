//! Exercise the real standard-library repeat entry through its uniform value ABI.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module M\npublic factory : Unit -> (Int -> 'a -> Array 'a)\nlet factory _ = Array.repeat\npublic records : (Int -> { value : Int } -> Array { value : Int }) -> Int\nlet records repeat =\n  let original = { value = 7 }\n  let values = repeat 3 original\n  let first = Array.unsafeGet 0 values\n  let changed = { first with value = 99 }\n  let updated = Array.unsafeSet 0 changed values\n  original.value + (Array.unsafeGet 0 updated).value + (Array.unsafeGet 1 updated).value\npublic main : Runtime -> Unit\nlet main _ = ()\n";

struct Harness {
    program: fai_driver::CompiledProgram,
    repeat: rt::Value,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), SOURCE.into());
        let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        let repeat =
            rt::apply(program.function(Symbol::intern("factory")).unwrap(), &[rt::make_int(0)]);
        Self { program, repeat, _guard: guard }
    }

    fn repeat(&self, count: i64, value: rt::Value) -> rt::Value {
        rt::apply(rt::fai_dup(self.repeat), &[rt::make_int(count), value])
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        rt::fai_drop(self.repeat);
    }
}

fn length(array: rt::Value) -> i64 {
    rt::read_int(rt::fai_array_length_borrowed(array))
}

#[test]
fn empty_repeat_releases_the_supplied_value() {
    let harness = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let array = harness.repeat(0, rt::make_int(i64::MAX));
    assert_eq!(length(array), 0);
    rt::fai_drop(array);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn minimum_negative_count_is_empty_and_leak_free() {
    let harness = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let array = harness.repeat(i64::MIN, rt::make_int(i64::MAX));
    assert_eq!(length(array), 0);
    rt::fai_drop(array);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn full_width_repeated_values_share_one_existing_box() {
    let harness = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = rt::make_int(i64::MAX);
    rt::reset_allocations();
    let array = harness.repeat(32, value);
    assert_eq!(rt::allocations(), 1, "only the pre-sized array is allocated");
    assert_eq!(length(array), 32);
    let element = rt::fai_array_get_borrowed(array, rt::make_int(31));
    assert_eq!(element, value);
    assert_eq!(rt::read_int(element), i64::MAX);
    rt::fai_drop(element);
    rt::fai_drop(array);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn repeated_float_bits_use_one_raw_array() {
    let harness = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let bits = 0xfff8_0000_0000_002au64;
    let value = rt::fai_box_float(bits as i64);
    rt::reset_allocations();
    let array = harness.repeat(32, value);
    assert_eq!(rt::allocations(), 1);
    let element = rt::fai_array_get_borrowed(array, rt::make_int(31));
    assert_eq!(rt::read_float(element).to_bits(), bits);
    rt::fai_drop(element);
    rt::fai_drop(array);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn changing_one_repeated_record_preserves_other_aliases() {
    let mut harness = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let records = harness.program.function(Symbol::intern("records")).unwrap();
    let result = rt::apply(records, &[rt::fai_dup(harness.repeat)]);
    assert_eq!(rt::read_int(result), 113);
    rt::fai_drop(result);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn partial_repeat_keeps_its_count() {
    let harness = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let partial = rt::apply(rt::fai_dup(harness.repeat), &[rt::make_int(3)]);
    let array = rt::apply(partial, &[rt::make_int(7)]);
    assert_eq!(length(array), 3);
    let last = rt::fai_array_get_borrowed(array, rt::make_int(2));
    assert_eq!(rt::read_int(last), 7);
    rt::fai_drop(last);
    rt::fai_drop(array);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn full_width_repeat_count_checks_the_allocation_limit() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "repeat_overflow_worker", "--nocapture"])
        .env("FAI_REPEAT_OVERFLOW", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("allocation size exceeds supported range"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn repeat_overflow_worker() {
    if std::env::var_os("FAI_REPEAT_OVERFLOW").is_none() {
        return;
    }
    Harness::new().repeat(i64::MAX, rt::make_int(7));
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn every_initialized_slot_keeps_the_original_value() {
        let harness = Harness::new();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 64,
            ..ProptestConfig::default()
        });
        runner
            .run(&(-8i64..256, any::<i64>()), |(count, value)| {
                let baseline = (rt::live_count(), rt::live_bytes());
                let array = harness.repeat(count, rt::make_int(value));
                prop_assert_eq!(length(array), count.max(0));
                for index in 0..count.max(0) {
                    let actual = rt::fai_array_get_borrowed(array, rt::make_int(index));
                    prop_assert_eq!(rt::read_int(actual), value);
                    rt::fai_drop(actual);
                }
                rt::fai_drop(array);
                prop_assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
                Ok(())
            })
            .unwrap();
    }
}
