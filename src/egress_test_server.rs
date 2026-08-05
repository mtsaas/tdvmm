//! `tdvmm __egress-test-server` — a controlled loopback endpoint for the egress
//! safety suite. It is the "external" world in CI doctrine: a TCP server bound to
//! `127.0.0.1` (never a routable interface, so NO internet is ever involved) that
//! the host-side [`crate::egress`] backend connects to on the guest's behalf.
//!
//! It binds an EPHEMERAL port (so parallel test runs never collide), prints the
//! chosen port as a single `EGRESS_TEST_SERVER_PORT=<n>` line, and then serves each
//! connection with one of a few scripted, timing-precise behaviors — the knobs the
//! safety tests turn to prove the phase gate:
//!
//! * `delay-then-respond <secs>` — read the request, hold `secs` of REAL time with
//!   the request in flight, then send a fixed 200 response and close. Drives T1
//!   (the clock must not jump while a request is in flight) and T5 (transport
//!   under `--ff off`).
//! * `dribble <bytes> <interval_ms>` — send the response header, then drip the
//!   body one byte per `interval_ms` of REAL time. Drives T3c (the idle gaps
//!   between drips must not be fast-forwarded past the guest's read timeout).
//! * `hold-open <secs>` — read the request and hold the socket open `secs` of real
//!   time WITHOUT responding, keeping a host session established. Drives T3b (the
//!   negative control: a jump attempted against an open session must abort).
//!
//! Std only, one thread per accepted connection; it serves forever until the test
//! harness kills it.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

/// The fixed response body `delay-then-respond`/`dribble` return, so the guest
/// probe can assert an exact payload came back through the mux.
const OK_BODY: &[u8] = b"EGRESSOK\n";

/// One scripted endpoint behavior, parsed from the subcommand args. Plain scalars,
/// so it is `Copy` — each accepted connection takes its own copy.
#[derive(Clone, Copy)]
enum Behavior {
    /// Hold `delay` real time with the request in flight, then respond + close.
    DelayThenRespond { delay: Duration },
    /// Drip a `bytes`-long body one byte per `interval`, then close.
    Dribble { bytes: usize, interval: Duration },
    /// Hold the socket open `hold` real time without responding, then close.
    HoldOpen { hold: Duration },
}

impl Behavior {
    /// Parse `behavior` + its positional `args`. Returns a usage string on any
    /// malformed input.
    fn parse(behavior: &str, args: &[String]) -> Result<Behavior, String> {
        let usage = || {
            "usage: __egress-test-server (delay-then-respond <secs> | \
             dribble <bytes> <interval_ms> | hold-open <secs>)"
                .to_string()
        };
        let f = |i: usize| args.get(i).and_then(|s| s.parse::<f64>().ok());
        let u = |i: usize| args.get(i).and_then(|s| s.parse::<u64>().ok());
        match behavior {
            "delay-then-respond" => Ok(Behavior::DelayThenRespond {
                delay: secs(f(0).ok_or_else(usage)?),
            }),
            "dribble" => Ok(Behavior::Dribble {
                bytes: u(0).ok_or_else(usage)? as usize,
                interval: Duration::from_millis(u(1).ok_or_else(usage)?),
            }),
            "hold-open" => Ok(Behavior::HoldOpen { hold: secs(f(0).ok_or_else(usage)?) }),
            _ => Err(usage()),
        }
    }
}

/// Seconds (fractional) to a `Duration`.
fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s.max(0.0))
}

/// Run the server: bind an ephemeral loopback port, announce it, and serve forever.
/// Returns a nonzero exit code only on a setup failure.
pub(crate) fn run(behavior: &str, args: &[String]) -> i32 {
    let behavior = match Behavior::parse(behavior, args) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };

    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => l,
        Err(e) => {
            eprintln!("egress-test-server: bind failed: {e}");
            return 1;
        }
    };
    let port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => {
            eprintln!("egress-test-server: local_addr failed: {e}");
            return 1;
        }
    };
    // The one contract line the harness greps for. Flush so it is readable
    // immediately (the harness blocks on it before launching the guest).
    println!("EGRESS_TEST_SERVER_PORT={port}");
    let _ = io::stdout().flush();

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || serve(stream, behavior));
            }
            Err(e) => eprintln!("egress-test-server: accept failed: {e}"),
        }
    }
    0
}

/// Serve one connection per the behavior. All I/O is best-effort: the peer is the
/// host egress backend, which may reset at any point.
fn serve(mut stream: TcpStream, behavior: Behavior) {
    // Read (and discard) the request headers so the client's write completes
    // before we act. Bounded and time-limited so a client that sends nothing
    // cannot wedge the server.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    drain_request(&mut stream);

    match behavior {
        Behavior::DelayThenRespond { delay } => {
            thread::sleep(delay);
            let _ = write_response(&mut stream, OK_BODY);
        }
        Behavior::Dribble { bytes, interval } => {
            let header = http_header(bytes);
            if stream.write_all(header.as_bytes()).is_ok() {
                let _ = stream.flush();
                for _ in 0..bytes {
                    thread::sleep(interval);
                    if stream.write_all(b"x").is_err() {
                        break;
                    }
                    let _ = stream.flush();
                }
            }
        }
        Behavior::HoldOpen { hold } => {
            thread::sleep(hold);
        }
    }
    // Explicit close (drop) sends FIN, so the host reads EOF and the stream
    // reaches the closed-both-sides state the quiescence predicate needs.
}

/// Read the client's request until the header terminator, EOF, a small cap, or the
/// read timeout — whichever comes first. The content is irrelevant; we only need
/// the client's write to have happened.
fn drain_request(stream: &mut TcpStream) {
    let mut buf = [0u8; 2048];
    let mut total = 0usize;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") || total >= 64 * 1024 {
                    break;
                }
            }
            Err(_) => break, // timeout / reset: proceed with the behavior anyway.
        }
    }
}

/// The response header for a `len`-byte body: HTTP/1.0, explicit length, and an
/// explicit `Connection: close` so the client (and the host, on EOF) tears the
/// session fully down.
fn http_header(len: usize) -> String {
    format!("HTTP/1.0 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
}

/// Write a complete response (header + `body`) and flush.
fn write_response(stream: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    stream.write_all(http_header(body.len()).as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_behavior() {
        assert!(matches!(
            Behavior::parse("delay-then-respond", &["2".into()]),
            Ok(Behavior::DelayThenRespond { .. })
        ));
        assert!(matches!(
            Behavior::parse("dribble", &["6".into(), "500".into()]),
            Ok(Behavior::Dribble { bytes: 6, .. })
        ));
        assert!(matches!(
            Behavior::parse("hold-open", &["10".into()]),
            Ok(Behavior::HoldOpen { .. })
        ));
    }

    #[test]
    fn rejects_unknown_or_malformed() {
        assert!(Behavior::parse("nope", &[]).is_err());
        assert!(Behavior::parse("dribble", &["notanumber".into(), "1".into()]).is_err());
        assert!(Behavior::parse("delay-then-respond", &[]).is_err());
    }

    #[test]
    fn header_declares_length_and_close() {
        let h = http_header(9);
        assert!(h.contains("Content-Length: 9"));
        assert!(h.contains("Connection: close"));
    }
}
