//! Lowering tests: surface programs to compact Core renderings.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_syntax::Symbol;

use crate::{core, pretty_def};

fn db_with(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::prelude::load_prelude(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn lower(src: &str, name: &str) -> String {
    let (db, file) = db_with(src);
    pretty_def(&core(&db, file, Symbol::intern(name)))
}

fn codes(src: &str, name: &str) -> Vec<String> {
    let (db, file) = db_with(src);
    core::accumulated::<Diag>(&db, file, Symbol::intern(name))
        .into_iter()
        .map(|d| d.0.code.as_str().to_owned())
        .collect()
}

#[test]
fn lowers_arithmetic() {
    let got = lower("module M\n\npublic add : Int -> Int -> Int\nlet add x y = x + y\n", "add");
    assert_eq!(got, "fn0(%0, %1) = (+ %0 %1)\n");
}

#[test]
fn lowers_if_and_negation() {
    let got = lower("module M\n\nlet f n = if n < 0 then 0 - n else n\n", "f");
    assert_eq!(got, "fn0(%0) = (if (< %0 0) (- 0 %0) %0)\n");
}

#[test]
fn lowers_string_concat() {
    let got = lower("module M\n\nlet greet name = \"Hi \" ++ name\n", "greet");
    assert_eq!(got, "fn0(%0) = (++ \"Hi \" %0)\n");
}

#[test]
fn lowers_console_write_line() {
    let src = "module M\n\npublic main : Runtime -> Unit\nlet main runtime = Console.writeLine runtime \"Hi\"\n";
    assert_eq!(lower(src, "main"), "fn0(%0) = (writeLine %0 \"Hi\")\n");
}

#[test]
fn lowers_let_block() {
    let src = "module M\n\nlet f a =\n  let b = a + 1\n  b + b\n";
    assert_eq!(lower(src, "f"), "fn0(%0) = (let %1 = (+ %0 1); (+ %1 %1))\n");
}

#[test]
fn lowers_partial_application_as_general_app() {
    let src = "module M\n\nlet add x y = x + y\n\nlet inc = add 1\n";
    assert_eq!(lower(src, "inc"), "fn0() = (app @add 1)\n");
}

#[test]
fn lowers_pipe_to_application() {
    let src = "module M\n\nlet f x = x\n\nlet g n = n |> f\n";
    assert_eq!(lower(src, "g"), "fn0(%1) = (app @f %1)\n");
}

#[test]
fn lowers_pipe_into_primitive() {
    let src = "module M\n\nlet describe n = n |> intToString\n";
    assert_eq!(lower(src, "describe"), "fn0(%0) = (intToString %0)\n");
}

#[test]
fn lowers_compose_with_capture() {
    let src = "module M\n\npublic twice : ('a -> 'a) -> 'a -> 'a\nlet twice f = f >> f\n";
    let got = lower(src, "twice");
    assert_eq!(got, "fn0(%0) = (closure fn1 [%0])\nfn1(%1) [caps %0] = (app %0 (app %0 %1))\n");
}

#[test]
fn lowers_lambda_with_capture() {
    let src = "module M\n\nlet adder x = fun y -> x + y\n";
    let got = lower(src, "adder");
    assert_eq!(got, "fn0(%0) = (closure fn1 [%0])\nfn1(%1) [caps %0] = (+ %0 %1)\n");
}

#[test]
fn lowers_not_equal_to_not_of_eq() {
    let src = "module M\n\nlet f a b = a <> b\n";
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (not (= %0 %1))\n");
}

#[test]
fn lowers_short_circuit_booleans() {
    let src = "module M\n\nlet f a b = a && b\n";
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (if %0 %1 false)\n");
}

#[test]
fn references_prelude_helper_as_global() {
    let src = "module M\n\nlet f a b = notEqual a b\n";
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (app @notEqual %0 %1)\n");
}

#[test]
fn float_is_unsupported() {
    let src = "module M\n\nlet x = 3.0\n";
    assert_eq!(lower(src, "x"), "fn0() = <error>\n");
    assert!(codes(src, "x").contains(&"FAI7001".to_owned()));
}

#[test]
fn tuples_are_unsupported() {
    let src = "module M\n\nlet pair a b = (a, b)\n";
    assert!(codes(src, "pair").contains(&"FAI7001".to_owned()));
}

#[test]
fn integer_literals_are_decoded() {
    let src = "module M\n\nlet x = 0xFF + 1_000\n";
    assert_eq!(lower(src, "x"), "fn0() = (+ 255 1000)\n");
}
