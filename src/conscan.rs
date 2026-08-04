//! Console line scanner: the host-side reader of the guest COM1 console.
//!
//! Under `--logs-dir`, the COM1 writer ([`crate::serial::ConsoleOut`]) tees every
//! guest-console byte into a shared buffer. This scanner drains that buffer at
//! vCPU-loop boundaries, splits it into complete lines, stamps each with virtual
//! time, and demultiplexes per compose service into `<logs-dir>/<service>.log` —
//! so a *crashed* guest still leaves per-service logs (the end-of-run agent pull
//! needs a live guest and yields nothing on a crash).
//!
//! It is a PASSIVE observer: it only reads bytes the guest already emitted, on the
//! vCPU thread, touching no guest state, the clock, or the event queue — so it adds
//! zero guest wakes and is fast-forward-neutral. Every file error warns once and is
//! then skipped; it never affects the verdict, JSONL, report, or exit code.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::vtsc::TscFrequency;

/// Bound on the reassembly fragment for a line with no newline: a runaway writer
/// is flushed as a truncated line rather than grown without bound.
const PARTIAL_CAP: usize = 64 * 1024;

/// Basename (sans `.log`) for console output not attributed to a service — kernel
/// and launcher lines, sentinels, and the `[stack]`/`[hc]`/`[census]` streams.
const RESIDUE: &str = "console";

pub struct ConsoleScan {
    tee: Arc<Mutex<Vec<u8>>>,
    partial: Vec<u8>,
    dir: PathBuf,
    /// The `<project>-` container-name prefix, e.g. `tdvmm_tigerbeetle-`.
    project_prefix: String,
    services: Vec<String>,
    /// Lazily-opened per-file handles; a cached `None` marks a file we gave up on.
    files: HashMap<String, Option<File>>,
    /// Residue basename — `_console` if a service is literally named `console`.
    residue: String,
    t0: u64,
    hz: u64,
}

impl ConsoleScan {
    pub fn new(
        tee: Arc<Mutex<Vec<u8>>>,
        dir: PathBuf,
        project: &str,
        services: Vec<String>,
        t0: u64,
        freq: TscFrequency,
    ) -> Self {
        let residue = if services.iter().any(|s| s == RESIDUE) {
            format!("_{RESIDUE}")
        } else {
            RESIDUE.to_string()
        };
        Self {
            tee,
            partial: Vec::new(),
            dir,
            project_prefix: format!("{project}-"),
            services,
            files: HashMap::new(),
            residue,
            t0,
            hz: freq.hz(),
        }
    }

    /// Drain whatever the tee accumulated and write out every complete line,
    /// stamping the lines drained this call with `now_vtsc`. No-op when idle.
    pub fn drain(&mut self, now_vtsc: u64) {
        // Lock-swap the shared buffer so the guest writer path stays fast.
        let chunk = {
            let mut b = self.tee.lock().unwrap();
            if b.is_empty() {
                return;
            }
            std::mem::take(&mut *b)
        };
        self.partial.extend_from_slice(&chunk);
        self.emit_complete_lines(now_vtsc);
    }

    /// Flush a trailing fragment (a final line with no terminating newline).
    pub fn finish(&mut self, now_vtsc: u64) {
        self.drain(now_vtsc);
        if !self.partial.is_empty() {
            let line = std::mem::take(&mut self.partial);
            self.write_line(&line, now_vtsc);
        }
    }

    fn emit_complete_lines(&mut self, now_vtsc: u64) {
        loop {
            if let Some(pos) = self.partial.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.partial.drain(..=pos).collect();
                line.pop(); // '\n'
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.write_line(&line, now_vtsc);
            } else {
                // No newline yet: flush a pathologically long fragment as a
                // truncated line rather than buffer it unbounded.
                if self.partial.len() > PARTIAL_CAP {
                    let line = std::mem::take(&mut self.partial);
                    self.write_line(&line, now_vtsc);
                }
                break;
            }
        }
    }

    fn write_line(&mut self, line: &[u8], now_vtsc: u64) {
        let (stem, message) = self.route(line);
        let t_s = now_vtsc.wrapping_sub(self.t0) as f64 / self.hz as f64;
        let mut out = format!("{t_s:.3} ").into_bytes();
        out.extend_from_slice(message);
        out.push(b'\n');
        self.append(&stem, &out);
    }

    /// Decide the target file for a line and strip a matched `[service]` prefix.
    /// Returns (basename-stem, message). Unattributed lines go to the residue file
    /// verbatim.
    fn route<'a>(&self, line: &'a [u8]) -> (String, &'a [u8]) {
        if let Some(name) = bracket_prefix(line) {
            if let Some(service) = self.service_of(name) {
                let cut = name.len() + 2; // '[' + name + ']'
                let rest = line.get(cut..).unwrap_or(&[]);
                let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                return (service, rest);
            }
        }
        (self.residue.clone(), line)
    }

    /// Map a bracket name to a compose service: the container form
    /// `<project>-<service>-<n>`, or a bracket that is exactly a service name.
    fn service_of(&self, name: &[u8]) -> Option<String> {
        let name = std::str::from_utf8(name).ok()?;
        if let Some(rest) = name.strip_prefix(&self.project_prefix) {
            // rest = <service>-<n>; strip a trailing -<digits>.
            if let Some(dash) = rest.rfind('-') {
                let (svc, num) = (&rest[..dash], &rest[dash + 1..]);
                if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
                    if let Some(s) = self.services.iter().find(|s| *s == svc) {
                        return Some(s.clone());
                    }
                }
            }
        }
        // Fallback: the bracket is exactly a service name.
        self.services.iter().find(|s| *s == name).cloned()
    }

    fn append(&mut self, stem: &str, bytes: &[u8]) {
        if !self.files.contains_key(stem) {
            let opened = self.open_file(stem);
            self.files.insert(stem.to_string(), opened);
        }
        if let Some(Some(f)) = self.files.get_mut(stem) {
            if let Err(e) = f.write_all(bytes) {
                crate::log_line(format_args!(
                    "[tdvmm][WARN] --logs-dir: writing {stem}.log: {e} — dropping this file"
                ));
                self.files.insert(stem.to_string(), None);
            }
        }
    }

    fn open_file(&self, stem: &str) -> Option<File> {
        let fname = match crate::sanitize_service_filename(stem) {
            Some(f) => f,
            None => {
                crate::log_line(format_args!(
                    "[tdvmm][WARN] --logs-dir: unsafe service name {stem:?} — skipping its log"
                ));
                return None;
            }
        };
        let path = self.dir.join(format!("{fname}.log"));
        // Append mode, not `File::create`'s positional writes: every write lands at
        // the current EOF, so if the end-of-run agent pull truncates+rewrites this
        // path, a post-capture flush through this cached handle appends after the
        // agent's copy instead of corrupting it at a now-stale offset. Rust rejects
        // O_TRUNC|O_APPEND together, so clear a stale prior-run file via set_len(0)
        // on this first (and only) open per service.
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => {
                let _ = f.set_len(0);
                Some(f)
            }
            Err(e) => {
                crate::log_line(format_args!(
                    "[tdvmm][WARN] --logs-dir: creating {}: {e} — skipping",
                    path.display()
                ));
                None
            }
        }
    }
}

/// If `line` starts with `[name]`, return `name` (the bytes between the brackets).
fn bracket_prefix(line: &[u8]) -> Option<&[u8]> {
    if line.first() != Some(&b'[') {
        return None;
    }
    let close = line.iter().position(|&b| b == b']')?;
    Some(&line[1..close])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ghz() -> TscFrequency {
        TscFrequency::from_hz(1_000_000_000)
    }

    fn test_dir(name: &str) -> PathBuf {
        let d = PathBuf::from("target/test-conscan").join(name);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn scan(dir: PathBuf, services: &[&str]) -> ConsoleScan {
        ConsoleScan::new(
            Arc::new(Mutex::new(Vec::new())),
            dir,
            "tdvmm_tigerbeetle",
            services.iter().map(|s| s.to_string()).collect(),
            0,
            ghz(),
        )
    }

    #[test]
    fn maps_container_names_to_services() {
        let sc = scan(test_dir("_map"), &["replica0", "replica1", "replica2", "client"]);
        assert_eq!(sc.service_of(b"tdvmm_tigerbeetle-replica2-1").as_deref(), Some("replica2"));
        assert_eq!(sc.service_of(b"tdvmm_tigerbeetle-client-1").as_deref(), Some("client"));
        // A bracket that is exactly a service name (fallback path).
        assert_eq!(sc.service_of(b"replica0").as_deref(), Some("replica0"));
        // Non-service brackets and a wrong project resolve to nothing (residue).
        assert_eq!(sc.service_of(b"stack"), None);
        assert_eq!(sc.service_of(b"tdvmm_other-replica0-1"), None);
    }

    #[test]
    fn reassembles_a_line_split_across_drains() {
        let dir = test_dir("reassemble");
        let mut sc = scan(dir.clone(), &["replica0"]);
        let tee = sc.tee.clone();
        tee.lock().unwrap().extend_from_slice(b"[tdvmm_tigerbeetle-replica0-1] hel");
        sc.drain(0); // no newline yet → buffered
        tee.lock().unwrap().extend_from_slice(b"lo world\n");
        sc.drain(1_000_000_000); // +1s of virtual time completes the line
        let out = std::fs::read_to_string(dir.join("replica0.log")).unwrap();
        assert_eq!(out, "1.000 hello world\n");
    }

    #[test]
    fn demuxes_services_and_routes_the_rest_to_console() {
        let dir = test_dir("demux");
        let mut sc = scan(dir.clone(), &["replica0", "client"]);
        let tee = sc.tee.clone();
        tee.lock().unwrap().extend_from_slice(
            b"[tdvmm_tigerbeetle-replica0-1] up\n[stack] booting\nplain kernel line\n\
              [tdvmm_tigerbeetle-client-1] tx\n",
        );
        sc.drain(0);
        sc.finish(0);
        assert_eq!(std::fs::read_to_string(dir.join("replica0.log")).unwrap(), "0.000 up\n");
        assert_eq!(std::fs::read_to_string(dir.join("client.log")).unwrap(), "0.000 tx\n");
        let console = std::fs::read_to_string(dir.join("console.log")).unwrap();
        assert!(console.contains("[stack] booting"), "residue keeps unmatched brackets verbatim");
        assert!(console.contains("plain kernel line"), "residue keeps unprefixed lines");
    }

    #[test]
    fn flushes_an_overlong_partial_as_a_truncated_line() {
        let dir = test_dir("cap");
        let mut sc = scan(dir.clone(), &[]);
        let tee = sc.tee.clone();
        tee.lock().unwrap().extend(std::iter::repeat(b'a').take(PARTIAL_CAP + 10)); // no newline
        sc.drain(0);
        assert!(sc.partial.is_empty(), "overlong fragment must be flushed, not buffered");
        let console = std::fs::read_to_string(dir.join("console.log")).unwrap();
        assert!(console.len() > PARTIAL_CAP);
    }

    #[test]
    fn residue_avoids_colliding_with_a_service_named_console() {
        let sc = scan(test_dir("_residue"), &["console"]);
        assert_eq!(sc.residue, "_console");
    }

    #[test]
    fn appends_after_an_external_rewrite_instead_of_corrupting_it() {
        // Mirrors the end-of-run agent pull truncating+rewriting a service file
        // mid-run: the scanner's cached handle must land at the current EOF, never
        // at a stale positional offset (which would leave a NUL hole / mid-file
        // overwrite in the fresh copy).
        let dir = test_dir("append");
        let mut sc = scan(dir.clone(), &["replica0"]);
        let tee = sc.tee.clone();
        tee.lock().unwrap().extend_from_slice(b"[tdvmm_tigerbeetle-replica0-1] live1\n");
        sc.drain(0); // opens replica0.log, writes through the cached handle
        let path = dir.join("replica0.log");
        // An external rewrite through a different handle (the agent copy).
        std::fs::write(&path, b"AGENT COPY\n").unwrap();
        // A line teed after the rewrite must append after it, not clobber it.
        tee.lock().unwrap().extend_from_slice(b"[tdvmm_tigerbeetle-replica0-1] live2\n");
        sc.drain(0);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "AGENT COPY\n0.000 live2\n");
    }
}
