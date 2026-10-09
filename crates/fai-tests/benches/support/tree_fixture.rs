//! Untimed cross-language validation of the matched binary-tree fixture.

use std::sync::Mutex;

use fai_db::Db;
use fai_runtime as rt;
use fai_syntax::Symbol;
use fai_tests::tree_lookup::{self as tree, BinaryTree};

static LOCK: Mutex<()> = Mutex::new(());

fn list(mut value: rt::Value) -> Vec<i64> {
    let mut out = Vec::new();
    while rt::data_tag_of(value) != 0 {
        let head = rt::fai_data_field(value, 0);
        let tail = rt::fai_data_field(value, 1);
        out.push(rt::read_int(head));
        rt::fai_drop(head);
        rt::fai_drop(value);
        value = tail;
    }
    rt::fai_drop(value);
    out
}

/// Checks full preorder/null shape, dimensions, every hit/miss in the fixed
/// domain around the tree, and the weighted result before any timing begins.
#[track_caller]
pub fn validate(count: i64) {
    let _guard = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let reference = BinaryTree::build(count);
    let expected_height = u64::BITS - (count as u64).leading_zeros();
    assert_eq!(reference.dimensions(), (count as usize, expected_height as usize));
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("OptionTreeFind.fai".into(), tree::fai_source());
    let mut program = fai_driver::jit_compile(&db, db.source_file(id).unwrap())
        .unwrap_or_else(|d| panic!("{d:?}"));
    let baseline = rt::live_count();
    let make = program.function(Symbol::intern("make")).unwrap();
    let value = rt::apply(make, &[rt::make_int(count)]);
    let shape = program.function(Symbol::intern("shape")).unwrap();
    assert_eq!(list(rt::apply(shape, &[rt::fai_dup(value)])), reference.shape());
    let read_number = |f, args: &[rt::Value]| {
        let answer = rt::apply(f, args);
        let number = rt::read_int(answer);
        rt::fai_drop(answer);
        number
    };
    let nodes = program.function(Symbol::intern("count")).unwrap();
    assert_eq!(read_number(nodes, &[rt::fai_dup(value)]), count);
    let height = program.function(Symbol::intern("height")).unwrap();
    assert_eq!(read_number(height, &[rt::fai_dup(value)]), i64::from(expected_height));
    let lookup = program.function(Symbol::intern("answer")).unwrap();
    for key in -1..=count * 2 {
        let result = rt::apply(lookup, &[rt::fai_dup(value), rt::make_int(key)]);
        let answer = if rt::data_tag_of(result) == 0 {
            None
        } else {
            let field = rt::fai_data_field(result, 0);
            let n = rt::read_int(field);
            rt::fai_drop(field);
            Some(n)
        };
        rt::fai_drop(result);
        assert_eq!(answer, reference.find(key), "key {key}");
    }
    let probe = program.function(Symbol::intern("probe")).unwrap();
    let queries = count * 2 + 2;
    assert_eq!(
        read_number(probe, &[rt::fai_dup(value), rt::make_int(queries)]),
        reference.checksum(queries)
    );
    rt::fai_drop(value);
    assert_eq!(rt::live_count(), baseline);

    if let Some(binary) = fai_tests::ocaml::tree_baseline() {
        let output = std::process::Command::new(binary)
            .args(["describe", &count.to_string()])
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8(output.stdout).unwrap();
        let lines: Vec<_> = text.lines().collect();
        let shape = reference.shape().iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let answers = (-1..=count * 2)
            .map(|k| reference.find(k).map_or_else(|| "none".to_owned(), |v| v.to_string()))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(lines, vec![format!("{count},{expected_height}"), shape, answers]);
    }
}
