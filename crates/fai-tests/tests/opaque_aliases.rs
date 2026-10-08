//! Abstract signatures remain stable while representation-aware native queries update.

use fai_db::{Db, Diag};
use fai_syntax::Symbol;

#[test]
fn representation_edits_match_clean_abstract_types_and_native_abis() {
    let integer = "module Lib\npublic opaque type Secret = Int\n";
    let float = "module Lib\npublic opaque type Secret = Float\n";
    let point = "module Lib\npublic opaque type Secret = { x : Float, y : Float }\n";
    let main =
        "module Main\npublic bounce : Lib.Secret -> Lib.Secret\nlet bounce secret = secret\n";
    fai_tests::assert_incremental_with_std_matches_clean(
        &[
            &[("Lib.fai", integer), ("Main.fai", main)],
            &[("Lib.fai", float), ("Main.fai", main)],
            &[("Lib.fai", point), ("Main.fai", main)],
        ],
        |db, ids| {
            let file = db.source_file(ids[1]).unwrap();
            let name = Symbol::intern("bounce");
            let errors: Vec<_> = fai_types::check_file::accumulated::<Diag>(db, file)
                .into_iter()
                .map(|d| d.0.code.as_str().to_owned())
                .collect();
            (
                fai_types::render_scheme(&fai_types::def_type(db, file, name)),
                format!("{:?}", fai_core::abi::abi(db, file, name)),
                errors,
            )
        },
    );
}
