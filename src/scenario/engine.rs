//! The scenario engine: the state machine that drives virtual time through a
//! scenario's steps, issues agent commands at their scheduled vtsc, folds replies
//! and bridged guest events into a verdict, and emits the run-log + report.
//!
//! This is `scenario::engine`, distinct from the top-level [`crate::engine`] (the
//! podman choke point); the type here is [`ScenarioEngine`].

use serde_json::json;

use dvmm_proto::{decode_line, encode_line, GuestEvent, Request};

use crate::control::ControlChannel;
use crate::vtsc::TscFrequency;

use super::eval::{
    check_unexpected_deaths, eval_containers_assertion, eval_exec_assertion, eval_until,
};
use super::ledger::AssertionLedger;
use super::log::Logger;
use super::report::{FfSummary, Report, RunMeta, StepReport};
use super::schema::{step_kind_str, ExecReq, PreparedKind, ProbeReq, Scenario, ScenarioError};
use super::{
    round3, truncate, AgentLine, DEFAULT_AGENT_TIMEOUT_S, DEFAULT_EXEC_TIMEOUT_S,
    DEFAULT_WAITFOR_EVERY_S, SCHEMA_VERSION,
};

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
    /// `run: until: done` — every step passed; the run stays alive with NO timer
    /// (`next_deadline = None`) until a guest `done` event or the horizon fires.
    AwaitDone,
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
    /// Guest→host assertion events folded into the verdict (schema 3+).
    ledger: AssertionLedger,
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
            ledger: AssertionLedger::default(),
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

    /// Fold the assertion ledger into the terminal verdict. An empty ledger yields
    /// ("pass", 0, None) — byte-identical to a run that saw no guest events.
    fn finish_with_ledger(&mut self, now: u64) {
        let (verdict, code, failure) = self.ledger.verdict();
        self.finish(now, verdict, code, failure);
    }

    /// Complete the run once every step (and any final container census) has
    /// passed. With `run: until: done` and no `done` event yet, stay alive in
    /// `AwaitDone` — no timer — so bridged assertions keep accumulating; otherwise
    /// fold the ledger into the terminal verdict now, so a recorded `always:false`
    /// still fails the run rather than a census pass hard-coding exit 0. The single
    /// owner of the completion policy — every "all steps passed" site routes here.
    fn complete_run(&mut self, now: u64) {
        if self.scn.run.until_done && !self.ledger.done {
            self.phase = Phase::AwaitDone;
            self.next_deadline = None;
        } else {
            self.finish_with_ledger(now);
        }
    }

    /// The `--max-virtual-time` horizon fired. In `AwaitDone` (every step already
    /// passed) this is a legitimate completion — evaluate the ledger. In any other
    /// phase the scenario did not complete in time: an infrastructure error (exit
    /// 2), byte-identical to the prior unconditional `record_abort`.
    pub fn on_horizon(&mut self, now: u64) {
        if self.outcome.is_some() {
            return;
        }
        if let Phase::AwaitDone = self.phase {
            // `until: done` reached the horizon without its `done` event. An empty
            // ledger means no guest event ever arrived — the bridge is almost
            // certainly broken (FIFO missing, agent dead, workload crashed), and
            // grading that PASS would launder "no news" into "good news"; treat it
            // as infra error. A non-empty ledger folds normally, but warn.
            if self.ledger.is_empty() {
                self.record_abort(
                    now,
                    "virtual-time horizon reached awaiting `done` with no guest events (bridge broken?)",
                );
            } else {
                crate::log_line(format_args!(
                    "[dvmm][WARN] horizon reached before a `done` event; folding {} guest event(s) into the verdict",
                    self.ledger.events
                ));
                self.finish_with_ledger(now);
            }
        } else {
            self.record_abort(now, "scenario did not complete before the virtual-time horizon");
        }
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
                self.complete_run(now);
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
            self.complete_run(now);
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
            None => self.complete_run(now),
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
                // otherwise-passing run (every step already passed); log and
                // complete through the ledger (which still folds any assertions).
                self.logger.event(
                    now,
                    "final_census",
                    json!({ "ok": false, "error": "final census timed out" }),
                );
                self.complete_run(now);
            }
            // AwaitDone arms no deadline, so on_due is never called for it; the arm
            // exists only for match exhaustiveness.
            Phase::AwaitDone | Phase::Done => {}
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

        // Bridged guest→host assertion event (schema 3+): id-less, `event` set.
        // Handled before the id-gate below, which would otherwise drop it.
        if parsed.is_event() {
            if let Some(ev) = &parsed.event {
                self.on_guest_event(now, ev, parsed.seq);
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

    /// A bridged guest assertion event arrived (id-less line, `event` set). Log it
    /// (vtsc-stamped), fold it into the ledger, and — if `done` arrives while in
    /// `AwaitDone` — decide the verdict. Never verdict-affecting on its own except
    /// through the ledger fold at completion.
    fn on_guest_event(&mut self, now: u64, ev: &GuestEvent, seq: Option<u64>) {
        if let Some(dropped) = seq.and_then(|s| self.ledger.observe_seq(s)) {
            crate::log_line(format_args!(
                "[dvmm][WARN] guest event seq gap: {dropped} line(s) dropped"
            ));
        }
        self.logger.event(
            now,
            "guest_event",
            json!({
                "kind": ev.kind, "name": ev.name, "ok": ev.ok,
                "seq": seq, "details": ev.details,
            }),
        );
        self.ledger.record(ev);
        // A `done` while awaiting it decides the run (fold the ledger).
        if ev.kind == "done" {
            if let Phase::AwaitDone = self.phase {
                self.finish_with_ledger(now);
            }
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

        let mut run_end = json!({
            "verdict": out.verdict, "exit_code": out.exit_code,
            "steps_total": steps_total, "steps_passed": steps_passed,
            "duration_wall_s": round3(wall_s),
            "virtual_seconds": round3(ff.virtual_seconds),
            "failure": out.failure,
        });
        // The assertions block is added ONLY when at least one guest event was
        // seen, so an event-free run's `run_end` line is byte-for-byte unchanged.
        if !self.ledger.is_empty() {
            if let (Some(obj), Ok(summary)) =
                (run_end.as_object_mut(), serde_json::to_value(self.ledger.summary()))
            {
                obj.insert("assertions".into(), summary);
            }
        }
        self.logger.event(now, "run_end", run_end);

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
            assertions: if self.ledger.is_empty() {
                None
            } else {
                Some(self.ledger.summary())
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::testutil::{engine_for, ev};
    use dvmm_proto::Reply;

    #[test]
    fn await_done_at_horizon_folds_ledger_not_infra() {
        // Satisfied `sometimes` in AwaitDone, horizon fires -> pass(0), NOT infra(2).
        let mut e = engine_for("run:\n  until: done\nsteps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n");
        e.phase = Phase::AwaitDone;
        e.on_guest_event(1, &ev("sometimes", "s", Some(true)), Some(1));
        e.on_horizon(2);
        assert!(e.is_finished());
        assert_eq!(e.outcome.as_ref().unwrap().exit_code, 0);

        // always:false in AwaitDone, horizon fires -> fail(1).
        let mut e = engine_for("run:\n  until: done\nsteps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n");
        e.phase = Phase::AwaitDone;
        e.on_guest_event(1, &ev("always", "a", Some(false)), Some(1));
        e.on_horizon(2);
        assert_eq!(e.outcome.as_ref().unwrap().exit_code, 1);
    }

    #[test]
    fn non_await_done_horizon_stays_infra_error() {
        // A normal (no `until`) scenario reaching the horizon mid-run is exit 2 —
        // byte-compatible with the prior unconditional record_abort.
        let mut e = engine_for("steps:\n  - at: 10s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n");
        e.on_horizon(5);
        assert_eq!(e.outcome.as_ref().unwrap().exit_code, 2);
    }

    #[test]
    fn done_event_in_await_done_finishes() {
        let mut e = engine_for("run:\n  until: done\nsteps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n");
        e.phase = Phase::AwaitDone;
        e.on_guest_event(1, &ev("done", "", None), Some(1));
        assert!(e.is_finished());
        assert_eq!(e.outcome.as_ref().unwrap().exit_code, 0);
    }

    #[test]
    fn final_census_pass_folds_failed_assertion() {
        // Regression C1: a passing final census must fold the ledger, not hard-code
        // "pass" — a recorded `always:false` still fails the run (exit 1).
        let mut e = engine_for("steps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n");
        e.on_guest_event(1, &ev("always", "boom", Some(false)), Some(1));
        let census = Reply { ok: Some(true), containers: Some(Vec::new()), ..Default::default() };
        e.on_final_census_reply(2, &census);
        let out = e.outcome.as_ref().unwrap();
        assert_eq!((out.verdict, out.exit_code), ("fail", 1));
    }
    #[test]
    fn await_done_horizon_with_empty_ledger_is_infra_error() {
        // Regression C3: `until: done` reaching the horizon with NO guest events
        // means the bridge delivered nothing — infra error (2), not a false pass.
        let mut e = engine_for("run:\n  until: done\nsteps:\n  - at: 0s\n    exec: { container: service, cmd: \"true\" }\n    expect: { exit: 0 }\n");
        e.phase = Phase::AwaitDone;
        e.on_horizon(2);
        assert_eq!(e.outcome.as_ref().unwrap().exit_code, 2);
    }
}
