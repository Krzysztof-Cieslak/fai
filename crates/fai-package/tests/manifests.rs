use fai_diagnostics::Diagnostic;
use fai_package::{
    Dependency, GitSelector, ManifestDocument, ManifestKind, PackageVersion, TargetPlatform,
};
use fai_span::SourceId;
use indoc::indoc;

const BASE: &str = "manifest-version = 1\n[package]\nname = 'demo'\nversion = '0.1.0'\n";

#[test]
fn dependency_edits_retain_surrounding_comments() {
    let text = format!(
        "{BASE}\n# shared tools\n[dependencies]\ntools = {{path = '../old'}} # keep this comment\n"
    );
    let original = parse(&text);
    let alias = fai_package::DependencyAlias::parse("tools").unwrap();
    let edited = original
        .with_dependency(&alias, &Dependency::Path { path: "../new".into(), version: None }, false)
        .unwrap();
    assert!(edited.text().contains("# shared tools"));
    assert!(edited.text().contains("# keep this comment"));
    assert_eq!(original.text(), text);
}

#[test]
fn removing_a_normal_dependency_retains_development_entries() {
    let original = parse(&format!(
        "{BASE}\n[dependencies]\ntools = {{path = '../tools'}}\n[dev-dependencies]\nchecks = {{path = '../checks'}}\n"
    ));
    let alias = fai_package::DependencyAlias::parse("tools").unwrap();
    let edited = original.without_dependency(&alias, false).unwrap();
    assert!(edited.manifest().dependencies.is_empty());
    assert_eq!(edited.manifest().dev_dependencies.len(), 1);
}

fn parse(text: &str) -> ManifestDocument {
    ManifestDocument::parse(SourceId::new(7), text).unwrap()
}

fn errors(text: &str) -> Vec<Diagnostic> {
    ManifestDocument::parse(SourceId::new(7), text).unwrap_err()
}

fn excerpt<'a>(text: &'a str, diagnostic: &Diagnostic) -> &'a str {
    &text[diagnostic.primary.start().to_usize()..diagnostic.primary.end().to_usize()]
}

#[test]
fn complete_manifest_selects_platform_configuration() {
    let document = parse(include_str!("fixtures/package.toml"));
    let manifest = document.manifest();
    let windows =
        manifest.configuration(&TargetPlatform::parse("x86_64-pc-windows-msvc").unwrap()).unwrap();
    let linux = manifest
        .configuration(&TargetPlatform::parse("x86_64-unknown-linux-gnu").unwrap())
        .unwrap();
    assert_eq!(windows.native.libraries, ["common", "ws2_32"]);
    assert_eq!(linux.native.libraries, ["common"]);
    assert_eq!(
        windows.dependencies.keys().map(ToString::to_string).collect::<Vec<_>>(),
        ["json", "windows"]
    );
    assert_eq!(linux.dependencies.keys().map(ToString::to_string).collect::<Vec<_>>(), ["json"]);
    assert_eq!(manifest.lock_targets.len(), 2);
    assert_eq!(manifest.toolchain.as_ref().unwrap().to_string(), "0.5.3");
}

#[test]
fn semantic_model_excludes_comments_and_source_offsets() {
    assert_eq!(parse(BASE).manifest(), parse(&format!("# café 🦀\n{BASE}")).manifest());
    assert_eq!(
        serde_json::to_string(parse(BASE).manifest()).unwrap(),
        serde_json::to_string(parse(&format!("\n{BASE}")).manifest()).unwrap()
    );
}

#[test]
fn package_defaults_are_explicit_model_values() {
    let document = parse(BASE);
    let package = document.manifest().package.as_ref().unwrap();
    assert_eq!(package.source_dir.as_str(), "src");
    assert_eq!(package.test_dir.as_str(), "tests");
    assert!(package.auto_main);
    assert!(package.publish);
    assert!(!package.explicit_fai_version);
    assert_eq!(package.fai_version.to_string(), "*");
}

#[test]
fn virtual_workspace_is_valid() {
    let document = parse("manifest-version = 1\n[workspace]\nmembers = ['apps/*', 'packages/*']\n");
    assert!(document.manifest().package.is_none());
    assert_eq!(document.manifest().workspace.as_ref().unwrap().members, ["apps/*", "packages/*"]);
}

#[test]
fn private_workspace_package_opts_out_of_publication() {
    let document = parse(&format!("{BASE}publish = false\n"));
    assert!(!document.manifest().package.as_ref().unwrap().publish);
}

#[test]
fn publication_policy_requires_a_boolean() {
    let text = format!("{BASE}publish = 'no'\n");
    let diagnostics = errors(&text);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0800");
    assert_eq!(excerpt(&text, &diagnostics[0]), "'no'");
}

#[test]
fn unknown_field_has_exact_utf8_aware_location() {
    let text = format!("# café 🦀\n{BASE}soruce-dir = 'lib'\n");
    let diagnostics = errors(&text);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0800");
    assert_eq!(diagnostics[0].message, "unknown field `soruce-dir`");
    assert_eq!(diagnostics[0].primary.source(), SourceId::new(7));
    assert_eq!(excerpt(&text, &diagnostics[0]), "soruce-dir");
    assert_eq!(diagnostics[0].primary.start().to_usize(), text.find("soruce-dir").unwrap());
}

#[test]
fn unsupported_format_is_not_interpreted_as_current_schema() {
    let text = "manifest-version = 2\nnew-future-schema = true\n";
    let diagnostics = errors(text);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0801");
    assert_eq!(excerpt(text, &diagnostics[0]), "2");
}

#[test]
fn malformed_toml_is_a_diagnostic() {
    let diagnostics = errors("[package\nname = 'broken'\n");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0800");
}

#[test]
fn independent_field_errors_accumulate() {
    let text = "manifest-version = 1\n[package]\nname = 1\nversion = true\nauto-main = 'yes'\n";
    let diagnostics = errors(text);
    assert_eq!(diagnostics.len(), 3);
    assert_eq!(
        diagnostics.iter().map(|diagnostic| excerpt(text, diagnostic)).collect::<Vec<_>>(),
        ["1", "true", "'yes'"]
    );
}

#[test]
fn ordinary_manifest_requires_a_package_or_workspace() {
    let diagnostics = errors("manifest-version = 1\n");
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic.message.contains("[package], [workspace]"))
    );
}

#[test]
fn workspace_cannot_implicitly_supply_member_dependencies() {
    let text =
        "manifest-version = 1\n[workspace]\nmembers = []\n[dependencies]\nfoo = {path = 'foo'}\n";
    let diagnostics = errors(text);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.message == "`dependencies` requires a [package] table")
    );
}

#[test]
fn local_paths_may_refer_to_other_packages() {
    let document = parse(&format!("{BASE}[dependencies]\nother = {{path = '../other'}}\n"));
    let dependency = document.manifest().dependencies.values().next().unwrap();
    assert!(matches!(dependency, Dependency::Path { path, .. } if path == "../other"));
}

#[test]
fn contained_source_paths_cannot_escape() {
    let text = format!("{BASE}source-dir = '../outside'\n");
    let diagnostics = errors(&text);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0802");
    assert_eq!(excerpt(&text, &diagnostics[0]), "'../outside'");
}

#[test]
fn git_sources_require_exactly_one_selector() {
    let text = format!(
        "{BASE}[dependencies]\na = {{git = 'https://example.com/a', branch = 'main', version = '^1.0.0'}}\n"
    );
    assert!(errors(&text).iter().any(|diagnostic| diagnostic.message.contains("exactly one")));
}

#[test]
fn git_source_without_selector_is_rejected() {
    let text = format!("{BASE}[dependencies]\na = {{git = 'https://example.com/a'}}\n");
    assert!(errors(&text).iter().any(|diagnostic| diagnostic.message.contains("exactly one")));
}

#[test]
fn path_and_git_sources_cannot_be_combined() {
    let text = format!(
        "{BASE}[dependencies]\na = {{path = '../a', git = 'https://example.com/a', branch = 'main'}}\n"
    );
    assert!(errors(&text).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI0802"));
}

#[test]
fn dependency_bare_version_is_exact() {
    let document = parse(&format!(
        "{BASE}[dependencies]\na = {{git = 'https://example.com/a', version = '1.2.3'}}\n"
    ));
    let Dependency::Git { selector: GitSelector::Version(requirement), .. } =
        document.manifest().dependencies.values().next().unwrap()
    else {
        panic!("expected a version dependency");
    };
    assert!(!requirement.matches(&PackageVersion::parse("1.2.4").unwrap()));
}

#[test]
fn malformed_requirement_uses_its_value_span() {
    let text =
        format!("{BASE}[dependencies]\na = {{git = 'https://example.com/a', version = '^1.2'}}\n");
    let diagnostics = errors(&text);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0803");
    assert_eq!(excerpt(&text, &diagnostics[0]), "'^1.2'");
}

#[test]
fn wrong_typed_array_element_is_located() {
    let text = format!("{BASE}[native]\nlibraries = ['ok', 123]\n");
    let diagnostics = errors(&text);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(excerpt(&text, &diagnostics[0]), "123");
}

#[test]
fn normal_and_development_aliases_cannot_collide() {
    let text = format!(
        "{BASE}[dependencies]\na = {{path = 'one'}}\n[dev-dependencies]\na = {{path = 'two'}}\n"
    );
    let diagnostics = errors(&text);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0818");
    assert_eq!(excerpt(&text, &diagnostics[0]), "{path = 'two'}");
    assert_eq!(diagnostics[0].secondary.len(), 1);
}

#[test]
fn common_and_platform_aliases_cannot_collide() {
    let text = format!(
        "{BASE}[dependencies]\na = {{path = 'one'}}\n[[platform]]\nwhen = {{os = 'windows'}}\n[platform.dependencies]\na = {{path = 'two'}}\n"
    );
    assert!(errors(&text).iter().any(|diagnostic| diagnostic.code.as_str() == "FAI0818"));
}

#[test]
fn disjoint_platforms_can_bind_the_same_alias() {
    let text = format!(
        "{BASE}{}",
        indoc! {"
        [[platform]]
        when = { os = 'windows' }
        [platform.dependencies]
        platform = { path = '../windows' }
        [[platform]]
        when = { os = 'linux' }
        [platform.dependencies]
        platform = { path = '../linux' }
    "}
    );
    assert_eq!(parse(&text).manifest().platforms.len(), 2);
}

#[test]
fn errors_in_earlier_binaries_do_not_shift_duplicate_locations() {
    let text = format!(
        "{BASE}{}",
        indoc! {"
        [[bin]]
        name = 123
        path = 'bad.fai'
        [[bin]]
        name = 'same'
        path = 'one.fai'
        [[bin]]
        name = 'same'
        path = 'two.fai'
    "}
    );
    let diagnostics = errors(&text);
    let duplicate = diagnostics
        .iter()
        .find(|diagnostic| diagnostic.message.contains("duplicate executable"))
        .unwrap();
    assert_eq!(duplicate.primary.start().to_usize(), text.rfind("'same'").unwrap());
    assert_eq!(duplicate.secondary[0].span.start().to_usize(), text.find("'same'").unwrap());
}

#[test]
fn errors_in_earlier_platforms_do_not_shift_alias_locations() {
    let text = format!(
        "{BASE}{}",
        indoc! {"
        [[platform]]
        when = {}
        [[platform]]
        when = { os = 'windows' }
        [platform.dependencies]
        a = { path = 'one' }
        [[platform]]
        when = { arch = 'x86_64' }
        [platform.dependencies]
        a = { path = 'two' }
    "}
    );
    let diagnostics = errors(&text);
    let duplicate =
        diagnostics.iter().find(|diagnostic| diagnostic.code.as_str() == "FAI0818").unwrap();
    assert_eq!(excerpt(&text, duplicate), "{ path = 'two' }");
}

#[test]
fn invalid_root_array_elements_do_not_shift_duplicate_locations() {
    let text = format!(
        "{BASE}[[platform]]\nwhen = {{os = 'linux'}}\nsource-dirs = ['../bad', 'extra', 'extra']\n"
    );
    let diagnostics = errors(&text);
    let duplicate =
        diagnostics.iter().find(|diagnostic| diagnostic.code.as_str() == "FAI0819").unwrap();
    assert_eq!(duplicate.primary.start().to_usize(), text.rfind("'extra'").unwrap());
}

#[test]
fn overlapping_override_members_are_rejected() {
    let text = indoc! {"
        manifest-version = 1
        [workspace]
        members = ['apps/*']
        [[overrides]]
        replace = { git = 'https://github.com/Example/Json' }
        with = { path = 'one' }
        members = ['apps/server']
        [[overrides]]
        replace = { git = 'git@github.com:example/json.git' }
        with = { path = 'two' }
    "};
    let diagnostics = errors(text);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0816");
    assert_eq!(excerpt(text, &diagnostics[0]), "{ git = 'git@github.com:example/json.git' }");
}

#[test]
fn disjoint_override_members_are_allowed() {
    let text = indoc! {"
        manifest-version = 1
        [workspace]
        members = ['apps/*']
        [[overrides]]
        replace = { git = 'https://example.com/json' }
        with = { path = 'one' }
        members = ['apps/one']
        [[overrides]]
        replace = { git = 'https://example.com/json' }
        with = { path = 'two' }
        members = ['apps/two']
    "};
    assert_eq!(parse(text).manifest().overrides.len(), 2);
}

#[test]
fn empty_override_scope_is_rejected() {
    let text = format!(
        "{BASE}[[overrides]]\nreplace = {{git = 'https://example.com/a'}}\nwith = {{path = 'a'}}\nmembers = []\n"
    );
    assert!(
        errors(&text)
            .iter()
            .any(|diagnostic| diagnostic.message.contains("scope must not be empty"))
    );
}

#[test]
fn personal_manifests_contain_only_overrides() {
    let text = "manifest-version = 1\n[dependencies]\na = {path = 'a'}\n";
    let diagnostics = ManifestDocument::parse_local(SourceId::new(0), text).unwrap_err();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].message, "unknown field `dependencies`");
}

#[test]
fn personal_manifest_does_not_require_a_package() {
    let document =
        ManifestDocument::parse_local(SourceId::new(0), "manifest-version = 1\n").unwrap();
    assert_eq!(document.kind(), ManifestKind::Local);
}

#[test]
fn native_only_legacy_manifest_is_recognized() {
    let document = parse("[native]\nlibraries = ['math']\nobjects = ['native/helper.o']\n");
    assert_eq!(document.kind(), ManifestKind::LegacyNative);
    assert_eq!(document.manifest().native.libraries, ["math"]);
}

#[test]
fn version_edits_preserve_surrounding_comments_and_original_document() {
    let text = "# project\nmanifest-version = 1\n[package]\nname = 'demo'\nversion = \"0.1.0\" # public release\n# trailing\n";
    let original = parse(text);
    let updated = original.with_package_version(&PackageVersion::parse("0.2.0").unwrap()).unwrap();
    assert_eq!(updated.text(), text.replace("0.1.0", "0.2.0"));
    assert_eq!(original.text(), text);
    assert_eq!(updated.manifest().package.as_ref().unwrap().version.to_string(), "0.2.0");
}

#[test]
fn virtual_workspace_version_edit_is_a_diagnostic() {
    let document = parse("manifest-version = 1\n[workspace]\nmembers = []\n");
    assert!(document.with_package_version(&PackageVersion::parse("0.2.0").unwrap()).is_err());
}

#[test]
fn deeply_nested_metadata_is_a_parse_error_instead_of_a_stack_failure() {
    let text = format!("{BASE}[metadata]\ndeep = {}0{}\n", "[".repeat(256), "]".repeat(256));
    let diagnostics = errors(&text);
    assert_eq!(diagnostics[0].code.as_str(), "FAI0800");
}

#[test]
fn repeated_conflicting_overrides_report_one_error_per_entry() {
    let mut text = BASE.to_owned();
    for _ in 0..100 {
        text.push_str(
            "[[overrides]]\nreplace = {git = 'https://example.com/a'}\nwith = {path = 'a'}\n",
        );
    }
    let diagnostics = errors(&text);
    assert_eq!(diagnostics.len(), 99);
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.code.as_str() == "FAI0816"));
}

#[test]
fn repeated_conflicting_platform_aliases_report_one_error_per_entry() {
    let mut text = BASE.to_owned();
    for _ in 0..100 {
        text.push_str(
            "[[platform]]\nwhen = {os = 'linux'}\n[platform.dependencies]\na = {path = 'a'}\n",
        );
    }
    let diagnostics = errors(&text);
    assert_eq!(diagnostics.len(), 99);
    assert!(diagnostics.iter().all(|diagnostic| diagnostic.code.as_str() == "FAI0818"));
}
