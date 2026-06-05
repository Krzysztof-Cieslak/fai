//! Formatter golden snapshots, idempotence, and property tests.

use fai_span::SourceId;
use fai_syntax::ast::{
    ExprId, ExprKind, Item, ItemKind, LetStmt, Module, PatId, PatKind, RowTail, TypeId, TypeKind,
};
use fai_syntax::{ItemTree, TokenKind, build_item_tree, parse_module};
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;

fn fmt(src: &str) -> String {
    let parsed = parse_module(SourceId::new(0), src);
    fai_fmt::format(&parsed.module, &parsed.comments, src)
}

/// Persist proptest counterexamples in a committed, crate-local
/// `proptest-regressions/` directory. proptest's source-parallel default cannot
/// locate a crate root for integration tests, so it would otherwise drop the
/// seeds beside the source file under an awkward name; pinning the path keeps the
/// saved cases tidy and in source control (cargo runs the test with the package
/// root as the working directory).
fn regression_config() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/format.txt",
        ))),
        ..ProptestConfig::default()
    }
}

/// Formatting an already-formatted program is a no-op.
fn assert_idempotent(src: &str) {
    let once = fmt(src);
    let twice = fmt(&once);
    assert_eq!(once, twice, "fmt is not idempotent\n=== once ===\n{once}\n=== twice ===\n{twice}");
}

#[test]
fn hello() {
    let src = "module Hello\npublic main : Runtime -> Unit\nlet main runtime =\n  runtime.console.writeLine \"Hello, Fai!\"";
    insta::assert_snapshot!("hello", fmt(src));
    assert_idempotent(src);
}

#[test]
fn signatures_and_operators() {
    let src = "module Basics\npublic add : Int -> Int -> Int\nlet add x y = x + y\nlet ratio = 3.0 / 2.0\nlet isEven = count % 2 = 0";
    insta::assert_snapshot!("basics", fmt(src));
    assert_idempotent(src);
}

#[test]
fn pipes_collapse_when_they_fit() {
    let src = "module Funcs\npublic describe : Int -> String\nlet describe n =\n  n\n  |> inc\n  |> Int.toString";
    insta::assert_snapshot!("pipes", fmt(src));
    assert_idempotent(src);
}

#[test]
fn local_let_block() {
    let src = "module Locals\npublic hypotenuse : Float -> Float -> Float\nlet hypotenuse a b =\n  let a2 = a * a\n  let b2 = b * b\n  sqrt (a2 + b2)";
    insta::assert_snapshot!("locals", fmt(src));
    assert_idempotent(src);
}

#[test]
fn if_else_chain_collapses_when_it_fits() {
    let src = "module Locals\npublic classify : Int -> String\nlet classify n =\n  if n < 0 then \"negative\"\n  else if n = 0 then \"zero\"\n  else \"positive\"";
    insta::assert_snapshot!("classify", fmt(src));
    assert_idempotent(src);
}

#[test]
fn multiline_if_when_it_does_not_fit() {
    let src = "module M\nlet f x =\n  if someVeryLongCondition x then theFirstRatherLongBranchResult x else theSecondEquallyLongBranchResultValue x";
    insta::assert_snapshot!("multiline_if", fmt(src));
    assert_idempotent(src);
}

#[test]
fn tuples_lists_and_contracts() {
    let src = "module Tuples\npublic divMod : Int -> Int -> Int * Int\nlet divMod a b = (a / b, a % b)\nexample: divMod 7 3 = (2, 1)\nlet xs = [1, 2, 3]\npublic swap : 'a * 'b -> 'b * 'a\nlet swap pair =\n  let (x, y) = pair\n  (y, x)";
    insta::assert_snapshot!("tuples", fmt(src));
    assert_idempotent(src);
}

#[test]
fn comments_doc_leading_and_trailing() {
    let src = "module Comments\n// a standalone note\n/// Doc for answer.\npublic answer : Int\nlet answer = 42 // the trailing answer";
    insta::assert_snapshot!("comments", fmt(src));
    assert_idempotent(src);
}

#[test]
fn trailing_comment_on_local_let_survives() {
    // Exercises the expression-trailing attachment path end to end.
    let src = "module M\nlet f =\n  let a = 1 // keep me\n  a";
    let out = fmt(src);
    assert!(out.contains("let a = 1 // keep me"), "comment dropped:\n{out}");
    assert_idempotent(src);
}

#[test]
fn messy_input_is_canonicalized() {
    // Extra blank lines and odd spacing collapse to the canonical layout.
    let src = "module M\n\n\n\nlet    x=1\n\n\n\nlet y   =   2";
    insta::assert_snapshot!("messy", fmt(src));
    assert_idempotent(src);
}

proptest! {
    #![proptest_config(regression_config())]

    /// Formatting arbitrary input never panics.
    #[test]
    fn format_never_panics(input in any::<String>()) {
        let parsed = parse_module(SourceId::new(0), &input);
        let _ = fai_fmt::format(&parsed.module, &parsed.comments, &input);
    }

    /// Formatting is idempotent on generated bindings.
    #[test]
    fn idempotent_on_generated_bindings(name in "[a-z][a-zA-Z0-9_]*", value in 0u32..100_000) {
        prop_assume!(TokenKind::keyword(&name).is_none());
        let src = format!("module M\nlet {name} = {value}");
        let once = fmt(&src);
        let twice = fmt(&once);
        prop_assert_eq!(once, twice);
    }
}

// --- broader coverage -------------------------------------------------------

fn item_tree_of(src: &str) -> ItemTree {
    build_item_tree(&parse_module(SourceId::new(0), src).module)
}

/// Formatting must be idempotent, reparse cleanly, and preserve the item tree.
fn assert_canonical(src: &str) -> String {
    let once = fmt(src);
    let reparsed = parse_module(SourceId::new(0), &once);
    assert!(reparsed.diagnostics.is_empty(), "fmt output did not reparse cleanly:\n{once}");
    assert_eq!(fmt(&once), once, "fmt is not idempotent:\n{once}");
    assert_eq!(
        item_tree_of(src),
        build_item_tree(&reparsed.module),
        "fmt changed the item tree:\n{once}"
    );
    once
}

#[test]
fn all_binary_operators_format_with_spaces() {
    let src = "module M\nlet a = w - x * y / z % p\nlet b = c ++ d :: e\nlet c = p && q || r\nlet d = a = b\nlet e = a <> b\nlet f = a < b\nlet g = a <= b\nlet h = a > b\nlet i = a >= b\nlet j = f >> g\nlet k = x |> f";
    let out = assert_canonical(src);
    for needle in [
        "w - x * y / z % p",
        "c ++ d :: e",
        "p && q || r",
        "a = b",
        "a <> b",
        "a <= b",
        "a >= b",
        "f >> g",
        "x |> f",
    ] {
        assert!(out.contains(needle), "missing `{needle}` in:\n{out}");
    }
}

#[test]
fn parens_are_preserved_and_not_invented() {
    assert!(assert_canonical("module M\nlet x = a + b * c").contains("let x = a + b * c"));
    assert!(assert_canonical("module M\nlet x = (a + b) * c").contains("let x = (a + b) * c"));
    assert!(assert_canonical("module M\nlet x = a - (b - c)").contains("let x = a - (b - c)"));
    assert!(assert_canonical("module M\nlet x = ((a))").contains("let x = ((a))"));
}

#[test]
fn unary_minus_and_negatives() {
    assert!(assert_canonical("module M\nlet x = -a * b").contains("-a * b"));
    assert!(assert_canonical("module M\nlet y = f (-3)").contains("f (-3)"));
    assert!(assert_canonical("module M\nlet z = 0 - n").contains("0 - n"));
}

#[test]
fn literals_are_reproduced_verbatim() {
    let out = assert_canonical(
        "module M\nlet a = 0xFF\nlet b = 1_000\nlet c = 'a'\nlet d = 3.0\nlet e = \"hi\"",
    );
    for needle in ["= 0xFF", "= 1_000", "= 'a'", "= 3.0", "= \"hi\""] {
        assert!(out.contains(needle), "missing `{needle}` in:\n{out}");
    }
}

#[test]
fn string_escapes_are_preserved() {
    let out = assert_canonical("module M\nlet s = \"a\\nb\"");
    assert!(out.contains("let s = \"a\\nb\""), "out:\n{out}");
}

#[test]
fn type_signatures_format() {
    assert!(
        assert_canonical("module M\npublic f : Int -> Int -> Int\nlet f a b = a")
            .contains("public f : Int -> Int -> Int")
    );
    assert!(
        assert_canonical("module M\npublic g : 'a * 'b -> 'b * 'a\nlet g p = p")
            .contains("public g : 'a * 'b -> 'b * 'a")
    );
    assert!(
        assert_canonical("module M\npublic h : ('a -> 'b) -> List 'a -> List 'b\nlet h f = f")
            .contains("public h : ('a -> 'b) -> List 'a -> List 'b")
    );
}

#[test]
fn lambda_forms() {
    assert!(assert_canonical("module M\nlet a = fun x -> x").contains("fun x -> x"));
    assert!(
        assert_canonical("module M\nlet b = fun acc x -> acc + x").contains("fun acc x -> acc + x")
    );
    assert!(assert_canonical("module M\nlet c = fun (x, y) -> x").contains("fun (x, y) -> x"));
}

#[test]
fn field_access_and_application() {
    assert!(assert_canonical("module M\nlet a = r.x.y").contains("r.x.y"));
    assert!(assert_canonical("module M\nlet b = f (g x) y").contains("f (g x) y"));
}

#[test]
fn collections_and_unit() {
    assert!(assert_canonical("module M\nlet a = [(1, 2), (3, 4)]").contains("[(1, 2), (3, 4)]"));
    assert!(assert_canonical("module M\nlet b = ()").contains("let b = ()"));
    assert!(assert_canonical("module M\nlet c = []").contains("let c = []"));
}

#[test]
fn block_comment_leads_an_item() {
    assert!(assert_canonical("module M\n(* a note *)\nlet x = 1").contains("(* a note *)"));
}

#[test]
fn trailing_comment_on_a_signature() {
    let out = assert_canonical("module M\npublic f : Int // sig note\nlet f = 1");
    assert!(out.contains("public f : Int // sig note"), "out:\n{out}");
}

#[test]
fn aligned_trailing_comment_collapses_to_one_space() {
    let out = assert_canonical("module M\nlet x = 3        // aligned");
    assert!(out.contains("let x = 3 // aligned"), "out:\n{out}");
}

#[test]
fn comment_only_module_keeps_the_comment() {
    assert!(assert_canonical("module M\n// lonely").contains("// lonely"));
}

#[test]
fn contracts_stay_in_the_binding_group() {
    let out =
        assert_canonical("module M\npublic f : Int\nlet f = 1\nexample: f = 1\nforall x: f = x");
    assert!(
        out.contains("public f : Int\nlet f = 1\nexample: f = 1\nforall x: f = x"),
        "contracts were split from the binding:\n{out}",
    );
}

#[test]
fn distinct_bindings_get_a_blank_line() {
    assert!(assert_canonical("module M\nlet a = 1\nlet b = 2").contains("let a = 1\n\nlet b = 2"));
}

#[test]
fn equivalent_inputs_format_identically() {
    assert_eq!(fmt("module M\nlet x = a + b"), fmt("module M\n\n\nlet   x   =   a+b"));
}

proptest! {
    #![proptest_config(regression_config())]

    /// fmt output of a generated program reparses cleanly and is idempotent.
    #[test]
    fn generated_program_is_canonical(name in "[a-z][a-zA-Z0-9_]*", a in 0u32..1000, b in 0u32..1000) {
        prop_assume!(TokenKind::keyword(&name).is_none());
        let src = format!("module M\nlet {name} = {a} + {b} * {a}");
        let once = fmt(&src);
        let reparsed = parse_module(SourceId::new(0), &once);
        prop_assert!(reparsed.diagnostics.is_empty());
        prop_assert_eq!(fmt(&once), once);
        prop_assert_eq!(item_tree_of(&src), build_item_tree(&reparsed.module));
    }
}

// --- structural round-trip (span-free shape) --------------------------------
//
// `item_tree_of` only captures top-level names/kinds; it cannot see whether a
// *body* survives formatting. `shape` renders the entire tree to a span-free
// S-expression, so comparing `shape(parse(src))` with `shape(parse(fmt(src)))`
// proves the formatter preserves every node, nesting, operator, and literal.
//
// A `Block` whose only content is its tail is semantically that tail: the
// formatter collapses `let f =\n  x` to `let f = x` and re-expands a body when
// it must break, so tail-only blocks are normalized away on both sides of the
// comparison (they are the one intended, sound shape change).

fn shape(m: &Module) -> String {
    let mut out = format!("module {:?}", m.name.map(|s| s.as_str()));
    for item in &m.items {
        out.push('\n');
        out.push_str(&shape_item(m, item));
    }
    out
}

fn shape_item(m: &Module, item: &Item) -> String {
    match &item.kind {
        ItemKind::Signature { visibility, name, ty } => {
            format!("(sig {visibility:?} {} {})", name.as_str(), shape_type(m, *ty))
        }
        ItemKind::Binding { visibility, name, params, body } => format!(
            "(let {visibility:?} {} [{}] {})",
            name.as_str(),
            shape_pats(m, params),
            shape_expr(m, *body),
        ),
        ItemKind::Type { visibility, name, params, def } => {
            let ps = params.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(" ");
            let body = match def {
                fai_syntax::ast::TypeDef::Alias(ty) => format!("(alias {})", shape_type(m, *ty)),
                fai_syntax::ast::TypeDef::Union(variants) => {
                    let vs = variants
                        .iter()
                        .map(|v| {
                            let fs = v
                                .fields
                                .iter()
                                .map(|f| shape_type(m, *f))
                                .collect::<Vec<_>>()
                                .join(" ");
                            format!("(variant {} [{}])", v.name.as_str(), fs)
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!("(union {vs})")
                }
            };
            format!("(type {visibility:?} {} [{}] {})", name.as_str(), ps, body)
        }
        ItemKind::Example { body } => format!("(example {})", shape_expr(m, *body)),
        ItemKind::Forall { binders, body } => format!(
            "(forall [{}] {})",
            binders.iter().map(|b| b.as_str()).collect::<Vec<_>>().join(" "),
            shape_expr(m, *body),
        ),
        ItemKind::Error => "(item-error)".to_owned(),
    }
}

fn shape_expr(m: &Module, id: ExprId) -> String {
    match &m.expr(id).kind {
        ExprKind::Int(s) => format!("(int {})", s.as_str()),
        ExprKind::Float(s) => format!("(float {})", s.as_str()),
        ExprKind::String(s) => format!("(string {})", s.as_str()),
        ExprKind::Char(s) => format!("(char {})", s.as_str()),
        ExprKind::Var(s) => format!("(var {})", s.as_str()),
        ExprKind::Unit => "(unit)".to_owned(),
        ExprKind::App { func, arg } => {
            format!("(app {} {})", shape_expr(m, *func), shape_expr(m, *arg))
        }
        ExprKind::Binary { op, lhs, rhs } => {
            format!("({op:?} {} {})", shape_expr(m, *lhs), shape_expr(m, *rhs))
        }
        ExprKind::Unary { op, operand } => format!("({op:?} {})", shape_expr(m, *operand)),
        ExprKind::If { cond, then_branch, else_branch } => format!(
            "(if {} {} {})",
            shape_expr(m, *cond),
            shape_expr(m, *then_branch),
            shape_expr(m, *else_branch),
        ),
        ExprKind::Lambda { params, body } => {
            format!("(fun [{}] {})", shape_pats(m, params), shape_expr(m, *body))
        }
        ExprKind::Block { stmts, tail } if stmts.is_empty() => shape_expr(m, *tail),
        ExprKind::Block { stmts, tail } => format!(
            "(block [{}] {})",
            stmts.iter().map(|s| shape_stmt(m, s)).collect::<Vec<_>>().join(" "),
            shape_expr(m, *tail),
        ),
        ExprKind::Field { base, field } => {
            format!("(field {} {})", shape_expr(m, *base), field.as_str())
        }
        ExprKind::Paren(inner) => format!("(paren {})", shape_expr(m, *inner)),
        ExprKind::Tuple(xs) => format!("(tuple {})", shape_exprs(m, xs)),
        ExprKind::List(xs) => format!("(list {})", shape_exprs(m, xs)),
        ExprKind::Match { scrutinee, arms } => {
            let arms = arms
                .iter()
                .map(|a| format!("({} -> {})", shape_pat(m, a.pat), shape_expr(m, a.body)))
                .collect::<Vec<_>>()
                .join(" ");
            format!("(match {} [{}])", shape_expr(m, *scrutinee), arms)
        }
        ExprKind::Record(fields) => {
            let mut order: Vec<_> = fields.iter().collect();
            order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            let fs = order
                .iter()
                .map(|f| format!("{} = {}", f.name.as_str(), shape_expr(m, f.value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("(record [{fs}])")
        }
        ExprKind::RecordUpdate { base, fields } => {
            let mut order: Vec<_> = fields.iter().collect();
            order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            let fs = order
                .iter()
                .map(|f| format!("{} = {}", f.name.as_str(), shape_expr(m, f.value)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("(update {} [{fs}])", shape_expr(m, *base))
        }
        ExprKind::Error => "(expr-error)".to_owned(),
    }
}

fn shape_exprs(m: &Module, ids: &[ExprId]) -> String {
    ids.iter().map(|id| shape_expr(m, *id)).collect::<Vec<_>>().join(" ")
}

fn shape_stmt(m: &Module, stmt: &LetStmt) -> String {
    format!(
        "(let {} [{}] {})",
        shape_pat(m, stmt.pat),
        shape_pats(m, &stmt.params),
        shape_expr(m, stmt.value),
    )
}

fn shape_pat(m: &Module, id: PatId) -> String {
    match &m.pat(id).kind {
        PatKind::Var(s) => format!("(pvar {})", s.as_str()),
        PatKind::Wildcard => "(pwild)".to_owned(),
        PatKind::Unit => "(punit)".to_owned(),
        PatKind::Tuple(xs) => {
            format!(
                "(ptuple {})",
                xs.iter().map(|p| shape_pat(m, *p)).collect::<Vec<_>>().join(" ")
            )
        }
        PatKind::Paren(inner) => format!("(pparen {})", shape_pat(m, *inner)),
        PatKind::Constructor { name, args } => {
            format!("(pctor {} [{}])", name.as_str(), shape_pats(m, args))
        }
        PatKind::Int(s) => format!("(pint {})", s.as_str()),
        PatKind::Float(s) => format!("(pfloat {})", s.as_str()),
        PatKind::String(s) => format!("(pstring {})", s.as_str()),
        PatKind::Char(s) => format!("(pchar {})", s.as_str()),
        PatKind::Bool(b) => format!("(pbool {b})"),
        PatKind::List(xs) => format!("(plist {})", shape_pats(m, xs)),
        PatKind::Cons { head, tail } => {
            format!("(pcons {} {})", shape_pat(m, *head), shape_pat(m, *tail))
        }
        PatKind::Or(alts) => format!("(por {})", shape_pats(m, alts)),
        PatKind::Record { fields, open } => {
            let mut order: Vec<_> = fields.iter().collect();
            order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            let fs = order
                .iter()
                .map(|f| {
                    if f.punned {
                        f.name.as_str().to_owned()
                    } else {
                        format!("{} = {}", f.name.as_str(), shape_pat(m, f.pat))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("(precord [{fs}] open={open})")
        }
        PatKind::Error => "(pat-error)".to_owned(),
    }
}

fn shape_pats(m: &Module, ids: &[PatId]) -> String {
    ids.iter().map(|id| shape_pat(m, *id)).collect::<Vec<_>>().join(" ")
}

fn shape_type(m: &Module, id: TypeId) -> String {
    match &m.ty(id).kind {
        TypeKind::Var(s) => format!("(tvar {})", s.as_str()),
        TypeKind::Con(s) => format!("(tcon {})", s.as_str()),
        TypeKind::App { func, arg } => {
            format!("(tapp {} {})", shape_type(m, *func), shape_type(m, *arg))
        }
        TypeKind::Arrow { from, to } => {
            format!("(tarrow {} {})", shape_type(m, *from), shape_type(m, *to))
        }
        TypeKind::Tuple(xs) => format!(
            "(ttuple {})",
            xs.iter().map(|t| shape_type(m, *t)).collect::<Vec<_>>().join(" "),
        ),
        TypeKind::Record { fields, tail } => {
            let mut order: Vec<_> = fields.iter().collect();
            order.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
            let fs = order
                .iter()
                .map(|f| format!("{} : {}", f.name.as_str(), shape_type(m, f.ty)))
                .collect::<Vec<_>>()
                .join(", ");
            let t = match tail {
                RowTail::Closed => String::new(),
                RowTail::Open => " | _".to_owned(),
                RowTail::Named(r) => format!(" | {}", r.as_str()),
            };
            format!("(trecord [{fs}]{t})")
        }
        TypeKind::Unit => "(tunit)".to_owned(),
        TypeKind::Paren(inner) => format!("(tparen {})", shape_type(m, *inner)),
        TypeKind::Error => "(type-error)".to_owned(),
    }
}

#[test]
fn fmt_preserves_structure_examples() {
    // Cases that specifically exercise the block-collapse normalization and the
    // if-break path; the structural shape must survive a format round-trip.
    for src in [
        "module M\nlet f =\n  x",
        "module M\nlet g x = if c then a else b",
        "module M\nlet h x =\n  if someLongCondition then theFirstResult else theSecondResult",
        "module M\nlet a = w - x * y / z % p",
        "module M\nlet b = f (g x) (h y)",
        "module M\nlet t = ((a, b), [c, d])",
        "module M\nlet n = -a - -b",
        "module M\nlet p = a :: b :: c ++ d",
        "module M\npublic q : ('a -> 'b) -> List 'a -> List 'b\nlet q f = f",
        // Data types, match, and records.
        "module M\ntype Color =\n  | Red\n  | Green\n  | Blue",
        "module M\ntype Shape =\n  | Circle Float\n  | Rect Float Float",
        "module M\ntype Opt 'a =\n  | None\n  | Some 'a",
        "module M\ntype Celsius = Int",
        "module M\ntype Vec2 = { x : Float, y : Float }",
        "module M\npublic getX : { x : 'a | _ } -> 'a\nlet getX r = r.x",
        "module M\npublic setX : { x : 'a | 'r } -> { x : 'a | 'r }\nlet setX r = r",
        "module M\nlet p = { x = 1, y = 2 }",
        "module M\nlet q = { r with x = 1, y = 2 }",
        "module M\nlet g r = r.x.y",
        "module M\nlet f x =\n  match x with\n  | Some n -> n\n  | None -> 0",
        "module M\nlet f xs =\n  match xs with\n  | [] -> 0\n  | x :: rest -> x",
        "module M\nlet f n =\n  match n with\n  | 0 | 1 -> 1\n  | _ -> 2",
        "module M\nlet f r =\n  match r with\n  | { x = 0 | _ } -> 0\n  | { x, y } -> x",
    ] {
        let before = parse_module(SourceId::new(0), src);
        assert!(before.diagnostics.is_empty(), "sample did not parse: {src}");
        let out = fmt(src);
        let after = parse_module(SourceId::new(0), &out);
        assert!(after.diagnostics.is_empty(), "reformatted output did not parse:\n{out}");
        assert_eq!(shape(&before.module), shape(&after.module), "src: {src}\nout:\n{out}");
    }
}

#[test]
fn record_expressions_format() {
    assert!(assert_canonical("module M\nlet p = { x = 1, y = 2 }").contains("{ x = 1, y = 2 }"));
    assert!(assert_canonical("module M\nlet q = { r with x = 1 }").contains("{ r with x = 1 }"));
    assert!(assert_canonical("module M\nlet v = r.x.y").contains("r.x.y"));
}

#[test]
fn record_types_in_signatures_format() {
    assert!(
        assert_canonical("module M\npublic mk : Int -> { x : Int, y : Int }\nlet mk n = n")
            .contains("{ x : Int, y : Int }")
    );
    assert!(
        assert_canonical("module M\npublic getX : { x : 'a | _ } -> 'a\nlet getX r = r")
            .contains("{ x : 'a | _ }")
    );
    assert!(
        assert_canonical(
            "module M\npublic setX : { x : 'a | 'r } -> { x : 'a | 'r }\nlet setX r = r"
        )
        .contains("{ x : 'a | 'r }")
    );
}

#[test]
fn union_type_declaration_formats() {
    let out = assert_canonical("module M\ntype Shape =\n  | Circle Float\n  | Rect Float Float");
    assert!(out.contains("type Shape =\n  | Circle Float\n  | Rect Float Float"), "out:\n{out}");
}

#[test]
fn alias_and_record_type_declarations_format() {
    assert!(assert_canonical("module M\ntype Celsius = Int").contains("type Celsius = Int"));
    let out = assert_canonical("module M\ntype Vec2 = { x : Float, y : Float }");
    assert!(out.contains("type Vec2 = { x : Float, y : Float }"), "out:\n{out}");
}

#[test]
fn match_expression_formats_and_round_trips() {
    let src = "module M\npublic describe : Option Int -> String\nlet describe o =\n  match o with\n  | None -> \"none\"\n  | Some n -> Int.toString n";
    let out = assert_canonical(src);
    assert!(out.contains("match o with"), "out:\n{out}");
    assert!(out.contains("| None -> \"none\""), "out:\n{out}");
    assert!(out.contains("| Some n -> Int.toString n"), "out:\n{out}");
}

#[test]
fn snapshot_union_and_match() {
    let src = "module Shapes\ntype Shape =\n  | Circle Float\n  | Rect Float Float\npublic area : Shape -> Float\nlet area s =\n  match s with\n  | Circle r -> 3.14 * r * r\n  | Rect w h -> w * h";
    insta::assert_snapshot!("union_and_match", fmt(src));
    assert_idempotent(src);
}

#[test]
fn snapshot_records() {
    let src = "module Geo\ntype Vec2 = { x : Float, y : Float }\npublic scale : Float -> Vec2 -> Vec2\nlet scale k v = { v with x = v.x * k, y = v.y * k }";
    insta::assert_snapshot!("records", fmt(src));
    assert_idempotent(src);
}

fn arb_ident() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9]*".prop_filter("reserved keyword", |s| TokenKind::keyword(s).is_none())
}

fn arb_atom() -> impl Strategy<Value = String> {
    prop_oneof![arb_ident(), any::<u32>().prop_map(|n| n.to_string())]
}

const OPS: &[&str] =
    &["+", "-", "*", "/", "%", "++", "::", "|>", ">>", "&&", "||", "=", "<>", "<", "<=", ">", ">="];

/// Fully self-delimiting expressions (see the parser's generator): every value
/// is a single atom, so any composition parses cleanly.
fn arb_expr() -> impl Strategy<Value = String> {
    let leaf = prop_oneof![
        arb_ident(),
        any::<u32>().prop_map(|n| n.to_string()),
        Just("()".to_owned()),
        "[a-z ]*".prop_map(|s| format!("\"{s}\"")),
    ];
    leaf.prop_recursive(4, 48, 3, |inner| {
        let op = proptest::sample::select(OPS.to_vec());
        prop_oneof![
            (inner.clone(), inner.clone()).prop_map(|(f, a)| format!("({f} {a})")),
            (inner.clone(), op, inner.clone()).prop_map(|(a, o, b)| format!("({a} {o} {b})")),
            (inner.clone(), inner.clone(), inner.clone())
                .prop_map(|(c, t, e)| format!("(if {c} then {t} else {e})")),
            inner.clone().prop_map(|e| format!("({e})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("({a}, {b})")),
            (inner.clone(), inner.clone()).prop_map(|(a, b)| format!("[{a}, {b}]")),
            inner.clone().prop_map(|e| format!("(fun x -> {e})")),
            inner.prop_map(|e| format!("(-{e})")),
        ]
    })
}

fn arb_program() -> impl Strategy<Value = String> {
    proptest::collection::vec((arb_ident(), arb_expr()), 1..5).prop_map(|binds| {
        let mut src = "module M".to_owned();
        for (name, body) in binds {
            src.push_str(&format!("\nlet {name} = {body}"));
        }
        src
    })
}

proptest! {
    #![proptest_config(regression_config())]

    /// Formatting preserves a program's structure: the span-free shape of the
    /// tree is identical before and after a format round-trip, the output
    /// reparses cleanly, and formatting is idempotent.
    #[test]
    fn fmt_preserves_structure(src in arb_program()) {
        let before = parse_module(SourceId::new(0), &src);
        prop_assume!(before.diagnostics.is_empty());
        let once = fai_fmt::format(&before.module, &before.comments, &src);
        let after = parse_module(SourceId::new(0), &once);
        prop_assert!(after.diagnostics.is_empty(), "output did not reparse:\n{}", once);
        prop_assert_eq!(
            shape(&before.module),
            shape(&after.module),
            "fmt changed structure:\nsrc:\n{}\nout:\n{}", src, once,
        );
        let twice = fai_fmt::format(&after.module, &after.comments, &once);
        prop_assert_eq!(&twice, &once, "fmt is not idempotent:\n{}", once);
    }

    /// Unparenthesized operator chains keep their parse (precedence and
    /// associativity) across a format round-trip: the formatter emits exactly
    /// the parentheses the tree carries — no more, no fewer.
    #[test]
    fn operator_chains_round_trip(
        atoms in proptest::collection::vec(arb_atom(), 2..6),
        ops in proptest::collection::vec(proptest::sample::select(OPS.to_vec()), 1..6),
    ) {
        let n = atoms.len().min(ops.len() + 1);
        let mut expr = atoms[0].clone();
        for i in 1..n {
            expr.push_str(&format!(" {} {}", ops[i - 1], atoms[i]));
        }
        let src = format!("module M\nlet it = {expr}");
        let before = parse_module(SourceId::new(0), &src);
        prop_assume!(before.diagnostics.is_empty());
        let once = fai_fmt::format(&before.module, &before.comments, &src);
        let after = parse_module(SourceId::new(0), &once);
        prop_assert!(after.diagnostics.is_empty(), "output did not reparse:\n{}", once);
        prop_assert_eq!(shape(&before.module), shape(&after.module), "src: {}\nout: {}", src, once);
    }

    /// A record literal of distinct labels survives a format round-trip: it
    /// reparses cleanly, the span-free shape is preserved (the formatter sorts
    /// nothing and drops nothing), and formatting is idempotent.
    #[test]
    fn record_literals_round_trip(
        labels in proptest::collection::hash_set("[a-z][a-z0-9]{0,3}", 1..6),
    ) {
        prop_assume!(labels.iter().all(|l| TokenKind::keyword(l).is_none()));
        let fields = labels
            .iter()
            .enumerate()
            .map(|(i, l)| format!("{l} = {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!("module M\nlet r = {{ {fields} }}");
        let before = parse_module(SourceId::new(0), &src);
        prop_assume!(before.diagnostics.is_empty());
        let once = fai_fmt::format(&before.module, &before.comments, &src);
        let after = parse_module(SourceId::new(0), &once);
        prop_assert!(after.diagnostics.is_empty(), "output did not reparse:\n{}", once);
        prop_assert_eq!(shape(&before.module), shape(&after.module), "src: {}\nout: {}", src, once);
        let twice = fai_fmt::format(&after.module, &after.comments, &once);
        prop_assert_eq!(&twice, &once, "fmt is not idempotent:\n{}", once);
    }

    /// A union declaration of any width and a `match` covering its constructors
    /// round-trip through the formatter with their structure intact.
    #[test]
    fn union_and_match_round_trip(n in 1usize..6) {
        let variants = (0..n).map(|i| format!("  | C{i} Int")).collect::<Vec<_>>().join("\n");
        let arms = (0..n).map(|i| format!("  | C{i} x -> x + {i}")).collect::<Vec<_>>().join("\n");
        let src = format!(
            "module M\ntype T =\n{variants}\npublic eval : T -> Int\nlet eval t =\n  match t with\n{arms}"
        );
        let before = parse_module(SourceId::new(0), &src);
        prop_assume!(before.diagnostics.is_empty());
        let once = fai_fmt::format(&before.module, &before.comments, &src);
        let after = parse_module(SourceId::new(0), &once);
        prop_assert!(after.diagnostics.is_empty(), "output did not reparse:\n{}", once);
        prop_assert_eq!(shape(&before.module), shape(&after.module), "src:\n{}\nout:\n{}", src, once);
        let twice = fai_fmt::format(&after.module, &after.comments, &once);
        prop_assert_eq!(&twice, &once, "fmt is not idempotent:\n{}", once);
    }
}
