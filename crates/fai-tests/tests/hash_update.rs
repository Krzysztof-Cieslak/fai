//! Single-probe value updates preserve table contents, capacity and ownership.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use fai_core::{CExpr, ExprKind as K};
use fai_db::{Db, FaiDatabase, Setter};
use fai_resolve::LocalId;
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = "module M\npublic empty : Unit -> HashDict Int Int\nlet empty u = HashDict.empty\npublic insert : Int -> Int -> HashDict Int Int -> HashDict Int Int\nlet insert k v d = HashDict.insert k v d\npublic bump : Int -> HashDict Int Int -> HashDict Int Int\nlet bump k d = HashDict.updateOr 0 (fun n -> n + 1) k d\npublic defaulted : Int -> Int -> HashDict Int Int -> HashDict Int Int\nlet defaulted v k d = HashDict.updateOr v (fun n -> n + 1) k d\npublic remove : Int -> HashDict Int Int -> HashDict Int Int\nlet remove k d = HashDict.remove k d\npublic get : Int -> HashDict Int Int -> Int\nlet get k d = HashDict.getOr (-999) k d\npublic size : HashDict Int Int -> Int\nlet size d = HashDict.size d\npublic capacity : HashDict Int Int -> Int\nlet capacity d = HashDict.testCapacity d\npublic main : Runtime -> Unit\nlet main r = ()\n";

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let file = db
            .all_source_files()
            .into_iter()
            .find(|f| f.path(&db).ends_with("/HashDict.fai"))
            .unwrap();
        let source = format!(
            "{}\npublic testCapacity : HashDict 'k 'v -> Int\nlet testCapacity d = match d with | HD n slots -> Array.length slots\n",
            file.text(&db)
        );
        file.set_text(&mut db).to(source);
        let id = db.add_source("M.fai".into(), SOURCE.into());
        let program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
            .unwrap_or_else(|d| panic!("{d:?}"));
        Self { program, _guard: guard }
    }

    fn call(&mut self, name: &str, args: &[rt::Value]) -> rt::Value {
        rt::apply(self.program.function(Symbol::intern(name)).unwrap(), args)
    }

    fn number(&mut self, name: &str, args: &[rt::Value]) -> i64 {
        let value = self.call(name, args);
        let result = rt::read_int(value);
        rt::fai_drop(value);
        result
    }

    fn table(&mut self, keys: &[i64]) -> rt::Value {
        let mut table = self.call("empty", &[rt::FAI_UNIT]);
        for &key in keys {
            table = self.call("insert", &[rt::make_int(key), rt::make_int(10), table]);
        }
        table
    }

    fn verify(&mut self, table: rt::Value, expected: &BTreeMap<i64, i64>) {
        assert_eq!(self.number("size", &[rt::fai_dup(table)]), expected.len() as i64);
        for (&key, &value) in expected {
            assert_eq!(self.number("get", &[rt::make_int(key), rt::fai_dup(table)]), value);
        }
    }
}

#[test]
fn existing_key_at_load_threshold_does_not_grow() {
    let mut h = Harness::new();
    let table = h.table(&[0, 1, 2, 3, 4, 5]);
    assert_eq!(h.number("capacity", &[rt::fai_dup(table)]), 8);
    let updated = h.call("bump", &[rt::make_int(0), table]);
    assert_eq!(h.number("capacity", &[rt::fai_dup(updated)]), 8);
    assert_eq!(h.number("get", &[rt::make_int(0), rt::fai_dup(updated)]), 11);
    assert_eq!(h.number("size", &[rt::fai_dup(updated)]), 6);
    rt::fai_drop(updated);
}

#[test]
fn missing_key_at_load_threshold_grows_once() {
    let mut h = Harness::new();
    let table = h.table(&[0, 1, 2, 3, 4, 5]);
    let updated = h.call("bump", &[rt::make_int(6), table]);
    assert_eq!(h.number("capacity", &[rt::fai_dup(updated)]), 16);
    assert_eq!(h.number("get", &[rt::make_int(6), rt::fai_dup(updated)]), 1);
    assert_eq!(h.number("size", &[rt::fai_dup(updated)]), 7);
    rt::fai_drop(updated);
}

#[test]
fn fallback_is_used_on_a_miss_and_existing_value_on_a_hit() {
    let mut h = Harness::new();
    let table = h.table(&[]);
    let table = h.call("defaulted", &[rt::make_int(19), rt::make_int(1), table]);
    assert_eq!(h.number("get", &[rt::make_int(1), rt::fai_dup(table)]), 20);
    let table = h.call("defaulted", &[rt::make_int(999), rt::make_int(1), table]);
    assert_eq!(h.number("get", &[rt::make_int(1), rt::fai_dup(table)]), 21);
    rt::fai_drop(table);
}

#[test]
fn shared_replacement_preserves_the_original_table() {
    let mut h = Harness::new();
    let original = h.table(&[0, 1, 2, 3, 4, 5]);
    rt::reset_allocations();
    let updated = h.call("bump", &[rt::make_int(0), rt::fai_dup(original)]);
    assert_eq!(rt::array_copies(), 1);
    assert_eq!(h.number("get", &[rt::make_int(0), rt::fai_dup(original)]), 10);
    assert_eq!(h.number("get", &[rt::make_int(0), rt::fai_dup(updated)]), 11);
    rt::fai_drop(original);
    rt::fai_drop(updated);
}

#[test]
fn unique_replacements_allocate_only_entry_cells() {
    let mut h = Harness::new();
    let mut table = h.table(&[0, 1, 2, 3, 4, 5]);
    rt::reset_allocations();
    for _ in 0..100 {
        table = h.call("bump", &[rt::make_int(0), table]);
    }
    assert_eq!(rt::array_copies(), 0);
    assert!(rt::allocations() <= 101, "{} allocations", rt::allocations());
    assert_eq!(h.number("get", &[rt::make_int(0), rt::fai_dup(table)]), 110);
    rt::fai_drop(table);
}

#[test]
fn wrapped_collision_chains_survive_deletion_and_updates() {
    let mut h = Harness::new();
    let keys: Vec<_> = (0..1000)
        .filter(|&key| {
            let hash = rt::fai_hash_borrowed(rt::make_int(key));
            let bucket = rt::read_int(hash) & 7;
            rt::fai_drop(hash);
            bucket == 7
        })
        .take(5)
        .collect();
    assert_eq!(keys.len(), 5);
    let mut table = h.table(&keys);
    table = h.call("remove", &[rt::make_int(keys[2]), table]);
    table = h.call("bump", &[rt::make_int(keys[4]), table]);
    table = h.call("bump", &[rt::make_int(keys[2]), table]);
    let mut expected: BTreeMap<_, _> = keys.iter().map(|&k| (k, 10)).collect();
    expected.insert(keys[4], 11);
    expected.insert(keys[2], 1);
    h.verify(table, &expected);
    rt::fai_drop(table);
}

#[test]
fn signed_full_width_keys_keep_their_identity() {
    let mut h = Harness::new();
    let mut table = h.table(&[i64::MIN, -1, 0, i64::MAX]);
    table = h.call("bump", &[rt::make_int(i64::MIN), table]);
    h.verify(table, &BTreeMap::from([(i64::MIN, 11), (-1, 10), (0, 10), (i64::MAX, 10)]));
    rt::fai_drop(table);
}

fn callback_calls(e: &CExpr, callback: LocalId) -> usize {
    let sum = |values: &[CExpr]| values.iter().map(|e| callback_calls(e, callback)).sum::<usize>();
    match &e.kind {
        K::App { func, args, .. } => {
            usize::from(matches!(func.kind, K::Local(l) if l == callback))
                + callback_calls(func, callback)
                + sum(args)
        }
        K::If { cond, then, els } => {
            callback_calls(cond, callback)
                + callback_calls(then, callback).max(callback_calls(els, callback))
        }
        K::Let { value, body, .. } => {
            callback_calls(value, callback) + callback_calls(body, callback)
        }
        K::Prim { args, .. } | K::MakeData { args, .. } => sum(args),
        K::DataTag { base, .. } | K::DataField { base, .. } => callback_calls(base, callback),
        K::Lit(_) | K::Local(_) | K::Global(_) | K::MakeClosure { .. } | K::Error => 0,
        other => panic!("unexpected pre-RC node: {other:?}"),
    }
}

#[test]
fn update_has_one_hash_and_one_probe_load_per_step() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let file =
        db.all_source_files().into_iter().find(|f| f.path(&db).ends_with("/HashDict.fai")).unwrap();
    let root = fai_core::core_inlined(&db, file, Symbol::intern("updateOr"));
    let probe = fai_core::core_inlined(&db, file, Symbol::intern("updateIndex"));
    let root_ir = fai_core::pretty_def(&root);
    let probe_ir = fai_core::pretty_def(&probe);
    assert_eq!(root_ir.matches("(hash ").count(), 1, "{root_ir}");
    assert_eq!(probe_ir.matches("(hash ").count(), 0, "{probe_ir}");
    assert_eq!(probe_ir.matches("(arrayGet ").count(), 1, "{probe_ir}");
    assert!(!probe_ir.contains("@insert") && !probe_ir.contains("@getOr"));
    assert_eq!(callback_calls(&root.entry().body, root.entry().params[1]), 1);
    assert_eq!(callback_calls(&probe.entry().body, probe.entry().params[1]), 0);
}

#[track_caller]
fn typed(body: &str) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let body = body.lines().map(|l| format!("  {l}\n")).collect::<String>();
    let id = db.add_source(
        "Main.fai".into(),
        format!("module Main\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n{body}"),
    );
    rt::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    assert_eq!(result.exit_code, 0);
    assert_eq!(rt::capture_take(), "ok\n");
}

#[test]
fn float_bitwise_keys_remain_distinct() {
    typed(
        "let a = HashDict.updateOr 0 (fun n -> n + 1) 0.0 HashDict.empty\nlet b = HashDict.updateOr 0 (fun n -> n + 1) (-0.0) a\nlet c = HashDict.updateOr 0 (fun n -> n + 1) (Float.fromBits 0x7ff8000000000001) b\nlet good = HashDict.size c = 3 && HashDict.getOr 0 0.0 c = 1 && HashDict.getOr 0 (-0.0) c = 1\nr.console.writeLine (if good then \"ok\" else \"wrong\")",
    );
}

#[test]
fn independently_shared_values_keep_their_old_contents() {
    typed(
        "let values = [| 1, 2 |]\nlet old = HashDict.singleton 1 values\nlet updated = HashDict.updateOr [||] (Array.push 3) 1 old\nlet good = Array.toList values = [1, 2] && Array.toList (HashDict.getOr [||] 1 old) = [1, 2] && Array.toList (HashDict.getOr [||] 1 updated) = [1, 2, 3]\nr.console.writeLine (if good then \"ok\" else \"wrong\")",
    );
}

#[test]
fn updater_requires_a_pure_arrow() {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), "module M\neffect : Int -> Int / { Console }\nlet effect n =\n  let _ = stdConsole.writeLine \"called\"\n  n + 1\nlet invalid = HashDict.updateOr 0 effect 1 HashDict.empty\n".into());
    let diagnostics = fai_tests::check_source_diagnostics(&db, db.source_file(id).unwrap());
    assert!(diagnostics.iter().any(|d| d.code.as_str() == "FAI3001"), "{diagnostics:?}");
}

#[test]
fn standard_and_sample_update_contracts_hold() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let library =
        db.all_source_files().into_iter().find(|f| f.path(&db).ends_with("/HashDict.fai")).unwrap();
    let id = db.add_source(
        "HashCounts.fai".into(),
        include_str!("../../../samples/HashCounts.fai").into(),
    );
    let sample = db.source_file(id).unwrap();
    let result = fai_driver::test(&db, &[library, sample], None, fai_driver::TestConfig::default());
    assert!(result.ok, "{:?}", result.diagnostics);
    assert!(result.passed > 0);
}

mod proptests {
    use super::*;
    use proptest::prelude::*;
    use std::cell::RefCell;

    #[test]
    fn update_and_delete_traces_match_an_ordered_map() {
        let harness = RefCell::new(Harness::new());
        proptest::test_runner::TestRunner::default()
            .run(&prop::collection::vec((-12i64..13, any::<bool>()), 0..100), |ops| {
                let mut h = harness.borrow_mut();
                let baseline = rt::live_count();
                let mut table = h.table(&[]);
                let mut expected = BTreeMap::new();
                for (key, remove) in ops {
                    if remove {
                        table = h.call("remove", &[rt::make_int(key), table]);
                        expected.remove(&key);
                    } else {
                        table = h.call("bump", &[rt::make_int(key), table]);
                        *expected.entry(key).or_insert(0) += 1;
                    }
                }
                h.verify(table, &expected);
                rt::fai_drop(table);
                prop_assert_eq!(rt::live_count(), baseline);
                Ok(())
            })
            .unwrap();
    }
}
