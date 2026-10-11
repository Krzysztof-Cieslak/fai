//! Startup transitions preserve String owners and every carried scalar value.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module Main
public append : Int -> Int -> String -> String
let append n counter text =
  if n <= 0 then text
  else if counter % 2 = 0 then append (n - 1) (counter + 1) (text ++ "a")
  else append (n - 1) (counter + 1) (text ++ "λ")
public main : Runtime -> Unit
let main runtime = ()
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
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _guard: guard }
    }

    fn check(&mut self, n: i64, counter: i64, input: &str, shared: bool, sliced: bool) {
        let baseline = (rt::live_count(), rt::live_bytes());
        let mut seed = rt::make_str(input);
        let mut original = input.to_owned();
        if sliced {
            let half = input.chars().count() / 2;
            seed = rt::fai_string_take(rt::make_int(half as i64), seed);
            original = input.chars().take(half).collect();
        }
        let retained = shared.then(|| rt::fai_dup(seed));
        let function = self.program.function(Symbol::intern("append")).unwrap();
        let output = rt::apply(function, &[rt::make_int(n), rt::make_int(counter), seed]);
        let mut expected = original.clone();
        for i in 0..n {
            expected.push(if counter.wrapping_add(i) % 2 == 0 { 'a' } else { 'λ' });
        }
        assert_eq!(rt::read_string(output), expected.as_bytes());
        rt::fai_drop(output);
        if let Some(retained) = retained {
            assert_eq!(rt::read_string(retained), original.as_bytes());
            rt::fai_drop(retained);
        }
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
    }
}

#[test]
fn empty_startup_preserves_a_wrapping_counter() {
    Harness::new().check(12, i64::MAX, "", false, false);
}

#[test]
fn shared_startup_keeps_the_original_value() {
    Harness::new().check(32, 0, "seed", true, false);
}

#[test]
fn slice_startup_keeps_the_original_base_contents() {
    Harness::new().check(32, 0, &"λa".repeat(64), true, true);
}

#[test]
fn zero_iterations_leave_the_input_intact() {
    Harness::new().check(0, i64::MIN, "seed", true, false);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;
    use std::cell::RefCell;

    #[test]
    fn startup_transitions_preserve_text_and_ownership() {
        let h = RefCell::new(Harness::new());
        TestRunner::default()
            .run(
                &(-3i64..40, any::<i64>(), ".{0,96}", any::<bool>(), any::<bool>()),
                |(n, counter, text, shared, sliced)| {
                    h.borrow_mut().check(n, counter, &text, shared, sliced);
                    Ok(())
                },
            )
            .unwrap();
    }
}
