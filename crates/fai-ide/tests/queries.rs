//! JSON snapshot tests for the `fai query` commands over a small workspace.

use fai_db::{Db, DbSpanResolver, FaiDatabase, SourceFile};
use fai_ide::{
    ListOpts, api, callees, callers, def, definition_at, dependents, docs, document_symbols,
    hover_at, outline, prepare_rename_at, references_at, refs, rename_at, search,
    signature_help_at, symbols, type_at, workspace_symbols,
};
use indoc::indoc;

fn workspace() -> (FaiDatabase, Vec<SourceFile>) {
    let mut db = FaiDatabase::new();
    let a = db.add_source(
        "A.fai".into(),
        indoc! {r#"
            module A

            public inc : Int -> Int
            let inc x = x + 1

            public twice : ('a -> 'a) -> 'a -> 'a
            let twice f = f >> f
        "#}
        .to_owned(),
    );
    let b = db.add_source(
        "B.fai".into(),
        indoc! {r#"
            module B

            public two : Int
            let two = A.inc 1

            let four = A.inc (A.inc two)
        "#}
        .to_owned(),
    );
    let files = vec![db.source_file(a).unwrap(), db.source_file(b).unwrap()];
    (db, files)
}

fn json<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

#[test]
fn symbols_snapshot() {
    let (db, files) = workspace();
    let r = symbols(&db, &files, None, &DbSpanResolver::new(&db), ListOpts::default());
    insta::assert_snapshot!("symbols", json(&r));
}

#[test]
fn def_snapshot() {
    let (db, _files) = workspace();
    let r = def(&db, "A.inc", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("def_A_inc", json(&r));
}

#[test]
fn type_snapshot() {
    let (db, _files) = workspace();
    let r = type_at(&db, "A.twice", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("type_A_twice", json(&r));
}

#[test]
fn refs_snapshot() {
    let (db, files) = workspace();
    let r = refs(&db, &files, "A.inc", &DbSpanResolver::new(&db), ListOpts::default());
    insta::assert_snapshot!("refs_A_inc", json(&r));
}

#[test]
fn dependents_snapshot() {
    let (db, files) = workspace();
    let r = dependents(&db, &files, "A.inc", &DbSpanResolver::new(&db), false, ListOpts::default());
    insta::assert_snapshot!("dependents_A_inc", json(&r));
}

#[test]
fn dependents_transitive_follows_the_chain() {
    let mut db = FaiDatabase::new();
    let c = db.add_source(
        "C.fai".into(),
        indoc! {r#"
            module C

            public base : Int
            let base = 0

            let mid = base + 1

            let top = mid + 1
        "#}
        .to_owned(),
    );
    let files = vec![db.source_file(c).unwrap()];
    let names = |transitive: bool| -> Vec<String> {
        let r = dependents(
            &db,
            &files,
            "C.base",
            &DbSpanResolver::new(&db),
            transitive,
            ListOpts::default(),
        );
        r.dependents.iter().map(|s| s.name.clone()).collect()
    };
    // Direct: only `mid` references `base`.
    let direct = names(false);
    assert!(direct.contains(&"mid".to_owned()), "{direct:?}");
    assert!(!direct.contains(&"top".to_owned()), "{direct:?}");
    // Transitive: `top` reaches `base` through `mid`.
    let all = names(true);
    assert!(all.contains(&"mid".to_owned()) && all.contains(&"top".to_owned()), "{all:?}");
}

#[test]
fn callers_of_inc() {
    let (db, files) = workspace();
    let r = callers(&db, &files, "A.inc", &DbSpanResolver::new(&db));
    // Both `B.two` and `B.four` reference `A.inc` (edges sorted by path).
    let names: Vec<&str> = r.edges.iter().map(|e| e.symbol.name.as_str()).collect();
    assert_eq!(names, vec!["four", "two"], "{names:?}");
    let four = r.edges.iter().find(|e| e.symbol.name == "four").unwrap();
    assert_eq!(four.sites.len(), 2, "B.four calls A.inc twice");
}

#[test]
fn callees_of_four() {
    let (db, _files) = workspace();
    let r = callees(&db, "B.four", &DbSpanResolver::new(&db));
    let names: Vec<&str> = r.edges.iter().map(|e| e.symbol.name.as_str()).collect();
    assert!(names.contains(&"inc") && names.contains(&"two"), "{names:?}");
    let inc = r.edges.iter().find(|e| e.symbol.name == "inc").unwrap();
    assert_eq!(inc.sites.len(), 2, "B.four references A.inc twice");
}

#[test]
fn callers_snapshot() {
    let (db, files) = workspace();
    let r = callers(&db, &files, "A.inc", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("callers_A_inc", json(&r));
}

#[test]
fn callees_snapshot() {
    let (db, _files) = workspace();
    let r = callees(&db, "B.four", &DbSpanResolver::new(&db));
    insta::assert_snapshot!("callees_B_four", json(&r));
}

#[test]
fn search_ranks_exact_above_hole() {
    let mut db = FaiDatabase::new();
    let m = db.add_source(
        "S.fai".into(),
        indoc! {r#"
            module S

            public len : List 'a -> Int
            let len xs = 0

            public sumInts : List Int -> Int
            let sumInts xs = 0

            public other : Int -> Int
            let other x = x
        "#}
        .to_owned(),
    );
    let files = vec![db.source_file(m).unwrap()];
    let r = search(&db, &files, "List 'a -> Int", &DbSpanResolver::new(&db), ListOpts::default());
    let names: Vec<&str> = r.results.iter().map(|h| h.symbol.name.as_str()).collect();
    // `len` matches exactly (var<->var); `sumInts` matches with the hole bound to
    // `Int`; `other` does not match at all.
    assert_eq!(names.first(), Some(&"len"), "exact match ranks first: {names:?}");
    assert!(names.contains(&"sumInts"), "{names:?}");
    assert!(!names.contains(&"other"), "{names:?}");
    let len_hit = r.results.iter().find(|h| h.symbol.name == "len").unwrap();
    let sum_hit = r.results.iter().find(|h| h.symbol.name == "sumInts").unwrap();
    assert!(len_hit.score > sum_hit.score, "exact score should exceed hole score");
}

#[test]
fn outline_snapshot() {
    let (db, files) = workspace();
    let r = outline(&db, "A", &files, &DbSpanResolver::new(&db));
    insta::assert_snapshot!("outline_A", json(&r));
}

#[test]
fn api_snapshot() {
    let (db, files) = workspace();
    let r = api(&db, "A", &files, &DbSpanResolver::new(&db));
    insta::assert_snapshot!("api_A", json(&r));
}

#[test]
fn type_under_error_is_best_effort() {
    // A workspace with a type error still answers a `type` query for a good def.
    let mut db = FaiDatabase::new();
    let id = db.add_source(
        "M.fai".into(),
        indoc! {r#"
            module M

            public good : Int -> Int
            let good x = x + 1

            public bad : Int -> Bool
            let bad x = x + 1
        "#}
        .to_owned(),
    );
    let _ = db.source_file(id).unwrap();
    let r = type_at(&db, "M.good", &DbSpanResolver::new(&db));
    assert_eq!(r.ty.display, "Int -> Int");
}

// --- position-based queries (hover / go-to-definition) -----------------------

const POSITION_SOURCE: &str = indoc! {r#"
    module P

    type Color =
      | Red
      | Green

    public paint : Color -> Int
    let paint c =
      let shade = tag c
      shade + tag Red

    public tag : Color -> Int
    let tag c =
      match c with
      | Red -> 0
      | Green -> 1
"#};

/// A one-file workspace (`P.fai`) with locals, a constructor, and a self-call,
/// plus its source text (so tests can locate offsets and slice spans).
fn position_workspace() -> (FaiDatabase, SourceFile, &'static str) {
    let mut db = FaiDatabase::new();
    let id = db.add_source("P.fai".into(), POSITION_SOURCE.to_owned());
    let file = db.source_file(id).unwrap();
    (db, file, POSITION_SOURCE)
}

/// The byte offset of `needle` in `text` (panics if absent).
fn at(text: &str, needle: &str) -> u32 {
    text.find(needle).unwrap_or_else(|| panic!("`{needle}` not found")) as u32
}

#[test]
fn definition_at_jumps_to_a_local_binding() {
    let (db, file, text) = position_workspace();
    // The `shade` in the tail `shade + tag Red` is a use of the local binding.
    let offset = at(text, "shade + tag Red");
    let r = definition_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert!(r.target.is_none(), "a local has no addressable symbol");
    let span = &r.definitions[0].span;
    // It resolves to the binding occurrence `let shade = …`, not the use.
    assert_eq!(span.byte_start, at(text, "shade = tag c"));
    assert_eq!(&text[span.byte_start as usize..span.byte_end as usize], "shade");
}

#[test]
fn definition_at_jumps_to_a_constructor() {
    let (db, file, text) = position_workspace();
    // The `Red` in `tag Red` is a constructor reference.
    let offset = at(text, "tag Red") + "tag ".len() as u32;
    let r = definition_at(&db, file, offset, &DbSpanResolver::new(&db));
    let span = &r.definitions[0].span;
    // It resolves to the variant declaration `| Red`.
    assert_eq!(span.byte_start, at(text, "| Red\n") + "| ".len() as u32);
    assert_eq!(&text[span.byte_start as usize..span.byte_end as usize], "Red");
}

#[test]
fn definition_at_jumps_across_modules_via_outward_walk() {
    let (db, files) = workspace();
    // Point at the module segment `A` in `A.inc`; the bare `Var(A)` has no
    // resolution, so the query walks outward to the qualified reference.
    let b = files[1];
    let text = b.text(&db).clone();
    let offset = text.find("A.inc").unwrap() as u32;
    let r = definition_at(&db, b, offset, &DbSpanResolver::new(&db));
    assert_eq!(r.target.as_ref().unwrap().name, "inc");
    assert_eq!(r.definitions[0].span.file, "A.fai");
}

#[test]
fn definition_at_off_a_reference_is_empty() {
    let (db, file, text) = position_workspace();
    // The module header has no referencing expression.
    let offset = at(text, "module P");
    let r = definition_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert!(r.target.is_none() && r.definitions.is_empty());
}

#[test]
fn hover_at_reports_a_local_type() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "shade + tag Red");
    let r = hover_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert_eq!(r.name.as_deref(), Some("shade"));
    assert_eq!(r.ty.unwrap().display, "Int");
}

#[test]
fn hover_at_reports_a_function_reference_type() {
    let (db, file, text) = position_workspace();
    // The `tag` in the tail resolves to the top-level function.
    let offset = at(text, "tag Red");
    let r = hover_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert_eq!(r.name.as_deref(), Some("tag"));
    assert_eq!(r.ty.unwrap().display, "Color -> Int");
}

#[test]
fn hover_at_off_an_expression_is_empty() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "module P");
    let r = hover_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert!(r.ty.is_none() && r.name.is_none());
}

#[test]
fn hover_at_reports_a_char_literal_type() {
    let mut db = FaiDatabase::new();
    let src = "module M\n\nlet first = 'a'\n";
    let id = db.add_source("M.fai".into(), src.to_owned());
    let file = db.source_file(id).unwrap();
    let offset = src.find("'a'").unwrap() as u32;
    let r = hover_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert_eq!(r.ty.unwrap().display, "Char");
}

#[test]
fn definition_at_snapshot() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "tag Red") + "tag ".len() as u32;
    let r = definition_at(&db, file, offset, &DbSpanResolver::new(&db));
    insta::assert_snapshot!("definition_at_Red", json(&r));
}

#[test]
fn hover_at_snapshot() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "tag Red");
    let r = hover_at(&db, file, offset, &DbSpanResolver::new(&db));
    insta::assert_snapshot!("hover_at_tag", json(&r));
}

// --- find references at a position --------------------------------------------

#[test]
fn references_at_a_definition_spans_all_modules() {
    let (db, files) = workspace();
    let b = files[1];
    let b_text = b.text(&db).clone();
    // Point inside a use of `A.inc` in B; with the declaration included, the
    // result is every use across the workspace plus both declaration names in A
    // (the signature and the binding).
    let offset = at(&b_text, "A.inc") + "A.".len() as u32;
    let refs = references_at(&db, &files, b, offset, &DbSpanResolver::new(&db), true);
    assert_eq!(refs.len(), 5, "3 uses in B + signature and binding names in A: {refs:?}");
    let in_a = refs.iter().filter(|l| l.span.file == "A.fai").count();
    let in_b = refs.iter().filter(|l| l.span.file == "B.fai").count();
    assert_eq!((in_a, in_b), (2, 3), "{refs:?}");
    // Every reported reference is the name `inc`.
    let a_text = files[0].text(&db).clone();
    for loc in &refs {
        let text = if loc.span.file == "A.fai" { &a_text } else { &b_text };
        assert_eq!(&text[loc.span.byte_start as usize..loc.span.byte_end as usize], "inc");
    }
}

#[test]
fn references_at_a_definition_match_those_from_a_use() {
    let (db, files) = workspace();
    let a = files[0];
    let a_text = a.text(&db).clone();
    let b = files[1];
    let b_text = b.text(&db).clone();
    // From the binding's name `let inc x` …
    let from_def =
        references_at(&db, &files, a, at(&a_text, "inc x"), &DbSpanResolver::new(&db), true);
    // … and from a use `A.inc` — the same complete set.
    let from_use = references_at(
        &db,
        &files,
        b,
        at(&b_text, "A.inc") + "A.".len() as u32,
        &DbSpanResolver::new(&db),
        true,
    );
    assert_eq!(from_def, from_use, "references are independent of where they are requested");
}

#[test]
fn references_at_can_exclude_the_declaration() {
    let (db, files) = workspace();
    let b = files[1];
    let b_text = b.text(&db).clone();
    let offset = at(&b_text, "A.inc") + "A.".len() as u32;
    let refs = references_at(&db, &files, b, offset, &DbSpanResolver::new(&db), false);
    assert_eq!(refs.len(), 3, "only the three uses, no declaration: {refs:?}");
    assert!(refs.iter().all(|l| l.span.file == "B.fai"), "{refs:?}");
}

#[test]
fn references_at_a_local_stay_within_the_body() {
    let (db, file, text) = position_workspace();
    // `shade` is bound by `let shade = tag c` and used in `shade + tag Red`.
    let offset = at(text, "shade + tag Red");
    let with_decl = references_at(&db, &[file], file, offset, &DbSpanResolver::new(&db), true);
    assert_eq!(with_decl.len(), 2, "the binding plus one use: {with_decl:?}");
    let without_decl = references_at(&db, &[file], file, offset, &DbSpanResolver::new(&db), false);
    assert_eq!(without_decl.len(), 1, "just the use: {without_decl:?}");
}

#[test]
fn references_at_a_constructor_cover_uses_and_patterns() {
    let (db, file, text) = position_workspace();
    // `Red` is declared as a variant, used as an expression (`tag Red`), and
    // matched as a pattern (`| Red -> 0`).
    let offset = at(text, "tag Red") + "tag ".len() as u32;
    let refs = references_at(&db, &[file], file, offset, &DbSpanResolver::new(&db), true);
    assert_eq!(refs.len(), 3, "declaration + expression use + pattern use: {refs:?}");
    for loc in &refs {
        assert_eq!(&text[loc.span.byte_start as usize..loc.span.byte_end as usize], "Red");
    }
}

#[test]
fn references_off_a_symbol_is_empty() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "module P");
    let refs = references_at(&db, &[file], file, offset, &DbSpanResolver::new(&db), true);
    assert!(refs.is_empty(), "{refs:?}");
}

#[test]
fn references_at_snapshot() {
    let (db, files) = workspace();
    let b = files[1];
    let offset = at(&b.text(&db).clone(), "A.inc") + "A.".len() as u32;
    let refs = references_at(&db, &files, b, offset, &DbSpanResolver::new(&db), true);
    insta::assert_snapshot!("references_at_A_inc", json(&refs));
}

// --- document & workspace symbols --------------------------------------------

#[test]
fn document_symbols_mirror_the_outline() {
    // `fai query outline` is keyed by module name; `documentSymbol` by file. They
    // must agree (the former delegates to the latter).
    let (db, files) = workspace();
    let by_file = document_symbols(&db, files[0], &DbSpanResolver::new(&db));
    let by_name = outline(&db, "A", &files, &DbSpanResolver::new(&db));
    assert_eq!(json(&by_file), json(&by_name));
}

#[test]
fn document_symbols_snapshot() {
    let (db, file, _text) = position_workspace();
    let r = document_symbols(&db, file, &DbSpanResolver::new(&db));
    insta::assert_snapshot!("document_symbols_P", json(&r));
}

#[test]
fn workspace_symbols_filter_by_substring() {
    let (db, files) = workspace();
    let names = |q: &str| -> Vec<String> {
        workspace_symbols(&db, &files, q, &DbSpanResolver::new(&db), ListOpts::default())
            .symbols
            .into_iter()
            .map(|s| s.name)
            .collect()
    };
    // An empty query lists every definition, sorted by path (A.* before B.*).
    assert_eq!(names(""), vec!["inc", "twice", "four", "two"]);
    // A substring filters case-insensitively.
    assert_eq!(names("INC"), vec!["inc"]);
    // `t` matches `twice` and `two` (sorted by path: A.twice before B.two).
    assert_eq!(names("t"), vec!["twice", "two"]);
}

#[test]
fn workspace_symbols_snapshot() {
    let (db, files) = workspace();
    let r = workspace_symbols(&db, &files, "two", &DbSpanResolver::new(&db), ListOpts::default());
    insta::assert_snapshot!("workspace_symbols_two", json(&r));
}

// --- rename ------------------------------------------------------------------

#[test]
fn prepare_rename_reports_the_name_under_the_cursor() {
    let (db, files) = workspace();
    let b = files[1];
    let b_text = b.text(&db).clone();
    // On the `inc` of a qualified use `A.inc`, the rename range is just `inc`.
    let offset = at(&b_text, "A.inc") + "A.".len() as u32;
    let target = prepare_rename_at(&db, b, offset, &DbSpanResolver::new(&db)).expect("renameable");
    assert_eq!(target.name, "inc");
    let s = &b_text[target.span.byte_start as usize..target.span.byte_end as usize];
    assert_eq!(s, "inc", "the rename range covers only the member, not `A.inc`");
}

#[test]
fn prepare_rename_rejects_builtins_and_empty_positions() {
    let (db, files) = workspace();
    let a = files[0];
    let a_text = a.text(&db).clone();
    // The `+` operator is a standard-library method, not user code.
    let plus = at(&a_text, "+ 1");
    assert!(prepare_rename_at(&db, a, plus, &DbSpanResolver::new(&db)).is_none(), "no rename on +");
    // The module header is not a symbol.
    let header = at(&a_text, "module A");
    assert!(prepare_rename_at(&db, a, header, &DbSpanResolver::new(&db)).is_none());
}

#[test]
fn rename_local_rewrites_binding_and_uses() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "shade + tag Red");
    let edits = rename_at(&db, &[file], file, offset, "lvl", &DbSpanResolver::new(&db))
        .expect("a local is renameable");
    assert_eq!(edits.len(), 2, "the binding plus one use: {edits:?}");
    for loc in &edits {
        assert_eq!(&text[loc.span.byte_start as usize..loc.span.byte_end as usize], "shade");
    }
}

#[test]
fn rename_definition_rewrites_every_module() {
    let (db, files) = workspace();
    let b = files[1];
    let a_text = files[0].text(&db).clone();
    let b_text = b.text(&db).clone();
    let offset = at(&b_text, "A.inc") + "A.".len() as u32;
    let edits = rename_at(&db, &files, b, offset, "increment", &DbSpanResolver::new(&db))
        .expect("a user definition is renameable");
    // Both declaration names in A (signature + binding) plus three uses in B;
    // each edit targets the bare name, so applying them keeps the program valid.
    assert_eq!(edits.len(), 5, "{edits:?}");
    for loc in &edits {
        let text = if loc.span.file == "A.fai" { &a_text } else { &b_text };
        assert_eq!(&text[loc.span.byte_start as usize..loc.span.byte_end as usize], "inc");
    }
}

#[test]
fn rename_rejects_a_cross_namespace_or_malformed_name() {
    let (db, files) = workspace();
    let b = files[1];
    let offset = at(&b.text(&db).clone(), "A.inc") + "A.".len() as u32;
    // A value cannot become an upper-case (constructor) name…
    assert!(rename_at(&db, &files, b, offset, "Inc", &DbSpanResolver::new(&db)).is_none());
    // …nor a non-identifier.
    assert!(rename_at(&db, &files, b, offset, "in c", &DbSpanResolver::new(&db)).is_none());
    assert!(rename_at(&db, &files, b, offset, "", &DbSpanResolver::new(&db)).is_none());
}

#[test]
fn rename_constructor_requires_an_upper_name() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "tag Red") + "tag ".len() as u32;
    // A constructor keeps the upper-case namespace.
    let ok = rename_at(&db, &[file], file, offset, "Crimson", &DbSpanResolver::new(&db));
    assert_eq!(ok.expect("upper name is valid").len(), 3, "declaration + expr use + pattern use");
    assert!(
        rename_at(&db, &[file], file, offset, "crimson", &DbSpanResolver::new(&db)).is_none(),
        "a lower-case constructor name is rejected"
    );
}

#[test]
fn rename_rejects_standard_library_symbols() {
    // A qualified use of a std function resolves to a definition in the embedded
    // standard library, which is read-only.
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "U.fai".into(),
        indoc! {r#"
            module U

            public ids : List Int -> List Int
            let ids xs = List.map (fun x -> x) xs
        "#}
        .to_owned(),
    );
    let file = db.source_file(id).unwrap();
    let text = file.text(&db).clone();
    let offset = at(&text, "List.map") + "List.".len() as u32;
    assert!(
        prepare_rename_at(&db, file, offset, &DbSpanResolver::new(&db)).is_none(),
        "cannot rename a standard-library symbol"
    );
    let files = vec![file];
    assert!(rename_at(&db, &files, file, offset, "transform", &DbSpanResolver::new(&db)).is_none());
}

// --- doc extraction, richer hover, signature help ----------------------------

/// A one-file workspace with the given source, plus its text.
fn doc_workspace(source: &str) -> (FaiDatabase, SourceFile, String) {
    let mut db = FaiDatabase::new();
    let id = db.add_source("D.fai".into(), source.to_owned());
    let file = db.source_file(id).unwrap();
    let text = file.text(&db).clone();
    (db, file, text)
}

#[test]
fn docs_query_extracts_doc_and_contracts() {
    let (db, _file, _text) = doc_workspace(indoc! {r#"
        module D

        /// Increment by one.
        public inc : Int -> Int
        let inc x = x + 1
        example: inc 1 = 2
    "#});
    let r = docs(&db, "D.inc", &DbSpanResolver::new(&db));
    assert_eq!(r.doc.expect("a doc").markdown, "Increment by one.");
    assert_eq!(r.contracts.len(), 1, "the example is attached");
}

#[test]
fn docs_query_joins_multiline_doc() {
    let (db, _file, _text) = doc_workspace(indoc! {r#"
        module D

        /// First line.
        /// Second line.
        public inc : Int -> Int
        let inc x = x + 1
    "#});
    let r = docs(&db, "D.inc", &DbSpanResolver::new(&db));
    assert_eq!(r.doc.expect("a doc").markdown, "First line.\nSecond line.");
}

#[test]
fn private_binding_doc_on_the_binding_is_found() {
    // A signature-less private binding carries its doc on the binding itself.
    let (db, _file, _text) = doc_workspace(indoc! {r#"
        module D

        /// A local helper.
        let helper x = x
    "#});
    let r = docs(&db, "D.helper", &DbSpanResolver::new(&db));
    assert_eq!(r.doc.expect("a doc").markdown, "A local helper.");
}

#[test]
fn hover_includes_doc_and_contracts_of_the_referenced_definition() {
    let (db, file, text) = doc_workspace(indoc! {r#"
        module D

        /// Increment by one.
        public inc : Int -> Int
        let inc x = x + 1
        example: inc 1 = 2

        public two : Int
        let two = inc 7
    "#});
    // Hover the use `inc` in `let two = inc 7`.
    let offset = at(&text, "inc 7");
    let r = hover_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert_eq!(r.name.as_deref(), Some("inc"));
    assert_eq!(r.ty.unwrap().display, "Int -> Int");
    assert_eq!(r.doc.expect("doc").markdown, "Increment by one.");
    assert_eq!(r.contracts.len(), 1, "the definition's example travels with the hover");
}

#[test]
fn hover_on_a_local_has_no_doc() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "shade + tag Red");
    let r = hover_at(&db, file, offset, &DbSpanResolver::new(&db));
    assert_eq!(r.name.as_deref(), Some("shade"));
    assert!(r.doc.is_none() && r.contracts.is_empty(), "a local carries no doc/contracts");
}

#[test]
fn signature_help_reports_the_active_parameter() {
    let (db, file, text) = doc_workspace(indoc! {r#"
        module D

        public add : Int -> Int -> Int
        let add x y = x + y

        public apply : Int
        let apply = add 1 2
    "#});
    // Cursor on the first argument: parameter 0.
    let p0 = at(&text, "add 1 2") + "add ".len() as u32;
    let s0 = signature_help_at(&db, file, p0).expect("signature help");
    assert_eq!(s0.label, "add : Int -> Int -> Int");
    assert_eq!(s0.parameters.len(), 2);
    assert_eq!(s0.active_parameter, 0);
    // The parameter slices index back into the label.
    let p = &s0.parameters[0];
    assert_eq!(&s0.label[p.start as usize..p.end as usize], "Int");
    // Cursor on the second argument: parameter 1.
    let p1 = at(&text, "add 1 2") + "add 1 ".len() as u32;
    let s1 = signature_help_at(&db, file, p1).expect("signature help");
    assert_eq!(s1.active_parameter, 1);
}

#[test]
fn signature_help_before_the_first_argument() {
    // A function name followed by a space, no argument typed yet (the trailing
    // newline keeps the space after `add`).
    let (db, file, text) = doc_workspace(
        "module D\n\npublic add : Int -> Int -> Int\nlet add x y = x + y\n\npublic p : Int -> Int -> Int\nlet p = add \n",
    );
    let offset = at(&text, "= add ") + "= add ".len() as u32;
    let s = signature_help_at(&db, file, offset).expect("signature help");
    assert_eq!(s.label, "add : Int -> Int -> Int");
    assert_eq!(s.active_parameter, 0);
}

#[test]
fn signature_help_off_a_call_is_none() {
    let (db, file, text) = position_workspace();
    let offset = at(text, "module P");
    assert!(signature_help_at(&db, file, offset).is_none());
}
