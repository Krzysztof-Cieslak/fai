//! Minimal pre-dispatch argument and project discovery, independent of compiler grammar.

use std::ffi::OsString;

use camino::{Utf8Path, Utf8PathBuf};
use fai_package::PackageVersion;

use crate::{Registry, Result, atomic_write, invalid, lock_file, validate_version};

/// Project configuration governing this invocation.
#[derive(Debug, Clone)]
pub struct Pin {
    /// Active root manifest, possibly an enclosing workspace manifest.
    pub manifest: Utf8PathBuf,
    /// Exact required release.
    pub version: Option<PackageVersion>,
}

/// Launcher-only interpretation; unknown compiler flags are forwarded unchanged.
#[derive(Debug)]
pub struct Invocation {
    /// Original arguments with only the leading +version removed.
    pub arguments: Vec<OsString>,
    /// Top-level command, if present.
    pub command: Option<String>,
    /// Directory from which project discovery starts.
    pub directory: Utf8PathBuf,
    /// An explicit -C root retains the compiler's legacy directory-session semantics.
    pub explicit_project: bool,
    /// Explicit per-invocation release.
    pub explicit: Option<PackageVersion>,
    /// Whether machine-readable output was requested.
    pub json: bool,
}

impl Invocation {
    /// Reads only selection and global options, stopping at the program argument separator.
    pub fn parse(arguments: Vec<OsString>, cwd: &Utf8Path) -> Result<Self> {
        let mut forwarded = vec![arguments.first().cloned().unwrap_or_else(|| "fai".into())];
        let mut command = None;
        let mut explicit = None;
        let mut project = None;
        let mut source = None;
        let mut json = false;
        let mut index = 1;
        while index < arguments.len() {
            let argument = &arguments[index];
            let text =
                argument.to_str().ok_or_else(|| invalid("compiler arguments must be UTF-8"))?;
            if text == "--" {
                forwarded.extend_from_slice(&arguments[index..]);
                break;
            }
            if command.is_none()
                && let Some(version) = text.strip_prefix('+')
            {
                if explicit.replace(validate_version(version)?).is_some() {
                    return Err(invalid("specify only one +version"));
                }
                index += 1;
                continue;
            }
            forwarded.push(argument.clone());
            if matches!(text, "-C" | "--project") {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| invalid("--project requires a directory"))?;
                project = Some(
                    cwd.join(value.to_str().ok_or_else(|| invalid("project path must be UTF-8"))?),
                );
                forwarded.push(value.clone());
            } else if let Some(value) = text
                .strip_prefix("--project=")
                .or_else(|| text.strip_prefix("-C").filter(|value| !value.is_empty()))
            {
                project = Some(cwd.join(value));
            } else if text == "--message-format=json" {
                json = true;
            } else if takes_value(text) {
                index += 1;
                let value = arguments
                    .get(index)
                    .ok_or_else(|| invalid(format!("{text} requires a value")))?;
                if text == "--message-format" {
                    json = value == "json";
                }
                forwarded.push(value.clone());
            } else if !text.starts_with('-') {
                if command.is_none() {
                    command = Some(text.to_owned());
                } else if source.is_none()
                    && matches!(
                        command.as_deref(),
                        Some("check" | "fmt" | "test" | "build" | "run")
                    )
                {
                    source = Some(cwd.join(text));
                }
            }
            index += 1;
        }
        let explicit_project = project.is_some();
        let directory = project.unwrap_or_else(|| {
            source
                .as_deref()
                .filter(|path| path.is_file())
                .and_then(Utf8Path::parent)
                .unwrap_or(cwd)
                .to_owned()
        });
        Ok(Self { arguments: forwarded, command, directory, explicit_project, explicit, json })
    }

    /// Chooses a version while treating an exact project pin as a hard constraint.
    pub fn selection(
        &self,
        registry: &Registry,
        environment: Option<&str>,
    ) -> Result<(PackageVersion, &'static str)> {
        let pin = if self.explicit_project && !self.directory.join("fai.toml").is_file() {
            None
        } else {
            discover_pin(&self.directory)?.and_then(|pin| pin.version)
        };
        let environment = if self.explicit.is_none() {
            environment.map(validate_version).transpose()?
        } else {
            None
        };
        let explicit = self.explicit.as_ref().or(environment.as_ref());
        if let (Some(explicit), Some(pin)) = (explicit, &pin)
            && explicit != pin
        {
            return Err(invalid(format!(
                "selected Fai {explicit} conflicts with project pin {pin}; change the pin explicitly with fai toolchain pin"
            )));
        }
        if let Some(version) = explicit {
            return Ok((
                version.clone(),
                if self.explicit.is_some() { "argument" } else { "environment" },
            ));
        }
        if let Some(version) = pin {
            return Ok((version, "project"));
        }
        registry.settings()?.default_version.map(|version| (version, "default"))
            .ok_or_else(|| invalid("no default compiler; run fai toolchain install stable, then fai toolchain default <version>, or run fai upgrade"))
    }
}

fn takes_value(flag: &str) -> bool {
    matches!(
        flag,
        "--message-format"
            | "--color"
            | "--protocol-log"
            | "--package"
            | "-p"
            | "--target"
            | "--bin"
            | "--out"
            | "--match"
            | "--seed"
            | "--count"
            | "--max-size"
            | "--timeout"
            | "--cpu-limit"
            | "--memory-limit"
    )
}

fn document(path: &Utf8Path) -> Result<toml::Value> {
    let text = std::fs::read_to_string(path)?;
    text.parse().map_err(|error| invalid(format!("{path}: {error}")))
}

/// Discovers the owning manifest and any enclosing workspace's compiler pin.
pub fn discover_pin(start: &Utf8Path) -> Result<Option<Pin>> {
    let start = start.canonicalize_utf8()?;
    let Some(root) = start.ancestors().find(|directory| directory.join("fai.toml").is_file())
    else {
        return Ok(None);
    };
    let mut manifest = root.join("fai.toml");
    let mut data = document(&manifest)?;
    if data.as_table().is_some_and(|table| table.keys().all(|key| key == "native")) {
        return Ok(Some(Pin { manifest, version: None }));
    }
    if data.get("workspace").is_none() {
        for ancestor in root.ancestors().skip(1) {
            let candidate = ancestor.join("fai.toml");
            if !candidate.is_file() {
                continue;
            }
            let parent = document(&candidate)?;
            let Some(workspace) = parent.get("workspace") else { continue };
            let members =
                workspace.get("members").and_then(toml::Value::as_array).ok_or_else(|| {
                    invalid(format!("{candidate}: workspace.members must be an array"))
                })?;
            let relative = root
                .strip_prefix(ancestor)
                .map_err(|error| invalid(error.to_string()))?
                .as_str()
                .replace('\\', "/");
            let mut member = false;
            for pattern in members {
                let pattern =
                    pattern.as_str().ok_or_else(|| invalid("workspace member must be a string"))?;
                let pattern = glob::Pattern::new(pattern.trim_start_matches("./"))
                    .map_err(|error| invalid(error.to_string()))?;
                member |= pattern.matches_with(
                    &relative,
                    glob::MatchOptions {
                        case_sensitive: true,
                        require_literal_separator: true,
                        require_literal_leading_dot: true,
                    },
                );
            }
            if member {
                manifest = candidate;
                data = parent;
            }
            break;
        }
    }
    let version = match data.get("toolchain") {
        None => None,
        Some(table) => Some(validate_version(
            table.get("version").and_then(toml::Value::as_str).ok_or_else(|| {
                invalid(format!("{manifest}: toolchain.version must be an exact version string"))
            })?,
        )?),
    };
    Ok(Some(Pin { manifest, version }))
}

/// Edits only the active root's toolchain version, preserving authored TOML formatting.
pub fn pin_project(directory: &Utf8Path, version: &PackageVersion) -> Result<Utf8PathBuf> {
    let pin = discover_pin(directory)?
        .ok_or_else(|| invalid("no fai.toml found; create a project before pinning a compiler"))?;
    let root = pin.manifest.parent().ok_or_else(|| invalid("manifest has no parent"))?;
    let _lock = lock_file(&root.join(".fai/metadata.lock"))?;
    let before = std::fs::read_to_string(&pin.manifest)?;
    let mut document =
        before.parse::<toml_edit::DocumentMut>().map_err(|error| invalid(error.to_string()))?;
    if document.get("manifest-version").and_then(toml_edit::Item::as_integer) != Some(1) {
        return Err(invalid("compiler pins require a versioned project manifest"));
    }
    document["toolchain"]["version"] = toml_edit::value(version.to_string());
    if std::fs::read_to_string(&pin.manifest)? != before {
        return Err(invalid("manifest changed while preparing the compiler pin; retry"));
    }
    atomic_write(&pin.manifest, document.to_string().as_bytes())?;
    Ok(pin.manifest)
}
