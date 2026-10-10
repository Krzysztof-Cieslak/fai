//! Bounded control specialization agrees with unspecialized wrapping arithmetic.

mod proptests {
    use fai_db::{Db, FaiDatabase};
    use fai_runtime as rt;
    use fai_syntax::Symbol;
    use proptest::prelude::*;

    #[test]
    fn specialized_and_dynamic_levels_agree_on_every_int() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = "module M\nlet step level value = if level = 0 then value else step (level - 1) (value * 3 + 1)\npublic fast : Int -> Int\nlet fast value = step 3 value\npublic dynamic : Int -> Int -> Int\nlet dynamic level value = step level value\npublic main : Runtime -> Unit\nlet main _ = ()\n";
        let id = db.add_source("M.fai".into(), source.into());
        let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        let fast = program.function(Symbol::intern("fast")).unwrap();
        let dynamic = program.function(Symbol::intern("dynamic")).unwrap();
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<i64>(), |value| {
                let baseline = rt::live_count();
                let actual = rt::apply(rt::fai_dup(fast), &[rt::make_int(value)]);
                let ordinary =
                    rt::apply(rt::fai_dup(dynamic), &[rt::make_int(3), rt::make_int(value)]);
                let expected = (0..3).fold(value, |value, _| value.wrapping_mul(3).wrapping_add(1));
                prop_assert_eq!(rt::read_int(actual), expected);
                prop_assert_eq!(rt::read_int(ordinary), expected);
                rt::fai_drop(actual);
                rt::fai_drop(ordinary);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
