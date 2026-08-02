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

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::time::Instant;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use dvmm_proto::{decode_line, encode_line, ContainerInfo, Reply, Request};

use crate::control::ControlChannel;
use crate::vtsc::TscFrequency;

// ---- defaults (all virtual seconds) ----------------------------------------
const DEFAULT_AGENT_TIMEOUT_S: f64 = 120.0;
const DEFAULT_EXEC_TIMEOUT_S: f64 = 60.0;
const DEFAULT_WAITFOR_TIMEOUT_S: f64 = 60.0;
const DEFAULT_WAITFOR_EVERY_S: f64 = 5.0;
/// Slack added past the last step's `at` for the implicit end-horizon.
const HORIZON_SLACK_S: f64 = 300.0;
pub const SCHEMA_VERSION: u32 = 1;

// ============================================================================
// Raw YAML schema (deserialize) — `deny_unknown_fields` makes an unknown key a
// loud static-validation failure (fail in ~10ms, before boot).
// ============================================================================

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScenario {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    run: Option<RawRun>,
    /// Services whose container is DELIBERATELY killed/stopped by this scenario, so
    /// an exited-nonzero container for one of them is NOT counted as an unexpected
    /// death (TEST-1b expected-death policy). Every other exited-nonzero container
    /// is an unexpected death → verdict fail.
    #[serde(default)]
    expect_death: Vec<String>,
    steps: Vec<RawStep>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRun {
    #[serde(default)]
    cmdline: Option<String>,
    #[serde(default)]
    mem: Option<u64>,
    #[serde(default)]
    ff: Option<bool>,
    #[serde(default)]
    max_virtual_time: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStep {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    exec: Option<RawExec>,
    #[serde(default)]
    expect: Option<RawExpect>,
    #[serde(default)]
    containers: Option<ContainersAssert>,
    #[serde(default)]
    wait_for: Option<RawWaitFor>,
    // ---- TEST-1b fault ACTIONS (each a step at an `at:` time) ----
    /// `kill: <service>` — SIGKILL the service's running container.
    #[serde(default)]
    kill: Option<String>,
    /// `stop: <service>` — graceful stop (SIGTERM then SIGKILL).
    #[serde(default)]
    stop: Option<String>,
    /// `start: <service>` — restart a previously stopped/killed container.
    #[serde(default)]
    start: Option<String>,
    /// `partition: [A, B]` — drop all traffic between the two services (both ways).
    #[serde(default)]
    partition: Option<Vec<String>>,
    /// `heal: [A, B]` — undo one partition; `heal: all` — undo every partition.
    #[serde(default)]
    heal: Option<HealSpec>,
    /// Optional timeout for a fault action (default `DEFAULT_EXEC_TIMEOUT_S`).
    #[serde(default)]
    timeout: Option<String>,
}

/// `heal: all` (a string) or `heal: [A, B]` (a two-element list).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum HealSpec {
    All(String),
    Pair(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawExec {
    container: String,
    cmd: CmdSpec,
    #[serde(default)]
    timeout: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum CmdSpec {
    /// A shell string, run as `sh -c "<string>"` inside the container.
    Shell(String),
    /// An explicit argv, exec'd directly.
    Argv(Vec<String>),
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawExpect {
    #[serde(default)]
    exit: Option<i64>,
    #[serde(default)]
    output_matches: Option<String>,
    #[serde(default)]
    output_contains: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainersAssert {
    AllRunning,
    NoneExitedNonzero,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWaitFor {
    probe: RawProbe,
    until: RawUntil,
    #[serde(default)]
    every: Option<String>,
    #[serde(default)]
    timeout: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProbe {
    #[serde(default)]
    exec: Option<RawExec>,
    /// `containers: true` selects a container-census probe (no exec).
    #[serde(default)]
    containers: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RawUntil {
    ExitZero,
    ExitNonzero,
    AllRunning,
    NoneExitedNonzero,
    OutputMatches(String),
    OutputContains(String),
}

// ============================================================================
// Prepared (validated + compiled) scenario — regex compiled, one kind per step.
// ============================================================================

pub struct Scenario {
    pub name: String,
    pub run: ScenarioRun,
    pub steps: Vec<PreparedStep>,
    pub source_path: String,
    pub source_sha256: String,
    /// Services whose death is expected (deliberately killed/stopped). An
    /// exited-nonzero container NOT in this set is an unexpected death → fail.
    pub expect_death: HashSet<String>,
    /// Whether to enforce the expected-death policy with an implicit end-of-run
    /// container census. Enabled only for scenarios that could produce a death
    /// (declare `expect_death`, or use a `kill`/`stop` action), so pure-assertion
    /// TEST-1a scenarios are byte-for-byte unchanged.
    pub death_policy: bool,
}

#[derive(Clone, Default)]
pub struct ScenarioRun {
    pub cmdline: Option<String>,
    pub mem: Option<u64>,
    pub ff: Option<bool>,
    pub max_virtual_time: Option<String>,
}

pub struct PreparedStep {
    display: String,
    at_secs: f64,
    kind: PreparedKind,
}

enum PreparedKind {
    Exec {
        req: ExecReq,
        expect: PreparedExpect,
        timeout_secs: f64,
    },
    Containers {
        assert: ContainersAssert,
        timeout_secs: f64,
    },
    WaitFor {
        probe: ProbeReq,
        until: PreparedUntil,
        every_secs: f64,
        timeout_secs: f64,
    },
    /// A TEST-1b fault ACTION delivered to the agent at the step's `at:` vtsc.
    Action {
        /// Agent op: "kill" | "stop" | "start" | "partition" | "heal".
        op: &'static str,
        /// Primary service (the target, or partition/heal side A). `None` only for
        /// `heal all`.
        a: Option<String>,
        /// Peer service (partition / heal-pair side B). `None` otherwise.
        b: Option<String>,
        timeout_secs: f64,
    },
}

#[derive(Clone)]
struct ExecReq {
    container: String,
    argv: Vec<String>,
}

#[derive(Clone)]
enum ProbeReq {
    Exec(ExecReq),
    Containers,
}

struct PreparedExpect {
    exit: i64,
    output_matches: Option<Regex>,
    output_contains: Option<String>,
    desc: String,
}

enum PreparedUntil {
    ExitZero,
    ExitNonzero,
    AllRunning,
    NoneExitedNonzero,
    OutputMatches(Regex),
    OutputContains(String),
}

#[derive(Debug)]
pub struct ScenarioError(pub String);
impl std::fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ScenarioError {}

fn cmd_to_argv(cmd: &CmdSpec) -> Vec<String> {
    match cmd {
        // A shell string runs inside the container as `sh -c "<string>"`.
        CmdSpec::Shell(s) => vec!["sh".into(), "-c".into(), s.clone()],
        CmdSpec::Argv(v) => v.clone(),
    }
}

fn dur(field: &str, s: &Option<String>, default: f64, allow_zero: bool) -> Result<f64, ScenarioError> {
    match s {
        None => Ok(default),
        Some(v) => {
            if let Some(secs) = crate::parse_duration_secs(v) {
                Ok(secs)
            } else if allow_zero && parses_to_zero(v) {
                // `at: 0s` (fire immediately) is valid; the shared duration parser
                // rejects non-positive values, so accept an explicit zero here.
                Ok(0.0)
            } else {
                Err(ScenarioError(format!("invalid duration for `{field}`: {v:?}")))
            }
        }
    }
}

fn parses_to_zero(v: &str) -> bool {
    let t = v.trim();
    let num = t.trim_end_matches(|c: char| c.is_ascii_alphabetic());
    matches!(num.trim().parse::<f64>(), Ok(n) if n == 0.0)
}

fn re(field: &str, s: &str) -> Result<Regex, ScenarioError> {
    Regex::new(s).map_err(|e| ScenarioError(format!("invalid regex for `{field}` ({s:?}): {e}")))
}

impl Scenario {
    /// Parse + STATICALLY validate a scenario file against the artifact's service
    /// names. Fails loudly and fast (before boot) on: bad YAML, unknown keys,
    /// unparseable durations, a bad regex, an unknown service, or a malformed
    /// step (not exactly one kind).
    pub fn load_and_validate(
        path: &str,
        services: &HashSet<String>,
    ) -> Result<Scenario, ScenarioError> {
        let bytes = std::fs::read(path)
            .map_err(|e| ScenarioError(format!("reading scenario {path}: {e}")))?;
        let source_sha256 = crate::artifact::sha256_hex(&bytes);
        let raw: RawScenario = serde_yaml::from_slice(&bytes)
            .map_err(|e| ScenarioError(format!("parsing scenario {path}: {e}")))?;

        if raw.steps.is_empty() {
            return Err(ScenarioError("scenario has no steps".into()));
        }

        let check_service = |c: &str| -> Result<(), ScenarioError> {
            if services.contains(c) {
                Ok(())
            } else {
                let mut names: Vec<&String> = services.iter().collect();
                names.sort();
                Err(ScenarioError(format!(
                    "unknown service {c:?} (not in the artifact's compose.lock.yml; \
                     known services: {names:?})"
                )))
            }
        };

        let prepare_exec = |e: &RawExec| -> Result<ExecReq, ScenarioError> {
            check_service(&e.container)?;
            let argv = cmd_to_argv(&e.cmd);
            if argv.is_empty() {
                return Err(ScenarioError(format!(
                    "empty command for exec in {:?}",
                    e.container
                )));
            }
            Ok(ExecReq {
                container: e.container.clone(),
                argv,
            })
        };

        let mut steps = Vec::with_capacity(raw.steps.len());
        for (i, s) in raw.steps.iter().enumerate() {
            let where_ = s
                .name
                .clone()
                .unwrap_or_else(|| format!("step[{i}]"));
            let at_secs = dur(&format!("{where_}.at"), &s.at, 0.0, true)?;

            let action_count = s.kill.is_some() as u8
                + s.stop.is_some() as u8
                + s.start.is_some() as u8
                + s.partition.is_some() as u8
                + s.heal.is_some() as u8;
            let n_kinds = s.exec.is_some() as u8
                + s.containers.is_some() as u8
                + s.wait_for.is_some() as u8
                + action_count;
            if n_kinds != 1 {
                return Err(ScenarioError(format!(
                    "{where_}: a step must have exactly one kind (exec / containers / \
                     wait_for / kill / stop / start / partition / heal); found {n_kinds}"
                )));
            }
            if s.expect.is_some() && s.exec.is_none() {
                return Err(ScenarioError(format!(
                    "{where_}: `expect` is only valid on an `exec` step"
                )));
            }
            if s.timeout.is_some() && action_count == 0 {
                return Err(ScenarioError(format!(
                    "{where_}: a step-level `timeout` is only valid on a fault action \
                     (kill/stop/start/partition/heal); exec/wait_for carry their own"
                )));
            }

            let kind = if let Some(e) = &s.exec {
                let req = prepare_exec(e)?;
                let timeout_secs =
                    dur(&format!("{where_}.exec.timeout"), &e.timeout, DEFAULT_EXEC_TIMEOUT_S, false)?;
                let raw_expect = s.expect.clone().unwrap_or_default();
                let output_matches = match &raw_expect.output_matches {
                    Some(r) => Some(re(&format!("{where_}.expect.output_matches"), r)?),
                    None => None,
                };
                let mut parts = vec![format!("exit={}", raw_expect.exit.unwrap_or(0))];
                if let Some(r) = &raw_expect.output_matches {
                    parts.push(format!("output~=/{r}/"));
                }
                if let Some(c) = &raw_expect.output_contains {
                    parts.push(format!("output contains {c:?}"));
                }
                let expect = PreparedExpect {
                    exit: raw_expect.exit.unwrap_or(0),
                    output_matches,
                    output_contains: raw_expect.output_contains.clone(),
                    desc: parts.join(", "),
                };
                PreparedKind::Exec {
                    req,
                    expect,
                    timeout_secs,
                }
            } else if let Some(c) = &s.containers {
                PreparedKind::Containers {
                    assert: *c,
                    timeout_secs: DEFAULT_EXEC_TIMEOUT_S,
                }
            } else if let Some(w) = &s.wait_for {
                let probe = match (&w.probe.exec, w.probe.containers) {
                    (Some(e), _) => ProbeReq::Exec(prepare_exec(e)?),
                    (None, Some(true)) => ProbeReq::Containers,
                    _ => {
                        return Err(ScenarioError(format!(
                            "{where_}.wait_for.probe must set `exec:` or `containers: true`"
                        )))
                    }
                };
                let until = match &w.until {
                    RawUntil::ExitZero => PreparedUntil::ExitZero,
                    RawUntil::ExitNonzero => PreparedUntil::ExitNonzero,
                    RawUntil::AllRunning => PreparedUntil::AllRunning,
                    RawUntil::NoneExitedNonzero => PreparedUntil::NoneExitedNonzero,
                    RawUntil::OutputMatches(r) => {
                        PreparedUntil::OutputMatches(re(&format!("{where_}.wait_for.until"), r)?)
                    }
                    RawUntil::OutputContains(s) => PreparedUntil::OutputContains(s.clone()),
                };
                // guard: container predicates need a container probe, output/exit
                // predicates need an exec probe.
                match (&probe, &until) {
                    (ProbeReq::Containers, PreparedUntil::ExitZero)
                    | (ProbeReq::Containers, PreparedUntil::ExitNonzero)
                    | (ProbeReq::Containers, PreparedUntil::OutputMatches(_))
                    | (ProbeReq::Containers, PreparedUntil::OutputContains(_)) => {
                        return Err(ScenarioError(format!(
                            "{where_}.wait_for: an exit/output predicate needs an `exec` probe"
                        )))
                    }
                    (ProbeReq::Exec(_), PreparedUntil::AllRunning)
                    | (ProbeReq::Exec(_), PreparedUntil::NoneExitedNonzero) => {
                        return Err(ScenarioError(format!(
                            "{where_}.wait_for: a container-state predicate needs a `containers` probe"
                        )))
                    }
                    _ => {}
                }
                let every_secs =
                    dur(&format!("{where_}.wait_for.every"), &w.every, DEFAULT_WAITFOR_EVERY_S, false)?;
                let timeout_secs = dur(
                    &format!("{where_}.wait_for.timeout"),
                    &w.timeout,
                    DEFAULT_WAITFOR_TIMEOUT_S,
                    false,
                )?;
                PreparedKind::WaitFor {
                    probe,
                    until,
                    every_secs,
                    timeout_secs,
                }
            } else {
                // A TEST-1b fault ACTION (kill/stop/start/partition/heal).
                let timeout_secs = dur(
                    &format!("{where_}.timeout"),
                    &s.timeout,
                    DEFAULT_EXEC_TIMEOUT_S,
                    false,
                )?;
                let (op, a, b): (&'static str, Option<String>, Option<String>) =
                    if let Some(svc) = &s.kill {
                        check_service(svc.as_str())?;
                        ("kill", Some(svc.clone()), None)
                    } else if let Some(svc) = &s.stop {
                        check_service(svc.as_str())?;
                        ("stop", Some(svc.clone()), None)
                    } else if let Some(svc) = &s.start {
                        check_service(svc.as_str())?;
                        ("start", Some(svc.clone()), None)
                    } else if let Some(pair) = &s.partition {
                        if pair.len() != 2 {
                            return Err(ScenarioError(format!(
                                "{where_}.partition needs exactly two services (got {})",
                                pair.len()
                            )));
                        }
                        check_service(pair[0].as_str())?;
                        check_service(pair[1].as_str())?;
                        if pair[0] == pair[1] {
                            return Err(ScenarioError(format!(
                                "{where_}.partition: the two services must differ ({:?})",
                                pair[0]
                            )));
                        }
                        ("partition", Some(pair[0].clone()), Some(pair[1].clone()))
                    } else if let Some(h) = &s.heal {
                        match h {
                            HealSpec::All(v) => {
                                if v.as_str() != "all" {
                                    return Err(ScenarioError(format!(
                                        "{where_}.heal: expected `all` or a two-service \
                                         list, got {v:?}"
                                    )));
                                }
                                ("heal", None, None)
                            }
                            HealSpec::Pair(pair) => {
                                if pair.len() != 2 {
                                    return Err(ScenarioError(format!(
                                        "{where_}.heal needs exactly two services or \
                                         `all` (got {})",
                                        pair.len()
                                    )));
                                }
                                check_service(pair[0].as_str())?;
                                check_service(pair[1].as_str())?;
                                ("heal", Some(pair[0].clone()), Some(pair[1].clone()))
                            }
                        }
                    } else {
                        // unreachable: n_kinds == 1 guarantees one of the above.
                        return Err(ScenarioError(format!("{where_}: no recognized step kind")));
                    };
                PreparedKind::Action {
                    op,
                    a,
                    b,
                    timeout_secs,
                }
            };

            let _ = i;
            steps.push(PreparedStep {
                display: where_,
                at_secs,
                kind,
            });
        }

        // Expected-death services must be real services (same check as any ref).
        let mut expect_death = HashSet::new();
        for svc in &raw.expect_death {
            check_service(svc.as_str())?;
            expect_death.insert(svc.clone());
        }
        // Enforce the expected-death policy (an implicit end-of-run census) only
        // where a death could occur — a declared expect_death, or a kill/stop
        // action — so pure-assertion TEST-1a scenarios are byte-for-byte unchanged.
        let death_policy = !expect_death.is_empty()
            || raw.steps.iter().any(|s| s.kill.is_some() || s.stop.is_some());

        let run = raw.run.map(|r| ScenarioRun {
            cmdline: r.cmdline,
            mem: r.mem,
            ff: r.ff,
            max_virtual_time: r.max_virtual_time,
        });
        // validate a run.max_virtual_time override up front too.
        let run = match run {
            Some(r) => {
                if let Some(mvt) = &r.max_virtual_time {
                    dur("run.max_virtual_time", &Some(mvt.clone()), 0.0, false)?;
                }
                r
            }
            None => ScenarioRun::default(),
        };

        Ok(Scenario {
            name: raw.name.unwrap_or_else(|| "scenario".into()),
            run,
            steps,
            source_path: path.to_string(),
            source_sha256,
            expect_death,
            death_policy,
        })
    }

    /// The implicit end-horizon (virtual seconds): the last step's `at` plus the
    /// step's own duration bound, plus the agent-ready backstop and slack. A run
    /// that gets wedged is stopped by this in seconds of wall time (fast-forward);
    /// a healthy run finishes well before it. Overridable by `run.max_virtual_time`
    /// or `--max-virtual-time`.
    pub fn implicit_horizon_secs(&self) -> f64 {
        let mut h: f64 = DEFAULT_AGENT_TIMEOUT_S;
        for s in &self.steps {
            let step_dur = match &s.kind {
                PreparedKind::Exec { timeout_secs, .. } => *timeout_secs,
                PreparedKind::Containers { timeout_secs, .. } => *timeout_secs,
                PreparedKind::WaitFor { timeout_secs, .. } => *timeout_secs,
                PreparedKind::Action { timeout_secs, .. } => *timeout_secs,
            };
            h = h.max(s.at_secs + step_dur);
        }
        h + HORIZON_SLACK_S
    }
}

/// Extract compose service names from a `compose.lock.yml` byte buffer (the keys
/// under `services:`), for static validation of scenario container references.
pub fn service_names(compose_lock: &[u8]) -> Result<HashSet<String>, ScenarioError> {
    let doc: Value = serde_yaml::from_slice(compose_lock)
        .map_err(|e| ScenarioError(format!("parsing compose.lock.yml: {e}")))?;
    let mut set = HashSet::new();
    if let Some(services) = doc.get("services").and_then(|v| v.as_object()) {
        for k in services.keys() {
            set.insert(k.clone());
        }
    }
    if set.is_empty() {
        return Err(ScenarioError(
            "no services found in compose.lock.yml".into(),
        ));
    }
    Ok(set)
}

// ============================================================================
// Agent protocol (line-delimited JSON) — the wire types live in `dvmm-proto`,
// the ONE source of truth shared with the guest `dvmm-agent`. The host builds
// [`Request`]s and parses [`Reply`]s (a permissive superset that also carries the
// proactive hello). `ContainerInfo` is re-exported for the assertion evaluators.
// ============================================================================

/// The host parses every inbound line as a permissive [`Reply`] (it also carries
/// the proactive hello: `id`/`ok` absent, `agent` present).
type AgentLine = Reply;

// ============================================================================
// Run metadata + FF summary + report structs (the STABLE contract).
// ============================================================================

pub struct RunMeta {
    pub stack: String,
    pub artifact_sha256: String,
    pub fast_forward: bool,
    pub jsonl_path: String,
    pub report_path: String,
}

#[derive(Clone, Default, Serialize)]
pub struct FfSummary {
    pub jumps: u64,
    pub speedup: f64,
    pub virtual_seconds: f64,
    pub per_hop_mean_us: f64,
    pub max_delta_s: f64,
}

#[derive(Serialize)]
struct Report {
    schema: u32,
    verdict: String,
    exit_code: i32,
    stack: String,
    artifact_sha256: String,
    scenario: String,
    scenario_sha256: String,
    fast_forward: bool,
    duration_wall_s: f64,
    virtual_seconds: f64,
    ff: FfSummary,
    steps_total: usize,
    steps_passed: usize,
    steps: Vec<StepReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<String>,
}

#[derive(Clone, Serialize)]
struct StepReport {
    index: usize,
    name: String,
    kind: String,
    at_s: f64,
    outcome: String, // "pass" | "fail" | "error" | "skipped"
    detail: String,
}

// ============================================================================
// The engine.
// ============================================================================

enum Phase {
    AwaitAgent,
    Scheduled,
    AwaitReply { id: u64 },
    WaitPoll { id: u64 },
    WaitSleep,
    /// TEST-1b: the implicit end-of-run container census that enforces the
    /// expected-death policy (only for scenarios that could produce a death).
    FinalCensus { id: u64 },
    Done,
}

struct Outcome {
    verdict: &'static str, // "pass" | "fail" | "error"
    exit_code: i32,
    failure: Option<String>,
}

pub struct ScenarioEngine {
    scn: Scenario,
    meta: RunMeta,
    freq: TscFrequency,
    logger: Logger,

    phase: Phase,
    idx: usize,
    next_id: u64,
    run_start_vtsc: u64,
    t0: u64,
    next_deadline: Option<u64>,
    wait_overall_deadline: u64,
    /// Whether the implicit end-of-run census has already been issued.
    final_census_done: bool,

    step_reports: Vec<StepReport>,
    outcome: Option<Outcome>,
}

/// What the caller should do after an engine call.
pub enum Flow {
    Continue,
    Finished,
}

impl ScenarioEngine {
    pub fn new(scn: Scenario, freq: TscFrequency, meta: RunMeta) -> Result<Self, ScenarioError> {
        let logger = Logger::new(&meta.jsonl_path, freq)
            .map_err(|e| ScenarioError(format!("opening JSONL log {}: {e}", meta.jsonl_path)))?;
        Ok(Self {
            scn,
            meta,
            freq,
            logger,
            phase: Phase::AwaitAgent,
            idx: 0,
            next_id: 1,
            run_start_vtsc: 0,
            t0: 0,
            next_deadline: None,
            wait_overall_deadline: 0,
            final_census_done: false,
            step_reports: Vec::new(),
            outcome: None,
        })
    }

    fn secs_to_cycles(&self, secs: f64) -> u64 {
        self.freq.ns_to_cycles((secs * 1_000_000_000.0) as u64)
    }

    /// Record the run header and arm the agent-ready backstop. Call once, before
    /// the vCPU loop, with the starting vtsc.
    pub fn start(&mut self, now: u64) {
        self.run_start_vtsc = now;
        self.t0 = now;
        self.logger.set_t0(now);
        self.logger.event(
            now,
            "run_start",
            json!({
                "stack": self.meta.stack,
                "artifact_sha256": self.meta.artifact_sha256,
                "scenario": self.scn.source_path,
                "scenario_sha256": self.scn.source_sha256,
                "scenario_name": self.scn.name,
                "fast_forward": self.meta.fast_forward,
                "steps_total": self.scn.steps.len(),
                // The control-channel wire schema (Fable §4): the run-log header
                // records the proto version alongside the JSONL/report schema.
                "proto_schema": dvmm_proto::SCHEMA,
            }),
        );
        self.phase = Phase::AwaitAgent;
        self.next_deadline = Some(now.saturating_add(self.secs_to_cycles(DEFAULT_AGENT_TIMEOUT_S)));
    }

    pub fn next_deadline(&self) -> Option<u64> {
        self.next_deadline
    }

    pub fn is_finished(&self) -> bool {
        self.outcome.is_some()
    }

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }


    // ---- terminal transitions ----
    fn finish(&mut self, now: u64, verdict: &'static str, exit_code: i32, failure: Option<String>) {
        self.phase = Phase::Done;
        self.next_deadline = None;
        self.logger.event(
            now,
            "verdict_decided",
            json!({ "verdict": verdict, "exit_code": exit_code,
                    "failure": failure }),
        );
        self.outcome = Some(Outcome {
            verdict,
            exit_code,
            failure,
        });
    }

    fn record_step(&mut self, index: usize, outcome: &str, detail: String) {
        let (name, kind, at_s) = self
            .scn
            .steps
            .get(index)
            .map(|s| (s.display.clone(), step_kind_str(&s.kind).to_string(), s.at_secs))
            .unwrap_or_else(|| (format!("step[{index}]"), "?".into(), 0.0));
        self.step_reports.push(StepReport {
            index,
            name,
            kind,
            at_s,
            outcome: outcome.to_string(),
            detail,
        });
    }

    /// External stop (guest died, horizon, wall timeout) before the scenario
    /// completed: classify as an infrastructure error (exit 2).
    pub fn record_abort(&mut self, now: u64, reason: &str) {
        if self.outcome.is_some() {
            return;
        }
        // mark the in-flight step (if any) as errored.
        if self.idx < self.scn.steps.len() {
            let idx = self.idx;
            self.record_step(idx, "error", format!("aborted: {reason}"));
        }
        self.finish(now, "error", 2, Some(reason.to_string()));
    }

    // ---- scheduling ----
    fn schedule(&mut self, now: u64, com2: &mut ControlChannel) {
        if self.idx >= self.scn.steps.len() {
            // All steps passed. If this scenario could have produced a container
            // death, enforce the expected-death policy with ONE final census
            // before declaring PASS (an unexpected exited-nonzero container →
            // fail). Pure-assertion scenarios skip this and pass immediately.
            if self.scn.death_policy && !self.final_census_done {
                self.final_census_done = true;
                self.begin_final_census(now, com2);
            } else {
                self.finish(now, "pass", 0, None);
            }
            return;
        }
        let target = self
            .t0
            .wrapping_add(self.secs_to_cycles(self.scn.steps[self.idx].at_secs));
        if target <= now {
            self.begin_step(now, com2);
        } else {
            self.phase = Phase::Scheduled;
            self.next_deadline = Some(target);
        }
    }

    fn build_exec_req(&mut self, e: &ExecReq, timeout_secs: f64) -> (u64, Vec<u8>) {
        let id = self.new_id();
        let req = Request {
            id,
            op: "exec".into(),
            container: Some(e.container.clone()),
            peer: None,
            cmd: Some(e.argv.clone()),
            timeout_s: Some(timeout_secs.max(1.0) as u64),
            ..Default::default()
        };
        (id, encode_line(&req).unwrap())
    }

    fn build_containers_req(&mut self) -> (u64, Vec<u8>) {
        let id = self.new_id();
        let req = Request {
            id,
            op: "containers".into(),
            ..Default::default()
        };
        (id, encode_line(&req).unwrap())
    }

    /// Build a TEST-1b fault-action request (kill/stop/start/partition/heal).
    fn build_action_req(
        &mut self,
        op: &'static str,
        a: &Option<String>,
        b: &Option<String>,
        timeout_secs: f64,
    ) -> (u64, Vec<u8>) {
        let id = self.new_id();
        let req = Request {
            id,
            op: op.into(),
            container: a.clone(),
            peer: b.clone(),
            cmd: None,
            timeout_s: Some(timeout_secs.max(1.0) as u64),
            ..Default::default()
        };
        (id, encode_line(&req).unwrap())
    }

    /// Issue the implicit end-of-run container census (TEST-1b expected-death
    /// policy). Enters [`Phase::FinalCensus`]; the reply is evaluated in
    /// [`Self::on_final_census_reply`].
    fn begin_final_census(&mut self, now: u64, com2: &mut ControlChannel) {
        let (id, bytes) = self.build_containers_req();
        self.logger.event(
            now,
            "command",
            json!({ "id": id, "op": "containers(final-census)" }),
        );
        com2.send_frame(&bytes);
        self.phase = Phase::FinalCensus { id };
        self.next_deadline = Some(now.saturating_add(self.secs_to_cycles(DEFAULT_EXEC_TIMEOUT_S)));
    }

    /// Evaluate the end-of-run census against the expected-death policy. An
    /// exited-nonzero container NOT in `expect_death` → verdict fail (exit 1);
    /// otherwise the run passes. A census that could not be taken does NOT fail an
    /// otherwise-passing run (the steps already passed) — it is logged and passes.
    fn on_final_census_reply(&mut self, now: u64, reply: &AgentLine) {
        self.logger.event(
            now,
            "final_census",
            json!({
                "id": reply.id, "ok": reply.ok, "error": reply.error,
                "containers": reply.containers,
            }),
        );
        if reply.ok != Some(true) {
            self.finish(now, "pass", 0, None);
            return;
        }
        let empty = Vec::new();
        let list = reply.containers.as_ref().unwrap_or(&empty);
        match check_unexpected_deaths(list, &self.scn.expect_death) {
            Some(bad) => self.finish(
                now,
                "fail",
                1,
                Some(format!(
                    "unexpected container death: {bad} (not declared in expect_death)"
                )),
            ),
            None => self.finish(now, "pass", 0, None),
        }
    }

    fn begin_step(&mut self, now: u64, com2: &mut ControlChannel) {
        let idx = self.idx;
        let (display, kind_str) = {
            let s = &self.scn.steps[idx];
            (s.display.clone(), step_kind_str(&s.kind).to_string())
        };
        self.logger.event(
            now,
            "step_start",
            json!({ "step": idx, "name": display, "kind": kind_str }),
        );

        // Clone the small request specs out so we don't hold a borrow of self.scn.
        enum Action {
            Exec { req: ExecReq, timeout: f64 },
            Containers { timeout: f64 },
            WaitForExec { req: ExecReq, timeout: f64, overall: f64 },
            WaitForContainers { overall: f64 },
            Fault { op: &'static str, a: Option<String>, b: Option<String>, timeout: f64 },
        }
        let action = match &self.scn.steps[idx].kind {
            PreparedKind::Exec { req, timeout_secs, .. } => Action::Exec {
                req: req.clone(),
                timeout: *timeout_secs,
            },
            PreparedKind::Containers { timeout_secs, .. } => Action::Containers {
                timeout: *timeout_secs,
            },
            PreparedKind::WaitFor {
                probe,
                every_secs: _,
                timeout_secs,
                ..
            } => match probe {
                ProbeReq::Exec(req) => Action::WaitForExec {
                    req: req.clone(),
                    timeout: DEFAULT_EXEC_TIMEOUT_S,
                    overall: *timeout_secs,
                },
                ProbeReq::Containers => Action::WaitForContainers {
                    overall: *timeout_secs,
                },
            },
            PreparedKind::Action { op, a, b, timeout_secs } => Action::Fault {
                op,
                a: a.clone(),
                b: b.clone(),
                timeout: *timeout_secs,
            },
        };

        match action {
            Action::Exec { req, timeout } => {
                let (id, bytes) = self.build_exec_req(&req, timeout);
                self.log_command(now, idx, id, "exec", Some(&req));
                com2.send_frame(&bytes);
                let deadline = now.saturating_add(self.secs_to_cycles(timeout));
                self.phase = Phase::AwaitReply { id };
                self.next_deadline = Some(deadline);
            }
            Action::Containers { timeout } => {
                let (id, bytes) = self.build_containers_req();
                self.log_command(now, idx, id, "containers", None);
                com2.send_frame(&bytes);
                let deadline = now.saturating_add(self.secs_to_cycles(timeout));
                self.phase = Phase::AwaitReply { id };
                self.next_deadline = Some(deadline);
            }
            Action::WaitForExec { req, timeout, overall } => {
                self.wait_overall_deadline =
                    now.saturating_add(self.secs_to_cycles(overall));
                let (id, bytes) = self.build_exec_req(&req, timeout);
                self.log_command(now, idx, id, "exec(probe)", Some(&req));
                com2.send_frame(&bytes);
                self.phase = Phase::WaitPoll { id };
                self.next_deadline = Some(self.wait_overall_deadline);
            }
            Action::WaitForContainers { overall } => {
                self.wait_overall_deadline =
                    now.saturating_add(self.secs_to_cycles(overall));
                let (id, bytes) = self.build_containers_req();
                self.log_command(now, idx, id, "containers(probe)", None);
                com2.send_frame(&bytes);
                self.phase = Phase::WaitPoll { id };
                self.next_deadline = Some(self.wait_overall_deadline);
            }
            Action::Fault { op, a, b, timeout } => {
                // A TEST-1b fault delivered at its scheduled vtsc (THE LAW): logged
                // with op + service(+peer), then awaited like any command.
                let (id, bytes) = self.build_action_req(op, &a, &b, timeout);
                self.log_action(now, idx, id, op, &a, &b);
                com2.send_frame(&bytes);
                let deadline = now.saturating_add(self.secs_to_cycles(timeout));
                self.phase = Phase::AwaitReply { id };
                self.next_deadline = Some(deadline);
            }
        }
    }

    fn send_probe(&mut self, now: u64, com2: &mut ControlChannel) {
        let idx = self.idx;
        let probe = match &self.scn.steps[idx].kind {
            PreparedKind::WaitFor { probe, .. } => probe.clone(),
            _ => return,
        };
        match probe {
            ProbeReq::Exec(req) => {
                let (id, bytes) = self.build_exec_req(&req, DEFAULT_EXEC_TIMEOUT_S);
                self.log_command(now, idx, id, "exec(probe)", Some(&req));
                com2.send_frame(&bytes);
                self.phase = Phase::WaitPoll { id };
            }
            ProbeReq::Containers => {
                let (id, bytes) = self.build_containers_req();
                self.log_command(now, idx, id, "containers(probe)", None);
                com2.send_frame(&bytes);
                self.phase = Phase::WaitPoll { id };
            }
        }
        self.next_deadline = Some(self.wait_overall_deadline);
    }

    fn log_command(&mut self, now: u64, idx: usize, id: u64, op: &str, req: Option<&ExecReq>) {
        let mut payload = json!({ "step": idx, "id": id, "op": op });
        if let Some(r) = req {
            payload["container"] = json!(r.container);
            payload["cmd"] = json!(r.argv);
        }
        self.logger.event(now, "command", payload);
    }

    /// Log a TEST-1b fault command at its scheduled vtsc (op + service[+peer]).
    fn log_action(
        &mut self,
        now: u64,
        idx: usize,
        id: u64,
        op: &str,
        a: &Option<String>,
        b: &Option<String>,
    ) {
        let mut payload = json!({ "step": idx, "id": id, "op": op, "fault": true });
        if let Some(a) = a {
            payload["service"] = json!(a);
        }
        if let Some(b) = b {
            payload["peer"] = json!(b);
        }
        self.logger.event(now, "command", payload);
    }

    /// A `(vtsc, ScenarioStep)` deadline fired at `now`.
    pub fn on_due(&mut self, now: u64, com2: &mut ControlChannel) -> Flow {
        match self.phase {
            Phase::AwaitAgent => {
                self.record_abort(now, "agent did not report ready (no hello on ttyS1)");
            }
            Phase::Scheduled => {
                self.begin_step(now, com2);
            }
            Phase::AwaitReply { .. } => {
                let idx = self.idx;
                self.record_step(idx, "error", "command timed out (no agent reply)".into());
                self.finish(
                    now,
                    "error",
                    2,
                    Some(format!(
                        "step {idx} command timed out (no agent reply within its timeout)"
                    )),
                );
            }
            Phase::WaitPoll { .. } | Phase::WaitSleep => {
                if now >= self.wait_overall_deadline {
                    let idx = self.idx;
                    self.record_step(idx, "fail", "readiness timeout".into());
                    self.finish(
                        now,
                        "fail",
                        1,
                        Some(format!("step {idx} wait_for timed out (never became ready)")),
                    );
                } else {
                    // WaitSleep poll time: probe again.
                    self.send_probe(now, com2);
                }
            }
            Phase::FinalCensus { .. } => {
                // The end-of-run census did not return in time. Do NOT fail an
                // otherwise-passing run (every step already passed); log + pass.
                self.logger.event(
                    now,
                    "final_census",
                    json!({ "ok": false, "error": "final census timed out" }),
                );
                self.finish(now, "pass", 0, None);
            }
            Phase::Done => {}
        }
        if self.is_finished() {
            Flow::Finished
        } else {
            Flow::Continue
        }
    }

    /// A reply line arrived from the agent.
    pub fn on_reply(&mut self, line: &[u8], now: u64, com2: &mut ControlChannel) -> Flow {
        let parsed: AgentLine = match decode_line(line) {
            Ok(v) => v,
            Err(_) => {
                // Non-JSON noise on ttyS1: log and ignore.
                self.logger.event(
                    now,
                    "noise",
                    json!({ "raw": String::from_utf8_lossy(line) }),
                );
                return Flow::Continue;
            }
        };

        // Agent hello (proactive readiness announcement). Carries the agent's
        // wire schema + build hash — the compatibility oracle (Fable §4).
        if parsed.is_hello() {
            if let Phase::AwaitAgent = self.phase {
                self.t0 = now;
                if let Some(s) = parsed.schema {
                    if s != dvmm_proto::SCHEMA {
                        crate::log_line(format_args!(
                            "[dvmm][WARN] agent proto schema {s} != host {} \
                             (host+agent should ship in lockstep)",
                            dvmm_proto::SCHEMA
                        ));
                    }
                }
                self.logger.event(
                    now,
                    "agent_ready",
                    json!({
                        "agent": parsed.agent,
                        "agent_schema": parsed.schema,
                        "agent_build": parsed.build,
                        "proto_schema": dvmm_proto::SCHEMA,
                    }),
                );
                self.schedule(now, com2);
            }
            return self.flow();
        }

        let id = match parsed.id {
            Some(id) => id,
            None => return Flow::Continue,
        };

        match self.phase {
            Phase::AwaitReply { id: expect, .. } if id == expect => {
                self.on_assertion_reply(now, &parsed, com2);
            }
            Phase::WaitPoll { id: expect } if id == expect => {
                self.on_probe_reply(now, &parsed, com2);
            }
            Phase::FinalCensus { id: expect } if id == expect => {
                self.on_final_census_reply(now, &parsed);
            }
            _ => {
                // stale / unexpected reply id — ignore.
            }
        }
        self.flow()
    }

    fn flow(&self) -> Flow {
        if self.is_finished() {
            Flow::Finished
        } else {
            Flow::Continue
        }
    }

    fn on_assertion_reply(&mut self, now: u64, reply: &AgentLine, com2: &mut ControlChannel) {
        let idx = self.idx;
        self.logger.event(
            now,
            "command_result",
            json!({
                "step": idx, "id": reply.id, "ok": reply.ok,
                "exit": reply.exit, "dur_ms": reply.dur_ms,
                "stdout": reply.stdout, "stderr": reply.stderr,
                "error": reply.error,
            }),
        );

        // Agent could not run the command / reach the container -> infra (exit 2).
        if reply.ok != Some(true) {
            let msg = reply
                .error
                .clone()
                .unwrap_or_else(|| "agent could not execute the command".into());
            self.record_step(idx, "error", msg.clone());
            self.finish(
                now,
                "error",
                2,
                Some(format!("step {idx}: {msg}")),
            );
            return;
        }

        let kind = &self.scn.steps[idx].kind;
        let (passed, detail) = match kind {
            PreparedKind::Exec { expect, .. } => {
                eval_exec_assertion(expect, reply)
            }
            PreparedKind::Containers { assert, .. } => {
                eval_containers_assertion(*assert, reply, &self.scn.expect_death)
            }
            PreparedKind::Action { op, .. } => {
                // A fault reached the agent and was applied (ok:true above); an
                // action has no assertion of its own — it simply passes.
                (true, format!("{op}: {}", reply.stdout.as_deref().unwrap_or("ok")))
            }
            _ => (false, "internal: reply for non-assertion step".to_string()),
        };

        self.logger.event(
            now,
            "assertion",
            json!({ "step": idx, "passed": passed, "detail": detail }),
        );

        if passed {
            self.record_step(idx, "pass", detail);
            self.logger.event(
                now,
                "step_end",
                json!({ "step": idx, "outcome": "pass" }),
            );
            self.idx += 1;
            self.schedule(now, com2);
        } else {
            self.record_step(idx, "fail", detail.clone());
            self.logger
                .event(now, "step_end", json!({ "step": idx, "outcome": "fail" }));
            self.finish(now, "fail", 1, Some(format!("step {idx} assertion failed: {detail}")));
        }
    }

    fn on_probe_reply(&mut self, now: u64, reply: &AgentLine, com2: &mut ControlChannel) {
        let idx = self.idx;
        let (satisfied, detail) = match &self.scn.steps[idx].kind {
            PreparedKind::WaitFor { until, .. } => eval_until(until, reply, &self.scn.expect_death),
            _ => (false, "internal: probe for non-wait_for step".to_string()),
        };
        self.logger.event(
            now,
            "probe",
            json!({ "step": idx, "id": reply.id, "satisfied": satisfied,
                    "ok": reply.ok, "exit": reply.exit, "error": reply.error,
                    "stdout": reply.stdout, "stderr": reply.stderr, "detail": detail }),
        );

        if satisfied {
            self.record_step(idx, "pass", format!("ready: {detail}"));
            self.logger
                .event(now, "step_end", json!({ "step": idx, "outcome": "pass" }));
            self.idx += 1;
            self.schedule(now, com2);
            return;
        }

        // Not ready: retry after `every`, bounded by the overall deadline.
        if now >= self.wait_overall_deadline {
            self.record_step(idx, "fail", format!("readiness timeout: {detail}"));
            self.finish(
                now,
                "fail",
                1,
                Some(format!("step {idx} wait_for timed out (never became ready)")),
            );
            return;
        }
        let every = match &self.scn.steps[idx].kind {
            PreparedKind::WaitFor { every_secs, .. } => *every_secs,
            _ => DEFAULT_WAITFOR_EVERY_S,
        };
        let next = now
            .saturating_add(self.secs_to_cycles(every))
            .min(self.wait_overall_deadline);
        self.phase = Phase::WaitSleep;
        self.next_deadline = Some(next);
    }

    /// Finalize a completed (or aborted) run: emit `ff_stats` + `run_end`, write
    /// the JSON report, print the human summary. Returns the process exit code.
    pub fn finalize(&mut self, ff: &FfSummary, wall_s: f64, now: u64) -> i32 {
        // A run that stopped without a decision (shouldn't happen) is an error.
        if self.outcome.is_none() {
            self.record_abort(now, "run ended without a scenario verdict");
        }
        let out = self.outcome.take().unwrap();

        self.logger.event(
            now,
            "ff_stats",
            json!({
                "jumps": ff.jumps, "speedup": ff.speedup,
                "virtual_seconds": ff.virtual_seconds,
                "per_hop_mean_us": ff.per_hop_mean_us,
                "max_delta_s": ff.max_delta_s,
            }),
        );

        let steps_passed = self
            .step_reports
            .iter()
            .filter(|s| s.outcome == "pass")
            .count();
        let steps_total = self.scn.steps.len();

        self.logger.event(
            now,
            "run_end",
            json!({
                "verdict": out.verdict, "exit_code": out.exit_code,
                "steps_total": steps_total, "steps_passed": steps_passed,
                "duration_wall_s": round3(wall_s),
                "virtual_seconds": round3(ff.virtual_seconds),
                "failure": out.failure,
            }),
        );

        let report = Report {
            schema: SCHEMA_VERSION,
            verdict: out.verdict.to_string(),
            exit_code: out.exit_code,
            stack: self.meta.stack.clone(),
            artifact_sha256: self.meta.artifact_sha256.clone(),
            scenario: self.scn.source_path.clone(),
            scenario_sha256: self.scn.source_sha256.clone(),
            fast_forward: self.meta.fast_forward,
            duration_wall_s: round3(wall_s),
            virtual_seconds: round3(ff.virtual_seconds),
            ff: ff.clone(),
            steps_total,
            steps_passed,
            steps: self.step_reports.clone(),
            failure: out.failure.clone(),
        };
        match serde_json::to_vec_pretty(&report) {
            Ok(mut v) => {
                v.push(b'\n');
                if let Err(e) = std::fs::write(&self.meta.report_path, &v) {
                    crate::log_line(format_args!(
                        "[dvmm][WARN] could not write report {}: {e}",
                        self.meta.report_path
                    ));
                }
            }
            Err(e) => crate::log_line(format_args!("[dvmm][WARN] report serialize: {e}")),
        }

        self.print_summary(&report);
        out.exit_code
    }

    fn print_summary(&self, report: &Report) {
        let l = |a: std::fmt::Arguments| crate::log_line(a);
        l(format_args!(""));
        l(format_args!(
            "==== dvmm test: {} ({}) ====",
            self.scn.name, self.meta.stack
        ));
        l(format_args!(
            "  {:<4} {:<10} {:<28} {:<8} {}",
            "#", "kind", "step", "result", "detail"
        ));
        for s in &report.steps {
            l(format_args!(
                "  {:<4} {:<10} {:<28} {:<8} {}",
                s.index,
                s.kind,
                truncate(&s.name, 28),
                s.outcome.to_uppercase(),
                truncate(&s.detail, 60),
            ));
        }
        l(format_args!(
            "  steps: {}/{} passed | virtual {:.1}s in {:.1}s wall | speedup {:.0}x | {} jumps",
            report.steps_passed,
            report.steps_total,
            report.virtual_seconds,
            report.duration_wall_s,
            report.ff.speedup,
            report.ff.jumps,
        ));
        if let Some(f) = &report.failure {
            l(format_args!("  FAILURE: {f}"));
        }
        l(format_args!(
            "  VERDICT: {} (exit {})",
            report.verdict.to_uppercase(),
            report.exit_code
        ));
        l(format_args!(
            "  report: {}  |  jsonl: {}",
            self.meta.report_path, self.meta.jsonl_path
        ));
    }
}

fn step_kind_str(k: &PreparedKind) -> &'static str {
    match k {
        PreparedKind::Exec { .. } => "exec",
        PreparedKind::Containers { .. } => "containers",
        PreparedKind::WaitFor { .. } => "wait_for",
        PreparedKind::Action { op, .. } => op,
    }
}

// ---- assertion / predicate evaluation --------------------------------------

fn eval_exec_assertion(expect: &PreparedExpect, reply: &AgentLine) -> (bool, String) {
    let exit = reply.exit.unwrap_or(-1);
    // Output matchers run against the TRIMMED stdout: command output (psql, curl,
    // ...) almost always carries a trailing newline, and Rust's `$` does not match
    // before one — so `^[0-9]+$` on "5\n" would surprise every author. Trimming
    // outer whitespace is the intuitive, documented convention.
    let stdout = reply.stdout.clone().unwrap_or_default();
    let out = stdout.trim();
    let mut ok = true;
    let mut notes: Vec<String> = Vec::new();

    if exit == expect.exit {
        notes.push(format!("exit={exit} ✓"));
    } else {
        ok = false;
        notes.push(format!("exit={exit} (want {}) ✗", expect.exit));
    }
    if let Some(re) = &expect.output_matches {
        if re.is_match(out) {
            notes.push(format!("output~=/{}/ ✓", re.as_str()));
        } else {
            ok = false;
            notes.push(format!(
                "output~=/{}/ ✗ (got {:?})",
                re.as_str(),
                truncate(out, 40)
            ));
        }
    }
    if let Some(sub) = &expect.output_contains {
        if out.contains(sub) {
            notes.push(format!("contains {sub:?} ✓"));
        } else {
            ok = false;
            notes.push(format!("contains {sub:?} ✗"));
        }
    }
    let _ = &expect.desc;
    (ok, notes.join(", "))
}

fn eval_containers_assertion(
    assert: ContainersAssert,
    reply: &AgentLine,
    expect_death: &HashSet<String>,
) -> (bool, String) {
    let empty = Vec::new();
    let list = reply.containers.as_ref().unwrap_or(&empty);
    match assert {
        ContainersAssert::AllRunning => {
            let bad: Vec<String> = list
                .iter()
                .filter(|c| c.state != "running")
                .map(|c| format!("{}={}", disp_name(c), c.state))
                .collect();
            if list.is_empty() {
                (false, "all_running ✗ (no containers)".into())
            } else if bad.is_empty() {
                (true, format!("all_running ✓ ({} containers)", list.len()))
            } else {
                (false, format!("all_running ✗ ({})", bad.join(", ")))
            }
        }
        ContainersAssert::NoneExitedNonzero => {
            // A container that exited nonzero is only a violation if its death was
            // NOT expected (TEST-1b): a deliberately killed/stopped service listed
            // in `expect_death` is exempt.
            let bad: Vec<String> = list
                .iter()
                .filter(|c| {
                    c.state == "exited" && c.exit_code != 0 && !expect_death.contains(&c.service)
                })
                .map(|c| format!("{}=exit{}", disp_name(c), c.exit_code))
                .collect();
            if bad.is_empty() {
                (true, format!("none_exited_nonzero ✓ ({} containers)", list.len()))
            } else {
                (false, format!("none_exited_nonzero ✗ ({})", bad.join(", ")))
            }
        }
    }
}

/// The first container that exited nonzero and is NOT in the expected-death set,
/// or `None` if every nonzero exit was expected. Backs the implicit end-of-run
/// census (TEST-1b expected-death policy).
fn check_unexpected_deaths(
    list: &[ContainerInfo],
    expect_death: &HashSet<String>,
) -> Option<String> {
    list.iter()
        .find(|c| c.state == "exited" && c.exit_code != 0 && !expect_death.contains(&c.service))
        .map(|c| format!("{}=exit{}", disp_name(c), c.exit_code))
}

fn eval_until(
    until: &PreparedUntil,
    reply: &AgentLine,
    expect_death: &HashSet<String>,
) -> (bool, String) {
    // A probe whose command could not run (ok:false) is simply "not ready yet".
    let ok = reply.ok == Some(true);
    match until {
        PreparedUntil::ExitZero => {
            let e = reply.exit.unwrap_or(-1);
            (ok && e == 0, format!("exit_zero (ok={ok} exit={e})"))
        }
        PreparedUntil::ExitNonzero => {
            let e = reply.exit.unwrap_or(-1);
            (ok && e != 0, format!("exit_nonzero (ok={ok} exit={e})"))
        }
        PreparedUntil::OutputMatches(re) => {
            let s = reply.stdout.clone().unwrap_or_default();
            (ok && re.is_match(s.trim()), format!("output_matches /{}/", re.as_str()))
        }
        PreparedUntil::OutputContains(sub) => {
            let s = reply.stdout.clone().unwrap_or_default();
            (ok && s.trim().contains(sub), format!("output_contains {sub:?}"))
        }
        PreparedUntil::AllRunning => {
            let (r, d) = eval_containers_assertion(ContainersAssert::AllRunning, reply, expect_death);
            (ok && r, d)
        }
        PreparedUntil::NoneExitedNonzero => {
            let (r, d) =
                eval_containers_assertion(ContainersAssert::NoneExitedNonzero, reply, expect_death);
            (ok && r, d)
        }
    }
}

fn disp_name(c: &ContainerInfo) -> String {
    if !c.service.is_empty() {
        c.service.clone()
    } else {
        c.name.clone()
    }
}

// ---- JSONL logger ----------------------------------------------------------

struct Logger {
    file: File,
    freq: TscFrequency,
    t0: u64,
    start: Instant,
}

impl Logger {
    fn new(path: &str, freq: TscFrequency) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self {
            file,
            freq,
            t0: 0,
            start: Instant::now(),
        })
    }
    fn set_t0(&mut self, t0: u64) {
        self.t0 = t0;
    }
    fn event(&mut self, now: u64, typ: &str, mut payload: Value) {
        let obj = payload.as_object_mut();
        let t_s = now.saturating_sub(self.t0) as f64 / self.freq.hz() as f64;
        let mut line = serde_json::Map::new();
        line.insert("schema".into(), json!(SCHEMA_VERSION));
        line.insert("ts_vtsc".into(), json!(now));
        line.insert("t_s".into(), json!(round3(t_s)));
        line.insert("wall_ms".into(), json!(self.start.elapsed().as_millis() as u64));
        line.insert("type".into(), json!(typ));
        if let Some(o) = obj {
            for (k, v) in o.iter() {
                line.insert(k.clone(), v.clone());
            }
        }
        let mut bytes = serde_json::to_vec(&Value::Object(line)).unwrap_or_default();
        bytes.push(b'\n');
        let _ = self.file.write_all(&bytes);
        let _ = self.file.flush();
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The HOST side of the shared golden round-trip (Fable §3): the committed
    /// `dvmm-proto` fixtures decode + re-encode identically through the host's own
    /// wire path (`Request`/`Reply` + `encode_line`/`decode_line`). With the
    /// matching tests in `dvmm-proto` and `dvmm-agent`, every request/response
    /// variant is exercised by BOTH the host and agent code paths.
    #[test]
    fn host_roundtrips_proto_goldens() {
        use serde_json::Value;
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dvmm-proto/goldens");
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

    fn svc() -> HashSet<String> {
        ["postgres", "service"].iter().map(|s| s.to_string()).collect()
    }

    fn scn_from(yaml: &str) -> Result<Scenario, ScenarioError> {
        let dir = std::env::temp_dir();
        let p = dir.join(format!(
            "dvmm-scn-test-{}-{:p}.yml",
            std::process::id(),
            yaml
        ));
        std::fs::write(&p, yaml).unwrap();
        Scenario::load_and_validate(p.to_str().unwrap(), &svc())
    }

    /// The error message of an expected-to-fail scenario (avoids requiring
    /// `Scenario: Debug` for `unwrap_err`).
    fn err_of(yaml: &str) -> String {
        match scn_from(yaml) {
            Ok(_) => panic!("expected scenario to be rejected, but it validated"),
            Err(e) => e.0,
        }
    }

    #[test]
    fn rejects_unknown_key() {
        let e = err_of("steps:\n  - at: 1s\n    bogus: 1\n");
        assert!(e.contains("bogus") || e.to_lowercase().contains("unknown"), "{}", e);
    }

    #[test]
    fn rejects_unknown_service() {
        let y = "steps:\n  - at: 1s\n    exec:\n      container: nope\n      cmd: \"true\"\n";
        let e = err_of(y);
        assert!(e.contains("nope"), "{}", e);
    }

    #[test]
    fn rejects_bad_duration() {
        let y = "steps:\n  - at: 5furlongs\n    exec:\n      container: service\n      cmd: \"true\"\n";
        let e = err_of(y);
        assert!(e.contains("duration"), "{}", e);
    }

    #[test]
    fn rejects_bad_regex() {
        let y = "steps:\n  - exec:\n      container: service\n      cmd: \"true\"\n    expect:\n      output_matches: \"(\"\n";
        let e = err_of(y);
        assert!(e.contains("regex"), "{}", e);
    }

    #[test]
    fn rejects_multiple_kinds() {
        let y = "steps:\n  - exec:\n      container: service\n      cmd: \"true\"\n    containers: all_running\n";
        let e = err_of(y);
        assert!(e.contains("exactly one"), "{}", e);
    }

    #[test]
    fn accepts_valid_scenario_and_defaults_exit_zero() {
        let y = "\
name: t
run:
  cmdline: \"console=ttyS0 dvmm.maxrows=5\"
steps:
  - name: wait
    at: 0s
    wait_for:
      probe:
        exec:
          container: service
          cmd: \"pg_isready -q\"
      until: exit_zero
      every: 30s
      timeout: 5m
  - name: rows
    at: 2h
    exec:
      container: service
      cmd: \"psql -tAc 'select count(*) from events;'\"
    expect:
      output_matches: '^[0-9]+$'
";
        let s = scn_from(y).unwrap();
        assert_eq!(s.steps.len(), 2);
        assert_eq!(s.run.cmdline.as_deref(), Some("console=ttyS0 dvmm.maxrows=5"));
        // horizon covers the last step's at (2h) + slack.
        assert!(s.implicit_horizon_secs() > 2.0 * 3600.0);
    }

    #[test]
    fn exec_assertion_exit_and_regex() {
        let expect = PreparedExpect {
            exit: 0,
            output_matches: Some(Regex::new("^5$").unwrap()),
            output_contains: None,
            desc: String::new(),
        };
        let good = AgentLine {
            id: Some(1),
            ok: Some(true),
            exit: Some(0),
            stdout: Some("5\n".into()),
            ..Default::default()
        };
        let (p, _) = eval_exec_assertion(&expect, &good);
        assert!(p);
        let bad = AgentLine {
            id: Some(1),
            ok: Some(true),
            exit: Some(0),
            stdout: Some("6\n".into()),
            ..Default::default()
        };
        let (p, d) = eval_exec_assertion(&expect, &bad);
        assert!(!p, "{d}");
    }

    #[test]
    fn containers_all_running() {
        let reply = AgentLine {
            id: Some(1),
            ok: Some(true),
            containers: Some(vec![
                ContainerInfo { name: "a".into(), service: "postgres".into(), state: "running".into(), exit_code: 0, health: String::new() },
                ContainerInfo { name: "b".into(), service: "service".into(), state: "running".into(), exit_code: 0, health: String::new() },
            ]),
            ..Default::default()
        };
        let none = HashSet::new();
        assert!(eval_containers_assertion(ContainersAssert::AllRunning, &reply, &none).0);
        let reply2 = AgentLine {
            id: Some(1),
            ok: Some(true),
            containers: Some(vec![
                ContainerInfo { name: "b".into(), service: "service".into(), state: "exited".into(), exit_code: 1, health: String::new() },
            ]),
            ..Default::default()
        };
        assert!(!eval_containers_assertion(ContainersAssert::AllRunning, &reply2, &none).0);
        assert!(!eval_containers_assertion(ContainersAssert::NoneExitedNonzero, &reply2, &none).0);
        // But if `service`'s death is EXPECTED, none_exited_nonzero passes.
        let expect: HashSet<String> = ["service".to_string()].into_iter().collect();
        assert!(eval_containers_assertion(ContainersAssert::NoneExitedNonzero, &reply2, &expect).0);
    }

    #[test]
    fn accepts_fault_action_steps() {
        let y = "\
name: faults
expect_death: [postgres]
steps:
  - at: 1h
    kill: postgres
  - at: 2h
    start: postgres
  - at: 3h
    partition: [postgres, service]
  - at: 4h
    heal: [postgres, service]
  - at: 5h
    heal: all
";
        let s = scn_from(y).unwrap();
        assert_eq!(s.steps.len(), 5);
        assert!(s.death_policy, "kill + expect_death must enable the death policy");
        assert!(s.expect_death.contains("postgres"));
    }

    #[test]
    fn rejects_fault_unknown_service() {
        let e = err_of("steps:\n  - at: 1h\n    kill: nope\n");
        assert!(e.contains("nope"), "{}", e);
        let e = err_of("steps:\n  - at: 1h\n    partition: [service, nope]\n");
        assert!(e.contains("nope"), "{}", e);
    }

    #[test]
    fn rejects_expect_death_unknown_service() {
        let e = err_of("expect_death: [nope]\nsteps:\n  - at: 1h\n    kill: service\n");
        assert!(e.contains("nope"), "{}", e);
    }

    #[test]
    fn rejects_partition_wrong_arity_and_action_plus_kind() {
        let e = err_of("steps:\n  - at: 1h\n    partition: [service]\n");
        assert!(e.contains("two services"), "{}", e);
        // exec + a fault action on the same step -> not exactly one kind.
        let e = err_of(
            "steps:\n  - at: 1h\n    kill: service\n    exec: { container: service, cmd: \"true\" }\n",
        );
        assert!(e.contains("exactly one"), "{}", e);
    }

    #[test]
    fn check_unexpected_deaths_honors_expect_death() {
        let list = vec![
            ContainerInfo { name: "p".into(), service: "postgres".into(), state: "exited".into(), exit_code: 137, health: String::new() },
            ContainerInfo { name: "s".into(), service: "service".into(), state: "running".into(), exit_code: 0, health: String::new() },
        ];
        let none = HashSet::new();
        assert!(check_unexpected_deaths(&list, &none).is_some(), "unexpected death must be flagged");
        let expect: HashSet<String> = ["postgres".to_string()].into_iter().collect();
        assert!(check_unexpected_deaths(&list, &expect).is_none(), "expected death must be exempt");
    }
}
