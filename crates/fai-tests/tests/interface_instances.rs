//! Qualified interface construction and lexical lookup survive native lowering and edits.

use fai_db::{Db, Diag};
use fai_syntax::Symbol;

#[test]
fn qualified_interface_methods_dispatch_in_the_jit() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source(
        "Library.fai".into(),
        "module Library\nmodule Deep =\n  public interface Service = run : Int -> Int\n".into(),
    );
    let source = include_str!("../../../samples/InterfaceScopes.fai")
        .replace("{ Inner.Service with", "{ Library.Deep.Service with");
    let id = db.add_source("Main.fai".into(), source);
    fai_runtime::capture_start();
    let result = fai_driver::jit_run_program(&db, db.source_file(id).unwrap());
    let output = fai_runtime::capture_take();
    assert_eq!(result.exit_code, 0, "{:?}", result.diagnostics);
    assert_eq!(output, "42\n");
}

#[test]
fn lexical_interface_edits_match_clean_resolution_and_inference() {
    let before = "module M\ninterface I = outer : Int -> Int\nmodule Inner =\n  interface I = run : Int -> Int\n  let value = { I with run x = x }\n";
    let after = before.replace("  interface I = run : Int -> Int\n", "");
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("M.fai", before)], &[("M.fai", &after)], &[("M.fai", before)]],
        |db, ids| {
            let file = db.source_file(ids[0]).unwrap();
            let codes: Vec<_> = fai_types::check_file::accumulated::<Diag>(db, file)
                .into_iter()
                .map(|d| d.0.code.as_str().to_owned())
                .collect();
            (
                codes,
                fai_types::render_scheme(&fai_types::def_type(
                    db,
                    file,
                    Symbol::intern("Inner.value"),
                )),
            )
        },
    );
}
