//! Bounded recursive expansion preserves all low-word arithmetic results.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn full_width_branching_recursion_matches_its_recurrence() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db
            .add_source("Main.fai".into(), include_str!("fixtures/ScalarPeelingDepth.fai").into());
        let program =
            RefCell::new(fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), -3i64..11), |(value, depth)| {
                let baseline = rt::live_count();
                let function = program.borrow_mut().function(Symbol::intern("expand")).unwrap();
                let result = rt::apply(function, &[rt::make_int(depth), rt::make_int(value)]);
                let n = depth.max(0);
                let expected = value.wrapping_mul(1 << n).wrapping_add(if n == 0 {
                    0
                } else {
                    n * (1 << (n - 1))
                });
                prop_assert_eq!(rt::read_int(result), expected);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
