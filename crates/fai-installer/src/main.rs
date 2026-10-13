//! Native Fai entry point, installed as `fai` beside versioned compiler payloads.

mod archive;
mod cli;
mod install;
mod network;

use camino::Utf8PathBuf;
use fai_toolchain::{Invocation, Registry, Result, invalid};

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    let flags = &args[..args.iter().position(|arg| arg == "--").unwrap_or(args.len())];
    let json = flags.iter().any(|arg| arg == "--message-format=json")
        || flags.windows(2).any(|args| args[0] == "--message-format" && args[1] == "json");
    match run(args) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"schemaVersion": 1, "ok": false, "error": error.to_string()})
                );
            } else {
                eprintln!("fai: {error}");
            }
            std::process::exit(3);
        }
    }
}

fn run(args: Vec<std::ffi::OsString>) -> Result<i32> {
    if std::env::var_os("FAI_TOOLCHAIN_PROBE").is_some() {
        return Err(invalid("register a compiler payload, not the fai launcher"));
    }
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|_| invalid("working directory must be UTF-8"))?;
    let invocation = Invocation::parse(args, &cwd)?;
    let registry = Registry::from_env()?;
    if matches!(invocation.command.as_deref(), Some("toolchain" | "installer" | "upgrade")) {
        return cli::run(&invocation, &registry);
    }
    if invocation.arguments.len() == 1
        || invocation.arguments.iter().any(|arg| arg == "--help" || arg == "-h")
            && invocation.command.is_none()
    {
        println!(
            "Fai compiler and toolchain\n\nfai <compiler command> [arguments]\nfai +VERSION <command>\nfai update [package]       Update project dependencies\nfai upgrade                Upgrade the default compiler\nfai installer upgrade      Upgrade this launcher\nfai toolchain --help       Manage installed compiler versions"
        );
        return Ok(0);
    }
    let environment = std::env::var("FAI_TOOLCHAIN").ok();
    let (version, _) = invocation.selection(&registry, environment.as_deref())?;
    let compiler = registry.select(&version, &fai_toolchain::host())?;
    if compiler.executable.as_std_path() == std::env::current_exe()?.canonicalize()? {
        return Err(invalid("compiler inventory points at the launcher"));
    }
    let mut command = std::process::Command::new(&compiler.executable);
    command
        .args(invocation.arguments.iter().skip(1))
        .env("FAI_TOOLCHAIN_DISPATCHED", version.to_string());
    // The compiler validates its project pin again and starts workers using its own path.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        Ok(command.status()?.code().unwrap_or(1))
    }
}
