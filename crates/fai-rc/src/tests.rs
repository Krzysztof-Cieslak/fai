//! Tests that dup/drop land in the expected positions, plus structural
//! invariants of the reference-count transform.

use std::collections::HashSet;

use fai_core::ir::{CExpr, ExprKind, LoweredDef};
use fai_core::pretty_def;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_resolve::LocalId;
use fai_syntax::Symbol;
use proptest::prelude::*;

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

/// Tallies the reference-count structure of one function body.
#[derive(Default)]
struct Counts {
    dups: usize,
    drops: usize,
    locals: usize,
    lets: usize,
    let_locals: HashSet<LocalId>,
    drop_locals: Vec<LocalId>,
}

fn tally(expr: &CExpr, c: &mut Counts) {
    match &expr.kind {
        ExprKind::Local(_) => c.locals += 1,
        ExprKind::Lit(_) | ExprKind::Global(_) | ExprKind::MakeClosure { .. } | ExprKind::Error => {
        }
        ExprKind::Prim { args, .. } => args.iter().for_each(|a| tally(a, c)),
        ExprKind::App { func, args } => {
            tally(func, c);
            args.iter().for_each(|a| tally(a, c));
        }
        ExprKind::If { cond, then, els } => {
            tally(cond, c);
            tally(then, c);
            tally(els, c);
        }
        ExprKind::Let { local, value, body } => {
            c.lets += 1;
            c.let_locals.insert(*local);
            tally(value, c);
            tally(body, c);
        }
        ExprKind::Dup { body, .. } => {
            c.dups += 1;
            tally(body, c);
        }
        ExprKind::Drop { local, body } => {
            c.drops += 1;
            c.drop_locals.push(*local);
            tally(body, c);
        }
    }
}

/// Asserts the plain-RC invariants on every function of `def`.
fn assert_rc_invariants(def: &LoweredDef) {
    for f in &def.fns {
        let mut c = Counts::default();
        tally(&f.body, &mut c);

        // Every variable use is wrapped in exactly one `Dup`.
        assert_eq!(c.dups, c.locals, "each use must be duplicated once");

        // Exactly one `Drop` per owned binding (parameter or `let`).
        assert_eq!(c.drops, f.params.len() + c.lets, "one drop per owned binding");

        // No drop targets a captured (borrowed) variable.
        let captures: HashSet<LocalId> = f.captures.iter().copied().collect();
        let drop_set: HashSet<LocalId> = c.drop_locals.iter().copied().collect();
        assert_eq!(c.drop_locals.len(), drop_set.len(), "no binding dropped twice");
        for d in &c.drop_locals {
            assert!(!captures.contains(d), "a captured variable must not be dropped");
        }
        // Every parameter and `let` binding is dropped.
        for p in &f.params {
            assert!(drop_set.contains(p), "parameter must be dropped");
        }
        for l in &c.let_locals {
            assert!(drop_set.contains(l), "let binding must be dropped");
        }
    }
}

#[test]
fn rc_invariants_hold_over_a_corpus() {
    let corpus: &[(&str, &str)] = &[
        ("module M\n\nlet id x = x\n", "id"),
        ("module M\n\nlet add x y = x + y\n", "add"),
        ("module M\n\nlet k x y = x\n", "k"),
        ("module M\n\nlet abs n = if n < 0 then 0 - n else n\n", "abs"),
        ("module M\n\nlet f a =\n  let b = a + 1\n  let c = b + a\n  b + c\n", "f"),
        ("module M\n\nlet twice f = f >> f\n", "twice"),
        ("module M\n\nlet adder x = fun y -> x + y\n", "adder"),
        ("module M\n\nlet pipe n = n |> intToString\n", "pipe"),
        ("module M\n\nlet neq a b = a <> b\n", "neq"),
        ("module M\n\nlet both a b = a && b\n", "both"),
        (
            "module M\n\npublic main : Runtime -> Unit\nlet main r = Console.writeLine r (\"a\" ++ \"b\")\n",
            "main",
        ),
    ];
    for (src, name) in corpus {
        let (db, file) = db_with(src);
        assert_rc_invariants(&rc(&db, file, Symbol::intern(name)));
    }
}

/// A strategy generating `Int`-typed expressions over the parameter `x`.
fn int_expr() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![Just("x".to_string()), (0i64..1000).prop_map(|n| n.to_string())];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone(), "[-+*]")
                .prop_map(|(a, b, op)| format!("({a} {op} {b})")),
            (inner.clone(), inner.clone(), inner.clone(), inner.clone())
                .prop_map(|(a, b, t, e)| format!("(if {a} < {b} then {t} else {e})")),
        ]
    })
}

proptest! {
    #[test]
    fn rc_invariants_hold_for_generated_expressions(expr in int_expr()) {
        let src = format!("module M\n\nlet f x = {expr}\n");
        let (db, file) = db_with(&src);
        let lowered = rc(&db, file, Symbol::intern("f"));
        assert_rc_invariants(&lowered);
    }
}
