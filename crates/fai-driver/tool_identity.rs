//! Whole-tool source identity shared with the portable compiler bundle tooling.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Collects every declared input, including new files and optional Cargo config.
pub fn inputs(root: &Path) -> Vec<PathBuf> {
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("scripts/compiler-inputs.json")).expect("compiler input manifest"),
    )
    .expect("valid compiler input manifest");
    assert_eq!(manifest["schemaVersion"], 1);
    let strings = |name: &str| -> Vec<String> {
        manifest[name]
            .as_array()
            .expect("input manifest array")
            .iter()
            .map(|value| value.as_str().expect("input manifest string").to_owned())
            .collect()
    };
    let ignored = strings("ignoredDirectories");
    let suffixes = strings("ignoredSuffixes");
    let mut paths: Vec<_> = strings("files").iter().map(|name| root.join(name)).collect();
    for tree in strings("trees") {
        let directory = root.join(tree);
        if directory.is_dir() {
            walk(&directory, &ignored, &suffixes, &mut paths);
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

fn walk(directory: &Path, ignored: &[String], suffixes: &[String], paths: &mut Vec<PathBuf>) {
    println!("cargo:rerun-if-changed={}", directory.display());
    for entry in std::fs::read_dir(directory).expect("compiler input directory") {
        let path = entry.expect("compiler input entry").path();
        let name = path.file_name().expect("file name").to_string_lossy();
        if path.is_dir() {
            if !ignored.iter().any(|ignored| ignored == &name) {
                assert!(!path.is_symlink(), "compiler source directories must not be symlinks");
                walk(&path, ignored, suffixes, paths);
            }
        } else if !suffixes.iter().any(|suffix| name.ends_with(suffix)) {
            assert!(!path.is_symlink(), "compiler source files must not be symlinks");
            paths.push(path);
        }
    }
}

/// Hashes sorted relative UTF-8 paths and raw contents with length framing.
/// `scripts/compiler.py` implements the same platform-independent wire format.
pub fn source_id(files: &[(String, Vec<u8>)]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"fai-tool-sources-v1\0");
    let mut files: Vec<_> = files.iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, bytes) in files {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())
}

/// Locates non-system import/static libraries that must travel with the compiler.
pub fn native_libraries(flags: &str, directories: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut found = std::collections::BTreeMap::new();
    for flag in flags.split_whitespace() {
        let name = if flag.ends_with(".lib") {
            flag.to_owned()
        } else if let Some(name) = flag.strip_prefix("-l:") {
            name.to_owned()
        } else if let Some(name) = flag.strip_prefix("-l") {
            format!("lib{name}.a")
        } else {
            continue;
        };
        assert_eq!(Path::new(&name).file_name().and_then(|v| v.to_str()), Some(name.as_str()));
        if let Some(path) =
            directories.iter().map(|dir| dir.join(&name)).find(|path| path.is_file())
        {
            found.entry(name).or_insert(path);
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_inputs_include_cli_and_exclude_source_packages() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap();
        let paths = inputs(&root);
        assert!(paths.contains(&root.join("crates/fai-cli/src/lib.rs")));
        assert!(paths.contains(&root.join("std/core/Prelude.fai")));
        assert!(!paths.iter().any(|path| path.starts_with(root.join("packages"))));
    }

    #[test]
    fn source_fingerprint_is_order_independent_and_content_sensitive() {
        let a = ("a".into(), b"one".to_vec());
        let b = ("b".into(), b"two".to_vec());
        assert_eq!(source_id(&[a.clone(), b.clone()]), source_id(&[b.clone(), a.clone()]));
        assert_ne!(source_id(&[a]), source_id(&[b]));
    }

    #[test]
    fn native_import_libraries_follow_search_order_and_skip_system_libraries() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(first.join("windows.lib"), "first").unwrap();
        std::fs::write(second.join("windows.lib"), "second").unwrap();
        assert_eq!(
            native_libraries("kernel32.lib windows.lib windows.lib", &[first.clone(), second]),
            vec![("windows.lib".into(), first.join("windows.lib"))]
        );
    }
}
