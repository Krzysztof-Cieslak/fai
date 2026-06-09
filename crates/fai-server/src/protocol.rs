//! The client↔daemon wire protocol: JSON-RPC 2.0 semantics encoded with
//! MessagePack, in length-prefixed frames (CLI.md §7.2).
//!
//! Each frame is a little-endian `u32` byte length followed by a MessagePack
//! object. A request is a single [`Request`] frame; the server replies with zero
//! or more [`ServerMessage`] frames ending in a [`ServerMessage::Result`]
//! (streaming `Output` frames precede it for `run`).

use std::io::{self, Read, Write};

use fai_driver::{CommandSpec, ContractEvent, DirtyFile, RenderOpts, Rendered};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The protocol version. Bumped on any incompatible wire change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest frame we will read, guarding against a corrupt length prefix (64 MiB).
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// A client→server request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    /// The handshake; must be the first request on a connection.
    Initialize(InitParams),
    /// Run a build/dev/query command and return rendered output.
    Command(CommandRequest),
    /// Report daemon status.
    Status,
    /// Run a program under daemon supervision (streamed output, then exit code).
    Run(RunRequest),
    /// Run example/forall contracts under daemon supervision (streamed
    /// per-contract events, then the rendered report).
    Test(TestRequest),
    /// Subscribe this connection to a JSON decode of the frames flowing on every
    /// other connection (debugging; see [`TapFrame`]). The daemon acknowledges
    /// with [`Response::Ok`], then streams [`ServerMessage::TapFrame`]s until the
    /// client disconnects.
    Tap,
    /// Graceful shutdown: reply, then exit the daemon process.
    Shutdown,
    /// Close this connection.
    Exit,
}

/// Handshake parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitParams {
    /// The client's protocol version.
    pub protocol_version: u32,
    /// The client's compiler version (must match the daemon's).
    pub compiler_version: String,
    /// The absolute workspace root the client expects.
    pub workspace_root: String,
}

/// The handshake result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitResult {
    /// The daemon's protocol version.
    pub protocol_version: u32,
    /// The daemon's compiler version.
    pub compiler_version: String,
}

/// A command invocation plus the client's render options and dirty-set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRequest {
    /// The entry file (workspace-relative or absolute).
    pub path: String,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Optional client-declared changed files.
    #[serde(default)]
    pub dirty: Vec<DirtyFile>,
}

/// A `test` invocation: an optional path/match selection, generator overrides,
/// the client's render options, and a dirty-set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRequest {
    /// File/dir to select contracts from; `None` = the whole workspace.
    pub path: Option<String>,
    /// Run only contracts whose subject/module matches this pattern.
    pub r#match: Option<String>,
    /// The initial PRNG seed override.
    pub seed: Option<i64>,
    /// The number of trials override.
    pub count: Option<i64>,
    /// The maximum generation size override.
    pub max_size: Option<i64>,
    /// Rendering options (format/color) from the client.
    pub opts: RenderOpts,
    /// Optional client-declared changed files.
    #[serde(default)]
    pub dirty: Vec<DirtyFile>,
}

/// A server→client message: streamed output/events, then a final result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// A chunk of a supervised program's output (`$/output`).
    Output {
        /// Which stream the chunk belongs to.
        stream: OutputStream,
        /// The raw bytes.
        chunk: Vec<u8>,
    },
    /// A per-contract result from a supervised `test` run (`$/testEvent`).
    TestEvent(ContractEvent),
    /// A JSON decode of one frame observed on another connection, streamed to a
    /// `tap` subscriber.
    TapFrame(TapFrame),
    /// The terminal response for the request.
    Result(Response),
}

/// One frame observed on a daemon connection, decoded for a `tap` subscriber:
/// which connection carried it, its direction, and a JSON rendering of the frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapFrame {
    /// A per-connection sequence id (stable for the life of the connection),
    /// distinguishing concurrent clients in the interleaved feed.
    pub conn: u64,
    /// Whether the frame travelled toward the daemon or back to its client.
    pub direction: TapDirection,
    /// The frame decoded to a compact JSON string (see [`frame_to_json`]).
    pub json: String,
}

/// The travel direction of a [`TapFrame`], relative to the observed client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TapDirection {
    /// A request travelling from the client to the daemon.
    Inbound,
    /// A message travelling from the daemon back to the client.
    Outbound,
}

/// Renders a [`TapFrame`] as a single line: `#<conn> <arrow> <json>`, where the
/// arrow (`->` inbound, `<-` outbound) mirrors the `--protocol-log` format.
#[must_use]
pub fn render_tap(frame: &TapFrame) -> String {
    let arrow = match frame.direction {
        TapDirection::Inbound => "->",
        TapDirection::Outbound => "<-",
    };
    format!("#{} {arrow} {}", frame.conn, frame.json)
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Handshake accepted.
    Initialized(InitResult),
    /// A command's rendered output.
    Command(Rendered),
    /// Daemon status.
    Status(StatusInfo),
    /// A supervised program finished with this exit code.
    RunExit(i32),
    /// A supervised `test` run's rendered report (stdout/stderr/exit).
    Test(Rendered),
    /// An acknowledgement (e.g. for `Shutdown`).
    Ok,
    /// A request-level error (message; carries a `FAInnnn` when applicable).
    Error(String),
}

/// Daemon status, reported by [`Request::Status`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusInfo {
    /// The daemon process id.
    pub pid: u32,
    /// The daemon's compiler version.
    pub compiler_version: String,
    /// The daemon's protocol version.
    pub protocol_version: u32,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Number of `Command` requests (check/query/fmt/build) served (latency
    /// profiling; excludes `run`).
    pub commands_served: u64,
    /// Total processing time of those commands, in microseconds.
    pub command_micros_total: u64,
    /// The slowest single command's processing time, in microseconds.
    pub command_micros_max: u64,
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

/// Decodes a frame body to a compact (one-line) JSON string, for
/// `--protocol-log`/`tap`.
#[must_use]
pub fn frame_to_json<T: Serialize>(message: &T) -> String {
    serde_json::to_string(message).unwrap_or_else(|_| "<unserializable>".to_owned())
}

#[cfg(test)]
mod tests {
    use fai_driver::{
        CommandSpec, ContractEvent, ContractStatus, DirtyFile, OutputFormat, QueryRequest,
        RenderOpts, Rendered,
    };

    use super::*;

    /// Encodes then decodes a value through a frame, asserting it survives.
    fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
        let mut buf = Vec::new();
        write_frame(&mut buf, value).unwrap();
        let mut cursor = std::io::Cursor::new(buf);
        let decoded: T = read_frame(&mut cursor).unwrap();
        assert_eq!(&decoded, value);
    }

    fn opts() -> RenderOpts {
        RenderOpts { format: OutputFormat::Json, color: true }
    }

    #[test]
    fn every_request_variant_round_trips() {
        round_trip(&Request::Initialize(InitParams {
            protocol_version: PROTOCOL_VERSION,
            compiler_version: "0.1.0".to_owned(),
            workspace_root: "/ws".to_owned(),
        }));
        round_trip(&Request::Command(CommandRequest {
            spec: CommandSpec::Check { path: None },
            opts: opts(),
            dirty: vec![DirtyFile {
                path: "A.fai".to_owned(),
                hash: Some("blake3:abc".to_owned()),
                content: Some("module A\n".to_owned()),
            }],
        }));
        round_trip(&Request::Command(CommandRequest {
            spec: CommandSpec::Query(QueryRequest::Def { target: "M.f".to_owned() }),
            opts: opts(),
            dirty: Vec::new(),
        }));
        round_trip(&Request::Run(RunRequest {
            path: "Main.fai".to_owned(),
            args: vec!["--".to_owned(), "x".to_owned()],
            dirty: Vec::new(),
        }));
        round_trip(&Request::Test(TestRequest {
            path: Some("M.fai".to_owned()),
            r#match: Some("foo".to_owned()),
            seed: Some(7),
            count: None,
            max_size: Some(50),
            opts: opts(),
            dirty: Vec::new(),
        }));
        round_trip(&Request::Status);
        round_trip(&Request::Tap);
        round_trip(&Request::Shutdown);
        round_trip(&Request::Exit);
    }

    #[test]
    fn every_server_message_variant_round_trips() {
        round_trip(&ServerMessage::Output {
            stream: OutputStream::Stdout,
            chunk: b"hello\n".to_vec(),
        });
        round_trip(&ServerMessage::Output { stream: OutputStream::Stderr, chunk: Vec::new() });
        round_trip(&ServerMessage::TestEvent(ContractEvent {
            ordinal: 0,
            symbol: Some("M.f".to_owned()),
            kind: "forall".to_owned(),
            status: ContractStatus::Failed,
            counterexample: Some("n = 0".to_owned()),
            seed: 0,
            trials: 100,
            max_size: 100,
        }));
        round_trip(&ServerMessage::Result(Response::Test(Rendered {
            stdout: "1 passed, 0 failed, 0 could not run (of 1)\n".to_owned(),
            stderr: String::new(),
            exit: 0,
        })));
        round_trip(&ServerMessage::Result(Response::Initialized(InitResult {
            protocol_version: PROTOCOL_VERSION,
            compiler_version: "0.1.0".to_owned(),
        })));
        round_trip(&ServerMessage::Result(Response::Command(Rendered {
            stdout: "ok\n".to_owned(),
            stderr: String::new(),
            exit: 0,
        })));
        round_trip(&ServerMessage::Result(Response::Status(StatusInfo {
            pid: 42,
            compiler_version: "0.1.0".to_owned(),
            protocol_version: PROTOCOL_VERSION,
            uptime_secs: 12,
            commands_served: 7,
            command_micros_total: 1500,
            command_micros_max: 400,
        })));
        round_trip(&ServerMessage::TapFrame(TapFrame {
            conn: 3,
            direction: TapDirection::Inbound,
            json: r#"{"Status":null}"#.to_owned(),
        }));
        round_trip(&ServerMessage::Result(Response::RunExit(124)));
        round_trip(&ServerMessage::Result(Response::Ok));
        round_trip(&ServerMessage::Result(Response::Error("boom".to_owned())));
    }

    #[test]
    fn several_frames_decode_in_sequence() {
        // A streamed `run`: two output frames, then a terminal result, all on one
        // buffer — the reader must split them on the length prefixes.
        let mut buf = Vec::new();
        write_frame(
            &mut buf,
            &ServerMessage::Output { stream: OutputStream::Stdout, chunk: b"a".to_vec() },
        )
        .unwrap();
        write_frame(
            &mut buf,
            &ServerMessage::Output { stream: OutputStream::Stderr, chunk: b"b".to_vec() },
        )
        .unwrap();
        write_frame(&mut buf, &ServerMessage::Result(Response::RunExit(0))).unwrap();

        let mut cursor = std::io::Cursor::new(buf);
        let m1: ServerMessage = read_frame(&mut cursor).unwrap();
        let m2: ServerMessage = read_frame(&mut cursor).unwrap();
        let m3: ServerMessage = read_frame(&mut cursor).unwrap();
        assert!(matches!(m1, ServerMessage::Output { stream: OutputStream::Stdout, .. }));
        assert!(matches!(m2, ServerMessage::Output { stream: OutputStream::Stderr, .. }));
        assert!(matches!(m3, ServerMessage::Result(Response::RunExit(0))));
        // Nothing left to read.
        let trailing: io::Result<ServerMessage> = read_frame(&mut cursor);
        assert!(trailing.is_err());
    }

    #[test]
    fn truncated_body_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Request::Status).unwrap();
        buf.truncate(buf.len() - 1); // drop the last body byte
        let mut cursor = std::io::Cursor::new(buf);
        assert!(read_frame::<_, Request>(&mut cursor).is_err());
    }

    #[test]
    fn truncated_length_prefix_is_an_error() {
        let cursor = std::io::Cursor::new(vec![0u8, 0u8]); // < 4 length bytes
        let mut cursor = cursor;
        assert!(read_frame::<_, Request>(&mut cursor).is_err());
    }

    #[test]
    fn empty_input_is_an_error() {
        let mut cursor = std::io::Cursor::new(Vec::new());
        assert!(read_frame::<_, Request>(&mut cursor).is_err());
    }

    #[test]
    fn rejects_oversized_frame() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut cursor = std::io::Cursor::new(buf);
        let result: io::Result<Request> = read_frame(&mut cursor);
        assert!(result.is_err());
    }

    #[test]
    fn frame_to_json_is_valid_json() {
        let json = frame_to_json(&Request::Status);
        let _: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    }

    #[test]
    fn render_tap_inbound_uses_a_right_arrow() {
        let frame = TapFrame {
            conn: 7,
            direction: TapDirection::Inbound,
            json: r#"{"Status":null}"#.to_owned(),
        };
        assert_eq!(render_tap(&frame), r##"#7 -> {"Status":null}"##);
    }

    #[test]
    fn render_tap_outbound_uses_a_left_arrow() {
        let frame = TapFrame {
            conn: 2,
            direction: TapDirection::Outbound,
            json: r#"{"Result":"Ok"}"#.to_owned(),
        };
        assert_eq!(render_tap(&frame), r##"#2 <- {"Result":"Ok"}"##);
    }

    #[test]
    fn a_tapped_frame_carries_valid_json() {
        // A tap decode of a real request must itself be valid JSON the client can
        // re-parse, since `tap` consumers may pipe the stream into a JSON tool.
        let json = frame_to_json(&Request::Command(CommandRequest {
            spec: CommandSpec::Check { path: None },
            opts: opts(),
            dirty: Vec::new(),
        }));
        let frame = TapFrame { conn: 0, direction: TapDirection::Inbound, json };
        let value: serde_json::Value = serde_json::from_str(&frame.json).expect("valid JSON");
        assert!(value.get("Command").is_some(), "decode should name the request variant: {value}");
    }
}
