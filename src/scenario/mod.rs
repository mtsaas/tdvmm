//! TEST-1a scenario engine: drive virtual time, wait for readiness, probe guest
//! state, assert, and emit a verdict.
//!
//! A **scenario** (host-side YAML, `--scenario s.yml`) is a timeline of steps.
//! Each step has an `at:` virtual-duration and one kind:
//!
//! * `exec`     — run a command in a container (via the agent) and assert its
//!                exit code and/or a stdout regex (covers SQL via psql, HTTP via
//!                curl, ...).
//! * `containers` — assert container states (`all_running` /
//!                `none_exited_nonzero`).
//! * `wait_for` — poll a probe at a virtual interval until a predicate holds or a
//!                virtual deadline passes (readiness).
//!
//! (TEST-1b fault ACTIONS — kill/stop/start/partition/heal — are also step
//! kinds; an unknown `op` is rejected by the agent.)
//!
//! ## Time is the timeline
//!
//! Every `at:` becomes a `(vtsc, ScenarioStep)` queue event, GENERALIZING the
//! existing `(vtsc, StopRun)` horizon (see `main.rs`). `at: 24h` therefore
//! fast-forwards through idle exactly like any other deadline — no forced jump
//! past guest activity. The engine only ever schedules ONE deadline at a time
//! (the next step, the next poll, a reply timeout, or the agent-ready backstop);
//! `main.rs` mirrors it into the one event queue each loop boundary, just as it
//! mirrors the LAPIC deadline.
//!
//! ## The LAW for commands
//!
//! The engine never touches the control UART directly for delivery: it queues a
//! line via [`ControlChannel::send_frame`], and `main.rs` pumps it into the FIFO
//! at the scheduled vtsc (on the vCPU thread). Commands are delivered at their
//! scheduled virtual time as queue events, never as an ad-hoc side channel.
//!
//! ## The verdict / JSONL schema — a STABLE, SHARED contract
//!
//! Two artifacts, both a documented, versioned contract shared with the future
//! e2e test runner:
//!
//! * **JSONL run log** (`--jsonl`, default `<report>.jsonl`): one line per event,
//!   each `{"schema":1,"ts_vtsc":<u64>,"t_s":<f64>,"wall_ms":<u64>,"type":<str>, ...}`.
//!   Event `type`s: `run_start`, `agent_ready`, `step_start`, `command`,
//!   `command_result`, `probe`, `assertion`, `containers`, `step_end`,
//!   `ff_stats`, `run_end`. The artifact sha256 + the scenario (+ its sha256) +
//!   this log = a reproduction package.
//! * **JSON report** (`--report`, default `<stack>.report.json`): one object —
//!   `{"schema":1,"verdict":"pass|fail|error","exit_code":0|1|2, ...,"steps":[...]}`.
//!
//! Plus a human summary table to stdout. Exit codes: **0** all assertions passed;
//! **1** an assertion failed (or a wait_for never became ready, or a container
//! exited nonzero); **2** an infrastructure error (bad scenario, agent/boot/guest
//! failure, or the agent could not reach a container). CI can tell "your stack is
//! wrong" (1) from "the tool broke" (2).

use tdvmm_proto::Reply;

mod eval;
mod engine;
mod ledger;
mod log;
mod report;
mod schema;

pub use engine::ScenarioEngine;
pub use report::{FfSummary, RunMeta};
pub use schema::{service_names, Scenario, ScenarioRun};

// ---- defaults (all virtual seconds) ----------------------------------------
const DEFAULT_AGENT_TIMEOUT_S: f64 = 120.0;
const DEFAULT_EXEC_TIMEOUT_S: f64 = 60.0;
const DEFAULT_WAITFOR_TIMEOUT_S: f64 = 60.0;
const DEFAULT_WAITFOR_EVERY_S: f64 = 5.0;
/// Slack added past the last step's `at` for the implicit end-horizon.
const HORIZON_SLACK_S: f64 = 300.0;
pub(crate) const SCHEMA_VERSION: u32 = 1;

/// Every inbound agent line is parsed as a permissive [`Reply`] — the wire type in
/// `tdvmm-proto`, which also carries the proactive hello and bridged guest events.
/// Aliased for intent at the [`engine`] and [`eval`] call sites.
type AgentLine = Reply;

/// Round `x` to 3 decimal places — the numeric precision of the JSONL/report.
fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Truncate `s` to at most `n` display characters (newlines flattened to spaces),
/// appending `…` when shortened. Used for the human summary table.
fn truncate(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n {
        s
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Shared test fixtures for the submodule test suites.
#[cfg(test)]
pub(crate) mod testutil {
    use std::collections::HashSet;

    use tdvmm_proto::GuestEvent;

    use crate::vtsc::TscFrequency;

    use super::schema::ScenarioError;
    use super::{RunMeta, Scenario, ScenarioEngine};

    pub(crate) fn svc() -> HashSet<String> {
        ["postgres", "service"].iter().map(|s| s.to_string()).collect()
    }

    pub(crate) fn scn_from(yaml: &str) -> Result<Scenario, ScenarioError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        // A monotonic counter guarantees a unique path per call. Keying the
        // filename on the literal's address (`{:p}`) collided instead: identical
        // string literals dedupe to one address, so concurrent tests raced the
        // same file across `fs::write`'s truncate window.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir()
            .join(format!("tdvmm-scn-test-{}-{n}.yml", std::process::id()));
        std::fs::write(&p, yaml).unwrap();
        let result = Scenario::load_and_validate(p.to_str().unwrap(), &svc());
        let _ = std::fs::remove_file(&p);
        result
    }

    /// The error message of an expected-to-fail scenario (avoids requiring
    /// `Scenario: Debug` for `unwrap_err`).
    pub(crate) fn err_of(yaml: &str) -> String {
        match scn_from(yaml) {
            Ok(_) => panic!("expected scenario to be rejected, but it validated"),
            Err(e) => e.0,
        }
    }

    pub(crate) fn ev(kind: &str, name: &str, ok: Option<bool>) -> GuestEvent {
        GuestEvent { kind: kind.into(), name: name.into(), ok, details: None }
    }

    pub(crate) fn engine_for(yaml: &str) -> ScenarioEngine {
        let scn = scn_from(yaml).unwrap();
        // These tests assert on the engine outcome, not on file contents, so sink
        // the JSONL/report to /dev/null rather than leaking temp files.
        let meta = RunMeta {
            stack: "t".into(),
            artifact_sha256: "x".into(),
            fast_forward: true,
            egress: false,
            jsonl_path: "/dev/null".into(),
            report_path: "/dev/null".into(),
        };
        let mut e = ScenarioEngine::new(scn, TscFrequency::from_hz(1_000_000_000), meta).unwrap();
        e.start(0);
        e
    }
}

#[cfg(test)]
mod tests {
    use tdvmm_proto::{decode_line, encode_line, Reply, Request};

    #[test]
    fn host_roundtrips_proto_goldens() {
        use serde_json::Value;
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tdvmm-proto/goldens");
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
            .collect();
        files.sort();
        assert!(!files.is_empty(), "no goldens in {}", dir.display());
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
            assert_eq!(golden, reenc, "{name}: host round-trip mismatch");
        }
    }
}
