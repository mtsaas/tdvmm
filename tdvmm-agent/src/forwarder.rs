//! The guest-side egress forwarder: a SOCKS5h proxy that terminates each
//! guest-initiated TCP connection and relays it over the COM4 / ttyS3 mux to the
//! host-side `EgressBackend`. It is the guest peer of `crate::egress` on the host
//! and the exact counterpart of the control-channel agent — but on the fourth
//! UART and speaking the binary mux ([`tdvmm_proto::egress`]) instead of line
//! JSON.
//!
//! Started by the guest init ONLY on `tdvmm.egress=1`. It listens on
//! `0.0.0.0:1080`; workload containers reach it at their bridge gateway
//! (`socks5h://<gateway>:1080`). SOCKS5**h** means the guest sends the destination
//! *hostname* — the host resolves it — so the guest needs no resolver and the
//! closed-world topology is preserved (egress leaves only through this one
//! enumerable channel).
//!
//! ## Correct-by-construction framing (the malformed-frame obligation)
//!
//! A malformed mux frame is fatal on the host by design (a framing loss on a
//! delimiter-free byte stream is unrecoverable), so the forwarder MUST NEVER emit
//! one. Three properties guarantee it:
//!
//! * **One writer.** Every byte toward ttyS3 goes through [`TtyWriter::send`],
//!   which holds a mutex across a whole `encode`-then-`write_all`. Frames from
//!   concurrent connections can never interleave on the wire.
//! * **Whole frames only.** [`TtyWriter::send`] encodes one [`EgressFrame`] into a
//!   scratch buffer and writes it in full — a partial write cannot leave a torn
//!   header on the line.
//! * **Every field is in range before encode.** A SOCKS domain length is a single
//!   byte (≤ 255 = [`tdvmm_proto::egress::EGRESS_MAX_HOST_LEN`]); an IP literal
//!   formats far shorter; and client bytes are chunked to
//!   [`tdvmm_proto::egress::EGRESS_MAX_PAYLOAD`] before framing. So
//!   [`tdvmm_proto::egress::encode`] never returns its length errors, and the only
//!   remaining failure is a real I/O error on the UART — surfaced, never ignored.
//!
//! The bytes a container speaks to the SOCKS port are untrusted: the negotiator
//! validates every field, bounds every read, and rejects a malformed handshake
//! cleanly (a SOCKS error reply or a dropped connection) — it never panics and
//! never turns bad client input into a bad mux frame.
//!
//! ## Threads and teardown
//!
//! No `libc`, no `epoll` (the agent's dependency rule) — the design is blocking
//! std threads:
//!
//! * one **demux reader** owns the ttyS3 read side, decodes host→guest frames, and
//!   routes each to its stream's channel (a torn-down stream's frames are dropped);
//! * each accepted connection runs a handler that negotiates SOCKS, opens the mux
//!   stream, then relays with an **up** thread (client → host `DATA`) while the
//!   handler itself drains the **down** direction (host `DATA` → client).
//!
//! A stream is a request/response relay: when the remote closes its read side
//! (host `CLOSE`) the response has been fully delivered, so the forwarder tears the
//! whole stream down — half-closing toward the host with a `CLOSE`, which lets the
//! host reach quiescence (both directions closed) and the phase gate re-engage
//! fast-forward. Client bytes still queued in that instant are not forwarded; a
//! client that keeps uploading after the server has responded and closed is not
//! supported (documented cross-stream/HOL simplicity, matching the host).

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use tdvmm_proto::egress::{
    decode, encode, EgressFrame, EgressReason, EGRESS_MAX_PAYLOAD,
};

use crate::sys;

/// The SOCKS port the forwarder listens on inside the guest.
const LISTEN_ADDR: &str = "0.0.0.0:1080";

/// One client-socket read's scratch buffer. Comfortably below
/// [`EGRESS_MAX_PAYLOAD`], so a read is always one `DATA` frame (the chunker below
/// is then a no-op that stays correct if this ever grows).
const READ_CHUNK: usize = 16 * 1024;

/// Grant host→guest window credit back once this many delivered bytes have gone
/// un-acknowledged. Half the host's initial [`crate`]-side window, so a large
/// response streams without the credit ever reaching zero, while a small one never
/// pays for a `WINDOW` frame at all.
const WINDOW_GRANT_THRESHOLD: usize = 16 * 1024;

// ============================================================================
// Errors
// ============================================================================

/// A fatal forwarder setup/transport failure. Per-connection and per-frame
/// problems are handled in-band (a SOCKS error reply, a dropped stream) and never
/// surface here; only losing the transport does.
#[derive(Debug)]
enum ForwarderError {
    /// A host-resource I/O failure, tagged with the operation; the underlying
    /// [`io::Error`] is the `source`.
    Io { what: String, source: io::Error },
    /// Setting raw mode on the ttyS3 fd failed (errno); the mux would otherwise be
    /// mangled by the tty line discipline.
    RawMode { errno: i64 },
}

impl ForwarderError {
    fn io(what: impl Into<String>, source: io::Error) -> Self {
        ForwarderError::Io { what: what.into(), source }
    }
}

impl fmt::Display for ForwarderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForwarderError::Io { what, source } => write!(f, "{what}: {source}"),
            ForwarderError::RawMode { errno } => {
                write!(f, "cannot set raw mode on the egress tty: errno {errno}")
            }
        }
    }
}

impl std::error::Error for ForwarderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ForwarderError::Io { source, .. } => Some(source),
            ForwarderError::RawMode { .. } => None,
        }
    }
}

// ============================================================================
// The ttyS3 writer — the single, atomic, whole-frame egress path
// ============================================================================

/// The one writer to ttyS3. Every mux frame the guest emits goes through
/// [`send`](Self::send), which holds the mutex across encoding and the whole
/// `write_all`, so frames from concurrent connections never interleave and a torn
/// header can never reach the host. Shared as `Arc<TtyWriter>`.
struct TtyWriter {
    tty: Mutex<Box<dyn Write + Send>>,
}

impl TtyWriter {
    fn new(w: impl Write + Send + 'static) -> Self {
        TtyWriter { tty: Mutex::new(Box::new(w)) }
    }

    /// Encode `frame` and write it whole. Every caller passes in-range fields (see
    /// the module doc), so `encode` never fails; a returned error is a real UART
    /// I/O failure.
    fn send(&self, frame: &EgressFrame<'_>) -> io::Result<()> {
        let mut scratch = Vec::with_capacity(16);
        // In-range by construction: map the (impossible) encode error to I/O so it
        // is surfaced, never silently dropped, and never panics.
        encode(frame, &mut scratch).map_err(io::Error::other)?;
        let mut tty = self.tty.lock().expect("egress tty writer mutex poisoned");
        tty.write_all(&scratch)?;
        tty.flush()
    }

    /// A guest→host `DATA` relay: split `payload` into `EGRESS_MAX_PAYLOAD` frames
    /// (a no-op for a [`READ_CHUNK`]-sized read) so every emitted `DATA` frame is
    /// in range.
    fn send_data(&self, stream: u16, payload: &[u8]) -> io::Result<()> {
        for chunk in payload.chunks(EGRESS_MAX_PAYLOAD) {
            self.send(&EgressFrame::Data { stream, payload: chunk })?;
        }
        Ok(())
    }
}

// ============================================================================
// Host → guest frame routing
// ============================================================================

/// A host→guest event delivered to a stream's handler by the demux reader.
enum Inbound {
    /// The stream connected (`OPEN_OK`).
    Connected,
    /// The open failed (`OPEN_ERR`), with the reason for the SOCKS reply.
    Failed(EgressReason),
    /// Response bytes for the client (`DATA`).
    Data(Vec<u8>),
    /// The remote closed its read side (`CLOSE`): no more response bytes.
    Close,
    /// Abortive close (`RST`).
    Rst,
}

/// Live streams: mux id → the channel feeding that stream's handler. The single
/// source of truth for which ids are in use, so id allocation and routing agree.
type Registry = Arc<Mutex<HashMap<u16, Sender<Inbound>>>>;

/// The demux reader: own the ttyS3 read side, decode every host→guest frame, and
/// route it to the target stream (dropping frames for a stream that has already
/// torn down, and the two guest→host-only frame types the host never sends).
///
/// A decode error means the mux byte stream lost framing — unrecoverable on a
/// delimiter-free stream — so it stops the whole process: the transport is dead
/// and every open stream is now unreliable.
fn demux_loop(mut tty: impl Read, reg: Registry) {
    let mut buf: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut tmp = [0u8; READ_CHUNK];
    loop {
        let n = match tty.read(&mut tmp) {
            Ok(0) => {
                eprintln!("tdvmm-agent[forward]: egress tty reached EOF; stopping");
                std::process::exit(1);
            }
            Ok(n) => n,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("tdvmm-agent[forward]: egress tty read failed: {e}; stopping");
                std::process::exit(1);
            }
        };
        buf.extend_from_slice(&tmp[..n]);

        let mut off = 0;
        loop {
            match decode(&buf[off..]) {
                Ok(Some(d)) => {
                    route(d.frame, &reg);
                    off += d.consumed;
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("tdvmm-agent[forward]: egress mux framing lost: {e}; stopping");
                    std::process::exit(1);
                }
            }
        }
        buf.drain(..off);
    }
}

/// Route one decoded host→guest frame to its stream's handler.
fn route(frame: EgressFrame<'_>, reg: &Registry) {
    let (stream, msg) = match frame {
        EgressFrame::OpenOk { stream } => (stream, Inbound::Connected),
        EgressFrame::OpenErr { stream, reason } => (stream, Inbound::Failed(reason)),
        EgressFrame::Data { stream, payload } => (stream, Inbound::Data(payload.to_vec())),
        EgressFrame::Close { stream } => (stream, Inbound::Close),
        EgressFrame::Rst { stream } => (stream, Inbound::Rst),
        // OPEN and WINDOW are guest→host only; the host never sends them. Ignore
        // rather than tear the transport down over one stray frame.
        EgressFrame::Open { .. } | EgressFrame::Window { .. } => return,
    };
    if let Some(tx) = reg.lock().expect("egress registry mutex poisoned").get(&stream) {
        // A closed receiver (the handler has torn down) just drops the frame.
        let _ = tx.send(msg);
    }
}

// ============================================================================
// SOCKS5h negotiation (hand-rolled, untrusted input)
// ============================================================================

/// The destination a `CONNECT` names: a hostname (SOCKS5h) or an IP literal,
/// formatted as the string the host resolver takes.
struct Socks5Target {
    host: String,
    port: u16,
}

/// A clean SOCKS negotiation rejection. Carries the SOCKS reply code to send back
/// (or `None` when the greeting itself was unusable and no reply is warranted).
#[derive(Debug, PartialEq, Eq)]
enum SocksError {
    /// The bytes were not a SOCKS5 handshake we can serve; the `u8` is the reply
    /// code to send (0 = drop without replying).
    Rejected(u8),
    /// The client hung up / an I/O error mid-handshake.
    Io,
}

// SOCKS5 reply codes (RFC 1928 §6).
const SOCKS_OK: u8 = 0x00;
const SOCKS_GENERAL_FAILURE: u8 = 0x01;
const SOCKS_NET_UNREACHABLE: u8 = 0x03;
const SOCKS_HOST_UNREACHABLE: u8 = 0x04;
const SOCKS_CONN_REFUSED: u8 = 0x05;
const SOCKS_CMD_UNSUPPORTED: u8 = 0x07;
const SOCKS_ATYP_UNSUPPORTED: u8 = 0x08;

impl From<io::Error> for SocksError {
    fn from(_: io::Error) -> Self {
        SocksError::Io
    }
}

/// Run the SOCKS5 handshake on `s`: method negotiation (no-auth only) then a
/// `CONNECT` request, returning the destination. Every field is length-bounded by
/// its byte width, so a hostile client cannot force an unbounded read; malformed
/// input is rejected, never panicked on.
fn socks5_negotiate<S: Read + Write>(s: &mut S) -> Result<Socks5Target, SocksError> {
    // Greeting: VER, NMETHODS, METHODS[NMETHODS].
    let mut head = [0u8; 2];
    s.read_exact(&mut head)?;
    if head[0] != 0x05 {
        return Err(SocksError::Rejected(0)); // not SOCKS5: no meaningful reply.
    }
    let nmethods = head[1] as usize;
    let mut methods = [0u8; 255];
    s.read_exact(&mut methods[..nmethods])?;
    if !methods[..nmethods].contains(&0x00) {
        // No acceptable method: reply 0xFF and drop.
        let _ = s.write_all(&[0x05, 0xFF]);
        return Err(SocksError::Rejected(0));
    }
    s.write_all(&[0x05, 0x00])?; // choose no-auth.

    // Request: VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT.
    let mut req = [0u8; 4];
    s.read_exact(&mut req)?;
    if req[0] != 0x05 {
        return Err(SocksError::Rejected(SOCKS_GENERAL_FAILURE));
    }
    if req[1] != 0x01 {
        // Only CONNECT; BIND/UDP-ASSOCIATE are not offered.
        return Err(SocksError::Rejected(SOCKS_CMD_UNSUPPORTED));
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a)?;
            format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
        }
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len)?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name)?;
            // A hostname is ASCII/IDNA bytes; reject non-UTF-8 cleanly.
            match String::from_utf8(name) {
                Ok(h) => h,
                Err(_) => return Err(SocksError::Rejected(SOCKS_GENERAL_FAILURE)),
            }
        }
        0x04 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a)?;
            let seg = |i: usize| u16::from_be_bytes([a[i], a[i + 1]]);
            format!(
                "{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}:{:x}",
                seg(0), seg(2), seg(4), seg(6), seg(8), seg(10), seg(12), seg(14)
            )
        }
        _ => return Err(SocksError::Rejected(SOCKS_ATYP_UNSUPPORTED)),
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port)?;
    let port = u16::from_be_bytes(port);
    if host.is_empty() || port == 0 {
        return Err(SocksError::Rejected(SOCKS_GENERAL_FAILURE));
    }
    Ok(Socks5Target { host, port })
}

/// Write a SOCKS5 reply with code `rep` and a `0.0.0.0:0` bound address (the mux
/// hides the real one, and clients ignore BND for CONNECT).
fn socks5_reply<W: Write>(w: &mut W, rep: u8) -> io::Result<()> {
    w.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
}

/// Map a host-side open failure to the closest SOCKS reply code.
fn reply_code_for(reason: EgressReason) -> u8 {
    match reason {
        EgressReason::ResolveFailed => SOCKS_HOST_UNREACHABLE,
        EgressReason::ConnectRefused => SOCKS_CONN_REFUSED,
        EgressReason::Unreachable => SOCKS_NET_UNREACHABLE,
        EgressReason::StreamLimit
        | EgressReason::ProtocolError
        | EgressReason::Overrun
        | EgressReason::Other(_) => SOCKS_GENERAL_FAILURE,
    }
}

// ============================================================================
// Per-connection relay
// ============================================================================

/// Allocate a free mux stream id and register its inbound channel atomically, so
/// the id is reserved the instant it is chosen.
fn register(reg: &Registry, counter: &AtomicU32) -> (u16, Receiver<Inbound>) {
    let (tx, rx) = mpsc::channel();
    let mut map = reg.lock().expect("egress registry mutex poisoned");
    let id = loop {
        let cand = counter.fetch_add(1, Ordering::Relaxed) as u16;
        if !map.contains_key(&cand) {
            break cand;
        }
    };
    map.insert(id, tx);
    (id, rx)
}

/// Handle one accepted SOCKS client end to end: negotiate, open the mux stream,
/// then relay until the stream tears down. Errors are logged and the connection
/// dropped — never fatal to the forwarder.
fn handle_connection(mut client: TcpStream, reg: Registry, counter: Arc<AtomicU32>, writer: Arc<TtyWriter>) {
    let target = match socks5_negotiate(&mut client) {
        Ok(t) => t,
        Err(SocksError::Rejected(rep)) => {
            if rep != 0 {
                let _ = socks5_reply(&mut client, rep);
            }
            return;
        }
        Err(SocksError::Io) => return,
    };

    let (id, rx) = register(&reg, &counter);

    // Open the stream. A UART write failure means the transport is gone; drop.
    let open = EgressFrame::Open { stream: id, port: target.port, host: target.host.as_bytes() };
    if writer.send(&open).is_err() {
        reg.lock().expect("egress registry mutex poisoned").remove(&id);
        return;
    }

    // Await the connect verdict.
    match rx.recv() {
        Ok(Inbound::Connected) => {
            if socks5_reply(&mut client, SOCKS_OK).is_err() {
                teardown(&reg, id, &writer, &client);
                return;
            }
        }
        Ok(Inbound::Failed(reason)) => {
            let _ = socks5_reply(&mut client, reply_code_for(reason));
            reg.lock().expect("egress registry mutex poisoned").remove(&id);
            return;
        }
        // Any other verdict (or the demux gone) before connect: give up.
        _ => {
            let _ = socks5_reply(&mut client, SOCKS_GENERAL_FAILURE);
            reg.lock().expect("egress registry mutex poisoned").remove(&id);
            return;
        }
    }

    // Relay. The up thread reads the client and frames guest→host DATA; this
    // thread drains host→guest DATA to the client.
    let up_client = match client.try_clone() {
        Ok(c) => c,
        Err(_) => {
            teardown(&reg, id, &writer, &client);
            return;
        }
    };
    let up_writer = Arc::clone(&writer);
    let up = thread::spawn(move || relay_up(up_client, id, &up_writer));

    relay_down(&mut client, &rx, id, &writer);

    // The down direction ended (remote closed, reset, or a client write error).
    // Unblock the up thread's read and let it finish (it emits the final CLOSE, so
    // the host reaches quiescence), then deregister the stream.
    let _ = client.shutdown(Shutdown::Both);
    let _ = up.join();
    reg.lock().expect("egress registry mutex poisoned").remove(&id);
}

/// Client → host: read the client socket and frame each read as `DATA`; on the
/// client's EOF send `CLOSE` (half-close), on error send `RST`.
fn relay_up(mut client: TcpStream, id: u16, writer: &TtyWriter) {
    let mut buf = [0u8; READ_CHUNK];
    loop {
        match client.read(&mut buf) {
            Ok(0) => {
                let _ = writer.send(&EgressFrame::Close { stream: id });
                return;
            }
            Ok(n) => {
                if writer.send_data(id, &buf[..n]).is_err() {
                    return; // transport gone
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => {
                let _ = writer.send(&EgressFrame::Rst { stream: id });
                return;
            }
        }
    }
}

/// Host → client: deliver `DATA` to the client socket, granting window credit back
/// as bytes drain; on host `CLOSE` half-close the client (its `read` sees EOF); on
/// `RST` or a client write error, stop (the caller resets the socket).
fn relay_down(client: &mut TcpStream, rx: &Receiver<Inbound>, id: u16, writer: &TtyWriter) {
    let mut ungranted = 0usize;
    loop {
        match rx.recv() {
            Ok(Inbound::Data(bytes)) => {
                if client.write_all(&bytes).is_err() {
                    let _ = writer.send(&EgressFrame::Rst { stream: id });
                    return;
                }
                ungranted += bytes.len();
                if ungranted >= WINDOW_GRANT_THRESHOLD {
                    // In range: threshold-bounded, well within u32.
                    let credit = ungranted as u32;
                    let _ = writer.send(&EgressFrame::Window { stream: id, credit });
                    ungranted = 0;
                }
            }
            Ok(Inbound::Close) => {
                let _ = client.shutdown(Shutdown::Write);
                return;
            }
            Ok(Inbound::Rst) => return,
            // A duplicate connect verdict cannot happen post-connect; ignore.
            Ok(Inbound::Connected) | Ok(Inbound::Failed(_)) => {}
            // The demux reader is gone: the transport is dead.
            Err(_) => return,
        }
    }
}

/// Reset a stream toward the host and drop it from the registry (the pre-relay
/// give-up path: the stream connected but the client side could not be set up).
fn teardown(reg: &Registry, id: u16, writer: &TtyWriter, client: &TcpStream) {
    let _ = writer.send(&EgressFrame::Rst { stream: id });
    let _ = client.shutdown(Shutdown::Both);
    reg.lock().expect("egress registry mutex poisoned").remove(&id);
}

// ============================================================================
// Entry point
// ============================================================================

/// Run the forwarder: open ttyS3, put it in raw mode, start the demux reader, and
/// serve SOCKS on `0.0.0.0:1080` forever. Returns [`ExitCode::FAILURE`] only on a
/// fatal setup error (the transport or the listener); otherwise it never returns.
pub fn run() -> ExitCode {
    match try_run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tdvmm-agent[forward]: fatal: {e}");
            ExitCode::FAILURE
        }
    }
}

fn try_run() -> Result<(), ForwarderError> {
    let dev = std::env::var("TDVMM_EGRESS_TTY").unwrap_or_else(|_| "/dev/ttyS3".to_string());
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&dev)
        .map_err(|e| ForwarderError::io(format!("opening {dev}"), e))?;

    // Raw mode is REQUIRED — the default line discipline would ECHO and translate
    // the binary mux, corrupting every frame (same rationale as the ttyS1 agent).
    sys::set_raw(tty.as_raw_fd()).map_err(|errno| ForwarderError::RawMode { errno })?;

    let read_half = tty
        .try_clone()
        .map_err(|e| ForwarderError::io("duplicating the egress tty", e))?;
    let writer = Arc::new(TtyWriter::new(tty));
    let reg: Registry = Arc::new(Mutex::new(HashMap::new()));
    let counter = Arc::new(AtomicU32::new(1));

    let demux_reg = Arc::clone(&reg);
    thread::spawn(move || demux_loop(read_half, demux_reg));

    let listen = std::env::var("TDVMM_EGRESS_LISTEN").unwrap_or_else(|_| LISTEN_ADDR.to_string());
    let listener = TcpListener::bind(&listen)
        .map_err(|e| ForwarderError::io(format!("binding SOCKS listener on {listen}"), e))?;
    eprintln!("tdvmm-agent[forward]: SOCKS5h proxy on {listen} over {dev}");

    for conn in listener.incoming() {
        match conn {
            Ok(client) => {
                let reg = Arc::clone(&reg);
                let counter = Arc::clone(&counter);
                let writer = Arc::clone(&writer);
                thread::spawn(move || handle_connection(client, reg, counter, writer));
            }
            // A transient accept error must not kill the proxy.
            Err(e) => eprintln!("tdvmm-agent[forward]: accept failed: {e}"),
        }
    }
    Ok(())
}

// ============================================================================
// Tests — untrusted-input negotiation + correct-by-construction framing.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A read source + a write sink over in-memory buffers: exercises the real
    /// negotiator I/O paths without a socket.
    struct Duplex {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Duplex {
        fn new(input: Vec<u8>) -> Self {
            Duplex { input: Cursor::new(input), output: Vec::new() }
        }
    }
    impl Read for Duplex {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.input.read(out)
        }
    }
    impl Write for Duplex {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Greeting (no-auth) + a CONNECT request to `host:port` via ATYP=domain.
    fn connect_domain(host: &str, port: u16) -> Vec<u8> {
        let mut v = vec![0x05, 0x01, 0x00]; // greeting: 1 method, no-auth
        v.extend_from_slice(&[0x05, 0x01, 0x00, 0x03]); // VER CONNECT RSV ATYP=domain
        v.push(host.len() as u8);
        v.extend_from_slice(host.as_bytes());
        v.extend_from_slice(&port.to_be_bytes());
        v
    }

    #[test]
    fn negotiates_domain_connect_and_chooses_no_auth() {
        let mut d = Duplex::new(connect_domain("example.com", 443));
        let t = socks5_negotiate(&mut d).expect("valid handshake");
        assert_eq!(t.host, "example.com");
        assert_eq!(t.port, 443);
        // The method reply (no-auth) was written before the request was read.
        assert_eq!(&d.output[..2], &[0x05, 0x00]);
    }

    #[test]
    fn negotiates_ipv4_literal() {
        let mut v = vec![0x05, 0x01, 0x00];
        v.extend_from_slice(&[0x05, 0x01, 0x00, 0x01, 127, 0, 0, 1]);
        v.extend_from_slice(&80u16.to_be_bytes());
        let mut d = Duplex::new(v);
        let t = socks5_negotiate(&mut d).unwrap();
        assert_eq!(t.host, "127.0.0.1");
        assert_eq!(t.port, 80);
    }

    #[test]
    fn rejects_non_socks5_greeting_without_reply() {
        let mut d = Duplex::new(vec![0x04, 0x01, 0x00]); // SOCKS4
        assert!(matches!(socks5_negotiate(&mut d), Err(SocksError::Rejected(0))));
    }

    #[test]
    fn rejects_unsupported_command() {
        let mut v = vec![0x05, 0x01, 0x00];
        v.extend_from_slice(&[0x05, 0x02, 0x00, 0x01, 1, 2, 3, 4]); // CMD=BIND
        v.extend_from_slice(&80u16.to_be_bytes());
        let mut d = Duplex::new(v);
        assert!(matches!(
            socks5_negotiate(&mut d),
            Err(SocksError::Rejected(SOCKS_CMD_UNSUPPORTED))
        ));
    }

    #[test]
    fn rejects_when_no_acceptable_method() {
        // One method offered (0x02 = user/pass), which we do not support.
        let mut d = Duplex::new(vec![0x05, 0x01, 0x02]);
        assert!(matches!(socks5_negotiate(&mut d), Err(SocksError::Rejected(0))));
        assert_eq!(&d.output[..2], &[0x05, 0xFF]);
    }

    #[test]
    fn truncated_handshake_is_io_not_panic() {
        // Greeting claims 3 methods but supplies none: read_exact hits EOF.
        let mut d = Duplex::new(vec![0x05, 0x03]);
        assert!(matches!(socks5_negotiate(&mut d), Err(SocksError::Io)));
    }

    #[test]
    fn zero_port_is_rejected() {
        let mut d = Duplex::new(connect_domain("example.com", 0));
        assert!(matches!(
            socks5_negotiate(&mut d),
            Err(SocksError::Rejected(SOCKS_GENERAL_FAILURE))
        ));
    }

    #[test]
    fn a_domain_name_always_fits_the_open_host_field() {
        // A SOCKS domain length is a single byte, so the longest possible name is
        // 255 bytes — exactly the mux host limit. Every such OPEN encodes.
        let host = "a".repeat(255);
        let frame = EgressFrame::Open { stream: 7, port: 443, host: host.as_bytes() };
        let mut out = Vec::new();
        assert!(encode(&frame, &mut out).is_ok(), "a max-length SOCKS host must encode");
    }

    #[test]
    fn oversized_client_read_splits_into_in_range_data_frames() {
        // A payload larger than one frame chunks into >1 valid DATA frame, each in
        // range — the correct-by-construction guarantee for guest→host bytes.
        let payload = vec![0xABu8; EGRESS_MAX_PAYLOAD + 100];
        let mut frames = 0;
        for chunk in payload.chunks(EGRESS_MAX_PAYLOAD) {
            assert!(chunk.len() <= EGRESS_MAX_PAYLOAD);
            let mut out = Vec::new();
            encode(&EgressFrame::Data { stream: 1, payload: chunk }, &mut out)
                .expect("each chunk is in range");
            frames += 1;
        }
        assert_eq!(frames, 2);
    }

    #[test]
    fn tty_writer_emits_whole_frames_only() {
        // The writer path encodes and writes a complete frame; the sink holds
        // exactly the encoded bytes (header + payload), never a partial header.
        let sink = SharedSink::default();
        let w = TtyWriter::new(sink.clone());
        w.send(&EgressFrame::Data { stream: 3, payload: b"hi" }).unwrap();
        let mut expected = Vec::new();
        encode(&EgressFrame::Data { stream: 3, payload: b"hi" }, &mut expected).unwrap();
        assert_eq!(sink.bytes(), expected);
    }

    #[derive(Clone, Default)]
    struct SharedSink(Arc<Mutex<Vec<u8>>>);
    impl SharedSink {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }
    impl Write for SharedSink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
