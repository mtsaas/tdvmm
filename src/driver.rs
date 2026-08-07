//! Host-side driver-run state: the verdict a container declared over the guest
//! control socket ([`tdvmm_proto::OP_FINISH`]), and its map onto `tdvmm run`'s
//! exit code. Inert until a container drives; a plain `run` is unaffected.
//!
//! The raw verdict code is collapsed onto the exit contract and kept verbatim in
//! the run summary and `--metrics-out`:
//!
//! | verdict | `run` exits |
//! |---|---|
//! | `finish(0)` | 0 — PASS |
//! | `finish(n)`, n ≠ 0 | 1 — FAIL (raw `n` reported, not returned) |
//! | `finish` with no `exit` | 1 — FAIL; graded infra, never a pass |
//!
//! A run with no `finish` exits on its [`StopReason`](crate::exit::StopReason).

use tdvmm_proto::{decode_line, GuestEvent, Reply};

use crate::exit::{EXIT_FAIL, EXIT_INFRA, EXIT_PASS};

/// The verdict a container declared with `finish`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Verdict {
    /// The raw code the driver supplied (0 = pass). Reported, not returned.
    pub(crate) code: i64,
    /// The driver's optional one-line reason, for the run summary.
    pub(crate) message: Option<String>,
}

impl Verdict {
    /// This verdict as a process exit code: 0 for a passing verdict, else FAIL(1).
    pub(crate) fn exit_code(&self) -> i32 {
        if self.code == 0 {
            EXIT_PASS
        } else {
            EXIT_FAIL
        }
    }

    /// `PASS` / `FAIL`, for the summary line.
    pub(crate) fn label(&self) -> &'static str {
        if self.code == 0 {
            "PASS"
        } else {
            "FAIL"
        }
    }
}

/// Accumulates what the guest agent reports for a run: whether the agent came up,
/// the control-socket command trace, and the terminal verdict. Inert until a
/// container drives.
#[derive(Default)]
pub(crate) struct DriverRun {
    /// The hello arrived: the agent (and the control socket) is live.
    agent_ready: bool,
    /// The FIRST `finish`. The guard makes "first finish wins" hold host-side even
    /// if the agent were to emit a second.
    verdict: Option<Verdict>,
    /// Control-socket commands seen, for the end-of-run summary.
    commands: u64,
    /// Whether any control-socket command failed (a mis-targeted fault, usually).
    failed_commands: u64,
}

impl DriverRun {
    /// Fold one agent line (a [`Reply`] off ttyS1) into the run state. Returns
    /// `true` once a verdict has arrived and the run must stop. `now_virtual_s` is
    /// the virtual timestamp the line was observed at, used for the fault trace.
    pub(crate) fn on_agent_line(&mut self, line: &[u8], now_virtual_s: f64) -> bool {
        let Ok(reply) = decode_line::<Reply>(line) else {
            // Non-JSON noise on ttyS1 (kernel chatter): ignore.
            return false;
        };
        if reply.is_hello() {
            self.agent_ready = true;
            if let Some(s) = reply.schema {
                if s != tdvmm_proto::SCHEMA {
                    crate::log_line(format_args!(
                        "[tdvmm][WARN] agent proto schema {s} != host {} \
                         (host+agent should ship in lockstep)",
                        tdvmm_proto::SCHEMA
                    ));
                }
            }
            return false;
        }
        match reply.event {
            Some(ev) => self.on_event(&ev, now_virtual_s),
            None => false,
        }
    }

    fn on_event(&mut self, ev: &GuestEvent, now_virtual_s: f64) -> bool {
        match ev.kind.as_str() {
            tdvmm_proto::OP_FINISH => {
                if self.verdict.is_some() {
                    return true; // already decided; stay decided.
                }
                let verdict = Verdict {
                    // A `finish` with no `exit` is malformed; grade it as infra,
                    // never a pass.
                    code: ev.exit.unwrap_or(i64::from(EXIT_INFRA)),
                    message: Some(ev.name.clone()).filter(|m| !m.is_empty()),
                };
                crate::log_line(format_args!(
                    "[tdvmm][driver] t+{now_virtual_s:.1}s virtual: finish {} ({}){}",
                    verdict.code,
                    verdict.label(),
                    match &verdict.message {
                        Some(m) => format!(" — {m}"),
                        None => String::new(),
                    },
                ));
                self.verdict = Some(verdict);
                true
            }
            "ctl" => {
                self.commands += 1;
                let ok = ev.ok == Some(true);
                if !ok {
                    self.failed_commands += 1;
                }
                let target = ev
                    .details
                    .as_ref()
                    .map(|d| {
                        match (d.get("service").and_then(|v| v.as_str()), d.get("peer").and_then(|v| v.as_str())) {
                            (Some(a), Some(b)) => format!(" {a} <-> {b}"),
                            (Some(a), None) => format!(" {a}"),
                            _ => String::new(),
                        }
                    })
                    .unwrap_or_default();
                // One trace line per command, stamped with the virtual time it landed.
                crate::log_line(format_args!(
                    "[tdvmm][driver] t+{now_virtual_s:.1}s virtual: {}{target}{}",
                    ev.name,
                    if ok {
                        String::new()
                    } else {
                        let why = ev
                            .details
                            .as_ref()
                            .and_then(|d| d.get("error"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("failed");
                        format!(" — FAILED: {why}")
                    },
                ));
                false
            }
            // Workload assertion events (schema 3) are ignored here; only `finish`
            // decides the verdict.
            _ => false,
        }
    }

    /// The declared verdict, if a container finished the run.
    pub(crate) fn verdict(&self) -> Option<&Verdict> {
        self.verdict.as_ref()
    }

    /// Whether this run was driven at all (a container used the control socket).
    pub(crate) fn was_driven(&self) -> bool {
        self.verdict.is_some() || self.commands > 0
    }

    /// The end-of-run summary line, or `None` for a run nobody drove (whose
    /// output must stay byte-identical to a plain `run`).
    pub(crate) fn summary(&self) -> Option<String> {
        if !self.was_driven() {
            return None;
        }
        let verdict = match &self.verdict {
            Some(v) => format!("{} (finish {})", v.label(), v.code),
            // Driven but never finished: the horizon or the wall timeout ended it.
            None => "NO VERDICT (no container called finish)".to_string(),
        };
        Some(format!(
            "==== tdvmm driver: {verdict} | {} control command(s), {} failed | agent {} ====",
            self.commands,
            self.failed_commands,
            if self.agent_ready { "ready" } else { "NEVER READY" },
        ))
    }

    /// The `--metrics-out` block for a driven run: the raw verdict code (the only
    /// place it survives the exit-code collapse) plus the command counts. Empty
    /// for an undriven run.
    pub(crate) fn metrics_block(&self) -> String {
        if !self.was_driven() {
            return String::new();
        }
        let mut out = String::from("driver yes\n");
        match &self.verdict {
            Some(v) => {
                out.push_str(&format!("driver_verdict {}\n", v.label().to_lowercase()));
                out.push_str(&format!("driver_exit_raw {}\n", v.code));
                if let Some(m) = &v.message {
                    out.push_str(&format!("driver_message {m}\n"));
                }
            }
            None => out.push_str("driver_verdict none\n"),
        }
        out.push_str(&format!("driver_commands {}\n", self.commands));
        out.push_str(&format!("driver_commands_failed {}\n", self.failed_commands));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tdvmm_proto::encode_line;

    fn line(reply: &Reply) -> Vec<u8> {
        encode_line(reply).unwrap()
    }

    fn finish(exit: Option<i64>, message: &str) -> Vec<u8> {
        line(&Reply::from_event(
            1,
            GuestEvent {
                kind: "finish".into(),
                name: message.into(),
                exit,
                ..Default::default()
            },
        ))
    }

    #[test]
    fn a_passing_finish_stops_the_run_with_exit_zero() {
        let mut d = DriverRun::default();
        assert!(d.on_agent_line(&finish(Some(0), ""), 12.5), "finish stops the run");
        let v = d.verdict().unwrap();
        assert_eq!(v.exit_code(), 0);
        assert_eq!(v.label(), "PASS");
    }

    #[test]
    fn a_failing_finish_collapses_onto_exit_one() {
        // The mapping decision: a driver's nonzero code becomes 1, so it can never
        // be confused with 2 (infra) or 3 (horizon). The raw code survives in the
        // verdict for the summary and the metrics file.
        for raw in [1, 2, 3, 42, 137] {
            let mut d = DriverRun::default();
            assert!(d.on_agent_line(&finish(Some(raw), "boom"), 1.0));
            let v = d.verdict().unwrap();
            assert_eq!(v.exit_code(), 1, "driver exit {raw} must map to FAIL(1)");
            assert_eq!(v.code, raw, "the raw code is preserved for reporting");
            assert_eq!(v.message.as_deref(), Some("boom"));
        }
    }

    #[test]
    fn a_malformed_finish_is_infra_not_a_pass() {
        // "No news" must never launder into "good news".
        let mut d = DriverRun::default();
        assert!(d.on_agent_line(&finish(None, ""), 1.0));
        let v = d.verdict().unwrap();
        assert_eq!(v.code, 2);
        assert_eq!(v.exit_code(), 1, "still a failure, never a pass");
    }

    #[test]
    fn an_undriven_run_is_inert_and_silent() {
        // The property that keeps a plain `tdvmm run` unchanged: no driver, no
        // verdict, no summary line.
        let mut d = DriverRun::default();
        assert!(!d.on_agent_line(&line(&Reply::hello("tdvmm-agent/1", "abc")), 0.0));
        assert!(!d.on_agent_line(b"[    0.1] some kernel noise", 0.0));
        assert!(!d.on_agent_line(
            &line(&Reply { id: Some(1), ok: Some(true), ..Default::default() }),
            0.0
        ));
        assert!(!d.was_driven());
        assert!(d.verdict().is_none());
        assert!(d.summary().is_none());
    }

    #[test]
    fn control_commands_are_traced_and_counted() {
        let mut d = DriverRun::default();
        let ctl = |name: &str, ok: bool| {
            line(&Reply::from_event(
                1,
                GuestEvent {
                    kind: "ctl".into(),
                    name: name.into(),
                    ok: Some(ok),
                    details: Some(serde_json::json!({ "service": "a", "peer": "b" })),
                    ..Default::default()
                },
            ))
        };
        assert!(!d.on_agent_line(&ctl("partition", true), 1.0));
        assert!(!d.on_agent_line(&ctl("kill", false), 2.0));
        assert!(d.was_driven(), "a run with control commands is a driven run");
        let s = d.summary().unwrap();
        assert!(s.contains("2 control command(s), 1 failed"), "{s}");
        // Driven but never finished — the summary must say so rather than imply a pass.
        assert!(s.contains("NO VERDICT"), "{s}");
    }

    #[test]
    fn metrics_block_preserves_the_raw_code_the_exit_code_collapses() {
        let mut d = DriverRun::default();
        d.on_agent_line(&finish(Some(137), "replica never rejoined"), 3.0);
        let m = d.metrics_block();
        assert!(m.contains("driver_verdict fail"), "{m}");
        assert!(m.contains("driver_exit_raw 137"), "{m}");
        assert!(m.contains("driver_message replica never rejoined"), "{m}");
        // An undriven run must not perturb the metrics file at all.
        assert_eq!(DriverRun::default().metrics_block(), "");
    }

    #[test]
    fn a_second_finish_cannot_change_the_verdict() {
        let mut d = DriverRun::default();
        assert!(d.on_agent_line(&finish(Some(0), "first"), 1.0));
        assert!(d.on_agent_line(&finish(Some(1), "second"), 2.0));
        assert_eq!(d.verdict().unwrap().code, 0, "first finish wins, host-side too");
    }
}
