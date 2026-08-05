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
//! [`EgressBackend::pump`] takes the guest bytes newly captured from the UART and
//! returns a [`PumpReport`]; it never touches the LAPIC. Feeding the RX FIFO and
//! raising IRQ3 for the frames in `tx_pending` is the caller's job (the VMM
//! wiring), which is why `pump` reports whether new frames became available
//! toward the guest rather than delivering them itself.

// The whole module is wired into the crate but called from nowhere yet. P3 is the
// VMM integration (construction, boundary pumps, the phase gate) that consumes
// this public surface; until then it is exercised only by this module's own tests
// and would otherwise read as dead in a non-test build. Remove this attribute as
// part of P3, once the backend has real call sites.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io;
use std::net::{SocketAddr, Shutdown, TcpStream, ToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use tdvmm_proto::egress::{decode, encode, EgressCodecError, EgressFrame, EgressReason, EGRESS_MAX_PAYLOAD};

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

/// What [`EgressBackend::pump`] made available for the caller to deliver. The
/// backend never raises the IRQ itself (it has no LAPIC); the caller feeds the
/// FIFO from `tx_pending` and raises IRQ3 when this reports new frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PumpReport {
    /// New frames became available toward the guest during this pump.
    pub frames_toward_guest: bool,
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

    /// The one effects function. Absorbs `from_guest` (bytes newly captured from
    /// the UART), parses whole frames, advances every session (resolve → connect →
    /// established → close), performs non-blocking socket I/O through the
    /// connector, harvests finished resolutions, and enqueues host→guest frames —
    /// applying the per-session window and resetting streams that overrun.
    ///
    /// It never touches the LAPIC: the returned [`PumpReport`] tells the caller
    /// whether to feed the FIFO and raise IRQ3.
    ///
    /// # Errors
    ///
    /// [`EgressError::Frame`] if the guest byte stream loses framing (the channel
    /// must then be torn down); [`EgressError::Io`] on an epoll failure.
    pub fn pump(&mut self, from_guest: &[u8]) -> Result<PumpReport, EgressError> {
        let before = self.tx_pending.len();
        self.ingest_and_parse(from_guest)?;
        self.harvest_resolves()?;
        self.service_sessions()?;
        Ok(PumpReport { frames_toward_guest: self.tx_pending.len() > before })
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
    use std::rc::Rc;
    use tdvmm_proto::egress::EGRESS_VERSION;

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
}
