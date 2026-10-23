//! Result components cross direct calls without temporary wrapper/tuple cells.

use fai_db::{Db, FaiDatabase, Setter};
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
        Self::controlled(fai_db::AllocationControl::Enabled)
    }

    fn controlled(control: fai_db::AllocationControl) -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        db.set_allocation_control(control);
        fai_types::std_lib::load_std(&mut db);
        db.add_source("Producer.fai".into(), r#"module Producer
public produce : Int -> Result (Int * String) String
let produce n = if n < 0 then Err "bad" else Ok (n, Int.toString n)
public pass : Result (Int * String) String -> Result (Int * String) String
let pass value = value
public descend : Int -> Result (Int * String) String
let descend n = if n <= 0 then Ok (42, "x") else descend (n - 1)
public errorTuple : Int -> Result Unit (Int * String)
let errorTuple n = if n = 0 then Ok () else Err (n, "error")
public wide : Int -> Int -> Int -> Int -> Int -> Int -> Int -> Int -> Result (Int * Int * Int * Int * Int * Int * String) String
let wide a b c d e f g h = Ok (a + h, b, c, d, e, f + g, "wide")
public effectful : Int -> Result (Int * String) String / { Console }
let effectful n =
  let _ = stdConsole.writeLine "produce"
  if n = 0 then Err "stop" else Ok (n, "value")
public floating : Float -> Result (Float * Float) String
let floating value = Ok (value, Float.fromBits (-9223372036854775808))
"#.into());
        let id = db.add_source("Main.fai".into(), r#"module Main
public consume : Int -> Int
let consume n = match Producer.produce n with | Ok (value, text) -> value + String.length text | Err error -> String.length error
public boxed : Int -> Result (Int * String) String
let boxed n = Producer.produce n
public ignore : Int -> Bool
let ignore n = match Producer.produce n with | Ok _ -> true | Err _ -> false
public passthrough : Int -> Int
let passthrough n = match Producer.pass (Producer.produce n) with | Ok (value, text) -> value + String.length text | Err error -> String.length error
public recursive : Int -> Int
let recursive n = match Producer.descend n with | Ok (value, text) -> value + String.length text | Err _ -> 0
public errorFields : Int -> Int
let errorFields n = match Producer.errorTuple n with | Ok _ -> 0 | Err (value, text) -> value + String.length text
public wideFields : Int -> Int
let wideFields n = match Producer.wide n 2 3 4 5 6 7 8 with | Ok (a, b, c, d, e, f, text) -> a + b + c + d + e + f + String.length text | Err _ -> 0
public effects : Int -> Int / { Console }
let effects n =
  let result = Producer.effectful n
  let _ = stdConsole.writeLine "consume"
  match result with | Ok (value, text) -> value + String.length text | Err error -> String.length error
public floatBits : Int -> Int
let floatBits bits = match Producer.floating (Float.fromBits bits) with | Ok (value, zero) -> if Float.toBits zero = -9223372036854775808 then Float.toBits value else 0 | Err _ -> 0
public shared : Int -> Int
let shared n =
  let result = Producer.produce n
  let alias = result
  let value = match result with | Ok (value, _) -> value | Err _ -> 0
  if result = alias then value else 0
public rematerialized : Int -> Int
let rematerialized n =
  let result = Producer.produce n
  let copied = Producer.pass result
  let first = match copied with | Ok (value, text) -> value + String.length text | Err text -> String.length text
  let second = match result with | Ok (value, text) -> value + String.length text | Err text -> String.length text
  first + second
public main : Runtime -> Unit / { Console }
let main r =
  let _ = boxed 7
  let _ = ignore 1
  r.console.writeLine (Int.toString (consume 42))
"#.into());
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap()).unwrap();
        Self { program, _guard: guard }
    }

    fn call(&mut self, name: &str, n: i64) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), &[rt::make_int(n)])
    }
}

#[test]
fn direct_success_allocates_only_its_text() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    rt::reset_allocations();
    let value = h.call("consume", 42);
    assert_eq!(rt::read_int(value), 44);
    assert_eq!(rt::allocations(), 1);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn boxed_control_materializes_both_result_and_tuple() {
    let mut h = Harness::controlled(fai_db::AllocationControl::NoScalarReplacement);
    rt::reset_allocations();
    let value = h.call("consume", 42);
    assert_eq!(rt::read_int(value), 44);
    assert_eq!(rt::allocations(), 3);
    rt::fai_drop(value);
}

#[test]
fn direct_error_needs_no_wrapper_allocation() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    rt::reset_allocations();
    let value = h.call("consume", -1);
    assert_eq!(rt::read_int(value), 3);
    assert_eq!(rt::allocations(), 0);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn first_class_result_reassembles_and_releases_its_payload() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    rt::reset_allocations();
    let value = h.call("boxed", 42);
    assert_eq!(rt::allocations(), 3);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn unused_success_fields_release_their_owners() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("ignore", 42);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn full_width_success_values_keep_wrapping_integer_semantics() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("consume", i64::MAX);
    assert_eq!(rt::read_int(value), i64::MAX.wrapping_add(19));
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn rematerialized_results_retain_components_for_later_reads() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("rematerialized", 42);
    assert_eq!(rt::read_int(value), 88);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn rematerialized_full_width_components_keep_their_owners() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("rematerialized", i64::MAX);
    assert_eq!(rt::read_int(value), i64::MAX.wrapping_add(19).wrapping_mul(2));
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn boxed_input_success_is_exploded_without_losing_ownership() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("passthrough", 42);
    assert_eq!(rt::read_int(value), 44);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn boxed_input_error_never_reads_success_tuple_fields() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("passthrough", -1);
    assert_eq!(rt::read_int(value), 3);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn exactly_returned_recursive_results_keep_constant_stack() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("recursive", 100_000);
    assert_eq!(rt::read_int(value), 43);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn tuple_error_payloads_keep_their_tag_and_fields() {
    let mut h = Harness::new();
    let value = h.call("errorFields", 37);
    assert_eq!(rt::read_int(value), 42);
    rt::fai_drop(value);
}

#[test]
fn wide_results_and_spilled_arguments_agree_on_the_calling_convention() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("wideFields", 3);
    assert_eq!(rt::read_int(value), 42);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn result_decomposition_preserves_success_effect_order() {
    let mut h = Harness::new();
    rt::capture_start();
    let value = h.call("effects", 37);
    let output = rt::capture_take();
    assert_eq!(rt::read_int(value), 42);
    rt::fai_drop(value);
    assert_eq!(output, "produce\nconsume\n");
}

#[test]
fn result_decomposition_preserves_error_effect_order() {
    let mut h = Harness::new();
    rt::capture_start();
    let value = h.call("effects", 0);
    let output = rt::capture_take();
    assert_eq!(rt::read_int(value), 4);
    rt::fai_drop(value);
    assert_eq!(output, "produce\nconsume\n");
}

#[test]
fn unsplit_float_tuples_preserve_nan_payloads_and_negative_zero() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let bits = 0x7ff8_0000_0000_0042;
    let value = h.call("floatBits", bits);
    assert_eq!(rt::read_int(value), bits);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn shared_results_reassemble_for_structural_observation() {
    let mut h = Harness::new();
    let baseline = (rt::live_count(), rt::live_bytes());
    let value = h.call("shared", 42);
    assert_eq!(rt::read_int(value), 42);
    rt::fai_drop(value);
    assert_eq!((rt::live_count(), rt::live_bytes()), baseline);
}

#[test]
fn result_abi_preserves_the_cross_module_body_edit_firewall() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let old = "module Producer\npublic make : Int -> Result (Int * String) String\nlet make n = Ok (n + 1, \"text\")\n";
    let new = old.replace("n + 1", "n + 2");
    let producer_id = db.add_source("Producer.fai".into(), old.into());
    let caller_id = db.add_source("Caller.fai".into(), "module Caller\npublic run : Int -> Int\nlet run n = match Producer.make n with | Ok (value, _) -> value | Err _ -> 0\n".into());
    let producer = db.source_file(producer_id).unwrap();
    let caller = db.source_file(caller_id).unwrap();
    let before_abi = fai_core::abi::abi(&db, producer, Symbol::intern("make"));
    let before = fai_driver::object_code(&db, caller, Symbol::intern("run"), false);
    db.enable_event_log();
    producer.set_text(&mut db).to(new);
    let after = fai_driver::object_code(&db, caller, Symbol::intern("run"), false);
    assert_eq!(before, after);
    assert_eq!(before_abi, fai_core::abi::abi(&db, producer, Symbol::intern("make")));
    assert!(!db.take_events().iter().any(|event| event.contains("object_code")));
    let mut clean = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut clean);
    clean.add_source("Producer.fai".into(), producer.text(&db).clone());
    let clean_id = clean.add_source("Caller.fai".into(), caller.text(&db).clone());
    let clean_object = fai_driver::object_code(
        &clean,
        clean.source_file(clean_id).unwrap(),
        Symbol::intern("run"),
        false,
    );
    assert_eq!(after, clean_object, "incremental and clean callers must agree");
}
