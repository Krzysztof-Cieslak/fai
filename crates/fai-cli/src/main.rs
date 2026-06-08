//! Thin entry point for the `fai` binary: forward to [`fai_cli::run`].

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Use the unlocked stream handles (each write locks briefly) rather than
    // holding the locks for the whole run: `fai lsp` hands stdio to the language
    // server, whose own writer thread must be able to lock stdout — a persistent
    // lock held here would deadlock it.
    let mut out = std::io::stdout();
    let mut err = std::io::stderr();

    let code = fai_cli::run(std::env::args_os(), &mut out, &mut err);

    let _ = out.flush();
    let _ = err.flush();
    // Exit codes are small, defined constants; clamp to u8 for ExitCode.
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
