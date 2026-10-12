use fai_diagnostics::Diagnostic;
use fai_package::{ManifestDocument, PackagePath, SourcePlan, TargetPlatform};
use fai_span::SourceId;

const BASE: &str = "manifest-version = 1\n[package]\nname = 'demo'\nversion = '0.1.0'\n";

fn plan(text: &str, target: &str, files: &[&str]) -> Result<SourcePlan, Vec<Diagnostic>> {
    let document = ManifestDocument::parse(SourceId::new(0), text).unwrap();
    let files = files.iter().map(|file| PackagePath::parse(file).unwrap()).collect::<Vec<_>>();
    SourcePlan::build(&document, &TargetPlatform::parse(target).unwrap(), &files)
}

fn linux(text: &str, files: &[&str]) -> SourcePlan {
    plan(text, "x86_64-unknown-linux-gnu", files).unwrap()
}

fn names(paths: &[PackagePath]) -> Vec<&str> {
    paths.iter().map(PackagePath::as_str).collect()
}

#[test]
fn conventional_main_is_executable_and_not_library_api() {
    let sources = linux(BASE, &["src/Main.fai", "src/Engine.fai"]);
    assert_eq!(names(&sources.library), ["src/Engine.fai"]);
    assert_eq!(sources.executables.len(), 1);
    assert_eq!(sources.executables[0].name.as_str(), "demo");
    assert_eq!(names(&sources.executables[0].sources), ["src/Engine.fai", "src/Main.fai"]);
}

#[test]
fn library_does_not_require_an_entry() {
    let sources = linux(BASE, &["src/Library.fai"]);
    assert!(sources.executables.is_empty());
    assert_eq!(names(&sources.library), ["src/Library.fai"]);
}

#[test]
fn multiple_entries_do_not_see_each_other() {
    let text = format!("{BASE}[[bin]]\nname = 'tool'\npath = 'tools/Main.fai'\n");
    let sources = linux(&text, &["src/Main.fai", "src/Library.fai", "tools/Main.fai"]);
    assert_eq!(names(&sources.executables[0].sources), ["src/Library.fai", "src/Main.fai"]);
    assert_eq!(names(&sources.executables[1].sources), ["src/Library.fai", "tools/Main.fai"]);
}

#[test]
fn explicit_name_replaces_the_conventional_entry() {
    let text = format!("{BASE}[[bin]]\nname = 'demo'\npath = 'tools/Other.fai'\n");
    let sources = linux(&text, &["src/Main.fai", "src/Library.fai", "tools/Other.fai"]);
    assert_eq!(sources.executables.len(), 1);
    assert_eq!(sources.executables[0].entry.as_str(), "tools/Other.fai");
    assert_eq!(names(&sources.library), ["src/Library.fai"]);
}

#[test]
fn explicit_path_claim_does_not_infer_a_duplicate_entry() {
    let text = format!("{BASE}[[bin]]\nname = 'custom'\npath = 'src/Main.fai'\n");
    let sources = linux(&text, &["src/Main.fai"]);
    assert_eq!(sources.executables.len(), 1);
    assert_eq!(sources.executables[0].name.as_str(), "custom");
}

#[test]
fn disabling_main_inference_restores_an_ordinary_library_file() {
    let text = format!("{BASE}auto-main = false\n");
    let sources = linux(&text, &["src/Main.fai"]);
    assert!(sources.executables.is_empty());
    assert_eq!(names(&sources.library), ["src/Main.fai"]);
}

#[test]
fn test_directory_is_pruned_from_a_broad_library_root() {
    let text = format!("{BASE}source-dir = '.'\n");
    let sources = linux(&text, &["Library.fai", "tests/LibraryTests.fai"]);
    assert_eq!(names(&sources.library), ["Library.fai"]);
    assert_eq!(names(&sources.tests), ["tests/LibraryTests.fai"]);
}

#[test]
fn inactive_platform_roots_do_not_leak_into_common_scanning() {
    let text =
        format!("{BASE}[[platform]]\nwhen = {{os = 'windows'}}\nsource-dirs = ['src/windows']\n");
    let sources = linux(&text, &["src/Library.fai", "src/windows/Platform.fai"]);
    assert_eq!(names(&sources.library), ["src/Library.fai"]);
}

#[test]
fn active_platform_roots_are_added_to_the_library() {
    let text = format!(
        "{BASE}[[platform]]\nwhen = {{os = 'windows'}}\nsource-dirs = ['platform/windows']\n"
    );
    let sources = plan(
        &text,
        "x86_64-pc-windows-msvc",
        &["src/Library.fai", "platform/windows/Platform.fai"],
    )
    .unwrap();
    assert_eq!(names(&sources.library), ["platform/windows/Platform.fai", "src/Library.fai"]);
}

#[test]
fn inactive_test_roots_are_reserved_from_common_tests() {
    let text = format!(
        "{BASE}[[platform]]\nwhen = {{os = 'windows'}}\ntest-source-dirs = ['tests/windows']\n"
    );
    let sources = linux(&text, &["tests/Common.fai", "tests/windows/PlatformTests.fai"]);
    assert_eq!(names(&sources.tests), ["tests/Common.fai"]);
}

#[test]
fn nested_package_sources_are_not_owned_by_the_parent() {
    let sources = linux(BASE, &["src/Library.fai", "src/child/fai.toml", "src/child/Child.fai"]);
    assert_eq!(names(&sources.library), ["src/Library.fai"]);
}

#[test]
fn an_entry_in_another_package_is_rejected() {
    let text = format!("{BASE}[[bin]]\nname = 'child'\npath = 'child/src/Main.fai'\n");
    let diagnostics =
        plan(&text, "x86_64-unknown-linux-gnu", &["child/fai.toml", "child/src/Main.fai"])
            .unwrap_err();
    assert_eq!(diagnostics[0].code.as_str(), "FAI0819");
    assert!(diagnostics[0].message.contains("outside this package's source ownership"));
}

#[test]
fn generated_and_vendor_directories_are_excluded() {
    let text = format!("{BASE}source-dir = '.'\n[vendor]\ndirectory = 'vendor'\n");
    let sources = linux(&text, &["Library.fai", ".fai/generated/Hidden.fai", "vendor/Hidden.fai"]);
    assert_eq!(names(&sources.library), ["Library.fai"]);
}

#[test]
fn missing_explicit_entry_points_at_the_path_field() {
    let text = format!("{BASE}[[bin]]\nname = 'tool'\npath = 'tools/Missing.fai'\n");
    let diagnostics = plan(&text, "x86_64-unknown-linux-gnu", &[]).unwrap_err();
    let range = diagnostics[0].primary;
    assert_eq!(&text[range.start().to_usize()..range.end().to_usize()], "'tools/Missing.fai'");
}

#[test]
fn default_run_must_identify_an_existing_target() {
    let text = format!("{BASE}default-run = 'missing'\n");
    let diagnostics = plan(&text, "x86_64-unknown-linux-gnu", &["src/Main.fai"]).unwrap_err();
    assert_eq!(diagnostics[0].message, "default-run names unknown executable `missing`");
}

#[test]
fn supported_platforms_restrict_source_planning() {
    let text = format!("{BASE}supported-platforms = [{{os = 'windows'}}]\n");
    let diagnostics = plan(&text, "x86_64-unknown-linux-gnu", &[]).unwrap_err();
    assert_eq!(diagnostics[0].code.as_str(), "FAI0810");
}

#[test]
fn file_inventory_order_and_duplicates_do_not_change_the_plan() {
    let ordered = linux(BASE, &["src/A.fai", "src/B.fai", "src/Main.fai"]);
    let shuffled = linux(BASE, &["src/Main.fai", "src/B.fai", "src/A.fai", "src/B.fai"]);
    assert_eq!(ordered, shuffled);
}
