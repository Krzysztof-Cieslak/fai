//! The largest admitted expression tree must fit the front end's thread stack.

use fai_db::{Db, Diag, FaiDatabase};

#[test]
fn accepted_syntax_tree_budget_fits_a_small_thread_stack() {
    std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(|| {
            let mut db = FaiDatabase::new();
            crate::std_lib::load_std(&mut db);
            let source = format!("module M\nlet value = {}0\n", "1 + ".repeat(255));
            let id = db.add_source("M.fai".into(), source);
            let file = db.source_file(id).unwrap();
            assert!(fai_syntax::parse::accumulated::<Diag>(&db, file).is_empty());
            let diagnostics = crate::check_file::accumulated::<Diag>(&db, file);
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
        })
        .unwrap()
        .join()
        .unwrap();
}
