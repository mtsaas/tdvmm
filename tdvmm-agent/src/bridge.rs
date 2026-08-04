//! The agent's control+event I/O: a `\n`-framing `LineReader` over any `Read`, a
//! typed `poll2` wrapper, and the `run_loop` that multiplexes ttyS1 (control) and
//! the guest event FIFO under one blocked poll. Framing lives here, so no
//! `BufReader` ever strands a complete line while `poll` blocks on the fd.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;

use tdvmm_proto::{decode_line, encode_line, ErrorKind, GuestEvent, Reply, Request};

use crate::agent::Agent;
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

/// Block until ttyS1 and/or the (optional) event FIFO is readable; return
/// `(control_ready, events_ready)`. Infinite timeout ⇒ no timer armed ⇒
/// fast-forward-transparent, exactly like the former blocked read. EINTR retries.
fn poll2(control_fd: i32, events_fd: Option<i32>) -> io::Result<(bool, bool)> {
    let mut fds = [
        PollFd { fd: control_fd, events: POLLIN, revents: 0 },
        PollFd { fd: events_fd.unwrap_or(-1), events: POLLIN, revents: 0 },
    ];
    let nfds: u64 = if events_fd.is_some() { 2 } else { 1 };
    loop {
        let r = unsafe { poll(fds.as_mut_ptr(), nfds, -1) };
        if r >= 0 {
            break;
        }
        if r != -EINTR {
            return Err(io::Error::from_raw_os_error(-r as i32));
        }
    }
    let ready = |p: &PollFd| p.revents & (POLLIN | POLLHUP | POLLERR) != 0;
    Ok((ready(&fds[0]), events_fd.is_some() && ready(&fds[1])))
}

/// The agent's core: multiplex ttyS1 (control) and the event FIFO under one blocked
/// `poll`, draining each as an iterator of complete lines. Control is served before
/// events. Control EOF/error stops the agent (as before); a FIFO hiccup never does.
pub(crate) fn run_loop(control: File, events: Option<File>, writer: &mut File, agent: &mut Agent) {
    let control_fd = control.as_raw_fd();
    let events_fd = events.as_ref().map(|f| f.as_raw_fd());
    let mut control = LineReader::new(control);
    let mut events = events.map(LineReader::new);
    let mut seq: u64 = 0;

    loop {
        let (control_ready, events_ready) = match poll2(control_fd, events_fd) {
            Ok(r) => r,
            Err(_) => return,
        };

        if control_ready {
            match control.fill() {
                Ok(true) => {
                    for line in control.by_ref() {
                        handle_control_line(&line, writer, agent);
                    }
                }
                _ => return, // EOF or read error on the control channel: stop.
            }
        }

        // `events_ready` implies the FIFO half is present. Because the agent holds
        // the FIFO `O_RDWR`, it is always a writer, so a read never reports EOF
        // (`Ok(false)`); only an `Err` skips the batch.
        if let Some(events) = events.as_mut().filter(|_| events_ready) {
            if matches!(events.fill(), Ok(true)) {
                for line in events.by_ref() {
                    seq += 1;
                    write_line(writer, &Reply::from_event(seq, parse_event(&line)));
                }
            }
        }
    }
}

/// Handle one complete control (ttyS1) line: decode a [`Request`], dispatch, reply.
/// Byte-for-byte the former inline behavior.
fn handle_control_line(line: &[u8], writer: &mut File, agent: &mut Agent) {
    if tdvmm_proto::trim_frame(line).is_empty() {
        return;
    }
    let req: Request = match decode_line(line) {
        Ok(r) => r,
        Err(e) => {
            write_line(writer, &bad_request(e.to_string()));
            return;
        }
    };
    let reply = agent.handle(&req);
    write_line(writer, &reply);
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
pub(crate) fn parse_event(line: &[u8]) -> GuestEvent {
    let trimmed = tdvmm_proto::trim_frame(line);
    let capped = &trimmed[..trimmed.len().min(FIFO_EVENT_CAP)];
    match serde_json::from_slice::<GuestEvent>(capped) {
        Ok(ev) if !ev.kind.is_empty() => ev,
        _ => {
            let raw = String::from_utf8_lossy(&capped[..capped.len().min(256)]).into_owned();
            GuestEvent {
                kind: "invalid".into(),
                details: Some(serde_json::json!({ "raw": raw })),
                ..Default::default()
            }
        }
    }
}

pub(crate) fn write_line<W: Write>(w: &mut W, reply: &Reply) {
    if let Ok(bytes) = encode_line(reply) {
        let _ = w.write_all(&bytes);
        let _ = w.flush();
    }
}
