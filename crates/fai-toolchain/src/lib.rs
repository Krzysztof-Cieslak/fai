//! Shared, compiler-independent installation and selection contracts.

mod invocation;
mod registry;
mod release;

pub use invocation::{Invocation, Pin, discover_pin, pin_project};
pub use registry::{
    InstalledCompiler, Registry, Settings, atomic_write, lease_current, lock_file, open_lock, probe,
};
pub use release::{Artifact, Release, TargetRelease, digest, file_digest, validate_version};

/// Host for which this executable was built, independent of a project's target flags.
pub fn host() -> String {
    target_lexicon::HOST.to_string()
}

/// A recoverable installation, selection, or metadata failure.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem or process operation failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// Metadata could not be decoded.
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    /// Invalid input or a failed precondition.
    #[error("{0}")]
    Invalid(String),
}

/// Shared result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Creates an actionable metadata error.
pub fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}
