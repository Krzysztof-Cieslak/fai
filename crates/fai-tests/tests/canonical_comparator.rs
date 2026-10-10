//! Comparator predicates preserve wrapping subtraction and callback boundaries.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db
            .add_source("Main.fai".into(), include_str!("fixtures/CanonicalComparator.fai").into());
        Self {
            program: fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap(),
            _guard: guard,
        }
    }

    fn check(&mut self, a: i64, b: i64) {
        let baseline = (rt::live_count(), rt::live_bytes());
        let expected = if a.wrapping_sub(b) <= 0 { 3 } else { 0 };
        let function = self.program.function(Symbol::intern("compared")).unwrap();
        let result = rt::apply(function, &[rt::make_int(a), rt::make_int(b)]);
        assert_eq!(rt::read_int(result), expected, "{a} - {b}");
        rt::fai_drop(result);
        let expected = if a.wrapping_sub(b) <= 0 { a.wrapping_sub(b).wrapping_mul(2) } else { 0 };
        let function = self.program.function(Symbol::intern("reused")).unwrap();
        let result = rt::apply(function, &[rt::make_int(a), rt::make_int(b)]);
        assert_eq!(rt::read_int(result), expected, "reused {a} - {b}");
        rt::fai_drop(result);
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
    }
}

#[test]
fn immediate_extremes_use_the_full_width_difference_sign() {
    Harness::new().check(-(1 << 62), (1 << 62) - 1);
}

#[test]
fn positive_overflow_retains_wrapping_sign() {
    Harness::new().check(i64::MAX, -1);
}

#[test]
fn negative_overflow_retains_wrapping_sign() {
    Harness::new().check(i64::MIN, 1);
}

#[test]
fn equal_full_width_operands_keep_balanced_ownership() {
    Harness::new().check(i64::MAX, i64::MAX);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn arbitrary_operands_preserve_predicates_and_reused_results() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), any::<i64>()), |(a, b)| {
                harness.borrow_mut().check(a, b);
                Ok(())
            })
            .unwrap();
    }
}
