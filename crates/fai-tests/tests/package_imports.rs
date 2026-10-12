//! Explicit imports and module re-exports retain their package and type identity.

use std::collections::BTreeMap;

use fai_db::{Db, FaiDatabase, PackageId, SourceFile, SourceOrigin, SourceRole};
use fai_syntax::Symbol;

fn source(db: &mut FaiDatabase, package: PackageId, path: &str, text: &str) -> SourceFile {
    let id = db.add_source_with_origin(path.into(), text.into(), SourceOrigin::Package(package));
    db.source_file(id).unwrap()
}

fn bind(db: &mut FaiDatabase, package: PackageId, dependencies: &[(&str, PackageId)]) {
    db.set_package_dependencies(
        package,
        dependencies.iter().map(|(name, id)| ((*name).to_owned(), *id)).collect(),
        BTreeMap::new(),
    );
}

fn errors(db: &FaiDatabase, file: SourceFile) -> Vec<fai_diagnostics::Diagnostic> {
    fai_driver::check(db, &[file])
        .diagnostics
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == fai_diagnostics::Severity::Error)
        .collect()
}

#[test]
fn imports_values_types_and_constructors_from_a_direct_dependency() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let json = db.register_package("json".into());
    bind(&mut db, app, &[("json", json)]);
    source(
        &mut db,
        json,
        "json/Json.fai",
        "module Json\npublic type Value = | Number Int\npublic identity : Value -> Value\nlet identity value = value\n",
    );
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport json.Json\npublic value : Json.Value\nlet value = Json.identity (Json.Number 1)\n",
    );
    assert!(errors(&db, file).is_empty(), "{:?}", errors(&db, file));
}

#[test]
fn importing_a_nested_module_preserves_qualified_declarations() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let lib = db.register_package("lib".into());
    bind(&mut db, app, &[("lib", lib)]);
    source(
        &mut db,
        lib,
        "lib/Outer.fai",
        "module Outer\nmodule Inner =\n  public type Value = | Number Int\n  public identity : Value -> Value\n  let identity value = value\n",
    );
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport lib.Outer.Inner as I\npublic value : I.Value\nlet value = I.identity (I.Number 1)\n",
    );
    assert!(errors(&db, file).is_empty(), "{:?}", errors(&db, file));
}

#[test]
fn public_reexports_preserve_nominal_identity() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let json = db.register_package("json".into());
    let web = db.register_package("web".into());
    bind(&mut db, app, &[("json", json), ("web", web)]);
    bind(&mut db, web, &[("json", json)]);
    source(
        &mut db,
        json,
        "json/Json.fai",
        "module Json\npublic type Value = | Number Int\npublic identity : Value -> Value\nlet identity value = value\n",
    );
    source(&mut db, web, "web/Web.fai", "module Web\npublic import json.Json as Data\n");
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport web.Web\nimport json.Json as Direct\npublic value : Web.Data.Value\nlet value = Web.Data.identity (Direct.Number 1)\n",
    );
    assert!(errors(&db, file).is_empty(), "{:?}", errors(&db, file));
}

#[test]
fn consumers_can_import_a_public_reexport_without_a_transitive_dependency_alias() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let json = db.register_package("json".into());
    let web = db.register_package("web".into());
    bind(&mut db, app, &[("web", web)]);
    bind(&mut db, web, &[("json", json)]);
    source(&mut db, json, "json/Json.fai", "module Json\npublic value : Int\nlet value = 1\n");
    source(&mut db, web, "web/Web.fai", "module Web\npublic import json.Json\n");
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport web.Web.Json\nlet value = Json.value\n",
    );
    assert!(errors(&db, file).is_empty());
}

#[test]
fn private_imports_cannot_be_followed_as_reexports() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let json = db.register_package("json".into());
    let web = db.register_package("web".into());
    bind(&mut db, app, &[("web", web)]);
    bind(&mut db, web, &[("json", json)]);
    source(&mut db, json, "json/Json.fai", "module Json\n");
    source(&mut db, web, "web/Web.fai", "module Web\nimport json.Json\n");
    let file = source(&mut db, app, "app/App.fai", "module App\nimport web.Web.Json\n");
    let diagnostics = errors(&db, file);
    assert!(diagnostics.iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2022"
        && diagnostic.message.contains("private")));
}

#[test]
fn an_import_does_not_expose_package_internal_members() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let lib = db.register_package("lib".into());
    bind(&mut db, app, &[("lib", lib)]);
    source(
        &mut db,
        lib,
        "lib/Library.fai",
        "module Library\ninternal secret : Int\nlet secret = 1\n",
    );
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport lib.Library\nlet value = Library.secret\n",
    );
    assert!(errors(&db, file).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2020"));
}

#[test]
fn automatic_module_collision_requires_an_alias() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let lib = db.register_package("lib".into());
    bind(&mut db, app, &[("lib", lib)]);
    source(&mut db, lib, "lib/Json.fai", "module Json\n");
    source(&mut db, app, "app/Json.fai", "module Json\n");
    let file = source(&mut db, app, "app/App.fai", "module App\nimport lib.Json\n");
    assert!(errors(&db, file).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2023"));
}

#[test]
fn unknown_dependency_points_at_the_alias() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let text = "module App\nimport absent.Json\n";
    let file = source(&mut db, app, "App.fai", text);
    let diagnostics = errors(&db, file);
    let error =
        diagnostics.iter().find(|diagnostic| diagnostic.code.as_str() == "FAI2021").unwrap();
    assert_eq!(&text[error.primary.start().to_usize()..error.primary.end().to_usize()], "absent");
}

#[test]
fn development_dependencies_are_importable_only_by_test_sources() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let helper = db.register_package("helper".into());
    db.set_package_dependencies(app, BTreeMap::new(), BTreeMap::from([("helper".into(), helper)]));
    source(&mut db, helper, "helper/Helper.fai", "module Helper\n");
    let production = source(&mut db, app, "Library.fai", "module Library\nimport helper.Helper\n");
    let test_id = db.add_source_scoped(
        "Tests.fai".into(),
        "module Tests\nimport helper.Helper\n".into(),
        SourceOrigin::Package(app),
        SourceRole::Test,
    );
    assert!(errors(&db, production).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2021"));
    let test = db.source_file(test_id).unwrap();
    let diagnostics = errors(&db, test);
    // Checking tests also validates their production library. Its invalid import
    // remains an error without making the test scope's valid import an error.
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.primary.source() == production.source(&db))
    );
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.primary.source() != test.source(&db)));
}

#[test]
fn cyclic_reexports_are_diagnostics_instead_of_salsa_cycles() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let a = db.register_package("a".into());
    let b = db.register_package("b".into());
    bind(&mut db, app, &[("a", a)]);
    bind(&mut db, a, &[("b", b)]);
    bind(&mut db, b, &[("a", a)]);
    source(&mut db, a, "a/A.fai", "module A\npublic import b.B.Y as X\n");
    source(&mut db, b, "b/B.fai", "module B\npublic import a.A.X as Y\n");
    let file = source(&mut db, app, "app/App.fai", "module App\nimport a.A.X\n");
    assert!(errors(&db, file).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2024"));
}

fn arithmetic_program() -> (FaiDatabase, SourceFile) {
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let app = db.register_package("app".into());
    let left = db.register_package("left".into());
    let right = db.register_package("right".into());
    bind(&mut db, app, &[("left", left), ("right", right)]);
    source(&mut db, left, "left/Math.fai", "module Math\npublic value : Int\nlet value = 10\n");
    source(&mut db, right, "right/Math.fai", "module Math\npublic value : Int\nlet value = 20\n");
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport left.Math as Left\nimport right.Math as Right\npublic compute : Int -> Int\nlet compute x = Left.value + Right.value + x\npublic main : Runtime -> Unit / { Console }\nlet main rt = rt.console.writeLine (Int.toString (compute 0))\n",
    );
    (db, file)
}

#[test]
fn jit_compiles_distinct_same_named_dependency_modules() {
    let (db, file) = arithmetic_program();
    let mut program = fai_driver::jit_compile(&db, file).unwrap();
    let function = program.function(Symbol::intern("compute")).unwrap();
    let result = fai_runtime::apply(function, &[fai_runtime::make_int(1)]);
    assert_eq!(fai_runtime::read_int(result), 31);
    fai_runtime::fai_drop(result);
}

#[test]
fn worker_bundle_preserves_same_named_dependency_identities() {
    let (db, file) = arithmetic_program();
    let result = fai_driver::build_run_bundle(&db, file);
    assert!(result.diagnostics.is_empty(), "{:?}", result.diagnostics);
    let bundle = result.bundle.unwrap();
    let values: Vec<_> = bundle
        .defs
        .iter()
        .filter(|definition| {
            definition.id.name == "value" && definition.id.module.ends_with(":Math")
        })
        .collect();
    assert_eq!(values.len(), 2);
    assert_ne!(values[0].id.module, values[1].id.module);
}

#[test]
fn imported_interface_names_resolve_in_instances_and_signatures() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let library = db.register_package("library".into());
    bind(&mut db, app, &[("library", library)]);
    source(
        &mut db,
        library,
        "library/Api.fai",
        "module Api\npublic interface Service =\n  run : Int -> Int\n",
    );
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport library.Api as Api\npublic service : Api.Service\nlet service = { Api.Service with run x = x }\n",
    );
    assert!(errors(&db, file).is_empty(), "{:?}", errors(&db, file));
}

#[test]
fn imported_opaque_types_preserve_the_observing_file_boundary() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let library = db.register_package("library".into());
    bind(&mut db, app, &[("library", library)]);
    source(
        &mut db,
        library,
        "library/Api.fai",
        "module Api\npublic opaque type Hidden = | Hidden Int\n",
    );
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport library.Api\nlet value = Api.Hidden 1\n",
    );
    assert!(errors(&db, file).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2018"));
}

#[test]
fn executable_sources_are_not_importable_library_modules() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let library = db.register_package("library".into());
    bind(&mut db, app, &[("library", library)]);
    db.add_source_scoped(
        "library/Main.fai".into(),
        "module Main\n".into(),
        SourceOrigin::Package(library),
        SourceRole::Executable("tool".into()),
    );
    let file = source(&mut db, app, "app/App.fai", "module App\nimport library.Main\n");
    assert!(errors(&db, file).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI2022"));
}

#[test]
fn reexport_edits_invalidate_import_bindings_and_clear_errors() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let library = db.register_package("library".into());
    let bridge = db.register_package("bridge".into());
    bind(&mut db, app, &[("bridge", bridge)]);
    bind(&mut db, bridge, &[("library", library)]);
    source(&mut db, library, "library/Api.fai", "module Api\npublic value : Int\nlet value = 1\n");
    source(&mut db, bridge, "bridge/Bridge.fai", "module Bridge\nimport library.Api\n");
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport bridge.Bridge.Api\nlet value = Api.value\n",
    );
    assert!(!errors(&db, file).is_empty());
    source(&mut db, bridge, "bridge/Bridge.fai", "module Bridge\npublic import library.Api\n");
    assert!(errors(&db, file).is_empty(), "{:?}", errors(&db, file));
}

#[test]
fn dependency_binding_edits_change_types_without_reparsing_the_importer() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let integer = db.register_package("integer".into());
    let boolean = db.register_package("boolean".into());
    source(&mut db, integer, "integer/Api.fai", "module Api\npublic value : Int\nlet value = 1\n");
    source(
        &mut db,
        boolean,
        "boolean/Api.fai",
        "module Api\npublic value : Bool\nlet value = true\n",
    );
    bind(&mut db, app, &[("library", integer)]);
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport library.Api\nlet value = Api.value\n",
    );
    assert_eq!(
        fai_types::render_scheme(&fai_types::def_type(&db, file, Symbol::intern("value"))),
        "Int"
    );
    for source in db.all_source_files() {
        let _ = fai_syntax::parse(&db, source);
    }
    db.enable_event_log();
    bind(&mut db, app, &[("library", boolean)]);
    assert_eq!(
        fai_types::render_scheme(&fai_types::def_type(&db, file, Symbol::intern("value"))),
        "Bool"
    );
    let events = db.take_events();
    assert!(!events.iter().any(|event| event.contains("parse(")), "{events:?}");
}

#[test]
fn imported_module_completion_uses_the_original_public_surface() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let library = db.register_package("library".into());
    bind(&mut db, app, &[("library", library)]);
    let definition = source(
        &mut db,
        library,
        "library/Api.fai",
        "module Api\npublic value : Int\nlet value = 1\ninternal secret : Int\nlet secret = 2\n",
    );
    let text = "module App\nimport library.Api as A\nlet result = A.";
    let file = source(&mut db, app, "app/App.fai", text);
    let completion = fai_ide::completions_at(&db, file, text.len() as u32);
    assert!(completion.items.iter().any(|item| item.label == "value"
        && item.data.as_ref().is_some_and(|data| data.file == definition.source(&db).raw())));
    assert!(!completion.items.iter().any(|item| item.label == "secret"));
}

#[test]
fn import_go_to_definition_targets_the_original_module() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let library = db.register_package("library".into());
    bind(&mut db, app, &[("library", library)]);
    let definition = source(&mut db, library, "library/Api.fai", "module Api\n");
    let text = "module App\nimport library.Api as A\n";
    let file = source(&mut db, app, "app/App.fai", text);
    let result = fai_ide::definition_at(
        &db,
        file,
        text.find("library.Api").unwrap() as u32,
        &fai_db::DbSpanResolver::new(&db),
    );
    assert_eq!(result.definitions.len(), 1);
    assert_eq!(result.definitions[0].span.source, definition.source(&db));
}

#[test]
fn package_import_sample_is_formatted_typechecked_and_runs() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/package_imports");
    let mut db = FaiDatabase::new();
    fai_types::std_lib::load_std(&mut db);
    let app = db.register_package("app".into());
    let collections = db.register_package("collections".into());
    let facade = db.register_package("facade".into());
    bind(&mut db, app, &[("collections", collections), ("facade", facade)]);
    bind(&mut db, facade, &[("collections", collections)]);
    let mut app_file = None;
    for (package, path) in [
        (collections, "collections/src/Values.fai"),
        (facade, "facade/src/Facade.fai"),
        (app, "app/src/Main.fai"),
    ] {
        let text = std::fs::read_to_string(root.join(path)).unwrap();
        let parsed = fai_syntax::parse_module(fai_span::SourceId::new(0), &text);
        assert!(parsed.diagnostics.is_empty(), "{path}: {:?}", parsed.diagnostics);
        assert_eq!(fai_fmt::format(&parsed.module, &parsed.comments, &text), text, "{path}");
        let file = source(&mut db, package, path, &text);
        assert!(errors(&db, file).is_empty(), "{path}: {:?}", errors(&db, file));
        if package == app {
            app_file = Some(file);
        }
    }
    let outcome =
        fai_driver::run_tests(&db, &[app_file.unwrap()], None, fai_driver::TestConfig::default());
    assert!(outcome.ok, "{:?}", outcome.diagnostics);
    assert_eq!(outcome.passed, 1);
}

#[test]
fn aot_links_distinct_same_named_dependency_modules() {
    let (db, file) = arithmetic_program();
    let directory = tempfile::tempdir().unwrap();
    let output = camino::Utf8PathBuf::from_path_buf(
        directory.path().join(format!("imports{}", std::env::consts::EXE_SUFFIX)),
    )
    .unwrap();
    let outcome = fai_driver::build_native(&db, file, &output);
    assert!(outcome.ok, "{:?}", outcome.diagnostics);
    let result = std::process::Command::new(output.as_std_path()).output().unwrap();
    assert!(result.status.success(), "{:?}", result);
    assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "30");
}

#[test]
fn api_queries_expose_public_module_reexports() {
    let mut db = FaiDatabase::new();
    let library = db.register_package("library".into());
    let facade = db.register_package("facade".into());
    bind(&mut db, facade, &[("library", library)]);
    let original = source(&mut db, library, "library/Api.fai", "module Api\n");
    let file = source(
        &mut db,
        facade,
        "facade/Facade.fai",
        "module Facade\npublic import library.Api as Data\n",
    );
    let api = fai_ide::api(&db, "Facade", &[file], &fai_db::DbSpanResolver::new(&db));
    assert_eq!(api.modules.len(), 1);
    assert_eq!(api.modules[0].name, "Data");
    assert_eq!(api.modules[0].target, "Api");
    assert_eq!(api.modules[0].span.source, original.source(&db));
}

#[test]
fn record_projection_after_an_imported_value_is_not_a_module_segment() {
    let mut db = FaiDatabase::new();
    let app = db.register_package("app".into());
    let library = db.register_package("library".into());
    bind(&mut db, app, &[("library", library)]);
    source(
        &mut db,
        library,
        "library/Api.fai",
        "module Api\npublic data : { value : Int }\nlet data = { value = 1 }\n",
    );
    let file = source(
        &mut db,
        app,
        "app/App.fai",
        "module App\nimport library.Api\nlet value = Api.data.value\n",
    );
    assert!(errors(&db, file).is_empty());
    assert_eq!(
        fai_types::render_scheme(&fai_types::def_type(&db, file, Symbol::intern("value"))),
        "Int"
    );
}

mod proptests {
    use super::*;
    use proptest::prelude::*;

    fn fixture(public: bool) -> (FaiDatabase, SourceFile, PackageId) {
        let mut db = FaiDatabase::new();
        let app = db.register_package("app".into());
        let library = db.register_package("library".into());
        let facade = db.register_package("facade".into());
        bind(&mut db, app, &[("facade", facade)]);
        bind(&mut db, facade, &[("library", library)]);
        source(
            &mut db,
            library,
            "library/Api.fai",
            "module Api\npublic value : Int\nlet value = 1\n",
        );
        set_facade(&mut db, facade, public);
        let file = source(
            &mut db,
            app,
            "app/App.fai",
            "module App\nimport facade.Facade.Api\nlet value = Api.value\n",
        );
        (db, file, facade)
    }

    fn set_facade(db: &mut FaiDatabase, facade: PackageId, public: bool) {
        let marker = if public { "public " } else { "" };
        source(
            db,
            facade,
            "facade/Facade.fai",
            &format!("module Facade\n{marker}import library.Api\n"),
        );
    }

    proptest! {
        #[test]
        fn reexport_visibility_edits_match_clean_checks(states in prop::collection::vec(any::<bool>(), 1..8)) {
            let (mut db, file, facade) = fixture(false);
            for public in states {
                set_facade(&mut db, facade, public);
                let (clean, clean_file, _) = fixture(public);
                let codes = |db: &FaiDatabase, file| errors(db, file).into_iter().map(|error| error.code.as_str()).collect::<Vec<_>>();
                prop_assert_eq!(codes(&db, file), codes(&clean, clean_file));
            }
        }
    }
}
