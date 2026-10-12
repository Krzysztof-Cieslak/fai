use fai_span::SourceId;
use fai_syntax::{ast::Visibility, build_item_tree, parse_module};

#[test]
fn parses_aliased_and_public_module_imports() {
    let parsed = parse_module(
        SourceId::new(0),
        "module App\nimport json.Json as J\npublic import web.Web.Json\nlet value = 1\n",
    );
    assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
    assert_eq!(parsed.module.imports.len(), 2);
    assert_eq!(parsed.module.imports[0].binding().unwrap().as_str(), "J");
    assert_eq!(parsed.module.imports[1].visibility, Visibility::Public);
    assert_eq!(parsed.module.roots.len(), 1);
}

#[test]
fn import_path_and_alias_locations_are_exact() {
    let text = "module App\n// café\nimport json.Json as Alias\n";
    let parsed = parse_module(SourceId::new(7), text);
    let import = &parsed.module.imports[0];
    let parts: Vec<_> = import
        .path_spans
        .iter()
        .map(|span| &text[span.start().to_usize()..span.end().to_usize()])
        .collect();
    assert_eq!(parts, ["json", "Json"]);
    let span = import.alias_span.unwrap();
    assert_eq!(&text[span.start().to_usize()..span.end().to_usize()], "Alias");
}

#[test]
fn import_summary_is_position_independent() {
    let first = parse_module(SourceId::new(0), "module App\nimport json.Json\n");
    let shifted = parse_module(SourceId::new(0), "module App\n// moved\nimport json.Json\n");
    assert_eq!(build_item_tree(&first.module), build_item_tree(&shifted.module));
}

#[test]
fn import_target_change_changes_the_item_tree() {
    let first = parse_module(SourceId::new(0), "module App\nimport json.Json\n");
    let changed = parse_module(SourceId::new(0), "module App\nimport alternate.Json\n");
    assert_ne!(build_item_tree(&first.module), build_item_tree(&changed.module));
}

#[test]
fn import_after_an_ordinary_item_is_rejected() {
    let parsed = parse_module(SourceId::new(0), "module App\nlet value = 1\nimport json.Json\n");
    assert!(
        parsed.diagnostics.iter().any(|diagnostic| diagnostic.message.contains("must precede"))
    );
}

#[test]
fn incomplete_import_recovers_to_the_next_item() {
    let parsed = parse_module(SourceId::new(0), "module App\nimport json.\nlet value = 1\n");
    assert!(!parsed.diagnostics.is_empty());
    assert_eq!(parsed.module.roots.len(), 1);
}

#[test]
fn importing_a_value_instead_of_a_module_is_rejected() {
    let parsed = parse_module(SourceId::new(0), "module App\nimport json.parse\n");
    assert!(
        parsed
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message.contains("upper-case module"))
    );
}

#[test]
fn nested_imports_are_rejected() {
    let parsed = parse_module(SourceId::new(0), "module App\nmodule Inner =\n  import json.Json\n");
    assert!(parsed.diagnostics.iter().any(|diagnostic| diagnostic.message.contains("file-level")));
}

#[test]
fn internal_imports_are_rejected() {
    let parsed = parse_module(SourceId::new(0), "module App\ninternal import json.Json\n");
    assert!(!parsed.diagnostics.is_empty());
}
