//! Deterministic identity for the compiler sources and build configuration.

/// Hashes named source contents and build settings without timestamps or absolute
/// checkout paths. Input ordering does not affect the identity.
pub fn fingerprint(
    files: &[(String, Vec<u8>)],
    settings: &[(String, String)],
    rustc_version: &str,
) -> String {
    fn field(hasher: &mut blake3::Hasher, bytes: &[u8]) {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    let mut hasher = blake3::Hasher::new();
    field(&mut hasher, b"fai-compiler-build-v1");
    field(&mut hasher, rustc_version.as_bytes());
    let mut files: Vec<_> = files.iter().collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    field(&mut hasher, &(files.len() as u64).to_le_bytes());
    for (name, bytes) in files {
        field(&mut hasher, name.as_bytes());
        field(&mut hasher, bytes);
    }
    let mut settings: Vec<_> = settings.iter().collect();
    settings.sort();
    for (name, value) in settings {
        field(&mut hasher, name.as_bytes());
        field(&mut hasher, value.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_changes_change_the_build_identity() {
        let a = vec![("src/emit.rs".into(), b"old".to_vec())];
        let b = vec![("src/emit.rs".into(), b"new".to_vec())];
        assert_ne!(fingerprint(&a, &[], "rustc"), fingerprint(&b, &[], "rustc"));
    }

    #[test]
    fn source_and_setting_order_do_not_change_identity() {
        let files = vec![("b.rs".into(), vec![2]), ("a.rs".into(), vec![1])];
        let settings = vec![("B".into(), "2".into()), ("A".into(), "1".into())];
        let mut reverse_files = files.clone();
        reverse_files.reverse();
        let mut reverse_settings = settings.clone();
        reverse_settings.reverse();
        assert_eq!(
            fingerprint(&files, &settings, "rustc"),
            fingerprint(&reverse_files, &reverse_settings, "rustc")
        );
    }

    #[test]
    fn build_settings_change_identity() {
        assert_ne!(
            fingerprint(&[], &[("debug".into(), "true".into())], "rustc"),
            fingerprint(&[], &[("debug".into(), "false".into())], "rustc")
        );
    }

    #[test]
    fn compiler_toolchain_changes_identity() {
        assert_ne!(fingerprint(&[], &[], "rustc-a"), fingerprint(&[], &[], "rustc-b"));
    }
}
