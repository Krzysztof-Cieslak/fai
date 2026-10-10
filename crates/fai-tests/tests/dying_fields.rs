//! Destructive unpacking preserves shared snapshots and exact field ownership.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module Main\npublic make : Int -> (String * String)\nlet make x = (Int.toString x, Int.toString (x + 1))\npublic swap : (String * String) -> (String * String)\nlet swap pair = match pair with | (a, b) -> (b, a)\npublic duplicate : (String * String) -> (String * String)\nlet duplicate pair = match pair with | (a, _) -> (a, a)\npublic mixed : (Float * String) -> String\nlet mixed pair = match pair with | (_, text) -> text\npublic main : Runtime -> Unit\nlet main _ = ()\n";

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}
impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("Main.fai".into(), SOURCE.into());
        Self {
            program: fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap(),
            _guard: guard,
        }
    }
    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }
    fn swap(&mut self, number: i64, shared: bool) {
        let baseline = (rt::live_count(), rt::live_bytes());
        let input = self.call("make", &[rt::make_int(number)]);
        let retained = shared.then(|| rt::fai_dup(input));
        let result = self.call("swap", &[input]);
        let left = rt::fai_data_field(result, 0);
        let right = rt::fai_data_field(result, 1);
        assert_eq!(rt::read_string(left), number.wrapping_add(1).to_string().into_bytes());
        assert_eq!(rt::read_string(right), number.to_string().into_bytes());
        rt::fai_drop(left);
        rt::fai_drop(right);
        rt::fai_drop(result);
        if let Some(retained) = retained {
            let original = rt::fai_data_field(retained, 0);
            assert_eq!(rt::read_string(original), number.to_string().into_bytes());
            rt::fai_drop(original);
            rt::fai_drop(retained);
        }
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
    }
}

#[test]
fn a_unique_tuple_transfers_and_reuses_its_fields() {
    Harness::new().swap(123, false);
}

#[test]
fn shared_tuple_fields_remain_unchanged() {
    Harness::new().swap(i64::MAX, true);
}

#[test]
fn one_transferred_field_can_have_multiple_result_owners() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = h.call("make", &[rt::make_int(42)]);
    let result = h.call("duplicate", &[input]);
    let a = rt::fai_data_field(result, 0);
    let b = rt::fai_data_field(result, 1);
    assert_eq!(rt::read_string(a), b"42");
    assert_eq!(a, b);
    rt::fai_drop(a);
    rt::fai_drop(b);
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn transferred_fields_preserve_all_shared_and_unique_values() {
        let h = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), any::<bool>()), |(value, shared)| {
                h.borrow_mut().swap(value, shared);
                Ok(())
            })
            .unwrap();
    }
}
