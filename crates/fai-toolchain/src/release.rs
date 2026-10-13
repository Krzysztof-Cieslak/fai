//! Versioned public release metadata and content verification.

use std::{collections::BTreeMap, io::Read, path::Path};

use fai_package::PackageVersion;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Result, invalid};

/// Rejects noncanonical versions and build metadata, matching project compiler pins.
pub fn validate_version(value: &str) -> Result<PackageVersion> {
    let version = PackageVersion::parse(value).map_err(|error| invalid(error.to_string()))?;
    if version.to_string() != value {
        return Err(invalid("compiler versions must use canonical major.minor.patch notation"));
    }
    Ok(version)
}

/// Lowercase SHA-256 of bytes.
pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Streams a file through SHA-256 without retaining the file in memory.
pub fn file_digest(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// An archive and its executable, both identified by their final shipped bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    /// Single release asset filename.
    pub name: String,
    /// Archive byte length.
    pub size: u64,
    /// Archive SHA-256.
    pub sha256: String,
    /// Executable SHA-256 after extraction.
    pub executable_sha256: String,
}

/// Complete artifacts and compatibility declaration for one native host.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetRelease {
    /// Tested operating-system or libc baseline.
    pub minimum_system: String,
    /// Standalone compiler archive.
    pub compiler: Artifact,
    /// Native launcher archive.
    pub installer: Artifact,
    /// Metadata emitted by the real compiler.
    pub build_info: serde_json::Value,
}

/// The immutable release manifest downloaded from a GitHub release.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Release {
    /// Wire format, currently one.
    pub schema_version: u32,
    /// Distinguishes compiler releases from source-package releases.
    pub kind: String,
    /// Exact compiler version.
    pub version: String,
    /// Canonical annotated tag.
    pub tag: String,
    /// Full source commit.
    pub commit: String,
    /// Oldest installer able to consume this release.
    pub minimum_installer: String,
    /// Native target triples, ordered deterministically.
    pub targets: BTreeMap<String, TargetRelease>,
}

impl Release {
    /// Validates names, identities, checksums, and supported schema before any download.
    pub fn validate(&self) -> Result<()> {
        validate_version(&self.version)?;
        validate_version(&self.minimum_installer)?;
        if self.schema_version != 1 || self.kind != "fai-compiler" {
            return Err(invalid("unsupported release manifest; run fai installer upgrade"));
        }
        if self.tag != format!("v{}", self.version)
            || self.commit.len() != 40
            || !self.commit.bytes().all(|byte| byte.is_ascii_hexdigit())
            || self.targets.is_empty()
        {
            return Err(invalid("invalid compiler release identity"));
        }
        for (host, target) in &self.targets {
            if !matches!(
                host.as_str(),
                "x86_64-unknown-linux-gnu"
                    | "aarch64-unknown-linux-gnu"
                    | "x86_64-apple-darwin"
                    | "aarch64-apple-darwin"
                    | "x86_64-pc-windows-msvc"
                    | "aarch64-pc-windows-msvc"
            ) {
                return Err(invalid(format!("unsupported release host: {host}")));
            }
            if target.build_info["version"] != self.version
                || target.build_info["target"] != *host
                || target.build_info["profile"] != "release"
                || target.build_info["debugAssertions"] != false
                || target.minimum_system.is_empty()
            {
                return Err(invalid(format!(
                    "compiler metadata disagrees with release for {host}"
                )));
            }
            for field in ["sourceId", "toolBuildId", "backendBuildId"] {
                if !target.build_info[field].as_str().is_some_and(checksum) {
                    return Err(invalid(format!("invalid compiler {field}")));
                }
            }
            let extension = if host.contains("windows") { "zip" } else { "tar.gz" };
            for (kind, artifact) in
                [("fai", &target.compiler), ("fai-installer", &target.installer)]
            {
                if artifact.name != format!("{kind}-{}-{host}.{extension}", self.version)
                    || artifact.size == 0
                    || artifact.size > 2 * 1024 * 1024 * 1024
                    || !checksum(&artifact.sha256)
                    || !checksum(&artifact.executable_sha256)
                {
                    return Err(invalid(format!("invalid {kind} artifact for {host}")));
                }
            }
        }
        Ok(())
    }
}

fn checksum(value: &str) -> bool {
    value.len() == 64
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
