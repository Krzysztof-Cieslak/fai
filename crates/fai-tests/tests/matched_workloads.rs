//! Structural and negative controls for the source-matched benchmark algorithms.

use std::process::Command;
use std::sync::Mutex;

use fai_db::Db;
use fai_runtime as rt;
use fai_tests::algorithms::by_module;
use fai_tests::benchmark_aot::Entry;
use fai_tests::sorting;

static LOCK: Mutex<()> = Mutex::new(());

fn array_values(value: rt::Value) -> Vec<i64> {
    let length = rt::read_int(rt::fai_array_length_borrowed(value));
    let values = (0..length)
        .map(|index| {
            let item = rt::fai_array_get_borrowed(value, rt::make_int(index));
            let result = rt::read_int(item);
            rt::fai_drop(item);
            result
        })
        .collect();
    rt::fai_drop(value);
    values
}

#[track_caller]
fn check_merge(pattern: usize, n: usize) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let algorithm = by_module("MergeSort").unwrap();
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("MergeSort.fai".into(), algorithm.source().into());
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let generate = program.function(fai_syntax::Symbol::intern("generate")).unwrap();
    let sort = program.function(fai_syntax::Symbol::intern("sortValues")).unwrap();
    let input = sorting::input(pattern, n);
    let mut expected = input.clone();
    expected.sort();
    assert_eq!(sorting::merge_sort(input.clone()), expected);
    let source = rt::apply(generate, &[rt::make_int(pattern as i64), rt::make_int(n as i64)]);
    let result = rt::apply(sort, &[rt::fai_dup(source)]);
    assert_eq!(array_values(result), expected);
    assert_eq!(array_values(source), input, "sorting must preserve a shared input");

    let literal = |values: &[i64]| values.iter().map(i64::to_string).collect::<Vec<_>>().join("; ");
    let source = format!(
        "{}\nlet () =\n  let input = sort_input {pattern} {n} in\n  assert (Array.to_list input = [{}]);\n  assert (Array.to_list (merge_sort_values input) = [{}])\n",
        fai_tests::ocaml::isolated_source(algorithm, Entry::Once),
        literal(&input),
        literal(&expected)
    );
    if let Some(binary) = fai_tests::ocaml::build("matched-merge", &source) {
        let output = Command::new(binary).arg(n.to_string()).output().unwrap();
        fai_tests::benchmark_process::ExpectedAnswer::at_size(algorithm, n as i64)
            .verify("matched OCaml merge", &output)
            .unwrap();
    }
}

macro_rules! merge_cases {
    ($($name:ident => ($pattern:literal, $n:literal)),* $(,)?) => {
        $(#[test] fn $name() { check_merge($pattern, $n); })*
    };
}

merge_cases! {
    merge_empty => (2, 0),
    merge_singleton => (2, 1),
    merge_ascending => (0, 65),
    merge_descending => (1, 65),
    merge_shuffled => (2, 65),
    merge_equal => (3, 65),
    merge_few_keys => (4, 65),
    merge_partial_run => (5, 65),
}

#[test]
fn removing_the_merge_sort_changes_the_checked_answer() {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let algorithm = by_module("MergeSort").unwrap();
    let source =
        algorithm.source().replace("sortValues (generate pattern n)", "generate pattern n");
    assert_ne!(source, algorithm.source());
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("MergeSort.fai".into(), source);
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
        .unwrap_or_else(|errors| panic!("{errors:?}"));
    let run = program.function(fai_syntax::Symbol::intern("run")).unwrap();
    let answer = rt::apply(run, &[rt::make_int(65)]);
    let actual = rt::read_int(answer);
    rt::fai_drop(answer);
    assert_ne!(actual, sorting::checksum(&sorting::input(0, 65)));
}
