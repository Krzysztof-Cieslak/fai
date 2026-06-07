//! Tests for precise reference-count insertion.
//!
//! The primary guard is an **abstract reference-count interpreter**
//! ([`assert_rc_sound`]): it walks each reference-counted function on every path,
//! modeling ownership (owned-live / consumed / dropped), borrowing (projection
//! bases and offset evidence read without consuming), and captures (borrowed,
//! never dropped). It asserts that every owned binding is consumed-or-dropped
//! exactly once per path, that no value is used after release or dropped twice,
//! that captures are never dropped, and that branches leave a consistent state.
//! Snapshot tests pin the exact dup/drop shapes for representative programs.

use std::collections::HashMap;

use fai_core::ir::{CExpr, ExprKind, FieldIndex, LoweredDef};
use fai_core::pretty_def;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_resolve::LocalId;
use fai_syntax::Symbol;
use indoc::{formatdoc, indoc};
use proptest::prelude::*;

use crate::rc;

fn db_with(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

fn rc_of(src: &str, name: &str) -> String {
    let (db, file) = db_with(src);
    pretty_def(&rc(&db, file, Symbol::intern(name)))
}

// ---------------------------------------------------------------------------
// Snapshot tests: exact dup/drop placement.
// ---------------------------------------------------------------------------

#[test]
fn identity_passes_ownership_through() {
    let got = rc_of(
        indoc! {r#"
            module M

            let id x = x
        "#},
        "id",
    );
    // Ownership flows straight out: no dup, no drop.
    assert_eq!(got, "fn0(%0) = %0\n");
}

#[test]
fn arithmetic_consumes_each_operand() {
    let got = rc_of(
        indoc! {r#"
            module M

            let add x y = x + y
        "#},
        "add",
    );
    // The primitive consumes both operands; nothing to dup or drop.
    assert_eq!(got, "fn0(%0, %1) = (+ %0 %1)\n");
}

#[test]
fn unused_argument_is_dropped() {
    let got = rc_of(
        indoc! {r#"
            module M

            let k x y = x
        "#},
        "k",
    );
    assert_eq!(got, "fn0(%0, %1) = (drop %1; %0)\n");
}

#[test]
fn reused_binding_is_duplicated_once() {
    let got = rc_of(
        indoc! {r#"
            module M

            let f a =
              let b = a + 1
              b + b
        "#},
        "f",
    );
    assert_eq!(got, "fn0(%0) = (let %1 = (+ %0 1); (dup %1; (+ %1 %1)))\n");
}

#[test]
fn captures_dup_on_use_and_are_never_dropped() {
    let got = rc_of(
        indoc! {r#"
            module M

            public twice : ('a -> 'a) -> 'a -> 'a
            let twice f = fun x -> f (f x)
        "#},
        "twice",
    );
    // `f` moves into the closure env (no dup at the last use). In the lifted
    // body, A-normal form names the inner `f x`; the captured `f` is duplicated
    // per use and never dropped, and `x`/the temporary are consumed.
    assert_eq!(
        got,
        "fn0(%0) = (closure fn1 [%0])\n\
         fn1(%1) [caps %0] = (let %2 = (dup %0; (app %0 %1)); (dup %0; (app %0 %2)))\n"
    );
}

// ---------------------------------------------------------------------------
// Abstract reference-count interpreter.
// ---------------------------------------------------------------------------

/// Asserts the reference-count soundness invariants on every function of `def`.
fn assert_rc_sound(def: &LoweredDef) {
    for (i, f) in def.fns.iter().enumerate() {
        let captures: std::collections::HashSet<LocalId> = f.captures.iter().copied().collect();
        let mut refs: HashMap<LocalId, i64> = HashMap::new();
        for &p in &f.params {
            refs.insert(p, 1);
        }
        let mut ck = Checker { captures: &captures, fn_index: i };
        ck.eval(&f.body, &mut refs);
        for (l, n) in &refs {
            assert_eq!(*n, 0, "fn{i}: local %{} left with {n} refs at exit", l.index());
        }
    }
}

struct Checker<'a> {
    captures: &'a std::collections::HashSet<LocalId>,
    fn_index: usize,
}

impl Checker<'_> {
    fn owned(&self, x: LocalId) -> bool {
        !self.captures.contains(&x)
    }

    /// Consumes one owned reference of `x` (no-op for a borrowed capture).
    fn consume(&self, x: LocalId, refs: &mut HashMap<LocalId, i64>) {
        if !self.owned(x) {
            return;
        }
        let n = refs.get_mut(&x).unwrap_or_else(|| {
            panic!("fn{}: consume of unbound/owned %{}", self.fn_index, x.index())
        });
        assert!(*n >= 1, "fn{}: use of released %{}", self.fn_index, x.index());
        *n -= 1;
    }

    /// Reads `x` without consuming it (borrow); the value must still be alive.
    fn borrow(&self, x: LocalId, refs: &HashMap<LocalId, i64>) {
        if !self.owned(x) {
            return;
        }
        let n = refs.get(&x).copied().unwrap_or(0);
        assert!(n >= 1, "fn{}: borrow of released/unbound %{}", self.fn_index, x.index());
    }

    fn eval(&mut self, e: &CExpr, refs: &mut HashMap<LocalId, i64>) {
        match &e.kind {
            ExprKind::Lit(_) | ExprKind::Global(_) | ExprKind::Error => {}
            ExprKind::Local(x) => self.consume(*x, refs),
            ExprKind::Prim { args, .. } => {
                args.iter().for_each(|a| self.eval(a, refs));
            }
            ExprKind::MakeData { args, reuse, .. } => {
                args.iter().for_each(|a| self.eval(a, refs));
                if let Some(t) = reuse {
                    self.consume(*t, refs); // the reuse token is consumed here
                }
            }
            ExprKind::App { func, args } => {
                self.eval(func, refs);
                args.iter().for_each(|a| self.eval(a, refs));
            }
            ExprKind::MakeClosure { captures, .. } => {
                captures.iter().for_each(|&c| self.consume(c, refs));
            }
            ExprKind::DataTag(base) => self.borrow_atom(base, refs),
            ExprKind::DataField { base, index } => {
                self.borrow_atom(base, refs);
                if let FieldIndex::Dyn { evidence, .. } = index {
                    self.borrow(*evidence, refs);
                }
            }
            ExprKind::If { cond, then, els } => {
                // The condition (an immediate Bool) is consumed by the test.
                self.eval(cond, refs);
                let mut t = refs.clone();
                let mut e2 = refs.clone();
                self.eval(then, &mut t);
                self.eval(els, &mut e2);
                assert_eq!(
                    t, e2,
                    "fn{}: branches leave inconsistent reference state",
                    self.fn_index
                );
                *refs = t;
            }
            ExprKind::Let { local, value, body } => {
                self.eval(value, refs);
                assert!(
                    refs.insert(*local, 1).is_none(),
                    "fn{}: rebound %{}",
                    self.fn_index,
                    local.index()
                );
                self.eval(body, refs);
                let n = refs.remove(local).unwrap_or(0);
                assert_eq!(n, 0, "fn{}: let %{} left with {n} refs", self.fn_index, local.index());
            }
            ExprKind::Dup { local, body } => {
                if self.owned(*local) {
                    let n = refs.get_mut(local).unwrap_or_else(|| {
                        panic!("fn{}: dup of unbound %{}", self.fn_index, local.index())
                    });
                    assert!(*n >= 1, "fn{}: dup of released %{}", self.fn_index, local.index());
                    *n += 1;
                }
                self.eval(body, refs);
            }
            ExprKind::Drop { local, body } => {
                assert!(
                    self.owned(*local),
                    "fn{}: drop of captured %{}",
                    self.fn_index,
                    local.index()
                );
                self.consume(*local, refs);
                self.eval(body, refs);
            }
            ExprKind::Reset { value, token, body } => {
                self.eval(value, refs);
                assert!(
                    refs.insert(*token, 1).is_none(),
                    "fn{}: rebound reuse token %{}",
                    self.fn_index,
                    token.index()
                );
                self.eval(body, refs);
                let n = refs.remove(token).unwrap_or(0);
                assert_eq!(
                    n,
                    0,
                    "fn{}: reuse token %{} left with {n} refs",
                    self.fn_index,
                    token.index()
                );
            }
        }
    }

    /// A projection base is an atom after A-normal form; borrow it.
    fn borrow_atom(&self, base: &CExpr, refs: &HashMap<LocalId, i64>) {
        if let ExprKind::Local(x) = base.kind {
            self.borrow(x, refs);
        } else {
            // Defensive: a non-atom base is itself an owned temporary.
            // (A-normal form should prevent this.)
            panic!("fn{}: projection base is not an atom", self.fn_index);
        }
    }
}

// ---------------------------------------------------------------------------
// Corpus: the interpreter must accept every reference-counted definition.
// ---------------------------------------------------------------------------

#[test]
fn rc_is_sound_over_a_corpus() {
    let corpus: &[(&str, &str)] = &[
        ("module M\n\nlet id x = x\n", "id"),
        ("module M\n\nlet add x y = x + y\n", "add"),
        ("module M\n\nlet k x y = x\n", "k"),
        ("module M\n\nlet abs n = if n < 0 then 0 - n else n\n", "abs"),
        (
            indoc! {r#"
                module M

                let f a =
                  let b = a + 1
                  let c = b + a
                  b + c
            "#},
            "f",
        ),
        ("module M\n\nlet twice f = f >> f\n", "twice"),
        ("module M\n\nlet adder x = fun y -> x + y\n", "adder"),
        ("module M\n\nlet pipe n = n |> Int.toString\n", "pipe"),
        ("module M\n\nlet neq a b = a <> b\n", "neq"),
        ("module M\n\nlet both a b = a && b\n", "both"),
        ("module M\n\nlet nested f g x = f (g (g x))\n", "nested"),
        (
            indoc! {r#"
                module M

                public main : Runtime -> Unit
                let main r = r.console.writeLine ("a" ++ "b")
            "#},
            "main",
        ),
        (
            indoc! {r#"
                module M

                interface Greeter =
                  greet : String -> String

                let exclaim = { Greeter with greet name = name ++ "!" }
            "#},
            "exclaim",
        ),
        (
            indoc! {r#"
                module M

                public getX : { x : Int | 'r } -> Int
                let getX rec = rec.x
            "#},
            "getX",
        ),
        (
            indoc! {r#"
                module M

                public sumXY : { x : Int, y : Int | 'r } -> Int
                let sumXY rec = rec.x + rec.y
            "#},
            "sumXY",
        ),
        (
            indoc! {r#"
                module M

                public announce : { console : Console | 'r } -> String -> Unit
                let announce env msg = env.console.writeLine msg
            "#},
            "announce",
        ),
        (
            indoc! {r#"
                module M

                public bump : { n : Int | 'r } -> { n : Int | 'r }
                let bump rec = { rec with n = rec.n + 1 }
            "#},
            "bump",
        ),
        // A list `match` that destructures and rebuilds (the reuse-shaped case).
        (
            indoc! {r#"
                module M

                let inc xs =
                  match xs with
                  | [] -> []
                  | x :: rest -> (x + 1) :: inc rest
            "#},
            "inc",
        ),
        // A recursive inspector (destructures, never rebuilds).
        (
            indoc! {r#"
                module M

                let len xs =
                  match xs with
                  | [] -> 0
                  | _ :: rest -> 1 + len rest
            "#},
            "len",
        ),
        // A higher-order map over a list.
        (
            indoc! {r#"
                module M

                let map f xs =
                  match xs with
                  | [] -> []
                  | x :: rest -> f x :: map f rest
            "#},
            "map",
        ),
        // A monomorphic record literal, field access, and update.
        (
            indoc! {r#"
                module M

                type P = { x : Int, y : Int }

                let mk a = { x = a, y = a + 1 }
            "#},
            "mk",
        ),
        (
            indoc! {r#"
                module M

                type P = { x : Int, y : Int }

                let shift p = { p with x = p.x + 1 }
            "#},
            "shift",
        ),
        // An ADT with a constructor match.
        (
            indoc! {r#"
                module M

                type T =
                  | A Int
                  | B Int Int

                let eval t =
                  match t with
                  | A x -> x
                  | B x y -> x + y
            "#},
            "eval",
        ),
        // A constructor used to build a value (and a nested call chain).
        (
            indoc! {r#"
                module M

                type T =
                  | A Int
                  | B Int Int

                let pair a = B a a
            "#},
            "pair",
        ),
    ];
    for (src, name) in corpus {
        let (db, file) = db_with(src);
        let def = rc(&db, file, Symbol::intern(name));
        assert_rc_sound(&def);
    }
}

/// Every definition transitively reachable from a program's `main` (including the
/// standard library and the `Runtime` value binding) must be reference-count
/// sound. This guards leaks the single-definition corpus cannot reach — e.g.
/// projecting a method off a forced top-level interface instance.
#[test]
fn rc_is_sound_over_a_whole_program() {
    let src = indoc! {r#"
        module M

        interface Thing =
          a : Unit -> Int
          b : Unit -> Int

        let inst = { Thing with a u = 1, b u = 2 }

        public main : Runtime -> Unit
        let main runtime =
          let total = inst.a () + inst.b ()
          runtime.console.writeLine (Int.toString total)
    "#};
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let user = db.source_file(id).unwrap();
    let mut files = std::collections::HashMap::new();
    for f in db.all_source_files() {
        files.insert(f.source(&db), f);
    }
    let entry = fai_resolve::DefId::new(user.source(&db), Symbol::intern("main"));
    let runtime = fai_resolve::DefId::new(
        fai_resolve::prelude_module_file(&db).expect("prelude module").source(&db),
        Symbol::intern("defaultRuntime"),
    );
    let mut seen = std::collections::HashSet::new();
    let mut work = vec![entry, runtime];
    while let Some(def) = work.pop() {
        if !seen.insert(def) {
            continue;
        }
        let Some(&file) = files.get(&def.file) else { continue };
        let lowered = rc(&db, file, def.name);
        work.extend(lowered.referenced_globals());
        assert_rc_sound(&lowered);
    }
}

// ---------------------------------------------------------------------------
// Property: generated integer expressions are reference-count sound.
// ---------------------------------------------------------------------------

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
    fn rc_is_sound_for_generated_expressions(expr in int_expr()) {
        let src = formatdoc! {r#"
            module M

            let f x = {expr}
        "#};
        let (db, file) = db_with(&src);
        let def = rc(&db, file, Symbol::intern("f"));
        assert_rc_sound(&def);
    }
}
