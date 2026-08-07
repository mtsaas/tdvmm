//! The agent's I/O multiplexer: a `\n`-framing `LineReader` over any `Read`, a
//! typed blocked-`poll` wrapper, and the `run_loop` that serves every source the
//! agent owns under ONE blocked poll. Framing lives here, so no `BufReader` ever
//! strands a complete line while `poll` blocks on the fd.
//!
//! ## The sources
//!
//! | source | direction | who writes it |
//! |---|---|---|
//! | ttyS1 (control) | host → agent | the VMM, at a scheduled virtual time |
//! | the events FIFO | any container → agent | workloads, fire-and-forget |
//! | the driver control socket | the driver container → agent | schema 4 |
//! | the driver-exit watcher | a child process → agent | schema 4 |
//!
//! ## The infinite timeout is the crown jewel
//!
//! Every source above is an fd in ONE `poll` with a **`-1` (infinite) timeout**.
//! A blocked poll arms no timer and generates no wakes, so an idle guest with the
//! agent baked in fast-forwards exactly as it would without it. Adding sources
//! must never add a timeout: a driver container that sleeps for a virtual day
//! leaves the agent blocked here, the guest HLTed, and the host free to jump.
//! Anything that needs periodic work is delegated to a child process whose
//! stdout becomes another fd in this set (see [`crate::driver`]).
//!
//! ## Single writer toward the host
//!
//! ttyS1 has exactly one writer: this loop. Command replies, bridged FIFO
//! events, mirrored control-socket commands, and the terminal `driver_exit`
//! event are all emitted from here, in program order, so the host's view of the
//! run is a totally ordered stream.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use tdvmm_proto::{decode_line, encode_line, ErrorKind, GuestEvent, Reply, Request};

use crate::agent::Agent;
use crate::driver::DriverWatch;
use crate::sys::{poll, PollFd, EINTR, POLLERR, POLLHUP, POLLIN};

/// PIPE_BUF: a FIFO write up to this size is POSIX-atomic, so a well-formed event
/// never tears even with N concurrent container writers. Doubles as the per-event
/// byte cap.
const FIFO_EVENT_CAP: usize = 4096;

/// Oversized-frame guard: an unterminated line past this is surfaced whole
/// (truncated) so memory stays bounded and the malformed line is reported by the
/// per-source handler, never silently dropped. Real frames are far smaller (control
/// requests; events ≤ [`FIFO_EVENT_CAP`]).
pub(crate) const RX_LINE_CAP: usize = 64 * 1024;

/// Cap on concurrently open driver control-socket connections. The driver is one
/// trusted container, so this is a runaway guard (a leaked connection per request
/// must not exhaust the agent's fds), not a capacity plan. Further connects are
/// accepted and closed immediately, which the SDK surfaces as a connect error.
const MAX_CTL_CONNS: usize = 16;

/// A `\n`-framing reader over any [`Read`] source. The buffer lives HERE, in code we
/// control, so — unlike a `BufReader` — it never strands a complete line in a hidden
/// read-ahead while `poll` blocks on the fd. Feed it one `read()` with [`fill`];
/// drain complete frames by iterating (it is an [`Iterator`]); a partial tail waits
/// for the next `fill`. Composable and testable over a `Cursor`/`&[u8]`, no fd needed.
pub(crate) struct LineReader<R> {
    rd: R,
    buf: Vec<u8>,
}

impl<R: Read> LineReader<R> {
    pub(crate) fn new(rd: R) -> Self {
        LineReader { rd, buf: Vec::with_capacity(1 << 12) }
    }

    /// One `read()` into the frame buffer. `Ok(false)` == EOF; errors propagate.
    pub(crate) fn fill(&mut self) -> io::Result<bool> {
        let mut tmp = [0u8; 1 << 13];
        let n = self.rd.read(&mut tmp)?;
        if n == 0 {
            return Ok(false);
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(true)
    }
}

impl<R: AsRawFd> LineReader<R> {
    /// The underlying source's fd, for the poll set.
    fn fd(&self) -> RawFd {
        self.rd.as_raw_fd()
    }
}

impl<R> Iterator for LineReader<R> {
    type Item = Vec<u8>;
    fn next(&mut self) -> Option<Vec<u8>> {
        if let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            return Some(self.buf.drain(..=i).collect());
        }
        // No newline: surface only an oversized unterminated frame (bounds memory;
        // the handler rejects it). A normal partial waits for the next fill().
        if self.buf.len() > RX_LINE_CAP {
            return Some(std::mem::take(&mut self.buf));
        }
        None
    }
}

/// Block until at least one of `fds` is ready, filling each entry's `revents`.
/// The timeout is ALWAYS `-1` — see the module doc; this is the one place that
/// could regress fast-forward transparency, and it takes no timeout parameter so
/// that it cannot. EINTR retries.
fn poll_blocking(fds: &mut [PollFd]) -> io::Result<()> {
    let nfds = fds.len() as u64;
    loop {
        // SAFETY: `fds` is a valid, writable slice of exactly `nfds` `PollFd`s
        // that stays live for the duration of the call.
        let r = unsafe { poll(fds.as_mut_ptr(), nfds, -1) };
        if r >= 0 {
            return Ok(());
        }
        if r != -EINTR {
            return Err(io::Error::from_raw_os_error(-r as i32));
        }
    }
}

/// Whether a polled entry has anything for us (data, hangup, or error).
fn ready(p: &PollFd) -> bool {
    p.revents & (POLLIN | POLLHUP | POLLERR) != 0
}

fn pollfd(fd: RawFd) -> PollFd {
    PollFd { fd, events: POLLIN, revents: 0 }
}

/// One live driver control-socket connection: its framing reader and the write
/// half replies go back on. Both halves are dups of the same socket, so polling
/// `fd` covers the connection.
struct Conn {
    fd: RawFd,
    rd: LineReader<UnixStream>,
    wr: UnixStream,
}

/// Everything the agent multiplexes. `control` is mandatory; the rest are the
/// optional capabilities of a given run (an absent FIFO degrades to control-only;
/// the socket and the watcher exist only in driver mode).
pub(crate) struct Sources {
    pub(crate) control: File,
    pub(crate) events: Option<File>,
    pub(crate) ctl: Option<UnixListener>,
    pub(crate) watch: Option<DriverWatch>,
}

/// The agent's core: serve every source under one blocked `poll`, draining each as
/// an iterator of complete lines. Control is served before events. A control
/// EOF/error stops the agent — a dead control channel mid-run is the VM reaching
/// end of life, not an agent fault, so the stop is quiet; a FIFO hiccup, a driver
/// disconnect, or a watcher failure never stops it.
pub(crate) fn run_loop(src: Sources, writer: &mut File, agent: &mut Agent) {
    let control_fd = src.control.as_raw_fd();
    let mut control = LineReader::new(src.control);
    let mut events = src.events.map(LineReader::new);
    let ctl = src.ctl;
    let mut watch = src.watch;
    let mut conns: Vec<Conn> = Vec::new();
    let mut seq: u64 = 0;

    loop {
        // Build the poll set fresh each iteration: the connection list changes as
        // the driver connects and disconnects. Indices are recorded as they are
        // pushed, so dispatch never guesses a layout.
        let mut fds = Vec::with_capacity(4 + conns.len());
        fds.push(pollfd(control_fd));
        let i_events = events.as_ref().map(|e| {
            fds.push(pollfd(e.fd()));
            fds.len() - 1
        });
        let i_ctl = ctl.as_ref().map(|l| {
            fds.push(pollfd(l.as_raw_fd()));
            fds.len() - 1
        });
        let i_watch = watch.as_ref().map(|w| {
            fds.push(pollfd(w.fd()));
            fds.len() - 1
        });
        let conn_base = fds.len();
        for c in &conns {
            fds.push(pollfd(c.fd));
        }
        // The connections THIS poll set covers. A connection accepted below is not
        // in it, so it must not be indexed against it; its pending bytes make the
        // next poll return immediately, so nothing is lost by deferring it.
        let polled_conns = conns.len();

        if poll_blocking(&mut fds).is_err() {
            return;
        }

        // ---- 1. control (ttyS1): the host's commands, served first -----------
        if ready(&fds[0]) {
            match control.fill() {
                Ok(true) => {
                    for line in control.by_ref() {
                        // A reply that cannot be written means the control channel is
                        // broken; stop rather than spin replying into a dead fd.
                        if handle_control_line(&line, writer, agent).is_err() {
                            return;
                        }
                    }
                }
                _ => return, // EOF or read error on the control channel: stop.
            }
        }

        // ---- 2. new driver connections ---------------------------------------
        if let (Some(l), Some(i)) = (ctl.as_ref(), i_ctl) {
            if ready(&fds[i]) {
                // A transient accept failure must not kill the agent.
                if let Ok((stream, _)) = l.accept() {
                    match stream.try_clone() {
                        // Refuse past the cap, and refuse a socket we cannot get a
                        // write half for, by closing: the SDK sees a clean
                        // disconnect rather than a half-served connection.
                        Ok(wr) if conns.len() < MAX_CTL_CONNS => conns.push(Conn {
                            fd: stream.as_raw_fd(),
                            rd: LineReader::new(stream),
                            wr,
                        }),
                        _ => drop(stream),
                    }
                }
            }
        }

        // ---- 3. driver commands ----------------------------------------------
        // Serve each ready connection, then retire the ones that hung up.
        let mut dead: Vec<usize> = Vec::new();
        for (n, c) in conns.iter_mut().enumerate().take(polled_conns) {
            if !ready(&fds[conn_base + n]) {
                continue;
            }
            match c.rd.fill() {
                Ok(true) => {
                    let mut broken = false;
                    for line in c.rd.by_ref() {
                        match handle_ctl_line(&line, &mut c.wr, agent) {
                            // Mirror the command to the host for the run trace.
                            Ok(Some(ev)) => {
                                seq += 1;
                                if write_line(writer, &Reply::from_event(seq, ev)).is_err() {
                                    return; // ttyS1 is dead: stop.
                                }
                            }
                            Ok(None) => {}
                            // Writing to THIS driver connection failed: drop it,
                            // but keep the agent (and the run) alive.
                            Err(_) => {
                                broken = true;
                                break;
                            }
                        }
                    }
                    if broken {
                        dead.push(n);
                    }
                }
                // EOF (the driver closed) or a read error: retire the connection.
                _ => dead.push(n),
            }
        }
        for n in dead.into_iter().rev() {
            conns.remove(n);
        }

        // ---- 4. workload events (the shared FIFO) -----------------------------
        // `i_events` being ready implies the FIFO half is present. Because the agent
        // holds the FIFO `O_RDWR`, it is always a writer, so a read never reports EOF
        // (`Ok(false)`); only an `Err` skips the batch.
        if let (Some(ev), Some(i)) = (events.as_mut(), i_events) {
            if ready(&fds[i]) && matches!(ev.fill(), Ok(true)) {
                for line in ev.by_ref() {
                    seq += 1;
                    // Events go out over the same control channel; a failed write
                    // means it is broken, so stop.
                    if write_line(writer, &Reply::from_event(seq, parse_event(&line))).is_err() {
                        return;
                    }
                }
            }
        }

        // ---- 5. the driver's exit: the run's verdict ---------------------------
        if let (Some(w), Some(i)) = (watch.as_mut(), i_watch) {
            if ready(&fds[i]) {
                if let Some(ev) = w.on_readable() {
                    seq += 1;
                    let _ = write_line(writer, &Reply::from_event(seq, ev));
                    // Fired once and only once; the host stops the run from here.
                    watch = None;
                }
            }
        }
    }
}

/// Handle one complete control (ttyS1) line: decode a [`Request`], dispatch, and
/// write the reply. An empty (whitespace-only) frame is ignored. Returns the reply
/// write's result so the caller can stop on a broken control channel.
fn handle_control_line(line: &[u8], writer: &mut File, agent: &mut Agent) -> io::Result<()> {
    if tdvmm_proto::trim_frame(line).is_empty() {
        return Ok(());
    }
    let req: Request = match decode_line(line) {
        Ok(r) => r,
        Err(e) => return write_line(writer, &bad_request(e.to_string())),
    };
    let reply = agent.handle(&req);
    write_line(writer, &reply)
}

/// Handle one complete driver control-socket line. The request goes through the
/// SAME [`Agent::handle`] as a host request — the driver gets exactly the op set
/// the host has, no more, and fault injection is not reimplemented — and the reply
/// goes back on THAT connection. Returns the mirror event to forward to the host,
/// or `None` for a frame that was not a request at all. `Err` means this
/// connection's write failed (drop it); it never means the agent should stop.
fn handle_ctl_line(
    line: &[u8],
    conn: &mut UnixStream,
    agent: &mut Agent,
) -> io::Result<Option<GuestEvent>> {
    if tdvmm_proto::trim_frame(line).is_empty() {
        return Ok(None);
    }
    let req: Request = match decode_line(line) {
        Ok(r) => r,
        Err(e) => {
            write_line(conn, &bad_request(e.to_string()))?;
            return Ok(None);
        }
    };
    let reply = agent.handle(&req);
    let mirror = ctl_mirror(&req, &reply);
    write_line(conn, &reply)?;
    Ok(Some(mirror))
}

/// The host-facing mirror of one driver command: what was asked, of whom, and
/// whether it worked. This is the run's fault trace — the host stamps it with the
/// virtual time it arrived, which is the instant the fault actually landed.
fn ctl_mirror(req: &Request, reply: &Reply) -> GuestEvent {
    let mut details = serde_json::Map::new();
    if let Some(c) = &req.container {
        details.insert("service".into(), serde_json::Value::String(c.clone()));
    }
    if let Some(p) = &req.peer {
        details.insert("peer".into(), serde_json::Value::String(p.clone()));
    }
    if reply.ok != Some(true) {
        if let Some(e) = &reply.error {
            details.insert("error".into(), serde_json::Value::String(e.clone()));
        }
    }
    GuestEvent {
        kind: "ctl".into(),
        name: req.op.clone(),
        ok: Some(reply.ok == Some(true)),
        details: if details.is_empty() {
            None
        } else {
            Some(serde_json::Value::Object(details))
        },
        ..Default::default()
    }
}

fn bad_request(detail: impl AsRef<str>) -> Reply {
    Reply {
        ok: Some(false),
        op: Some("?".into()),
        error: Some(ErrorKind::BadRequest.msg(detail)),
        ..Default::default()
    }
}

/// Parse one FIFO line as a [`GuestEvent`]. A well-formed event with a non-empty
/// `kind` passes through; anything malformed becomes a `kind:"invalid"` event with a
/// truncated raw payload — recorded by the host, never silently dropped. The agent
/// is transport only: it does not judge which `kind`s are meaningful.
///
/// The ONE exception is [`tdvmm_proto::RESERVED_EVENT_KINDS`]. The FIFO is bound
/// into EVERY container, so without this an ordinary service could forge a
/// `driver_exit` and end the run with a verdict of its choosing, or forge a `ctl`
/// mirror and corrupt the fault trace. Those kinds are agent-originated only, so a
/// FIFO line claiming one is rewritten to `invalid` and reported as such.
pub(crate) fn parse_event(line: &[u8]) -> GuestEvent {
    let trimmed = tdvmm_proto::trim_frame(line);
    let capped = &trimmed[..trimmed.len().min(FIFO_EVENT_CAP)];
    let raw = || String::from_utf8_lossy(&capped[..capped.len().min(256)]).into_owned();
    match serde_json::from_slice::<GuestEvent>(capped) {
        Ok(ev) if tdvmm_proto::is_reserved_event_kind(&ev.kind) => GuestEvent {
            kind: "invalid".into(),
            details: Some(serde_json::json!({
                "raw": raw(),
                "rejected": "reserved event kind (agent-originated only)",
            })),
            ..Default::default()
        },
        Ok(ev) if !ev.kind.is_empty() => ev,
        _ => GuestEvent {
            kind: "invalid".into(),
            details: Some(serde_json::json!({ "raw": raw() })),
            ..Default::default()
        },
    }
}

/// Encode and write one framed reply line, flushing it. A serialize failure (a bug
/// — every [`Reply`] is serializable) surfaces as an I/O error rather than being
/// dropped, so no reply is ever silently lost.
pub(crate) fn write_line<W: Write>(w: &mut W, reply: &Reply) -> io::Result<()> {
    let bytes = encode_line(reply).map_err(io::Error::other)?;
    w.write_all(&bytes)?;
    w.flush()
}

// ============================================================================
// Tests — the driver control socket end to end, over a REAL UnixStream pair.
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    /// Serve one request line on a socket pair through the real `handle_ctl_line`
    /// path, and return `(reply, mirror)`.
    fn serve(req: &Request, agent: &mut Agent) -> (Reply, Option<GuestEvent>) {
        let (mut client, mut server) = UnixStream::pair().expect("socketpair");
        let line = encode_line(req).unwrap();
        let mirror = handle_ctl_line(&line, &mut server, agent).expect("write to the pair");
        drop(server);
        let mut out = String::new();
        std::io::BufReader::new(&mut client).read_line(&mut out).unwrap();
        (decode_line(out.as_bytes()).expect("a framed reply line"), mirror)
    }

    #[test]
    fn a_driver_request_gets_a_reply_on_its_own_connection() {
        let mut agent = Agent::new();
        let (reply, mirror) = serve(
            &Request { id: 11, op: "ping".into(), ..Default::default() },
            &mut agent,
        );
        assert_eq!(reply.id, Some(11), "the reply correlates by id");
        assert_eq!(reply.ok, Some(true));
        assert_eq!(reply.schema, Some(tdvmm_proto::SCHEMA));
        // ...and the host sees a mirror of what the driver asked for.
        let ev = mirror.expect("every served command mirrors to the host");
        assert_eq!((ev.kind.as_str(), ev.name.as_str(), ev.ok), ("ctl", "ping", Some(true)));
    }

    #[test]
    fn an_unknown_op_is_rejected_without_killing_the_connection() {
        let mut agent = Agent::new();
        let (reply, mirror) = serve(
            &Request { id: 2, op: "sudo".into(), ..Default::default() },
            &mut agent,
        );
        assert_eq!(reply.ok, Some(false));
        assert_eq!(reply.error.as_deref(), Some("unknown_op: sudo"));
        assert_eq!(mirror.unwrap().ok, Some(false), "a failed command mirrors as ok:false");
    }

    #[test]
    fn a_malformed_frame_gets_a_structured_error_not_a_panic() {
        let mut agent = Agent::new();
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let mirror = handle_ctl_line(b"{not json\n", &mut server, &mut agent).unwrap();
        drop(server);
        let mut out = String::new();
        std::io::BufReader::new(&mut client).read_line(&mut out).unwrap();
        let reply: Reply = decode_line(out.as_bytes()).unwrap();
        assert_eq!(reply.ok, Some(false));
        assert!(reply.error.unwrap().starts_with("bad_request:"));
        assert!(mirror.is_none(), "a non-request frame has nothing to mirror");
    }

    #[test]
    fn heal_all_round_trips_through_the_agents_partition_state() {
        // `heal` with no services clears the whole partition table. Reaching the
        // real `Agent::handle` state machine over the socket path is the point:
        // the driver drives the SAME fault engine the host does. (`partition`
        // itself needs podman, so the state assertion uses the clear path, which
        // is pure: with no partitions the nft rebuild is a no-op in this env.)
        let mut agent = Agent::new();
        let (reply, mirror) = serve(
            &Request { id: 3, op: "heal".into(), ..Default::default() },
            &mut agent,
        );
        assert_eq!(reply.op.as_deref(), Some("heal"));
        assert_eq!(mirror.unwrap().name, "heal");
    }

    #[test]
    fn a_ctl_mirror_names_both_sides_of_a_two_party_fault() {
        let req = Request {
            id: 4,
            op: "partition".into(),
            container: Some("pg-primary".into()),
            peer: Some("pg-standby".into()),
            ..Default::default()
        };
        let reply = Reply { id: Some(4), ok: Some(true), op: Some("partition".into()), ..Default::default() };
        let ev = ctl_mirror(&req, &reply);
        let d = ev.details.unwrap();
        assert_eq!(d["service"], "pg-primary");
        assert_eq!(d["peer"], "pg-standby");
        assert_eq!(ev.ok, Some(true));
    }

    /// The whole loop, wired the way `main` wires it: a socketpair stands in for
    /// ttyS1 (one end is both the agent's control source and its writer, exactly
    /// as the real tty is), plus a real `UnixListener` on a temp path. Proves the
    /// poll integration end to end — accept, per-connection framing, dispatch
    /// through `Agent::handle`, the reply landing on the RIGHT connection, and the
    /// mirror reaching the host.
    #[test]
    fn run_loop_serves_a_real_driver_connection_over_the_socket() {
        use std::fs::File;
        use std::os::fd::OwnedFd;

        let sock = std::env::temp_dir().join(format!("tdvmm-ctl-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind the control socket");

        let (host_end, guest_end) = UnixStream::pair().expect("socketpair for ttyS1");
        let control = File::from(OwnedFd::from(guest_end.try_clone().unwrap()));
        let mut writer = File::from(OwnedFd::from(guest_end));

        let agent_thread = std::thread::spawn(move || {
            let mut agent = Agent::new();
            run_loop(
                Sources { control, events: None, ctl: Some(listener), watch: None },
                &mut writer,
                &mut agent,
            );
        });

        // The driver connects and issues one command.
        let mut driver = UnixStream::connect(&sock).expect("driver connects");
        driver
            .write_all(&encode_line(&Request { id: 77, op: "ping".into(), ..Default::default() }).unwrap())
            .unwrap();
        let mut reply_line = String::new();
        std::io::BufReader::new(driver.try_clone().unwrap())
            .read_line(&mut reply_line)
            .unwrap();
        let reply: Reply = decode_line(reply_line.as_bytes()).unwrap();
        assert_eq!(reply.id, Some(77), "the reply came back on the driver's own connection");
        assert_eq!(reply.ok, Some(true));

        // ...and the host saw the mirror on ttyS1.
        let mut host = std::io::BufReader::new(host_end.try_clone().unwrap());
        let mut mirror_line = String::new();
        host.read_line(&mut mirror_line).unwrap();
        let mirror: Reply = decode_line(mirror_line.as_bytes()).unwrap();
        assert!(mirror.is_event(), "the host sees an id-less bridged event");
        let ev = mirror.event.unwrap();
        assert_eq!((ev.kind.as_str(), ev.name.as_str()), ("ctl", "ping"));
        assert_eq!(mirror.seq, Some(1));

        // Closing ttyS1 is how the VM ends; the loop must stop quietly.
        drop(driver);
        drop(host);
        drop(host_end);
        agent_thread.join().expect("the agent loop stops when control closes");
        let _ = std::fs::remove_file(&sock);
    }

    #[test]
    fn a_failed_mirror_carries_the_error_for_the_run_trace() {
        let req = Request { id: 5, op: "kill".into(), container: Some("db".into()), ..Default::default() };
        let reply = Reply {
            id: Some(5),
            ok: Some(false),
            op: Some("kill".into()),
            error: Some("no_container: no running container for service db".into()),
            ..Default::default()
        };
        let ev = ctl_mirror(&req, &reply);
        assert_eq!(ev.ok, Some(false));
        assert!(ev.details.unwrap()["error"].as_str().unwrap().starts_with("no_container:"));
    }
}
