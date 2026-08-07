//! The agent's request dispatch and container ops: ping/exec/containers/lifecycle/
//! partition+heal/logs over podman + nft. Pure request -> reply; the transport is in
//! `bridge`.
//!
//! Every podman/nft/file helper fails with a structured [`AgentError`] that keeps the
//! underlying cause as its `source()`. Those errors are flattened to a wire `error`
//! string only at the boundary where a [`Reply`] is built.

use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tdvmm_proto::{ContainerInfo, ErrorKind, Reply, Request, MAX_LOGS_CHUNK_BYTES, SCHEMA};
use serde::Deserialize;

use crate::{AGENT_ID, BUILD};

/// The compose label podman/compose sets on each service's container(s).
const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

// ============================================================================
// Errors
// ============================================================================

/// The failure modes of the agent's container/network helpers. Each variant names a
/// genuinely distinct mode and keeps its cause as `source()` where one exists; the
/// context-attaching constructors ([`AgentError::io`], …) ensure every error records
/// the operation that produced it. The dispatch layer flattens these to the wire
/// `error` string.
#[derive(Debug)]
pub(crate) enum AgentError {
    /// Spawning a subprocess or reading a file failed. `what` names the operation
    /// (e.g. `"running podman ps"`); the underlying [`io::Error`] is the `source`.
    Io { what: String, source: io::Error },
    /// A subprocess ran to completion but did not succeed — a non-zero exit, or a
    /// kill after the timeout. Carries the command label, its exit `status` (`None`
    /// when it was killed by a signal or left no code), and the captured stderr.
    Command { what: String, status: Option<i32>, stderr: String },
    /// A subprocess emitted output that did not parse as the expected JSON; the
    /// [`serde_json::Error`] is the `source`.
    Parse { what: String, source: serde_json::Error },
    /// A subprocess succeeded but its output lacked the data we needed — an inspect
    /// that returned no bridge IP, or an empty log path. Not corrupt data, a miss.
    Missing { what: String },
}

impl AgentError {
    /// An [`Io`](AgentError::Io) with `what` context attached.
    pub(crate) fn io(what: impl Into<String>, source: io::Error) -> Self {
        AgentError::Io { what: what.into(), source }
    }
    /// A [`Command`](AgentError::Command) naming the command, its exit status, and
    /// captured stderr.
    pub(crate) fn command(what: impl Into<String>, status: Option<i32>, stderr: impl Into<String>) -> Self {
        AgentError::Command { what: what.into(), status, stderr: stderr.into() }
    }
    /// A [`Parse`](AgentError::Parse) wrapping a serde failure.
    pub(crate) fn parse(what: impl Into<String>, source: serde_json::Error) -> Self {
        AgentError::Parse { what: what.into(), source }
    }
    /// A [`Missing`](AgentError::Missing) from any displayable message.
    pub(crate) fn missing(what: impl Into<String>) -> Self {
        AgentError::Missing { what: what.into() }
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::Io { what, source } => write!(f, "{what}: {source}"),
            AgentError::Command { what, status, stderr } => {
                match status {
                    Some(code) => write!(f, "{what} exited with status {code}")?,
                    None => write!(f, "{what} did not complete")?,
                }
                if !stderr.is_empty() {
                    write!(f, ": {stderr}")?;
                }
                Ok(())
            }
            AgentError::Parse { what, source } => write!(f, "{what}: {source}"),
            AgentError::Missing { what } => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for AgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AgentError::Io { source, .. } => Some(source),
            AgentError::Parse { source, .. } => Some(source),
            AgentError::Command { .. } | AgentError::Missing { .. } => None,
        }
    }
}

/// Which container of a service [`resolve_by_service`] should return: the running
/// one (lifecycle / partition / logs) or a non-running one (`start`, or a killed
/// service's logs).
#[derive(Clone, Copy)]
enum Want {
    Running,
    Stopped,
}

// ============================================================================
// Agent state + dispatch
// ============================================================================

pub(crate) struct Agent {
    /// Canonical unordered service-pair key -> the two container IPs to drop
    /// between. The nft ruleset is rebuilt from this on every change; the
    /// `BTreeMap`'s sorted iteration keeps the emitted ruleset deterministic.
    partitions: BTreeMap<String, [String; 2]>,
    /// The verdict of the first [`tdvmm_proto::OP_FINISH`] served. First finish
    /// wins: a second caller is rejected rather than overwriting the outcome.
    finished: Option<i64>,
}

impl Agent {
    pub(crate) fn new() -> Self {
        Agent { partitions: BTreeMap::new(), finished: None }
    }

    pub(crate) fn handle(&mut self, req: &Request) -> Reply {
        match req.op.as_str() {
            tdvmm_proto::OP_FINISH => self.do_finish(req),
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

    /// The terminal op: record the run's verdict and answer. This stops nothing in
    /// the guest; the run ends when the host sees the `finish` event the bridge
    /// emits. First finish wins — a second is refused with the recorded verdict.
    fn do_finish(&mut self, req: &Request) -> Reply {
        let verdict = req.exit.unwrap_or(0);
        match self.finished {
            Some(first) => err(
                req.id,
                tdvmm_proto::OP_FINISH,
                format!("run already finished with exit {first}"),
            ),
            None => {
                self.finished = Some(verdict);
                ok_stdout(
                    req.id,
                    tdvmm_proto::OP_FINISH,
                    format!("finish {verdict}"),
                )
            }
        }
    }

    /// Whether this request was the accepted [`tdvmm_proto::OP_FINISH`] — i.e.
    /// the bridge must now tell the host to end the run. True for exactly one
    /// request per boot.
    pub(crate) fn accepted_finish(&self, req: &Request, reply: &Reply) -> bool {
        req.op == tdvmm_proto::OP_FINISH && reply.ok == Some(true)
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
            Err(e) => err(req.id, "containers", ErrorKind::PodmanPs.msg(e.to_string())),
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
            Err(e) => return err(req.id, "exec", ErrorKind::PodmanPs.msg(e.to_string())),
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
        let (id, name) = match resolve_by_service(&container, Want::Running) {
            Ok(Some(v)) => v,
            Ok(None) => {
                return err(
                    req.id,
                    op,
                    ErrorKind::NoContainer.msg(format!("no running container for service {container}")),
                )
            }
            Err(e) => return err(req.id, op, ErrorKind::PodmanPs.msg(e.to_string())),
        };
        if let Err(e) = run_podman(req.timeout_s.unwrap_or(0), &[op, &id]) {
            return err(req.id, op, format!("podman_{op}: {e}"));
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
        let (id, name) = match resolve_by_service(&container, Want::Stopped) {
            Ok(Some(v)) => v,
            Ok(None) => {
                // Already running? Treat as an idempotent success.
                if let Ok(Some(_)) = resolve_by_service(&container, Want::Running) {
                    return ok_stdout(req.id, "start", format!("already running {container}"));
                }
                return err(
                    req.id,
                    "start",
                    ErrorKind::NoContainer.msg(format!("no stopped container for service {container}")),
                );
            }
            Err(e) => return err(req.id, "start", ErrorKind::PodmanPs.msg(e.to_string())),
        };
        if let Err(e) = run_podman(req.timeout_s.unwrap_or(0), &["start", &id]) {
            return err(req.id, "start", format!("podman_start: {e}"));
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
        let aid = match resolve_by_service(&a, Want::Running) {
            Ok(v) => v,
            Err(e) => return err(req.id, "partition", ErrorKind::PodmanPs.msg(e.to_string())),
        };
        let bid = match resolve_by_service(&b, Want::Running) {
            Ok(v) => v,
            Err(e) => return err(req.id, "partition", ErrorKind::PodmanPs.msg(e.to_string())),
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
            return err(req.id, "partition", ErrorKind::Nft.msg(e.to_string()));
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
            return err(req.id, "heal", ErrorKind::Nft.msg(e.to_string()));
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
        let id = match resolve_by_service(&service, Want::Running) {
            Ok(Some((id, _))) => id,
            Ok(None) => match resolve_by_service(&service, Want::Stopped) {
                Ok(Some((id, _))) => id,
                Ok(None) => {
                    return err(
                        req.id,
                        "logs",
                        ErrorKind::NoContainer.msg(format!("no container for service {service}")),
                    )
                }
                Err(e) => return err(req.id, "logs", ErrorKind::PodmanPs.msg(e.to_string())),
            },
            Err(e) => return err(req.id, "logs", ErrorKind::PodmanPs.msg(e.to_string())),
        };

        let log_path = match container_log_path(&id) {
            Ok(p) => p,
            Err(e) => return err(req.id, "logs", ErrorKind::PodmanPs.msg(e.to_string())),
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
            Err(e) => err(req.id, "logs", ErrorKind::PodmanOp.msg(e.to_string())),
        }
    }

    /// Rebuild our nft table from `partitions` in ONE atomic transaction. The
    /// add/delete/add idiom gives a clean slate whether or not the table pre-
    /// existed. The chain is a default-ACCEPT forward base chain, so only our
    /// explicit drop rules matter and all other traffic falls through to netavark.
    fn apply_partitions(&self) -> Result<(), AgentError> {
        let mut b = String::new();
        b.push_str("add table inet tdvmm_faults\n");
        b.push_str("delete table inet tdvmm_faults\n");
        b.push_str("add table inet tdvmm_faults\n");
        b.push_str("add chain inet tdvmm_faults partition { type filter hook forward priority -300 ; policy accept ; }\n");
        for ips in self.partitions.values() {
            b.push_str(&format!(
                "add rule inet tdvmm_faults partition ip saddr {} ip daddr {} drop\n",
                ips[0], ips[1]
            ));
            b.push_str(&format!(
                "add rule inet tdvmm_faults partition ip saddr {} ip daddr {} drop\n",
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
            .map_err(|e| AgentError::io("spawning nft", e))?;
        // Feed the ruleset, then drop stdin so nft reads EOF and applies the
        // transaction. Hold the write result: when the write fails it is because nft
        // already died, and nft's own exit status and stderr below are the real
        // diagnostic — so wait for nft regardless (which also reaps it) and surface
        // the write error only if nft otherwise reports success.
        let write_result = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(b.as_bytes()),
            None => Ok(()),
        };
        let out = child
            .wait_with_output()
            .map_err(|e| AgentError::io("waiting for nft", e))?;
        if !out.status.success() {
            return Err(AgentError::command(
                "nft -f -",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        write_result.map_err(|e| AgentError::io("writing ruleset to nft stdin", e))?;
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

/// `podman <args>` capturing stdout as a JSON array of [`PodmanPs`]. Errors on spawn
/// failure, a non-zero exit, or output that does not parse.
fn podman_json(args: &[&str]) -> Result<Vec<PodmanPs>, AgentError> {
    let out = Command::new("podman")
        .args(args)
        .output()
        .map_err(|e| AgentError::io(format!("running podman {}", args.join(" ")), e))?;
    if !out.status.success() {
        return Err(AgentError::command(
            format!("podman {}", args.join(" ")),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    serde_json::from_slice(&out.stdout)
        .map_err(|e| AgentError::parse(format!("parsing podman {} output", args.join(" ")), e))
}

/// `podman ps -a --format json`, normalized to the wire census shape.
fn list_containers() -> Result<Vec<ContainerInfo>, AgentError> {
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
fn resolve_running(service: &str) -> Result<Option<(String, String)>, AgentError> {
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

/// Find a container of the service: the first RUNNING one for [`Want::Running`], else
/// the first NON-running one (for `start`). Matches label or name.
fn resolve_by_service(service: &str, want: Want) -> Result<Option<(String, String)>, AgentError> {
    let raw = podman_json(&["ps", "-a", "--format", "json"])?;
    for c in &raw {
        let mut matched = c.labels.get(COMPOSE_SERVICE_LABEL).map(|v| v == service) == Some(true);
        if !matched {
            matched = c.names.iter().any(|n| n == service);
        }
        if !matched {
            continue;
        }
        let running = c.state == "running";
        let wanted = match want {
            Want::Running => running,
            Want::Stopped => !running,
        };
        if wanted {
            return Ok(Some((c.id.clone(), c.names.first().cloned().unwrap_or_default())));
        }
    }
    Ok(None)
}

/// The first bridge IP of a container.
fn container_ip(id_or_name: &str) -> Result<String, AgentError> {
    let out = Command::new("podman")
        .args([
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}",
            id_or_name,
        ])
        .output()
        .map_err(|e| AgentError::io("running podman inspect", e))?;
    if !out.status.success() {
        return Err(AgentError::command(
            "podman inspect (network IP)",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    for tok in String::from_utf8_lossy(&out.stdout).split_whitespace() {
        if !tok.is_empty() {
            return Ok(tok.to_string());
        }
    }
    Err(AgentError::missing(format!("no bridge IP for {id_or_name}")))
}

/// The container's k8s-file log path. Podman (unlike Docker) has no top-level
/// `.LogPath`; the k8s-file backend records the file under
/// `.HostConfig.LogConfig.Path`. With the guest's `log_driver = "k8s-file"` this
/// is a plain file whose lines are `<RFC3339-ts> <stdout|stderr> <F|P> <message>`.
fn container_log_path(id_or_name: &str) -> Result<PathBuf, AgentError> {
    let out = Command::new("podman")
        .args([
            "inspect",
            "--format",
            "{{.HostConfig.LogConfig.Path}}",
            id_or_name,
        ])
        .output()
        .map_err(|e| AgentError::io("running podman inspect", e))?;
    if !out.status.success() {
        return Err(AgentError::command(
            "podman inspect (log path)",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if p.is_empty() {
        return Err(AgentError::missing(format!("empty log path for {id_or_name}")));
    }
    Ok(PathBuf::from(p))
}

/// Read up to `cap` raw bytes from `path` starting at `cursor`. Returns
/// `(lossy_utf8_data, raw_bytes_read, eof)`. A short read (fewer than `cap`
/// bytes) means the current end of the log was reached (`eof = true`). A missing
/// log file (a container that has not logged yet) reads as an empty log at EOF —
/// not an error. `next_cursor` is derived from `raw_bytes_read`, so paging stays
/// byte-exact even though `data` is lossy UTF-8.
pub(crate) fn read_log_chunk(
    path: impl AsRef<Path>,
    cursor: u64,
    cap: usize,
) -> Result<(String, usize, bool), AgentError> {
    let path = path.as_ref();
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((String::new(), 0, true));
        }
        Err(e) => return Err(AgentError::io(format!("opening log {}", path.display()), e)),
    };
    f.seek(SeekFrom::Start(cursor))
        .map_err(|e| AgentError::io(format!("seeking log {}", path.display()), e))?;
    let mut buf = vec![0u8; cap];
    let mut total = 0usize;
    while total < cap {
        let n = f
            .read(&mut buf[total..])
            .map_err(|e| AgentError::io(format!("reading log {}", path.display()), e))?;
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

/// Run `podman <args>` under a timeout (guest seconds; fast-forwards). std-only
/// timeout: poll `try_wait` and `child.kill()` on the deadline (no libc, no raw
/// signal syscall). The stdout/stderr pipes are drained on threads so a chatty
/// command cannot deadlock on a full pipe while we poll. On failure the captured
/// stderr rides along inside the [`AgentError::Command`]; stdout is not used.
fn run_podman(timeout_s: u64, args: &[&str]) -> Result<(), AgentError> {
    let timeout = Duration::from_secs(if timeout_s == 0 { 60 } else { timeout_s });
    let label = || format!("podman {}", args.join(" "));
    let mut child = Command::new("podman")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| AgentError::io(format!("spawning {}", label()), e))?;

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

    // Join both drains (also reaps the stdout thread); stdout itself is discarded.
    let _ = so_h.join();
    let stderr = String::from_utf8_lossy(&se_h.join().unwrap_or_default()).into_owned();
    match status {
        Some(st) if st.success() => Ok(()),
        other => Err(AgentError::command(label(), other.and_then(|st| st.code()), stderr)),
    }
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
