//! Driver-container exit watching: the run's verdict signal.
//!
//! In driver mode ONE compose service is the test driver (marked
//! [`tdvmm_proto::DRIVER_MARKER`] at bake time). Its container's exit status IS
//! the verdict — there is no host-side timeline and no assertion ledger — so the
//! agent must notice that exit and report it up ttyS1 as a `driver_exit`
//! [`tdvmm_proto::GuestEvent`].
//!
//! ## Why a child process and not a poll timeout
//!
//! The agent's whole fast-forward transparency rests on its `poll` having an
//! INFINITE timeout: a blocked read arms no timer, so an idle guest still jumps
//! (see [`crate::bridge::run_loop`]). Polling podman from the agent loop would
//! need a timeout and would end fast-forward for every run. Instead the wait is
//! delegated to a child (`podman wait`, the same verb [`crate::agent`] already
//! uses to make a kill deterministic) whose stdout is just another fd in the
//! agent's blocked poll set — no timer in the agent, and the child's exit-code
//! line is the readiness edge.
//!
//! The child does its own bounded polling in two places, both priced:
//!
//! * before the driver container exists (compose has not created it yet) it
//!   re-lists every [`DISCOVER_POLL_S`] seconds. This window is guest-busy
//!   anyway (compose-up is creating containers), so it costs nothing real.
//! * `podman wait --interval` defaults to 250 ms, which WOULD arm a guest timer
//!   four times a virtual second and dominate fast-forward, so it is pinned to
//!   [`WAIT_INTERVAL_S`]. The cost is one cheap FF hop every 10 virtual seconds
//!   (~8.6k hops per virtual day, well under a second of wall time), and the
//!   only latency it adds is on a run that is ENDING anyway.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::process::{Child, ChildStdout, Command, Stdio};

use tdvmm_proto::GuestEvent;

/// The compose label podman sets on each service's container(s) (mirrors
/// [`crate::agent`]'s constant; kept local so this module is self-contained).
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

/// Seconds between container-discovery attempts before the driver exists.
const DISCOVER_POLL_S: u32 = 2;

/// `podman wait --interval` (seconds). See the module doc for why the 250 ms
/// default is unacceptable here.
const WAIT_INTERVAL_S: u32 = 10;

/// Cap on the bytes read from the watcher child, so a misbehaving child cannot
/// grow the buffer without bound. An exit code line is a handful of bytes.
const OUT_CAP: usize = 4096;

/// A service name safe to interpolate into the watcher's shell script. The name
/// comes from the host (kernel cmdline), but a shell-quoting bug here would be a
/// command injection into a root guest process, so it is checked rather than
/// trusted: compose service names are `[A-Za-z0-9_.-]+` anyway.
fn is_safe_service(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// The watcher child + its stdout, held so the agent can poll the fd.
pub(crate) struct DriverWatch {
    service: String,
    child: Child,
    out: ChildStdout,
    buf: Vec<u8>,
}

impl DriverWatch {
    /// Spawn the watcher for `service`.
    ///
    /// # Errors
    ///
    /// [`io::ErrorKind::InvalidInput`] if the service name is not shell-safe;
    /// otherwise the spawn failure.
    pub(crate) fn spawn(service: &str) -> io::Result<DriverWatch> {
        if !is_safe_service(service) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsafe driver service name {service:?}"),
            ));
        }
        // Wait for the container to exist, then wait for it to stop and print its
        // exit code. `podman wait` on an ALREADY-exited container returns that
        // container's code immediately, so a driver that finishes before we get
        // here is not missed.
        let script = format!(
            "id=''; \
             while [ -z \"$id\" ]; do \
             id=$(podman ps -a --filter 'label={label}={service}' --format '{{{{.ID}}}}' 2>/dev/null | head -n1); \
             [ -n \"$id\" ] || sleep {discover}; \
             done; \
             exec podman wait --interval {interval}s \"$id\"",
            label = COMPOSE_SERVICE_LABEL,
            discover = DISCOVER_POLL_S,
            interval = WAIT_INTERVAL_S,
        );
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let out = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("driver watcher child has no stdout"))?;
        Ok(DriverWatch { service: service.to_string(), child, out, buf: Vec::new() })
    }

    /// The fd to place in the agent's poll set.
    pub(crate) fn fd(&self) -> RawFd {
        self.out.as_raw_fd()
    }

    /// The watcher's stdout became readable. Performs EXACTLY ONE read — `poll`
    /// promised this one will not block, a second one makes no such promise —
    /// and returns `Some(event)` once the pipe reaches EOF, meaning the child is
    /// done and the driver container has exited.
    pub(crate) fn on_readable(&mut self) -> Option<GuestEvent> {
        let mut tmp = [0u8; 256];
        match self.out.read(&mut tmp) {
            Ok(0) => Some(self.finish()), // EOF: the child is done.
            Ok(n) => {
                if self.buf.len() < OUT_CAP {
                    self.buf.extend_from_slice(&tmp[..n]);
                }
                None
            }
            // A signal interrupted the read; the next poll wake retries it.
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => None,
            Err(_) => Some(self.finish()),
        }
    }

    /// Reap the child and turn what it printed into the terminal event. A parsed
    /// integer is the driver's exit status; anything else means the wait itself
    /// failed, which the host grades as an infrastructure error rather than a
    /// test failure — never as a silent pass.
    fn finish(&mut self) -> GuestEvent {
        let _ = self.child.wait();
        let text = String::from_utf8_lossy(&self.buf);
        let code = text.lines().next().and_then(|l| l.trim().parse::<i64>().ok());
        match code {
            Some(exit) => GuestEvent {
                kind: "driver_exit".into(),
                name: self.service.clone(),
                exit: Some(exit),
                ..Default::default()
            },
            None => GuestEvent {
                kind: "driver_exit".into(),
                name: self.service.clone(),
                exit: None,
                details: Some(serde_json::json!({
                    "error": "could not read the driver container's exit status",
                    "raw": text.chars().take(200).collect::<String>(),
                })),
                ..Default::default()
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_service_names() {
        assert!(is_safe_service("pg-primary"));
        assert!(is_safe_service("driver_1.a"));
        assert!(!is_safe_service(""));
        // Anything that could break out of the single-quoted shell filter.
        for bad in ["a'; rm -rf /; #", "a b", "a$(id)", "a`id`", "a\"b", "a\nb"] {
            assert!(!is_safe_service(bad), "{bad:?} must be rejected");
        }
    }
}
