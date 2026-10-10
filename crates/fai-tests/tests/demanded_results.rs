//! Immediate destructuring removes only the exposed layer of recursive results.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module Main
step : Int -> Int -> Option (Int * Int)
let step n value =
  if n <= 0 then Some (value, n) else if n = 1 then None else step (n - 1) value
public unpack : Int -> Int -> Int
let unpack n value =
  match step n value with
  | None -> -1
  | Some (left, right) -> left + right
public main : Runtime -> Unit
let main runtime = ()
"#;

struct Harness {
    program: fai_driver::CompiledProgram,
    _lock: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("Main.fai".into(), SOURCE.into());
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _lock: lock }
    }

    fn call(&mut self, n: i64, value: i64) -> (i64, i64) {
        let function = self.program.function(Symbol::intern("unpack")).unwrap();
        let baseline = (rt::live_count(), rt::live_bytes());
        let args = [rt::make_int(n), rt::make_int(value)];
        rt::reset_allocations();
        let output = rt::apply(function, &args);
        let result = (rt::read_int(output), rt::allocations());
        rt::fai_drop(output);
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
        result
    }
}

#[test]
fn exposed_success_path_needs_no_result_cell() {
    assert_eq!(Harness::new().call(0, 42), (42, 0));
}

#[test]
fn recursive_failure_path_retains_its_result() {
    assert_eq!(Harness::new().call(10_000, 42).0, -1);
}

#[test]
fn exposed_fields_preserve_full_width_wrapping() {
    assert_eq!(Harness::new().call(-1, i64::MIN).0, i64::MAX);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;
    use std::cell::RefCell;

    #[test]
    fn exposed_and_recursive_paths_match_the_original_result() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&(-8i64..24, any::<i64>()), |(n, value)| {
                let expected = if n <= 0 { value.wrapping_add(n) } else { -1 };
                prop_assert_eq!(harness.borrow_mut().call(n, value).0, expected);
                Ok(())
            })
            .unwrap();
    }
}
