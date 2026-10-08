//! Structural and content invariants of bulk ordered-tree operations.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase, Setter};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module Main
let range a b = List.range (if a < b then a else b) (if a < b then b else a)
public emptyDict : Unit -> Dict Int Int
let emptyDict u = Dict.empty
public emptySet : Unit -> Set Int
let emptySet u = Set.empty
public stepDict : Int -> Int -> Int -> Dict Int Int -> Dict Int Int
let stepDict op a b tree =
  if op = 0 then Dict.insert a b tree else if op = 1 then Dict.remove a tree else if op = 2 then Dict.filter (fun key value -> key < a || key = b) tree else
    let other = Dict.fromList (List.map (fun key -> (key, key * 3 + 1)) (range a b))
    if op = 3 then Dict.union tree other else if op = 4 then Dict.intersection tree other else Dict.difference tree other
public stepSet : Int -> Int -> Int -> Set Int -> Set Int
let stepSet op a b tree =
  if op = 0 then Set.insert a tree else if op = 1 then Set.remove a tree else if op = 2 then Set.filter (fun key -> key < a || key = b) tree else
    let other = Set.fromList (range a b)
    if op = 3 then Set.union tree other else if op = 4 then Set.intersection tree other else Set.difference tree other
public main : Runtime -> Unit
let main r = ()
"#;

struct Owned(rt::Value);

impl Owned {
    fn into_raw(self) -> rt::Value {
        let value = self.0;
        std::mem::forget(self);
        value
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        rt::fai_drop(self.0);
    }
}

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
        let program = fai_driver::jit_compile(&db, file).unwrap_or_else(|d| panic!("{d:?}"));
        Self { program, _guard: guard }
    }

    fn call(&mut self, name: &str, args: &[rt::Value]) -> Owned {
        let function = self.program.function(Symbol::intern(name)).unwrap();
        Owned(rt::apply(rt::fai_dup(function), args))
    }

    fn empty(&mut self, dict: bool) -> Owned {
        self.call(if dict { "emptyDict" } else { "emptySet" }, &[rt::FAI_UNIT])
    }

    fn step(&mut self, dict: bool, op: i64, a: i64, b: i64, tree: Owned) -> Owned {
        self.call(
            if dict { "stepDict" } else { "stepSet" },
            &[rt::make_int(op), rt::make_int(a), rt::make_int(b), tree.into_raw()],
        )
    }
}

fn int_field(tree: rt::Value, index: i64) -> i64 {
    let field = Owned(rt::fai_data_field(tree, index));
    rt::read_int(field.0)
}

/// Inspect the private runtime shape, checking every node rather than trusting
/// cached sizes or validating only the externally visible key sequence.
fn validate(
    tree: rt::Value,
    dict: bool,
    lower: Option<i64>,
    upper: Option<i64>,
) -> (BTreeMap<i64, i64>, usize) {
    if rt::data_tag_of(tree) == 0 {
        return (BTreeMap::new(), 0);
    }
    assert_eq!(rt::data_tag_of(tree), 1);
    let cached = int_field(tree, 0);
    let left = Owned(rt::fai_data_field(tree, 1));
    let key = int_field(tree, 2);
    let value = if dict { int_field(tree, 3) } else { 0 };
    let right = Owned(rt::fai_data_field(tree, if dict { 4 } else { 3 }));
    assert!(lower.is_none_or(|bound| bound < key));
    assert!(upper.is_none_or(|bound| key < bound));
    let (mut left_entries, left_height) = validate(left.0, dict, lower, Some(key));
    let (right_entries, right_height) = validate(right.0, dict, Some(key), upper);
    let sl = left_entries.len();
    let sr = right_entries.len();
    assert_eq!(cached, (sl + sr + 1) as i64, "cached size at key {key}");
    assert!(
        sl + sr <= 1 || (sl <= 3 * sr && sr <= 3 * sl),
        "unbalanced key {key}: left {sl}, right {sr}"
    );
    left_entries.insert(key, value);
    left_entries.extend(right_entries);
    (left_entries, 1 + left_height.max(right_height))
}

#[track_caller]
fn filter_regression(dict: bool) {
    let mut harness = Harness::new();
    let baseline = rt::live_count();
    let mut tree = harness.empty(dict);
    for key in 0..14 {
        tree = harness.step(dict, 0, key, key, tree);
    }
    tree = harness.step(dict, 2, 7, 13, tree);
    let (actual, height) = validate(tree.0, dict, None, None);
    let expected = (0..14)
        .filter(|key| *key < 7 || *key == 13)
        .map(|key| (key, if dict { key } else { 0 }))
        .collect();
    assert_eq!(actual, expected);
    assert!(height <= 5);
    drop(tree);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn dict_filter_preserves_weight_balance_after_large_shrinkage() {
    filter_regression(true);
}

#[test]
fn set_filter_preserves_weight_balance_after_large_shrinkage() {
    filter_regression(false);
}

fn model_step(tree: &mut BTreeMap<i64, i64>, dict: bool, op: i64, a: i64, b: i64) {
    let low = a.min(b);
    let high = a.max(b);
    match op {
        0 => {
            tree.insert(a, if dict { b } else { 0 });
        }
        1 => {
            tree.remove(&a);
        }
        2 => tree.retain(|key, _| *key < a || *key == b),
        3 => {
            for key in low..high {
                tree.entry(key).or_insert(if dict { key * 3 + 1 } else { 0 });
            }
        }
        4 => tree.retain(|key, _| (low..high).contains(key)),
        _ => tree.retain(|key, _| !(low..high).contains(key)),
    }
}

fn mixed_operations(harness: &mut Harness, dict: bool, operations: &[(i64, i64, i64)]) {
    let baseline = rt::live_count();
    let mut tree = harness.empty(dict);
    let mut expected = BTreeMap::new();
    for &(op, a, b) in operations {
        tree = harness.step(dict, op, a, b, tree);
        model_step(&mut expected, dict, op, a, b);
        let (actual, _) = validate(tree.0, dict, None, None);
        assert_eq!(actual, expected, "after operation {op}({a}, {b})");
    }
    drop(tree);
    assert_eq!(rt::live_count(), baseline);
}

#[track_caller]
fn work_after_bulk_filter(dict: bool) {
    let mut harness = Harness::new();
    let baseline = rt::live_count();
    let mut tree = harness.empty(dict);
    for key in 0..1024 {
        tree = harness.step(dict, 0, key, key, tree);
    }
    rt::reset_allocations();
    tree = harness.step(dict, 2, 512, 1023, tree);
    let allocations = rt::allocations();
    let (entries, height) = validate(tree.0, dict, None, None);
    assert_eq!(entries.len(), 513);
    assert!(height <= 20, "height {height}");
    assert!(
        allocations <= 2 * entries.len() as i64,
        "bulk reconstruction must stay linear, allocated {allocations}"
    );
    println!("bulk filter: height={height}, allocations={allocations}");
    drop(tree);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn dict_bulk_filter_bounds_allocations_and_height() {
    work_after_bulk_filter(true);
}

#[test]
fn set_bulk_filter_bounds_allocations_and_height() {
    work_after_bulk_filter(false);
}

#[track_caller]
fn shared_filter(dict: bool) {
    let mut harness = Harness::new();
    let baseline = rt::live_count();
    let mut tree = harness.empty(dict);
    for key in 0..14 {
        tree = harness.step(dict, 0, key, key, tree);
    }
    let original = Owned(rt::fai_dup(tree.0));
    tree = harness.step(dict, 2, 7, 13, tree);
    assert_eq!(validate(original.0, dict, None, None).0.len(), 14);
    assert_eq!(validate(tree.0, dict, None, None).0.len(), 8);
    drop(tree);
    drop(original);
    assert_eq!(rt::live_count(), baseline);
}

#[test]
fn shared_dict_filter_preserves_both_balanced_versions() {
    shared_filter(true);
}

#[test]
fn shared_set_filter_preserves_both_balanced_versions() {
    shared_filter(false);
}

#[track_caller]
fn native_filter(dict: bool) {
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt;

    let mut db = FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let module = if dict { "Dict" } else { "Set" };
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|file| file.path(&db).ends_with(&format!("/{module}.fai")))
        .unwrap();
    let ty = if dict { "Dict Int Int" } else { "Set Int" };
    let pattern = if dict { "DictNode n l k v r" } else { "SetNode n l k r" };
    let check = format!(
        "{}\npublic auditValid : {ty} -> Bool\nlet auditValid tree =\n  match tree with\n  | {module}Empty -> true\n  | {pattern} ->\n    let sl = size l\n    let sr = size r\n    n = sl + sr + 1 && (sl + sr <= 1 || (sl <= 3 * sr && sr <= 3 * sl)) && auditValid l && auditValid r\n",
        file.text(&db)
    );
    file.set_text(&mut db).to(check);
    let input = if dict {
        "Dict.fromList (List.map (fun k -> (k, k)) (List.range 0 14))"
    } else {
        "Set.fromList (List.range 0 14)"
    };
    let predicate = if dict { "fun k v -> k < 7 || k = 13" } else { "fun k -> k < 7 || k = 13" };
    let source = format!(
        "module Main\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n  let result = {module}.filter ({predicate}) ({input})\n  r.console.writeLine (if {module}.auditValid result && {module}.size result = 8 then \"ok\" else \"unbalanced\")\n"
    );
    let id = db.add_source("Main.fai".into(), source);
    let entry = db.source_file(id).unwrap();
    let path = std::env::temp_dir().join(format!("fai-bulk-{module}-{}", std::process::id()));
    let path = camino::Utf8PathBuf::from_path_buf(path).unwrap();
    let built = fai_driver::build_native(&db, entry, &path);
    assert!(built.ok, "{:?}", built.diagnostics);
    let artifact = built.artifact.unwrap();
    let mut child =
        Command::new(&artifact).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let finished = child.wait_timeout(std::time::Duration::from_secs(15)).unwrap().is_some();
    if !finished {
        child.kill().unwrap();
    }
    let output = child.wait_with_output().unwrap();
    std::fs::remove_file(artifact).unwrap();
    assert!(finished && output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(output.stdout, b"ok\n");
}

#[test]
fn native_dict_bulk_filter_preserves_weight_balance() {
    native_filter(true);
}

#[test]
fn native_set_bulk_filter_preserves_weight_balance() {
    native_filter(false);
}

mod proptests {
    use std::cell::RefCell;

    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    use super::*;

    fn operations() -> impl Strategy<Value = Vec<(i64, i64, i64)>> {
        proptest::collection::vec((0i64..6, -24i64..24, -24i64..24), 0..80)
    }

    #[test]
    fn mixed_dict_operations_preserve_contents_ordering_sizes_and_balance() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&operations(), |operations| {
                mixed_operations(&mut harness.borrow_mut(), true, &operations);
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn mixed_set_operations_preserve_contents_ordering_sizes_and_balance() {
        let harness = RefCell::new(Harness::new());
        TestRunner::default()
            .run(&operations(), |operations| {
                mixed_operations(&mut harness.borrow_mut(), false, &operations);
                Ok(())
            })
            .unwrap();
    }
}
