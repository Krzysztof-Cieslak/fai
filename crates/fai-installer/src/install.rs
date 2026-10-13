//! Recoverable compiler installation and ownership-aware removal.

use std::{fs::File, io::Write};

use camino::{Utf8Path, Utf8PathBuf};
use fai_toolchain::{
    InstalledCompiler, Registry, Result, atomic_write, file_digest, host, invalid, lock_file,
    open_lock, probe, validate_version,
};
use serde::{Deserialize, Serialize};

use crate::{archive, network::Client};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Receipt {
    schema_version: u32,
    compiler: InstalledCompiler,
    commit: String,
    archive_sha256: String,
    executable_sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
enum Pending {
    Install { receipt: Receipt, stage: String, previous: Option<InstalledCompiler> },
    Remove { compiler: InstalledCompiler, trash: String },
}

pub fn executable() -> &'static str {
    if cfg!(windows) { "fai.exe" } else { "fai" }
}

fn slot(registry: &Registry, version: &str, target: &str) -> Result<Utf8PathBuf> {
    validate_version(version)?;
    if target != host() {
        return Err(invalid("toolchain does not match the native installer host"));
    }
    std::fs::create_dir_all(&registry.directory)?;
    Ok(registry.directory.canonicalize_utf8()?.join(format!("{version}-{target}")))
}

fn journal(registry: &Registry) -> Utf8PathBuf {
    registry.directory.join("transaction.json")
}

fn read_ownership(directory: &Utf8Path) -> Result<Receipt> {
    if std::fs::symlink_metadata(directory)?.file_type().is_symlink() {
        return Err(invalid("managed compiler directories must not be symbolic links"));
    }
    let receipt: Receipt = serde_json::from_slice(&std::fs::read(directory.join("receipt.json"))?)?;
    if receipt.schema_version != 1 || receipt.compiler.target != host() {
        return Err(invalid("invalid toolchain ownership receipt"));
    }
    Ok(receipt)
}

fn read_receipt(directory: &Utf8Path) -> Result<Receipt> {
    let receipt = read_ownership(directory)?;
    let binary = directory.join("bin").join(executable());
    if std::fs::symlink_metadata(&binary)?.file_type().is_symlink()
        || file_digest(binary.as_std_path())? != receipt.executable_sha256
    {
        return Err(invalid("installed compiler checksum mismatch"));
    }
    let actual = probe(&binary)?;
    if actual.version != receipt.compiler.version
        || actual.target != receipt.compiler.target
        || actual.build_id != receipt.compiler.build_id
    {
        return Err(invalid("installed compiler identity mismatch"));
    }
    Ok(receipt)
}

fn staged_path(registry: &Registry, name: &str) -> Result<Utf8PathBuf> {
    if !name.starts_with(".fai-") || name.contains(['/', '\\', ':']) || name.contains("..") {
        return Err(invalid("invalid installation transaction path"));
    }
    Ok(registry.directory.join(name))
}

pub fn recover(registry: &Registry) -> Result<()> {
    let _lock = registry.lock()?;
    recover_locked(registry)
}

fn recover_locked(registry: &Registry) -> Result<()> {
    recover_with_lease(registry, false)
}

fn recover_with_lease(registry: &Registry, lease_held: bool) -> Result<()> {
    let bytes = match std::fs::read(journal(registry)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let pending: Pending = serde_json::from_slice(&bytes)?;
    let mut entries = registry.list()?;
    match pending {
        Pending::Install { receipt, stage, previous } => {
            let compiler = &receipt.compiler;
            let destination = slot(registry, &compiler.version.to_string(), &compiler.target)?;
            if compiler.executable != destination.join("bin").join(executable()) {
                return Err(invalid("transaction executable escapes its toolchain directory"));
            }
            let current = entries
                .iter()
                .find(|entry| entry.version == compiler.version && entry.target == compiler.target);
            if current != previous.as_ref() && current != Some(compiler) {
                return Err(invalid(
                    "compiler registration changed during installation; transaction retained",
                ));
            }
            let stage = staged_path(registry, &stage)?;
            if !destination.exists() {
                let actual = read_receipt(&stage)?;
                if actual.executable_sha256 != receipt.executable_sha256
                    || actual.compiler != receipt.compiler
                {
                    return Err(invalid("staged compiler does not match transaction"));
                }
                std::fs::rename(&stage, &destination)?;
            }
            let actual = read_receipt(&destination)?;
            if actual.compiler != *compiler || actual.executable_sha256 != receipt.executable_sha256
            {
                return Err(invalid("destination does not match installation transaction"));
            }
            entries.retain(|entry| {
                entry.version != compiler.version || entry.target != compiler.target
            });
            entries.push(compiler.clone());
            registry.write(entries)?;
            if stage.exists() {
                std::fs::remove_dir_all(stage)?;
            }
        }
        Pending::Remove { compiler, trash } => {
            let destination = slot(registry, &compiler.version.to_string(), &compiler.target)?;
            if compiler.executable != destination.join("bin").join(executable()) {
                return Err(invalid(
                    "removal transaction executable escapes its toolchain directory",
                ));
            }
            let current = entries
                .iter()
                .find(|entry| entry.version == compiler.version && entry.target == compiler.target);
            if current.is_some_and(|entry| entry != &compiler) {
                return Err(invalid("registration changed during removal; transaction retained"));
            }
            let lease = open_lock(&registry.lease_path(&compiler.executable))?;
            if !lease_held {
                lease.try_lock().map_err(|_| {
                    invalid("toolchain is in use; stop its daemons and language servers before removing it")
                })?;
            }
            let trash = staged_path(registry, &trash)?;
            if destination.exists() {
                if read_ownership(&destination)?.compiler != compiler {
                    return Err(invalid("toolchain ownership changed during removal"));
                }
                std::fs::rename(&destination, &trash)?;
            }
            entries.retain(|entry| entry != &compiler);
            registry.write(entries)?;
            if trash.exists() {
                if std::fs::symlink_metadata(&trash)?.file_type().is_symlink() {
                    return Err(invalid(
                        "removal staging directory was replaced by a symbolic link",
                    ));
                }
                if trash.join("receipt.json").exists()
                    && read_ownership(&trash)?.compiler != compiler
                {
                    return Err(invalid("removed compiler ownership changed; files preserved"));
                }
                std::fs::remove_dir_all(trash)?;
            }
        }
    }
    std::fs::remove_file(journal(registry))?;
    Ok(())
}

pub fn install(registry: &Registry, version: &str) -> Result<InstalledCompiler> {
    recover(registry)?;
    let client = Client::new()?;
    let release = client.release(version)?;
    if validate_version(env!("CARGO_PKG_VERSION"))? < validate_version(&release.minimum_installer)?
    {
        return Err(invalid("this compiler requires a newer installer; run fai installer upgrade"));
    }
    let target = release.targets.get(&host()).ok_or_else(|| {
        invalid(format!("release {} has no compiler for {}", release.version, host()))
    })?;
    require_system(&target.minimum_system)?;
    let destination = slot(registry, &release.version, &host())?;
    let _version_lock = lock_file(
        &registry
            .directory
            .join(format!(".{}.lock", destination.file_name().unwrap_or("compiler"))),
    )?;
    if destination.exists() {
        let receipt = read_receipt(&destination)?;
        if receipt.archive_sha256 != target.compiler.sha256 || receipt.commit != release.commit {
            return Err(invalid("published release changed; existing compiler preserved"));
        }
        let _lock = registry.lock()?;
        let mut entries = registry.list()?;
        if let Some(current) = entries.iter().find(|entry| {
            entry.version == receipt.compiler.version && entry.target == receipt.compiler.target
        }) {
            if current != &receipt.compiler {
                return Err(invalid(
                    "this version has an external registration; unregister it before installing",
                ));
            }
        } else {
            entries.push(receipt.compiler.clone());
            registry.write(entries)?;
        }
        return Ok(receipt.compiler);
    }
    eprintln!("Downloading Fai {} for {}", release.version, host());
    let archive = client.download(&release, &target.compiler)?;
    let staging =
        tempfile::Builder::new().prefix(".fai-install-").tempdir_in(&registry.directory)?;
    archive::extract(
        archive.path(),
        staging.path(),
        cfg!(windows),
        &target.compiler.executable_sha256,
    )?;
    let staged =
        Utf8Path::from_path(staging.path()).ok_or_else(|| invalid("staging path must be UTF-8"))?;
    let mut compiler = probe(&staged.join("bin").join(executable()))?;
    if compiler.version.to_string() != release.version
        || compiler.target != host()
        || target.build_info["toolBuildId"].as_str() != Some(&compiler.build_id)
    {
        return Err(invalid("downloaded compiler does not match the release manifest"));
    }
    compiler.executable = destination.join("bin").join(executable());
    let receipt = Receipt {
        schema_version: 1,
        compiler: compiler.clone(),
        commit: release.commit,
        archive_sha256: target.compiler.sha256.clone(),
        executable_sha256: target.compiler.executable_sha256.clone(),
    };
    atomic_write(&staged.join("receipt.json"), &serde_json::to_vec_pretty(&receipt)?)?;
    let _lock = registry.lock()?;
    recover_locked(registry)?;
    let previous = registry
        .list()?
        .into_iter()
        .find(|entry| entry.version == compiler.version && entry.target == compiler.target);
    if previous.is_some() {
        return Err(invalid("this version is already registered; unregister it before installing"));
    }
    let stage = staged.file_name().ok_or_else(|| invalid("invalid staging directory"))?.to_owned();
    atomic_write(
        &journal(registry),
        &serde_json::to_vec(&Pending::Install { receipt, stage, previous })?,
    )?;
    let _retained = staging.keep();
    recover_locked(registry)?;
    Ok(compiler)
}

pub fn uninstall(registry: &Registry, version: &str) -> Result<()> {
    recover(registry)?;
    let version = validate_version(version)?;
    let _lock = registry.lock()?;
    if registry.settings()?.default_version.as_ref() == Some(&version) {
        return Err(invalid(
            "select a different default compiler before uninstalling this version",
        ));
    }
    let mut entries = registry.list()?;
    let compiler = entries
        .iter()
        .find(|entry| entry.version == version && entry.target == host())
        .cloned()
        .ok_or_else(|| invalid("compiler is not registered"))?;
    let destination = slot(registry, &version.to_string(), &host())?;
    if compiler.executable != destination.join("bin").join(executable()) {
        entries.retain(|entry| entry != &compiler);
        return registry.write(entries);
    }
    if read_ownership(&destination)?.compiler != compiler {
        return Err(invalid("registered compiler does not match its ownership receipt"));
    }
    let lease = open_lock(&registry.lease_path(&compiler.executable))?;
    lease.try_lock().map_err(|_| {
        invalid("toolchain is in use; stop its daemons and language servers before removing it")
    })?;
    // Keep the inode lease across the entire journal transaction.
    atomic_write(
        &journal(registry),
        &serde_json::to_vec(&Pending::Remove {
            compiler,
            trash: format!(".fai-trash-{}-{}", version, std::process::id()),
        })?,
    )?;
    recover_with_lease(registry, true)
}

/// Atomically replaces a launcher without exposing a non-executable intermediate file.
pub fn copy_executable(source: &Utf8Path, destination: &Utf8Path) -> Result<()> {
    let parent = destination.parent().ok_or_else(|| invalid("executable has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::copy(&mut File::open(source)?, &mut temporary)?;
    temporary.flush()?;
    temporary.as_file().set_permissions(std::fs::metadata(source)?.permissions())?;
    temporary.as_file().sync_all()?;
    temporary.persist(destination).map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstallerReceipt {
    schema_version: u32,
    version: String,
    target: String,
    sha256: String,
}

fn installer_receipt(registry: &Registry) -> Utf8PathBuf {
    registry.home.join("installer.json")
}

fn verify_launcher_owner(registry: &Registry) -> Result<()> {
    let path = registry.home.join("bin").join(executable());
    if !path.exists() {
        return Ok(());
    }
    let receipt: InstallerReceipt =
        serde_json::from_slice(&std::fs::read(installer_receipt(registry)).map_err(|_| {
            invalid("installation path contains an unowned executable; choose another FAI_HOME")
        })?)?;
    if receipt.schema_version != 1
        || receipt.target != host()
        || file_digest(path.as_std_path())? != receipt.sha256
    {
        return Err(invalid("installed launcher does not match its ownership receipt"));
    }
    Ok(())
}

fn record_installer(registry: &Registry, version: &str, sha256: String) -> Result<()> {
    atomic_write(
        &installer_receipt(registry),
        &serde_json::to_vec_pretty(&InstallerReceipt {
            schema_version: 1,
            version: version.into(),
            target: host(),
            sha256,
        })?,
    )
}

pub fn setup(registry: &Registry, version: &str, modify_path: bool) -> Result<InstalledCompiler> {
    let source = Utf8PathBuf::from_path_buf(std::env::current_exe()?)
        .map_err(|_| invalid("installer path must be UTF-8"))?;
    {
        let _lock = registry.lock()?;
        if !recover_installer(registry)? {
            resume_installer(registry)?;
            return Err(invalid(
                "an installer upgrade is pending; close running launchers and retry",
            ));
        }
        verify_launcher_owner(registry)?;
        std::fs::create_dir_all(registry.home.join("updates"))?;
        let stage = tempfile::Builder::new()
            .prefix("installer-")
            .tempdir_in(registry.home.join("updates"))?;
        let staged = Utf8Path::from_path(stage.path())
            .ok_or_else(|| invalid("staging path must be UTF-8"))?
            .join("bin")
            .join(executable());
        copy_executable(&source, &staged)?;
        publish_installer(
            registry,
            stage,
            &staged,
            env!("CARGO_PKG_VERSION"),
            &file_digest(source.as_std_path())?,
        )?;
    }
    let compiler = install(registry, version)?;
    registry.initialize_default(&compiler.version, &host())?;
    if modify_path {
        configure_path(registry)?;
    }
    eprintln!(
        "Installed Fai {}. Add {} to PATH or start a new shell.",
        compiler.version,
        registry.home.join("bin")
    );
    Ok(compiler)
}

pub fn upgrade_installer(registry: &Registry) -> Result<String> {
    {
        let _lock = registry.lock()?;
        if !recover_installer(registry)? {
            return resume_installer(registry);
        }
    }
    let client = Client::new()?;
    let release = client.release("stable")?;
    let target = release
        .targets
        .get(&host())
        .ok_or_else(|| invalid("release has no installer for this host"))?;
    require_system(&target.minimum_system)?;
    let archive = client.download(&release, &target.installer)?;
    std::fs::create_dir_all(registry.home.join("updates"))?;
    let stage =
        tempfile::Builder::new().prefix("installer-").tempdir_in(registry.home.join("updates"))?;
    archive::extract(
        archive.path(),
        stage.path(),
        cfg!(windows),
        &target.installer.executable_sha256,
    )?;
    let stage_path =
        Utf8Path::from_path(stage.path()).ok_or_else(|| invalid("staging path must be UTF-8"))?;
    let source = stage_path.join("bin").join(executable());
    let output = std::process::Command::new(&source)
        .args(["installer", "--version", "--message-format=json"])
        .output()?;
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    if !output.status.success()
        || info["kind"] != "fai-installer"
        || info["version"] != release.version
        || info["target"] != host()
    {
        return Err(invalid("downloaded installer has the wrong identity"));
    }
    let _lock = registry.lock()?;
    verify_launcher_owner(registry)?;
    publish_installer(
        registry,
        stage,
        &source,
        &release.version,
        &target.installer.executable_sha256,
    )?;
    Ok(release.version)
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InstallerTransaction {
    version: String,
    source: Utf8PathBuf,
    previous_sha256: Option<String>,
    sha256: String,
}

fn publish_installer(
    registry: &Registry,
    stage: tempfile::TempDir,
    source: &Utf8Path,
    version: &str,
    sha256: &str,
) -> Result<()> {
    if registry.home.join("installer.pending.json").exists() {
        return Err(invalid("an installer transaction is already pending"));
    }
    let destination = registry.home.join("bin").join(executable());
    let pending = InstallerTransaction {
        version: version.into(),
        source: source.canonicalize_utf8()?,
        previous_sha256: if destination.exists() {
            Some(file_digest(destination.as_std_path())?)
        } else {
            None
        },
        sha256: sha256.into(),
    };
    #[cfg(unix)]
    {
        File::open(stage.path())?.sync_all()?;
        File::open(registry.home.join("updates"))?.sync_all()?;
    }
    atomic_write(&registry.home.join("installer.pending.json"), &serde_json::to_vec(&pending)?)?;
    let _retained = stage.keep();
    if !recover_installer(registry)? {
        resume_installer(registry)?;
    }
    Ok(())
}

fn resume_installer(registry: &Registry) -> Result<String> {
    let pending: InstallerTransaction =
        serde_json::from_slice(&std::fs::read(registry.home.join("installer.pending.json"))?)?;
    #[cfg(windows)]
    std::process::Command::new(&pending.source)
        .args(["installer", "complete-upgrade"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    eprintln!("Installer upgrade staged; close running launchers so replacement can complete.");
    Ok(pending.version)
}

fn recover_installer(registry: &Registry) -> Result<bool> {
    let path = registry.home.join("installer.pending.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let pending: InstallerTransaction = serde_json::from_slice(&bytes)?;
    validate_version(&pending.version)?;
    let source = pending.source.canonicalize_utf8()?;
    if !source.starts_with(registry.home.join("updates").canonicalize_utf8()?)
        || file_digest(source.as_std_path())? != pending.sha256
    {
        return Err(invalid("staged installer does not match its transaction"));
    }
    let destination = registry.home.join("bin").join(executable());
    let current =
        if destination.exists() { Some(file_digest(destination.as_std_path())?) } else { None };
    if current != pending.previous_sha256 && current.as_ref() != Some(&pending.sha256) {
        return Err(invalid("launcher changed while upgrade was pending; transaction retained"));
    }
    if current.as_ref() != Some(&pending.sha256) {
        match copy_executable(&source, &destination) {
            Ok(()) => {}
            #[cfg(windows)]
            Err(fai_toolchain::Error::Io(error))
                if error.kind() == std::io::ErrorKind::PermissionDenied =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        }
    }
    record_installer(registry, &pending.version, pending.sha256)?;
    std::fs::remove_file(path)?;
    #[cfg(not(windows))]
    if let Some(directory) = source.parent().and_then(Utf8Path::parent) {
        std::fs::remove_dir_all(directory)?;
    }
    Ok(true)
}

#[cfg(windows)]
pub fn complete_upgrade(registry: &Registry) -> Result<()> {
    for _ in 0..240 {
        let _lock = registry.lock()?;
        if recover_installer(registry)? {
            return Ok(());
        }
        drop(_lock);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(invalid(
        "installer update remains staged; close running launchers and rerun fai installer upgrade",
    ))
}

fn require_system(minimum: &str) -> Result<()> {
    let (program, arguments, required): (&str, &[&str], Vec<u64>) = if cfg!(target_os = "linux") {
        let version = minimum.strip_prefix("glibc ").ok_or_else(|| {
            invalid("unknown Linux compatibility requirement; upgrade the installer")
        })?;
        ("getconf", &["GNU_LIBC_VERSION"], system_version(version)?)
    } else if cfg!(target_os = "macos") {
        let version = minimum.strip_prefix("macOS ").ok_or_else(|| {
            invalid("unknown macOS compatibility requirement; upgrade the installer")
        })?;
        ("sw_vers", &["-productVersion"], system_version(version)?)
    } else {
        let build = minimum
            .strip_prefix("Windows build ")
            .ok_or_else(|| {
                invalid(
                    "unknown Windows compatibility requirement; use the release's bootstrap script",
                )
            })?
            .parse::<u64>()
            .map_err(|_| invalid("invalid Windows build requirement"))?;
        ("cmd.exe", &["/d", "/c", "ver"], vec![10, 0, build])
    };
    let output = std::process::Command::new(program).args(arguments).output()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let actual = text
        .split(|character: char| !character.is_ascii_digit() && character != '.')
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .ok_or_else(|| {
            invalid(format!("cannot determine host compatibility; release requires {minimum}"))
        })?;
    if !output.status.success() || system_version(actual)? < required {
        return Err(invalid(format!(
            "release requires {minimum}; this host reports {}",
            text.trim()
        )));
    }
    Ok(())
}

fn system_version(text: &str) -> Result<Vec<u64>> {
    text.trim_end_matches('.')
        .split('.')
        .map(|part| part.parse().map_err(|_| invalid("invalid operating-system version")))
        .collect()
}

fn configure_path(registry: &Registry) -> Result<()> {
    #[cfg(unix)]
    {
        let home = std::env::var("HOME")
            .map_err(|_| invalid("HOME is required to configure shell startup files"))?;
        let quote = |text: &str| format!("'{}'", text.replace('\'', "'\\''"));
        let bin = quote(registry.home.join("bin").as_str());
        let body = format!(
            "export FAI_HOME={}\nexport FAI_TOOLCHAIN_DIR={}\ncase \":$PATH:\" in *:{bin}:*) ;; *) export PATH={bin}:\"$PATH\" ;; esac",
            quote(registry.home.as_str()),
            quote(registry.directory.as_str())
        );
        for name in [".profile", ".bashrc", ".zshrc"] {
            update_block(&Utf8Path::new(&home).join(name), &body)?;
        }
        let fish_quote =
            |text: &str| format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"));
        let body = format!(
            "set -gx FAI_HOME {}\nset -gx FAI_TOOLCHAIN_DIR {}\nif not contains -- {} $PATH\n  set -gx PATH {} $PATH\nend",
            fish_quote(registry.home.as_str()),
            fish_quote(registry.directory.as_str()),
            fish_quote(registry.home.join("bin").as_str()),
            fish_quote(registry.home.join("bin").as_str())
        );
        update_block(&Utf8Path::new(&home).join(".config/fish/conf.d/fai.fish"), &body)?;
    }
    #[cfg(windows)]
    {
        let status = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; $p=[Environment]::GetEnvironmentVariable('Path','User'); $b=$env:FAI_INSTALL_BIN; if (($p -split ';') -notcontains $b) { [Environment]::SetEnvironmentVariable('Path', ($b+';'+$p), 'User') }; [Environment]::SetEnvironmentVariable('FAI_HOME',$env:FAI_INSTALL_HOME,'User'); [Environment]::SetEnvironmentVariable('FAI_TOOLCHAIN_DIR',$env:FAI_INSTALL_TOOLS,'User')"])
            .env("FAI_INSTALL_BIN", registry.home.join("bin")).env("FAI_INSTALL_HOME", &registry.home).env("FAI_INSTALL_TOOLS", &registry.directory).status()?;
        if !status.success() {
            return Err(invalid("could not update user PATH"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn update_block(path: &Utf8Path, body: &str) -> Result<()> {
    let resolved = if path.exists() { path.canonicalize_utf8()? } else { path.to_owned() };
    let path = resolved.as_path();
    let before = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    let start = "# >>> fai installer >>>";
    let end = "# <<< fai installer <<<";
    if before.matches(start).count() > 1 || before.matches(end).count() > 1 {
        return Err(invalid(format!(
            "ambiguous Fai PATH blocks in {path}; use --no-modify-path or consolidate them"
        )));
    }
    let block = format!("{start}\n{body}\n{end}");
    let after = if let Some(offset) = before.find(start) {
        let finish = before[offset..]
            .find(end)
            .ok_or_else(|| invalid(format!("incomplete Fai PATH block in {path}")))?
            + offset
            + end.len();
        format!("{}{}{}", &before[..offset], block, &before[finish..])
    } else {
        format!("{before}\n{block}\n")
    };
    atomic_write(path, after.as_bytes())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn staged() -> (tempfile::TempDir, Registry, Receipt, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().canonicalize().unwrap()).unwrap();
        let registry = Registry { home: root.join("home"), directory: root.join("tools") };
        let destination = slot(&registry, "1.0.0", &host()).unwrap();
        let stage = ".fai-stage-test".to_owned();
        let directory = registry.directory.join(&stage);
        std::fs::create_dir_all(directory.join("bin")).unwrap();
        let executable = directory.join("bin/fai");
        let info = serde_json::json!({"version":"1.0.0","target":host(),"toolBuildId":"test"});
        std::fs::write(&executable, format!("#!/bin/sh\nprintf '%s\\n' '{info}'\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut compiler = probe(&executable).unwrap();
        compiler.executable = destination.join("bin/fai");
        let receipt = Receipt {
            schema_version: 1,
            compiler,
            commit: "a".repeat(40),
            archive_sha256: "b".repeat(64),
            executable_sha256: file_digest(executable.as_std_path()).unwrap(),
        };
        atomic_write(&directory.join("receipt.json"), &serde_json::to_vec(&receipt).unwrap())
            .unwrap();
        (temp, registry, receipt, stage)
    }

    #[test]
    fn interrupted_installation_publishes_the_verified_staged_compiler() {
        let (_temp, registry, receipt, stage) = staged();
        atomic_write(
            &journal(&registry),
            &serde_json::to_vec(&Pending::Install {
                receipt: receipt.clone(),
                stage,
                previous: None,
            })
            .unwrap(),
        )
        .unwrap();
        recover(&registry).unwrap();
        assert_eq!(registry.list().unwrap(), vec![receipt.compiler]);
        assert!(!journal(&registry).exists());
    }

    #[test]
    fn recovery_preserves_a_conflicting_registration() {
        let (_temp, registry, receipt, stage) = staged();
        let mut other = receipt.compiler.clone();
        other.build_id = "user-rebuild".into();
        registry.write(vec![other.clone()]).unwrap();
        atomic_write(
            &journal(&registry),
            &serde_json::to_vec(&Pending::Install { receipt, stage, previous: None }).unwrap(),
        )
        .unwrap();
        assert!(recover(&registry).is_err());
        assert_eq!(registry.list().unwrap(), vec![other]);
        assert!(journal(&registry).exists());
    }

    #[test]
    fn shared_process_lease_prevents_uninstallation() {
        let (_temp, registry, receipt, stage) = staged();
        atomic_write(
            &journal(&registry),
            &serde_json::to_vec(&Pending::Install {
                receipt: receipt.clone(),
                stage,
                previous: None,
            })
            .unwrap(),
        )
        .unwrap();
        recover(&registry).unwrap();
        let lease = open_lock(&registry.lease_path(&receipt.compiler.executable)).unwrap();
        lease.lock_shared().unwrap();
        assert!(uninstall(&registry, "1.0.0").unwrap_err().to_string().contains("in use"));
        assert!(receipt.compiler.executable.is_file());
        drop(lease);
        uninstall(&registry, "1.0.0").unwrap();
        assert!(!receipt.compiler.executable.exists());
    }

    #[test]
    fn partially_deleted_removal_staging_can_be_recovered() {
        let (_temp, registry, receipt, stage) = staged();
        let trash = ".fai-trash-test";
        std::fs::rename(registry.directory.join(stage), registry.directory.join(trash)).unwrap();
        std::fs::remove_file(registry.directory.join(trash).join("receipt.json")).unwrap();
        atomic_write(
            &journal(&registry),
            &serde_json::to_vec(&Pending::Remove {
                compiler: receipt.compiler,
                trash: trash.into(),
            })
            .unwrap(),
        )
        .unwrap();
        recover(&registry).unwrap();
        assert!(!registry.directory.join(trash).exists());
        assert!(!journal(&registry).exists());
    }

    #[test]
    fn owned_broken_compilers_can_be_removed_without_running_them() {
        let (_temp, registry, receipt, stage) = staged();
        atomic_write(
            &journal(&registry),
            &serde_json::to_vec(&Pending::Install {
                receipt: receipt.clone(),
                stage,
                previous: None,
            })
            .unwrap(),
        )
        .unwrap();
        recover(&registry).unwrap();
        std::fs::write(&receipt.compiler.executable, b"not executable code").unwrap();
        uninstall(&registry, "1.0.0").unwrap();
        assert!(!receipt.compiler.executable.exists());
    }

    #[test]
    fn shell_configuration_is_idempotent_and_preserves_surrounding_text() {
        let temp = tempfile::tempdir().unwrap();
        let path = Utf8Path::from_path(temp.path()).unwrap().join("profile");
        std::fs::write(&path, "export USER_SETTING=keep\n").unwrap();
        update_block(&path, "export PATH=test:$PATH").unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        update_block(&path, "export PATH=test:$PATH").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert!(before.starts_with("export USER_SETTING=keep\n"));
    }

    #[test]
    fn interrupted_launcher_replacement_recovers_its_receipt() {
        let (_temp, registry, _receipt, _stage) = staged();
        let source = registry.home.join("updates/installer-test/bin/fai");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::write(&source, b"new launcher").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o755)).unwrap();
        let destination = registry.home.join("bin/fai");
        copy_executable(&source, &destination).unwrap();
        let hash = file_digest(source.as_std_path()).unwrap();
        atomic_write(
            &registry.home.join("installer.pending.json"),
            &serde_json::to_vec(&InstallerTransaction {
                version: "1.0.0".into(),
                source,
                previous_sha256: None,
                sha256: hash.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(recover_installer(&registry).unwrap());
        let receipt: InstallerReceipt =
            serde_json::from_slice(&std::fs::read(installer_receipt(&registry)).unwrap()).unwrap();
        assert_eq!(receipt.sha256, hash);
        assert!(!registry.home.join("installer.pending.json").exists());
    }
}
