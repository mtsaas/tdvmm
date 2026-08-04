//! `dvmm-proto` — the control-channel wire protocol, the ONE source of truth for
//! the line-delimited JSON spoken over COM2/ttyS1 between the VMM host and the
//! guest-side `dvmm-agent`.
//!
//! Fable LOCKED this crate to be **protocol-only**: the request/response types
//! (serde), the [`SCHEMA`] version constant, the error taxonomy as plain data
//! ([`ErrorKind`]), and the **line-framing** encode/decode helpers (framing is
//! part of the wire contract). Dependencies are `serde` + `serde_json` ONLY — no
//! business logic, no I/O, no `anyhow`/`chrono`.
//!
//! ## The transport
//!
//! One JSON object per line, `\n`-delimited, in both directions:
//!
//! * host → agent: a [`Request`] (`op` = `ping`/`exec`/`containers`/`kill`/
//!   `stop`/`start`/`partition`/`heal`/`logs`; unknown `op` is rejected by the agent).
//! * agent → host: a [`Reply`]. The agent also emits one **proactive** [`Reply`]
//!   on start — the *hello* handshake (`agent`/`schema`/`build` set, no `id`/`ok`)
//!   — which the host waits on to mark the agent ready. `ping` echoes the same
//!   `schema` + `build`, so the handshake doubles as the compatibility oracle.
//!
//! A single permissive [`Reply`] type carries BOTH the hello and every command
//! reply: `id`/`ok`/`op` are optional, so a hello (no `id`, no `ok`) and a
//! command reply (both present) share one type. The host distinguishes a hello by
//! `id.is_none() && agent.is_some()`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The wire-protocol schema version. Embedded in the hello + `ping` reply and
/// recorded in the run-log preamble. Bump on ANY change to the types below (a
/// bump also requires regenerating the golden fixtures in the same commit).
///
/// * 1 — the original op set (`ping`/`exec`/`containers`/`kill`/`stop`/`start`/
///   `partition`/`heal`).
/// * 2 — adds the `logs` op (cursor-paged per-container k8s-file log fetch).
/// * 3 — adds guest→host assertion events: unsolicited, id-less [`Reply`]s
///   carrying a [`GuestEvent`], which the agent bridges from a guest FIFO.
pub const SCHEMA: u32 = 3;

/// Hard cap on a single `logs` reply's `data` payload: 128 KiB of raw k8s-file
/// bytes. JSON-escaping can ~double that on the wire, which still stays well
/// under the host's 1 MiB captured-TX drop threshold (`control.rs` TX_BUF_CAP) —
/// so a reply is never silently dropped. The agent enforces this regardless of a
/// larger requested `max_bytes`; the host loops via `next_cursor`/`eof` to read a
/// log larger than one chunk.
pub const MAX_LOGS_CHUNK_BYTES: u64 = 128 * 1024;

/// The guest-side event-bridge FIFO path (schema 3). The single source of truth
/// shared by the host (compose bind injection) and the agent (its read fd); the
/// boot script's `mkfifo` literal must match this string.
pub const EVENT_FIFO_PATH: &str = "/run/dvmm/events";

// ============================================================================
// Messages
// ============================================================================

/// host → agent. One command line.
///
/// `op` is a free string (NOT an enum) on purpose: the agent must be able to
/// deserialize an *unknown* op in order to reply with a structured `unknown_op`
/// rejection, exactly like the original Go agent.
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

/// A guest→host assertion/telemetry event (schema 3+). The agent bridges these
/// from a guest FIFO and forwards each as an unsolicited, id-less [`Reply`]
/// (`event` set, no `id`) — distinct from a command reply and from the hello.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GuestEvent {
    /// `always` | `sometimes` | `fault` | `done` | `invalid`.
    pub kind: String,
    /// Assertion identity (the aggregation key). Empty for `done`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// The verdict bit for `always`/`sometimes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ok: Option<bool>,
    /// Bounded free-form payload: a `fault` request's op/service, or the
    /// truncated raw line for `invalid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// agent → host. Serves BOTH the proactive *hello* and every command reply.
///
/// All discriminating fields are optional so one type covers the hello
/// (`agent`/`schema`/`build`, no `id`/`ok`) and command replies (`id`/`ok` set).
/// Empty string payloads are omitted on the wire (`skip_serializing_if`), mirror-
/// ing the Go agent's `omitempty`; `exit` is present even when `0` (it is a real
/// result, so `exec` always carries it).
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

/// Encode one message as a single framed wire line: canonical JSON + a trailing
/// `\n`. This is the ONLY sanctioned way to put a message on the wire.
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
    /// commit. Set `DVMM_REGEN_GOLDENS=1` to rewrite the fixtures from the types.
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

        let regen = std::env::var("DVMM_REGEN_GOLDENS").is_ok();
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
    fn hello_is_distinguishable_from_ping_reply() {
        let hello = Reply::hello("dvmm-agent/1", "abc123");
        assert!(hello.is_hello());
        let ping = Reply {
            id: Some(1),
            ok: Some(true),
            op: Some("ping".into()),
            agent: Some("dvmm-agent/1".into()),
            schema: Some(SCHEMA),
            build: Some("abc123".into()),
            ..Default::default()
        };
        assert!(!ping.is_hello(), "ping reply has an id, so it is not a hello");
    }
}
