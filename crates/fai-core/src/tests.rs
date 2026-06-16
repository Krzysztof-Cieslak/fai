//! Lowering tests: surface programs to compact Core renderings.

use fai_db::{Db, Diag, FaiDatabase, SourceFile};
use fai_syntax::Symbol;
use indoc::indoc;

use crate::{core, pretty_def};

fn db_with(src: &str) -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
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
    let src = indoc! {r#"
        module M

        public add : Int -> Int -> Int
        let add x y = x + y
    "#};
    let got = lower(src, "add");
    assert_eq!(got, "fn0(%0, %1) = (+ %0 %1)\n");
}

#[test]
fn lowers_if_and_negation() {
    let src = indoc! {r#"
        module M

        let f n = if n < 0 then 0 - n else n
    "#};
    let got = lower(src, "f");
    assert_eq!(got, "fn0(%0) = (if (< %0 0) (- 0 %0) %0)\n");
}

// `++`, `|>`, and `>>` are ordinary `Prelude` operator functions now, so their
// lowering is plain application of a global (covered by the application tests and
// the standard-library/e2e suites) rather than a dedicated Core form.

#[test]
fn lowers_console_capability_access() {
    // Console output goes through the `Runtime` record: `runtime.console` is a
    // constant-offset projection (`console` is field 1 of the sorted
    // `{clock, console, random}`), then `.writeLine` projects method 0 of the
    // `Console` dictionary, applied to the string.
    let src = indoc! {r#"
        module M

        public main : Runtime -> Unit / { Console }
        let main runtime = runtime.console.writeLine "Hi"
    "#};
    assert_eq!(lower(src, "main"), "fn0(%0) = (app (field 0 (field 1 %0)) \"Hi\")\n");
}

#[test]
fn lowers_foreign_decl_to_a_foreign_call() {
    // A `foreign` declaration has no Fai body: its entry is synthesized as the
    // eta-expanded native call `fn(p…) = Foreign{symbol, [p…]}`, with one parameter
    // per arrow of the declared type.
    let src = indoc! {r#"
        module M

        foreign "fai_console_write_line" consoleWriteLine : String -> Unit / { Console }
    "#};
    assert_eq!(
        lower(src, "consoleWriteLine"),
        "fn0(%0) = (foreign \"fai_console_write_line\" %0)\n"
    );
}

#[test]
fn lowers_let_block() {
    let src = indoc! {r#"
        module M

        let f a =
          let b = a + 1
          b + b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0) = (let %1 = (+ %0 1); (+ %1 %1))\n");
}

#[test]
fn lowers_partial_application_as_general_app() {
    let src = indoc! {r#"
        module M

        let add x y = x + y

        let inc = add 1
    "#};
    assert_eq!(lower(src, "inc"), "fn0() = (app @add 1)\n");
}

#[test]
fn function_alias_let_is_copy_propagated() {
    // `let g = f` (f a non-row-polymorphic function) drops the binding and
    // propagates `g` to `@f`, so the call head is the global directly (a direct
    // call at code generation), not a local bound to its closure.
    let src = indoc! {r#"
        module M

        let f x = x + 1

        let caller a =
          let g = f
          g a
    "#};
    // The call head is `@f` (a direct call) and the alias binding is gone.
    let got = lower(src, "caller");
    assert!(got.contains("(app @f "), "call head is the global directly: {got}");
    assert!(!got.contains("(let "), "the alias binding is dropped: {got}");
}

#[test]
fn transitive_function_alias_is_copy_propagated() {
    // The alias composes along a chain: `let h = g` lowers `g` to `@f`, so `h`
    // aliases `f` too, and both bindings vanish.
    let src = indoc! {r#"
        module M

        let f x = x + 1

        let caller a =
          let g = f
          let h = g
          h a
    "#};
    let got = lower(src, "caller");
    assert!(got.contains("(app @f "), "call head is the global directly: {got}");
    assert!(!got.contains("(let "), "both alias bindings are dropped: {got}");
}

#[test]
fn nullary_value_alias_is_not_propagated() {
    // `let g = <nullary value>` binds a *forced* value, not a function alias
    // (its type is not an arrow), so the binding is kept — propagating it would
    // re-force the value at every use.
    let src = indoc! {r#"
        module M

        let base = 41

        let caller =
          let g = base
          g + 1
    "#};
    let got = lower(src, "caller");
    assert!(got.contains("(let "), "the nullary-value binding is kept: {got}");
    assert!(got.contains("@base"), "and still references the global: {got}");
}

#[test]
fn lowers_lambda_with_capture() {
    let src = indoc! {r#"
        module M

        let adder x = fun y -> x + y
    "#};
    let got = lower(src, "adder");
    // Lowering marks a capturing lambda `heap`; escape analysis may later restamp
    // a non-escaping one as `stack`.
    assert_eq!(got, "fn0(%0) = (closure/heap fn1 [%0])\nfn1(%1) [caps %0] = (+ %0 %1)\n");
}

#[test]
fn lowers_not_equal_to_not_of_eq() {
    let src = indoc! {r#"
        module M

        let f a b = a <> b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (not (= %0 %1))\n");
}

#[test]
fn lowers_short_circuit_booleans() {
    let src = indoc! {r#"
        module M

        let f a b = a && b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (if %0 %1 false)\n");
}

#[test]
fn references_prelude_helper_as_global() {
    let src = indoc! {r#"
        module M

        let f a b = compare a b
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0, %1) = (app @compare %0 %1)\n");
}

#[test]
fn float_lowers_to_a_boxed_literal() {
    let src = indoc! {r#"
        module M

        let x = 3.0
    "#};
    assert_eq!(lower(src, "x"), "fn0() = 3\n");
    assert!(codes(src, "x").is_empty());
}

#[test]
fn tuples_lower_to_data() {
    let src = indoc! {r#"
        module M

        let pair a b = (a, b)
    "#};
    assert_eq!(lower(src, "pair"), "fn0(%0, %1) = (data 0 %0 %1)\n");
    assert!(codes(src, "pair").is_empty());
}

#[test]
fn integer_literals_are_decoded() {
    let src = indoc! {r#"
        module M

        let x = 0xFF + 1_000
    "#};
    assert_eq!(lower(src, "x"), "fn0() = (+ 255 1000)\n");
}

#[test]
fn char_literal_lowers_to_an_immediate() {
    let src = indoc! {r#"
        module M

        let c = 'a'
    "#};
    assert_eq!(lower(src, "c"), "fn0() = 'a'\n");
    assert!(codes(src, "c").is_empty());
}

#[test]
fn char_pattern_lowers_to_an_equality_test() {
    let src = indoc! {r#"
        module M

        let f c =
          match c with
          | 'a' -> 1
          | _ -> 0
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0) = (let %2 = %0; (if (= %2 'a') 1 0))\n");
    assert!(codes(src, "f").is_empty());
}

#[test]
fn char_escape_literal_decodes_in_lowering() {
    let src = indoc! {r#"
        module M

        let c = '\n'
    "#};
    assert_eq!(lower(src, "c"), "fn0() = '\\n'\n");
    assert!(codes(src, "c").is_empty());
}

#[test]
fn multi_arm_char_match_lowers_to_chained_equality_tests() {
    let src = indoc! {r#"
        module M

        let f c =
          match c with
          | 'a' -> 1
          | 'b' -> 2
          | _ -> 0
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0) = (let %2 = %0; (if (= %2 'a') 1 (if (= %2 'b') 2 0)))\n");
    assert!(codes(src, "f").is_empty());
}

#[test]
fn char_in_argument_and_list_positions_lowers_cleanly() {
    let src = indoc! {r#"
        module M

        let f g = g 'a'

        let xs = ['a', 'b', 'c']
    "#};
    assert!(codes(src, "f").is_empty());
    assert!(codes(src, "xs").is_empty());
    assert_eq!(lower(src, "xs"), "fn0() = (data 1 'a' (data 1 'b' (data 1 'c' (data 0))))\n");
}

#[test]
fn list_literal_lowers_to_cons_and_nil() {
    let src = indoc! {r#"
        module M

        let xs = [1, 2, 3]
    "#};
    assert_eq!(lower(src, "xs"), "fn0() = (data 1 1 (data 1 2 (data 1 3 (data 0))))\n");
    assert!(codes(src, "xs").is_empty());
}

#[test]
fn cons_lowers_to_data() {
    let src = indoc! {r#"
        module M

        let f x = x :: []
    "#};
    assert_eq!(lower(src, "f"), "fn0(%0) = (data 1 %0 (data 0))\n");
    assert!(codes(src, "f").is_empty());
}

#[test]
fn float_in_argument_position_lowers() {
    let src = indoc! {r#"
        module M

        let f = Float.toString 3.0
    "#};
    assert!(codes(src, "f").is_empty());
}

#[test]
fn list_prelude_helper_lowers_to_a_global() {
    // `List.length` is an ordinary standard-library definition, reached as a global.
    let src = indoc! {r#"
        module M

        let n = List.length [1]
    "#};
    assert!(codes(src, "n").is_empty());
}

#[test]
fn tuple_let_binding_destructures() {
    let src = indoc! {r#"
        module M

        let f p =
          let (x, y) = p
          x
    "#};
    assert!(codes(src, "f").is_empty());
}

#[test]
fn monomorphic_record_access_lowers_to_a_projection() {
    let src = indoc! {r#"
        module M

        public dist : { x : Int, y : Int } -> Int
        let dist v = v.x + v.y
    "#};
    // `x` is field 0, `y` is field 1 in canonical (sorted) order.
    assert_eq!(lower(src, "dist"), "fn0(%0) = (+ (field 0 %0) (field 1 %0))\n");
    assert!(codes(src, "dist").is_empty());
}

#[test]
fn row_polymorphic_field_access_uses_offset_evidence() {
    // A field access through a row variable cannot pick a constant slot; the
    // function takes a leading offset-evidence parameter (here `%1`) and the slot
    // is `base + evidence`.
    let src = indoc! {r#"
        module M

        let getX r = r.x
    "#};
    assert!(codes(src, "getX").is_empty(), "got {:?}", codes(src, "getX"));
    assert_eq!(lower(src, "getX"), "fn0(%1, %0) = (field 0+%1 %0)\n");
}

#[test]
fn record_literal_lowers_to_sorted_data() {
    // Fields are stored in canonical (sorted-by-label) order, so `x` precedes
    // `y` regardless of how the literal is written.
    let src = indoc! {r#"
        module M

        let p = { y = 2, x = 1 }
    "#};
    assert_eq!(lower(src, "p"), "fn0() = (data 0 1 2)\n");
    assert!(codes(src, "p").is_empty());
}

#[test]
fn nullary_constructor_lowers_to_tagged_data() {
    let src = indoc! {r#"
        module M

        type T =
          | A
          | B Int

        let mkA = A
    "#};
    assert_eq!(lower(src, "mkA"), "fn0() = (data 0)\n");
    assert!(codes(src, "mkA").is_empty());
}

#[test]
fn constructor_with_field_lowers_to_tagged_data() {
    // `B` is the second constructor, so it carries tag 1 and its field.
    let src = indoc! {r#"
        module M

        type T =
          | A
          | B Int

        let mkB n = B n
    "#};
    assert_eq!(lower(src, "mkB"), "fn0(%0) = (data 1 %0)\n");
    assert!(codes(src, "mkB").is_empty());
}

#[test]
fn match_lowers_to_tag_tests_and_field_projections() {
    // The scrutinee is bound once, then a chain of tag tests selects an arm and
    // projects the matched constructor's field; the impossible final fallthrough
    // of this exhaustive match is an unreachable `<error>` leaf.
    let src = indoc! {r#"
        module M

        type T =
          | A Int
          | B Int

        let f t =
          match t with
          | A x -> x
          | B y -> y
    "#};
    assert_eq!(
        lower(src, "f"),
        "fn0(%0) = (let %3 = %0; (if (= (tag %3) 0) (let %1 = (field 0 %3); %1) (if (= (tag %3) 1) (let %2 = (field 0 %3); %2) <error>)))\n"
    );
    assert!(codes(src, "f").is_empty());
}

#[test]
fn float_comparison_selects_the_float_primitive() {
    let src = indoc! {r#"
        module M

        let lt = 1.0 < 2.0
    "#};
    assert_eq!(lower(src, "lt"), "fn0() = (<. 1 2)\n");
    assert!(codes(src, "lt").is_empty());
}

#[test]
fn structural_compare_lowers_to_the_compare_primitive() {
    // `<` on a non-numeric type (a tuple here) becomes the structural `compare`.
    let src = indoc! {r#"
        module M

        let before a b = (a, 1) < (b, 2)
    "#};
    assert!(codes(src, "before").is_empty());
    assert!(lower(src, "before").contains("compare"), "got {}", lower(src, "before"));
}

/// The lowered entry's arity matches the binding's parameter count, and every
/// global the body references resolves to a real binding somewhere.
#[track_caller]
fn assert_lowering_invariants(src: &str, name: &str) {
    use fai_syntax::ast::ItemKind;

    let (db, file) = db_with(src);
    let lowered = core(&db, file, fai_syntax::Symbol::intern(name));

    let params = fai_syntax::parse(&db, file)
        .module
        .items
        .iter()
        .find_map(|it| match &it.kind {
            ItemKind::Binding { name: n, params, .. } if n.as_str() == name => Some(params.len()),
            _ => None,
        })
        .unwrap();
    assert_eq!(lowered.entry().params.len(), params, "{name}: entry arity");

    for def in lowered.referenced_globals() {
        let target = db.source_file(def.file).expect("global's file is registered");
        assert!(
            fai_resolve::module_defs(&db, target).get(def.name).is_some(),
            "{name}: dangling global {}",
            def.name
        );
    }
}

#[test]
fn lowering_invariants_simple_binding() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let add x y = x + y
        "#},
        "add",
    );
}

#[test]
fn lowering_invariants_composition() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let twice f = f >> f
        "#},
        "twice",
    );
}

#[test]
fn lowering_invariants_returns_closure() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let adder x = fun y -> x + y
        "#},
        "adder",
    );
}

#[test]
fn lowering_invariants_calls_helper_and_capability() {
    assert_lowering_invariants(
        indoc! {r#"
            module M

            let helper x = x + 1

            public main : Runtime -> Unit / { Console }
            let main r = r.console.writeLine (Int.toString (helper 1))
        "#},
        "main",
    );
}

// ── Intrinsic inlining ───────────────────────────────────────────────────────

mod inline {
    use fai_syntax::Symbol;

    use super::db_with;
    use crate::ir::Prim;
    use crate::{PrimWrapper, core, core_inlined, pretty_def, prim_wrapper};

    /// The recognizer's verdict on `name` in `src`.
    fn wrapper(src: &str, name: &str) -> Option<PrimWrapper> {
        let (db, file) = db_with(src);
        prim_wrapper(&db, file, Symbol::intern(name))
    }

    /// `name`'s Core after intrinsic inlining, rendered compactly.
    fn inlined(src: &str, name: &str) -> String {
        let (db, file) = db_with(src);
        pretty_def(&core_inlined(&db, file, Symbol::intern(name)))
    }

    /// `name`'s Core before inlining, rendered compactly.
    fn raw(src: &str, name: &str) -> String {
        let (db, file) = db_with(src);
        pretty_def(&core(&db, file, Symbol::intern(name)))
    }

    // The recognizer (`prim_wrapper`). It is shape-based, so a user one-liner that
    // eta-expands an operator (which lowers to a primitive) is recognized too.

    #[test]
    fn recognizes_identity_operator_wrapper() {
        let src = "module M\n\nlet myAdd a b = a + b\n";
        assert_eq!(
            wrapper(src, "myAdd"),
            Some(PrimWrapper { op: Prim::IntAdd, slots: vec![0, 1] })
        );
    }

    #[test]
    fn recognizes_permuted_operator_wrapper() {
        // `b - a` reverses the operands, so the slots are the swapping permutation.
        let src = "module M\n\nlet rsub a b = b - a\n";
        assert_eq!(wrapper(src, "rsub"), Some(PrimWrapper { op: Prim::IntSub, slots: vec![1, 0] }));
    }

    #[test]
    fn rejects_partial_arity_wrapper() {
        // One parameter, a two-operand primitive (a literal fills the other slot):
        // not an eta-expansion of the primitive over its parameters.
        let src = "module M\n\nlet inc x = x + 1\n";
        assert_eq!(wrapper(src, "inc"), None);
    }

    #[test]
    fn rejects_nested_body() {
        let src = "module M\n\nlet f a b = (a + b) + 1\n";
        assert_eq!(wrapper(src, "f"), None);
    }

    #[test]
    fn rejects_non_bijection_duplicate_parameter() {
        let src = "module M\n\nlet dbl a = a + a\n";
        assert_eq!(wrapper(src, "dbl"), None);
    }

    #[test]
    fn rejects_non_wrapper_definition() {
        let src = "module M\n\nlet f x = if x then 1 else 2\n";
        assert_eq!(wrapper(src, "f"), None);
    }

    // The inliner (`core_inlined`).

    #[test]
    fn inlines_identity_std_wrapper() {
        // `Int.toString x` is a saturated call to a re-export wrapper; it becomes
        // the primitive directly (the wrapper hop is gone).
        let src = "module M\n\nlet f x = Int.toString x\n";
        assert_eq!(raw(src, "f"), "fn0(%0) = (app @toString %0)\n");
        assert_eq!(inlined(src, "f"), "fn0(%0) = (intToString %0)\n");
    }

    #[test]
    fn inlines_into_nested_argument() {
        let src = "module M\n\nlet f x = Int.toString (x + 1)\n";
        assert_eq!(inlined(src, "f"), "fn0(%0) = (intToString (+ %0 1))\n");
    }

    #[test]
    fn inlines_identity_user_wrapper() {
        // `myAdd`'s parameters take locals %0/%1, so `f`'s `x` is %2.
        let src = "module M\n\nlet myAdd a b = a + b\n\nlet f x = myAdd x 1\n";
        assert_eq!(inlined(src, "f"), "fn0(%2) = (+ %2 1)\n");
    }

    #[test]
    fn inlines_the_compare_prelude_wrapper() {
        // `Prelude.compare` is `let compare a b = Prim.compare a b`, so a saturated
        // `compare a b` inlines to the comparison primitive — at codegen, one
        // inline immediate compare on a known-immediate operand — not a call into
        // the wrapper.
        let src = "module M\n\nlet f a b = compare a b\n";
        assert_eq!(inlined(src, "f"), "fn0(%0, %1) = (compare %0 %1)\n");
    }

    #[test]
    fn inlines_permuted_wrapper_with_order_preserving_lets() {
        // `Array.push x xs = Prim.arrayPush xs x` reverses its operands. The call's
        // arguments are bound in source order (%2 = x, %3 = xs) and referenced
        // through the permutation, so evaluation order is preserved.
        let src = "module M\n\nlet f x xs = Array.push x xs\n";
        assert_eq!(
            inlined(src, "f"),
            "fn0(%0, %1) = (let %2 = %0; (let %3 = %1; (arrayPush %3 %2)))\n"
        );
    }

    #[test]
    fn does_not_inline_first_class_use() {
        // The wrapper is used as a value, not in a saturated call, so it keeps its
        // `Global` reference (and stays compiled for first-class use).
        let src = "module M\n\nlet f = Int.toString\n";
        assert_eq!(inlined(src, "f"), raw(src, "f"));
    }

    #[test]
    fn does_not_inline_partial_application() {
        let src = "module M\n\nlet myAdd a b = a + b\n\nlet f = myAdd 1\n";
        assert_eq!(inlined(src, "f"), raw(src, "f"));
    }

    #[test]
    fn does_not_inline_non_wrapper_call() {
        // `g`'s parameter takes local %0, so `f`'s `x` is %1.
        let src = "module M\n\nlet g x = x + 1\n\nlet f x = g x\n";
        assert_eq!(inlined(src, "f"), "fn0(%1) = (app @g %1)\n");
    }

    #[test]
    fn inlines_inside_a_lifted_lambda() {
        // The wrapper call lives in a lifted lambda (`fun x -> Int.toString x`),
        // which `core_inlined` rewrites just like the entry function.
        let src = "module M\n\nlet f = fun x -> Int.toString x\n";
        let out = inlined(src, "f");
        assert!(out.contains("(intToString %"), "expected inlined prim in the lambda:\n{out}");
        assert!(!out.contains("@toString"), "wrapper call should be gone:\n{out}");
    }

    #[test]
    fn recognizes_and_inlines_a_foreign_wrapper() {
        // A `foreign` declaration's synthesized entry is itself an
        // eta-foreign-wrapper (its body is a bare `Foreign` over its parameters), so
        // a saturated call to it folds to a bare foreign call — the foreign peer of
        // the prim wrapper — with no wrapper hop.
        use crate::{ForeignWrapper, foreign_wrapper};
        let src = "module M\n\n\
            foreign \"fai_x\" prim : Int -> Int / { Console }\n\n\
            let caller n = prim n\n";
        let (db, file) = db_with(src);
        assert_eq!(
            foreign_wrapper(&db, file, Symbol::intern("prim")),
            Some(ForeignWrapper { symbol: Symbol::intern("fai_x"), slots: vec![0] })
        );
        assert_eq!(inlined(src, "caller"), "fn0(%0) = (foreign \"fai_x\" %0)\n");
    }
}

// ── General helper inlining ──────────────────────────────────────────────────

mod helper_inline {
    use fai_syntax::Symbol;

    use super::db_with;
    use crate::{core_inlined, helper_inlined, inline_summary, pretty_def};

    /// `name`'s Core after general helper inlining, rendered compactly.
    fn folded(src: &str, name: &str) -> String {
        let (db, file) = db_with(src);
        pretty_def(&helper_inlined(&db, file, Symbol::intern(name)))
    }

    /// `name`'s Core after only intrinsic (prim) inlining, rendered compactly.
    fn prim_only(src: &str, name: &str) -> String {
        let (db, file) = db_with(src);
        pretty_def(&core_inlined(&db, file, Symbol::intern(name)))
    }

    /// The inliner's eligibility verdict for `name` (its arity when inlinable).
    fn summary(src: &str, name: &str) -> Option<usize> {
        let (db, file) = db_with(src);
        inline_summary(&db, file, Symbol::intern(name))
    }

    // Eligibility (`inline_summary`).

    #[test]
    fn small_non_recursive_helper_is_eligible() {
        // `inc x = x + 1` is not a prim wrapper (the `1` operand is not a parameter),
        // so the intrinsic inliner leaves it; the helper inliner admits it (arity 1).
        let src = "module M\n\nlet inc x = x + 1\n";
        assert_eq!(summary(src, "inc"), Some(1));
    }

    #[test]
    fn self_recursive_helper_is_ineligible() {
        let src = "module M\n\nlet loop n = if n <= 0 then 0 else loop (n - 1)\n";
        assert_eq!(summary(src, "loop"), None);
    }

    #[test]
    fn mutually_recursive_helper_is_ineligible() {
        let src = "module M\n\nlet ping n = if n <= 0 then 0 else pong (n - 1)\n\n\
                   let pong n = if n <= 0 then 1 else ping (n - 1)\n";
        assert_eq!(summary(src, "ping"), None);
        assert_eq!(summary(src, "pong"), None);
    }

    #[test]
    fn nullary_binding_is_ineligible() {
        // A 0-arg constant is referenced as a bare global, not a call; out of scope.
        let src = "module M\n\nlet answer = 42\n";
        assert_eq!(summary(src, "answer"), None);
    }

    #[test]
    fn lambda_carrying_helper_is_ineligible() {
        // A nested lambda lifts to a second function; a multi-function definition is
        // not inlined (its `MakeClosure` would reference a sibling that is not copied).
        let src = "module M\n\nlet adder x = fun y -> x + y\n";
        assert_eq!(summary(src, "adder"), None);
    }

    #[test]
    fn oversized_helper_is_ineligible() {
        // A long sum chain exceeds the node budget; a short one does not.
        let big_terms = vec!["x"; 60].join(" + ");
        let big = format!("module M\n\nlet big x = {big_terms}\n");
        assert_eq!(summary(&big, "big"), None, "a >64-node body is over budget");
        let small = "module M\n\nlet small x = x + x + x\n";
        assert_eq!(summary(small, "small"), Some(1));
    }

    // The inliner (`helper_inlined`).

    #[test]
    fn inlines_a_small_helper_call() {
        // `f`'s call to `inc` becomes the bound-and-folded body; `inc`'s parameter
        // (%0) is remapped to a fresh local (%2) bound to the argument (%1).
        let src = "module M\n\nlet inc x = x + 1\n\nlet f y = inc y\n";
        assert_eq!(prim_only(src, "f"), "fn0(%1) = (app @inc %1)\n");
        assert_eq!(folded(src, "f"), "fn0(%1) = (let %2 = %1; (+ %2 1))\n");
    }

    #[test]
    fn folds_transitively_through_a_chain() {
        // `outer` calls `inner`; `f` calls `outer`. The folded `f` contains neither
        // call: `inner` was folded into `outer`, and that folded `outer` into `f`.
        let src = "module M\n\nlet inner x = x + 1\n\n\
                   let outer y = inner y + 10\n\n\
                   let f z = outer z\n";
        let out = folded(src, "f");
        assert!(!out.contains("@outer"), "outer should be folded into f:\n{out}");
        assert!(!out.contains("@inner"), "inner should be folded transitively:\n{out}");
    }

    #[test]
    fn does_not_inline_a_recursive_call() {
        // `loop` is recursive, so a caller keeps the call (the body is not unrolled).
        let src = "module M\n\nlet loop n = if n <= 0 then 0 else loop (n - 1)\n\n\
                   let f x = loop x\n";
        let out = folded(src, "f");
        assert!(out.contains("@loop"), "a recursive callee stays a call:\n{out}");
    }

    #[test]
    fn does_not_inline_a_first_class_use() {
        // `inc` is used as a value, not in a call, so it keeps its `Global` reference.
        let src = "module M\n\nlet inc x = x + 1\n\nlet f = inc\n";
        assert_eq!(folded(src, "f"), prim_only(src, "f"));
    }

    #[test]
    fn does_not_inline_a_partial_application() {
        // An under-saturated call is a closure, not a saturated direct call.
        let src = "module M\n\nlet add a b = a + b + 0\n\nlet f = add 1\n";
        assert_eq!(folded(src, "f"), prim_only(src, "f"));
    }

    #[test]
    fn inlines_the_prefix_of_an_over_application() {
        // `pick` (arity 1) returns a function; `pick true x` over-applies it. The
        // prefix is inlined and the surplus argument applied to the result, so the
        // call to `pick` is gone but the returned functions remain referenced.
        let src = "module M\n\nlet twice n = n + n\n\nlet thrice n = n + n + n\n\n\
                   let pick b = if b then twice else thrice\n\n\
                   let f x = pick true x\n";
        let out = folded(src, "f");
        assert!(!out.contains("@pick"), "pick's prefix should be inlined:\n{out}");
        assert!(out.contains("(if "), "the inlined body keeps its conditional:\n{out}");
        assert!(out.contains("@twice") && out.contains("@thrice"), "branches stay:\n{out}");
    }

    #[test]
    fn unchanged_definition_returns_the_prim_inlined_form() {
        // A definition with no inlinable calls is returned as-is (the early-cutoff
        // fast path), identical to its intrinsic-inlined form.
        let src = "module M\n\nlet f x = x + 1\n";
        assert_eq!(folded(src, "f"), prim_only(src, "f"));
    }

    /// The eligibility verdict for `name` in the standard-library module `module`.
    fn std_summary(module: &str, name: &str) -> Option<usize> {
        let (db, _m) = db_with("module M\n");
        let file = fai_resolve::module_file(&db, fai_resolve::ModuleName(Symbol::intern(module)))
            .expect("the standard library module is embedded");
        inline_summary(&db, file, Symbol::intern(name))
    }

    #[test]
    fn dict_smart_constructors_inline_but_balance_does_not() {
        // The reuse-critical smart constructors are small enough to fold in, so a
        // hot-spine `bin`/`singleton` call recycles the matched cell. The rotating
        // `balance` stays a shared call (well over the budget), so rebalancing
        // allocates fresh — exactly the cost model the acceptance pins.
        assert!(std_summary("Dict", "bin").is_some(), "Dict.bin must inline");
        assert!(std_summary("Dict", "singleton").is_some(), "Dict.singleton must inline");
        assert_eq!(std_summary("Dict", "balance"), None, "Dict.balance must stay a call");
        assert!(std_summary("Set", "bin").is_some(), "Set.bin must inline");
        assert!(std_summary("Set", "singleton").is_some(), "Set.singleton must inline");
        assert_eq!(std_summary("Set", "balance"), None, "Set.balance must stay a call");
    }
}

mod simplify {
    use std::sync::Arc;

    use fai_db::Db;
    use fai_syntax::Symbol;

    use super::db_with;
    use crate::{core_inlined, helper_inlined, pretty_def, simplified};

    /// `name`'s Core after the simplify pass, rendered compactly.
    fn simp(src: &str, name: &str) -> String {
        let (db, file) = db_with(src);
        pretty_def(&simplified(&db, file, Symbol::intern(name)))
    }

    /// `name`'s Core after simplify then helper inlining, rendered compactly.
    fn simp_helper(src: &str, name: &str) -> String {
        let (db, file) = db_with(src);
        pretty_def(&helper_inlined(&db, file, Symbol::intern(name)))
    }

    /// The `FoldPipeline` shape: a CAF composed from `>>` and a partial application,
    /// folded over a range. The headline closure-confinement case.
    const FOLD_PIPELINE: &str = "module M\n\n\
        let shift k x = x + k\n\n\
        let transform = (fun x -> x + 1) >> (fun x -> x * 2) >> shift 3\n\n\
        let run n = List.foldl (fun acc x -> acc + transform x) 0 (List.range 0 n)\n";

    #[test]
    fn caf_composition_reduces_to_arithmetic() {
        // Inlining the `transform` CAF, reducing the two `>>` compositions, and
        // beta-reducing the two lambdas leaves a saturated `shift` call over pure
        // arithmetic — no closure, no `>>`, no reference to the now-dead `transform`.
        let got = simp(FOLD_PIPELINE, "run");
        assert_eq!(
            got,
            "fn0(%4) = (app @foldl (closure/static fn1 []) 0 (app @range 0 %4))\n\
             fn1(%5, %6) = (+ %5 (app @shift 3 (let %10 = (let %9 = %6; (+ %9 1)); (* %10 2))))\n",
        );
        assert!(!got.contains("@transform"), "the CAF is inlined away: {got}");
        assert!(!got.contains("@>>"), "the compositions are reduced: {got}");
        assert!(!got.contains("closure/heap"), "no heap closure remains: {got}");
    }

    #[test]
    fn fold_pipeline_helper_inlines_shift_to_pure_arithmetic() {
        // After simplify, the residual same-file `shift` call is folded by helper
        // inlining, so the fold's element body is pure `+`/`*` — what fusion then
        // turns into a register loop (no closure, no call).
        let got = simp_helper(FOLD_PIPELINE, "run");
        assert!(!got.contains("@shift"), "shift is helper-inlined: {got}");
        assert!(!got.contains("@transform") && !got.contains("@>>"), "no composition: {got}");
        assert!(!got.contains("closure/heap") && !got.contains("(app stack"), "no closure: {got}");
        // The element body is the composed arithmetic `((x + 1) * 2) + 3`.
        assert!(got.contains("(+ ") && got.contains("(* "), "arithmetic body: {got}");
    }

    #[test]
    fn identity_application_reduces_to_its_argument() {
        let src = "module M\n\nlet f x = identity x\n";
        assert_eq!(simp(src, "f"), "fn0(%0) = %0\n");
    }

    #[test]
    fn pipe_application_reduces_to_a_direct_call() {
        // `x |> g` becomes `g x` — a direct application of the same-file `g`.
        let src = "module M\n\nlet g y = y + 1\n\nlet f x = x |> g\n";
        let got = simp(src, "f");
        assert_eq!(got, "fn0(%1) = (app @g %1)\n");
        assert!(!got.contains("@|>"), "the pipe operator is reduced away: {got}");
    }

    #[test]
    fn const_application_keeps_the_discarded_operand() {
        // `const a b` reduces to `a` but binds `b` to a dead local, so its strict
        // evaluation (and any effect/trap) is preserved.
        let src = "module M\n\nlet f a b = const a b\n";
        assert_eq!(simp(src, "f"), "fn0(%0, %1) = (let %2 = %1; %0)\n");
    }

    #[test]
    fn beta_reduces_an_applied_lambda() {
        // `(fun y -> y + 1) x` inlines the lambda, binding its parameter to a fresh
        // local; the lifted lambda is then dead and pruned (one function remains).
        let src = "module M\n\nlet f x = (fun y -> y + 1) x\n";
        let got = simp(src, "f");
        assert_eq!(got, "fn0(%0) = (let %2 = %0; (+ %2 1))\n");
        assert!(!got.contains("closure"), "the applied lambda is beta-reduced: {got}");
    }

    #[test]
    fn beta_reduces_an_applied_capturing_lambda() {
        // A capturing lambda applied in place inlines too, mapping its capture slot
        // to the supplied local.
        let src = "module M\n\nlet f k x = (fun y -> y + k) x\n";
        let got = simp(src, "f");
        assert!(!got.contains("closure"), "the capturing lambda is beta-reduced: {got}");
        // The body adds the argument (bound fresh) to the captured `k` (%0).
        assert!(got.contains("(+ ") && got.contains("%0"), "captured k is used: {got}");
    }

    #[test]
    fn caf_partial_application_flattens_to_a_saturated_call() {
        // A CAF bound to a partial application (`shift 3`) inlines and the curried
        // application flattens to one saturated call.
        let src = "module M\n\nlet shift k x = x + k\n\nlet add3 = shift 3\n\nlet f x = add3 x\n";
        let got = simp(src, "f");
        assert_eq!(got, "fn0(%2) = (app @shift 3 %2)\n");
        assert!(!got.contains("@add3"), "the CAF is inlined: {got}");
    }

    #[test]
    fn value_position_caf_is_not_inlined() {
        // A CAF used as a value (passed to `List.map`), not applied, is left alone —
        // inlining it there only relocates allocation for no benefit.
        let src = "module M\n\n\
            let transform = (fun x -> x + 1) >> (fun x -> x * 2)\n\n\
            let f xs = List.map transform xs\n";
        let got = simp(src, "f");
        assert!(got.contains("@transform"), "a value-position CAF is kept: {got}");
    }

    #[test]
    fn impure_compose_operand_is_not_reduced() {
        // `(getf 0 >> double) x`: the first operand is an indirect call (through a
        // parameter), which the purity guard treats as impure, so the composition is
        // left intact rather than reordered.
        let src = "module M\n\n\
            let double y = y * 2\n\n\
            let f getf x = (getf 0 >> double) x\n";
        let got = simp(src, "f");
        assert!(got.contains("@>>"), "an impure-operand composition is not reduced: {got}");
    }

    #[test]
    fn simplify_is_skipped_in_the_standard_library() {
        // A file classified as standard-library (its path under `<std>/`) is not
        // simplified, so the combinators stay exercised by their own contracts. The
        // identical construct in a user module *is* reduced.
        let user = "module U\n\nlet g y = y + 1\n\nlet f x = x |> g\n";
        let std = "module S\n\nlet g y = y + 1\n\nlet f x = x |> g\n";
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let uid = db.add_source("U.fai".into(), user.to_owned());
        let sid = db.add_source("<std>/S.fai".into(), std.to_owned());
        let ufile = db.source_file(uid).unwrap();
        let sfile = db.source_file(sid).unwrap();
        let name = Symbol::intern("f");

        // The user module reduces the pipe; the std-classified one does not.
        assert!(!pretty_def(&simplified(&db, ufile, name)).contains("@|>"), "user reduced");
        assert!(pretty_def(&simplified(&db, sfile, name)).contains("@|>"), "std kept");
        // The std skip returns the `core_inlined` input unchanged (pointer-equal).
        assert!(Arc::ptr_eq(&simplified(&db, sfile, name), &core_inlined(&db, sfile, name)));
    }

    #[test]
    fn caf_with_an_unsupported_body_is_not_inlined() {
        // `(<)` as a first-class value is outside the native subset (a lowering
        // error node with a diagnostic). Inlining such a CAF would move the error
        // into the caller and make the CAF unreachable, dropping its diagnostic — so
        // a CAF whose body contains an error is left as a call (still reachable).
        let src = "module M\n\n\
            public lt : Int -> Int -> Bool\n\
            let lt = (<)\n\n\
            public run : Int -> Bool\n\
            let run k = lt k 2\n";
        let got = simp(src, "run");
        assert!(got.contains("@lt"), "an error-bodied CAF is not inlined: {got}");
    }

    #[test]
    fn row_polymorphic_call_evidence_is_not_flattened() {
        // A row-polymorphic function's call lowers to a nested application whose
        // leading argument is offset *evidence* (`(app (app @shift 0) rec)`), which
        // code generation passes as a separate partial application. Flattening must
        // leave it intact (an unsignatured record-update helper generalizes to
        // row-polymorphic, so this is the common case).
        let src = "module M\n\n\
            type P = { x : Int, y : Int }\n\n\
            let shift p = { p with x = p.x + 1 }\n\n\
            public run : Int -> Int\n\
            let run k = (shift { x = k, y = 0 }).x\n";
        let got = simp(src, "run");
        assert!(got.contains("(app (app @shift"), "evidence application is preserved: {got}");
    }

    #[test]
    fn cross_module_caf_is_not_inlined() {
        // CAF inlining is intra-file (the cross-module firewall): a public CAF
        // applied from another module is left as a call, not spliced, so a body edit
        // never crosses a module boundary.
        let a = "module A\n\n\
            public transform : Int -> Int\n\
            let transform = (fun x -> x + 1) >> (fun x -> x * 2)\n";
        let b = "module B\n\n\
            public run : Int -> Int\n\
            let run x = A.transform x\n";
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        db.add_source("A.fai".into(), a.to_owned());
        let bid = db.add_source("B.fai".into(), b.to_owned());
        let bfile = db.source_file(bid).unwrap();
        let got = pretty_def(&simplified(&db, bfile, Symbol::intern("run")));
        assert!(got.contains("@transform"), "a cross-module CAF is not inlined: {got}");
    }

    #[test]
    fn a_user_shadowed_compose_is_not_reduced() {
        // Recognition is by resolved identity: a module that defines its own `>>`
        // resolves the call to *its* definition, not `Prelude`'s, so the composition
        // is left intact (a user operator is an ordinary function).
        let src = "module M\n\n\
            let (>>) f g = fun x -> g (f x)\n\n\
            let inc z = z + 1\n\n\
            let double y = y * 2\n\n\
            let f x = (inc >> double) x\n";
        assert!(simp(src, "f").contains("@>>"), "a shadowed `>>` is not the recognized combinator");
    }

    #[test]
    fn caf_body_edit_resimplifies_caller_but_not_recognition() {
        // Editing the `transform` CAF's body re-runs `simplified` for `run` (the
        // inlining dependency is tracked) and changes its result, but does not re-run
        // `combinator_defs` — recognition reads only module headers, so it is
        // firewalled from any combinator/CAF body edit.
        let module = |k: i64| {
            format!(
                "module M\n\n\
                 let shift k x = x + k\n\n\
                 let transform = (fun x -> x + 1) >> (fun x -> x * 2) >> shift {k}\n\n\
                 let run n = List.foldl (fun acc x -> acc + transform x) 0 (List.range 0 n)\n"
            )
        };
        let mut db = fai_db::FaiDatabase::new();
        fai_types::std_lib::load_std(&mut db);
        let id = db.add_source("M.fai".into(), module(3));
        let file = db.source_file(id).unwrap();
        let run = Symbol::intern("run");
        let before = pretty_def(&simplified(&db, file, run));

        db.enable_event_log();
        db.add_source("M.fai".into(), module(5));
        let after = pretty_def(&simplified(&db, file, run));
        let events = db.take_events();

        assert_ne!(before, after, "a CAF body edit changes the caller's reduced body");
        assert!(after.contains("@shift 5"), "the edited constant flows into `run`: {after}");
        assert!(
            events.iter().any(|e| e.contains("simplified")),
            "`run` is re-simplified after the CAF body edit",
        );
        assert!(
            !events.iter().any(|e| e.contains("combinator_defs")),
            "recognition (combinator_defs) is firewalled from a body edit: {events:?}",
        );
    }
}
