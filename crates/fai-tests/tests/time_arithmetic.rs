//! Wide-integer oracles for day/remainder time arithmetic.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

const DAY: i64 = 86_400_000_000_000;
static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module Main
make : Int -> Int -> LocalDateTime
let make day nanos = LocalDateTime.of (LocalDate.fromEpochDay day) (LocalTime.fromNanoOfDay nanos)
parts : LocalDateTime -> Int * Int
let parts value = (LocalDate.toEpochDay (LocalDateTime.date value), LocalTime.nanoOfDay (LocalDateTime.time value))
public addDateTime : Int -> Int -> Int -> Int -> Int * Int
let addDateTime unit amount day nanos =
  let value = make day nanos
  parts (match unit with
  | 0 -> LocalDateTime.plusHours amount value
  | 1 -> LocalDateTime.plusMinutes amount value
  | 2 -> LocalDateTime.plusSeconds amount value
  | _ -> LocalDateTime.plusNanoseconds amount value)
public addClock : Int -> Int -> Int -> Int
let addClock unit amount nanos =
  let value = LocalTime.fromNanoOfDay nanos
  LocalTime.nanoOfDay (match unit with
  | 0 -> LocalTime.plusHours amount value
  | 1 -> LocalTime.plusMinutes amount value
  | 2 -> LocalTime.plusSeconds amount value
  | _ -> LocalTime.plusNanoseconds amount value)
public scale : Int -> Int -> Int -> Int * Int
let scale factor day nanos =
  let value = Duration.plus (Duration.ofDays day) (Duration.ofNanoseconds nanos)
  let result = Duration.multiply factor value
  (Duration.days result, Duration.nanosecondOfDay result)
public addPeriod : Int -> Int -> Int -> Int -> Int -> Int -> Int * Int
let addPeriod day nanos hours minutes seconds extra =
  let period = Period.add (Period.ofHours hours) (Period.add (Period.ofMinutes minutes) (Period.add (Period.ofSeconds seconds) (Period.ofNanoseconds extra)))
  parts (Period.addToDateTime period (make day nanos))
public main : Runtime -> Unit
let main r = ()
"#;

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
        let file = db.source_file(id).unwrap();
        let program =
            fai_driver::jit_compile(&db, file).unwrap_or_else(|diags| panic!("{diags:?}"));
        Self { program, _guard: guard }
    }

    fn apply(&mut self, name: &str, args: &[i64]) -> rt::Value {
        let function = self.program.function(Symbol::intern(name)).unwrap();
        let args: Vec<_> = args.iter().copied().map(rt::make_int).collect();
        rt::apply(rt::fai_dup(function), &args)
    }

    fn pair(&mut self, name: &str, args: &[i64]) -> (i64, i64) {
        let baseline = rt::live_count();
        let result = self.apply(name, args);
        let days = rt::fai_data_field(result, 0);
        let nanos = rt::fai_data_field(result, 1);
        let pair = (rt::read_int(days), rt::read_int(nanos));
        rt::fai_drop(days);
        rt::fai_drop(nanos);
        rt::fai_drop(result);
        assert_eq!(rt::live_count(), baseline);
        pair
    }

    fn clock(&mut self, unit: i64, amount: i64, nanos: i64) -> i64 {
        let baseline = rt::live_count();
        let result = self.apply("addClock", &[unit, amount, nanos]);
        let nanos = rt::read_int(result);
        rt::fai_drop(result);
        assert_eq!(rt::live_count(), baseline);
        nanos
    }
}

fn unit_nanos(unit: i64) -> i128 {
    match unit {
        0 => 3_600_000_000_000,
        1 => 60_000_000_000,
        2 => 1_000_000_000,
        _ => 1,
    }
}

fn added(day: i64, nanos: i128) -> (i64, i64) {
    let divisor = i128::from(DAY);
    ((i128::from(day) + nanos.div_euclid(divisor)) as i64, nanos.rem_euclid(divisor) as i64)
}

fn scaled(factor: i64, day: i64, nanos: i64) -> (i64, i64) {
    let fraction = i128::from(nanos) * i128::from(factor);
    let divisor = i128::from(DAY);
    (
        (i128::from(day) * i128::from(factor) + fraction.div_euclid(divisor)) as i64,
        fraction.rem_euclid(divisor) as i64,
    )
}

#[test]
fn millions_of_hours_preserve_the_day_count() {
    assert_eq!(Harness::new().pair("addDateTime", &[0, 3_000_000, 0, 0]), (125_000, 0));
}

#[test]
fn large_duration_scaling_preserves_carry_and_remainder() {
    assert_eq!(
        Harness::new().pair("scale", &[1_000_000, 0, 4 * 3_600_000_000_000]),
        (166_666, 57_600_000_000_000)
    );
}

#[test]
fn minimum_factor_scales_a_nearly_complete_day() {
    assert_eq!(Harness::new().pair("scale", &[i64::MIN, 0, DAY - 1]), scaled(i64::MIN, 0, DAY - 1));
}

#[test]
fn maximum_nanoseconds_wrap_time_without_intermediate_overflow() {
    assert_eq!(
        Harness::new().clock(3, i64::MAX, DAY - 1),
        (i128::from(i64::MAX) + i128::from(DAY - 1)).rem_euclid(i128::from(DAY)) as i64
    );
}

#[test]
fn minimum_nanoseconds_wrap_time_without_intermediate_overflow() {
    assert_eq!(
        Harness::new().clock(3, i64::MIN, DAY - 1),
        (i128::from(i64::MIN) + i128::from(DAY - 1)).rem_euclid(i128::from(DAY)) as i64
    );
}

#[test]
fn final_positive_day_overflow_wraps_after_carry() {
    assert_eq!(Harness::new().pair("addDateTime", &[3, 1, i64::MAX, DAY - 1]), (i64::MIN, 0));
}

#[test]
fn final_negative_day_overflow_wraps_after_borrow() {
    assert_eq!(Harness::new().pair("addDateTime", &[3, -1, i64::MIN, 0]), (i64::MAX, DAY - 1));
}

#[test]
fn duration_day_overflow_keeps_the_fraction_canonical() {
    assert_eq!(Harness::new().pair("scale", &[2, i64::MAX, DAY - 1]), (-1, DAY - 2));
}

mod proptests {
    use std::cell::RefCell;

    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    use super::*;

    #[test]
    fn date_time_unit_additions_match_a_wide_oracle() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&(0i64..4, any::<i64>(), any::<i64>(), 0i64..DAY), |(unit, amount, day, nanos)| {
                let actual = harness.borrow_mut().pair("addDateTime", &[unit, amount, day, nanos]);
                prop_assert_eq!(
                    actual,
                    added(day, i128::from(nanos) + i128::from(amount) * unit_nanos(unit))
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn time_of_day_unit_additions_match_a_wide_oracle() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&(0i64..4, any::<i64>(), 0i64..DAY), |(unit, amount, nanos)| {
                let actual = harness.borrow_mut().clock(unit, amount, nanos);
                prop_assert_eq!(
                    actual,
                    added(0, i128::from(nanos) + i128::from(amount) * unit_nanos(unit)).1
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn duration_scaling_matches_a_wide_oracle() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&(any::<i64>(), any::<i64>(), 0i64..DAY), |(factor, day, nanos)| {
                prop_assert_eq!(
                    harness.borrow_mut().pair("scale", &[factor, day, nanos]),
                    scaled(factor, day, nanos)
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn mixed_period_time_components_match_a_wide_oracle() {
        let harness = RefCell::new(Harness::new());
        let inputs = (
            -1_000_000i64..1_000_000,
            0i64..DAY,
            any::<i64>(),
            any::<i64>(),
            any::<i64>(),
            any::<i64>(),
        );
        TestRunner::default()
            .run(&inputs, |(day, nanos, hours, minutes, seconds, extra)| {
                let actual = harness
                    .borrow_mut()
                    .pair("addPeriod", &[day, nanos, hours, minutes, seconds, extra]);
                let total = i128::from(nanos)
                    + i128::from(hours) * unit_nanos(0)
                    + i128::from(minutes) * unit_nanos(1)
                    + i128::from(seconds) * unit_nanos(2)
                    + i128::from(extra);
                prop_assert_eq!(actual, added(day, total));
                Ok(())
            })
            .unwrap();
    }
}
