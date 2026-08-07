//! `tdvmm-proto` — the control-channel wire protocol: the line-delimited JSON
//! spoken between the VMM host and the guest-side `tdvmm-agent`. Protocol-only —
//! the message types, the [`SCHEMA`] constant, the [`ErrorKind`] taxonomy, and the
//! line-framing helpers; `serde` + `serde_json` are the only dependencies.
//!
//! One JSON object per line, `\n`-delimited, both directions:
//!
//! * host → agent: a [`Request`]; an unknown `op` is rejected.
//! * agent → host: a [`Reply`], covering both a command reply and the proactive
//!   *hello* the agent emits on start (`agent`/`schema`/`build`, no `id`/`ok`),
//!   which the host waits on to mark the agent ready. A hello is identified by
//!   `id.is_none() && agent.is_some()`.
//!
//! [`CONTROL_SOCKET_PATH`] carries the same [`Request`]/[`Reply`] JSON over a unix
//! socket the agent serves in-guest, bind-mounted into every container: any
//! container injects the host's op set through the same handler, and ends the run
//! with a verdict via [`OP_FINISH`].

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The binary mux protocol for the `--allow-egress` channel (COM4 / ttyS3). A
/// separate wire contract from the line-JSON control channel above; see
/// [`egress::EGRESS_SCHEMA`].
pub mod egress;

/// The wire-protocol schema version, embedded in the hello + `ping` reply. Bump on
/// any change to the types below; a bump requires regenerating the golden fixtures
/// in the same commit.
pub const SCHEMA: u32 = 4;

/// Hard cap on a single `logs` reply's `data` payload (128 KiB of raw k8s-file
/// bytes); JSON-escaping stays under the host's captured-TX drop threshold. The
/// agent enforces it regardless of a larger requested `max_bytes`; the host pages
/// a larger log via `next_cursor`/`eof`.
pub const MAX_LOGS_CHUNK_BYTES: u64 = 128 * 1024;

/// The guest-side event-bridge FIFO path: the source of truth shared by the host
/// (compose bind injection), the agent (its read fd), and the boot script's
/// `mkfifo`.
pub const EVENT_FIFO_PATH: &str = "/run/tdvmm/events";

/// The control-socket directory, bind-mounted read-write into every service. The
/// directory (not the socket file) is the bind unit, so the agent's `bind()`
/// creates an inode every container sees through the mount.
pub const CONTROL_DIR: &str = "/run/tdvmm/ctl";

/// The control socket: a unix-domain socket inside [`CONTROL_DIR`], served by the
/// agent and speaking this crate's line JSON. The connect path shared by the agent
/// and the language SDKs.
pub const CONTROL_SOCKET_PATH: &str = "/run/tdvmm/ctl/sock";

/// The terminal op: a container declares the run over with a verdict
/// ([`Request::exit`] + optional [`Request::message`]). The agent turns the first
/// one into a `finish` [`GuestEvent`]; the host ends the run on it.
pub const OP_FINISH: &str = "finish";

/// [`GuestEvent`] kinds only the agent may originate. A FIFO line claiming one is
/// rewritten to `invalid`, so the verdict and the fault trace can come only from a
/// command the agent served. Part of the wire contract, so it lives here.
pub const RESERVED_EVENT_KINDS: [&str; 2] = [OP_FINISH, "ctl"];

/// Whether `kind` is agent-originated only (see [`RESERVED_EVENT_KINDS`]).
pub fn is_reserved_event_kind(kind: &str) -> bool {
    RESERVED_EVENT_KINDS.contains(&kind)
}

// ============================================================================
// Messages
// ============================================================================

/// host → agent. One command line. `op` is a free string, not an enum, so the
/// agent can deserialize an unknown op and reply with a structured `unknown_op`
/// rejection.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Request {
    pub id: u64,
    pub op: String,
    /// Primary service/container target (also side A of a two-party fault).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Side B for the two-party network faults (`partition`/`heal`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    /// argv for `exec`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,
    /// Optional per-command timeout, in (virtual) seconds inside the guest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
    /// `logs`: byte offset into the target container's k8s-file log to read from.
    /// Absent = 0 (the start of the log). The host pages a whole log by looping
    /// from 0 and advancing to each reply's `next_cursor` until `eof`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<u64>,
    /// `logs`: host-requested read cap in bytes. The agent reads at most
    /// `min(max_bytes, MAX_LOGS_CHUNK_BYTES)` — the cap is a hard ceiling, never a
    /// promise of that many bytes. Absent = the agent's own cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    /// [`OP_FINISH`]: the run's verdict — 0 pass, nonzero fail. Absent is
    /// treated as 0, so `finish` with no argument means "passed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    /// [`OP_FINISH`]: an optional one-line reason, surfaced in the host's run
    /// summary (e.g. the assertion that failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// One container in a census (`containers` reply).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct ContainerInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub service: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default)]
    pub health: String,
}

/// A guest→host assertion/telemetry event, forwarded as an unsolicited, id-less
/// [`Reply`] (`event` set, no `id`). Two kinds are agent-originated only
/// ([`RESERVED_EVENT_KINDS`]):
///
/// * `ctl` — a control-socket command mirrored for the run trace: `name` = the op,
///   `ok` = whether it succeeded, `details` = its target.
/// * `finish` — a container called [`OP_FINISH`]; [`exit`](Self::exit) is the
///   verdict and `name` its optional message. The host ends the run on this.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GuestEvent {
    /// `always` | `sometimes` | `fault` | `done` | `invalid` | `ctl` | `finish`.
    pub kind: String,
    /// Assertion identity (the aggregation key); the `finish` message. Empty for
    /// `done`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The verdict bit for `always`/`sometimes`, or whether a mirrored `ctl`
    /// command succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// The run's verdict (`finish` only): 0 pass, nonzero fail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    /// Bounded free-form payload: a `fault` request's op/service, or the
    /// truncated raw line for `invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// agent → host. Covers both the proactive *hello* and every command reply: all
/// discriminating fields are optional, so a hello (`agent`/`schema`/`build`, no
/// `id`/`ok`) and a command reply (`id`/`ok` set) share one type. Empty payloads
/// are omitted on the wire; `exit` is kept even when `0`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Reply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containers: Option<Vec<ContainerInfo>>,
    /// Agent identity string (hello + `ping`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Wire-protocol schema the agent was built against (hello + `ping`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<u32>,
    /// Agent build hash — the compatibility oracle (hello + `ping`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// `logs`: the raw k8s-file bytes read starting at the request's `cursor`
    /// (lossy UTF-8; ≤ `MAX_LOGS_CHUNK_BYTES`). Empty payloads are omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// `logs`: `cursor + <raw bytes returned>` — the offset to pass as the next
    /// request's `cursor`. A byte offset into the file, independent of the lossy
    /// `data` string's length, so paging stays byte-exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<u64>,
    /// `logs`: true when this read reached the current end of the log (the host
    /// stops paging). A short read (fewer bytes than the cap) implies EOF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eof: Option<bool>,
    /// A guest→host assertion event (schema 3+); set only on an unsolicited,
    /// id-less line the agent bridges from the guest FIFO. See [`GuestEvent`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<GuestEvent>,
    /// Agent-stamped monotone per-boot sequence for bridged events; a gap tells
    /// the host an event line was dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

impl Reply {
    /// The proactive readiness handshake the agent emits on start.
    pub fn hello(agent: impl Into<String>, build: impl Into<String>) -> Reply {
        Reply {
            agent: Some(agent.into()),
            schema: Some(SCHEMA),
            build: Some(build.into()),
            ..Default::default()
        }
    }

    /// True if this line is a hello (readiness announcement), not a command reply.
    pub fn is_hello(&self) -> bool {
        self.id.is_none() && self.agent.is_some()
    }

    /// Wrap a bridged guest event as an unsolicited, id-less reply line.
    pub fn from_event(seq: u64, event: GuestEvent) -> Reply {
        Reply {
            seq: Some(seq),
            event: Some(event),
            ..Default::default()
        }
    }

    /// True if this line is a bridged guest event (schema 3+), not a hello or a
    /// command reply. A hello has `agent` set and no `event`, so the two never
    /// overlap.
    pub fn is_event(&self) -> bool {
        self.id.is_none() && self.event.is_some()
    }
}

// ============================================================================
// Error taxonomy (plain data)
// ============================================================================

/// The stable set of agent-side failure kinds. Plain data: it carries no I/O and
/// no logic, only a stable `code()` prefix so host and agent agree on the taxon-
/// omy of the free-form `error` string on the wire (`"<code>: <detail>"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Line was not valid request JSON.
    BadRequest,
    /// `op` is not one the agent implements.
    UnknownOp,
    /// A required field (`container`, `cmd`, `peer`, ...) was missing.
    MissingArgs,
    /// No matching container for a service (retryable inside `wait_for`).
    NoContainer,
    /// `podman ps`/`inspect` (enumeration) failed.
    PodmanPs,
    /// `podman exec` itself could not run (not the command's own exit).
    PodmanExec,
    /// A `podman` lifecycle verb (`kill`/`stop`/`start`) failed.
    PodmanOp,
    /// Resolving a container's bridge IP failed.
    Ip,
    /// Applying the nftables ruleset failed.
    Nft,
}

impl ErrorKind {
    /// The stable, machine-matchable prefix used in the wire `error` string.
    pub const fn code(self) -> &'static str {
        match self {
            ErrorKind::BadRequest => "bad_request",
            ErrorKind::UnknownOp => "unknown_op",
            ErrorKind::MissingArgs => "missing_args",
            ErrorKind::NoContainer => "no_container",
            ErrorKind::PodmanPs => "podman_ps",
            ErrorKind::PodmanExec => "podman_exec",
            ErrorKind::PodmanOp => "podman_op",
            ErrorKind::Ip => "ip",
            ErrorKind::Nft => "nft",
        }
    }

    /// Format the wire `error` string: `"<code>: <detail>"`.
    pub fn msg(self, detail: impl AsRef<str>) -> String {
        format!("{}: {}", self.code(), detail.as_ref())
    }
}

// ============================================================================
// Line framing (part of the wire contract; used by BOTH host and agent)
// ============================================================================

/// Trim leading/trailing ASCII whitespace (incl. the `\n` delimiter and a
/// tolerated `\r`) from one raw wire line.
pub fn trim_frame(line: &[u8]) -> &[u8] {
    let start = line
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(line.len());
    let end = line
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map(|i| i + 1)
        .unwrap_or(start);
    &line[start..end]
}

/// Encode one message as a framed wire line: JSON + a trailing `\n`.
pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    let mut buf = serde_json::to_vec(value)?;
    buf.push(b'\n');
    Ok(buf)
}

/// Decode one framed wire line (with or without its trailing delimiter /
/// tolerated `\r`) into a message.
pub fn decode_line<T: DeserializeOwned>(line: &[u8]) -> Result<T, serde_json::Error> {
    serde_json::from_slice(trim_frame(line))
}

// ============================================================================
// Tests — framing + the GOLDEN protocol fixtures (round-tripped here, and by
// BOTH the host and agent crates against these same committed files).
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::path::PathBuf;

    fn goldens_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("goldens")
    }

    #[test]
    fn frame_roundtrip_tolerates_crlf_and_ws() {
        let req = Request {
            id: 7,
            op: "ping".into(),
            ..Default::default()
        };
        let line = encode_line(&req).unwrap();
        assert_eq!(line.last(), Some(&b'\n'));
        // decode both the clean frame and a CRLF/whitespace-padded one.
        let a: Request = decode_line(&line).unwrap();
        let mut noisy = b"  ".to_vec();
        noisy.extend_from_slice(&serde_json::to_vec(&req).unwrap());
        noisy.extend_from_slice(b"\r\n");
        let b: Request = decode_line(&noisy).unwrap();
        assert_eq!(a, req);
        assert_eq!(b, req);
    }

    /// The canonical golden round-trip: parse each committed fixture into its
    /// typed message and re-encode it; the JSON must be **semantically** identical
    /// (field order-independent). Any proto change that drops/renames a field
    /// makes a fixture mismatch — forcing a SCHEMA bump + golden regen in the same
    /// commit. Set `TDVMM_REGEN_GOLDENS=1` to rewrite the fixtures from the types.
    #[test]
    fn goldens_roundtrip_and_carry_current_schema() {
        let dir = goldens_dir();
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        entries.sort();
        assert!(!entries.is_empty(), "no golden fixtures in {}", dir.display());

        let regen = std::env::var("TDVMM_REGEN_GOLDENS").is_ok();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let raw = std::fs::read(&path).unwrap();
            let golden: Value = decode_line(&raw)
                .unwrap_or_else(|e| panic!("{name}: fixture is not valid JSON: {e}"));

            // Round-trip through the concrete typed message for this fixture.
            let reencoded: Value = if name.starts_with("req_") {
                let msg: Request = decode_line(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
                serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap()
            } else if name.starts_with("rep_") {
                let msg: Reply = decode_line(&raw).unwrap_or_else(|e| panic!("{name}: {e}"));
                serde_json::from_slice(&serde_json::to_vec(&msg).unwrap()).unwrap()
            } else {
                panic!("{name}: golden must be prefixed req_ or rep_");
            };

            if regen {
                let mut bytes = serde_json::to_vec(&reencoded).unwrap();
                bytes.push(b'\n');
                std::fs::write(&path, bytes).unwrap();
                continue;
            }

            assert_eq!(
                golden, reencoded,
                "{name}: typed round-trip differs from the committed golden \
                 (proto changed without a golden regen?)"
            );

            // Any fixture that carries a `schema` must match the current SCHEMA.
            if let Some(s) = golden.get("schema").and_then(|v| v.as_u64()) {
                assert_eq!(
                    s as u32, SCHEMA,
                    "{name}: golden schema {s} != current SCHEMA {SCHEMA} (bump + regen)"
                );
            }
        }
    }

    #[test]
    fn reserved_event_kinds_are_agent_originated_only() {
        // The kinds a non-driver container must never be able to forge over the
        // shared events FIFO.
        assert!(is_reserved_event_kind("finish"));
        assert!(is_reserved_event_kind("ctl"));
        // Everything the workload assertion SDK emits stays acceptable.
        for k in ["always", "sometimes", "fault", "done", "invalid"] {
            assert!(!is_reserved_event_kind(k), "{k} must stay writable by workloads");
        }
    }

    #[test]
    fn finish_event_carries_the_verdict_code() {
        let ev = GuestEvent {
            kind: OP_FINISH.into(),
            name: "quorum was not lost".into(),
            exit: Some(1),
            ..Default::default()
        };
        let line = encode_line(&Reply::from_event(3, ev)).unwrap();
        let back: Reply = decode_line(&line).unwrap();
        assert!(back.is_event());
        let ev = back.event.unwrap();
        assert_eq!(ev.kind, "finish");
        assert_eq!(ev.exit, Some(1));
    }

    #[test]
    fn hello_is_distinguishable_from_ping_reply() {
        let hello = Reply::hello("tdvmm-agent/1", "abc123");
        assert!(hello.is_hello());
        let ping = Reply {
            id: Some(1),
            ok: Some(true),
            op: Some("ping".into()),
            agent: Some("tdvmm-agent/1".into()),
            schema: Some(SCHEMA),
            build: Some("abc123".into()),
            ..Default::default()
        };
        assert!(!ping.is_hello(), "ping reply has an id, so it is not a hello");
    }
}
