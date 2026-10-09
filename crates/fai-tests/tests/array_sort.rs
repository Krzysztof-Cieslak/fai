//! Correctness, comparison budgets, and ownership of standard array sorting.

use std::sync::{Mutex, MutexGuard};

use fai_db::{Db, FaiDatabase, Setter};
use fai_runtime as rt;
use fai_syntax::Symbol;

static LOCK: Mutex<()> = Mutex::new(());
const SOURCE: &str = r#"module Main
cmp : Console -> Int -> Int -> Int / { Console }
let cmp console a b =
  let _ = console.write "c"
  compare a b
public sort : Array Int -> Array Int
let sort xs = Array.sort xs
public sortBy : Array Int -> Array Int / { Console }
let sortBy xs = Array.sortBy (cmp stdConsole) xs
public heap : Array Int -> Array Int
let heap xs = Array.testSortBudget 0 xs
public heapBy : Array Int -> Array Int / { Console }
let heapBy xs = Array.testSortBudgetBy (cmp stdConsole) 0 xs
public limited : Array Int -> Array Int
let limited xs = Array.testSortBudget 1 xs
public limitedBy : Array Int -> Array Int / { Console }
let limitedBy xs = Array.testSortBudgetBy (cmp stdConsole) 1 xs
public descending : Array Int -> Array Int / { Console }
let descending xs = Array.sortBy (fun a b -> cmp stdConsole b a) xs
public main : Runtime -> Unit
let main r = ()
"#;

struct Harness {
    program: fai_driver::CompiledProgram,
    _guard: MutexGuard<'static, ()>,
}

fn read_array(value: rt::Value) -> Vec<i64> {
    let length = rt::fai_array_length_borrowed(value);
    let count = rt::read_int(length);
    rt::fai_drop(length);
    (0..count)
        .map(|index| {
            let element = rt::fai_array_get_borrowed(value, rt::make_int(index));
            let number = rt::read_int(element);
            rt::fai_drop(element);
            number
        })
        .collect()
}

impl Harness {
    fn new() -> Self {
        let guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut db = FaiDatabase::new();
        let ids = fai_types::std_lib::load_std(&mut db);
        let array = ids
            .into_iter()
            .filter_map(|id| db.source_file(id))
            .find(|file| file.path(&db).ends_with("/Array.fai"))
            .unwrap();
        let source = format!(
            "{}\npublic testSortBudget : Int -> Array 'a -> Array 'a\nlet testSortBudget depth xs = qsortOrd depth 0 (length xs) xs\npublic testSortBudgetBy : ('a -> 'a -> Int / 'e) -> Int -> Array 'a -> Array 'a / 'e\nlet testSortBudgetBy cmp depth xs = qsort cmp depth 0 (length xs) xs\n",
            array.text(&db)
        );
        array.set_text(&mut db).to(source);
        let id = db.add_source("Main.fai".into(), SOURCE.into());
        let file = db.source_file(id).unwrap();
        let program = fai_driver::jit_compile(&db, file).unwrap_or_else(|d| panic!("{d:?}"));
        Self { program, _guard: guard }
    }

    fn sort(&mut self, name: &str, values: &[i64], shared: bool) -> (usize, i64) {
        let baseline = rt::live_count();
        let array = values.iter().fold(
            rt::fai_array_with_capacity(rt::make_int(values.len() as i64)),
            |array, &value| rt::fai_array_push(array, rt::make_int(value)),
        );
        let original = shared.then(|| rt::fai_dup(array));
        let function = self.program.function(Symbol::intern(name)).unwrap();
        rt::capture_start();
        rt::reset_allocations();
        let sorted = rt::apply(rt::fai_dup(function), &[array]);
        let copies = rt::array_copies();
        let comparisons = rt::capture_take().len();
        let mut expected = values.to_vec();
        expected.sort_unstable();
        if name == "descending" {
            expected.reverse();
        }
        assert_eq!(read_array(sorted), expected);
        if let Some(original) = original {
            assert_eq!(read_array(original), values);
            rt::fai_drop(original);
        } else {
            assert_eq!(sorted, array, "a unique input keeps its backing buffer");
        }
        rt::fai_drop(sorted);
        assert_eq!(rt::live_count(), baseline);
        (comparisons, copies)
    }
}

#[test]
fn all_equal_sort_by_has_linear_comparison_work() {
    let size = 512;
    let (comparisons, copies) = Harness::new().sort("sortBy", &vec![7; size], false);
    assert!(comparisons <= 2 * size + 8, "{comparisons} comparisons for {size} equal elements");
    assert_eq!(copies, 0);
}

#[test]
fn all_equal_structural_sort_stays_in_place() {
    assert_eq!(Harness::new().sort("sort", &vec![7; 2048], false).1, 0);
}

#[test]
fn two_key_sort_by_has_linear_comparison_work() {
    let values: Vec<_> = (0..2048).map(|i| i % 2).collect();
    let (comparisons, copies) = Harness::new().sort("sortBy", &values, false);
    assert!(comparisons <= 3 * values.len() + 12, "{comparisons}");
    assert_eq!(copies, 0);
}

#[test]
fn two_key_structural_sort_stays_in_place() {
    let values: Vec<_> = (0..2048).map(|i| i % 2).collect();
    assert_eq!(Harness::new().sort("sort", &values, false).1, 0);
}

#[test]
fn skewed_duplicates_have_bounded_comparison_work() {
    let values: Vec<_> = (0..2048).map(|i| i64::from(i % 97 == 0)).collect();
    let (comparisons, copies) = Harness::new().sort("sortBy", &values, false);
    assert!(comparisons <= 3 * values.len() + 12, "{comparisons}");
    assert_eq!(copies, 0);
}

#[test]
fn sorted_input_is_sorted_without_copying() {
    let values: Vec<_> = (0..1024).collect();
    assert_eq!(Harness::new().sort("sort", &values, false).1, 0);
}

#[test]
fn reversed_input_has_bounded_comparison_work() {
    let values: Vec<_> = (0..1024).rev().collect();
    let (comparisons, copies) = Harness::new().sort("sortBy", &values, false);
    assert!(comparisons <= 40 * values.len(), "{comparisons}");
    assert_eq!(copies, 0);
}

#[test]
fn shared_sort_copies_once_and_preserves_the_original() {
    assert_eq!(Harness::new().sort("sort", &[9, 1, 3, 1, 0, 8], true).1, 1);
}

#[test]
fn shared_sort_by_copies_once_and_preserves_the_original() {
    assert_eq!(Harness::new().sort("sortBy", &[9, 1, 3, 1, 0, 8], true).1, 1);
}

#[test]
fn comparator_direction_is_honored() {
    assert_eq!(Harness::new().sort("descending", &[1, 3, 2, 2, 0], false).1, 0);
}

#[test]
fn heap_fallback_sorts_in_place() {
    let values: Vec<_> = (0..1024).map(|i| (i * 73 + 11) % 101).collect();
    assert_eq!(Harness::new().sort("heap", &values, false).1, 0);
}

#[test]
fn comparator_heap_fallback_sorts_in_place() {
    let values: Vec<_> = (0..1024).map(|i| (i * 73 + 11) % 101).collect();
    let (comparisons, copies) = Harness::new().sort("heapBy", &values, false);
    assert!(comparisons <= 30 * values.len(), "{comparisons}");
    assert_eq!(copies, 0);
}

#[test]
fn structural_heap_fallback_honors_partition_offsets() {
    let values: Vec<_> = (0..1024).map(|i| (i * 73 + 11) % 101).collect();
    assert_eq!(Harness::new().sort("limited", &values, false).1, 0);
}

#[test]
fn comparator_heap_fallback_honors_partition_offsets() {
    let values: Vec<_> = (0..1024).map(|i| (i * 73 + 11) % 101).collect();
    assert_eq!(Harness::new().sort("limitedBy", &values, false).1, 0);
}

#[test]
fn empty_structural_sort_keeps_its_buffer() {
    assert_eq!(Harness::new().sort("sort", &[], false).1, 0);
}

#[test]
fn singleton_structural_sort_keeps_its_buffer() {
    assert_eq!(Harness::new().sort("sort", &[i64::MIN], false).1, 0);
}

#[test]
fn shared_ascending_run_needs_no_copy() {
    assert_eq!(Harness::new().sort("sort", &(0..512).collect::<Vec<_>>(), true).1, 0);
}

#[test]
fn unique_descending_run_reverses_in_place() {
    assert_eq!(Harness::new().sort("sort", &(0..512).rev().collect::<Vec<_>>(), false).1, 0);
}

#[test]
fn shared_descending_run_preserves_its_original() {
    assert_eq!(Harness::new().sort("sort", &(0..512).rev().collect::<Vec<_>>(), true).1, 1);
}

#[test]
fn equal_prefix_can_start_a_descending_run() {
    assert_eq!(Harness::new().sort("sort", &[3, 3, 2, 2, 1, 1], false).1, 0);
}

#[test]
fn late_run_violation_falls_back_to_general_sorting() {
    let mut values: Vec<_> = (0..128).collect();
    values.push(-1);
    assert_eq!(Harness::new().sort("sort", &values, false).1, 0);
}

#[test]
fn organ_pipe_input_remains_sorted_and_in_place() {
    let values = (0..64).chain((0..64).rev()).collect::<Vec<_>>();
    assert_eq!(Harness::new().sort("sort", &values, false).1, 0);
}

#[test]
fn full_width_integer_order_is_preserved() {
    assert_eq!(Harness::new().sort("sort", &[i64::MAX, 0, i64::MIN, -1, i64::MAX], false).1, 0);
}

#[test]
fn partition_at_insertion_threshold_is_correct() {
    let values = (0..16).map(|i| (i * 11) % 17).collect::<Vec<_>>();
    assert_eq!(Harness::new().sort("sort", &values, false).1, 0);
}

#[test]
fn partition_above_insertion_threshold_is_correct() {
    let values = (0..17).map(|i| (i * 11) % 18).collect::<Vec<_>>();
    assert_eq!(Harness::new().sort("sort", &values, false).1, 0);
}

#[track_caller]
fn typed_sort(body: &str) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Key.fai".into(), "module Key\npublic opaque type Key = | Value Int\npublic make : Int -> Key\nlet make x = Value x\npublic value : Key -> Int\nlet value k = match k with | Value x -> x\n".into());
    let body = body.lines().map(|l| format!("  {l}\n")).collect::<String>();
    let id = db.add_source(
        "Main.fai".into(),
        format!("module Main\npublic main : Runtime -> Unit / {{ Console }}\nlet main r =\n{body}"),
    );
    rt::capture_start();
    let outcome = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(rt::capture_take(), "ok\n");
}

#[test]
fn float_runs_use_total_bitwise_order() {
    typed_sort(
        "let bits = [| 0x7ff8000000000001, 0x7ff0000000000000, 0, 0x8000000000000000, 0xfff0000000000000, 0xfff8000000000001 |]\nlet sorted = Array.toList (Array.map Float.toBits (Array.sort (Array.map Float.fromBits bits)))\nlet good = sorted = [0xfff8000000000001, 0xfff0000000000000, 0x8000000000000000, 0, 0x7ff0000000000000, 0x7ff8000000000001]\nr.console.writeLine (if good then \"ok\" else \"wrong\")",
    );
}

#[test]
fn boxed_records_and_retained_aliases_keep_their_contents() {
    typed_sort(
        "let original = [| { key = 2, text = \"two\" }, { key = 1, text = \"one\" } |]\nlet sorted = Array.sort original\nlet good = (Array.unsafeGet 0 original).text = \"two\" && (Array.unsafeGet 0 sorted).text = \"one\"\nr.console.writeLine (if good then \"ok\" else \"wrong\")",
    );
}

#[test]
fn opaque_comparable_values_sort_by_their_representation() {
    typed_sort(
        "let values = Array.map Key.make [| 3, 1, 2 |]\nlet sorted = Array.toList (Array.map Key.value (Array.sort values))\nr.console.writeLine (if sorted = [1, 2, 3] then \"ok\" else \"wrong\")",
    );
}

#[test]
fn standard_array_contracts_hold() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut db = FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|file| file.path(&db).ends_with("/Array.fai"))
        .unwrap();
    let result = fai_driver::test(&db, &[file], None, fai_driver::TestConfig::default());
    assert!(result.ok, "{:?}", result.diagnostics);
    assert!(result.passed > 0);
}

#[test]
fn native_sorts_preserve_shared_duplicate_inputs() {
    use std::process::{Command, Stdio};
    use wait_timeout::ChildExt;

    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let source = "module Main\npublic main : Runtime -> Unit / { Console }\nlet main r =\n  let input = Array.init 2048 (fun i -> i % 3)\n  let sorted = Array.toList (Array.sort input)\n  let sortedBy = Array.toList (Array.sortBy compare input)\n  let expected = List.sort (Array.toList input)\n  r.console.writeLine (if sorted = expected && sortedBy = expected && Array.unsafeGet 1 input = 1 then \"ok\" else \"failed\")\n";
    let id = db.add_source("Main.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let path = std::env::temp_dir().join(format!("fai-array-sort-{}", std::process::id()));
    let path = camino::Utf8PathBuf::from_path_buf(path).unwrap();
    let outcome = fai_driver::build_native(&db, file, &path);
    assert!(outcome.ok, "{:?}", outcome.diagnostics);
    let artifact = outcome.artifact.unwrap();
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

mod proptests {
    use std::cell::RefCell;

    use proptest::prelude::*;
    use proptest::test_runner::TestRunner;

    use super::*;

    #[test]
    fn all_sort_paths_match_vec_on_duplicate_heavy_arrays() {
        let harness = RefCell::new(Harness::new());
        let input = (0usize..6, any::<bool>(), proptest::collection::vec(-8i64..8, 0..128));
        TestRunner::default()
            .run(&input, |(path, shared, values)| {
                let name = ["sort", "sortBy", "heap", "heapBy", "limited", "limitedBy"][path];
                let (_, copies) = harness.borrow_mut().sort(name, &values, shared);
                if !shared {
                    prop_assert_eq!(copies, 0);
                }
                Ok(())
            })
            .unwrap();
    }
}
