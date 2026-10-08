//! Custom date/time patterns invert formatting for valid, full-precision values.

mod proptests {
    use fai_db::{Db, FaiDatabase};
    use fai_runtime as rt;
    use fai_syntax::Symbol;
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    #[test]
    fn full_precision_patterns_round_trip_a_broad_gregorian_domain() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = "module Main\npublic roundTrip : Int -> Int -> Int -> Int\nlet roundTrip selector day nanos =\n  let value = LocalDateTime.of (LocalDate.fromEpochDay day) (LocalTime.fromNanoOfDay nanos)\n  let pattern = if selector = 0 then \"yyyy-MM-dd EEE hh:mm:ss.fffffffff tt\" else if selector = 1 then \"yyyyMMddHHmmss.fffffffff\" else \"yyyy MM MMM dd d EEE EEEE HH hh tt:mm:ss.fffffffff\"\n  if DateTimeFormat.parse pattern (DateTimeFormat.format pattern value) = Some value then 1 else 0\npublic main : Runtime -> Unit\nlet main r = ()\n";
        let id = db.add_source("Main.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        let mut program =
            fai_driver::jit_compile(&db, file).unwrap_or_else(|diags| panic!("{diags:?}"));
        let function = program.function(Symbol::intern("roundTrip")).unwrap();
        let inputs = (0i64..3, -719162i64..2932897, 0i64..86_400_000_000_000);
        TestRunner::default()
            .run(&inputs, |(pattern, day, nanos)| {
                let baseline = rt::live_count();
                let value = rt::apply(
                    rt::fai_dup(function),
                    &[rt::make_int(pattern), rt::make_int(day), rt::make_int(nanos)],
                );
                let actual = rt::read_int(value);
                rt::fai_drop(value);
                prop_assert_eq!(rt::live_count(), baseline);
                prop_assert_eq!(
                    actual,
                    1,
                    "pattern {}, epoch day {}, nanos {}",
                    pattern,
                    day,
                    nanos
                );
                Ok(())
            })
            .unwrap();
    }
}
