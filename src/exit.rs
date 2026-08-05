//! Run-stop bookkeeping: why the vCPU loop stopped, and the process exit code
//! that maps to.
//!
//! Process exit codes. A testing platform wants the *cause* of a stop to be a
//! first-class, machine-readable outcome, so each distinct stop reason maps to a
//! distinct code (see `StopReason` and the shutdown-cause logging near the exit
//! handlers).

/// Guest-initiated stop: the guest shut down or rebooted on its own (triple
/// fault / system event, e.g. panic+reboot or `reboot -f`). The "normal" way a
/// test guest ends.
pub(crate) const EXIT_GUEST_STOP: i32 = 0;
/// A VMM policy stop: `--max-virtual-time` horizon reached. Distinct from a
/// guest-initiated stop so a harness can tell "the guest ended" from "we cut it
/// off at the virtual-time budget".
pub(crate) const EXIT_HORIZON: i32 = 3;
/// Infrastructure-error exit code for `tdvmm test` (the CI contract): 0 = all
/// assertions passed, 1 = an assertion / readiness failure (from the scenario
/// verdict), 2 = an infrastructure error (bad scenario, or a boot/bake/agent
/// failure — the tool broke, not your stack).
pub(crate) const EXIT_TEST_INFRA: i32 = 2;

/// Why the run loop stopped. Mapped to a distinct process exit code (above) and
/// logged distinguishably at the stop site (guest-initiated vs VMM policy).
#[derive(Clone, Copy, Debug)]
pub(crate) enum StopReason {
    /// KVM_EXIT_SHUTDOWN — the guest triple-faulted (crash / panic+reboot /
    /// `reboot=t`). Guest-initiated.
    GuestShutdown,
    /// KVM_SYSTEM_EVENT (reset/shutdown/crash). Guest-initiated. The event type
    /// is logged at the stop site; only the guest-vs-VMM distinction matters here.
    GuestSystemEvent,
    /// KVM_EXIT_HLT taken with interrupts disabled (IF=0): a terminal halt that
    /// can NEVER wake, because no interrupt is deliverable. This is where the
    /// guest's `poweroff` ends when there is no ACPI (the kernel finishes in
    /// `cli; hlt`, "System halted"). Guest-initiated, a clean stop (status 0) —
    /// distinct from an ordinary idle `sti; hlt` (IF=1), which parks.
    GuestHalt,
    /// `--max-virtual-time` horizon fired as a `(vtsc, StopRun)` queue event.
    /// VMM policy stop, deterministic in virtual time.
    Horizon,
    /// TEST-1a: the `--scenario` reached a verdict (all steps done, or a failure).
    /// The process exit code comes from the scenario verdict, not `exit_code()`.
    Scenario,
}

impl StopReason {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            StopReason::GuestShutdown
            | StopReason::GuestSystemEvent
            | StopReason::GuestHalt => EXIT_GUEST_STOP,
            StopReason::Horizon => EXIT_HORIZON,
            // Placeholder: a scenario run's real exit code is the verdict's (see
            // `RunOutcome` / `ScenarioEngine::finalize`); this is never used.
            StopReason::Scenario => EXIT_GUEST_STOP,
        }
    }

    /// Stable machine-readable token for the `--metrics-out` file (a harness keys
    /// off this to tell a guest-initiated stop from the VMM's horizon budget).
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            StopReason::GuestShutdown => "guest_shutdown",
            StopReason::GuestSystemEvent => "guest_system_event",
            StopReason::GuestHalt => "guest_halt",
            StopReason::Horizon => "horizon",
            StopReason::Scenario => "scenario",
        }
    }

    /// A plain-language description of the stop, for the human run summary.
    pub(crate) fn human(self) -> &'static str {
        match self {
            StopReason::GuestShutdown => "guest shut down (triple fault or reboot)",
            StopReason::GuestSystemEvent => "guest requested reset or shutdown",
            StopReason::GuestHalt => "guest halted (poweroff)",
            StopReason::Horizon => "--max-virtual-time horizon reached",
            StopReason::Scenario => "scenario reached a verdict",
        }
    }
}

/// The result of a full boot+run: why it stopped, and the process exit code. For
/// `boot`/`run` the code is `stop.exit_code()`; for `test` it is the scenario
/// verdict's code (0 pass / 1 assertion fail / 2 infrastructure).
pub(crate) struct RunOutcome {
    #[allow(dead_code)]
    pub(crate) stop: StopReason,
    pub(crate) exit_code: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_reasons_map_to_distinct_exit_codes() {
        assert_eq!(StopReason::GuestShutdown.exit_code(), EXIT_GUEST_STOP);
        assert_eq!(StopReason::GuestSystemEvent.exit_code(), EXIT_GUEST_STOP);
        // An IF=0 terminal halt (poweroff, no ACPI) is a clean guest stop (0).
        assert_eq!(StopReason::GuestHalt.exit_code(), EXIT_GUEST_STOP);
        assert_eq!(StopReason::Horizon.exit_code(), EXIT_HORIZON);
        // The horizon must be distinguishable from a guest-initiated stop.
        assert_ne!(
            StopReason::Horizon.exit_code(),
            StopReason::GuestShutdown.exit_code()
        );
        assert_ne!(
            StopReason::Horizon.exit_code(),
            StopReason::GuestHalt.exit_code()
        );
    }
}
