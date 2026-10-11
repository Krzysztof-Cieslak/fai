//! Destructive unpacking preserves shared snapshots and exact field ownership.

use fai_db::{Db, FaiDatabase};
use fai_runtime as rt;
use fai_syntax::Symbol;
use std::sync::{Mutex, MutexGuard};

static LOCK: Mutex<()> = Mutex::new(());
const BRANCH_SOURCE: &str = r#"
public type Branch = | Left String String | Right String String
public makeBranch : Int -> Int -> Branch
let makeBranch tag number =
  let a = Int.toString number
  let b = Int.toString (number + 1)
  if tag = 0 then Left a b else Right b a
public unpackBranch : Branch -> (String * String)
let unpackBranch value =
  match value with
  | Left a b -> (b, a)
  | Right a b -> (a, b)
public makeAliasedBranch : Int -> Branch
let makeAliasedBranch number =
  let text = Int.toString number
  Right text text
"#;
const SOURCE: &str = "module Main\npublic make : Int -> (String * String)\nlet make x = (Int.toString x, Int.toString (x + 1))\npublic swap : (String * String) -> (String * String)\nlet swap pair = match pair with | (a, b) -> (b, a)\npublic duplicate : (String * String) -> (String * String)\nlet duplicate pair = match pair with | (a, _) -> (a, a)\npublic mixed : (Float * String) -> String\nlet mixed pair = match pair with | (_, text) -> text\npublic main : Runtime -> Unit\nlet main _ = ()\n";

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}
impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("Main.fai".into(), format!("{SOURCE}{BRANCH_SOURCE}"));
        Self {
            program: fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap(),
            _guard: guard,
        }
    }
    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }
    fn swap(&mut self, number: i64, shared: bool) {
        let baseline = (rt::live_count(), rt::live_bytes());
        let input = self.call("make", &[rt::make_int(number)]);
        let retained = shared.then(|| rt::fai_dup(input));
        let result = self.call("swap", &[input]);
        let left = rt::fai_data_field(result, 0);
        let right = rt::fai_data_field(result, 1);
        assert_eq!(rt::read_string(left), number.wrapping_add(1).to_string().into_bytes());
        assert_eq!(rt::read_string(right), number.to_string().into_bytes());
        rt::fai_drop(left);
        rt::fai_drop(right);
        rt::fai_drop(result);
        if let Some(retained) = retained {
            let original = rt::fai_data_field(retained, 0);
            assert_eq!(rt::read_string(original), number.to_string().into_bytes());
            rt::fai_drop(original);
            rt::fai_drop(retained);
        }
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
    }

    fn branch(&mut self, tag: i64, number: i64, shared: bool) {
        let baseline = (rt::live_count(), rt::live_bytes());
        let input = self.call("makeBranch", &[rt::make_int(tag), rt::make_int(number)]);
        let retained = shared.then(|| rt::fai_dup(input));
        let result = self.call("unpackBranch", &[input]);
        let left = rt::fai_data_field(result, 0);
        let right = rt::fai_data_field(result, 1);
        assert_eq!(rt::read_string(left), number.wrapping_add(1).to_string().into_bytes());
        assert_eq!(rt::read_string(right), number.to_string().into_bytes());
        rt::fai_drop(left);
        rt::fai_drop(right);
        rt::fai_drop(result);
        if let Some(retained) = retained {
            let original = rt::fai_data_field(retained, 0);
            let expected = if tag == 0 { number } else { number.wrapping_add(1) };
            assert_eq!(rt::read_string(original), expected.to_string().into_bytes());
            rt::fai_drop(original);
            rt::fai_drop(retained);
        }
        assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
    }
}

#[test]
fn a_unique_tuple_transfers_and_reuses_its_fields() {
    Harness::new().swap(123, false);
}

#[test]
fn shared_tuple_fields_remain_unchanged() {
    Harness::new().swap(i64::MAX, true);
}

#[test]
fn one_transferred_field_can_have_multiple_result_owners() {
    let mut h = Harness::new();
    let baseline = rt::live_count();
    let input = h.call("make", &[rt::make_int(42)]);
    let result = h.call("duplicate", &[input]);
    let a = rt::fai_data_field(result, 0);
    let b = rt::fai_data_field(result, 1);
    assert_eq!(rt::read_string(a), b"42");
    assert_eq!(a, b);
    rt::fai_drop(a);
    rt::fai_drop(b);
    rt::fai_drop(result);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn the_first_constructor_transfers_its_owned_fields() {
    Harness::new().branch(0, i64::MIN, false);
}

#[test]
fn the_second_constructor_transfers_its_owned_fields() {
    Harness::new().branch(1, i64::MAX, false);
}

#[test]
fn alternative_field_transfers_preserve_shared_parents() {
    Harness::new().branch(1, 42, true);
}

#[test]
fn alternative_field_transfers_preserve_aliased_children() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let input = h.call("makeAliasedBranch", &[rt::make_int(42)]);
    let result = h.call("unpackBranch", &[input]);
    let a = rt::fai_data_field(result, 0);
    let b = rt::fai_data_field(result, 1);
    assert_eq!(a, b);
    assert_eq!(rt::read_string(a), b"42");
    rt::fai_drop(a);
    rt::fai_drop(b);
    rt::fai_drop(result);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn transferred_fields_preserve_all_shared_and_unique_values() {
        let h = RefCell::new(Harness::new());
        let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
            cases: 128,
            ..ProptestConfig::default()
        });
        runner
            .run(&(any::<i64>(), any::<bool>()), |(value, shared)| {
                h.borrow_mut().swap(value, shared);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn branch_field_transfers_preserve_shared_and_unique_values() {
        let h = RefCell::new(Harness::new());
        proptest::test_runner::TestRunner::default()
            .run(&(0i64..2, any::<i64>(), any::<bool>()), |(tag, value, shared)| {
                h.borrow_mut().branch(tag, value, shared);
                Ok(())
            })
            .unwrap();
    }
}
