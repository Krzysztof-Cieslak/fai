//! Integer/state multi-results retain ownership at direct and uniform boundaries.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    program: fai_driver::CompiledProgram,
    split: bool,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let pairs = db.add_source(
            "Pairs.fai".into(),
            include_str!("fixtures/integer_state/Pairs.fai").into(),
        );
        let id = db
            .add_source("Main.fai".into(), include_str!("fixtures/integer_state/Main.fai").into());
        let split = fai_core::abi::abi(&db, db.source_file(pairs).unwrap(), Symbol::intern("make"))
            .spread_return()
            .is_some();
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, split, _guard: guard }
    }
    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }
}

#[test]
fn discarding_the_state_component_releases_it() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    rt::reset_allocations();
    let result = h.call("number", &[rt::make_int(7)]);
    assert_eq!(rt::read_int(result), 7);
    assert_eq!(rt::allocations(), if h.split { 1 } else { 2 });
    rt::fai_drop(result);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn discarding_the_integer_component_keeps_owned_state() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let result = h.call("state", &[rt::make_int(i64::MIN)]);
    let element = rt::fai_array_get_borrowed(result, rt::make_int(2));
    assert_eq!(rt::read_int(element), i64::MIN);
    rt::fai_drop(element);
    rt::fai_drop(result);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn full_width_values_and_state_survive_both_results() {
        let h = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<i64>(), |value| {
                let baseline = rt::live_count();
                let number = h.borrow_mut().call("number", &[rt::make_int(value)]);
                prop_assert_eq!(rt::read_int(number), value);
                rt::fai_drop(number);
                let state = h.borrow_mut().call("state", &[rt::make_int(value)]);
                let element = rt::fai_array_get_borrowed(state, rt::make_int(0));
                prop_assert_eq!(rt::read_int(element), value);
                rt::fai_drop(element);
                rt::fai_drop(state);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
