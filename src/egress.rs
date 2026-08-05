//! The host-side egress backend: the `--allow-egress` proxy endpoint that
//! terminates guest-initiated TCP streams on the host and mediates them under
//! time dilation.
//!
//! This module owns the session table for the mux carried over COM4 / ttyS3. It
//! is vCPU-thread-owned, exactly like [`crate::control::ControlChannel`]: its one
//! effects function [`EgressBackend::pump`] runs only at loop boundaries, so
//! every guest-visible effect stays on the single writer.
//!
//! ## Why the backend owns the session table
//!
//! The `--allow-egress` safety predicate is "no established external state". A
//! SOCKS-style proxy terminates TCP *on the host*, so the host can OWN every
//! session — and then the predicate is a field read, not a guess about packets in
//! flight. That is the whole reason egress is a proxy and not a NIC: a connection
//! table is host-observable; a packet stream is not.
//!
//! ## Quiescence — the jump-legality invariant (INV-E1..E4)
//!
//! Define
//!
//! ```text
//! E = |sessions|
//!   + resolves_in_flight
//!   + (rx_parse non-empty ? 1 : 0)
//!   + (tx_pending non-empty ? 1 : 0)
//! ```
//!
//! and [`EgressBackend::is_quiescent`] ⇔ `E == 0`. The four terms enumerate every
//! place external state or un-transferred bytes can hide:
//!
//! * **`sessions`** — any live stream, in ANY state (`Resolving`/`Connecting`/
//!   `Established`/`HalfClosed`). A session is removed ONLY when both directions
//!   are closed AND both of its buffers are empty, so it cannot vanish while data
//!   is still moving.
//! * **`resolves_in_flight`** — a DNS query is a live real-time event even after
//!   its session is gone (the guest may reset a stream mid-resolve), so it is
//!   counted on its own until the worker's answer is harvested.
//! * **`rx_parse`** — guest bytes received but not yet consumed as whole frames
//!   (a partial frame straddling a pump).
//! * **`tx_pending`** — frames built for the guest but not yet handed to the UART
//!   FIFO.
//!
//! A fast-forward jump is legal only when egress is off OR `E == 0`
//! ([`ff_jump_allowed`], the unit mirror of INV-E1). Because `E == 0` means zero
//! live external fds, the window between the check and the TSC-offset write is
//! empty, not merely small (INV-E2). While `E > 0` the caller runs the real-rate
//! park and never writes the offset (INV-E3). Every session is guest-initiated —
//! the backend never listens, accepts, or maps a host port inward — so quiescence
//! is *sufficient* for jump legality (INV-E4).
//!
//! ## Effects seam
//!
//! The pure [`EgressBackend`] never touches the LAPIC: [`EgressBackend::pump`]
//! takes the guest bytes newly captured from the UART and updates the session
//! state and `tx_pending`. Feeding the RX FIFO and raising IRQ3 for the frames in
//! `tx_pending` is done by [`EgressChannel`] — the COM4 UART wrapper that owns the
//! backend, mirroring how [`crate::control::ControlChannel`] wraps COM2. The
//! channel drains only as many host→guest bytes into the FIFO as it will accept;
//! the rest stay in `tx_pending`, so `E > 0` (non-quiescent) holds until every
//! byte has actually been handed to the guest. All of it runs on the vCPU thread
//! at loop boundaries (single-writer).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io;
use std::net::{SocketAddr, Shutdown, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tdvmm_proto::egress::{decode, encode, EgressCodecError, EgressFrame, EgressReason, EGRESS_MAX_PAYLOAD};

use crate::arch;
use crate::ioapic::Ioapic;
use crate::lapic::Lapic;
use crate::serial::{self, ControlSerial, EventFdTrigger};

/// Per-session host→guest flow-control window: the host buffers at most this many
/// unsent bytes read from a socket, and stops reading (drops `EPOLLIN`) once the
/// buffer is full. It is also the initial credit a stream starts with.
const WINDOW_BYTES: usize = 32 * 1024;

/// Per-socket read chunk. Bounds one `read` syscall's scratch buffer.
const READ_CHUNK: usize = 16 * 1024;

/// Cap on a session's guest→host buffer (bytes waiting to be written to the
/// socket). A guest that floods a stalled socket past this is reset — the
/// pathological-overrun bound, matching `control.rs`'s TX cap discipline.
const TO_SOCKET_CAP: usize = 256 * 1024;

/// Cap on concurrently live streams. Bounds `sessions` (and thus `tx_pending`)
/// against a guest that opens without bound; further opens get an `OPEN_ERR`.
const MAX_SESSIONS: usize = 256;

// ============================================================================
// Errors
// ============================================================================

/// The failure modes of the egress backend. Two genuinely distinct modes, each
/// keeping its cause as `source()`, built through context-attaching constructors —
/// the same shape as [`crate::artifact`]'s and [`crate::cpio`]'s error enums.
///
/// In-band conditions (a refused connect, a reset stream, a stream-limit hit) are
/// NOT errors: they are reported to the guest as `OPEN_ERR`/`RST` frames and the
/// backend keeps running. Only a lost UART framing (unrecoverable) or a genuine
/// host-resource failure surfaces here.
#[derive(Debug)]
pub enum EgressError {
    /// A host-resource I/O failure (epoll, eventfd, the resolver thread), tagged
    /// with the operation; the underlying [`io::Error`] is the `source`.
    Io { what: String, source: io::Error },
    /// The guest's mux byte stream lost framing — an unrecoverable codec error, so
    /// the channel must be torn down. The [`EgressCodecError`] is the `source`.
    Frame { source: EgressCodecError },
}

impl EgressError {
    /// An [`Io`](EgressError::Io) with `what` context attached.
    fn io(what: impl Into<String>, source: io::Error) -> Self {
        EgressError::Io { what: what.into(), source }
    }
    /// A [`Frame`](EgressError::Frame) wrapping a codec failure.
    fn frame(source: EgressCodecError) -> Self {
        EgressError::Frame { source }
    }
}

impl fmt::Display for EgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EgressError::Io { what, source } => write!(f, "{what}: {source}"),
            EgressError::Frame { source } => write!(f, "egress mux framing lost: {source}"),
        }
    }
}

impl std::error::Error for EgressError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EgressError::Io { source, .. } => Some(source),
            EgressError::Frame { source } => Some(source),
        }
    }
}

// ============================================================================
// Reporting
// ============================================================================

/// Cumulative egress counters, surfaced in the run report. `gated_*` are stamped
/// by the phase gate (each real-rate interval spent waiting for quiescence); the
/// rest are stamped by [`EgressBackend::pump`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EgressStats {
    /// Streams the guest opened (whether or not they connected).
    pub sessions_total: u64,
    /// Opens that never reached `Established` (resolve/connect failure, limit).
    pub opens_failed: u64,
    /// Guest→host payload bytes written to sockets.
    pub bytes_up: u64,
    /// Host→guest payload bytes framed toward the guest.
    pub bytes_down: u64,
    /// Real nanoseconds fast-forward was held off while egress was non-quiescent.
    pub gated_real_ns: u64,
    /// Number of distinct real-rate intervals the gate imposed.
    pub gated_intervals: u64,
}

// ============================================================================
// The connector seam
// ============================================================================

/// A guest-chosen mux stream id (the `stream` field on the wire).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct StreamId(pub u16);

/// A completed hostname resolution handed back by [`EgressConnector::take_resolved`].
/// `addr` is `None` when resolution failed.
#[derive(Clone, Copy, Debug)]
pub struct Resolved {
    pub stream: StreamId,
    pub addr: Option<SocketAddr>,
}

/// The state of a non-blocking connect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectPoll {
    /// Still connecting.
    Pending,
    /// The socket is connected and ready.
    Connected,
    /// The connect failed; carries the reason to send in `OPEN_ERR`.
    Failed(EgressReason),
}

/// The host-side effects the backend needs from the outside world: DNS
/// resolution and non-blocking TCP sockets, keyed by [`StreamId`].
///
/// Keying every operation by stream id (rather than handing the backend a raw
/// socket) keeps the trait dyn-compatible and lets the test double own its own
/// socket table with no real fds — every quiescence and flow-control decision is
/// then unit-testable with no network. [`RealConnector`] is the production impl;
/// `MockConnector` (test-only) is script-driven.
pub trait EgressConnector {
    /// Begin asynchronous resolution of `host:port` for `stream`. Must not block;
    /// the answer arrives later via [`take_resolved`](Self::take_resolved).
    fn start_resolve(&mut self, stream: StreamId, host: String, port: u16);

    /// Drain every resolution that has completed since the last call. Each entry
    /// corresponds to one earlier [`start_resolve`](Self::start_resolve).
    fn take_resolved(&mut self) -> Vec<Resolved>;

    /// The readiness fd the resolver signals on (registered in the backend's
    /// epoll set), or `None` for a synchronous / mock resolver.
    fn resolver_fd(&self) -> Option<RawFd>;

    /// Begin a non-blocking connect to `addr` for `stream`. `Err` means the socket
    /// could not even be created or initiated (a host-resource failure).
    fn start_connect(&mut self, stream: StreamId, addr: SocketAddr) -> io::Result<()>;

    /// Poll a connect started with [`start_connect`](Self::start_connect).
    fn poll_connect(&mut self, stream: StreamId) -> ConnectPoll;

    /// Non-blocking read from `stream`'s socket. `Ok(None)` = would-block,
    /// `Ok(Some(0))` = EOF, `Ok(Some(n))` = `n` bytes read into `buf`.
    fn read(&mut self, stream: StreamId, buf: &mut [u8]) -> io::Result<Option<usize>>;

    /// Non-blocking write to `stream`'s socket. `Ok(None)` = would-block (nothing
    /// accepted), `Ok(Some(n))` = `n` bytes accepted.
    fn write(&mut self, stream: StreamId, data: &[u8]) -> io::Result<Option<usize>>;

    /// Shut down the write half of `stream`'s socket (the guest sent `CLOSE`).
    fn shutdown_write(&mut self, stream: StreamId);

    /// Drop `stream`'s socket entirely (`CLOSE` complete, `RST`, or teardown).
    fn close(&mut self, stream: StreamId);

    /// `stream`'s socket fd, for epoll registration and `EPOLLIN` toggling;
    /// `None` when there is no real fd (mock).
    fn stream_fd(&self, stream: StreamId) -> Option<RawFd>;
}

// ============================================================================
// Session table
// ============================================================================

/// The lifecycle of one stream. `HalfClosed` covers a stream with exactly one
/// direction closed as well as one draining its last buffered bytes after both
/// directions closed; the fine-grained close bits live on [`Session`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Awaiting the hostname resolution.
    Resolving,
    /// Awaiting the non-blocking connect to complete.
    Connecting,
    /// Connected; both directions open.
    Established,
    /// One or both directions closed; kept until both buffers drain.
    HalfClosed,
}

/// One live stream. Removed from the table ONLY when both directions are closed
/// AND both buffers are empty — the guarantee that lets `|sessions|` stand in for
/// "live external state" in the quiescence sum.
struct Session {
    state: SessionState,
    /// Guest→host bytes awaiting the socket write.
    to_socket: VecDeque<u8>,
    /// Host→guest bytes read from the socket, awaiting framing into `tx_pending`.
    to_guest: VecDeque<u8>,
    /// Remaining bytes the guest has authorised us to send toward it (the
    /// host→guest window credit); starts at [`WINDOW_BYTES`].
    to_guest_credit: usize,
    /// The guest sent `CLOSE`: no more guest→host data will arrive.
    guest_write_closed: bool,
    /// The socket reached EOF: no more host→guest data will arrive.
    host_read_closed: bool,
    /// The epoll interest mask currently applied to this stream's fd.
    epoll_mask: u32,
}

impl Session {
    fn new_resolving() -> Self {
        Session {
            state: SessionState::Resolving,
            to_socket: VecDeque::new(),
            to_guest: VecDeque::new(),
            to_guest_credit: WINDOW_BYTES,
            guest_write_closed: false,
            host_read_closed: false,
            epoll_mask: 0,
        }
    }

    /// Both directions closed and both buffers drained — safe to remove.
    fn is_drained(&self) -> bool {
        self.guest_write_closed
            && self.host_read_closed
            && self.to_socket.is_empty()
            && self.to_guest.is_empty()
    }
}

/// A guest frame copied out of the parse buffer, so acting on it no longer
/// borrows `rx_parse`.
enum OwnedFrame {
    Open { stream: StreamId, port: u16, host: Vec<u8> },
    Data { stream: StreamId, payload: Vec<u8> },
    Close { stream: StreamId },
    Rst { stream: StreamId },
    Window { stream: StreamId, credit: u32 },
    /// A host→guest-only frame type arrived from the guest — a protocol error.
    Unexpected { stream: StreamId },
}

/// Whether a serviced session should stay in the table.
enum Disposition {
    Keep,
    Remove,
}

// ============================================================================
// The backend
// ============================================================================

/// The host-side egress endpoint. Owns the session table, the parse and
/// pending-transmit buffers, the resolve-in-flight count, and the single epoll fd
/// aggregating every live socket plus the resolver's readiness fd.
pub struct EgressBackend {
    connector: Box<dyn EgressConnector>,
    /// The one epoll fd handed to the park's poll set.
    epoll: OwnedFd,
    /// Live streams, keyed by id. `BTreeMap` so iteration (and `state_summary`)
    /// is deterministic.
    sessions: BTreeMap<StreamId, Session>,
    /// Stream ids with a DNS query dispatched but not yet harvested — retained
    /// even after their session is gone. The single source of truth for the
    /// resolves-in-flight term of `E`: there is exactly one entry per id (a second
    /// `OPEN` for an id already resolving is rejected), so the set size *is* the
    /// count, and an answer can never bind to a reused id.
    resolving: BTreeSet<StreamId>,
    /// Guest bytes received but not yet consumed as whole frames.
    rx_parse: Vec<u8>,
    /// Encoded frames built for the guest, not yet handed to the FIFO.
    tx_pending: VecDeque<u8>,
    stats: EgressStats,
}

impl EgressBackend {
    /// Build a backend backed by the real resolver + TCP sockets.
    ///
    /// # Errors
    ///
    /// [`EgressError::Io`] if the epoll fd, the resolver eventfd, or the resolver
    /// thread cannot be created.
    pub fn new() -> Result<Self, EgressError> {
        let connector = RealConnector::new().map_err(|e| EgressError::io("starting egress connector", e))?;
        Self::with_connector(Box::new(connector))
    }

    /// Build a backend over an arbitrary connector (the seam the test double uses).
    ///
    /// # Errors
    ///
    /// [`EgressError::Io`] if the epoll fd cannot be created or the resolver fd
    /// cannot be registered.
    pub fn with_connector(connector: Box<dyn EgressConnector>) -> Result<Self, EgressError> {
        // SAFETY: epoll_create1 returns a fresh owned fd or -1/errno.
        let raw = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if raw < 0 {
            return Err(EgressError::io("creating egress epoll fd", io::Error::last_os_error()));
        }
        // SAFETY: `raw` is a fresh fd we exclusively own; wrap it for RAII close.
        let epoll = unsafe { OwnedFd::from_raw_fd(raw) };
        if let Some(fd) = connector.resolver_fd() {
            epoll_add(epoll.as_raw_fd(), fd, libc::EPOLLIN as u32)
                .map_err(|e| EgressError::io("registering resolver fd", e))?;
        }
        Ok(EgressBackend {
            connector,
            epoll,
            sessions: BTreeMap::new(),
            resolving: BTreeSet::new(),
            rx_parse: Vec::new(),
            tx_pending: VecDeque::new(),
            stats: EgressStats::default(),
        })
    }

    /// The single epoll fd for the park's poll set.
    pub fn epoll_fd(&self) -> RawFd {
        self.epoll.as_raw_fd()
    }

    /// Whether egress holds no live external state — `E == 0`. See the module doc.
    pub fn is_quiescent(&self) -> bool {
        self.sessions.is_empty()
            && self.resolving.is_empty()
            && self.rx_parse.is_empty()
            && self.tx_pending.is_empty()
    }

    /// A snapshot of the counters for the run report.
    pub fn stats(&self) -> EgressStats {
        self.stats
    }

    /// A one-line description of the quiescence terms, for the always-on gate
    /// assert's panic message.
    pub fn state_summary(&self) -> String {
        format!(
            "sessions={} resolves_in_flight={} rx_parse={}B tx_pending={}B",
            self.sessions.len(),
            self.resolving.len(),
            self.rx_parse.len(),
            self.tx_pending.len(),
        )
    }

    /// Record one real-rate interval the phase gate imposed while non-quiescent.
    pub fn note_gated_interval(&mut self, real_ns: u64) {
        self.stats.gated_real_ns = self.stats.gated_real_ns.saturating_add(real_ns);
        self.stats.gated_intervals = self.stats.gated_intervals.saturating_add(1);
    }

    /// Remove up to `max` pending host→guest bytes for delivery into the FIFO.
    pub fn drain_to_guest(&mut self, max: usize) -> Vec<u8> {
        let take = max.min(self.tx_pending.len());
        self.tx_pending.drain(..take).collect()
    }

    /// Return bytes taken by [`drain_to_guest`](Self::drain_to_guest) that the RX
    /// FIFO could not accept, to the FRONT of the pending queue — so they are the
    /// next bytes offered and, crucially, keep counting toward `tx_pending` (thus
    /// toward `E`) until the guest actually drains room for them.
    pub fn unshift_to_guest(&mut self, bytes: &[u8]) {
        for &b in bytes.iter().rev() {
            self.tx_pending.push_front(b);
        }
    }

    /// Whether any host→guest bytes are still staged in `tx_pending`.
    pub fn has_frames_for_guest(&self) -> bool {
        !self.tx_pending.is_empty()
    }

    /// Count of live streams — the open-session count named in the long-gate WARN.
    pub fn open_session_count(&self) -> usize {
        self.sessions.len()
    }

    /// The one effects function. Absorbs `from_guest` (bytes newly captured from
    /// the UART), parses whole frames, advances every session (resolve → connect →
    /// established → close), performs non-blocking socket I/O through the
    /// connector, harvests finished resolutions, and enqueues host→guest frames —
    /// applying the per-session window and resetting streams that overrun.
    ///
    /// It never touches the LAPIC: the caller ([`EgressChannel`]) drains
    /// `tx_pending` into the FIFO and raises IRQ3.
    ///
    /// # Errors
    ///
    /// [`EgressError::Frame`] if the guest byte stream loses framing (the channel
    /// must then be torn down); [`EgressError::Io`] on an epoll failure.
    pub fn pump(&mut self, from_guest: &[u8]) -> Result<(), EgressError> {
        self.ingest_and_parse(from_guest)?;
        self.harvest_resolves()?;
        self.service_sessions()?;
        Ok(())
    }

    /// Absorb new guest bytes, split off every whole frame, and act on each.
    fn ingest_and_parse(&mut self, from_guest: &[u8]) -> Result<(), EgressError> {
        self.rx_parse.extend_from_slice(from_guest);

        let mut owned = Vec::new();
        let mut offset = 0;
        loop {
            match decode(&self.rx_parse[offset..]) {
                Ok(Some(d)) => {
                    owned.push(to_owned(d.frame));
                    offset += d.consumed;
                }
                Ok(None) => break,
                Err(e) => {
                    // Framing is a single continuous byte stream with no delimiter
                    // to resync on, so a malformed frame is unrecoverable. Drop the
                    // unparseable bytes and surface the error; the caller tears the
                    // channel down.
                    self.rx_parse.clear();
                    return Err(EgressError::frame(e));
                }
            }
        }
        self.rx_parse.drain(..offset);

        for frame in owned {
            self.handle_frame(frame)?;
        }
        Ok(())
    }

    fn handle_frame(&mut self, frame: OwnedFrame) -> Result<(), EgressError> {
        match frame {
            OwnedFrame::Open { stream, port, host } => self.handle_open(stream, port, &host),
            OwnedFrame::Data { stream, payload } => self.handle_data(stream, &payload),
            OwnedFrame::Close { stream } => {
                self.handle_close(stream);
                Ok(())
            }
            OwnedFrame::Rst { stream } => {
                self.drop_stream(stream);
                Ok(())
            }
            OwnedFrame::Window { stream, credit } => {
                if let Some(s) = self.sessions.get_mut(&stream) {
                    s.to_guest_credit = s.to_guest_credit.saturating_add(credit as usize);
                }
                Ok(())
            }
            OwnedFrame::Unexpected { stream } => self.reset_stream(stream),
        }
    }

    fn handle_open(&mut self, stream: StreamId, port: u16, host: &[u8]) -> Result<(), EgressError> {
        if self.sessions.contains_key(&stream) || self.resolving.contains(&stream) {
            // Re-opening a live stream id — or one whose earlier resolve is still
            // outstanding after a reset — is a protocol violation. Rejecting it is
            // what stops a late answer from binding to a reused id.
            return self.reset_stream(stream);
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return self.fail_open(stream, EgressReason::StreamLimit);
        }
        if host.is_empty() || port == 0 {
            return self.fail_open(stream, EgressReason::ProtocolError);
        }
        self.connector
            .start_resolve(stream, String::from_utf8_lossy(host).into_owned(), port);
        self.resolving.insert(stream);
        self.stats.sessions_total += 1;
        self.sessions.insert(stream, Session::new_resolving());
        Ok(())
    }

    fn handle_data(&mut self, stream: StreamId, payload: &[u8]) -> Result<(), EgressError> {
        enum Act {
            Ok,
            Overrun,
            NoStream,
        }
        let act = match self.sessions.get_mut(&stream) {
            Some(s) if !s.guest_write_closed => {
                s.to_socket.extend(payload.iter().copied());
                if s.to_socket.len() > TO_SOCKET_CAP {
                    Act::Overrun
                } else {
                    Act::Ok
                }
            }
            // No such stream, or the guest already half-closed and is now sending
            // data — either way a protocol violation.
            _ => Act::NoStream,
        };
        match act {
            Act::Ok => Ok(()),
            Act::Overrun | Act::NoStream => self.reset_stream(stream),
        }
    }

    fn handle_close(&mut self, stream: StreamId) {
        let existed = match self.sessions.get_mut(&stream) {
            Some(s) => {
                s.guest_write_closed = true;
                if s.state == SessionState::Established {
                    s.state = SessionState::HalfClosed;
                }
                true
            }
            None => false,
        };
        if existed {
            // No-op before the socket exists; re-applied after connect completes.
            self.connector.shutdown_write(stream);
        }
    }

    /// Harvest finished resolutions and advance each still-resolving session to
    /// `Connecting`, or fail it.
    fn harvest_resolves(&mut self) -> Result<(), EgressError> {
        for r in self.connector.take_resolved() {
            self.resolving.remove(&r.stream);
            let still_resolving = matches!(
                self.sessions.get(&r.stream).map(|s| s.state),
                Some(SessionState::Resolving)
            );
            if !still_resolving {
                // The session was reset while resolving; the answer is discarded,
                // but the count above still had to drop.
                continue;
            }
            match r.addr {
                None => self.fail_open(r.stream, EgressReason::ResolveFailed)?,
                Some(addr) => match self.connector.start_connect(r.stream, addr) {
                    Ok(()) => {
                        let mask = (libc::EPOLLIN | libc::EPOLLOUT) as u32;
                        if let Some(fd) = self.connector.stream_fd(r.stream) {
                            epoll_add(self.epoll.as_raw_fd(), fd, mask)
                                .map_err(|e| EgressError::io("registering egress socket", e))?;
                        }
                        if let Some(s) = self.sessions.get_mut(&r.stream) {
                            s.state = SessionState::Connecting;
                            s.epoll_mask = mask;
                        }
                    }
                    Err(_) => self.fail_open(r.stream, EgressReason::Unreachable)?,
                },
            }
        }
        Ok(())
    }

    /// Advance every session: poll pending connects, run socket I/O, frame
    /// host→guest data, and drop drained streams.
    fn service_sessions(&mut self) -> Result<(), EgressError> {
        let ids: Vec<StreamId> = self.sessions.keys().copied().collect();
        for id in ids {
            // Take the session out so per-session work borrows only the connector,
            // buffers, and epoll — never the session table.
            let mut sess = match self.sessions.remove(&id) {
                Some(s) => s,
                None => continue,
            };
            let disposition = match sess.state {
                SessionState::Resolving => Ok(Disposition::Keep),
                SessionState::Connecting => self.run_connecting(id, &mut sess),
                SessionState::Established | SessionState::HalfClosed => self.run_open_io(id, &mut sess),
            };
            match disposition {
                Ok(Disposition::Keep) => {
                    self.sessions.insert(id, sess);
                }
                Ok(Disposition::Remove) => {} // connector + epoll already cleaned up
                Err(e) => {
                    self.sessions.insert(id, sess);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    fn run_connecting(&mut self, id: StreamId, sess: &mut Session) -> Result<Disposition, EgressError> {
        match self.connector.poll_connect(id) {
            ConnectPoll::Pending => Ok(Disposition::Keep),
            ConnectPoll::Connected => {
                enqueue(&mut self.tx_pending, EgressFrame::OpenOk { stream: id.0 })?;
                sess.state = SessionState::Established;
                if sess.guest_write_closed {
                    // A CLOSE arrived before the socket existed; apply it now.
                    self.connector.shutdown_write(id);
                    sess.state = SessionState::HalfClosed;
                }
                self.update_epoll(id, sess)?;
                Ok(Disposition::Keep)
            }
            ConnectPoll::Failed(reason) => {
                self.detach_socket(id);
                self.stats.opens_failed += 1;
                enqueue(&mut self.tx_pending, EgressFrame::OpenErr { stream: id.0, reason })?;
                Ok(Disposition::Remove)
            }
        }
    }

    fn run_open_io(&mut self, id: StreamId, sess: &mut Session) -> Result<Disposition, EgressError> {
        // 1. Flush guest→host bytes to the socket.
        while !sess.to_socket.is_empty() {
            sess.to_socket.make_contiguous();
            let front = sess.to_socket.as_slices().0;
            let flen = front.len();
            match self.connector.write(id, front) {
                Ok(Some(n)) if n > 0 => {
                    // Clamp: a connector that over-reports cannot drain past what
                    // it was handed.
                    let n = n.min(flen);
                    sess.to_socket.drain(..n);
                    self.stats.bytes_up += n as u64;
                }
                Ok(_) => break, // would-block / nothing accepted
                Err(_) => return self.abort_open(id),
            }
        }

        // 2. Read host→guest bytes into the window buffer (bounded by WINDOW_BYTES).
        if !sess.host_read_closed {
            let mut buf = [0u8; READ_CHUNK];
            while sess.to_guest.len() < WINDOW_BYTES {
                let want = (WINDOW_BYTES - sess.to_guest.len()).min(READ_CHUNK);
                match self.connector.read(id, &mut buf[..want]) {
                    Ok(Some(0)) => {
                        sess.host_read_closed = true;
                        enqueue(&mut self.tx_pending, EgressFrame::Close { stream: id.0 })?;
                        if sess.state == SessionState::Established {
                            sess.state = SessionState::HalfClosed;
                        }
                        break;
                    }
                    Ok(Some(n)) => {
                        // Clamp: a connector cannot have filled past the slice it
                        // was given.
                        let n = n.min(want);
                        sess.to_guest.extend(buf[..n].iter().copied());
                    }
                    Ok(None) => break, // would-block
                    Err(_) => return self.abort_open(id),
                }
            }
        }

        // 3. Frame the window buffer toward the guest, bounded by remaining credit.
        while !sess.to_guest.is_empty() && sess.to_guest_credit > 0 {
            let take = sess.to_guest.len().min(sess.to_guest_credit).min(EGRESS_MAX_PAYLOAD);
            sess.to_guest.make_contiguous();
            let chunk = &sess.to_guest.as_slices().0[..take];
            enqueue(&mut self.tx_pending, EgressFrame::Data { stream: id.0, payload: chunk })?;
            sess.to_guest_credit -= take;
            self.stats.bytes_down += take as u64;
            sess.to_guest.drain(..take);
        }

        // 4. Keep the epoll interest in step with the window and the write backlog.
        self.update_epoll(id, sess)?;

        // 5. Remove only once both directions are closed AND both buffers drained.
        if sess.is_drained() {
            self.detach_socket(id);
            return Ok(Disposition::Remove);
        }
        Ok(Disposition::Keep)
    }

    /// Recompute this stream's epoll interest: `EPOLLIN` while the window has room
    /// and the read side is open (dropped "when over credit"), `EPOLLOUT` while a
    /// write backlog remains. `epoll_mask` is updated even without a real fd, so a
    /// mock-backed test can observe the toggling.
    fn update_epoll(&mut self, id: StreamId, sess: &mut Session) -> Result<(), EgressError> {
        let mut mask = 0u32;
        if !sess.host_read_closed && sess.to_guest.len() < WINDOW_BYTES {
            mask |= libc::EPOLLIN as u32;
        }
        if !sess.to_socket.is_empty() {
            mask |= libc::EPOLLOUT as u32;
        }
        if mask != sess.epoll_mask {
            if let Some(fd) = self.connector.stream_fd(id) {
                epoll_mod(self.epoll.as_raw_fd(), fd, mask)
                    .map_err(|e| EgressError::io("updating egress epoll interest", e))?;
            }
            sess.epoll_mask = mask;
        }
        Ok(())
    }

    /// Reset a live stream: tell the guest with `RST`, drop the socket, and remove
    /// the session.
    fn reset_stream(&mut self, stream: StreamId) -> Result<(), EgressError> {
        self.detach_socket(stream);
        self.sessions.remove(&stream);
        enqueue(&mut self.tx_pending, EgressFrame::Rst { stream: stream.0 })
    }

    /// The `run_open_io` reset path: the session is already removed from the table
    /// (serviced out), so just tell the guest and drop the socket.
    fn abort_open(&mut self, id: StreamId) -> Result<Disposition, EgressError> {
        self.detach_socket(id);
        enqueue(&mut self.tx_pending, EgressFrame::Rst { stream: id.0 })?;
        Ok(Disposition::Remove)
    }

    /// Fail an open before it is established: tell the guest with `OPEN_ERR`, drop
    /// any socket, and remove the session.
    fn fail_open(&mut self, stream: StreamId, reason: EgressReason) -> Result<(), EgressError> {
        self.detach_socket(stream);
        self.sessions.remove(&stream);
        self.stats.opens_failed += 1;
        enqueue(&mut self.tx_pending, EgressFrame::OpenErr { stream: stream.0, reason })
    }

    /// A guest `RST`: drop the socket and session with no frame back.
    fn drop_stream(&mut self, stream: StreamId) {
        self.detach_socket(stream);
        self.sessions.remove(&stream);
    }

    /// Deregister a stream's fd from epoll (if any) and close its socket.
    fn detach_socket(&mut self, stream: StreamId) {
        if let Some(fd) = self.connector.stream_fd(stream) {
            epoll_del(self.epoll.as_raw_fd(), fd);
        }
        self.connector.close(stream);
    }
}

/// The unit mirror of INV-E1: a fast-forward jump is legal iff egress is absent
/// or quiescent. The phase gate calls this just before writing the TSC offset.
pub fn ff_jump_allowed(egress: Option<&EgressBackend>) -> bool {
    match egress {
        None => true,
        Some(backend) => backend.is_quiescent(),
    }
}

/// The always-on phase-gate tripwire (INV-E1), asserted immediately before every
/// `vtsc.bump_offset`. Panics — aborting the run, release included, exactly like
/// the queue-discipline assert — if a fast-forward jump was about to skip real
/// time while external egress state is open. Under normal operation the gating
/// branch parks at real rate until quiescent so this never fires; it is the
/// last-line detector that stays live even when `TDVMM_EGRESS_UNSAFE_JUMPS`
/// disables the gate (that env skips the park, NOT this check). The message names
/// the live quiescence terms so a breach is diagnosable from the abort alone.
pub fn assert_ff_jump_legal(egress: Option<&EgressBackend>) {
    assert!(
        ff_jump_allowed(egress),
        "egress gate breached: a fast-forward jump was about to skip real time \
         while external egress state is open ({})",
        egress.map(EgressBackend::state_summary).unwrap_or_default(),
    );
}

// ============================================================================
// EgressChannel — the COM4 / ttyS3 UART wrapper over the backend
// ============================================================================

/// After the fast-forward has been gated by egress for this long (contiguous
/// real time), warn — the "guest is holding a connection open" signature.
const GATE_WARN_AFTER: Duration = Duration::from_secs(30);
/// Minimum spacing between long-gate WARNs, so a long-held connection warns
/// periodically instead of every gated interval.
const GATE_WARN_COOLDOWN: Duration = Duration::from_secs(30);

/// Rate-limited tracker for the long-gate WARN. Telemetry only — it never affects
/// control flow (mirrors [`crate::telemetry::FfState`]'s jump-rate WARN).
/// `span_start` marks when the current contiguous gated stretch began; a quiescent
/// pump clears it. The testable core is [`GateWarn::warn_at`].
#[derive(Default)]
struct GateWarn {
    span_start: Option<Instant>,
    last_warn: Option<Instant>,
}

impl GateWarn {
    /// A gated interval just occurred: open a span if none is active.
    fn note(&mut self, now: Instant) {
        self.span_start.get_or_insert(now);
    }

    /// Egress went quiescent: the gated span (if any) has ended.
    fn clear(&mut self) {
        self.span_start = None;
    }

    /// The WARN message iff the current gated span has exceeded [`GATE_WARN_AFTER`]
    /// and the cooldown since the last WARN has elapsed. Pure of I/O so it is
    /// unit-testable with synthetic instants.
    fn warn_at(&mut self, now: Instant, open_sessions: usize, gated_real_s: f64) -> Option<String> {
        let start = *self.span_start.get_or_insert(now);
        if now.duration_since(start) < GATE_WARN_AFTER {
            return None;
        }
        let cooled = self
            .last_warn
            .is_none_or(|t| now.duration_since(t) >= GATE_WARN_COOLDOWN);
        if !cooled {
            return None;
        }
        self.last_warn = Some(now);
        Some(format!(
            "[tdvmm][WARN] fast-forward has been gated by egress for {:.0}s \
             ({open_sessions} open session(s), {gated_real_s:.0}s real gated total) — \
             the guest is holding a connection open; NOT stopping (bound the run with \
             --wall-timeout / --max-virtual-time, or close the connection).",
            now.duration_since(start).as_secs_f64(),
        ))
    }
}

/// The COM4 / ttyS3 device the guest talks egress over: the pure [`EgressBackend`]
/// wrapped with a `vm-superio` 16550 UART, the exact shape
/// [`crate::control::ControlChannel`] wraps COM2. This is the effects seam the
/// module doc names — the backend stays LAPIC-free; this type feeds the RX FIFO
/// and raises IRQ3. vCPU-thread-owned; every guest-visible effect happens in
/// [`pump`](Self::pump) at a loop boundary (single-writer).
pub struct EgressChannel {
    backend: EgressBackend,
    serial: ControlSerial,
    drain: EventFdTrigger,
    /// Guest → host captured bytes: the raw mux stream the guest wrote to ttyS3
    /// (not line-delimited — a binary frame stream, drained whole each pump).
    tx: Arc<Mutex<Vec<u8>>>,
    /// `TDVMM_EGRESS_UNSAFE_JUMPS=1`: skip the phase-gate park (NOT the assert).
    /// Read once at construction; consulted only by the FF gating branch to enable
    /// the negative-control test that proves the always-on tripwire is live.
    unsafe_jumps: bool,
    gate_warn: GateWarn,
}

impl EgressChannel {
    /// Build the COM4 channel over the real resolver + TCP connector.
    ///
    /// # Errors
    ///
    /// [`EgressError::Io`] if the backend (epoll/eventfd/resolver) or the UART
    /// cannot be created.
    pub fn new() -> Result<Self, EgressError> {
        Self::wrap(EgressBackend::new()?)
    }

    /// Wrap a prebuilt backend in a fresh COM4 UART (the seam the tests use).
    fn wrap(backend: EgressBackend) -> Result<Self, EgressError> {
        let (serial, drain, tx) =
            serial::new_control_serial().map_err(|e| EgressError::io("creating COM4 UART", e))?;
        Ok(Self {
            backend,
            serial,
            drain,
            tx,
            unsafe_jumps: std::env::var_os("TDVMM_EGRESS_UNSAFE_JUMPS").is_some(),
            gate_warn: GateWarn::default(),
        })
    }

    /// Whether `port` is one of COM4's eight I/O ports.
    pub fn handles(port: u16) -> bool {
        (arch::SERIAL4_PORT_BASE..arch::SERIAL4_PORT_BASE + 8).contains(&port)
    }

    /// Service a guest PIO write to COM4 (THR: the guest forwarder emitting a mux
    /// frame; captured into `tx` for the next [`pump`](Self::pump)).
    pub fn pio_write(&mut self, port: u16, byte: u8, lapic: &mut Lapic, ioapic: &Ioapic) {
        let _ = self.serial.write((port - arch::SERIAL4_PORT_BASE) as u8, byte);
        self.after_uart_io(lapic, ioapic);
    }

    /// Service a guest PIO read from COM4 (RBR/status: the forwarder draining the
    /// RX FIFO or probing the UART).
    pub fn pio_read(&mut self, port: u16, lapic: &mut Lapic, ioapic: &Ioapic) -> u8 {
        let v = self.serial.read((port - arch::SERIAL4_PORT_BASE) as u8);
        self.after_uart_io(lapic, ioapic);
        v
    }

    /// After any UART register access, deliver the IRQ3 edge iff the model asserted
    /// an interrupt — the exact COM2 discipline (`control.rs`), on the SHARED line.
    fn after_uart_io(&mut self, lapic: &mut Lapic, ioapic: &Ioapic) {
        if self.drain.drain().is_ok() {
            crate::raise_irq(lapic, ioapic, arch::SERIAL4_IRQ);
        }
    }

    /// The one boundary effects function. Absorbs what the guest wrote to ttyS3,
    /// advances the backend (resolves, sockets, framing), then feeds as many
    /// host→guest bytes as the RX FIFO accepts and raises IRQ3 if any moved —
    /// modelling `control.rs`'s pump on the shared line. Bytes that do not fit stay
    /// in the backend's `tx_pending` (so `is_quiescent()` stays false) and stream
    /// out on later pumps as the guest drains the FIFO.
    ///
    /// # Errors
    ///
    /// [`EgressError::Frame`] if the guest's mux stream loses framing;
    /// [`EgressError::Io`] on a backend epoll failure.
    pub fn pump(&mut self, lapic: &mut Lapic, ioapic: &Ioapic) -> Result<(), EgressError> {
        let from_guest = std::mem::take(&mut *self.tx.lock().unwrap());
        self.backend.pump(&from_guest)?;

        let mut sent = false;
        loop {
            let cap = self.serial.fifo_capacity();
            if cap == 0 || !self.backend.has_frames_for_guest() {
                break;
            }
            let chunk = self.backend.drain_to_guest(cap);
            match self.serial.enqueue_raw_bytes(&chunk) {
                Ok(n) if n > 0 => {
                    if n < chunk.len() {
                        // Partial accept: return the tail so it stays counted in
                        // tx_pending and retries next pump. The FIFO is now full.
                        self.backend.unshift_to_guest(&chunk[n..]);
                        sent = true;
                        break;
                    }
                    sent = true;
                }
                _ => {
                    // Nothing accepted (or an error): return the whole chunk.
                    self.backend.unshift_to_guest(&chunk);
                    break;
                }
            }
        }
        if sent {
            self.after_uart_io(lapic, ioapic);
        }
        // A quiescent pump ends the current gated span (for the long-gate WARN).
        if self.backend.is_quiescent() {
            self.gate_warn.clear();
        }
        Ok(())
    }

    // ---- the phase-gate surface (read on the vCPU thread at the park boundary) --

    /// Whether egress holds no live external state (`E == 0`).
    pub fn is_quiescent(&self) -> bool {
        self.backend.is_quiescent()
    }

    /// The pure backend, for the always-on gate assert ([`assert_ff_jump_legal`]).
    pub fn backend(&self) -> &EgressBackend {
        &self.backend
    }

    /// The single epoll fd to place in the park's poll set while non-quiescent.
    pub fn epoll_fd(&self) -> RawFd {
        self.backend.epoll_fd()
    }

    /// A snapshot of the counters for the run report.
    pub fn stats(&self) -> EgressStats {
        self.backend.stats()
    }

    /// Whether `TDVMM_EGRESS_UNSAFE_JUMPS=1` is set (skip the gate park, not the
    /// assert). Consulted only by the FF gating branch.
    pub fn unsafe_jumps(&self) -> bool {
        self.unsafe_jumps
    }

    /// Stamp one real-rate interval the phase gate imposed while non-quiescent, and
    /// open the long-gate WARN span if this begins a gated stretch.
    pub fn note_gated_interval(&mut self, real_ns: u64) {
        self.backend.note_gated_interval(real_ns);
        self.gate_warn.note(Instant::now());
    }

    /// Emit the rate-limited WARN if fast-forward has been gated for over 30 real
    /// seconds contiguously, naming the open-session count.
    pub fn maybe_warn_long_gate(&mut self) {
        let sessions = self.backend.open_session_count();
        let gated_real_s = self.backend.stats().gated_real_ns as f64 / 1e9;
        if let Some(msg) = self.gate_warn.warn_at(Instant::now(), sessions, gated_real_s) {
            crate::log_line(format_args!("{msg}"));
        }
    }

    /// Free space in the COM4 RX FIFO (64 bytes when empty). A value below 64 means
    /// host→guest bytes are staged for the guest to read. Test-only observer.
    #[cfg(test)]
    fn rx_fifo_free(&mut self) -> usize {
        self.serial.fifo_capacity()
    }
}

/// Copy a decoded frame out of the parse buffer into an owned form. `OPEN_OK` /
/// `OPEN_ERR` are host→guest-only, so receiving them from the guest is flagged
/// [`OwnedFrame::Unexpected`].
fn to_owned(frame: EgressFrame<'_>) -> OwnedFrame {
    match frame {
        EgressFrame::Open { stream, port, host } => OwnedFrame::Open {
            stream: StreamId(stream),
            port,
            host: host.to_vec(),
        },
        EgressFrame::Data { stream, payload } => OwnedFrame::Data {
            stream: StreamId(stream),
            payload: payload.to_vec(),
        },
        EgressFrame::Close { stream } => OwnedFrame::Close { stream: StreamId(stream) },
        EgressFrame::Rst { stream } => OwnedFrame::Rst { stream: StreamId(stream) },
        EgressFrame::Window { stream, credit } => OwnedFrame::Window { stream: StreamId(stream), credit },
        EgressFrame::OpenOk { stream } | EgressFrame::OpenErr { stream, .. } => {
            OwnedFrame::Unexpected { stream: StreamId(stream) }
        }
    }
}

/// Encode a host→guest frame onto the pending-transmit queue.
fn enqueue(tx: &mut VecDeque<u8>, frame: EgressFrame<'_>) -> Result<(), EgressError> {
    let mut buf = Vec::new();
    encode(&frame, &mut buf).map_err(EgressError::frame)?;
    tx.extend(buf);
    Ok(())
}

// ============================================================================
// epoll helpers
// ============================================================================

fn epoll_ctl(epfd: RawFd, op: libc::c_int, fd: RawFd, events: u32) -> io::Result<()> {
    let mut ev = libc::epoll_event { events, u64: fd as u64 };
    // SAFETY: `epfd` and `fd` are valid fds owned by the backend / connector, and
    // `ev` lives for the duration of the call.
    let rc = unsafe { libc::epoll_ctl(epfd, op, fd, &mut ev) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn epoll_add(epfd: RawFd, fd: RawFd, events: u32) -> io::Result<()> {
    epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, events)
}

fn epoll_mod(epfd: RawFd, fd: RawFd, events: u32) -> io::Result<()> {
    epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, events)
}

/// Best-effort deregister; a stream is being torn down regardless of the result.
fn epoll_del(epfd: RawFd, fd: RawFd) {
    // SAFETY: valid fds; DEL ignores the (null) event pointer on modern kernels.
    unsafe {
        libc::epoll_ctl(epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut());
    }
}

// ============================================================================
// RealConnector — real DNS (a worker thread) + non-blocking TCP sockets
// ============================================================================

/// One resolution request handed to the worker thread.
struct ResolveJob {
    stream: StreamId,
    host: String,
    port: u16,
}

/// The production connector: a resolver worker thread reached through an
/// mpsc mailbox, whose answers are announced on an eventfd (so the backend's
/// epoll wakes), plus a table of non-blocking [`TcpStream`]s keyed by stream id.
struct RealConnector {
    conns: std::collections::HashMap<StreamId, TcpStream>,
    /// Request channel to the worker; dropped on teardown so the worker exits.
    to_worker: Option<mpsc::Sender<ResolveJob>>,
    /// Answer channel from the worker.
    from_worker: mpsc::Receiver<Resolved>,
    /// The eventfd the worker signals after posting an answer.
    event: OwnedFd,
    worker: Option<JoinHandle<()>>,
    /// Synthetic failures for resolves whose request could not reach the worker (a
    /// dead resolver). Drained alongside the real answers, so the in-flight count
    /// still decrements and the stream gets a clean `OPEN_ERR: ResolveFailed`
    /// instead of leaking `E > 0` forever.
    failed: Vec<Resolved>,
}

impl RealConnector {
    fn new() -> io::Result<Self> {
        // SAFETY: eventfd with valid flags returns a fresh fd or -1/errno.
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh fd we exclusively own.
        let event = unsafe { OwnedFd::from_raw_fd(raw) };
        let event_worker = event.try_clone()?;

        let (to_worker, jobs) = mpsc::channel::<ResolveJob>();
        let (answers, from_worker) = mpsc::channel::<Resolved>();
        let worker = thread::Builder::new()
            .name("tdvmm-egress-resolver".into())
            .spawn(move || resolver_loop(&jobs, &answers, &event_worker))?;

        Ok(RealConnector {
            conns: std::collections::HashMap::new(),
            to_worker: Some(to_worker),
            from_worker,
            event,
            worker: Some(worker),
            failed: Vec::new(),
        })
    }
}

/// The resolver worker: block on the mailbox, resolve each host, post the answer,
/// and pulse the eventfd. Exits when the request channel is dropped.
fn resolver_loop(jobs: &mpsc::Receiver<ResolveJob>, answers: &mpsc::Sender<Resolved>, event: &OwnedFd) {
    while let Ok(job) = jobs.recv() {
        let addr = (job.host.as_str(), job.port)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next());
        if answers.send(Resolved { stream: job.stream, addr }).is_err() {
            break;
        }
        let one: u64 = 1;
        // SAFETY: `event` is a valid eventfd we own; write the 8-byte counter.
        unsafe {
            libc::write(event.as_raw_fd(), std::ptr::addr_of!(one).cast(), 8);
        }
    }
}

impl EgressConnector for RealConnector {
    fn start_resolve(&mut self, stream: StreamId, host: String, port: u16) {
        let sent = match &self.to_worker {
            Some(tx) => tx.send(ResolveJob { stream, host, port }).is_ok(),
            None => false,
        };
        if !sent {
            // The resolver is gone (channel closed / worker dead). Synthesize a
            // failure so this resolve is still harvested and the stream fails
            // cleanly rather than pinning `E > 0` with nothing coming.
            self.failed.push(Resolved { stream, addr: None });
        }
    }

    fn take_resolved(&mut self) -> Vec<Resolved> {
        // Self-pipe discipline: clear the eventfd counter FIRST, then read the
        // answers. Any answer the worker posts after this read leaves the counter
        // set, so epoll re-fires and it is harvested next pump — no lost wakeup.
        let mut sink = [0u8; 8];
        // SAFETY: reading up to 8 bytes from our non-blocking eventfd.
        unsafe {
            libc::read(self.event.as_raw_fd(), sink.as_mut_ptr().cast(), 8);
        }
        let mut out = std::mem::take(&mut self.failed);
        while let Ok(r) = self.from_worker.try_recv() {
            out.push(r);
        }
        out
    }

    fn resolver_fd(&self) -> Option<RawFd> {
        Some(self.event.as_raw_fd())
    }

    fn start_connect(&mut self, stream: StreamId, addr: SocketAddr) -> io::Result<()> {
        let family = match addr {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        // SAFETY: socket() with valid args returns a fresh fd or -1/errno.
        let fd = unsafe {
            libc::socket(family, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh socket we own; TcpStream takes over its close.
        let sock = unsafe { TcpStream::from_raw_fd(fd) };
        let (sa, len) = to_sockaddr(addr);
        // SAFETY: `sa` holds a valid sockaddr of `len` bytes for `fd`'s family.
        let rc = unsafe { libc::connect(fd, std::ptr::addr_of!(sa).cast(), len) };
        if rc != 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EINPROGRESS) {
                return Err(e); // `sock` drops → fd closed
            }
        }
        self.conns.insert(stream, sock);
        Ok(())
    }

    fn poll_connect(&mut self, stream: StreamId) -> ConnectPoll {
        let fd = match self.conns.get(&stream) {
            Some(s) => s.as_raw_fd(),
            None => return ConnectPoll::Failed(EgressReason::Unreachable),
        };
        let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
        // SAFETY: one valid pollfd; timeout 0 = non-blocking readiness probe.
        let n = unsafe { libc::poll(&mut pfd, 1, 0) };
        if n <= 0 {
            return ConnectPoll::Pending;
        }
        // Writable (or errored): read the pending socket error to decide.
        let mut err: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: SO_ERROR writes an int into `err`; `len` is its size.
        let rc = unsafe {
            libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, std::ptr::addr_of_mut!(err).cast(), &mut len)
        };
        if rc != 0 {
            return ConnectPoll::Failed(EgressReason::Unreachable);
        }
        match err {
            0 => ConnectPoll::Connected,
            e if e == libc::ECONNREFUSED => ConnectPoll::Failed(EgressReason::ConnectRefused),
            _ => ConnectPoll::Failed(EgressReason::Unreachable),
        }
    }

    fn read(&mut self, stream: StreamId, buf: &mut [u8]) -> io::Result<Option<usize>> {
        use std::io::Read;
        match self.conns.get_mut(&stream) {
            Some(s) => match s.read(buf) {
                Ok(n) => Ok(Some(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            },
            None => Ok(Some(0)),
        }
    }

    fn write(&mut self, stream: StreamId, data: &[u8]) -> io::Result<Option<usize>> {
        use std::io::Write;
        match self.conns.get_mut(&stream) {
            Some(s) => match s.write(data) {
                Ok(n) => Ok(Some(n)),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            },
            None => Ok(None),
        }
    }

    fn shutdown_write(&mut self, stream: StreamId) {
        if let Some(s) = self.conns.get(&stream) {
            let _ = s.shutdown(Shutdown::Write);
        }
    }

    fn close(&mut self, stream: StreamId) {
        self.conns.remove(&stream);
    }

    fn stream_fd(&self, stream: StreamId) -> Option<RawFd> {
        self.conns.get(&stream).map(|s| s.as_raw_fd())
    }
}

impl Drop for RealConnector {
    fn drop(&mut self) {
        // Dropping the request sender ends the worker's `recv` loop; then join it.
        self.to_worker.take();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

/// Convert a [`SocketAddr`] into a libc `sockaddr_storage` + its length.
fn to_sockaddr(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: a zeroed sockaddr_storage is a valid empty sockaddr; each arm fills
    // only the fields of the matching family and returns that family's length.
    unsafe {
        let mut storage: libc::sockaddr_storage = std::mem::zeroed();
        match addr {
            SocketAddr::V4(v4) => {
                let sin = &mut *(std::ptr::addr_of_mut!(storage).cast::<libc::sockaddr_in>());
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = v4.port().to_be();
                sin.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) };
                (storage, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
            }
            SocketAddr::V6(v6) => {
                let sin6 = &mut *(std::ptr::addr_of_mut!(storage).cast::<libc::sockaddr_in6>());
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = v6.port().to_be();
                sin6.sin6_addr = libc::in6_addr { s6_addr: v6.ip().octets() };
                sin6.sin6_flowinfo = v6.flowinfo();
                sin6.sin6_scope_id = v6.scope_id();
                (storage, std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
            }
        }
    }
}

// ============================================================================
// Tests — the quiescence state machine and the jump-legality mirror, driven
// entirely through a script-driven MockConnector with NO real sockets or DNS.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::rc::Rc;
    use tdvmm_proto::egress::EGRESS_VERSION;

    use crate::control::ControlChannel;
    use crate::ioapic::IOAPIC_BASE;
    use crate::park::Parker;
    use crate::vtsc::{TscFrequency, VirtualClock};

    // ---- the test double -----------------------------------------------------

    /// A script-driven connector: no real fds, no network. Tests configure resolve
    /// answers, connect results, and per-stream socket bytes, then inspect what the
    /// backend wrote. Cloning shares the state, so a test keeps a control handle
    /// after the backend takes ownership of one clone.
    #[derive(Clone)]
    struct MockConnector {
        s: Rc<RefCell<MockState>>,
    }

    #[derive(Default)]
    struct MockState {
        deliver: Vec<Resolved>,
        connect: HashMap<StreamId, ConnectPoll>,
        recv: HashMap<StreamId, VecDeque<u8>>,
        eof: HashMap<StreamId, bool>,
        sent: HashMap<StreamId, Vec<u8>>,
        write_blocked: HashMap<StreamId, bool>,
        closed: Vec<StreamId>,
    }

    impl MockConnector {
        fn new() -> Self {
            MockConnector { s: Rc::new(RefCell::new(MockState::default())) }
        }
        /// Queue a resolution answer to be handed back on the next pump.
        fn complete_resolve(&self, stream: u16, addr: Option<SocketAddr>) {
            self.s.borrow_mut().deliver.push(Resolved { stream: StreamId(stream), addr });
        }
        fn set_connect(&self, stream: u16, poll: ConnectPoll) {
            self.s.borrow_mut().connect.insert(StreamId(stream), poll);
        }
        fn push_recv(&self, stream: u16, bytes: &[u8]) {
            self.s.borrow_mut().recv.entry(StreamId(stream)).or_default().extend(bytes.iter().copied());
        }
        fn set_eof(&self, stream: u16) {
            self.s.borrow_mut().eof.insert(StreamId(stream), true);
        }
        fn set_write_blocked(&self, stream: u16, blocked: bool) {
            self.s.borrow_mut().write_blocked.insert(StreamId(stream), blocked);
        }
        fn sent(&self, stream: u16) -> Vec<u8> {
            self.s.borrow().sent.get(&StreamId(stream)).cloned().unwrap_or_default()
        }
    }

    impl EgressConnector for MockConnector {
        fn start_resolve(&mut self, _stream: StreamId, _host: String, _port: u16) {}
        fn take_resolved(&mut self) -> Vec<Resolved> {
            std::mem::take(&mut self.s.borrow_mut().deliver)
        }
        fn resolver_fd(&self) -> Option<RawFd> {
            None
        }
        fn start_connect(&mut self, _stream: StreamId, _addr: SocketAddr) -> io::Result<()> {
            Ok(())
        }
        fn poll_connect(&mut self, stream: StreamId) -> ConnectPoll {
            self.s.borrow().connect.get(&stream).copied().unwrap_or(ConnectPoll::Connected)
        }
        fn read(&mut self, stream: StreamId, buf: &mut [u8]) -> io::Result<Option<usize>> {
            let mut st = self.s.borrow_mut();
            if let Some(q) = st.recv.get_mut(&stream) {
                if !q.is_empty() {
                    let n = buf.len().min(q.len());
                    for (i, b) in q.drain(..n).enumerate() {
                        buf[i] = b;
                    }
                    return Ok(Some(n));
                }
            }
            if *st.eof.get(&stream).unwrap_or(&false) {
                return Ok(Some(0));
            }
            Ok(None)
        }
        fn write(&mut self, stream: StreamId, data: &[u8]) -> io::Result<Option<usize>> {
            let mut st = self.s.borrow_mut();
            if *st.write_blocked.get(&stream).unwrap_or(&false) {
                return Ok(None);
            }
            st.sent.entry(stream).or_default().extend_from_slice(data);
            Ok(Some(data.len()))
        }
        fn shutdown_write(&mut self, _stream: StreamId) {}
        fn close(&mut self, stream: StreamId) {
            let mut st = self.s.borrow_mut();
            st.closed.push(stream);
            st.recv.remove(&stream);
        }
        fn stream_fd(&self, _stream: StreamId) -> Option<RawFd> {
            None
        }
    }

    // ---- helpers -------------------------------------------------------------

    fn setup() -> (EgressBackend, MockConnector) {
        let mock = MockConnector::new();
        let backend = EgressBackend::with_connector(Box::new(mock.clone())).unwrap();
        (backend, mock)
    }

    fn framed(frame: EgressFrame<'_>) -> Vec<u8> {
        let mut v = Vec::new();
        encode(&frame, &mut v).unwrap();
        v
    }

    fn addr() -> SocketAddr {
        "127.0.0.1:9".parse().unwrap()
    }

    fn state(b: &EgressBackend, stream: u16) -> Option<SessionState> {
        b.sessions.get(&StreamId(stream)).map(|s| s.state)
    }

    fn open(stream: u16) -> Vec<u8> {
        framed(EgressFrame::Open { stream, port: 443, host: b"example.com" })
    }

    /// Drive one stream to `Established`.
    fn establish(b: &mut EgressBackend, m: &MockConnector, stream: u16) {
        b.pump(&open(stream)).unwrap();
        m.complete_resolve(stream, Some(addr()));
        b.pump(&[]).unwrap(); // harvest -> connect (default Connected) -> Established
        assert_eq!(state(b, stream), Some(SessionState::Established));
    }

    // ---- T4: the quiescence state machine ------------------------------------

    #[test]
    fn empty_backend_is_quiescent() {
        let (b, _m) = setup();
        assert!(b.is_quiescent());
    }

    #[test]
    fn resolving_session_is_not_quiescent() {
        let (mut b, _m) = setup();
        b.pump(&open(1)).unwrap();
        assert_eq!(state(&b, 1), Some(SessionState::Resolving));
        assert!(b.resolving.contains(&StreamId(1)));
        assert!(!b.is_quiescent());
    }

    #[test]
    fn connecting_session_is_not_quiescent() {
        let (mut b, m) = setup();
        m.set_connect(1, ConnectPoll::Pending);
        b.pump(&open(1)).unwrap();
        m.complete_resolve(1, Some(addr()));
        b.pump(&[]).unwrap();
        assert_eq!(state(&b, 1), Some(SessionState::Connecting));
        assert!(b.resolving.is_empty());
        assert!(!b.is_quiescent());
    }

    #[test]
    fn established_session_is_not_quiescent() {
        let (mut b, m) = setup();
        establish(&mut b, &m, 1);
        assert!(!b.is_quiescent());
    }

    #[test]
    fn partial_frame_in_rx_parse_blocks_quiescence() {
        let (mut b, _m) = setup();
        // Three bytes of a valid-version header: a partial frame, held in rx_parse.
        b.pump(&[EGRESS_VERSION, 4, 1]).unwrap();
        assert!(!b.rx_parse.is_empty());
        assert!(b.sessions.is_empty());
        assert!(!b.is_quiescent());
    }

    #[test]
    fn pending_tx_alone_blocks_quiescence() {
        let (mut b, m) = setup();
        b.pump(&open(1)).unwrap();
        m.complete_resolve(1, None); // resolution FAILS
        b.pump(&[]).unwrap();
        // Session gone, resolve harvested, but OPEN_ERR is queued toward the guest.
        assert!(b.sessions.is_empty());
        assert!(b.resolving.is_empty());
        assert!(b.rx_parse.is_empty());
        assert!(!b.tx_pending.is_empty());
        assert!(!b.is_quiescent());
        // Quiescent only once the last buffer drains.
        let _ = b.drain_to_guest(usize::MAX);
        assert!(b.is_quiescent());
    }

    #[test]
    fn resolve_in_flight_alone_blocks_quiescence() {
        let (mut b, m) = setup();
        b.pump(&open(1)).unwrap();
        // Guest resets the stream before the resolve returns: the session is
        // removed but the DNS query is still live.
        b.pump(&framed(EgressFrame::Rst { stream: 1 })).unwrap();
        assert!(b.sessions.is_empty());
        assert_eq!(b.resolving.len(), 1);
        assert!(b.rx_parse.is_empty());
        assert!(b.tx_pending.is_empty()); // guest RST => nothing sent back
        assert!(!b.is_quiescent()); // ONLY the in-flight resolve is non-zero
        // The orphaned answer is harvested and discarded; now quiescent.
        m.complete_resolve(1, Some(addr()));
        b.pump(&[]).unwrap();
        assert!(b.resolving.is_empty());
        assert!(b.is_quiescent());
    }

    #[test]
    fn reopening_an_id_with_a_resolve_still_in_flight_is_reset() {
        let (mut b, m) = setup();
        // OPEN(1): a resolve is dispatched and held.
        b.pump(&open(1)).unwrap();
        assert!(b.resolving.contains(&StreamId(1)));
        // Guest resets the stream before the answer returns: the session is gone
        // but the DNS query is still outstanding.
        b.pump(&framed(EgressFrame::Rst { stream: 1 })).unwrap();
        assert!(b.sessions.is_empty());
        assert!(b.resolving.contains(&StreamId(1)));
        // Guest re-opens id 1 with a DIFFERENT host: it must be rejected with RST,
        // creating no new session or resolve — so the first answer can never bind
        // to this reused id.
        b.pump(&framed(EgressFrame::Open { stream: 1, port: 80, host: b"other.test" }))
            .unwrap();
        assert!(b.sessions.is_empty());
        assert_eq!(b.stats().sessions_total, 1); // no second session opened
        let frames = b.drain_to_guest(usize::MAX);
        let d = decode(&frames).unwrap().unwrap();
        assert_eq!(d.frame, EgressFrame::Rst { stream: 1 });
        // The original orphaned answer arrives, is harvested, discarded, and frees
        // the id — leaving the backend quiescent.
        m.complete_resolve(1, Some(addr()));
        b.pump(&[]).unwrap();
        assert!(!b.resolving.contains(&StreamId(1)));
        assert!(b.is_quiescent());
    }

    #[test]
    fn quiescent_only_after_the_last_buffer_drains_not_on_close() {
        let (mut b, m) = setup();
        establish(&mut b, &m, 1);
        // A guest write is stuck in the socket buffer (writes blocked).
        m.set_write_blocked(1, true);
        b.pump(&framed(EgressFrame::Data { stream: 1, payload: b"hello" })).unwrap();
        // Close BOTH directions: socket EOF + guest CLOSE.
        m.set_eof(1);
        b.pump(&framed(EgressFrame::Close { stream: 1 })).unwrap();
        // Both directions closed, yet the session survives because to_socket is not
        // empty — closure alone does not make it quiescent.
        assert!(b.sessions.contains_key(&StreamId(1)));
        assert!(!b.is_quiescent());
        // Unblock the socket: the buffer drains, the session is finally removed.
        m.set_write_blocked(1, false);
        b.pump(&[]).unwrap();
        assert!(!b.sessions.contains_key(&StreamId(1)));
        assert_eq!(m.sent(1), b"hello");
        // The frames toward the guest still block quiescence until delivered.
        assert!(!b.is_quiescent());
        let _ = b.drain_to_guest(usize::MAX);
        assert!(b.is_quiescent());
    }

    // ---- T4b: the jump-legality mirror (INV-E1) ------------------------------

    #[test]
    fn ff_jump_allowed_enumerates_every_component() {
        // None (egress off) is always allowed.
        assert!(ff_jump_allowed(None));

        let (mut b, _m) = setup();
        // Quiescent => allowed.
        assert!(ff_jump_allowed(Some(&b)));

        // Each non-quiescent component, in isolation, forbids a jump.
        b.sessions.insert(StreamId(9), Session::new_resolving());
        assert!(!ff_jump_allowed(Some(&b)));
        b.sessions.clear();
        assert!(ff_jump_allowed(Some(&b)));

        b.resolving.insert(StreamId(1));
        assert!(!ff_jump_allowed(Some(&b)));
        b.resolving.clear();
        assert!(ff_jump_allowed(Some(&b)));

        b.rx_parse.push(0xAB);
        assert!(!ff_jump_allowed(Some(&b)));
        b.rx_parse.clear();
        assert!(ff_jump_allowed(Some(&b)));

        b.tx_pending.push_back(0xCD);
        assert!(!ff_jump_allowed(Some(&b)));
        b.tx_pending.clear();
        assert!(ff_jump_allowed(Some(&b)));
    }

    // ---- flow control, data, and hardening -----------------------------------

    #[test]
    fn host_to_guest_window_bounds_the_first_pump_and_credit_gates_the_rest() {
        let (mut b, m) = setup();
        establish(&mut b, &m, 1);
        // Far more than one window of readable data.
        m.push_recv(1, &vec![0xEE; 100 * 1024]);

        // First pump: the initial credit (one window) is framed toward the guest.
        b.pump(&[]).unwrap();
        assert_eq!(b.stats().bytes_down, WINDOW_BYTES as u64);

        // Second pump: the window buffer fills but no credit remains, so EPOLLIN is
        // dropped ("deregister when over credit") and nothing more is framed.
        b.pump(&[]).unwrap();
        let sess = b.sessions.get(&StreamId(1)).unwrap();
        assert_eq!(sess.to_guest.len(), WINDOW_BYTES);
        assert_eq!(sess.epoll_mask & (libc::EPOLLIN as u32), 0);
        assert_eq!(b.stats().bytes_down, WINDOW_BYTES as u64);

        // A WINDOW grant re-opens the flow.
        b.pump(&framed(EgressFrame::Window { stream: 1, credit: 1_000_000 })).unwrap();
        assert!(b.stats().bytes_down > WINDOW_BYTES as u64);
    }

    #[test]
    fn guest_to_host_data_reaches_the_socket_and_counts_up() {
        let (mut b, m) = setup();
        establish(&mut b, &m, 1);
        b.pump(&framed(EgressFrame::Data { stream: 1, payload: b"abcxyz" })).unwrap();
        assert_eq!(m.sent(1), b"abcxyz");
        assert_eq!(b.stats().bytes_up, 6);
    }

    #[test]
    fn established_open_ok_is_reported_and_delivered() {
        let (mut b, m) = setup();
        establish(&mut b, &m, 1);
        // The OPEN_OK frame is pending toward the guest.
        let frames = b.drain_to_guest(usize::MAX);
        let d = decode(&frames).unwrap().unwrap();
        assert_eq!(d.frame, EgressFrame::OpenOk { stream: 1 });
    }

    #[test]
    fn lost_framing_is_an_error_not_a_panic() {
        let (mut b, _m) = setup();
        let err = b.pump(&[0xEE, 0, 0, 0, 0, 0]).unwrap_err();
        assert!(matches!(err, EgressError::Frame { .. }));
        assert!(b.rx_parse.is_empty());
    }

    #[test]
    fn guest_sending_a_host_only_frame_is_reset() {
        let (mut b, _m) = setup();
        b.pump(&framed(EgressFrame::OpenOk { stream: 5 })).unwrap();
        let frames = b.drain_to_guest(usize::MAX);
        let d = decode(&frames).unwrap().unwrap();
        assert_eq!(d.frame, EgressFrame::Rst { stream: 5 });
    }

    #[test]
    fn guest_to_host_overrun_resets_the_stream() {
        let (mut b, m) = setup();
        establish(&mut b, &m, 1);
        m.set_write_blocked(1, true); // nothing drains to the socket
        // A run of max-size DATA frames accumulates past the cap, forcing a reset.
        let chunk = vec![0u8; 60_000];
        let mut input = Vec::new();
        for _ in 0..=(TO_SOCKET_CAP / chunk.len()) {
            input.extend(framed(EgressFrame::Data { stream: 1, payload: &chunk }));
        }
        b.pump(&input).unwrap();
        assert!(!b.sessions.contains_key(&StreamId(1)));
        let frames = b.drain_to_guest(usize::MAX);
        // OPEN_OK then RST.
        let d0 = decode(&frames).unwrap().unwrap();
        assert_eq!(d0.frame, EgressFrame::OpenOk { stream: 1 });
        let d1 = decode(&frames[d0.consumed..]).unwrap().unwrap();
        assert_eq!(d1.frame, EgressFrame::Rst { stream: 1 });
    }

    #[test]
    fn state_summary_names_the_terms() {
        let (mut b, _m) = setup();
        b.rx_parse.push(1);
        assert!(b.state_summary().contains("rx_parse=1B"));
    }

    #[test]
    fn note_gated_interval_accumulates() {
        let (mut b, _m) = setup();
        b.note_gated_interval(1000);
        b.note_gated_interval(500);
        assert_eq!(b.stats().gated_intervals, 2);
        assert_eq!(b.stats().gated_real_ns, 1500);
    }

    // ---- RealConnector lifecycle (no network) --------------------------------

    #[test]
    fn real_connector_backend_constructs_and_drops_cleanly() {
        // Exercises the real epoll fd + resolver eventfd + worker-thread lifecycle
        // without ever requesting a resolution, so no DNS or socket is touched.
        let b = EgressBackend::new().expect("real backend builds");
        assert!(b.epoll_fd() >= 0);
        assert!(b.is_quiescent());
        drop(b); // joins the resolver thread
    }

    #[test]
    fn a_dead_resolver_yields_a_synthetic_failure_not_a_leak() {
        // Simulate a resolver that can no longer receive requests (its channel is
        // gone) — the same outcome as a panicked worker. start_resolve must then
        // synthesize a ResolveFailed answer so take_resolved still hands one back
        // and the in-flight count decrements.
        let mut c = RealConnector::new().unwrap();
        c.to_worker.take();
        if let Some(h) = c.worker.take() {
            let _ = h.join();
        }
        c.start_resolve(StreamId(7), "example.com".into(), 80);
        let answers = c.take_resolved();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].stream, StreamId(7));
        assert!(answers[0].addr.is_none());
    }

    // ---- T3a/T3b: the always-on gate assert (the shipped tripwire) ------------

    #[test]
    fn assert_ff_jump_legal_allows_off_and_quiescent() {
        // Egress off (None) is always legal, and a fresh (quiescent) backend is too.
        assert_ff_jump_legal(None);
        let (b, _m) = setup();
        assert_ff_jump_legal(Some(&b));
    }

    #[test]
    #[should_panic(expected = "egress gate breached")]
    fn assert_ff_jump_legal_panics_on_a_nonquiescent_backend() {
        // A single open session must trip the always-on tripwire. This is T3a: the
        // detector is a shipped test, not just a claim.
        let (mut b, _m) = setup();
        b.pump(&open(1)).unwrap(); // Resolving -> non-quiescent
        assert!(!b.is_quiescent());
        assert_ff_jump_legal(Some(&b)); // must panic
    }

    #[test]
    #[should_panic(expected = "egress gate breached")]
    fn t3b_a_real_open_session_trips_the_tripwire() {
        // T3b negative control, driven HOST-side (no guest forwarder yet): a REAL
        // TCP session to a loopback listener makes the backend non-quiescent, and
        // the always-on assert — which fires even when TDVMM_EGRESS_UNSAFE_JUMPS
        // skips the gate — aborts. Proves the tripwire catches a live connection.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut ch = EgressChannel::new().unwrap();
        let mut parker = Parker::new().unwrap();
        let (mut lapic, ioapic) = armed_pin3();
        write_guest_frame(&mut ch, &mut lapic, &ioapic, &framed(EgressFrame::Open {
            stream: 1,
            port,
            host: b"127.0.0.1",
        }));
        let _server = drive_to_established(&mut ch, &mut parker, &mut lapic, &ioapic, &listener);
        assert!(!ch.is_quiescent(), "a live TCP session must be non-quiescent");
        // The gate would normally have parked instead of reaching here; the assert
        // is the last line even when the gate is skipped.
        assert_ff_jump_legal(Some(ch.backend())); // must panic
    }

    // ---- the park-wake proof (host half of the chain) ------------------------

    #[test]
    fn park_wake_delivers_a_late_egress_response_through_the_epoll_fd() {
        // The exact P2-checkpoint sequence, host side: a stream is open with NO
        // host bytes yet, so the park blocks; a late host write makes the backend
        // epoll fd ready; the park wakes (wakes.egress); the boundary pump frames
        // the response into the COM4 FIFO and raises IRQ3 — ready for the guest to
        // read. (The final guest-read hop is the P4 e2e with a real forwarder.)
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut ch = EgressChannel::new().unwrap();
        let mut parker = Parker::new().unwrap();
        let (mut lapic, ioapic) = armed_pin3();
        enable_rx_irq_com4(&mut ch, &mut lapic, &ioapic);

        // Guest opens a stream; drive it to Established over the loopback connect.
        write_guest_frame(&mut ch, &mut lapic, &ioapic, &framed(EgressFrame::Open {
            stream: 1,
            port,
            host: b"127.0.0.1",
        }));
        let server = drive_to_established(&mut ch, &mut parker, &mut lapic, &ioapic, &listener);
        assert!(!ch.is_quiescent(), "an open session is non-quiescent (gate holds)");
        let down_before = ch.stats().bytes_down;

        // Park with the egress fd BEFORE any host bytes exist: it must NOT wake on
        // egress (the guest would be blocked at HLT on a ttyS3 read here).
        let w = parker.park(Some(150_000_000), Some(ch.epoll_fd())).unwrap();
        assert!(!w.egress, "park must block until host-side bytes exist");

        // Late host write: the client socket becomes readable -> epoll fd signals.
        (&server).write_all(b"late-egress-response").unwrap();
        let w = wait_for_egress_wake(&mut parker, ch.epoll_fd());
        assert!(w.egress, "the backend epoll fd must wake the park");

        // Boundary pump: frame the response toward the guest + feed FIFO + IRQ3.
        ch.pump(&mut lapic, &ioapic).unwrap();
        assert!(ch.stats().bytes_down > down_before, "pump frames the response toward the guest");
        assert!(ch.rx_fifo_free() < 64, "the framed response is staged in the COM4 RX FIFO");
        assert_eq!(lapic.deliverable_vector(), Some(GATE_TEST_VECTOR), "IRQ3 is raised for the guest");
    }

    // ---- concurrent COM2 + COM4 on the shared IRQ3 line ----------------------

    #[test]
    fn shared_irq3_services_both_com2_and_com4_together() {
        // Both channels raise IRQ3 (pin 3) and both stage guest-bound bytes in one
        // pump round — the guest's single shared 8250 handler would then poll both
        // ttyS1 and ttyS3. The P2 spike proved them separately; this proves them
        // serviced together from the one line.
        let (mut lapic, ioapic) = armed_pin3();

        // COM2: a control command -> its FIFO fed + IRQ3 raised. Enable its RX
        // interrupt first (the guest agent would), so the enqueue asserts the edge.
        let mut com2 = ControlChannel::new().unwrap();
        com2.pio_write(crate::arch::SERIAL2_PORT_BASE + 1, 0x01, &mut lapic, &ioapic);
        let cmd = b"{\"op\":\"ping\"}\n";
        com2.send_frame(cmd);
        com2.pump(&mut lapic, &ioapic);
        assert_eq!(lapic.deliverable_vector(), Some(GATE_TEST_VECTOR), "COM2 asserts IRQ3");

        // COM4: an egress stream reaches Established (OPEN_OK staged toward guest);
        // its pump feeds the COM4 FIFO + raises the SAME IRQ3 line.
        let mock = MockConnector::new();
        let mut com4 = EgressChannel::wrap(
            EgressBackend::with_connector(Box::new(mock.clone())).unwrap(),
        )
        .unwrap();
        enable_rx_irq_com4(&mut com4, &mut lapic, &ioapic);
        write_guest_frame(&mut com4, &mut lapic, &ioapic, &open(1));
        com4.pump(&mut lapic, &ioapic).unwrap(); // dispatch resolve
        mock.complete_resolve(1, Some(addr()));
        com4.pump(&mut lapic, &ioapic).unwrap(); // harvest -> connect -> Established -> OPEN_OK -> FIFO + IRQ3

        // The shared line is asserted and COM4's FIFO holds its guest-bound bytes.
        assert_eq!(lapic.deliverable_vector(), Some(GATE_TEST_VECTOR), "COM4 asserts the SAME IRQ3");
        assert!(com4.rx_fifo_free() < 64, "COM4 FIFO holds the OPEN_OK toward the guest");
        // COM2's FIFO still holds its command (both serviced, neither clobbered):
        // the guest's first RBR read on ttyS1 returns the command's first byte.
        let first = com2.pio_read(crate::arch::SERIAL2_PORT_BASE, &mut lapic, &ioapic);
        assert_eq!(first, cmd[0], "COM2 FIFO delivered its command byte to the guest");
    }

    // ---- FIFO backpressure preserves the quiescence invariant ----------------

    #[test]
    fn fifo_backpressure_keeps_undelivered_bytes_counted_in_tx_pending() {
        // A host->guest payload larger than the 64-byte RX FIFO must NOT be dropped
        // into a side buffer: the un-fed remainder stays in the backend's
        // tx_pending (a quiescence term), so `E > 0` holds until the guest drains
        // the FIFO and later pumps deliver the rest. This is the safety property
        // that lets the channel wrap the backend without hiding host->guest bytes.
        let (mut lapic, ioapic) = armed_pin3();
        let mock = MockConnector::new();
        let mut ch = EgressChannel::wrap(
            EgressBackend::with_connector(Box::new(mock.clone())).unwrap(),
        )
        .unwrap();
        write_guest_frame(&mut ch, &mut lapic, &ioapic, &open(1));
        ch.pump(&mut lapic, &ioapic).unwrap();
        mock.complete_resolve(1, Some(addr()));
        ch.pump(&mut lapic, &ioapic).unwrap(); // Established (+ OPEN_OK)
        // Drain the OPEN_OK the guest would have read, so the FIFO starts empty.
        drain_fifo(&mut ch, &mut lapic, &ioapic);
        // A big readable payload: framed toward the guest, far exceeding the FIFO.
        mock.push_recv(1, &[0xAB; 4096]);
        ch.pump(&mut lapic, &ioapic).unwrap();
        assert_eq!(ch.rx_fifo_free(), 0, "the RX FIFO is full");
        assert!(
            ch.backend().has_frames_for_guest(),
            "the un-fed remainder stays in tx_pending (E > 0), not a hidden buffer"
        );
        assert!(!ch.is_quiescent(), "undelivered host->guest bytes keep egress non-quiescent");
    }

    // ---- the long-gate WARN core (telemetry only) ----------------------------

    #[test]
    fn long_gate_warn_fires_after_30s_rate_limited_and_resets_on_clear() {
        let t0 = Instant::now();
        let mut gw = GateWarn::default();
        gw.note(t0);
        // Under 30s: silent.
        assert!(gw.warn_at(t0 + Duration::from_secs(29), 1, 29.0).is_none());
        // Past 30s: one WARN, naming the open-session count.
        let m = gw.warn_at(t0 + Duration::from_secs(31), 2, 31.0).expect("warn at 31s");
        assert!(m.contains("gated by egress"));
        assert!(m.contains("2 open session"));
        // Within the cooldown: suppressed.
        assert!(gw.warn_at(t0 + Duration::from_secs(45), 2, 45.0).is_none());
        // Past the cooldown: warns again.
        assert!(gw.warn_at(t0 + Duration::from_secs(62), 2, 62.0).is_some());
        // clear() ends the span: a fresh note starts a new 30s window (times chosen
        // past the cooldown, so the silence is due to the span reset, not cooldown).
        gw.clear();
        gw.note(t0 + Duration::from_secs(120));
        assert!(gw.warn_at(t0 + Duration::from_secs(125), 1, 125.0).is_none());
    }

    // ---- shared test scaffolding for the integration tests -------------------

    /// The IO-APIC pin-3 vector the tests program, so `raise_irq(.., IRQ3)` lands a
    /// deliverable interrupt on the enabled LAPIC.
    const GATE_TEST_VECTOR: u8 = 0x33;

    /// A LAPIC (enabled) + IO-APIC with pin 3 (IRQ3, shared COM2+COM4) programmed
    /// to [`GATE_TEST_VECTOR`], edge, unmasked — so a raised IRQ3 is deliverable.
    fn armed_pin3() -> (Lapic, Ioapic) {
        let clock = VirtualClock::new(0, TscFrequency::from_hz(1_000_000_000));
        let mut lapic = Lapic::new(clock, 160, 2);
        // Enable the LAPIC: SVR (MMIO 0xf0) enable bit (1<<8) + a spurious vector.
        lapic.mmio_write(0x0f0, (1 << 8) | 0xff);
        let mut ioapic = Ioapic::new(2);
        // Program redirection entry for pin 3 (low dword at REDTBL index 0x10+3*2).
        let idx = 0x10 + 3 * 2;
        ioapic.mmio_write(IOAPIC_BASE, idx);
        ioapic.mmio_write(IOAPIC_BASE + 0x10, u32::from(GATE_TEST_VECTOR)); // vector, edge, unmasked
        ioapic.mmio_write(IOAPIC_BASE, idx + 1);
        ioapic.mmio_write(IOAPIC_BASE + 0x10, 0);
        (lapic, ioapic)
    }

    /// Simulate the guest enabling COM4's RX-data-available interrupt (IER bit 0,
    /// register offset 1) — the 8250 driver does this when it opens ttyS3. Without
    /// it, `vm-superio` enqueues RX bytes but never asserts the interrupt, so no
    /// IRQ3 edge is produced (the same discipline holds for COM2).
    fn enable_rx_irq_com4(ch: &mut EgressChannel, lapic: &mut Lapic, ioapic: &Ioapic) {
        ch.pio_write(arch::SERIAL4_PORT_BASE + 1, 0x01, lapic, ioapic);
    }

    /// Simulate the guest writing `frame` to /dev/ttyS3 byte-by-byte (THR PIO),
    /// exactly as the forwarder would — captured for the next [`EgressChannel::pump`].
    fn write_guest_frame(ch: &mut EgressChannel, lapic: &mut Lapic, ioapic: &Ioapic, frame: &[u8]) {
        for &b in frame {
            ch.pio_write(arch::SERIAL4_PORT_BASE, b, lapic, ioapic);
        }
    }

    /// Drive an opened stream to `Established` over a real loopback connect,
    /// returning the accepted server-side socket.
    fn drive_to_established(
        ch: &mut EgressChannel,
        parker: &mut Parker,
        lapic: &mut Lapic,
        ioapic: &Ioapic,
        listener: &TcpListener,
    ) -> std::net::TcpStream {
        listener.set_nonblocking(true).unwrap();
        let mut server = None;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            ch.pump(lapic, ioapic).unwrap();
            if server.is_none() {
                if let Ok((s, _)) = listener.accept() {
                    server = Some(s);
                }
            }
            if state(ch.backend(), 1) == Some(SessionState::Established) {
                if let Some(s) = server.take() {
                    return s;
                }
            }
            // Wait on the backend epoll fd (resolver eventfd / socket writable) so
            // the connect completes without busy-spinning.
            let _ = parker.park(Some(50_000_000), Some(ch.epoll_fd())).unwrap();
        }
        panic!("stream did not reach Established within 5s");
    }

    /// Park (with the egress fd) until an egress wake arrives, or fail after 5s.
    fn wait_for_egress_wake(parker: &mut Parker, fd: RawFd) -> crate::park::Wakes {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let w = parker.park(Some(200_000_000), Some(fd)).unwrap();
            if w.egress {
                return w;
            }
        }
        panic!("egress wake never arrived");
    }

    /// Drain the COM4 RX FIFO by simulating guest RBR reads until it is empty.
    fn drain_fifo(ch: &mut EgressChannel, lapic: &mut Lapic, ioapic: &Ioapic) {
        while ch.rx_fifo_free() < 64 {
            let _ = ch.pio_read(arch::SERIAL4_PORT_BASE, lapic, ioapic);
        }
    }
}
