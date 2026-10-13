//! Public installer commands, handled before any compiler is selected.

use camino::Utf8PathBuf;
use clap::{Parser, Subcommand, ValueEnum};
use fai_toolchain::{Invocation, Registry, Result, host, invalid, pin_project, validate_version};

use crate::{install, network::Client};

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    Human,
    Json,
}

#[derive(Parser)]
#[command(name = "fai", about = "Fai compiler installation and version management")]
struct Cli {
    #[arg(long, global = true, default_value = "human")]
    message_format: Format,
    #[arg(long = "project", short = 'C', global = true)]
    project: Option<Utf8PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install the latest stable compiler and select it as the global default.
    Upgrade,
    /// Manage the native launcher independently of selected compilers.
    Installer {
        /// Show the installer version and host.
        #[arg(long)]
        version: bool,
        #[command(subcommand)]
        command: Option<Installer>,
    },
    /// Install, select, inspect, and remove compiler versions.
    Toolchain {
        #[command(subcommand)]
        command: Toolchain,
    },
}

#[derive(Subcommand)]
enum Installer {
    /// Upgrade the native launcher from the current stable release.
    Upgrade,
    #[command(hide = true)]
    Setup {
        #[arg(long, default_value = "stable")]
        version: String,
        #[arg(long)]
        no_modify_path: bool,
    },
    #[cfg(windows)]
    #[command(hide = true)]
    CompleteUpgrade,
}

#[derive(Subcommand)]
enum Toolchain {
    /// Download and register an exact compiler, or the stable release.
    Install { version: String },
    /// Select an already installed global default.
    Default { version: String },
    /// Set the exact compiler version in the active root's fai.toml.
    Pin { version: String },
    /// List local compilers, or explicitly query available releases.
    List {
        #[arg(long)]
        available: bool,
    },
    /// Explain the effective compiler version, path, and selection source.
    Which,
    /// Remove a managed compiler or unregister an external executable.
    Uninstall { version: String },
    /// Register a user-owned compiler, replacing its release/host registration.
    Register { executable: Utf8PathBuf },
    /// Check compiler selection, PATH, and native build prerequisites.
    Doctor,
}

pub fn run(invocation: &Invocation, registry: &Registry) -> Result<i32> {
    let cli = match Cli::try_parse_from(&invocation.arguments) {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            if error.use_stderr() {
                if invocation.json {
                    println!(
                        "{}",
                        serde_json::json!({"schemaVersion": 1, "ok": false, "error": error.to_string()})
                    );
                } else {
                    eprint!("{error}");
                }
            } else {
                print!("{error}");
            }
            return Ok(code);
        }
    };
    let json = matches!(cli.message_format, Format::Json);
    match cli.command {
        Command::Upgrade => {
            let compiler = install::install(registry, "stable")?;
            registry.set_default(&compiler.version, &host())?;
            output(
                json,
                "upgrade",
                serde_json::to_value(&compiler)?,
                &format!("Default compiler: {}", compiler.version),
            );
        }
        Command::Installer { version: true, .. } => {
            let info = serde_json::json!({"schemaVersion": 1, "kind": "fai-installer", "version": env!("CARGO_PKG_VERSION"), "target": host()});
            if json {
                println!("{info}");
            } else {
                println!("fai installer {} ({})", env!("CARGO_PKG_VERSION"), host());
            }
        }
        Command::Installer { command, .. } => match command
            .ok_or_else(|| invalid("use fai installer upgrade or fai installer --version"))?
        {
            Installer::Upgrade => {
                let version = install::upgrade_installer(registry)?;
                output(
                    json,
                    "installer upgrade",
                    serde_json::json!({"version": version}),
                    &format!("Installer upgrade: {version}"),
                );
            }
            Installer::Setup { version, no_modify_path } => {
                let compiler = install::setup(registry, &version, !no_modify_path)?;
                output(
                    json,
                    "installer setup",
                    serde_json::to_value(compiler)?,
                    "Installation complete",
                );
            }
            #[cfg(windows)]
            Installer::CompleteUpgrade => install::complete_upgrade(registry)?,
        },
        Command::Toolchain { command } => match command {
            Toolchain::Install { version } => {
                let compiler = install::install(registry, &version)?;
                output(
                    json,
                    "toolchain install",
                    serde_json::to_value(&compiler)?,
                    &format!("Installed {} at {}", compiler.version, compiler.executable),
                );
            }
            Toolchain::Default { version } => {
                install::recover(registry)?;
                registry.set_default(&validate_version(&version)?, &host())?;
                output(
                    json,
                    "toolchain default",
                    serde_json::json!({"version": version}),
                    &format!("Default compiler: {version}"),
                );
            }
            Toolchain::Pin { version } => {
                let path = pin_project(&invocation.directory, &validate_version(&version)?)?;
                output(
                    json,
                    "toolchain pin",
                    serde_json::json!({"version": version, "manifest": path}),
                    &format!("Pinned {version} in {path}"),
                );
            }
            Toolchain::List { available: true } => {
                let versions = Client::new()?.available()?;
                output(
                    json,
                    "toolchain list",
                    serde_json::json!({"versions": versions}),
                    &versions.join("\n"),
                );
            }
            Toolchain::List { available: false } => {
                let entries = registry.list()?;
                let text = entries
                    .iter()
                    .map(|entry| format!("{} {} {}", entry.version, entry.target, entry.executable))
                    .collect::<Vec<_>>()
                    .join("\n");
                // Preserve the existing inventory command's JSON contract.
                if json {
                    println!("{}", serde_json::json!({"schemaVersion": 1, "toolchains": entries}));
                } else {
                    println!("{text}");
                }
            }
            Toolchain::Which => {
                let environment = std::env::var("FAI_TOOLCHAIN").ok();
                let (version, reason) = invocation.selection(registry, environment.as_deref())?;
                let compiler = registry.select(&version, &host())?;
                output(
                    json,
                    "toolchain which",
                    serde_json::json!({"compiler": compiler, "reason": reason}),
                    &format!("{} ({reason})\n{}", compiler.version, compiler.executable),
                );
            }
            Toolchain::Register { executable } => {
                install::recover(registry)?;
                let compiler = registry.register(&executable)?;
                output(
                    json,
                    "toolchain register",
                    serde_json::to_value(&compiler)?,
                    &format!("Registered {} at {}", compiler.version, compiler.executable),
                );
            }
            Toolchain::Uninstall { version } => {
                install::uninstall(registry, &version)?;
                output(
                    json,
                    "toolchain uninstall",
                    serde_json::json!({"version": version}),
                    &format!("Removed registration for {version}"),
                );
            }
            Toolchain::Doctor => return doctor(json, invocation, registry),
        },
    }
    Ok(0)
}

fn output(json: bool, command: &str, value: serde_json::Value, human: &str) {
    if json {
        println!(
            "{}",
            serde_json::json!({"schemaVersion": 1, "ok": true, "command": command, "result": value})
        );
    } else {
        println!("{human}");
    }
}

fn doctor(json: bool, invocation: &Invocation, registry: &Registry) -> Result<i32> {
    let environment = std::env::var("FAI_TOOLCHAIN").ok();
    let selection = invocation
        .selection(registry, environment.as_deref())
        .and_then(|(version, _)| registry.select(&version, &host()));
    let linker = if cfg!(windows) { "link.exe" } else { "cc" };
    let checks = vec![
        serde_json::json!({"name": "compiler", "ok": selection.is_ok(), "detail": match &selection { Ok(compiler) => compiler.executable.to_string(), Err(error) => error.to_string() }}),
        serde_json::json!({"name": "linker", "ok": on_path(linker), "detail": if cfg!(windows) { "AOT requires MSVC Build Tools, Windows SDK, and a developer shell" } else if cfg!(target_os = "macos") { "AOT requires Xcode Command Line Tools" } else { "AOT requires a C toolchain and system development libraries" }}),
        serde_json::json!({"name": "git", "ok": on_path(if cfg!(windows) { "git.exe" } else { "git" }), "detail": "Git is needed for Git-backed dependencies"}),
        serde_json::json!({"name": "launcherPath", "ok": std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|path| path == registry.home.join("bin").as_std_path())), "detail": registry.home.join("bin")}),
    ];
    let ok = checks.iter().all(|check| check["ok"] == true);
    if json {
        println!("{}", serde_json::json!({"schemaVersion": 1, "ok": ok, "checks": checks}));
    } else {
        for check in checks {
            println!(
                "{}: {} — {}",
                check["name"].as_str().unwrap_or("check"),
                if check["ok"] == true { "ok" } else { "missing" },
                check["detail"].as_str().unwrap_or("")
            );
        }
    }
    Ok(if ok { 0 } else { 1 })
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|path| path.join(name).is_file()))
}
