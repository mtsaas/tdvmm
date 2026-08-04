//! The agent's request dispatch and container ops: ping/exec/containers/lifecycle/
//! partition+heal/logs over podman + nft. Pure request -> reply; the transport is in
//! `bridge`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use dvmm_proto::{ContainerInfo, ErrorKind, Reply, Request, MAX_LOGS_CHUNK_BYTES, SCHEMA};
use serde::Deserialize;

use crate::{AGENT_ID, BUILD};

/// The compose label podman/compose sets on each service's container(s).
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

// ============================================================================
// Agent state + dispatch
// ============================================================================

pub(crate) struct Agent {
    /// Canonical unordered service-pair key -> the two container IPs to drop
    /// between. The whole nft ruleset is rebuilt from this on every change, so the
    /// installed rules are always exactly the active set. BTreeMap => sorted keys
    /// (deterministic ruleset, matching the Go agent's `sort.Strings`).
    partitions: BTreeMap<String, [String; 2]>,
}

impl Agent {
    pub(crate) fn new() -> Self {
        Agent { partitions: BTreeMap::new() }
    }

    pub(crate) fn handle(&mut self, req: &Request) -> Reply {
        match req.op.as_str() {
            "ping" => Reply {
                id: Some(req.id),
                ok: Some(true),
                op: Some("ping".into()),
                agent: Some(AGENT_ID.into()),
                schema: Some(SCHEMA),
                build: Some(BUILD.into()),
                ..Default::default()
            },
            "containers" => self.do_containers(req),
            "exec" => self.do_exec(req),
            "kill" => self.do_lifecycle(req, "kill"),
            "stop" => self.do_lifecycle(req, "stop"),
            "start" => self.do_start(req),
            "partition" => self.do_partition(req),
            "heal" => self.do_heal(req),
            "logs" => self.do_logs(req),
            other => Reply {
                id: Some(req.id),
                ok: Some(false),
                op: Some(other.into()),
                error: Some(ErrorKind::UnknownOp.msg(other)),
                ..Default::default()
            },
        }
    }

    fn do_containers(&self, req: &Request) -> Reply {
        match list_containers() {
            Ok(list) => Reply {
                id: Some(req.id),
                ok: Some(true),
                op: Some("containers".into()),
                containers: Some(list),
                ..Default::default()
            },
            Err(e) => err(req.id, "containers", ErrorKind::PodmanPs.msg(e)),
        }
    }

    fn do_exec(&self, req: &Request) -> Reply {
        let container = req.container.clone().unwrap_or_default();
        let cmd = req.cmd.clone().unwrap_or_default();
        if container.is_empty() || cmd.is_empty() {
            return err(req.id, "exec", "exec requires `container` and `cmd`".into());
        }
        let id = match resolve_running(&container) {
            Ok(Some((id, _))) => id,
            Ok(None) => {
                // No running container for this service — the VMM treats this as
                // retryable inside wait_for, or an infra error for a hard exec.
                return err(
                    req.id,
                    "exec",
                    ErrorKind::NoContainer.msg(format!("no running container for service {container}")),
                );
            }
            Err(e) => return err(req.id, "exec", ErrorKind::PodmanPs.msg(e)),
        };

        let start = Instant::now();
        let out = Command::new("podman")
            .arg("exec")
            .arg(&id)
            .args(&cmd)
            .output();
        let dur = start.elapsed().as_millis() as u64;
        match out {
            Ok(o) => Reply {
                id: Some(req.id),
                ok: Some(true),
                op: Some("exec".into()),
                exit: Some(o.status.code().unwrap_or(-1) as i64),
                stdout: nonempty(o.stdout),
                stderr: nonempty(o.stderr),
                dur_ms: Some(dur),
                ..Default::default()
            },
            // podman itself could not run (not the command's exit) — infra.
            Err(e) => Reply {
                id: Some(req.id),
                ok: Some(false),
                op: Some("exec".into()),
                error: Some(ErrorKind::PodmanExec.msg(e.to_string())),
                dur_ms: Some(dur),
                ..Default::default()
            },
        }
    }

    /// `kill` / `stop`: resolve the RUNNING container, run the podman verb, then
    /// WAIT for it to actually stop so a following census is deterministic.
    fn do_lifecycle(&self, req: &Request, op: &str) -> Reply {
        let container = req.container.clone().unwrap_or_default();
        if container.is_empty() {
            return err(req.id, op, format!("{op} requires `container`"));
        }
        let (id, name) = match resolve_by_service(&container, true) {
            Ok(Some(v)) => v,
            Ok(None) => {
                return err(
                    req.id,
                    op,
                    ErrorKind::NoContainer.msg(format!("no running container for service {container}")),
                )
            }
            Err(e) => return err(req.id, op, ErrorKind::PodmanPs.msg(e)),
        };
        let (_so, se, run) = run_podman(req.timeout_s.unwrap_or(0), &[op, &id]);
        if let Err(e) = run {
            return err(req.id, op, format!("podman_{op}: {e} {}", se.trim()));
        }
        // Block until the container has actually stopped (kill does not wait; stop
        // does, but this is idempotent + cheap and makes the census deterministic).
        let _ = run_podman(req.timeout_s.unwrap_or(0), &["wait", &id]);
        ok_stdout(req.id, op, format!("{op} {container} ({name})"))
    }

    /// Restart a previously stopped/killed container of the service.
    fn do_start(&self, req: &Request) -> Reply {
        let container = req.container.clone().unwrap_or_default();
        if container.is_empty() {
            return err(req.id, "start", "start requires `container`".into());
        }
        let (id, name) = match resolve_by_service(&container, false) {
            Ok(Some(v)) => v,
            Ok(None) => {
                // Already running? Treat as an idempotent success.
                if let Ok(Some(_)) = resolve_by_service(&container, true) {
                    return ok_stdout(req.id, "start", format!("already running {container}"));
                }
                return err(
                    req.id,
                    "start",
                    ErrorKind::NoContainer.msg(format!("no stopped container for service {container}")),
                );
            }
            Err(e) => return err(req.id, "start", ErrorKind::PodmanPs.msg(e)),
        };
        let (_so, se, run) = run_podman(req.timeout_s.unwrap_or(0), &["start", &id]);
        if let Err(e) = run {
            return err(req.id, "start", format!("podman_start: {e} {}", se.trim()));
        }
        ok_stdout(req.id, "start", format!("start {container} ({name})"))
    }

    /// Drop all traffic between the two services' running containers.
    fn do_partition(&mut self, req: &Request) -> Reply {
        let a = req.container.clone().unwrap_or_default();
        let b = req.peer.clone().unwrap_or_default();
        if a.is_empty() || b.is_empty() {
            return err(
                req.id,
                "partition",
                "partition requires two services (`container` and `peer`)".into(),
            );
        }
        let aid = match resolve_by_service(&a, true) {
            Ok(v) => v,
            Err(e) => return err(req.id, "partition", ErrorKind::PodmanPs.msg(e)),
        };
        let bid = match resolve_by_service(&b, true) {
            Ok(v) => v,
            Err(e) => return err(req.id, "partition", ErrorKind::PodmanPs.msg(e)),
        };
        let (aid, bid) = match (aid, bid) {
            (Some((aid, _)), Some((bid, _))) => (aid, bid),
            (aopt, bopt) => {
                return err(
                    req.id,
                    "partition",
                    ErrorKind::NoContainer.msg(format!(
                        "need both running ({a} running={}, {b} running={})",
                        aopt.is_some(),
                        bopt.is_some()
                    )),
                )
            }
        };
        let aip = match container_ip(&aid) {
            Ok(ip) => ip,
            Err(e) => return err(req.id, "partition", format!("ip({a}): {e}")),
        };
        let bip = match container_ip(&bid) {
            Ok(ip) => ip,
            Err(e) => return err(req.id, "partition", format!("ip({b}): {e}")),
        };
        enable_bridge_netfilter();
        let key = pair_key(&a, &b);
        self.partitions.insert(key.clone(), [aip.clone(), bip.clone()]);
        if let Err(e) = self.apply_partitions() {
            self.partitions.remove(&key);
            return err(req.id, "partition", ErrorKind::Nft.msg(e));
        }
        ok_stdout(
            req.id,
            "partition",
            format!("partition {a}({aip}) <-x-> {b}({bip})"),
        )
    }

    /// Remove the partition between two services, or ALL partitions when no
    /// services are given (`heal` / `heal: all`).
    fn do_heal(&mut self, req: &Request) -> Reply {
        let a = req.container.clone().unwrap_or_default();
        let b = req.peer.clone().unwrap_or_default();
        match (a.is_empty(), b.is_empty()) {
            (true, true) => self.partitions.clear(),
            (false, false) => {
                self.partitions.remove(&pair_key(&a, &b));
            }
            _ => {
                return err(
                    req.id,
                    "heal",
                    "heal needs two services (`container` and `peer`) or none (heal all)".into(),
                )
            }
        }
        if let Err(e) = self.apply_partitions() {
            return err(req.id, "heal", ErrorKind::Nft.msg(e));
        }
        let detail = if !a.is_empty() {
            format!("heal {a} <-> {b}")
        } else {
            "heal all".to_string()
        };
        ok_stdout(req.id, "heal", detail)
    }

    /// Fetch one BOUNDED chunk of a service's container log. The compose k8s-file
    /// backend gives every container a plain log file (path via `podman inspect
    /// {{.LogPath}}`); we seek to `cursor`, read up to `min(max_bytes, cap)` raw
    /// bytes, and return them with the advanced cursor + an EOF flag. The host
    /// pages a whole log by looping from cursor 0 to `eof`. A single read, then
    /// the agent blocks again — NEVER a follow/tail (which would defeat the host's
    /// fast-forward).
    fn do_logs(&self, req: &Request) -> Reply {
        let service = req.container.clone().unwrap_or_default();
        if service.is_empty() {
            return err(req.id, "logs", "logs requires `container`".into());
        }
        let cursor = req.cursor.unwrap_or(0);
        // Hard-cap the read at MAX_LOGS_CHUNK_BYTES regardless of a larger request,
        // so a reply's JSON-escaped `data` can never approach the host TX_BUF_CAP.
        let cap = req
            .max_bytes
            .unwrap_or(MAX_LOGS_CHUNK_BYTES)
            .min(MAX_LOGS_CHUNK_BYTES) as usize;

        // Resolve service -> a container id. Prefer a running one, but fall back to
        // a stopped/exited container so a killed service's logs are still pullable.
        let id = match resolve_by_service(&service, true) {
            Ok(Some((id, _))) => id,
            Ok(None) => match resolve_by_service(&service, false) {
                Ok(Some((id, _))) => id,
                Ok(None) => {
                    return err(
                        req.id,
                        "logs",
                        ErrorKind::NoContainer.msg(format!("no container for service {service}")),
                    )
                }
                Err(e) => return err(req.id, "logs", ErrorKind::PodmanPs.msg(e)),
            },
            Err(e) => return err(req.id, "logs", ErrorKind::PodmanPs.msg(e)),
        };

        let log_path = match container_log_path(&id) {
            Ok(p) => p,
            Err(e) => return err(req.id, "logs", ErrorKind::PodmanPs.msg(e)),
        };

        match read_log_chunk(&log_path, cursor, cap) {
            Ok((data, n, eof)) => Reply {
                id: Some(req.id),
                ok: Some(true),
                op: Some("logs".into()),
                data: if data.is_empty() { None } else { Some(data) },
                next_cursor: Some(cursor.saturating_add(n as u64)),
                eof: Some(eof),
                ..Default::default()
            },
            Err(e) => err(
                req.id,
                "logs",
                ErrorKind::PodmanOp.msg(format!("read log {log_path}: {e}")),
            ),
        }
    }

    /// Rebuild our nft table from `partitions` in ONE atomic transaction. The
    /// add/delete/add idiom gives a clean slate whether or not the table pre-
    /// existed. The chain is a default-ACCEPT forward base chain, so only our
    /// explicit drop rules matter and all other traffic falls through to netavark.
    fn apply_partitions(&self) -> Result<(), String> {
        let mut b = String::new();
        b.push_str("add table inet dvmm_faults\n");
        b.push_str("delete table inet dvmm_faults\n");
        b.push_str("add table inet dvmm_faults\n");
        b.push_str("add chain inet dvmm_faults partition { type filter hook forward priority -300 ; policy accept ; }\n");
        for ips in self.partitions.values() {
            b.push_str(&format!(
                "add rule inet dvmm_faults partition ip saddr {} ip daddr {} drop\n",
                ips[0], ips[1]
            ));
            b.push_str(&format!(
                "add rule inet dvmm_faults partition ip saddr {} ip daddr {} drop\n",
                ips[1], ips[0]
            ));
        }
        let mut child = Command::new("nft")
            .arg("-f")
            .arg("-")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b.as_bytes());
        }
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "exit status {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(())
    }
}

// ============================================================================
// podman helpers
// ============================================================================

/// A subset of `podman ps --format json` (only the fields we use).
#[derive(Deserialize, Default)]
struct PodmanPs {
    #[serde(rename = "Id", default)]
    id: String,
    #[serde(rename = "Names", default)]
    names: Vec<String>,
    #[serde(rename = "State", default)]
    state: String,
    #[serde(rename = "ExitCode", default)]
    exit_code: i64,
    #[serde(rename = "Labels", default)]
    labels: BTreeMap<String, String>,
    #[serde(rename = "Status", default)]
    status: String,
}

/// `podman <args>` capturing stdout; Err on spawn failure or nonzero exit.
fn podman_json(args: &[&str]) -> Result<Vec<PodmanPs>, String> {
    let out = Command::new("podman")
        .args(args)
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())
}

/// `podman ps -a --format json`, normalized to the wire census shape.
fn list_containers() -> Result<Vec<ContainerInfo>, String> {
    let raw = podman_json(&["ps", "-a", "--format", "json"])?;
    let mut list = Vec::with_capacity(raw.len());
    for c in &raw {
        let name = c.names.first().cloned().unwrap_or_default();
        let svc = c.labels.get(COMPOSE_SERVICE_LABEL).cloned().unwrap_or_default();
        let s = c.status.to_lowercase();
        let health = if s.contains("unhealthy") {
            "unhealthy"
        } else if s.contains("healthy") {
            "healthy"
        } else if s.contains("starting") {
            "starting"
        } else {
            ""
        };
        list.push(ContainerInfo {
            name,
            service: svc,
            state: c.state.to_lowercase(),
            exit_code: c.exit_code,
            health: health.to_string(),
        });
    }
    Ok(list)
}

/// Find a RUNNING container id + name for the given compose service (label, else
/// a name match for single-name stacks). `Ok(None)` = not found (retryable).
fn resolve_running(service: &str) -> Result<Option<(String, String)>, String> {
    let raw = podman_json(&["ps", "--format", "json"])?;
    for c in &raw {
        if c.state != "running" {
            continue;
        }
        if c.labels.get(COMPOSE_SERVICE_LABEL).map(|v| v == service) == Some(true) {
            return Ok(Some((c.id.clone(), c.names.first().cloned().unwrap_or_default())));
        }
        for n in &c.names {
            if n == service {
                return Ok(Some((c.id.clone(), n.clone())));
            }
        }
    }
    Ok(None)
}

/// Find a container of the service: the first RUNNING one if `want_running`, else
/// the first NON-running one (for `start`). Matches label or name.
fn resolve_by_service(service: &str, want_running: bool) -> Result<Option<(String, String)>, String> {
    let raw = podman_json(&["ps", "-a", "--format", "json"])?;
    for c in &raw {
        let mut matched = c.labels.get(COMPOSE_SERVICE_LABEL).map(|v| v == service) == Some(true);
        if !matched {
            matched = c.names.iter().any(|n| n == service);
        }
        if !matched {
            continue;
        }
        if (c.state == "running") == want_running {
            return Ok(Some((c.id.clone(), c.names.first().cloned().unwrap_or_default())));
        }
    }
    Ok(None)
}

/// The first bridge IP of a container.
fn container_ip(id_or_name: &str) -> Result<String, String> {
    let out = Command::new("podman")
        .args([
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
            id_or_name,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    for tok in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if !tok.is_empty() {
            return Ok(tok.to_string());
        }
    }
    Err(format!("no IP for {id_or_name}"))
}

/// The container's k8s-file log path. Podman (unlike Docker) has no top-level
/// `.LogPath`; the k8s-file backend records the file under
/// `.HostConfig.LogConfig.Path`. With the guest's `log_driver = "k8s-file"` this
/// is a plain file whose lines are `<RFC3339-ts> <stdout|stderr> <F|P> <message>`.
fn container_log_path(id_or_name: &str) -> Result<String, String> {
    let out = Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{.HostConfig.LogConfig.Path}}",
            id_or_name,
        ])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        return Err(format!("empty LogPath for {id_or_name}"));
    }
    Ok(p)
}

/// Read up to `cap` raw bytes from `path` starting at `cursor`. Returns
/// `(lossy_utf8_data, raw_bytes_read, eof)`. A short read (fewer than `cap`
/// bytes) means the current end of the log was reached (`eof = true`). A missing
/// log file (a container that has not logged yet) reads as an empty log at EOF —
/// not an error. `next_cursor` is derived from `raw_bytes_read`, so paging stays
/// byte-exact even though `data` is lossy UTF-8.
pub(crate) fn read_log_chunk(path: &str, cursor: u64, cap: usize) -> std::io::Result<(String, usize, bool)> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((String::new(), 0, true));
        }
        Err(e) => return Err(e),
    };
    f.seek(SeekFrom::Start(cursor))?;
    let mut buf = vec![0u8; cap];
    let mut total = 0usize;
    while total < cap {
        let n = f.read(&mut buf[total..])?;
        if n == 0 {
            break;
        }
        total += n;
    }
    let eof = total < cap; // short read => reached the current end of the log.
    let data = String::from_utf8_lossy(&buf[..total]).into_owned();
    Ok((data, total, eof))
}

/// Make bridged (intra-network) packets traverse the ip/inet netfilter hooks, so
/// our forward drop rules see container-to-container traffic. netavark already
/// sets this; set it defensively and ignore errors.
fn enable_bridge_netfilter() {
    for p in [
        "/proc/sys/net/bridge/bridge-nf-call-iptables",
        "/proc/sys/net/bridge/bridge-nf-call-ip6tables",
    ] {
        let _ = std::fs::write(p, "1\n");
    }
}

/// Run `podman <args>` under a timeout (guest seconds; fast-forwards), capturing
/// stdout/stderr. Returns `(stdout, stderr, Ok/Err)`. std-only timeout: poll
/// `try_wait`, `child.kill()` on the deadline (no libc, no raw signal syscall).
fn run_podman(timeout_s: u64, args: &[&str]) -> (String, String, Result<(), String>) {
    let timeout = Duration::from_secs(if timeout_s == 0 { 60 } else { timeout_s });
    let mut child = match Command::new("podman")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (String::new(), String::new(), Err(e.to_string())),
    };
    // Drain the pipes on threads so a chatty command can't deadlock on a full pipe
    // while we poll for the timeout.
    let mut so_pipe = child.stdout.take();
    let mut se_pipe = child.stderr.take();
    let so_h = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = so_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let se_h = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = se_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    break child.wait().ok();
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => break None,
        }
    };

    let so = String::from_utf8_lossy(&so_h.join().unwrap_or_default()).into_owned();
    let se = String::from_utf8_lossy(&se_h.join().unwrap_or_default()).into_owned();
    let res = match status {
        Some(st) if st.success() => Ok(()),
        Some(st) => Err(format!("exit status {}", st.code().unwrap_or(-1))),
        None => Err("timed out".to_string()),
    };
    (so, se, res)
}

// ============================================================================
// small helpers
// ============================================================================

/// Canonical unordered service-pair key: `min(a,b) \0 max(a,b)`.
pub(crate) fn pair_key(a: &str, b: &str) -> String {
    if a > b {
        format!("{b}\0{a}")
    } else {
        format!("{a}\0{b}")
    }
}

fn nonempty(bytes: Vec<u8>) -> Option<String> {
    if bytes.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }
}

fn err(id: u64, op: &str, error: String) -> Reply {
    Reply {
        id: Some(id),
        ok: Some(false),
        op: Some(op.into()),
        error: Some(error),
        ..Default::default()
    }
}

fn ok_stdout(id: u64, op: &str, stdout: String) -> Reply {
    Reply {
        id: Some(id),
        ok: Some(true),
        op: Some(op.into()),
        stdout: Some(stdout),
        ..Default::default()
    }
}
