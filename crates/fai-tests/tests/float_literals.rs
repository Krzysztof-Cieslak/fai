//! Literal Float shortcuts agree bit-for-bit with total ordering and arithmetic.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut source = String::from("module M\n");
        for (name, value) in
            [("positive", "4.0"), ("negative", "-4.0"), ("zero", "0.0"), ("negativeZero", "-0.0")]
        {
            source += &format!(
                "public {name} : Float -> Int\nlet {name} x = (if x < {value} then 1 else 0) + (if x <= {value} then 2 else 0) + (if x > {value} then 4 else 0) + (if x >= {value} then 8 else 0) + (if {value} < x then 16 else 0) + (if {value} <= x then 32 else 0) + (if {value} > x then 64 else 0) + (if {value} >= x then 128 else 0)\n"
            );
        }
        source += "public left : Float -> Float\nlet left x = 2.0 * x\npublic right : Float -> Float\nlet right x = x * 2.0\npublic main : Runtime -> Unit\nlet main _ = ()\n";
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), source);
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _guard: guard }
    }

    #[track_caller]
    fn check(&mut self, bits: u64) {
        let x = f64::from_bits(bits);
        let baseline = (rt::live_count(), rt::live_bytes());
        // Each field contributes to one combined result describing total order
        // against these fixed constants in both operand directions.
        for (name, constant) in
            [("positive", 4.0), ("negative", -4.0), ("zero", 0.0), ("negativeZero", -0.0)]
        {
            let ordering = x.total_cmp(&constant);
            let expected = i64::from(ordering.is_lt())
                + 2 * i64::from(ordering.is_le())
                + 4 * i64::from(ordering.is_gt())
                + 8 * i64::from(ordering.is_ge())
                + 16 * i64::from(ordering.is_gt())
                + 32 * i64::from(ordering.is_ge())
                + 64 * i64::from(ordering.is_lt())
                + 128 * i64::from(ordering.is_le());
            let function = self.program.function(Symbol::intern(name)).unwrap();
            let result = rt::apply(function, &[rt::fai_box_float(bits as i64)]);
            assert_eq!(rt::read_int(result), expected, "{name}: {bits:016x}");
            rt::fai_drop(result);
        }
        let function = self.program.function(Symbol::intern("left")).unwrap();
        let result = rt::apply(function, &[rt::fai_box_float(bits as i64)]);
        assert_eq!(rt::read_float(result).to_bits(), (2.0 * x).to_bits());
        rt::fai_drop(result);
        let function = self.program.function(Symbol::intern("right")).unwrap();
        let result = rt::apply(function, &[rt::fai_box_float(bits as i64)]);
        assert_eq!(rt::read_float(result).to_bits(), (x * 2.0).to_bits());
        rt::fai_drop(result);
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
    }
}

#[test]
fn positive_zero_keeps_total_order() {
    Harness::new().check(0);
}

#[test]
fn negative_zero_keeps_total_order() {
    Harness::new().check(1 << 63);
}

#[test]
fn positive_signaling_nan_keeps_payload() {
    Harness::new().check(0x7ff0_0000_0000_2345);
}

#[test]
fn negative_signaling_nan_keeps_payload() {
    Harness::new().check(0xfff0_0000_0000_2345);
}

#[test]
fn overflowing_finite_double_keeps_infinity() {
    Harness::new().check(f64::MAX.to_bits());
}

#[test]
fn subnormal_double_keeps_all_bits() {
    Harness::new().check(1);
}

#[test]
fn negative_infinity_keeps_total_order() {
    Harness::new().check(f64::NEG_INFINITY.to_bits());
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn arbitrary_float_bits_preserve_literal_operations() {
        let harness = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(&any::<u64>(), |bits| {
                harness.borrow_mut().check(bits);
                Ok(())
            })
            .unwrap();
    }
}
