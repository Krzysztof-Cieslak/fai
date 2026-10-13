//! Public release manifests reject ambiguous identities before downloading assets.

use fai_toolchain::Release;

fn manifest() -> serde_json::Value {
    let hash = "a".repeat(64);
    serde_json::json!({
        "schemaVersion": 1, "kind": "fai-compiler", "version": "1.0.0", "tag": "v1.0.0",
        "commit": "b".repeat(40), "minimumInstaller": "0.1.0",
        "targets": {"x86_64-unknown-linux-gnu": {
            "minimumSystem": "glibc 2.35",
            "buildInfo": {"version": "1.0.0", "target": "x86_64-unknown-linux-gnu", "profile": "release",
                          "debugAssertions": false, "sourceId": hash, "toolBuildId": hash, "backendBuildId": hash},
            "compiler": {"name": "fai-1.0.0-x86_64-unknown-linux-gnu.tar.gz", "size": 100, "sha256": hash, "executableSha256": hash},
            "installer": {"name": "fai-installer-1.0.0-x86_64-unknown-linux-gnu.tar.gz", "size": 100, "sha256": hash, "executableSha256": hash}
        }}
    })
}

#[test]
fn release_describes_actual_host_identity() {
    serde_json::from_value::<Release>(manifest()).unwrap().validate().unwrap();
}

#[test]
fn wrong_compiler_target_is_rejected() {
    let mut value = manifest();
    value["targets"]["x86_64-unknown-linux-gnu"]["buildInfo"]["target"] =
        "aarch64-apple-darwin".into();
    assert!(serde_json::from_value::<Release>(value).unwrap().validate().is_err());
}

#[test]
fn archive_names_cannot_escape_the_release_download_path() {
    let mut value = manifest();
    value["targets"]["x86_64-unknown-linux-gnu"]["compiler"]["name"] = "../../other/file".into();
    assert!(serde_json::from_value::<Release>(value).unwrap().validate().is_err());
}

#[test]
fn development_profiles_are_rejected() {
    let mut value = manifest();
    value["targets"]["x86_64-unknown-linux-gnu"]["buildInfo"]["profile"] = "package-dev".into();
    assert!(serde_json::from_value::<Release>(value).unwrap().validate().is_err());
}

#[test]
fn mutable_channel_tags_are_rejected() {
    let mut value = manifest();
    value["tag"] = "stable".into();
    assert!(serde_json::from_value::<Release>(value).unwrap().validate().is_err());
}
