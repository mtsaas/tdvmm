//! The guest-assertion ledger: folds `GuestEvent`s bridged from the workload
//! into the shared 0/1/2 verdict. Empty for a run that sees no events, so an
//! event-free scenario's verdict/report/JSONL are byte-for-byte unchanged.

use std::collections::BTreeMap;

use serde::Serialize;

use dvmm_proto::GuestEvent;

/// Accumulates guest→host assertion events into a verdict. Empty for a run that
/// sees no events, so an event-free scenario's verdict/report/JSONL are unchanged.
#[derive(Default)]
pub(super) struct AssertionLedger {
    pub(super) events: u64,
    pub(super) always_total: u64,
    /// Names of `always` events that reported `ok:false`.
    pub(super) always_failed: Vec<String>,
    /// `sometimes` name → whether it was ever satisfied (`ok:true`).
    pub(super) sometimes: BTreeMap<String, bool>,
    pub(super) faults: u64,
    pub(super) invalid: u64,
    pub(super) done: bool,
    last_seq: Option<u64>,
    pub(super) seq_gaps: u64,
}

impl AssertionLedger {
    pub(super) fn is_empty(&self) -> bool {
        self.events == 0
    }

    pub(super) fn record(&mut self, ev: &GuestEvent) {
        self.events += 1;
        match ev.kind.as_str() {
            "always" => {
                self.always_total += 1;
                if ev.ok == Some(false) {
                    self.always_failed.push(ev.name.clone());
                }
            }
            "sometimes" => {
                let sat = self.sometimes.entry(ev.name.clone()).or_insert(false);
                if ev.ok == Some(true) {
                    *sat = true;
                }
            }
            "fault" => self.faults += 1,
            "done" => self.done = true,
            _ => self.invalid += 1, // "invalid" and any unknown kind: recorded, non-verdict.
        }
    }

    /// Fold into the shared 0/1/2 verdict. Empty ledger ⇒ ("pass", 0, None),
    /// byte-identical to a run with no events.
    pub(super) fn verdict(&self) -> (&'static str, i32, Option<String>) {
        if !self.always_failed.is_empty() {
            return (
                "fail",
                1,
                Some(format!(
                    "assertion(s) failed: always {:?}",
                    self.always_failed
                )),
            );
        }
        let unsatisfied: Vec<&String> =
            self.sometimes.iter().filter(|(_, s)| !**s).map(|(n, _)| n).collect();
        if !unsatisfied.is_empty() {
            return (
                "fail",
                1,
                Some(format!("sometimes assertion(s) never satisfied: {unsatisfied:?}")),
            );
        }
        ("pass", 0, None)
    }

    /// Record a bridged event's sequence number and return how many lines the wire
    /// dropped since the previous one (a gap greater than one). A `seq` that does
    /// not advance is treated as an agent restart: tracking resets, no gap reported.
    pub(super) fn observe_seq(&mut self, s: u64) -> Option<u64> {
        let dropped = self
            .last_seq
            .and_then(|prev| s.checked_sub(prev))
            .filter(|&delta| delta > 1)
            .map(|delta| delta - 1);
        self.last_seq = Some(s);
        if let Some(d) = dropped {
            self.seq_gaps += d;
        }
        dropped
    }

    pub(super) fn summary(&self) -> AssertionSummary {
        AssertionSummary {
            always_failed: self.always_failed.clone(),
            always_total: self.always_total,
            events: self.events,
            faults: self.faults,
            invalid: self.invalid,
            seq_gaps: self.seq_gaps,
            sometimes_satisfied: self.sometimes.values().filter(|s| **s).count() as u64,
            sometimes_total: self.sometimes.len() as u64,
        }
    }
}

/// The guest-assertion summary embedded in `run_end` and the report (schema 3+).
/// Fields are ordered alphabetically so the typed report struct and the
/// `serde_json::Value` copy in the JSONL serialize to identical bytes under
/// serde_json's sorted-key default.
#[derive(Serialize)]
pub(super) struct AssertionSummary {
    pub(super) always_failed: Vec<String>,
    pub(super) always_total: u64,
    pub(super) events: u64,
    pub(super) faults: u64,
    pub(super) invalid: u64,
    pub(super) seq_gaps: u64,
    pub(super) sometimes_satisfied: u64,
    pub(super) sometimes_total: u64,
}

#[cfg(test)]
mod tests {
    use super::AssertionLedger;
    use crate::scenario::testutil::ev;

    #[test]
    fn ledger_verdict_folds_assertions() {
        // Empty ledger is a pass — the byte-identical event-free case.
        let l = AssertionLedger::default();
        assert!(l.is_empty());
        assert_eq!(l.verdict(), ("pass", 0, None));

        // always ok:false -> fail(1), naming the failed assertion.
        let mut l = AssertionLedger::default();
        l.record(&ev("always", "a", Some(true)));
        l.record(&ev("always", "b", Some(false)));
        let (v, c, f) = l.verdict();
        assert_eq!((v, c), ("fail", 1));
        assert!(f.unwrap().contains('b'));

        // sometimes registered but never satisfied -> fail(1); then satisfied -> pass.
        let mut l = AssertionLedger::default();
        l.record(&ev("sometimes", "s", Some(false)));
        assert_eq!(l.verdict().1, 1);
        l.record(&ev("sometimes", "s", Some(true)));
        assert_eq!(l.verdict(), ("pass", 0, None));
        assert!(!l.is_empty());

        // fault / invalid recorded but non-verdict-affecting.
        let mut l = AssertionLedger::default();
        l.record(&ev("fault", "", None));
        l.record(&ev("invalid", "", None));
        assert_eq!(l.verdict(), ("pass", 0, None));
        assert_eq!((l.faults, l.invalid), (1, 1));
    }
    #[test]
    fn observe_seq_counts_gaps_and_survives_reset() {
        // Regression C2: `seq` is guest-controlled and must never underflow.
        let mut l = AssertionLedger::default();
        assert_eq!(l.observe_seq(u64::MAX), None); // first observation: no gap
        assert_eq!(l.observe_seq(1), None); // reset (agent restart): no gap, no panic
        assert_eq!(l.seq_gaps, 0);

        let mut l = AssertionLedger::default();
        l.observe_seq(1);
        l.observe_seq(2); // consecutive: no gap
        assert_eq!(l.observe_seq(5), Some(2)); // 3, 4 dropped
        assert_eq!(l.seq_gaps, 2);
    }
}
