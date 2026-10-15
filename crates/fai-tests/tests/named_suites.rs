//! Type and incremental boundaries for ordinary named-suite values.

use fai_db::Db;

fn check(source: &str) -> fai_driver::CommandResult {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.into());
    fai_driver::check(&db, &[db.source_file(id).unwrap()])
}

#[test]
fn heterogeneous_suite_effects_join_without_erasure() {
    let result = check(
        "module M\na : Test.Context -> Test.Check / { Console }\nlet a ctx =\n  let printed = ctx.runtime.console.writeLine \"a\"\n  Test.pass\nb : Test.Context -> Test.Check / { Clock }\nlet b ctx =\n  let slept = ctx.runtime.clock.sleep 1\n  Test.pass\ntests : Test.Suite Test.Context { Console, Clock }\nlet tests = Test.describe \"all\" [Test.test \"pure\" (fun _ -> Test.pass), Test.test \"a\" a, Test.test \"b\" b]\n",
    );
    assert!(result.ok, "{:?}", result.diagnostics);
}

#[test]
fn pure_suite_cannot_hide_an_effectful_case() {
    let result = check(
        "module M\nnoisy : Test.Context -> Test.Check / { Console }\nlet noisy ctx =\n  let printed = ctx.runtime.console.writeLine \"effect\"\n  Test.pass\ntests : Test.Suite Test.Context {}\nlet tests = Test.describe \"all\" [Test.test \"pure\" (fun _ -> Test.pass), Test.test \"noisy\" noisy]\n",
    );
    assert!(!result.ok);
    assert!(result.diagnostics.iter().any(|d| matches!(d.code.as_str(), "FAI3001" | "FAI3004")));
}

#[test]
fn named_properties_cannot_hide_effectful_predicates() {
    let result = check(
        "module M\nnoisy : Int -> Test.Check / { Console }\nlet noisy n =\n  let printed = stdConsole.writeLine (Int.toString n)\n  Test.pass\ntests : Test.Suite Test.Context {}\nlet tests = Test.property \"noisy\" Test.int noisy\n",
    );
    assert!(!result.ok);
    assert!(result.diagnostics.iter().any(|d| matches!(d.code.as_str(), "FAI3001" | "FAI3004")));
}

#[test]
fn ordinary_function_lists_still_require_equal_effects() {
    let result = check(
        "module M\na : Runtime -> Unit / { Console }\nlet a rt = rt.console.writeLine \"a\"\nb : Runtime -> Unit / { Clock }\nlet b rt = rt.clock.sleep 1\nlet functions = [a, b]\n",
    );
    assert!(!result.ok);
    assert!(result.diagnostics.iter().any(|d| d.code.as_str() == "FAI3001"));
}

#[test]
fn a_user_type_named_suite_does_not_get_the_testing_join() {
    let result = check(
        "module M\ntype Suite 'e = | Wrap (Unit -> Int / 'e)\nnoisy : Unit -> Int / { Console }\nlet noisy _ =\n  let printed = stdConsole.writeLine \"a\"\n  1\nlet suites = [Wrap (fun _ -> 1), Wrap noisy]\n",
    );
    assert!(!result.ok);
    assert!(result.diagnostics.iter().any(|d| d.code.as_str() == "FAI3001"));
}

#[test]
fn source_coordinates_do_not_enter_the_lowered_suite_body() {
    let source = "module M\ntests : Test.Suite Test.Context {}\nlet tests = Test.test \"one\" (fun _ -> Expect.equal Int.toString 1 1)\n";
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let name = fai_syntax::Symbol::intern("tests");
    let before = fai_core::core(&db, file, name);
    db.add_source("M.fai".into(), format!("// shifted 🌍\n\n{source}"));
    let after = fai_core::core(&db, file, name);
    assert_eq!(before, after);
}

#[test]
fn property_source_coordinates_do_not_enter_the_lowered_suite_body() {
    let source = "module M\ntests : Test.Suite Test.Context {}\nlet tests = Test.property \"identity\" Test.int (fun n -> Expect.equal Int.toString n n)\n";
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source("M.fai".into(), source.into());
    let file = db.source_file(id).unwrap();
    let name = fai_syntax::Symbol::intern("tests");
    let before = fai_core::core(&db, file, name);
    db.add_source("M.fai".into(), format!("// shifted 🌍\n\n{source}"));
    let after = fai_core::core(&db, file, name);
    assert_eq!(before, after);
}

#[test]
fn a_named_root_requires_its_explicit_signature() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let id = db.add_source(
        "M.fai".into(),
        "module M\nlet tests = Test.test \"one\" (fun _ -> Test.pass)\n".into(),
    );
    let plan = fai_driver::build_test_plan(
        &db,
        &[db.source_file(id).unwrap()],
        None,
        fai_driver::TestConfig::default(),
    );
    assert!(plan.blocked);
    assert!(plan.pre_diagnostics.iter().any(|d| d.code.as_str() == "FAI6007"));
}

#[test]
fn embedded_testing_helpers_satisfy_their_contracts() {
    let mut db = fai_db::FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let modules = ["Test", "Expect"].map(|name| {
        fai_resolve::std_module_file(&db, fai_resolve::ModuleName(fai_syntax::Symbol::intern(name)))
            .unwrap()
    });
    let result = fai_driver::run_tests(&db, &modules, None, fai_driver::TestConfig::default());
    assert!(result.ok, "{:?}", result.diagnostics);
    assert_eq!(result.leaked, 0);
}
