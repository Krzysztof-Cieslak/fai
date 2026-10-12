//! Explicit registration and deterministic selection of already installed compilers.

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::PackageVersion;
use serde::{Deserialize, Serialize};

use crate::DriverError;

/// One verified installed executable, separate from package source resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstalledCompiler {
    /// Compiler release version.
    pub version: PackageVersion,
    /// Executable's host target.
    pub target: String,
    /// Absolute executable path.
    pub executable: Utf8PathBuf,
    /// Complete build identity reported by the executable.
    pub build_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Inventory {
    version: u32,
    compilers: Vec<InstalledCompiler>,
}

fn directory() -> Result<Utf8PathBuf, DriverError> {
    let path = if let Some(path) = std::env::var_os("FAI_TOOLCHAIN_DIR") {
        std::path::PathBuf::from(path)
    } else if let Some(home) = std::env::var_os("FAI_HOME") {
        std::path::PathBuf::from(home).join("toolchains")
    } else if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
    {
        std::path::PathBuf::from(home).join(".fai/toolchains")
    } else {
        return Err(unavailable(
            "set FAI_TOOLCHAIN_DIR to an installed-compiler inventory directory",
        ));
    };
    Utf8PathBuf::from_path_buf(path).map_err(|_| unavailable("toolchain directory is not UTF-8"))
}

/// Lists explicitly registered host executables. No installation is performed.
pub fn installed_compilers() -> Result<Vec<InstalledCompiler>, DriverError> {
    read_inventory(&directory()?)
}

fn read_inventory(directory: &Utf8Path) -> Result<Vec<InstalledCompiler>, DriverError> {
    let text = match std::fs::read(directory.join("installed.json")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(unavailable(error.to_string())),
    };
    let inventory: Inventory =
        serde_json::from_slice(&text).map_err(|error| unavailable(error.to_string()))?;
    if inventory.version != 1 {
        return Err(unavailable("unsupported installed compiler inventory"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for compiler in &inventory.compilers {
        if !compiler.executable.is_absolute()
            || !seen.insert((compiler.version.clone(), compiler.target.clone()))
        {
            return Err(unavailable("invalid or ambiguous installed compiler inventory"));
        }
    }
    Ok(inventory.compilers)
}

fn probe(path: &Utf8Path) -> Result<InstalledCompiler, DriverError> {
    let executable = path.canonicalize_utf8().map_err(|error| unavailable(error.to_string()))?;
    let output = std::process::Command::new(&executable)
        .arg("build-info")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|error| unavailable(error.to_string()))?;
    if !output.status.success() {
        return Err(unavailable("installed executable did not report compiler build information"));
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|error| unavailable(error.to_string()))?;
    let field = |name| {
        metadata
            .get(name)
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| unavailable(format!("compiler build information is missing {name}")))
    };
    let version =
        PackageVersion::parse(field("version")?).map_err(|error| unavailable(error.to_string()))?;
    Ok(InstalledCompiler {
        version,
        target: field("target")?.into(),
        executable,
        build_id: field("toolBuildId")?.into(),
    })
}

/// Registers one existing compiler. Re-registration replaces that release/host entry.
pub fn register_compiler(executable: &Utf8Path) -> Result<InstalledCompiler, DriverError> {
    let compiler = probe(executable)?;
    let directory = directory()?;
    let _lock = fai_package_store::lock_file(&directory.join("inventory.lock"))?;
    let mut compilers = read_inventory(&directory)?;
    compilers.retain(|old| old.version != compiler.version || old.target != compiler.target);
    compilers.push(compiler.clone());
    compilers.sort_by(|a, b| (&a.version, &a.target).cmp(&(&b.version, &b.target)));
    fai_package_store::atomic_write(
        &directory.join("installed.json"),
        &serde_json::to_vec_pretty(&Inventory { version: 1, compilers })
            .map_err(|error| unavailable(error.to_string()))?,
    )?;
    Ok(compiler)
}

/// Selects and re-verifies an installed exact release for this host, without downloads.
pub fn select_compiler(version: &PackageVersion) -> Result<InstalledCompiler, DriverError> {
    let host = crate::build_info().target;
    let compiler = installed_compilers()?.into_iter().find(|compiler| &compiler.version == version && compiler.target == host)
        .ok_or_else(|| unavailable(format!("Fai {version} for {host} is not registered; install it explicitly and run fai toolchain register <executable>")))?;
    let current = probe(&compiler.executable)?;
    if current != compiler {
        return Err(unavailable(
            "registered compiler changed; verify it and register the executable again",
        ));
    }
    Ok(compiler)
}

fn unavailable(message: impl Into<String>) -> DriverError {
    fai_package::ProviderFailure { code: fai_package::COMPILER_MISMATCH, message: message.into() }
        .into()
}
