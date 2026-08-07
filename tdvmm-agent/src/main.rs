//! `tdvmm-agent` — the guest-side control-channel executor.
//!
//! A tiny STATIC (musl) binary baked into every `.tdvmm`, running OUTSIDE the
//! workload containers. It is the guest end of the modeled control channel: the
//! 2nd 16550 (COM2 / ttyS1). Protocol: line-delimited JSON ([`tdvmm_proto`]), one
//! request per line in, one reply per line out.
//!
//! **Fast-forward transparency is the whole point of the transport:** the agent
//! BLOCKS reading `/dev/ttyS1`. A blocked read arms no timer and generates no
//! wakes, so an idle guest with the agent baked in fast-forwards exactly as it
//! would without it. When the VMM delivers a command (at its scheduled virtual
//! time) it raises IRQ3; the agent wakes, runs the command, writes one reply
//! line, and blocks again.
//!
//! Ops: `ping`, `exec`, `containers`, `kill`, `stop`, `start`, `partition`,
//! `heal`, `logs`. An unknown op is rejected. kill/stop/start WAIT for the
//! container to reach its new state so a following census is deterministic.
//! `logs` is a single BOUNDED read of a container's k8s-file log at a byte
//! cursor — never a follow/tail (`-f` would defeat host fast-forward) — so the
//! agent blocks again immediately after replying, exactly like every other op.
//!
//! ## Driver mode (schema 4)
//!
//! With `tdvmm.driver=<service>` on the kernel cmdline the agent also serves the
//! **driver control socket** ([`tdvmm_proto::CONTROL_SOCKET_PATH`]): the same
//! [`tdvmm_proto::Request`]/[`tdvmm_proto::Reply`] line JSON, over a unix socket
//! whose directory is bind-mounted into that ONE service — authorization IS the
//! mount, exactly like the events FIFO. A driver container therefore injects the
//! very faults the host can, through the very same [`agent::Agent`] dispatch, and
//! can do so WHILE its own requests to the cluster are in flight (the Jepsen
//! shape: workload and fault are one program). The agent also watches that
//! container and reports its exit as the run's verdict (see [`driver`]).
//!
//! Deps: `tdvmm-proto` + `serde_json` (+ `serde` derive, already transitive) +
//! `std` ONLY. Raw-mode termios is done with a std-only inline-asm `ioctl`
//! syscall — no `libc`.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::ExitCode;

use tdvmm_proto::Reply;

mod agent;
mod bridge;
mod driver;
mod forwarder;
mod sys;

use agent::Agent;
use bridge::{run_loop, write_line, Sources};
use driver::DriverWatch;

pub(crate) const AGENT_ID: &str = "tdvmm-agent/1";

/// Build hash embedded at compile time by the reproducible builder (the compat-
/// ibility oracle reported in the hello + `ping`). `dev` for plain host builds.
pub(crate) const BUILD: &str = match option_env!("TDVMM_AGENT_BUILD") {
    Some(s) => s,
    None => "dev",
};

fn main() -> ExitCode {
    // `tdvmm-agent forward` runs the egress SOCKS5h forwarder (COM4/ttyS3) instead
    // of the control-channel loop. The guest init starts it as a SEPARATE process
    // only on `tdvmm.egress=1`, so the default (no arg) path is unchanged.
    if std::env::args().nth(1).as_deref() == Some("forward") {
        return forwarder::run();
    }

    // The control channel is ttyS1. Open read+write; the VMM captures our TX and
    // feeds our RX at scheduled virtual times.
    let dev = std::env::var("TDVMM_AGENT_TTY").unwrap_or_else(|_| "/dev/ttyS1".to_string());
    let file = match OpenOptions::new().read(true).write(true).open(&dev) {
        Ok(f) => f,
        Err(e) => {
            // No control channel configured: nothing to do. Exit quietly (no wakes).
            eprintln!("tdvmm-agent: cannot open {dev}: {e}");
            return ExitCode::SUCCESS;
        }
    };

    // Raw mode is REQUIRED, not best-effort. The default tty line discipline ECHOes
    // input, which would bounce every received command straight back to the VMM as
    // spurious "reply" bytes and desync the line-delimited protocol; it also does
    // canonical line-buffering and \n<->\r\n translation, so the bytes on the wire
    // would no longer be what each side wrote. There is no usable fallback, so a
    // failure here is fatal. (VMIN=1/VTIME=0 => a read blocks until >=1 byte, arming
    // no timer, so it stays fast-forward-transparent.)
    if let Err(e) = sys::set_raw(file.as_raw_fd()) {
        eprintln!("tdvmm-agent: cannot set raw mode on {dev}: errno {e}");
        return ExitCode::FAILURE;
    }

    // The writer half. Without it we cannot reply, so the control channel is
    // unusable — fail loudly rather than run half-open.
    let mut writer = match file.try_clone() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("tdvmm-agent: cannot dup {dev}: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Event bridge (schema 3): a guest FIFO the workload containers write assertion
    // events to. O_RDWR is load-bearing — the agent's open never blocks, a container
    // `echo > fifo` never blocks, and poll never storms POLLHUP (the agent is always
    // a writer). An absent FIFO degrades to control-only.
    let fifo_path = std::env::var("TDVMM_AGENT_FIFO")
        .unwrap_or_else(|_| tdvmm_proto::EVENT_FIFO_PATH.to_string());
    let fifo = match OpenOptions::new().read(true).write(true).open(&fifo_path) {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("tdvmm-agent: no event FIFO at {fifo_path} ({e}); control-only");
            None
        }
    };

    let mut agent = Agent::new();

    // Driver mode (schema 4): `tdvmm.driver=<service>` names the ONE compose
    // service that drives this test. Bind its control socket and arm the
    // exit watcher BEFORE the hello, so "agent ready" implies "socket ready" and
    // a driver that connects the instant its container starts is never refused.
    let driver_service = driver_service_from_cmdline();
    let ctl = driver_service.as_deref().and_then(|svc| match bind_control_socket() {
        Ok(l) => {
            eprintln!(
                "tdvmm-agent: driver control socket on {} for service {svc}",
                tdvmm_proto::CONTROL_SOCKET_PATH
            );
            Some(l)
        }
        Err(e) => {
            // Without the socket the driver cannot inject faults. Report loudly and
            // keep serving ttyS1 — the host still gets a verdict from the driver's
            // exit, and the failure is visible in the console log.
            eprintln!(
                "tdvmm-agent: cannot bind {} ({e}); the driver cannot control the harness",
                tdvmm_proto::CONTROL_SOCKET_PATH
            );
            None
        }
    });
    let watch = driver_service.as_deref().and_then(|svc| match DriverWatch::spawn(svc) {
        Ok(w) => Some(w),
        Err(e) => {
            eprintln!("tdvmm-agent: cannot watch driver service {svc}: {e}");
            None
        }
    });

    // Proactive hello: the VMM's harness waits for this to mark the agent ready (no
    // ping round-trip needed). Carries schema + build (the compat oracle). If it
    // cannot be written the control channel is already dead, so give up.
    if let Err(e) = write_line(&mut writer, &Reply::hello(AGENT_ID, BUILD)) {
        eprintln!("tdvmm-agent: cannot write hello on {dev}: {e}");
        return ExitCode::FAILURE;
    }

    // One blocked poll over every source: ttyS1 (control), the event FIFO, and —
    // in driver mode — the control socket and the driver-exit watcher. An
    // infinite-timeout poll arms no timer and generates no wakes, so fast-forward
    // transparency holds no matter how many sources are attached.
    run_loop(Sources { control: file, events: fifo, ctl, watch }, &mut writer, &mut agent);
    ExitCode::SUCCESS
}

/// The driver service named by `tdvmm.driver=<service>` on the kernel cmdline.
/// The VMM appends it exactly when it runs in driver mode, mirroring how
/// `tdvmm.egress=1` gates the forwarder — so a plain `boot`/`run` never opens a
/// control socket at all.
fn driver_service_from_cmdline() -> Option<String> {
    let cmdline = std::env::var("TDVMM_AGENT_CMDLINE")
        .ok()
        .or_else(|| std::fs::read_to_string("/proc/cmdline").ok())?;
    cmdline
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("tdvmm.driver="))
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Create the control directory and bind the listener inside it.
///
/// The socket is made world-read/write on purpose: reaching it at all already
/// requires the directory bind, which only the driver service has, so the mount
/// IS the authorization — and the driver's image may well run as a non-root user
/// that could not otherwise connect. `/run` is a fresh tmpfs each boot, so the
/// unlink is belt-and-braces against a re-exec, not a real stale-socket case.
fn bind_control_socket() -> std::io::Result<UnixListener> {
    std::fs::create_dir_all(tdvmm_proto::CONTROL_DIR)?;
    let _ = std::fs::remove_file(tdvmm_proto::CONTROL_SOCKET_PATH);
    let listener = UnixListener::bind(tdvmm_proto::CONTROL_SOCKET_PATH)?;
    std::fs::set_permissions(
        tdvmm_proto::CONTROL_SOCKET_PATH,
        std::fs::Permissions::from_mode(0o666),
    )?;
    Ok(listener)
}

// ============================================================================
// Tests — the agent side of the GOLDEN round-trip (its real decode/encode paths).
// ============================================================================

#[cfg(test)]
mod tests {
    use crate::agent::{pair_key, read_log_chunk, Agent};
    use crate::bridge::{parse_event, LineReader, RX_LINE_CAP};
    use crate::AGENT_ID;
    use tdvmm_proto::{decode_line, encode_line, Reply, Request, SCHEMA};
    use serde_json::Value;
    use std::io::{self, Read};
    use std::path::PathBuf;

    fn goldens_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tdvmm-proto/goldens")
    }

    /// The agent's own code path: every REQUEST golden decodes via the same
    /// `decode_line::<Request>` the read loop uses, and every REPLY golden
    /// round-trips through `encode_line`/`decode_line` (what the agent emits).
    #[test]
    fn agent_roundtrips_goldens() {
        let dir = goldens_dir();
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        assert!(!files.is_empty());
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let raw = std::fs::read(&path).unwrap();
            let golden: Value = decode_line(&raw).unwrap();
            let reenc: Value = if name.starts_with("req_") {
                let m: Request = decode_line(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
                serde_json::from_slice(&encode_line(&m).unwrap()).unwrap()
            } else {
                let m: Reply = decode_line(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
                serde_json::from_slice(&encode_line(&m).unwrap()).unwrap()
            };
            assert_eq!(golden, reenc, "{name}: agent round-trip mismatch");
        }
    }

    #[test]
    fn unknown_op_is_rejected() {
        let mut a = Agent::new();
        let r = a.handle(&Request {
            id: 9,
            op: "frobnicate".into(),
            ..Default::default()
        });
        assert_eq!(r.ok, Some(false));
        assert_eq!(r.error.as_deref(), Some("unknown_op: frobnicate"));
    }

    #[test]
    fn ping_carries_schema_and_build() {
        let mut a = Agent::new();
        let r = a.handle(&Request {
            id: 1,
            op: "ping".into(),
            ..Default::default()
        });
        assert_eq!(r.ok, Some(true));
        assert_eq!(r.schema, Some(SCHEMA));
        assert_eq!(r.agent.as_deref(), Some(AGENT_ID));
        assert!(r.build.is_some());
    }

    #[test]
    fn pair_key_is_order_independent() {
        assert_eq!(pair_key("a", "b"), pair_key("b", "a"));
    }

    #[test]
    fn read_log_chunk_pages_and_flags_eof() {
        // A log longer than the chunk cap is read in cursor-advancing pieces; the
        // last (short) read flags EOF. next_cursor tracks RAW bytes, so paging is
        // byte-exact.
        let dir = std::env::temp_dir();
        let path = dir.join(format!("tdvmm-agent-logtest-{}.log", std::process::id()));
        let body: Vec<u8> = (0..25_000u32).map(|i| b'a' + (i % 26) as u8).collect();
        std::fs::write(&path, &body).unwrap();
        let p = path.to_str().unwrap();

        let cap = 10_000usize;
        let (d0, n0, eof0) = read_log_chunk(p, 0, cap).unwrap();
        assert_eq!(n0, cap);
        assert_eq!(d0.len(), cap);
        assert!(!eof0, "a full-cap read is not EOF");

        let (_d1, n1, eof1) = read_log_chunk(p, n0 as u64, cap).unwrap();
        assert_eq!(n1, cap);
        assert!(!eof1);

        let (_d2, n2, eof2) = read_log_chunk(p, (n0 + n1) as u64, cap).unwrap();
        assert_eq!(n2, 5_000);
        assert!(eof2, "a short read reached the end of the log");
        assert_eq!(n0 + n1 + n2, body.len());

        // Reading past the end returns empty + EOF (host over-read is harmless).
        let (d3, n3, eof3) = read_log_chunk(p, body.len() as u64, cap).unwrap();
        assert!(d3.is_empty() && n3 == 0 && eof3);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_log_chunk_missing_file_is_empty_eof() {
        let (d, n, eof) = read_log_chunk("/no/such/tdvmm/log/file", 0, 4096).unwrap();
        assert!(d.is_empty() && n == 0 && eof);
    }

    // ---- schema-3 event bridge -------------------------------------------------

    /// A `Read` that hands over `chunk` bytes per call — fragments frames across
    /// reads the way a real FIFO/tty does, so the reassembly is exercised, not just
    /// the happy single-read path.
    struct Drip<'a> {
        data: &'a [u8],
        pos: usize,
        chunk: usize,
    }
    impl Read for Drip<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.chunk.min(out.len()).min(self.data.len() - self.pos);
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    #[test]
    fn line_reader_reassembles_frames_across_fragmented_reads() {
        // Two atomic events + a trailing partial, delivered 3 bytes at a time.
        let data = b"{\"kind\":\"always\",\"name\":\"a\",\"ok\":true}\n\
                     {\"kind\":\"done\"}\n\
                     {\"kind\":\"sometimes\",\"name\":\"s\",\"ok\":false}"; // last: no \n
        let mut lr = LineReader::new(Drip { data, pos: 0, chunk: 3 });

        let mut lines = Vec::new();
        while lr.fill().unwrap() {
            lines.extend(lr.by_ref());
        }
        // Two complete lines; the unterminated tail is NOT surfaced (no phantom line).
        assert_eq!(lines.len(), 2);
        assert!(lr.next().is_none());
        assert_eq!(parse_event(&lines[0]).kind, "always");
        assert_eq!(parse_event(&lines[1]).kind, "done");
    }

    #[test]
    fn line_reader_bounds_an_oversized_unterminated_frame() {
        let big = vec![b'x'; RX_LINE_CAP + 10]; // no newline, ever
        let mut lr = LineReader::new(std::io::Cursor::new(big));
        let mut surfaced = 0;
        while lr.fill().unwrap() {
            while lr.next().is_some() {
                surfaced += 1; // truncated frame surfaced so memory stays bounded
            }
        }
        while lr.next().is_some() {
            surfaced += 1;
        }
        assert_eq!(surfaced, 1, "the oversized unterminated frame is surfaced once");
    }

    #[test]
    fn parse_event_passes_wellformed_and_flags_malformed() {
        let ok = parse_event(b"{\"kind\":\"always\",\"name\":\"books\",\"ok\":true}\n");
        assert_eq!(ok.kind, "always");
        assert_eq!(ok.name, "books");
        assert_eq!(ok.ok, Some(true));

        // Not JSON -> invalid, never dropped, raw preserved (truncated).
        let bad = parse_event(b"not json at all\n");
        assert_eq!(bad.kind, "invalid");
        let raw = bad.details.unwrap();
        assert_eq!(raw["raw"], "not json at all");

        // Valid JSON but no `kind` -> invalid (a kind is mandatory).
        let nokind = parse_event(b"{\"name\":\"x\",\"ok\":true}\n");
        assert_eq!(nokind.kind, "invalid");

        // An unknown kind is transport-passed as-is; the host decides policy.
        let unknown = parse_event(b"{\"kind\":\"weird\",\"name\":\"y\"}\n");
        assert_eq!(unknown.kind, "weird");
    }

    #[test]
    fn the_fifo_cannot_forge_an_agent_originated_event() {
        // The events FIFO is bound into EVERY container. A plain service must not
        // be able to end the run with a verdict of its choosing, nor fake a fault
        // mirror — those kinds are agent-originated only.
        let forged_exit = parse_event(b"{\"kind\":\"driver_exit\",\"exit\":0}\n");
        assert_eq!(forged_exit.kind, "invalid");
        assert_eq!(forged_exit.exit, None, "the forged verdict is discarded, not passed on");
        assert!(forged_exit.details.unwrap()["rejected"]
            .as_str()
            .unwrap()
            .contains("reserved"));

        let forged_ctl = parse_event(b"{\"kind\":\"ctl\",\"name\":\"kill\",\"ok\":true}\n");
        assert_eq!(forged_ctl.kind, "invalid");
    }

    #[test]
    fn forwarded_event_round_trips_as_an_id_less_reply() {
        let ev = parse_event(b"{\"kind\":\"sometimes\",\"name\":\"n\",\"ok\":true}\n");
        let line = encode_line(&Reply::from_event(7, ev)).unwrap();
        let back: Reply = decode_line(&line).unwrap();
        assert!(back.is_event(), "id-less + event set => is_event");
        assert!(!back.is_hello());
        assert_eq!(back.seq, Some(7));
        assert_eq!(back.event.as_ref().unwrap().kind, "sometimes");
    }
}
