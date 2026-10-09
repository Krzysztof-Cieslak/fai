//! Calendar conversions over every signed epoch day agree with wide arithmetic.

use fai_db::Db;
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static SERIAL: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module Main\npublic parts : Int -> Int * Int * Int * Int\nlet parts ed =\n  let date = LocalDate.fromEpochDay ed\n  (LocalDate.year date, LocalDate.month date, LocalDate.day date, DayOfWeek.toInt (LocalDate.dayOfWeek date))\npublic roundTrip : Int -> Int\nlet roundTrip ed =\n  let date = LocalDate.fromEpochDay ed\n  let parsed = LocalDate.parse (LocalDate.toString date)\n  LocalDate.toEpochDay (Option.withDefault (LocalDate.fromEpochDay 0) parsed)\npublic main : Runtime -> Unit\nlet main r = ()\n";

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}
impl Harness {
    fn new() -> Self {
        let guard = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("Main.fai".into(), SOURCE.into());
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
            .unwrap_or_else(|e| panic!("{e:?}"));
        Self { program, _guard: guard }
    }
    fn call(&mut self, name: &str, day: i64) -> rt::Value {
        let function = self.program.function(Symbol::intern(name)).unwrap();
        rt::apply(rt::fai_dup(function), &[rt::make_int(day)])
    }
    fn check(&mut self, day: i64) {
        let live = rt::live_count();
        let value = self.call("parts", day);
        let actual: Vec<_> = (0..4)
            .map(|i| {
                let field = rt::fai_data_field(value, i);
                let integer = rt::read_int(field);
                rt::fai_drop(field);
                integer
            })
            .collect();
        rt::fai_drop(value);
        assert_eq!(actual, expected(day), "epoch day {day}");
        let back = self.call("roundTrip", day);
        assert_eq!(rt::read_int(back), day, "ISO round trip of {day}");
        rt::fai_drop(back);
        assert_eq!(rt::live_count(), live);
    }
}

fn expected(day: i64) -> Vec<i64> {
    let z = i128::from(day) + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    vec![
        (year + i128::from(m <= 2)) as i64,
        m as i64,
        d as i64,
        ((i128::from(day) + 3).rem_euclid(7) + 1) as i64,
    ]
}

#[test]
fn maximum_epoch_day_has_the_correct_calendar_and_weekday() {
    Harness::new().check(i64::MAX);
}
#[test]
fn minimum_epoch_day_round_trips() {
    Harness::new().check(i64::MIN);
}
#[test]
fn epoch_offset_overflow_boundary_is_exact() {
    Harness::new().check(i64::MAX - 719467);
}
#[test]
fn negative_era_adjustment_is_exact() {
    Harness::new().check(-719469);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]
        #[test]
        fn full_width_epoch_days_match_a_wide_oracle(days in proptest::collection::vec(any::<i64>(), 1..65)) {
            let mut harness = Harness::new();
            for day in days { harness.check(day); }
        }
    }
}
