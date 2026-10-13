//! Backwards-compatible inventory and atomic settings for installed compilers.

use std::{fs::File, io::Write, path::Path, process::Command};

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::PackageVersion;
use serde::{Deserialize, Serialize};

use crate::{Result, digest, invalid, validate_version};

/// One verified compiler. The fields remain compatible with inventory version one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstalledCompiler {
    /// Compiler release.
    pub version: PackageVersion,
    /// Native target triple.
    pub target: String,
    /// Canonical executable path.
    pub executable: Utf8PathBuf,
    /// Complete compiler build identity.
    pub build_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    version: u32,
    compilers: Vec<InstalledCompiler>,
}

/// Launcher preferences are separate from the older compiler inventory schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// Settings wire version.
    pub schema_version: u32,
    /// Exact selected release, never a moving channel.
    pub default_version: Option<PackageVersion>,
}

impl Default for Settings {
    fn default() -> Self {
        Self { schema_version: 1, default_version: None }
    }
}

/// User-owned installation roots, also injectable for isolated tests.
#[derive(Debug, Clone)]
pub struct Registry {
    /// Launcher and settings directory.
    pub home: Utf8PathBuf,
    /// Compiler payloads and compatible inventory directory.
    pub directory: Utf8PathBuf,
}

impl Registry {
    /// Resolves existing FAI_HOME and FAI_TOOLCHAIN_DIR conventions.
    pub fn from_env() -> Result<Self> {
        let home = std::env::var_os("FAI_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|home| std::path::PathBuf::from(home).join(".fai"))
            })
            .or_else(|| {
                std::env::var_os("FAI_TOOLCHAIN_DIR").and_then(|directory| {
                    std::path::PathBuf::from(directory).parent().map(Path::to_owned)
                })
            })
            .ok_or_else(|| invalid("set FAI_HOME to an installation directory"))?;
        let home = absolute(&home)?;
        let directory = match std::env::var_os("FAI_TOOLCHAIN_DIR") {
            Some(path) => absolute(Path::new(&path))?,
            None => home.join("toolchains"),
        };
        Ok(Self { home, directory })
    }

    /// Acquires the shared inventory/settings mutation lock.
    pub fn lock(&self) -> Result<File> {
        lock_file(&self.directory.join("inventory.lock"))
    }

    /// Reads and validates the inventory without probing or downloading executables.
    pub fn list(&self) -> Result<Vec<InstalledCompiler>> {
        let bytes = match std::fs::read(self.directory.join("installed.json")) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let inventory: Inventory = serde_json::from_slice(&bytes)?;
        if inventory.version != 1 {
            return Err(invalid("unsupported installed compiler inventory"));
        }
        let mut seen = std::collections::BTreeSet::new();
        for item in &inventory.compilers {
            if !item.executable.is_absolute() || !seen.insert((&item.version, &item.target)) {
                return Err(invalid("invalid or ambiguous installed compiler inventory"));
            }
        }
        Ok(inventory.compilers)
    }

    /// Publishes a complete inventory. The caller must hold the inventory lock.
    pub fn write(&self, mut compilers: Vec<InstalledCompiler>) -> Result<()> {
        compilers.sort_by(|a, b| (&a.version, &a.target).cmp(&(&b.version, &b.target)));
        atomic_write(
            &self.directory.join("installed.json"),
            &serde_json::to_vec_pretty(&Inventory { version: 1, compilers })?,
        )
    }

    /// Explicitly registers a verified executable, replacing its release/host entry.
    pub fn register(&self, path: &Utf8Path) -> Result<InstalledCompiler> {
        let compiler = probe(path)?;
        let _lock = self.lock()?;
        let mut entries = self.list()?;
        entries
            .retain(|entry| entry.version != compiler.version || entry.target != compiler.target);
        entries.push(compiler.clone());
        self.write(entries)?;
        Ok(compiler)
    }

    /// Selects a registered exact host compiler and revalidates its reported identity.
    pub fn select(&self, version: &PackageVersion, host: &str) -> Result<InstalledCompiler> {
        let compiler = self
            .list()?
            .into_iter()
            .find(|entry| &entry.version == version && entry.target == host)
            .ok_or_else(|| {
                invalid(format!(
                    "Fai {version} for {host} is not installed; run fai toolchain install {version}"
                ))
            })?;
        if probe(&compiler.executable)? != compiler {
            return Err(invalid(
                "registered compiler changed; verify it and run fai toolchain register again",
            ));
        }
        Ok(compiler)
    }

    /// Reads the exact global default independently of inventory format.
    pub fn settings(&self) -> Result<Settings> {
        let settings: Settings = match std::fs::read(self.home.join("settings.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Settings::default(),
            Err(error) => return Err(error.into()),
        };
        if settings.schema_version != 1 {
            return Err(invalid("unsupported installer settings"));
        }
        Ok(settings)
    }

    /// Publishes a default after verifying it is installed. Does not alter project pins.
    pub fn set_default(&self, version: &PackageVersion, host: &str) -> Result<()> {
        let _lock = self.lock()?;
        self.select(version, host)?;
        atomic_write(
            &self.home.join("settings.json"),
            &serde_json::to_vec_pretty(&Settings {
                schema_version: 1,
                default_version: Some(version.clone()),
            })?,
        )
    }

    /// Initializes the first default without racing another installer or selection.
    pub fn initialize_default(&self, version: &PackageVersion, host: &str) -> Result<()> {
        let _lock = self.lock()?;
        if self.settings()?.default_version.is_some() {
            return Ok(());
        }
        self.select(version, host)?;
        atomic_write(
            &self.home.join("settings.json"),
            &serde_json::to_vec_pretty(&Settings {
                schema_version: 1,
                default_version: Some(version.clone()),
            })?,
        )
    }

    /// Stable, external lease path which survives moving a compiler payload.
    pub fn lease_path(&self, executable: &Utf8Path) -> Utf8PathBuf {
        self.directory
            .join("leases")
            .join(format!("{}.lock", digest(executable.as_str().as_bytes())))
    }
}

fn absolute(path: &Path) -> Result<Utf8PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(invalid("installation directories must not be empty"));
    }
    let path = std::path::absolute(path)?;
    Utf8PathBuf::from_path_buf(path).map_err(|_| invalid("installation path must be UTF-8"))
}

/// Probes a real compiler without opening a project or allowing launcher recursion.
pub fn probe(path: &Utf8Path) -> Result<InstalledCompiler> {
    let executable = path.canonicalize_utf8()?;
    let output = Command::new(&executable)
        .arg("build-info")
        .env("FAI_TOOLCHAIN_PROBE", "1")
        .stdin(std::process::Stdio::null())
        .output()?;
    if !output.status.success() {
        return Err(invalid(format!("{} did not report compiler build information", executable)));
    }
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let field = |name| {
        info[name]
            .as_str()
            .ok_or_else(|| invalid(format!("compiler build information is missing {name}")))
    };
    Ok(InstalledCompiler {
        version: validate_version(field("version")?)?,
        target: field("target")?.into(),
        executable,
        build_id: field("toolBuildId")?.into(),
    })
}

/// An exclusive advisory file lock, released on drop.
pub fn lock_file(path: &Utf8Path) -> Result<File> {
    let file = open_lock(path)?;
    file.lock()?;
    Ok(file)
}

/// Opens a lease/lock without truncating its inode.
pub fn open_lock(path: &Utf8Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(File::options().read(true).write(true).create(true).truncate(false).open(path)?)
}

/// Holds a managed compiler's shared lease for the entire process, including workers.
pub fn lease_current() -> Result<Option<File>> {
    let executable = std::env::current_exe()?;
    let executable = Utf8PathBuf::from_path_buf(executable)
        .map_err(|_| invalid("compiler path must be UTF-8"))?
        .canonicalize_utf8()?;
    let Some(root) = executable.parent().and_then(Utf8Path::parent) else { return Ok(None) };
    if !root.join("receipt.json").is_file() {
        return Ok(None);
    }
    let receipt: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("receipt.json"))?)?;
    if receipt["compiler"]["executable"].as_str() != Some(executable.as_str()) {
        return Err(invalid(
            "compiler installation moved while starting; retry with an installed version",
        ));
    }
    let registry = Registry::from_env()?;
    let file = open_lock(&registry.lease_path(&executable))?;
    file.lock_shared()?;
    if !executable.is_file() {
        return Err(invalid(
            "compiler was uninstalled while starting; retry with an installed version",
        ));
    }
    Ok(Some(file))
}

/// Synchronized temporary sibling followed by an atomic replacement.
pub fn atomic_write(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("output has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    if let Ok(metadata) = std::fs::metadata(path) {
        temporary.as_file().set_permissions(metadata.permissions())?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
