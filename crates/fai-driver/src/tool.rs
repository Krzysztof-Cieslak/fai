//! Identity and provenance of the complete compiler executable.

use serde::Serialize;

/// Complete executable identity, including CLI, formatter, IDE, and daemon code.
pub const TOOL_BUILD_ID: &str = env!("FAI_TOOL_BUILD_ID");

/// Portable compiler metadata, emitted by `fai build-info`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    /// Output schema version.
    pub schema_version: u32,
    /// Semantic package version.
    pub version: &'static str,
    /// Content identity of all declared compiler inputs.
    pub source_id: &'static str,
    /// Executable identity including compiler/build settings.
    pub tool_build_id: &'static str,
    /// Separate, narrower identity used by the native object cache.
    pub backend_build_id: &'static str,
    /// Native target triple.
    pub target: &'static str,
    /// Profile used to build the compiler.
    pub profile: &'static str,
    /// Whether runtime leak checking and compiler assertions are present.
    pub debug_assertions: bool,
    /// Rust compiler version used to build the executable.
    pub rustc_version: &'static str,
    /// Bundled non-system native libraries, extracted when linking programs.
    pub native_libraries: Vec<&'static str>,
}

/// Returns metadata without opening a workspace or connecting to a daemon.
#[must_use]
pub fn build_info() -> BuildInfo {
    BuildInfo {
        schema_version: 1,
        version: env!("CARGO_PKG_VERSION"),
        source_id: env!("FAI_TOOL_SOURCE_ID"),
        tool_build_id: TOOL_BUILD_ID,
        backend_build_id: env!("FAI_COMPILER_BUILD_ID"),
        target: env!("FAI_TOOL_TARGET"),
        profile: env!("FAI_TOOL_PROFILE"),
        debug_assertions: cfg!(debug_assertions),
        rustc_version: env!("FAI_TOOL_RUSTC"),
        native_libraries: crate::backend::runtime_library_names(),
    }
}
