//! Run-stop bookkeeping: why the vCPU loop stopped, and the process exit code it
//! maps to. Each distinct stop reason maps to a distinct code (see `StopReason`).
//!
//! The shared 0/1/2/3 contract:
//!
//! * **0** — the run completed cleanly: a driver's `finish(0)`, or a driverless
//!   guest that stopped on its own.
//! * **1** — FAIL: a driver called `finish` with a nonzero verdict. Its raw code
//!   is reported in the summary and `--metrics-out`, not returned (see
//!   [`crate::driver`]).
//! * **2** — infrastructure: a bad artifact, an unreachable agent, or the
//!   wall-clock safety timeout.
//! * **3** — a VMM policy stop: the `--max-virtual-time` horizon fired. Also
//!   `build`'s REJECTED code.

/// Guest-initiated stop: the guest shut down or rebooted on its own (triple
/// fault / system event). Also the code a passing driver run returns.
pub(crate) const EXIT_GUEST_STOP: i32 = 0;
/// A passing verdict; an alias of [`EXIT_GUEST_STOP`].
pub(crate) const EXIT_PASS: i32 = EXIT_GUEST_STOP;
/// FAIL: a driver declared a nonzero verdict.
pub(crate) const EXIT_FAIL: i32 = 1;
/// A VMM policy stop: `--max-virtual-time` horizon reached. Distinct from a
/// guest-initiated stop so a harness can tell "the guest ended" from "we cut it
/// off at the virtual-time budget".
pub(crate) const EXIT_HORIZON: i32 = 3;
/// Infrastructure error: a bad/unreadable artifact, an agent that never came up,
/// or the wall-clock safety timeout. The timeout path exits the process directly,
/// so it has no [`StopReason`].
pub(crate) const EXIT_INFRA: i32 = 2;

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
    /// A container called `finish` on the control socket: the run has a verdict.
    /// The process exit code comes from that verdict, not `exit_code()`.
    DriverFinish,
}

impl StopReason {
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            StopReason::GuestShutdown
            | StopReason::GuestSystemEvent
            | StopReason::GuestHalt => EXIT_GUEST_STOP,
            StopReason::Horizon => EXIT_HORIZON,
            // Placeholder: a driven run's real exit code is the verdict's (see
            // `driver::Verdict::exit_code`); this is never used.
            StopReason::DriverFinish => EXIT_PASS,
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
            StopReason::DriverFinish => "driver_finish",
        }
    }

    /// A plain-language description of the stop, for the human run summary.
    pub(crate) fn human(self) -> &'static str {
        match self {
            StopReason::GuestShutdown => "guest shut down (triple fault or reboot)",
            StopReason::GuestSystemEvent => "guest requested reset or shutdown",
            StopReason::GuestHalt => "guest halted (poweroff)",
            StopReason::Horizon => "--max-virtual-time horizon reached",
            StopReason::DriverFinish => "a container finished the run with a verdict",
        }
    }
}

/// The result of a full boot+run: why it stopped, and the process exit code —
/// `stop.exit_code()`, except for a run a container finished, where the code is
/// that verdict's (see [`crate::driver`]).
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
