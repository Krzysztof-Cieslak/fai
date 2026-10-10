//! Known-constructor reductions and conservative representation boundaries.

use super::*;
use fai_db::FaiDatabase;

fn output(source: &str) -> String {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.into());
    crate::pretty_def(
        &crate::fuse_def(&db, db.source_file(id).unwrap(), Symbol::intern("run")).body,
    )
}

#[test]
fn local_integer_pair_projections_need_no_cell() {
    let body =
        output("module M\nlet run a =\n  let pair = (a + 1, 3)\n  let (x, y) = pair\n  x + y\n");
    assert!(!body.contains("(data "), "{body}");
    assert!(!body.contains("(field "), "{body}");
}

#[test]
fn locally_constructed_options_reduce_each_reachable_branch() {
    let body = output(
        "module M\nlet run yes =\n  let value = if yes then Some (1, 2) else None\n  match value with | Some (a, b) -> a + b | None -> 0\n",
    );
    assert!(!body.contains("(data ") && !body.contains("(tag"), "{body}");
}

#[test]
fn unused_strict_fields_keep_their_possible_trap() {
    let body = output(
        "module M\nlet run divisor =\n  let pair = (1 / divisor, 3)\n  let (_, value) = pair\n  value\n",
    );
    assert!(body.contains("(/ "), "{body}");
    assert!(!body.contains("(data "), "{body}");
}

#[test]
fn escaping_pairs_keep_their_value_boundary() {
    let body = output("module M\nlet run x = (x + 1, 3)\n");
    assert!(body.contains("(data "), "{body}");
}

#[test]
fn unknown_generic_fields_keep_their_owner() {
    let body =
        output("module M\nlet run x =\n  let pair = (x, x)\n  let (first, _) = pair\n  first\n");
    assert!(body.contains("(data "), "{body}");
}

#[test]
fn callback_fields_keep_their_original_lifetimes() {
    let body = output(
        "module M\ntype Stored = | Stored (Int -> Int)\nlet run callback =\n  let value = Stored callback\n  match value with | Stored f -> f 1\n",
    );
    assert!(body.contains("(data "), "{body}");
}
