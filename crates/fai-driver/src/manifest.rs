//! The optional per-project native-dependency manifest (`fai.toml`).
//!
//! A program that calls user `foreign` functions declares the native libraries
//! and object files to link (AOT) or load (JIT) in a `fai.toml` at the workspace
//! root:
//!
//! ```toml
//! [native]
//! library-dirs = ["native"]    # added as `-L` search paths
//! libraries = ["mymath"]       # linked `-lmymath`; loaded as `libmymath.<ext>`
//! objects = ["native/extra.o"] # extra object/archive files (AOT only)
//! ```
//!
//! Relative paths are resolved against the workspace root. The manifest is the one
//! place a build's native dependencies are declared, so the same list drives AOT
//! linking and the JIT's dynamic loading.

use std::path::PathBuf;

use camino::Utf8Path;
use serde::Deserialize;

/// The name of the per-project native-dependency manifest.
pub const MANIFEST_NAME: &str = "fai.toml";

/// Native dependencies declared in `fai.toml`, resolved against the project root.
#[derive(Debug, Default, Clone)]
pub struct NativeDeps {
    /// Library search directories (absolute), passed as `-L` and used to locate a
    /// shared library to load.
    pub lib_dirs: Vec<PathBuf>,
    /// Library names, passed as `-l` (AOT) and resolved to a shared-library file to
    /// load (JIT).
    pub libs: Vec<String>,
    /// Extra object/archive files (absolute), linked directly (AOT only).
    pub objects: Vec<PathBuf>,
}

impl NativeDeps {
    /// Whether the project declares no native dependencies (the common case — no
    /// `fai.toml`, or an empty `[native]` section).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lib_dirs.is_empty() && self.libs.is_empty() && self.objects.is_empty()
    }
}

#[derive(Deserialize, Default)]
struct Manifest {
    #[serde(default)]
    native: NativeSection,
}

#[derive(Deserialize, Default)]
struct NativeSection {
    #[serde(default, rename = "library-dirs")]
    library_dirs: Vec<String>,
    #[serde(default)]
    libraries: Vec<String>,
    #[serde(default)]
    objects: Vec<String>,
}

/// Reads `<root>/fai.toml`, returning its native dependencies (empty when the file
/// is absent). Relative paths are resolved against `root`. A malformed manifest is
/// an error string.
pub fn read_native_manifest(root: &Utf8Path) -> Result<NativeDeps, String> {
    let path = root.join(MANIFEST_NAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(NativeDeps::default()),
        Err(e) => return Err(format!("reading {path}: {e}")),
    };
    let manifest: Manifest = toml::from_str(&text).map_err(|e| format!("parsing {path}: {e}"))?;
    let resolve = |p: &str| -> PathBuf {
        let pb = PathBuf::from(p);
        if pb.is_absolute() { pb } else { root.as_std_path().join(pb) }
    };
    Ok(NativeDeps {
        lib_dirs: manifest.native.library_dirs.iter().map(|d| resolve(d)).collect(),
        libs: manifest.native.libraries,
        objects: manifest.native.objects.iter().map(|o| resolve(o)).collect(),
    })
}
