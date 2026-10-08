//! The TLS engine backing the `Tls` capability (HTTPS), a thin sans-I/O wrapper
//! over [`rustls`] with the `ring` crypto provider.
//!
//! rustls does **no** socket I/O: it is a pure state machine fed ciphertext and
//! producing ciphertext/plaintext through in-memory buffers. So all the networking
//! stays in Fai over the existing `Net` capability — Fai drives the handshake and
//! shuttles bytes between the socket and this engine. These operations are
//! pure-CPU (no blocking, no parking); the only effect is the secure randomness the
//! handshake consumes, which is why they sit behind a capability.
//!
//! A `Tls` Fai value is a reference-counted heap cell (`KIND_TLS`) whose slot owns a
//! raw `Arc<TlsObject>` (a rustls connection behind a `Mutex`, so a value shared
//! across worker threads is safe); the free path drops that `Arc`. The handshake
//! and record layer, certificate parsing, and chain/hostname verification all live
//! inside rustls (audited), never reimplemented in Fai.

use std::io::{Read, Write};
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, Once};

use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::{
    ClientConfig, ClientConnection, Connection, RootCertStore, ServerConfig, ServerConnection,
};

use crate::Value;

/// State-flag bits returned by [`fai_tls_state`], read by the Fai pump loop.
const STATE_HANDSHAKING: i64 = 1;
const STATE_WANTS_WRITE: i64 = 2;
const STATE_WANTS_READ: i64 = 4;

/// The rustls connection owned by a `Tls` handle cell. Behind a `Mutex` so a handle
/// shared across tasks/workers (biased reference counting) is safe to step.
struct TlsObject {
    conn: Mutex<Connection>,
}

/// Installs the `ring` crypto provider as the process default once, so the rustls
/// config builders (which read the default provider) work without aws-lc-rs.
fn ensure_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // Ignore the result: a duplicate install (e.g. another caller raced) is fine.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Builds the client trust store: the bundled Mozilla roots, plus any extra PEM
/// certificate authorities (for a private CA or a test's self-signed cert).
fn client_roots(extra_pem: Option<&[u8]>) -> Result<RootCertStore, String> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_pem {
        let mut reader = pem;
        for cert in rustls_pemfile::certs(&mut reader) {
            let cert = cert.map_err(|e| format!("reading a root certificate: {e}"))?;
            roots.add(cert).map_err(|e| format!("adding a root certificate: {e}"))?;
        }
    }
    Ok(roots)
}

/// Builds a client TLS session for `hostname`, verifying the server against the
/// bundled roots plus any `extra_roots_pem`.
fn new_client(hostname: &str, extra_roots_pem: Option<&[u8]>) -> Result<TlsObject, String> {
    ensure_provider();
    let roots = client_roots(extra_roots_pem)?;
    let config = ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
    let server_name = ServerName::try_from(hostname.to_owned())
        .map_err(|e| format!("invalid server name: {e}"))?;
    let conn = ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("starting the TLS client: {e}"))?;
    Ok(TlsObject { conn: Mutex::new(Connection::Client(conn)) })
}

/// Builds a server TLS session presenting `cert_pem` (a certificate chain) with
/// `key_pem` (a PKCS#8/PKCS#1/SEC1 private key).
fn new_server(cert_pem: &[u8], key_pem: &[u8]) -> Result<TlsObject, String> {
    ensure_provider();
    let mut cert_reader = cert_pem;
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("reading the certificate chain: {e}"))?;
    if certs.is_empty() {
        return Err("no certificate found in the certificate PEM".to_owned());
    }
    let mut key_reader = key_pem;
    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("reading the private key: {e}"))?
        .ok_or_else(|| "no private key found in the key PEM".to_owned())?;
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("building the TLS server config: {e}"))?;
    let conn = ServerConnection::new(Arc::new(config))
        .map_err(|e| format!("starting the TLS server: {e}"))?;
    Ok(TlsObject { conn: Mutex::new(Connection::Server(conn)) })
}

/// Feeds ciphertext (read from the socket) into the session and advances the state
/// machine. Drains all of `data` into rustls, processing between reads.
fn feed_incoming(obj: &TlsObject, data: &[u8]) -> Result<(), String> {
    let mut conn = obj.conn.lock().expect("tls lock");
    let mut cursor = data;
    while !cursor.is_empty() {
        let n = conn.read_tls(&mut cursor).map_err(|e| format!("TLS read_tls: {e}"))?;
        if n == 0 {
            break;
        }
        conn.process_new_packets().map_err(|e| format!("TLS protocol error: {e}"))?;
    }
    conn.process_new_packets().map_err(|e| format!("TLS protocol error: {e}"))?;
    Ok(())
}

/// Drains the ciphertext the session wants to send (to be written to the socket).
fn take_outgoing(obj: &TlsObject) -> Result<Vec<u8>, String> {
    let mut conn = obj.conn.lock().expect("tls lock");
    let mut out = Vec::new();
    while conn.wants_write() {
        conn.write_tls(&mut out).map_err(|e| format!("TLS write_tls: {e}"))?;
    }
    Ok(out)
}

/// Reads up to `cap` bytes of decrypted application data. An empty result means none
/// is buffered yet (feed more ciphertext) or the peer has closed cleanly.
fn read_plaintext(obj: &TlsObject, cap: usize) -> Result<Vec<u8>, String> {
    let mut conn = obj.conn.lock().expect("tls lock");
    let mut buf = vec![0u8; cap];
    match conn.reader().read(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            Ok(buf)
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Vec::new()),
        Err(e) => Err(format!("TLS read: {e}")),
    }
}

/// Queues as much plaintext as the bounded output buffer accepts. Drain
/// [`take_outgoing`] before retrying the unaccepted suffix, including on zero.
fn write_plaintext(obj: &TlsObject, data: &[u8]) -> Result<usize, String> {
    let mut conn = obj.conn.lock().expect("tls lock");
    conn.writer().write(data).map_err(|e| format!("TLS write: {e}"))
}

/// The state-flag bitmask read by the Fai pump (handshaking / wants-write /
/// wants-read).
fn state(obj: &TlsObject) -> i64 {
    let conn = obj.conn.lock().expect("tls lock");
    let mut flags = 0;
    if conn.is_handshaking() {
        flags |= STATE_HANDSHAKING;
    }
    if conn.wants_write() {
        flags |= STATE_WANTS_WRITE;
    }
    if conn.wants_read() {
        flags |= STATE_WANTS_READ;
    }
    flags
}

// ---------------------------------------------------------------------------
// The `KIND_TLS` handle cell and the C-ABI operations.
// ---------------------------------------------------------------------------

/// Wraps a TLS session as a Fai `KIND_TLS` value owning the `Arc`.
fn tls_handle_value(obj: TlsObject) -> Value {
    let raw = Arc::into_raw(Arc::new(obj)) as usize as i64;
    let p = crate::alloc_obj(crate::HEADER_SIZE + 8, std::ptr::addr_of!(crate::FAI_TLS_DESC));
    // SAFETY: `p` has room for the header and one slot.
    unsafe { crate::write_i64(p, crate::HANDLE_PTR_OFFSET, raw) };
    crate::from_obj(p)
}

/// Releases the `Arc<TlsObject>` a dead TLS cell owned (called by `free_obj`).
pub(crate) fn drop_tls_handle(raw: i64) {
    // SAFETY: `raw` came from `Arc::into_raw` in `tls_handle_value`.
    drop(unsafe { Arc::from_raw(raw as usize as *const TlsObject) });
}

/// Borrows the `Arc<TlsObject>` from a TLS value without consuming a reference.
fn tls_of(v: Value) -> ManuallyDrop<Arc<TlsObject>> {
    // SAFETY: `v` is a live `KIND_TLS` cell whose slot holds the `Arc` pointer.
    let raw = unsafe { crate::read_i64(crate::as_obj(v), crate::HANDLE_PTR_OFFSET) };
    ManuallyDrop::new(unsafe { Arc::from_raw(raw as usize as *const TlsObject) })
}

/// Builds `Ok v` (tag 0).
fn ok_result(v: Value) -> Value {
    // SAFETY: one owned field moves into the `Ok` cell.
    unsafe { crate::fai_make_data(0, 1, [v].as_ptr()) }
}

/// Builds `Err <message>` (tag 1).
fn err_result(msg: &str) -> Value {
    let s = crate::make_string(msg.as_bytes());
    // SAFETY: one owned field moves into the `Err` cell.
    unsafe { crate::fai_make_data(1, 1, [s].as_ptr()) }
}

/// `Tls.client`: start a client session verifying `hostname` against the bundled
/// roots. Returns `Result Tls String`. Consumes `hostname`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_client(hostname: Value) -> Value {
    // SAFETY: `hostname` is a boxed `String`.
    let h = unsafe { crate::string_str(hostname) }.to_owned();
    crate::fai_drop(hostname);
    match new_client(&h, None) {
        Ok(obj) => ok_result(tls_handle_value(obj)),
        Err(e) => err_result(&e),
    }
}

/// `Tls.clientWithRoots`: like `client`, additionally trusting the certificate
/// authorities in `roots_pem` (a private CA, or a test's self-signed cert). Returns
/// `Result Tls String`. Consumes both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_client_with_roots(hostname: Value, roots_pem: Value) -> Value {
    // SAFETY: `hostname` is a boxed `String`, `roots_pem` a boxed `Bytes`.
    let h = unsafe { crate::string_str(hostname) }.to_owned();
    let result = {
        let pem = unsafe { crate::bytes_bytes(roots_pem) };
        new_client(&h, Some(pem))
    };
    crate::fai_drop(hostname);
    crate::fai_drop(roots_pem);
    match result {
        Ok(obj) => ok_result(tls_handle_value(obj)),
        Err(e) => err_result(&e),
    }
}

/// `Tls.server`: start a server session presenting `cert_pem` (chain) with
/// `key_pem`. Returns `Result Tls String`. Consumes both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_server(cert_pem: Value, key_pem: Value) -> Value {
    let result = {
        // SAFETY: both are boxed `Bytes`, valid until dropped below.
        let cert = unsafe { crate::bytes_bytes(cert_pem) };
        let key = unsafe { crate::bytes_bytes(key_pem) };
        new_server(cert, key)
    };
    crate::fai_drop(cert_pem);
    crate::fai_drop(key_pem);
    match result {
        Ok(obj) => ok_result(tls_handle_value(obj)),
        Err(e) => err_result(&e),
    }
}

/// `Tls.feedIncoming`: feed ciphertext (read from the socket) into the session.
/// Returns `Result Unit String`. Consumes both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_feed_incoming(tls: Value, bytes: Value) -> Value {
    let result = {
        let obj = tls_of(tls);
        // SAFETY: `bytes` is a boxed `Bytes`, valid until dropped below.
        let data = unsafe { crate::bytes_bytes(bytes) };
        feed_incoming(&obj, data)
    };
    crate::fai_drop(tls);
    crate::fai_drop(bytes);
    match result {
        Ok(()) => ok_result(crate::FAI_UNIT),
        Err(e) => err_result(&e),
    }
}

/// `Tls.takeOutgoing`: drain the ciphertext the session wants to send. Returns
/// `Result Bytes String` (empty when nothing is pending). Consumes `tls`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_take_outgoing(tls: Value) -> Value {
    let result = {
        let obj = tls_of(tls);
        take_outgoing(&obj)
    };
    crate::fai_drop(tls);
    match result {
        Ok(buf) => ok_result(crate::make_bytes(&buf)),
        Err(e) => err_result(&e),
    }
}

/// `Tls.readPlaintext`: read up to `max` bytes of decrypted application data.
/// Returns `Result Bytes String` (empty when none is buffered yet). Consumes `tls`;
/// `max` is an `Int`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_read_plaintext(tls: Value, max: Value) -> Value {
    let cap = crate::unbox_int(max).max(0) as usize;
    let result = {
        let obj = tls_of(tls);
        read_plaintext(&obj, cap)
    };
    crate::fai_drop(tls);
    crate::fai_drop(max);
    match result {
        Ok(buf) => ok_result(crate::make_bytes(&buf)),
        Err(e) => err_result(&e),
    }
}

/// `Tls.writePlaintext`: queue plaintext to be encrypted. Returns the accepted
/// byte count as `Result Int String` (zero when full). Consumes both operands.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_write_plaintext(tls: Value, bytes: Value) -> Value {
    let result = {
        let obj = tls_of(tls);
        // SAFETY: `bytes` is a boxed `Bytes`, valid until dropped below.
        let data = unsafe { crate::bytes_bytes(bytes) };
        write_plaintext(&obj, data)
    };
    crate::fai_drop(tls);
    crate::fai_drop(bytes);
    match result {
        Ok(count) => ok_result(crate::make_int(count as i64)),
        Err(e) => err_result(&e),
    }
}

/// `Tls.state`: the state-flag bitmask (bit 0 handshaking, bit 1 wants-write, bit 2
/// wants-read) as an `Int`. Consumes `tls`.
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_state(tls: Value) -> Value {
    let flags = {
        let obj = tls_of(tls);
        state(&obj)
    };
    crate::fai_drop(tls);
    crate::fai_box_int(flags)
}

/// `Tls.close`: send a close-notify alert and release this reference. Returns
/// `Unit` (the close-notify ciphertext is produced for the caller's last
/// `takeOutgoing` if it chooses to flush it).
#[unsafe(no_mangle)]
pub extern "C" fn fai_tls_close(tls: Value) -> Value {
    {
        let obj = tls_of(tls);
        obj.conn.lock().expect("tls lock").send_close_notify();
    }
    crate::fai_drop(tls);
    crate::FAI_UNIT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generates an ephemeral self-signed cert+key (PEM) for `localhost`.
    fn self_signed() -> (Vec<u8>, Vec<u8>) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])
            .expect("generate self-signed cert");
        (cert.cert.pem().into_bytes(), cert.key_pair.serialize_pem().into_bytes())
    }

    fn connected_pair() -> (TlsObject, TlsObject) {
        let (cert_pem, key_pem) = self_signed();
        let client = new_client("localhost", Some(&cert_pem)).expect("client");
        let server = new_server(&cert_pem, &key_pem).expect("server");

        // Pump until neither side has ciphertext to send and both finished handshaking.
        for _ in 0..20 {
            let c2s = take_outgoing(&client).expect("client out");
            if !c2s.is_empty() {
                feed_incoming(&server, &c2s).expect("server feed");
            }
            let s2c = take_outgoing(&server).expect("server out");
            if !s2c.is_empty() {
                feed_incoming(&client, &s2c).expect("client feed");
            }
            if !client.conn.lock().unwrap().is_handshaking()
                && !server.conn.lock().unwrap().is_handshaking()
                && c2s.is_empty()
                && s2c.is_empty()
            {
                break;
            }
        }
        assert!(!client.conn.lock().unwrap().is_handshaking(), "client handshake completed");
        assert!(!server.conn.lock().unwrap().is_handshaking(), "server handshake completed");
        (client, server)
    }

    #[test]
    fn client_and_server_complete_a_handshake_and_exchange_data() {
        let (client, server) = connected_pair();

        // Client writes application data; flush its ciphertext to the server.
        assert_eq!(write_plaintext(&client, b"hello tls").expect("write"), 9);
        let app = take_outgoing(&client).expect("client app out");
        feed_incoming(&server, &app).expect("server feed app");
        let got = read_plaintext(&server, 64).expect("server read");
        assert_eq!(&got, b"hello tls", "the server decrypted the client's data");
    }

    fn transfer(client: &TlsObject, server: &TlsObject, data: &[u8]) -> Vec<u8> {
        let mut offset = 0;
        let mut received = Vec::new();
        while offset < data.len() {
            let accepted = write_plaintext(client, &data[offset..]).expect("write progress");
            assert!(accepted > 0 && accepted <= data.len() - offset);
            offset += accepted;
            let cipher = take_outgoing(client).expect("ciphertext");
            assert!(cipher.len() <= 70_000, "outgoing data stays bounded");
            received.extend(receive_cipher(server, &cipher));
        }
        received
    }

    fn receive_cipher(server: &TlsObject, cipher: &[u8]) -> Vec<u8> {
        let mut received = Vec::new();
        // Mirror bounded socket reads, draining plaintext between input chunks.
        for chunk in cipher.chunks(16_384) {
            feed_incoming(server, chunk).expect("feed peer");
            loop {
                let plain = read_plaintext(server, 4096).expect("read peer");
                if plain.is_empty() {
                    break;
                }
                received.extend_from_slice(&plain);
            }
        }
        received
    }

    #[track_caller]
    fn large_roundtrip(size: usize) {
        let (client, server) = connected_pair();
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        assert_eq!(transfer(&client, &server, &data), data);
    }

    #[test]
    fn writes_below_output_capacity_round_trip() {
        large_roundtrip(65_535);
    }

    #[test]
    fn writes_at_output_capacity_round_trip() {
        large_roundtrip(65_536);
    }

    #[test]
    fn writes_above_output_capacity_round_trip() {
        large_roundtrip(65_537);
    }

    #[test]
    fn megabyte_writes_round_trip_with_bounded_buffering() {
        large_roundtrip(1_048_576);
    }

    #[test]
    fn empty_write_reports_zero_without_queuing_ciphertext() {
        let (client, _) = connected_pair();
        assert_eq!(write_plaintext(&client, b"").unwrap(), 0);
        assert!(take_outgoing(&client).unwrap().is_empty());
    }

    #[test]
    fn full_writer_reports_zero_and_resumes_after_drain() {
        let (client, server) = connected_pair();
        let data = vec![42; 1_048_576];
        let accepted = write_plaintext(&client, &data).unwrap();
        assert!(accepted > 0 && accepted < data.len());
        assert_eq!(write_plaintext(&client, &data[accepted..]).unwrap(), 0);
        let cipher = take_outgoing(&client).unwrap();
        assert_eq!(receive_cipher(&server, &cipher), data[..accepted]);
        assert_eq!(transfer(&client, &server, &data[accepted..]), data[accepted..]);
    }

    #[test]
    fn sequential_large_writes_preserve_byte_order() {
        let (client, server) = connected_pair();
        let first = vec![17; 131_072];
        let second = vec![93; 131_072];
        assert_eq!(transfer(&client, &server, &first), first);
        assert_eq!(transfer(&client, &server, &second), second);
    }

    #[test]
    fn small_output_buffer_reports_partial_progress() {
        let (client, server) = connected_pair();
        client.conn.lock().unwrap().set_buffer_limit(Some(1024));
        let data = vec![123; 65_537];
        assert_eq!(transfer(&client, &server, &data), data);
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(32))]
            #[test]
            fn bounded_writes_roundtrip_arbitrary_bytes(
                data in proptest::collection::vec(any::<u8>(), 0..131_073),
                capacity in 256usize..65_537,
            ) {
                let (client, server) = connected_pair();
                client.conn.lock().unwrap().set_buffer_limit(Some(capacity));
                prop_assert_eq!(transfer(&client, &server, &data), data);
            }
        }
    }

    #[test]
    fn client_rejects_an_untrusted_certificate() {
        // Without trusting the self-signed cert, the client's handshake fails when it
        // processes the server's certificate (chain verification rejects it).
        let (cert_pem, key_pem) = self_signed();
        let client = new_client("localhost", None).expect("client"); // bundled roots only
        let server = new_server(&cert_pem, &key_pem).expect("server");

        let mut rejected = false;
        for _ in 0..20 {
            let c2s = take_outgoing(&client).unwrap_or_default();
            if !c2s.is_empty() {
                let _ = feed_incoming(&server, &c2s);
            }
            let s2c = take_outgoing(&server).unwrap_or_default();
            if !s2c.is_empty() && feed_incoming(&client, &s2c).is_err() {
                rejected = true;
                break;
            }
            if c2s.is_empty() && s2c.is_empty() {
                break;
            }
        }
        assert!(rejected, "the client rejected the untrusted self-signed certificate");
    }
}
