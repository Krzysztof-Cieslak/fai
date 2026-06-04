//! Thin entry point for the `fai` binary: forward to [`fai_cli::run`].

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    let code = fai_cli::run(std::env::args_os(), &mut out, &mut err);

    let _ = out.flush();
    let _ = err.flush();
    // Exit codes are small, defined constants; clamp to u8 for ExitCode.
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
