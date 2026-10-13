//! Explicit registration shared with the native installer and launcher.

use camino::Utf8Path;
use fai_package::PackageVersion;

use crate::DriverError;

pub use fai_toolchain::InstalledCompiler;

fn unavailable(error: fai_toolchain::Error) -> DriverError {
    fai_package::ProviderFailure {
        code: fai_package::COMPILER_MISMATCH,
        message: error.to_string(),
    }
    .into()
}

/// Lists explicitly registered compilers without downloading or probing them.
pub fn installed_compilers() -> Result<Vec<InstalledCompiler>, DriverError> {
    fai_toolchain::Registry::from_env().and_then(|registry| registry.list()).map_err(unavailable)
}

/// Verifies and explicitly registers an existing compiler executable.
pub fn register_compiler(executable: &Utf8Path) -> Result<InstalledCompiler, DriverError> {
    fai_toolchain::Registry::from_env()
        .and_then(|registry| registry.register(executable))
        .map_err(unavailable)
}

/// Selects and revalidates a registered exact host compiler.
pub fn select_compiler(version: &PackageVersion) -> Result<InstalledCompiler, DriverError> {
    fai_toolchain::Registry::from_env()
        .and_then(|registry| registry.select(version, crate::build_info().target))
        .map_err(unavailable)
}
