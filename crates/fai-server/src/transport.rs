//! Transport: the version-stamped, per-workspace endpoint and its listener /
//! stream, built on `interprocess` local sockets (Unix-domain sockets on POSIX,
//! named pipes on Windows) so one safe code path serves both platforms.
//!
//! The endpoint name embeds a hash of the workspace root and the compiler
//! version, so different workspaces and different compiler versions never
//! collide (CLI.md §7.1). On Unix the socket file is created `0600` and a stale
//! file from a crashed daemon is reclaimed.

use std::io;
use std::path::PathBuf;

use camino::Utf8Path;
use interprocess::local_socket::traits::{Listener as _, Stream as _};
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(not(unix))]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{Listener, ListenerOptions, Name, Stream};

/// The compiler version, stamped into the endpoint name.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why binding the listener failed.
#[derive(Debug)]
pub enum BindError {
    /// Another live daemon already owns this endpoint.
    AlreadyRunning,
    /// An I/O error while binding.
    Io(io::Error),
}

impl From<io::Error> for BindError {
    fn from(error: io::Error) -> Self {
        BindError::Io(error)
    }
}

/// A short, stable hash of the canonicalized workspace root.
#[must_use]
pub fn workspace_id(root: &Utf8Path) -> String {
    let canonical = std::fs::canonicalize(root)
        .ok()
        .and_then(|p| p.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| root.as_str().to_owned());
    let hash = blake3::hash(canonical.as_bytes());
    hash.to_hex()[..16].to_owned()
}

/// The Unix socket file path for `root` (also used for `0600` and unlinking).
/// `None` on platforms that don't use a filesystem path (Windows pipes).
#[must_use]
pub fn socket_path(root: &Utf8Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        Some(runtime_dir().join(format!("{}-{VERSION}.sock", workspace_id(root))))
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        None
    }
}

/// The endpoint name used to connect or bind.
fn endpoint_name(root: &Utf8Path) -> io::Result<Name<'static>> {
    #[cfg(unix)]
    {
        let path = socket_path(root).expect("unix has a socket path");
        path.into_os_string().to_fs_name::<GenericFilePath>()
    }
    #[cfg(not(unix))]
    {
        format!("fai-{}-{VERSION}.sock", workspace_id(root)).to_ns_name::<GenericNamespaced>()
    }
}

/// The directory holding daemon sockets (Unix).
#[cfg(unix)]
fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("FAI_RUNTIME_DIR")
        .or_else(|| std::env::var_os("XDG_RUNTIME_DIR"))
        .or_else(|| std::env::var_os("TMPDIR"))
        .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    base.join("fai")
}

/// Connects to the daemon for `root`.
pub fn connect(root: &Utf8Path) -> io::Result<Stream> {
    let name = endpoint_name(root)?;
    Stream::connect(name)
}

/// Binds the daemon listener for `root`, reclaiming a stale endpoint.
///
/// Returns [`BindError::AlreadyRunning`] if a live daemon already holds the
/// endpoint (the caller should connect to it instead).
pub fn bind(root: &Utf8Path) -> Result<Listener, BindError> {
    #[cfg(unix)]
    if let Some(dir) = socket_path(root).and_then(|p| p.parent().map(ToOwned::to_owned)) {
        std::fs::create_dir_all(&dir)?;
    }

    match create(root) {
        Ok(listener) => Ok(after_bind(root, listener)),
        Err(error) if is_addr_in_use(&error) => {
            // Either a live daemon (connect succeeds → yield) or a stale socket
            // from a crash (connect fails → unlink and retry once).
            if connect(root).is_ok() {
                return Err(BindError::AlreadyRunning);
            }
            remove_stale(root);
            let listener = create(root)?;
            Ok(after_bind(root, listener))
        }
        Err(error) => Err(BindError::Io(error)),
    }
}

/// Creates the listener at the endpoint.
fn create(root: &Utf8Path) -> io::Result<Listener> {
    let name = endpoint_name(root)?;
    ListenerOptions::new().name(name).create_sync()
}

/// Post-bind setup: lock down the socket file permissions on Unix.
fn after_bind(root: &Utf8Path, listener: Listener) -> Listener {
    #[cfg(unix)]
    if let Some(path) = socket_path(root) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = root;
    listener
}

/// Removes a stale socket file (Unix).
fn remove_stale(root: &Utf8Path) {
    #[cfg(unix)]
    if let Some(path) = socket_path(root) {
        let _ = std::fs::remove_file(path);
    }
    #[cfg(not(unix))]
    let _ = root;
}

/// Whether an error means the endpoint is already in use.
fn is_addr_in_use(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AddrInUse
}

/// Accepts the next connection.
pub fn accept(listener: &Listener) -> io::Result<Stream> {
    listener.accept()
}
