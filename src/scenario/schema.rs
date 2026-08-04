//! Scenario schema: the raw YAML deserialize types, static validation, and the
//! prepared (compiled) scenario. [`Scenario::load_and_validate`] fails loudly and
//! fast — before boot — on bad YAML, unknown keys/services, unparseable
//! durations, a bad regex, or a malformed step. The prepared types are consumed
//! by the [`super::engine`] and [`super::eval`].

use std::collections::HashSet;

use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::{
    DEFAULT_AGENT_TIMEOUT_S, DEFAULT_EXEC_TIMEOUT_S, DEFAULT_WAITFOR_EVERY_S,
    DEFAULT_WAITFOR_TIMEOUT_S, HORIZON_SLACK_S,
};

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
    /// `until: done` — after all steps pass, keep the run alive (no timer) until a
    /// guest `done` event arrives or the virtual-time horizon fires, so workload
    /// assertions bridged over the FIFO can accumulate before the verdict.
    #[serde(default)]
    until: Option<String>,
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
pub(crate) enum ContainersAssert {
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
    /// Wait for a guest `done` event before deciding the verdict (from `run.until`).
    pub until_done: bool,
}

pub struct PreparedStep {
    pub(super) display: String,
    pub(super) at_secs: f64,
    pub(super) kind: PreparedKind,
}

pub(super) enum PreparedKind {
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
pub(super) struct ExecReq {
    pub(super) container: String,
    pub(super) argv: Vec<String>,
}

#[derive(Clone)]
pub(super) enum ProbeReq {
    Exec(ExecReq),
    Containers,
}

pub(super) struct PreparedExpect {
    pub(super) exit: i64,
    pub(super) output_matches: Option<Regex>,
    pub(super) output_contains: Option<String>,
}

pub(super) enum PreparedUntil {
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
    ///
    /// # Errors
    /// Returns [`ScenarioError`] on any of the failure modes above.
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
                let expect = PreparedExpect {
                    exit: raw_expect.exit.unwrap_or(0),
                    output_matches,
                    output_contains: raw_expect.output_contains.clone(),
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

        // `run.until`, if present, must be exactly "done" (the only completion mode).
        if let Some(u) = raw.run.as_ref().and_then(|r| r.until.as_deref()) {
            if u != "done" {
                return Err(ScenarioError(format!(
                    "run.until: expected \"done\" (got {u:?})"
                )));
            }
        }
        let run = raw.run.map(|r| ScenarioRun {
            until_done: r.until.as_deref() == Some("done"),
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

pub(super) fn step_kind_str(k: &PreparedKind) -> &'static str {
    match k {
        PreparedKind::Exec { .. } => "exec",
        PreparedKind::Containers { .. } => "containers",
        PreparedKind::WaitFor { .. } => "wait_for",
        PreparedKind::Action { op, .. } => op,
    }
}

#[cfg(test)]
mod tests {
    use crate::scenario::testutil::{err_of, scn_from};

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
    fn run_until_done_parses_and_rejects_junk() {
        let y = "run:\n  until: done\nsteps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n";
        assert!(scn_from(y).unwrap().run.until_done);
        let none = "steps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n";
        assert!(!scn_from(none).unwrap().run.until_done);
        assert!(err_of("run:\n  until: whenever\nsteps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n").contains("until"));
    }
}
