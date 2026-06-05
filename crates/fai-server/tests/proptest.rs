//! Property tests for the wire framing: arbitrary payloads must survive an
//! encode/decode round-trip unchanged.

use fai_driver::Rendered;
use fai_server::protocol::{OutputStream, Response, ServerMessage, read_frame, write_frame};
use proptest::prelude::*;

fn round_trip(message: &ServerMessage) -> ServerMessage {
    let mut buf = Vec::new();
    write_frame(&mut buf, message).unwrap();
    let mut cursor = std::io::Cursor::new(buf);
    read_frame(&mut cursor).unwrap()
}

proptest! {
    #[test]
    fn rendered_result_round_trips(stdout in ".*", stderr in ".*", exit in any::<i32>()) {
        let message = ServerMessage::Result(Response::Command(Rendered { stdout, stderr, exit }));
        prop_assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn output_chunk_round_trips(
        chunk in proptest::collection::vec(any::<u8>(), 0..4096),
        stderr in any::<bool>(),
    ) {
        let stream = if stderr { OutputStream::Stderr } else { OutputStream::Stdout };
        let message = ServerMessage::Output { stream, chunk };
        prop_assert_eq!(round_trip(&message), message);
    }

    #[test]
    fn error_message_round_trips(message in ".*") {
        let message = ServerMessage::Result(Response::Error(message));
        prop_assert_eq!(round_trip(&message), message);
    }
}
