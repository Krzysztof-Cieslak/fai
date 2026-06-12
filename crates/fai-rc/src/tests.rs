//! Tests for precise reference-count insertion.
//!
//! The primary guard is the abstract reference-count interpreter
//! ([`crate::check_rc`]): it walks each reference-counted function on every path,
//! modeling ownership (owned-live / consumed / dropped), borrowing (projection
//! bases and offset evidence read without consuming), and captures (borrowed,
//! never dropped). It checks that every owned binding is consumed-or-dropped
//! exactly once per path, that no value is used after release or dropped twice,
//! that captures are never dropped, and that branches leave a consistent state.
//! Snapshot tests pin the exact dup/drop shapes for representative programs.

use fai_core::ir::LoweredDef;
use fai_core::pretty_def;
use fai_db::{Db, FaiDatabase, SourceFile};
use fai_resolve::DefId;
use fai_syntax::Symbol;
use indoc::{formatdoc, indoc};
use proptest::prelude::*;

use crate::rc;

pub(crate) fn db_with(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file)
}

pub(crate) fn rc_of(src: &str, name: &str) -> String {
    let (db, file) = db_with(src);
    pretty_def(&rc(&db, file, Symbol::intern(name)))
}

/// Reference-counts `name` in `src` and checks soundness, returning the first
/// violation (if any). The one-shot form the generators drive. Rejects programs
/// that do not typecheck, so soundness is never asserted vacuously over `Error`
/// nodes.
pub(crate) fn check_program(src: &str, name: &str) -> Result<(), String> {
    let (db, file) = db_with(src);
    assert_well_typed(&db, file)?;
    let def = rc(&db, file, Symbol::intern(name));
    check_sound(&db, &def)
}

/// The inferred borrow signature of `name` in `src` (per-parameter: borrowed vs
/// owned). Asserts the program typechecks first.
pub(crate) fn borrow_sig(src: &str, name: &str) -> Vec<bool> {
    let (db, file) = db_with(src);
    assert_well_typed(&db, file).unwrap_or_else(|e| panic!("`{name}` {e}\n{src}"));
    crate::borrow_signature(&db, file, Symbol::intern(name)).0
}

/// Whether calling `name` in `src` is pure and total. Asserts the program
/// typechecks first.
pub(crate) fn pure_total(src: &str, name: &str) -> bool {
    let (db, file) = db_with(src);
    assert_well_typed(&db, file).unwrap_or_else(|e| panic!("`{name}` {e}\n{src}"));
    crate::purity::is_pure_total(&db, file, Symbol::intern(name))
}

/// The flattenable mutual-tail-recursion groups in `src`, as member-name lists.
pub(crate) fn mutual_member_groups(src: &str) -> Vec<Vec<String>> {
    let (db, file) = db_with(src);
    assert_well_typed(&db, file).unwrap_or_else(|e| panic!("{e}\n{src}"));
    crate::mutual_groups(&db, file)
        .groups
        .iter()
        .map(|g| g.members.iter().map(|m| m.name.as_str().to_owned()).collect())
        .collect()
}

/// For the group containing `member` in `src`, whether the combined function is
/// reference-count sound and was flattened into a loop, and whether each member's
/// wrapper is sound.
pub(crate) fn mutual_combined(src: &str, member: &str) -> (bool, bool, bool) {
    let (db, file) = db_with(src);
    assert_well_typed(&db, file).unwrap_or_else(|e| panic!("{e}\n{src}"));
    let groups = crate::mutual_groups(&db, file);
    let id = DefId::new(file.source(&db), Symbol::intern(member));
    let group = groups.group_of(id).expect("member belongs to a flattenable group");

    let combined = crate::combined_lowered(&db, file, group);
    let rc = crate::rc_lowered(&db, &combined, &crate::BorrowSig(vec![false; group.arity]));
    let sound = check_sound(&db, &rc).is_ok();
    let flattened = pretty_def(&rc).contains("(join ");

    let wrappers_sound = group.members.iter().all(|&m| {
        let w = crate::member_wrapper(&db, file, m, group);
        let n = w.entry().params.len();
        check_sound(&db, &crate::rc_lowered(&db, &w, &crate::BorrowSig(vec![false; n]))).is_ok()
    });
    (sound, flattened, wrappers_sound)
}

/// The reuse signature (accepted token size classes) of `name` in `src`.
pub(crate) fn reuse_sig(src: &str, name: &str) -> Vec<u32> {
    let (db, file) = db_with(src);
    assert_well_typed(&db, file).unwrap_or_else(|e| panic!("`{name}` {e}\n{src}"));
    crate::reuse_signature(&db, file, Symbol::intern(name)).0
}

/// The reuse signature of `name` in the embedded standard-library module `module`
/// (e.g. `Dict`, `Set`), so the analysis is exercised against the real
/// weight-balanced-tree sources rather than only synthetic shapes.
pub(crate) fn std_reuse_sig(module: &str, name: &str) -> Vec<u32> {
    let mut db = FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let want = Symbol::intern(module);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|&f| fai_resolve::module_name(&db, f).is_some_and(|m| m.0 == want))
        .expect("std module is loaded");
    crate::reuse_signature(&db, file, Symbol::intern(name)).0
}

/// Loads std and returns a database plus the `SourceFile` of module `module`.
fn std_db(module: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    let ids = fai_types::std_lib::load_std(&mut db);
    let want = Symbol::intern(module);
    let file = ids
        .into_iter()
        .filter_map(|id| db.source_file(id))
        .find(|&f| fai_resolve::module_name(&db, f).is_some_and(|m| m.0 == want))
        .expect("std module is loaded");
    (db, file)
}

/// The pretty-printed emit-ready lowering of `name` in std module `module`.
pub(crate) fn std_emit_pretty(module: &str, name: &str) -> String {
    let (db, file) = std_db(module);
    pretty_def(&crate::rc_emit(&db, file, Symbol::intern(name)))
}

/// Checks the emit-ready lowering of `name` in std module `module` is reference-
/// count sound, returning the first violation if any.
pub(crate) fn std_emit_sound(module: &str, name: &str) -> Result<(), String> {
    let (db, file) = std_db(module);
    let def = crate::rc_emit(&db, file, Symbol::intern(name));
    check_sound(&db, &def)
}

/// Whether `name` in std module `module` has a token-taking specialized entry.
pub(crate) fn std_has_reuse_entry(module: &str, name: &str) -> bool {
    let (db, file) = std_db(module);
    crate::rc_emit(&db, file, Symbol::intern(name)).reuse_entry.is_some()
}

/// Fails if `file` has any error-severity diagnostic. A program that does not
/// typecheck lowers to `Error` nodes that the soundness oracle accepts trivially,
/// so the corpus and generators must reject it explicitly.
pub(crate) fn assert_well_typed(db: &dyn Db, file: SourceFile) -> Result<(), String> {
    let diags = fai_types::check_file::accumulated::<fai_db::Diag>(db, file);
    let codes: Vec<&str> = diags
        .iter()
        .filter(|d| d.0.severity == fai_diagnostics::Severity::Error)
        .map(|d| d.0.code.as_str())
        .collect();
    if codes.is_empty() { Ok(()) } else { Err(format!("does not typecheck: {codes:?}")) }
}

// ---------------------------------------------------------------------------
// Reuse signatures: which forwarded reuse tokens a specialized entry accepts.
// ---------------------------------------------------------------------------

#[test]
fn balance_accepts_one_token() {
    // The weight-balanced rebalancer builds up to three same-size nodes (via the
    // inlined `bin`); a double rotation matches and resets all of its own children,
    // filling those, so a shorter rotation leaves exactly one construction free for
    // a token a caller (the search-path parent `insert`/`remove`) forwards.
    assert_eq!(std_reuse_sig("Dict", "balance"), vec![5]);
}

#[test]
fn set_balance_accepts_one_token() {
    // A `Set` node has four fields (size, left, element, right) rather than five.
    assert_eq!(std_reuse_sig("Set", "balance"), vec![4]);
}

#[test]
fn join_accepts_one_token() {
    // `join` builds a node on its small-difference branches, which a caller's token
    // can fill; its rebalancing branches forward to `balance` net of its own freed
    // child — one slot either way.
    assert_eq!(std_reuse_sig("Dict", "join"), vec![5]);
}

#[test]
fn union_is_a_pure_source() {
    // `union` builds nothing locally; it forwards its freed node to `join`, fully
    // spending `join`'s one slot, so it accepts no incoming token of its own.
    assert_eq!(std_reuse_sig("Dict", "union"), Vec::<u32>::new());
}

#[test]
fn intersection_is_a_pure_source() {
    assert_eq!(std_reuse_sig("Dict", "intersection"), Vec::<u32>::new());
}

#[test]
fn insert_fills_its_own_constructions() {
    // `insert` threads its freed search-path node into every construction it makes
    // (the empty/equal rebuild, the non-rebalancing `bin`) and forwards it on the
    // rebalance branch, so no slot is left for a caller's token — and nothing
    // forwards to `insert` anyway, so it gets no specialized entry.
    assert_eq!(std_reuse_sig("Dict", "insert"), Vec::<u32>::new());
}

#[test]
fn a_record_builder_accepts_its_field_class() {
    // A function whose tail builds a two-field record can recycle a forwarded
    // two-field cell.
    let src = indoc! {r#"
        module M

        type R = { a : Int, b : Int }

        mk : Int -> R
        let mk x = { a = x, b = x + 1 }
    "#};
    assert_eq!(reuse_sig(src, "mk"), vec![2]);
}

#[test]
fn a_non_constructing_function_accepts_nothing() {
    // `size`-style readers build nothing, so they accept no forwarded token.
    assert_eq!(std_reuse_sig("Dict", "size"), Vec::<u32>::new());
}

#[test]
fn reuse_class_of_a_tuple_is_its_arity() {
    let db = FaiDatabase::new();
    let ty = fai_types::Ty::Tuple(vec![fai_types::Ty::int(), fai_types::Ty::int()]);
    assert_eq!(crate::reuse_class(&db, &ty), Some(2));
}

#[test]
fn reuse_class_of_a_scalar_is_none() {
    let db = FaiDatabase::new();
    assert_eq!(crate::reuse_class(&db, &fai_types::Ty::int()), None);
}

#[test]
fn reuse_class_of_a_list_is_a_cons_cell() {
    let db = FaiDatabase::new();
    let ty = fai_types::Ty::list(fai_types::Ty::int());
    assert_eq!(crate::reuse_class(&db, &ty), Some(2));
}

// ---------------------------------------------------------------------------
// Emit-ready lowering: forwarded reuse tokens and specialized entries.
// ---------------------------------------------------------------------------

#[test]
fn balance_gets_a_specialized_entry() {
    // `balance` accepts a token, so its emit-ready lowering carries a token-taking
    // entry for callers to forward into.
    assert!(std_has_reuse_entry("Dict", "balance"));
    assert!(std_has_reuse_entry("Set", "balance"));
}

#[test]
fn a_pure_source_gets_no_specialized_entry() {
    // `union`/`insert` accept no token, so no specialized entry is built.
    assert!(!std_has_reuse_entry("Dict", "union"));
    assert!(!std_has_reuse_entry("Dict", "insert"));
}

#[test]
fn insert_forwards_its_freed_node_to_balance() {
    // The rebalance branch used to free the matched search-path node (`free-reuse`)
    // and call `balance`; the emit-ready lowering forwards that token into the call
    // (recorded as `@balance … @%token`) instead, so `balance` recycles it.
    let pretty = std_emit_pretty("Dict", "insert");
    // Every `balance` call is a forwarding call: `(app @balance <args> @%token)`,
    // its argument list ending in a forwarded reuse token (`@%n`) rather than the
    // token being freed before the call. (Local slot numbers are not pinned, as
    // they shift with unrelated lowering changes.)
    assert!(pretty.contains("@balance"), "insert should call balance:\n{pretty}");
    for (i, _) in pretty.match_indices("@balance") {
        let call = pretty[i..].split(')').next().unwrap_or_default();
        assert!(
            call.contains("@%"),
            "the balance call should carry a forwarded reuse token: `{call}`\n{pretty}"
        );
    }
}

#[test]
fn rc_emit_insert_is_sound() {
    std_emit_sound("Dict", "insert").unwrap();
}

#[test]
fn rc_emit_balance_is_sound() {
    std_emit_sound("Dict", "balance").unwrap();
}

#[test]
fn rc_emit_remove_is_sound() {
    std_emit_sound("Dict", "remove").unwrap();
}

#[test]
fn rc_emit_union_is_sound() {
    std_emit_sound("Dict", "union").unwrap();
}

#[test]
fn rc_emit_set_insert_is_sound() {
    std_emit_sound("Set", "insert").unwrap();
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
fn unused_argument_is_borrowed() {
    let got = rc_of(
        indoc! {r#"
            module M

            let k x y = x
        "#},
        "k",
    );
    // `x` is returned (owned); the unused `y` is borrowed, so the caller releases
    // it rather than `k`.
    assert_eq!(got, "fn0(%0, %1) = %0\n");
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
// Argument borrowing.
// ---------------------------------------------------------------------------

#[test]
fn caller_lends_a_borrowed_argument_without_duplicating() {
    // `len` borrows its list, so `count` — which only forwards `xs` to two `len`
    // calls — lends it to each rather than duplicating it (the churn win), and is
    // *itself* inferred to borrow `xs` (inter-procedural borrowing). A borrowed
    // parameter is released by the caller, so `count` neither dups nor drops it.
    let got = rc_of(
        indoc! {r#"
            module M

            let len xs =
              match xs with
              | [] -> 0
              | _ :: r -> 1 + len r

            let count xs = len xs + len xs
        "#},
        "count",
    );
    assert!(!got.contains("dup"), "a borrowed argument is lent, not duplicated: {got}");
    assert_eq!(
        got.matches("drop").count(),
        0,
        "count forwards its parameter to a borrowing function, so it borrows it too \
         (the caller releases it): {got}"
    );
}

// ---------------------------------------------------------------------------
// Reuse firing: a matched, reconstructed data cell is reset and recycled in
// place; pure inspectors and fresh constructions carry no reuse token. (The
// runtime makes the final per-cell decision from uniqueness and size; here we
// pin that the *opportunity* is emitted exactly where the source destructures
// and rebuilds.)
// ---------------------------------------------------------------------------

/// Reference-counts `name`, asserting it typechecks first so a marker assertion
/// is never made vacuously over `Error` nodes.
pub(crate) fn rc_checked(src: &str, name: &str) -> String {
    let (db, file) = db_with(src);
    assert_well_typed(&db, file).unwrap_or_else(|e| panic!("`{name}` {e}\n{src}"));
    pretty_def(&rc(&db, file, Symbol::intern(name)))
}

// ---------------------------------------------------------------------------
// Abstract reference-count interpreter (the soundness oracle).
// ---------------------------------------------------------------------------

/// Checks soundness of `def` using `db`'s real borrow signatures for direct calls.
pub(crate) fn check_sound(db: &dyn Db, def: &LoweredDef) -> Result<(), String> {
    let borrows = |d: DefId, nargs: usize| -> Vec<bool> {
        let Some(cf) = db.source_file(d.file) else { return vec![false; nargs] };
        let sig = crate::borrow_signature(db, cf, d.name);
        if sig.exploitable_at(nargs) { sig.0.clone() } else { vec![false; nargs] }
    };
    crate::check_rc(def, &borrows)
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

        public main : Runtime -> Unit / { Console }
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
        if let Err(e) = check_sound(&db, &lowered) {
            panic!("rc unsound for `{}`: {e}\n{}", def.name.as_str(), pretty_def(&lowered));
        }
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
        let r = check_sound(&db, &def);
        prop_assert!(r.is_ok(), "rc unsound: {}\n{}", r.unwrap_err(), pretty_def(&def));
    }
}

// ---------------------------------------------------------------------------
// Property: inter-procedural borrowing over arbitrary forwarding/mutual-recursion
// call graphs stays reference-count sound, and the borrow fixpoint always
// terminates (the salsa cycle converges or falls back, never panics).
// ---------------------------------------------------------------------------

/// Generates a module of `n` functions `f0..f{n-1}`, each `List Int -> Int`, whose
/// body either inspects its list, forwards the whole list to another function,
/// forwards the tail, or sums the head and recurses into another function. Targets
/// are unconstrained (0..n), so the call graph is arbitrary — including self- and
/// mutual recursion (borrow cycles). Every program is well-typed by construction.
fn forwarding_program() -> impl Strategy<Value = (String, usize)> {
    (1usize..=4).prop_flat_map(|n| {
        proptest::collection::vec((0u8..4u8, 0..n, 0i64..100), n).prop_map(move |defs| {
            let mut src = String::from("module M\n");
            for (i, &(kind, j, c)) in defs.iter().enumerate() {
                src.push('\n');
                let def = match kind {
                    // Forward the whole list to another function.
                    1 => format!("let f{i} xs = f{j} xs\n"),
                    // Forward the tail to another function.
                    2 => format!(
                        "let f{i} xs =\n  match xs with\n  | [] -> {c}\n  | _ :: r -> f{j} r\n"
                    ),
                    // Inspect the head, recurse into another function on the tail.
                    3 => format!(
                        "let f{i} xs =\n  match xs with\n  | [] -> {c}\n  | x :: r -> x + f{j} r\n"
                    ),
                    // Inspect the list, ignore the element.
                    _ => format!(
                        "let f{i} xs =\n  match xs with\n  | [] -> {c}\n  | _ :: _ -> {c}\n"
                    ),
                };
                src.push_str(&def);
            }
            (src, n)
        })
    })
}

proptest! {
    #[test]
    fn borrow_is_sound_over_forwarding_graphs((src, n) in forwarding_program()) {
        let (db, file) = db_with(&src);
        // Well-typed by construction; assert it so soundness is not vacuous over
        // `Error` nodes.
        prop_assert!(assert_well_typed(&db, file).is_ok(), "ill-typed:\n{src}");
        // Reference-counting each function drives `borrow_signature` (and its
        // cross-function fixpoint) and must stay sound on every member.
        for i in 0..n {
            let name = format!("f{i}");
            let def = rc(&db, file, Symbol::intern(&name));
            let r = check_sound(&db, &def);
            prop_assert!(r.is_ok(), "rc unsound for {name}: {}\n{src}", r.unwrap_err());
        }
    }
}
