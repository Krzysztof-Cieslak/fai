//! Tests that dup/drop land in the expected positions.

use fai_core::pretty_def;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_syntax::Symbol;

use crate::rc;

fn db_with(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn rc_of(src: &str, name: &str) -> String {
    let (db, file) = db_with(src);
    pretty_def(&rc(&db, file, Symbol::intern(name)))
}

#[test]
fn identity_dups_use_and_drops_param() {
    let got = rc_of("module M\n\nlet id x = x\n", "id");
    assert_eq!(got, "fn0(%0) = (drop %0; (dup %0; %0))\n");
}

#[test]
fn arithmetic_dups_each_operand() {
    let got = rc_of("module M\n\nlet add x y = x + y\n", "add");
    assert_eq!(got, "fn0(%0, %1) = (drop %0; (drop %1; (+ (dup %0; %0) (dup %1; %1))))\n");
}

#[test]
fn const_drops_unused_argument() {
    let got = rc_of("module M\n\nlet k x y = x\n", "k");
    assert_eq!(got, "fn0(%0, %1) = (drop %0; (drop %1; (dup %0; %0)))\n");
}

#[test]
fn let_binding_dropped_at_scope_end() {
    let got = rc_of("module M\n\nlet f a =\n  let b = a + 1\n  b + b\n", "f");
    assert_eq!(
        got,
        "fn0(%0) = (drop %0; (let %1 = (+ (dup %0; %0) 1); (drop %1; (+ (dup %1; %1) (dup %1; %1)))))\n"
    );
}

#[test]
fn captures_dup_on_use_but_are_not_dropped() {
    let got =
        rc_of("module M\n\npublic twice : ('a -> 'a) -> 'a -> 'a\nlet twice f = f >> f\n", "twice");
    assert_eq!(
        got,
        "fn0(%0) = (drop %0; (closure fn1 [%0]))\nfn1(%1) [caps %0] = (drop %1; (app (dup %0; %0) (app (dup %0; %0) (dup %1; %1))))\n"
    );
}

#[test]
fn console_write_line_drops_runtime() {
    let src = "module M\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime \"Hi\"\n";
    assert_eq!(rc_of(src, "main"), "fn0(%0) = (drop %0; (writeLine (dup %0; %0) \"Hi\"))\n");
}
