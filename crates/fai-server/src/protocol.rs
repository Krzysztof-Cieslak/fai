//! The client↔daemon wire protocol: JSON-RPC 2.0 semantics encoded with
//! MessagePack, in length-prefixed frames (CLI.md §7.2).
//!
//! Each frame is a little-endian `u32` byte length followed by a MessagePack
//! object. A request is a single [`Request`] frame; the server replies with zero
//! or more [`ServerMessage`] frames ending in a [`ServerMessage::Result`]
//! (streaming `Output` frames precede it for `run`).

use std::io::{self, Read, Write};

use fai_driver::{CommandSpec, DirtyFile, RenderOpts, Rendered};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The protocol version. Bumped on any incompatible wire change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame we will read, guarding against a corrupt length prefix (64 MiB).
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// A client→server request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// The handshake; must be the first request on a connection.
    Initialize(InitParams),
    /// Run a build/dev/query command and return rendered output.
    Command(CommandRequest),
    /// Report daemon status.
    Status,
    /// Run a program under daemon supervision (streamed output, then exit code).
    Run(RunRequest),
    /// Graceful shutdown: reply, then exit the daemon process.
    Shutdown,
    /// Close this connection.
    Exit,
}

/// Handshake parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitParams {
    /// The client's protocol version.
    pub protocol_version: u32,
    /// The client's compiler version (must match the daemon's).
    pub compiler_version: String,
    /// The absolute workspace root the client expects.
    pub workspace_root: String,
}

/// The handshake result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitResult {
    /// The daemon's protocol version.
    pub protocol_version: u32,
    /// The daemon's compiler version.
    pub compiler_version: String,
}

/// A command invocation plus the client's render options and dirty-set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    /// The command to run.
    pub spec: CommandSpec,
    /// Rendering options (format/color) from the client.
    pub opts: RenderOpts,
    /// Optional client-declared changed files (fast path; usually empty).
    #[serde(default)]
    pub dirty: Vec<DirtyFile>,
}

/// A `run` invocation: an entry file, program arguments, and a dirty-set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRequest {
    /// The entry file (workspace-relative or absolute).
    pub path: String,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Optional client-declared changed files.
    #[serde(default)]
    pub dirty: Vec<DirtyFile>,
}

/// A server→client message: streamed output, then a final result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMessage {
    /// A chunk of a supervised program's output (`$/output`).
    Output {
        /// Which stream the chunk belongs to.
        stream: OutputStream,
        /// The raw bytes.
        chunk: Vec<u8>,
    },
    /// The terminal response for the request.
    Result(Response),
}

/// Which standard stream an [`ServerMessage::Output`] chunk targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A terminal response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    /// Handshake accepted.
    Initialized(InitResult),
    /// A command's rendered output.
    Command(Rendered),
    /// Daemon status.
    Status(StatusInfo),
    /// A supervised program finished with this exit code.
    RunExit(i32),
    /// An acknowledgement (e.g. for `Shutdown`).
    Ok,
    /// A request-level error (message; carries a `FAInnnn` when applicable).
    Error(String),
}

/// Daemon status, reported by [`Request::Status`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    /// The daemon process id.
    pub pid: u32,
    /// The daemon's compiler version.
    pub compiler_version: String,
    /// The daemon's protocol version.
    pub protocol_version: u32,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
}

/// Writes one length-prefixed MessagePack frame.
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> io::Result<()> {
    let body = rmp_serde::to_vec_named(message)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame too large"))?;
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(&body)?;
    writer.flush()
}

/// Reads one length-prefixed MessagePack frame, decoding it as `T`.
pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame exceeds maximum size"));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    rmp_serde::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Decodes a frame body to a pretty JSON string, for `--protocol-log`/`tap`.
#[must_use]
pub fn frame_to_json<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).unwrap_or_else(|_| "<unserializable>".to_owned())
}

#[cfg(test)]
mod tests {
    use fai_driver::{CommandSpec, OutputFormat, RenderOpts};

    use super::*;

    #[test]
    fn request_frame_round_trips() {
        let request = Request::Command(CommandRequest {
            spec: CommandSpec::Check { path: None },
            opts: RenderOpts { format: OutputFormat::Json, color: false },
            dirty: Vec::new(),
        });
        let mut buf = Vec::new();
        write_frame(&mut buf, &request).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: Request = read_frame(&mut cursor).unwrap();
        match decoded {
            Request::Command(c) => {
                assert!(matches!(c.spec, CommandSpec::Check { path: None }));
                assert_eq!(c.opts.format, OutputFormat::Json);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn server_message_round_trips() {
        let msg = ServerMessage::Result(Response::RunExit(7));
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: ServerMessage = read_frame(&mut cursor).unwrap();
        assert!(matches!(decoded, ServerMessage::Result(Response::RunExit(7))));
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        let result: io::Result<Request> = read_frame(&mut cursor);
        assert!(result.is_err());
    }
}
