//! Calendar-period differences round-trip through ordered, clamping additions.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

mod proptests {
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    use super::*;

    #[test]
    fn differences_round_trip_across_the_proleptic_gregorian_calendar() {
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let source = "module Main\npublic roundTrip : Int -> Int -> Int\nlet roundTrip a b =\n  let start = LocalDate.fromEpochDay a\n  let finish = LocalDate.fromEpochDay b\n  let period = Period.between start finish\n  let direction = LocalDate.compare finish start\n  let afterYears = LocalDate.plusYears (Period.years period) start\n  let nextYear = LocalDate.plusYears (Period.years period + direction) start\n  let nextMonth = LocalDate.plusMonths (Period.months period + direction) afterYears\n  let maximal = direction = 0 || (LocalDate.compare nextYear finish * direction > 0 && LocalDate.compare nextMonth finish * direction > 0)\n  let signed = Period.years period * direction >= 0 && Period.months period * direction >= 0 && Period.days period * direction >= 0\n  if Period.addToDate period start = finish && maximal && signed && Int.abs (Period.months period) < 12 then 1 else 0\npublic main : Runtime -> Unit\nlet main r = ()\n";
        let id = db.add_source("Main.fai".into(), source.into());
        let file = db.source_file(id).unwrap();
        let mut program =
            fai_driver::jit_compile(&db, file).unwrap_or_else(|diags| panic!("{diags:?}"));
        let function = program.function(Symbol::intern("roundTrip")).unwrap();
        let dates = (-3_652_425i64..=3_652_425, -3_652_425i64..=3_652_425);
        TestRunner::default()
            .run(&dates, |(start, end)| {
                let baseline = rt::live_count();
                let result =
                    rt::apply(rt::fai_dup(function), &[rt::make_int(start), rt::make_int(end)]);
                let valid = rt::read_int(result);
                rt::fai_drop(result);
                prop_assert_eq!(rt::live_count(), baseline);
                prop_assert_eq!(valid, 1, "date interval {} to {}", start, end);
                Ok(())
            })
            .unwrap();
    }
}
