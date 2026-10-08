//! Forcing-effect changes are reflected in incremental diagnostics and public callers.

use fai_db::{Db, Diag};
use fai_syntax::Symbol;

#[test]
fn private_initializer_effect_edits_match_clean_inference() {
    let pure = "module Main\nlet value = 42\npublic read : Unit -> Int\nlet read u = value\n";
    let effectful = "module Main\nlet value =\n  let ignored = stdConsole.writeLine \"forced\"\n  42\npublic read : Unit -> Int\nlet read u = value\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[&[("Main.fai", pure)], &[("Main.fai", effectful)], &[("Main.fai", pure)]],
        |db, files| {
            let file = db.source_file(files[0]).unwrap();
            let mut codes: Vec<_> = fai_types::check_file::accumulated::<Diag>(db, file)
                .into_iter()
                .map(|d| d.0.code.as_str().to_owned())
                .collect();
            codes.sort();
            (fai_types::def_effect(db, file, Symbol::intern("read")), codes)
        },
    );
}

#[test]
fn cross_file_callers_see_private_forcing_effects_through_the_public_arrow() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    db.add_source("Library.fai".into(), "module Library\nlet value =\n  let ignored = stdConsole.writeLine \"forced\"\n  42\npublic read : Unit -> Int / { Console }\nlet read u = value\n".into());
    let id = db.add_source(
        "Main.fai".into(),
        "module Main\npublic read : Unit -> Int / { Console }\nlet read u = Library.read ()\n"
            .into(),
    );
    let file = db.source_file(id).unwrap();
    assert!(fai_types::check_file::accumulated::<Diag>(&db, file).is_empty());
    assert_eq!(fai_types::def_effect(&db, file, Symbol::intern("read")).labels.len(), 1);
}
