//! deterministic-vmm — Step 4: fast-forward virtual time on idle.
//!
//! A single-vCPU, Firecracker-shaped KVM VMM. The guest runs on a **userspace**
//! interrupt controller we own: no in-kernel irqchip, no in-kernel PIT. The
//! LAPIC's one-shot/periodic timer (driven by [`vtsc`]) is the tick; LAPIC/IOAPIC
//! register accesses are MMIO exits; and a halted guest parks at its HLT exit.
//!
//! ## Step 4: the JUMP
//!
//! When the guest is idle (HLTed) waiting for a future timer, the parker no
//! longer *waits* real time for that deadline — it **jumps** virtual time to it:
//! compute `Δ = next_event_vtsc − vtsc_now()`, bump the cached TSC offset by `Δ`
//! (write-through to `KVM_VCPU_TSC_OFFSET`), fire everything now due, and loop.
//! The guest experiences hours passing in seconds of wall clock. This is a
//! runtime flag (`--ff on|off`, default ON); with FF off the old 3b real-wait
//! park (`ppoll` on a `timerfd` + stdin) is used instead — the A/B for timing
//! bugs and the right mode for an interactive console. Only the *wait* changes;
//! the wake path (IRR → injection window → RUNNABLE) is unchanged 3b machinery.
//!
//! ## Single-writer invariant
//!
//! ALL guest-state effects — LAPIC/IOAPIC register state, interrupt raises, the
//! TSC-offset bump, and every KVM vcpu ioctl — happen on the vCPU thread at loop
//! boundaries. The offset is written ONLY while parked at a HLT exit, between
//! `KVM_RUN`s, never concurrent with a running vCPU. The vCPU thread owns console
//! input (it reads stdin while parked at HLT), so there is no off-thread writer
//! at all. (The in-kernel `--irqchip kernel` A/B backend — which could not
//! fast-forward, its timer ran on the host clock — was removed in Step 4.)
//!
//! ## vCPU loop shape
//!
//! `service_timers(); sync_tpr(); inject(); run(); handle_exit()` — timers fire
//! at loop boundaries (or at the HLT park), and the park is the one place that
//! converts a virtual-time deadline into either a real wait or a jump.

mod arch;
mod artifact;
mod boot;
mod build;
mod compose;
mod control;
mod cpio;
mod cpuid;
mod events;
mod ioapic;
mod lapic;
mod memory;
mod msrs;
mod mptable;
mod park;
mod pic;
mod pit;
mod regs;
mod scenario;
mod serial;
mod vtsc;

use clap::{Args, Parser, Subcommand};
use kvm_bindings::{kvm_interrupt, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd};
use vmm_sys_util::ioctl::ioctl_with_ref;
use vmm_sys_util::ioctl_iow_nr;

use crate::ioapic::Ioapic;
use crate::lapic::{apic_bus_hz_from_cpuid, apic_timer_tsc_ratio, Lapic, XAPIC_BASE, XAPIC_LEN};
use crate::mptable::isa_irq_to_ioapic_pin;
use crate::pic::PicStub;
use crate::pit::PitStub;
use crate::vtsc::VirtualClock;

// KVM_INTERRUPT = _IOW(KVMIO, 0x86, struct kvm_interrupt): queue one interrupt
// vector for injection on the next entry. Valid only without an in-kernel LAPIC
// (our userspace-irqchip backend). kvm-ioctls 0.25 does not wrap it, so we issue
// it directly on the vCPU fd, exactly as `vtsc.rs` does for the TSC device
// attributes. (Re-check on kvm-ioctls upgrades: drop this if a later release
// exposes `VcpuFd::interrupt`.)
const KVMIO: u32 = 0xAE;
ioctl_iow_nr!(KVM_INTERRUPT, KVMIO, 0x86, kvm_interrupt);

// ---- dvmm's own stderr logging (raw-tty aware) -----------------------------
//
// At an interactive console dvmm puts the tty in RAW mode (see
// `serial::RawTerminal`) so the GUEST owns the byte stream verbatim — which also
// turns OFF the terminal's newline->CRLF translation (ONLCR). A bare "\n" on OUR
// OWN log lines would then only drop down a row, not return to column 0, so our
// telemetry/startup/WARN lines would staircase across the guest's output. When
// raw mode is active we therefore terminate our log lines with CRLF and prepend a
// CR to snap to column 0 (embedded newlines get the same treatment). In cooked
// mode the terminal itself adds the CR, so a plain "\n" is already correct. This
// changes ONLY dvmm's own log lines — the guest's byte stream is untouched.
//
// The flag starts false and is set true only once `RawTerminal::enable` has put
// the tty in raw mode (see `run`), so lines emitted during cooked-mode boot setup
// still use a plain "\n".
static RAW_TTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Emit one dvmm log line to stderr with raw-tty-aware line endings (see the
/// module note above). Used via the [`dlog!`] macro (and directly by the device
/// modules); not for guest output.
pub(crate) fn log_line(args: std::fmt::Arguments) {
    use std::io::Write;
    let body = format!("{args}");
    let stderr = std::io::stderr();
    let mut h = stderr.lock();
    let _ = if RAW_TTY.load(std::sync::atomic::Ordering::Relaxed) {
        // Snap to column 0 and turn every embedded newline into a CRLF too.
        write!(h, "\r{}\r\n", body.replace('\n', "\r\n"))
    } else {
        writeln!(h, "{body}")
    };
    let _ = h.flush();
}

/// dvmm's own stderr log line, raw-tty aware (see [`log_line`]). A drop-in for
/// `eprintln!` for dvmm's OWN diagnostics — never for guest console bytes.
macro_rules! dlog {
    ($($arg:tt)*) => { crate::log_line(format_args!($($arg)*)) };
}

// `no_timer_check`: the userspace backend emits no PIT IRQ0, so the kernel must
// not run its "does the timer IRQ reach the CPU?" probe. `tsc=reliable`: trust
// the invariant TSC and skip the clocksource watchdog.
//
// `reboot=t` (triple fault), NOT `reboot=k`: on a guest reboot/panic the kernel
// resets the machine. We do NOT emulate an i8042 keyboard controller, so the
// `reboot=k` (keyboard-controller reset) method never completes here — the guest
// falls into a halt/re-arm-timer loop that fast-forward would advance FOREVER
// (the VMM never exits). `reboot=t` forces a triple fault, which surfaces as
// KVM_EXIT_SHUTDOWN and stops the VMM cleanly, so a guest that reboots/panics
// (e.g. `exit` at the PID-1 shell) actually terminates the run. The tested smoke
// paths already used `reboot=t`; this makes the interactive default match them.
const DEFAULT_CMDLINE: &str =
    "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable";
const DEFAULT_MEM_MIB: u64 = 2048;
/// Default fast-forward single-jump sanity bound (seconds). A jump larger than
/// this aborts the run (gate 3) — expected never to trip in normal operation.
/// A float so the bound can be set below the sub-second jumps a real workload
/// produces (this is a config threshold, not a timer/vtsc conversion).
const DEFAULT_MAX_JUMP_SECS: f64 = 300.0;

// Process exit codes. A testing platform wants the *cause* of a stop to be a
// first-class, machine-readable outcome, so each distinct stop reason maps to a
// distinct code (see `StopReason` and the shutdown-cause logging near the exit
// handlers).
/// Guest-initiated stop: the guest shut down or rebooted on its own (triple
/// fault / system event, e.g. panic+reboot or `reboot -f`). The "normal" way a
/// test guest ends.
const EXIT_GUEST_STOP: i32 = 0;
/// A VMM policy stop: `--max-virtual-time` horizon reached. Distinct from a
/// guest-initiated stop so a harness can tell "the guest ended" from "we cut it
/// off at the virtual-time budget".
const EXIT_HORIZON: i32 = 3;

/// Why the run loop stopped. Mapped to a distinct process exit code (above) and
/// logged distinguishably at the stop site (guest-initiated vs VMM policy).
#[derive(Clone, Copy, Debug)]
enum StopReason {
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
    fn exit_code(self) -> i32 {
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
    fn as_str(self) -> &'static str {
        match self {
            StopReason::GuestShutdown => "guest_shutdown",
            StopReason::GuestSystemEvent => "guest_system_event",
            StopReason::GuestHalt => "guest_halt",
            StopReason::Horizon => "horizon",
            StopReason::Scenario => "scenario",
        }
    }
}

/// The result of a full boot+run: why it stopped, and the process exit code. For
/// `boot`/`run` the code is `stop.exit_code()`; for `test` it is the scenario
/// verdict's code (0 pass / 1 assertion fail / 2 infrastructure).
struct RunOutcome {
    #[allow(dead_code)]
    stop: StopReason,
    exit_code: i32,
}

/// Everything `dvmm test` needs to run a scenario, built before boot and handed
/// to the vCPU loop (which builds the engine once the virtual clock exists).
struct ScenarioSetup {
    scenario: scenario::Scenario,
    meta: scenario::RunMeta,
}

/// A scheduled event in the one [`events::EventQueue`]. Every guest timer is an
/// entry here (today that is the LAPIC deadline, the tick); Step-4 adds the
/// virtual-time horizon as a first-class queue event so the run terminates
/// through the same drain path rather than a bolted-on loop check.
#[derive(Clone, Copy, Debug)]
enum TimerKind {
    /// The LAPIC one-shot/periodic timer deadline (the guest's tick).
    LapicDeadline,
    /// `--max-virtual-time`: when vtsc reaches the horizon, stop the run. A
    /// deterministic virtual-time event, not a real-time policy.
    StopRun,
    /// TEST-1a: a scenario step's scheduled `at:` (or a poll/reply deadline) has
    /// arrived. GENERALIZES `StopRun` — a control command is delivered at its
    /// scheduled vtsc through the same one queue, never an ad-hoc side channel.
    ScenarioStep,
}

// ---- Δvtsc jump histogram (Step 4 instrumentation) -------------------------
//
// Buckets each fast-forward jump by how far it advanced virtual time (Δ),
// converted to nanoseconds. The wedge signature is a flood of *uniform tiny* Δ
// jumps (~40k/s); bucketing makes that shape visible at a glance, and the same
// histogram is reusable instrumentation the later Go-runtime comparison will
// consume. Upper edges (ns); a Δ lands in the first bucket whose edge it is
// below, with a final ">=10s" overflow bucket.
const HIST_EDGES_NS: [u64; 8] = [
    1_000,          // <1us
    10_000,         // <10us
    100_000,        // <100us
    1_000_000,      // <1ms
    10_000_000,     // <10ms
    100_000_000,    // <100ms
    1_000_000_000,  // <1s
    10_000_000_000, // <10s
];
const HIST_LABELS: [&str; 9] = [
    "<1us", "<10us", "<100us", "<1ms", "<10ms", "<100ms", "<1s", "<10s", ">=10s",
];

/// A Δvtsc histogram: jump counts bucketed by how far they advanced virtual time.
#[derive(Clone, Copy, Debug)]
struct DeltaHistogram {
    buckets: [u64; 9],
}

impl DeltaHistogram {
    fn new() -> Self {
        Self { buckets: [0; 9] }
    }

    /// Bucket one jump of `delta_ns` nanoseconds.
    fn record_ns(&mut self, delta_ns: u64) {
        let mut i = 0;
        while i < HIST_EDGES_NS.len() && delta_ns >= HIST_EDGES_NS[i] {
            i += 1;
        }
        self.buckets[i] += 1;
    }

    /// A compact one-line summary: every bucket label with its count.
    fn summary(&self) -> String {
        let mut parts = Vec::with_capacity(self.buckets.len());
        for (i, c) in self.buckets.iter().enumerate() {
            parts.push(format!("{}:{}", HIST_LABELS[i], c));
        }
        format!("Δvtsc histogram [jumps by advance]: {}", parts.join(" "))
    }

    /// The bucket counts as a comma-separated list, for the machine-parseable
    /// `--metrics-out` file (the comparison harness renders these side by side).
    fn counts_csv(&self) -> String {
        self.buckets
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
}

// ---- per-hop cost histogram (for the p99 tail) -----------------------------
//
// The Δvtsc histogram above buckets jumps by how far they ADVANCE virtual time
// (the attribution instrument). This second, separate histogram buckets each
// jump by how long the jump COST in real nanoseconds — a different quantity,
// needed only to estimate the per-hop-cost p99 tail (mean + max are tracked
// exactly in `FfState`). Exponential-ish upper edges (ns) spanning the sub-µs
// common case out to the multi-ms tail; a final overflow bucket catches the rest.
const COST_EDGES_NS: [u64; 17] = [
    50, 100, 200, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 200_000, 500_000,
    1_000_000, 2_000_000, 5_000_000, 10_000_000,
];

/// A per-hop-cost latency histogram; supports a linearly-interpolated quantile
/// estimate for the p99 tail (we cannot store every per-hop sample — a chatty
/// runtime produces millions of jumps — so the tail is estimated from buckets).
#[derive(Clone, Copy, Debug)]
struct CostHistogram {
    buckets: [u64; 18],
    total: u64,
}

impl CostHistogram {
    fn new() -> Self {
        Self {
            buckets: [0; 18],
            total: 0,
        }
    }

    fn record_ns(&mut self, ns: u64) {
        let mut i = 0;
        while i < COST_EDGES_NS.len() && ns >= COST_EDGES_NS[i] {
            i += 1;
        }
        self.buckets[i] += 1;
        self.total += 1;
    }

    /// Estimate the `q` quantile (0.0..=1.0) of per-hop cost in ns, linearly
    /// interpolating inside the bucket the quantile falls in. Returns the bucket's
    /// lower..upper midpoint-interpolated value; the final overflow bucket reports
    /// its lower edge (a conservative floor for the extreme tail).
    fn quantile_ns(&self, q: f64) -> u64 {
        if self.total == 0 {
            return 0;
        }
        let target = (q * self.total as f64).ceil() as u64;
        let mut cum = 0u64;
        for (i, &c) in self.buckets.iter().enumerate() {
            let prev_cum = cum;
            cum += c;
            if cum >= target && c > 0 {
                let lo = if i == 0 { 0 } else { COST_EDGES_NS[i - 1] };
                let hi = if i < COST_EDGES_NS.len() {
                    COST_EDGES_NS[i]
                } else {
                    // overflow bucket: no upper edge — report the lower edge floor.
                    return lo;
                };
                // Linear position of `target` within this bucket's count.
                let into = (target - prev_cum) as f64 / c as f64;
                return lo + ((hi - lo) as f64 * into) as u64;
            }
        }
        COST_EDGES_NS[COST_EDGES_NS.len() - 1]
    }
}

// ---- WARN-only jump-rate telemetry (Step 4) --------------------------------
//
// If the fast-forward jump rate stays above the threshold for the sustain
// window, emit ONE rate-limited WARN line (the wedge signature made visible).
// This NEVER stops the run — termination is `--max-virtual-time`'s job.
const JUMP_RATE_WARN_THRESHOLD: f64 = 10_000.0; // jumps/s
const JUMP_RATE_WARN_SUSTAIN: std::time::Duration = std::time::Duration::from_secs(5);
const JUMP_RATE_WARN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);
const JUMP_RATE_WIN: std::time::Duration = std::time::Duration::from_secs(1);

/// Fast-forward accounting + the single-jump sanity bound (Step 4).
///
/// Tracks the jump count, the fast-forwarded virtual time, the per-hop real
/// cost, and the largest observed Δ — the inputs to the acceptance gates (the
/// speedup metric, per-hop cost, and max single jump). All integer cycles until
/// the final float conversion for reporting.
struct FfState {
    /// TSC frequency (Hz) — converts cycles to seconds for the reports.
    tsc_hz: u64,
    /// Single-jump bound (gate 3): abort if any Δ exceeds this many cycles.
    max_jump_cycles: u64,
    /// The same bound in seconds, for messages.
    max_jump_secs: f64,
    /// Number of jumps performed.
    jumps: u64,
    /// Largest single jump Δ observed (cycles).
    max_delta_cycles: u64,
    /// Sum of all jump Δs (cycles) — total virtual time fast-forwarded.
    sum_delta_cycles: u128,
    /// Sum of per-hop real cost (ns) and the max, for mean/max reporting.
    hop_ns_sum: u128,
    hop_ns_max: u64,
    /// Δvtsc histogram: jumps bucketed by how far they advanced virtual time.
    /// Feeds the horizon diagnostic dump and the WARN telemetry.
    hist: DeltaHistogram,
    /// Per-hop COST histogram (real ns per jump), for the p99 tail estimate.
    cost_hist: CostHistogram,
    // --- jump-rate WARN tracking (real-time, telemetry only; never stops) ---
    /// Start of the current 1s rate-measurement window.
    rate_win_start: std::time::Instant,
    /// Jump count at the start of the current window.
    rate_win_jumps: u64,
    /// When the rate first went (and stayed) above the threshold, if it is.
    high_since: Option<std::time::Instant>,
    /// When the last WARN was emitted, for cooldown rate-limiting.
    last_warn: Option<std::time::Instant>,
}

impl FfState {
    fn new(tsc_hz: u64, max_jump_secs: f64) -> Self {
        Self {
            tsc_hz,
            // f64 -> u64 saturates (never panics); the bound is a config threshold.
            max_jump_cycles: (max_jump_secs * tsc_hz as f64) as u64,
            max_jump_secs,
            jumps: 0,
            max_delta_cycles: 0,
            sum_delta_cycles: 0,
            hop_ns_sum: 0,
            hop_ns_max: 0,
            hist: DeltaHistogram::new(),
            cost_hist: CostHistogram::new(),
            rate_win_start: std::time::Instant::now(),
            rate_win_jumps: 0,
            high_since: None,
            last_warn: None,
        }
    }

    fn record_hop(&mut self, delta_cycles: u64, hop: std::time::Duration) {
        self.jumps += 1;
        self.sum_delta_cycles += u128::from(delta_cycles);
        if delta_cycles > self.max_delta_cycles {
            self.max_delta_cycles = delta_cycles;
        }
        // Bucket the jump by its virtual-time advance (Δ -> ns), for the histogram.
        let delta_ns =
            (u128::from(delta_cycles) * 1_000_000_000u128 / u128::from(self.tsc_hz)) as u64;
        self.hist.record_ns(delta_ns);
        let ns = hop.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.hop_ns_sum += u128::from(ns);
        if ns > self.hop_ns_max {
            self.hop_ns_max = ns;
        }
        self.cost_hist.record_ns(ns);
    }

    /// WARN-only jump-rate telemetry. Call once per jump. If the jump rate has
    /// stayed above [`JUMP_RATE_WARN_THRESHOLD`] for [`JUMP_RATE_WARN_SUSTAIN`],
    /// emit ONE rate-limited WARN line with a stats + histogram snapshot. NEVER
    /// stops the run (that is `--max-virtual-time`'s job).
    fn maybe_warn_high_jump_rate(&mut self) {
        if let Some(msg) = self.jump_rate_warn_at(std::time::Instant::now()) {
            dlog!("{msg}");
        }
    }

    /// Testable core of [`maybe_warn_high_jump_rate`]: given the current instant,
    /// update the sliding-window rate tracker and return the WARN message iff one
    /// should be emitted now (rate above threshold, sustained past the window,
    /// and past the cooldown since the last WARN). Pure of I/O so a unit test can
    /// drive it with synthetic instants — no real sleeping, deterministic.
    fn jump_rate_warn_at(&mut self, now: std::time::Instant) -> Option<String> {
        let win = now.duration_since(self.rate_win_start);
        if win < JUMP_RATE_WIN {
            return None; // accumulate a full window before judging the rate
        }
        let rate = (self.jumps - self.rate_win_jumps) as f64 / win.as_secs_f64();
        let mut msg = None;
        if rate > JUMP_RATE_WARN_THRESHOLD {
            let since = *self.high_since.get_or_insert(self.rate_win_start);
            let sustained = now.duration_since(since);
            let cooled = self
                .last_warn
                .map_or(true, |t| now.duration_since(t) >= JUMP_RATE_WARN_COOLDOWN);
            if sustained >= JUMP_RATE_WARN_SUSTAIN && cooled {
                msg = Some(format!(
                    "[dvmm][WARN] fast-forward jump rate {:.0}/s sustained for {:.0}s \
                     ({} jumps total, max Δ {:.3}s) — possible wedged or deeply-idle guest; \
                     NOT stopping (set --max-virtual-time to bound the run). {}",
                    rate,
                    sustained.as_secs_f64(),
                    self.jumps,
                    self.max_delta_secs(),
                    self.hist.summary(),
                ));
                self.last_warn = Some(now);
                self.high_since = Some(now); // re-arm: require another sustain window
            }
        } else {
            self.high_since = None; // rate dropped; reset the sustain clock
        }
        self.rate_win_start = now;
        self.rate_win_jumps = self.jumps;
        msg
    }

    fn mean_hop_ns(&self) -> u64 {
        if self.jumps == 0 {
            0
        } else {
            (self.hop_ns_sum / u128::from(self.jumps)) as u64
        }
    }

    fn max_delta_secs(&self) -> f64 {
        self.max_delta_cycles as f64 / self.tsc_hz as f64
    }

    /// Virtual seconds elapsed between two vtsc samples (the whole run's span,
    /// active execution included — this is the headline speedup numerator).
    fn virtual_secs_since(&self, vtsc_start: u64, vtsc_now: u64) -> f64 {
        vtsc_now.wrapping_sub(vtsc_start) as f64 / self.tsc_hz as f64
    }

    fn hop_p99_ns(&self) -> u64 {
        self.cost_hist.quantile_ns(0.99)
    }

    /// Real seconds spent performing the jumps themselves (the sum of per-hop
    /// park/jump costs). The rest of wall time is guest execution + VMM overhead.
    fn jump_real_secs(&self) -> f64 {
        self.hop_ns_sum as f64 / 1e9
    }

    /// Build the machine-parseable per-run metrics block (`--metrics-out`). Every
    /// field the comparison harness needs: hop count + rate, speedup, the per-hop
    /// cost mean/p99/max, the real-vs-virtual accounting (the busy-wait tripwire),
    /// and the Δvtsc histogram (reused, not duplicated). key<space>value per line,
    /// so it is trivially greppable and stable across runs.
    fn metrics_report(
        &self,
        stop: StopReason,
        vtsc_start: u64,
        vtsc_now: u64,
        real_secs: f64,
        hlt_count: u64,
    ) -> String {
        let virt_s = self.virtual_secs_since(vtsc_start, vtsc_now);
        let real_s = real_secs.max(1e-9);
        let speedup = virt_s / real_s;
        let vhours = (virt_s / 3600.0).max(1e-12);
        let jump_real = self.jump_real_secs();
        let exec_real = (real_s - jump_real).max(0.0);
        let edges = HIST_EDGES_NS
            .iter()
            .map(|e| e.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "# dvmm fast-forward per-run metrics (machine-parseable; --metrics-out)\n\
             schema 1\n\
             stop_reason {stop}\n\
             tsc_hz {tsc_hz}\n\
             real_secs {real_s:.6}\n\
             virtual_secs {virt_s:.3}\n\
             speedup {speedup:.3}\n\
             jumps {jumps}\n\
             hops_per_virtual_hour {hpvh:.3}\n\
             hop_ns_mean {mean}\n\
             hop_ns_p99 {p99}\n\
             hop_ns_max {max}\n\
             jump_real_secs {jump_real:.6}\n\
             exec_real_secs {exec_real:.6}\n\
             executing_fraction {exec_frac:.6}\n\
             jumping_fraction {jump_frac:.6}\n\
             exec_real_ms_per_vhour {exec_pvh:.3}\n\
             jump_real_ms_per_vhour {jump_pvh:.3}\n\
             max_delta_secs {maxd:.6}\n\
             hlt_count {hlt_count}\n\
             hlt_per_virtual_hour {hlt_pvh:.3}\n\
             hist_labels {labels}\n\
             hist_edges_ns {edges}\n\
             hist_counts {counts}\n",
            stop = stop.as_str(),
            tsc_hz = self.tsc_hz,
            jumps = self.jumps,
            hpvh = self.jumps as f64 / vhours,
            mean = self.mean_hop_ns(),
            p99 = self.hop_p99_ns(),
            max = self.hop_ns_max,
            exec_frac = exec_real / real_s,
            jump_frac = jump_real / real_s,
            exec_pvh = exec_real * 1000.0 / vhours,
            jump_pvh = jump_real * 1000.0 / vhours,
            maxd = self.max_delta_secs(),
            hlt_pvh = hlt_count as f64 / vhours,
            labels = HIST_LABELS.join(","),
            counts = self.hist.counts_csv(),
        )
    }
}

// ============================================================================
// CLI subcommands. `build` (OP-1b) bakes a compose stack into a `.dvmm` (host
// tool: podman + network). `boot` is the raw kernel+initramfs dev verb; `run`
// boots a `.dvmm` applying its baked run-defaults; `test` drives a scenario;
// `inspect`/`verify` read the artifact; `dump-cpuid` emits the manifest CPUID
// profile. `__seed-build` / `__assemble-initramfs` are internal `podman unshare`
// helpers `dvmm build` re-execs into.
// ============================================================================

#[derive(Parser)]
#[command(
    name = "dvmm",
    about = "deterministic KVM VMM — run/inspect/verify a .dvmm stack, or boot raw artifacts",
    long_about = "A single-vCPU, fast-forwardable KVM VMM. `run` boots a self-contained \
                  .dvmm stack artifact (baked defaults, overridable by flags); `boot` is \
                  the low-level raw kernel+initramfs verb for VMM development.\n\n\
                  Durations (--max-virtual-time): a bare number is seconds, or use a \
                  suffix (ms, s, m, h), e.g. 500ms, 30s, 5m, 2h.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Bake a compose stack into a self-contained .dvmm (host tool: podman + network).
    Build(BuildCliArgs),
    /// Boot a raw kernel + initramfs (the low-level VMM-dev / smoke verb).
    Boot(BootArgs),
    /// Run a .dvmm stack artifact: apply its baked run-defaults, then boot (offline).
    Run(RunArgs),
    /// Test a .dvmm stack against a scenario: drive virtual time, assert, verdict.
    Test(TestArgs),
    /// Print a .dvmm artifact's manifest.json (reads ONLY the manifest member).
    Inspect(ArtifactArg),
    /// Verify a .dvmm: recompute member hashes vs the manifest; print its sha256 identity.
    Verify(ArtifactArg),
    /// Print the effective guest clock/timer CPUID profile (the manifest artifact).
    DumpCpuid,
    /// [internal] Build the seed store inside `podman unshare` (used by `dvmm build`).
    #[command(name = "__seed-build", hide = true)]
    SeedBuild {
        #[arg(long, value_name = "PATH")]
        config: String,
    },
    /// [internal] Assemble the rootfs + emit the cpio inside `podman unshare`.
    #[command(name = "__assemble-initramfs", hide = true)]
    AssembleInitramfs {
        #[arg(long, value_name = "PATH")]
        config: String,
    },
}

/// `dvmm build` args (clap). Mirrors bake-stack.sh's flags.
#[derive(Args)]
struct BuildCliArgs {
    /// Path to the compose.yml to bake.
    #[arg(value_name = "compose.yml")]
    compose: String,
    /// Output .dvmm path (default guest/initramfs-alpine/<stack>.dvmm).
    #[arg(short, long, value_name = "PATH")]
    out: Option<String>,
    /// Stack name (default: the compose file's parent directory name).
    #[arg(long, value_name = "STR")]
    name: Option<String>,
    /// Guest RAM in MiB (default 3072).
    #[arg(long, value_name = "MiB")]
    mem: Option<u64>,
    /// Workload working-set allowance for the RAM estimate (MiB, default 512).
    #[arg(long, value_name = "MiB")]
    working_set: Option<u64>,
    /// Squash images larger than this many MiB to one vfs layer (default 100).
    #[arg(long, value_name = "MiB")]
    squash_threshold: Option<u64>,
    /// Only run the static compose validation (no pulls/boot); print + exit.
    #[arg(long)]
    validate_only: bool,
    /// Bypass the content-hash bake cache: force a full rebuild. The cache is keyed
    /// on ALL bake inputs, so an unchanged stack normally HITS (near-instant, skips
    /// pull/squash/assemble). Nightly bake-repeatability uses this to re-bake.
    #[arg(long)]
    no_cache: bool,
}

/// Flags shared by `boot` and `run`. On `boot` the `Option`s fall back to the
/// binary defaults; on `run` a `None` means "use the artifact's baked default"
/// and `Some` means the flag overrides it (baked < flag, Fable-locked).
#[derive(Args, Clone)]
struct CommonRunFlags {
    /// Guest RAM in MiB.
    #[arg(long, value_name = "MiB")]
    mem: Option<u64>,
    /// Kernel command line.
    #[arg(long, value_name = "STR")]
    cmdline: Option<String>,
    /// Fast-forward idle time: on|off.
    #[arg(long, value_parser = parse_onoff, value_name = "on|off")]
    ff: Option<bool>,
    /// Single-jump sanity bound (seconds); a larger jump aborts the run.
    #[arg(long, value_name = "N")]
    max_jump_secs: Option<f64>,
    /// Virtual-time horizon (duration); stop with exit 3 when reached.
    #[arg(long, value_name = "DUR")]
    max_virtual_time: Option<String>,
    /// Write the per-run fast-forward metrics block to this path at stop.
    #[arg(long, value_name = "PATH")]
    metrics_out: Option<String>,
}

#[derive(Args)]
struct BootArgs {
    /// Path to the uncompressed ELF vmlinux.
    #[arg(long, value_name = "PATH")]
    kernel: String,
    /// Path to the initramfs.
    #[arg(long, value_name = "PATH")]
    initrd: String,
    #[command(flatten)]
    common: CommonRunFlags,
}

#[derive(Args)]
struct RunArgs {
    /// Path to the .dvmm stack artifact.
    #[arg(value_name = "stack.dvmm")]
    artifact: String,
    /// Skip the default-ON member-hash verification on load.
    #[arg(long)]
    no_verify: bool,
    #[command(flatten)]
    common: CommonRunFlags,
}

#[derive(Args)]
struct TestArgs {
    /// Path to the .dvmm stack artifact.
    #[arg(value_name = "stack.dvmm")]
    artifact: String,
    /// The scenario YAML (steps + assertions).
    #[arg(long, value_name = "PATH")]
    scenario: String,
    /// Skip the default-ON member-hash verification on load.
    #[arg(long)]
    no_verify: bool,
    /// JSONL run-log path (default `<artifact>.jsonl`).
    #[arg(long, value_name = "PATH")]
    jsonl: Option<String>,
    /// JSON report path (default `<artifact>.report.json`).
    #[arg(long, value_name = "PATH")]
    report: Option<String>,
    /// Wall-clock safety timeout (seconds); a run exceeding it fails with exit 2.
    #[arg(long, value_name = "SECS", default_value_t = 600)]
    wall_timeout: u64,
    #[command(flatten)]
    common: CommonRunFlags,
}

#[derive(Args)]
struct ArtifactArg {
    /// Path to the .dvmm stack artifact.
    #[arg(value_name = "stack.dvmm")]
    artifact: String,
}

/// clap value parser for `--ff on|off` (also accepts 1/0/true/false).
fn parse_onoff(s: &str) -> Result<bool, String> {
    match s {
        "on" | "1" | "true" => Ok(true),
        "off" | "0" | "false" => Ok(false),
        _ => Err(format!("expected on|off (got {s:?})")),
    }
}

/// The resolved run configuration + a per-knob provenance string for the
/// EFFECTIVE-CONFIG line (the future record-log preamble). Provenance is
/// `baked` (from the artifact), `flag` (a CLI override), or `default` (binary
/// default). Override precedence is LOCKED: baked < flag.
struct EffectiveConfig {
    mem_mib: u64,
    cmdline: String,
    fast_forward: bool,
    /// Whether `--ff` was explicitly passed — feeds ONLY the FF mode statement's
    /// "how chosen" wording, never the FF decision.
    ff_explicit: bool,
    max_jump_secs: f64,
    max_virtual_time_secs: Option<f64>,
    metrics_out: Option<String>,
    /// The formatted per-knob provenance, e.g.
    /// `mem=3072 (baked) ff=off (flag) horizon=36h (baked) ...`.
    provenance: String,
}

impl EffectiveConfig {
    /// Resolve for `dvmm boot`: no baked defaults; each knob is a flag override of
    /// the binary default.
    fn from_boot(f: &CommonRunFlags) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, None, None)
    }

    /// Resolve for `dvmm run`: the artifact's baked run-defaults, each overridable
    /// by the corresponding CLI flag (baked < flag).
    fn from_run(
        f: &CommonRunFlags,
        baked: &artifact::RunDefaults,
    ) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, Some(baked), None)
    }

    /// Resolve for `dvmm test`: baked run-defaults, overridable by the scenario's
    /// `run:` block, overridable by CLI flags. Precedence: baked < scenario < flag.
    fn from_test(
        f: &CommonRunFlags,
        baked: &artifact::RunDefaults,
        scn: &scenario::ScenarioRun,
    ) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, Some(baked), Some(scn))
    }

    fn resolve(
        f: &CommonRunFlags,
        baked: Option<&artifact::RunDefaults>,
        scn: Option<&scenario::ScenarioRun>,
    ) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        let mut prov: Vec<String> = Vec::new();

        // mem: flag > scenario > baked > default.
        let (mem_mib, mem_src) = match (f.mem, scn.and_then(|s| s.mem), baked) {
            (Some(v), _, _) => (v, "flag"),
            (None, Some(v), _) => (v, "scenario"),
            (None, None, Some(b)) => (b.mem_mib, "baked"),
            (None, None, None) => (DEFAULT_MEM_MIB, "default"),
        };
        prov.push(format!("mem={mem_mib} ({mem_src})"));

        // cmdline
        let (cmdline, cl_src) = match (&f.cmdline, scn.and_then(|s| s.cmdline.as_ref()), baked) {
            (Some(v), _, _) => (v.clone(), "flag"),
            (None, Some(v), _) => (v.clone(), "scenario"),
            (None, None, Some(b)) => (b.cmdline.clone(), "baked"),
            (None, None, None) => (DEFAULT_CMDLINE.to_string(), "default"),
        };
        prov.push(format!("cmdline={cmdline:?} ({cl_src})"));

        // fast-forward
        let ff_explicit = f.ff.is_some();
        let (fast_forward, ff_src) = match (f.ff, scn.and_then(|s| s.ff), baked) {
            (Some(v), _, _) => (v, "flag"),
            (None, Some(v), _) => (v, "scenario"),
            (None, None, Some(b)) => (b.fast_forward, "baked"),
            (None, None, None) => (true, "default"),
        };
        prov.push(format!(
            "ff={} ({ff_src})",
            if fast_forward { "on" } else { "off" }
        ));

        // max-virtual-time (horizon)
        let scn_mvt = scn.and_then(|s| s.max_virtual_time.as_ref());
        let (max_virtual_time_secs, mvt_disp, mvt_src) = match (&f.max_virtual_time, scn_mvt, baked) {
            (Some(s), _, _) => (Some(parse_dur(s)?), s.clone(), "flag"),
            (None, Some(s), _) => (Some(parse_dur(s)?), s.clone(), "scenario"),
            (None, None, Some(b)) => match &b.max_virtual_time {
                Some(s) => (Some(parse_dur(s)?), s.clone(), "baked"),
                None => (None, "unset".to_string(), "baked"),
            },
            (None, None, None) => (None, "unset".to_string(), "default"),
        };
        prov.push(format!("max-virtual-time={mvt_disp} ({mvt_src})"));

        // max-jump-secs (no baked value)
        let (max_jump_secs, mj_src) = match f.max_jump_secs {
            Some(v) if v.is_finite() && v > 0.0 => (v, "flag"),
            Some(_) => return Err("--max-jump-secs must be finite and > 0".into()),
            None => (DEFAULT_MAX_JUMP_SECS, "default"),
        };
        prov.push(format!("max-jump-secs={max_jump_secs} ({mj_src})"));

        Ok(EffectiveConfig {
            mem_mib,
            cmdline,
            fast_forward,
            ff_explicit,
            max_jump_secs,
            max_virtual_time_secs,
            metrics_out: f.metrics_out.clone(),
            provenance: prov.join(" "),
        })
    }
}

/// Parse a duration string to seconds, erroring (not exiting) on junk — for the
/// resolution path, which propagates errors.
fn parse_dur(s: &str) -> Result<f64, Box<dyn std::error::Error>> {
    parse_duration_secs(s).ok_or_else(|| format!("invalid duration {s:?}").into())
}

/// Parse a duration to seconds (f64). A bare number is seconds; suffixes `ms`,
/// `s`, `m`, `h` are honored. Returns `None` on anything unparseable or <= 0.
/// Shared with `scenario.rs` for `at:` / `every:` / `timeout:` durations.
pub(crate) fn parse_duration_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, mult) = if let Some(v) = s.strip_suffix("ms") {
        (v, 0.001)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1.0)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60.0)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3600.0)
    } else {
        (s, 1.0)
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|n| n * mult)
        .filter(|&secs| secs.is_finite() && secs > 0.0)
}

/// The startup fast-forward **mode statement** (spec item 1): the FF state plus
/// how it was chosen. Rendered identically whether or not stdin is a tty, and
/// ALWAYS printed at startup, so the effective default is visible to a human and
/// can be mechanically asserted by the test suite. isatty NEVER feeds this — the
/// FF decision must not vary with the ambient environment (Fable-locked).
fn ff_mode_statement(fast_forward: bool, ff_explicit: bool) -> String {
    let state = if fast_forward { "ON" } else { "OFF" };
    let how = match (ff_explicit, fast_forward) {
        (true, true) => "--ff on",
        (true, false) => "--ff off",
        (false, _) => "default",
    };
    format!("fast-forward: {state} ({how})")
}

fn main() {
    match dispatch() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            dlog!("dvmm: fatal: {err}");
            std::process::exit(1);
        }
    }
}

/// Parse the CLI and dispatch to a subcommand handler.
fn dispatch() -> Result<i32, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Build(args) => build::cmd_build(build::BuildArgs {
            compose: args.compose,
            out: args.out,
            name: args.name,
            mem: args.mem,
            working_set: args.working_set,
            squash_threshold: args.squash_threshold,
            validate_only: args.validate_only,
            no_cache: args.no_cache,
        }),
        Cmd::SeedBuild { config } => build::cmd_seed_build(&config),
        Cmd::AssembleInitramfs { config } => build::cmd_assemble_initramfs(&config),
        Cmd::Boot(args) => cmd_boot(args),
        Cmd::Run(args) => cmd_run(args),
        Cmd::Test(args) => cmd_test(args),
        Cmd::Inspect(a) => cmd_inspect(&a.artifact),
        Cmd::Verify(a) => cmd_verify(&a.artifact),
        Cmd::DumpCpuid => {
            dump_cpuid(&Kvm::new()?)?;
            Ok(0)
        }
    }
}

// ---- `dvmm boot`: raw kernel + initramfs (low-level dev verb) ---------------

fn cmd_boot(args: BootArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let eff = EffectiveConfig::from_boot(&args.common)?;
    let kernel = std::fs::read(&args.kernel)
        .map_err(|e| format!("opening kernel {}: {e}", args.kernel))?;
    let initrd = std::fs::read(&args.initrd)
        .map_err(|e| format!("opening initrd {}: {e}", args.initrd))?;
    dlog!(
        "[dvmm] boot: kernel={} initrd={}",
        args.kernel,
        args.initrd
    );
    let out = boot_and_run(&kernel, &initrd, &eff, None)?;
    Ok(out.exit_code)
}

// ---- `dvmm run`: a .dvmm stack artifact (baked defaults + overrides) --------

fn cmd_run(args: RunArgs) -> Result<i32, Box<dyn std::error::Error>> {
    // Load the artifact's members into memory (NO temp-dir extraction): manifest
    // + kernel + initramfs + compose.lock (the last for the on-load verify).
    let payload = artifact::read_for_run(&args.artifact)?;

    // Member-hash verify on load is DEFAULT-ON (`--no-verify` to skip): recompute
    // each payload member's sha256 and compare to the manifest, so a corrupted or
    // tampered artifact is caught before we boot it.
    if args.no_verify {
        dlog!("[dvmm] run: member-hash verify SKIPPED (--no-verify)");
    } else {
        verify_payload_or_bail(&args.artifact, &payload)?;
        dlog!(
            "[dvmm] run: {} member hashes verified against manifest (identity {})",
            payload.manifest.members.len(),
            &artifact::file_sha256_hex(&args.artifact)?[..16],
        );
    }

    let eff = EffectiveConfig::from_run(&args.common, &payload.manifest.run_defaults)?;
    dlog!(
        "[dvmm] run: stack={} project={} (format v{})",
        payload.manifest.stack,
        payload.manifest.project,
        payload.manifest.format_version,
    );
    let out = boot_and_run(&payload.kernel, &payload.initramfs, &eff, None)?;
    Ok(out.exit_code)
}

// ---- `dvmm test`: drive a .dvmm stack against a scenario (verdict) ----------

/// Infrastructure-error exit code for `dvmm test` (the CI contract): 0 = all
/// assertions passed, 1 = an assertion / readiness failure (from the scenario
/// verdict), 2 = an infrastructure error (bad scenario, or a boot/bake/agent
/// failure — the tool broke, not your stack).
const EXIT_TEST_INFRA: i32 = 2;

fn cmd_test(args: TestArgs) -> Result<i32, Box<dyn std::error::Error>> {
    // Load the artifact (kernel + initramfs + compose.lock + manifest).
    let payload = match artifact::read_for_run(&args.artifact) {
        Ok(p) => p,
        Err(e) => {
            dlog!("[dvmm][test] infrastructure error: {e}");
            return Ok(EXIT_TEST_INFRA);
        }
    };
    if !args.no_verify {
        if let Err(e) = verify_payload_or_bail(&args.artifact, &payload) {
            dlog!("[dvmm][test] infrastructure error: {e}");
            return Ok(EXIT_TEST_INFRA);
        }
    }

    // STATIC validation (before boot; sub-second). Service names come from the
    // artifact's compose.lock.yml; unknown keys / durations / regex / services
    // fail loudly here.
    let services = match scenario::service_names(&payload.compose_lock) {
        Ok(s) => s,
        Err(e) => {
            dlog!("[dvmm][test] scenario rejected: {e}");
            return Ok(EXIT_TEST_INFRA);
        }
    };
    let scn = match scenario::Scenario::load_and_validate(&args.scenario, &services) {
        Ok(s) => s,
        Err(e) => {
            dlog!("[dvmm][test] scenario rejected (static validation): {e}");
            return Ok(EXIT_TEST_INFRA);
        }
    };

    // Resolve the run config: baked < scenario.run < CLI flags. If no horizon was
    // set anywhere, apply the scenario's implicit end-horizon (last step + slack)
    // so a wedged run is bounded in virtual time.
    let mut eff = EffectiveConfig::from_test(&args.common, &payload.manifest.run_defaults, &scn.run)?;
    if eff.max_virtual_time_secs.is_none() {
        let h = scn.implicit_horizon_secs();
        eff.max_virtual_time_secs = Some(h);
        dlog!(
            "[dvmm][test] implicit end-horizon: {:.0}s of virtual time (last step + slack)",
            h
        );
    }

    let artifact_sha = artifact::file_sha256_hex(&args.artifact)?;
    let jsonl_path = args
        .jsonl
        .clone()
        .unwrap_or_else(|| format!("{}.jsonl", args.artifact));
    let report_path = args
        .report
        .clone()
        .unwrap_or_else(|| format!("{}.report.json", args.artifact));

    dlog!(
        "[dvmm][test] stack={} scenario={} ({} steps) artifact-sha256={}",
        payload.manifest.stack,
        scn.source_path,
        scn.steps.len(),
        &artifact_sha[..16],
    );

    let meta = scenario::RunMeta {
        stack: payload.manifest.stack.clone(),
        artifact_sha256: artifact_sha,
        fast_forward: eff.fast_forward,
        jsonl_path,
        report_path,
    };
    let setup = ScenarioSetup { scenario: scn, meta };

    // Wall-clock safety watchdog: a genuinely wedged guest that busy-loops (never
    // HLTs) is bounded by the virtual-time horizon in seconds of wall time, but a
    // hard hang (e.g. a stuck host ioctl) is caught here → exit 2. The JSONL is
    // flushed per line, so a hard exit still leaves a complete partial log.
    let wall_timeout = args.wall_timeout;
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let done = done.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_secs(wall_timeout.max(1));
            while std::time::Instant::now() < deadline {
                if done.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            if !done.load(std::sync::atomic::Ordering::Relaxed) {
                crate::log_line(format_args!(
                    "[dvmm][test] WALL-CLOCK TIMEOUT after {wall_timeout}s — aborting (exit 2)"
                ));
                std::process::exit(EXIT_TEST_INFRA);
            }
        });
    }

    let out = boot_and_run(&payload.kernel, &payload.initramfs, &eff, Some(setup))?;
    done.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(out.exit_code)
}

/// Recompute the run payload's member hashes and bail (error) on any mismatch —
/// the default-ON on-load integrity check for `run`.
fn verify_payload_or_bail(
    path: &str,
    payload: &artifact::RunPayload,
) -> Result<(), Box<dyn std::error::Error>> {
    let m = &payload.manifest;
    let checks = [
        (artifact::MEMBER_COMPOSE_LOCK, &payload.compose_lock),
        (artifact::MEMBER_KERNEL, &payload.kernel),
        (artifact::MEMBER_INITRAMFS, &payload.initramfs),
    ];
    for (name, bytes) in checks {
        let expected = m
            .member(name)
            .ok_or_else(|| format!("{path}: manifest has no hash for member {name:?}"))?;
        let actual = artifact::sha256_hex(bytes);
        if actual != expected.sha256 {
            return Err(format!(
                "{path}: member {name:?} hash MISMATCH (manifest {}, actual {}) — \
                 artifact is corrupt or tampered; refusing to boot (pass --no-verify to override)",
                expected.sha256, actual
            )
            .into());
        }
    }
    Ok(())
}

// ---- `dvmm inspect`: print manifest.json (manifest member only) -------------

fn cmd_inspect(path: &str) -> Result<i32, Box<dyn std::error::Error>> {
    // Reads ONLY the first member (manifest.json) — never the big kernel/initramfs.
    let manifest = artifact::read_manifest(path)?;
    let json = manifest.to_canonical_json()?;
    // manifest.json to stdout verbatim (a machine can pipe it to jq).
    use std::io::Write;
    std::io::stdout().write_all(&json)?;
    Ok(0)
}

// ---- `dvmm verify`: member hashes vs manifest + the file identity -----------

fn cmd_verify(path: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let report = artifact::verify(path)?;
    // Identity first (always printed, even on failure — it names the file checked).
    println!("dvmm-artifact: {path}");
    println!("sha256 (identity): {}", report.file_sha256);
    for c in &report.checks {
        println!(
            "  {:<16} {}  {}",
            c.name,
            if c.ok { "OK  " } else { "FAIL" },
            if c.ok {
                c.actual.clone()
            } else {
                format!("expected {} got {}", c.expected, c.actual)
            }
        );
    }
    for name in &report.missing {
        println!("  {name:<16} MISSING (in manifest, absent from archive)");
    }
    if report.all_ok() {
        println!("VERIFY OK: all {} member hashes match the manifest", report.checks.len());
        Ok(0)
    } else {
        println!("VERIFY FAIL: member-hash mismatch or missing member");
        Ok(1)
    }
}

// ============================================================================
// The shared boot path — used by BOTH `boot` (raw) and `run` (artifact).
// ============================================================================

/// Set up the VM from in-memory kernel + initramfs byte buffers and the resolved
/// [`EffectiveConfig`], then hand off to the vCPU loop. The kernel is parsed from
/// bytes and the initramfs written straight into guest RAM — no temp files.
fn boot_and_run(
    kernel: &[u8],
    initrd: &[u8],
    eff: &EffectiveConfig,
    scenario: Option<ScenarioSetup>,
) -> Result<RunOutcome, Box<dyn std::error::Error>> {
    let mem_size = eff.mem_mib * 1024 * 1024;
    let kvm = Kvm::new()?;

    // Startup FF mode statement (item 1) + interactive banner. Always printed;
    // isatty gates ONLY the banner/advisory wording, never the FF decision.
    {
        let tty = serial::stdin_is_tty();
        let mode = ff_mode_statement(eff.fast_forward, eff.ff_explicit);
        if tty {
            dlog!(
                "[dvmm] {mode} — quit the guest with `poweroff` or `reboot` \
                 (`exit` now gives a fresh shell)"
            );
        } else {
            dlog!("[dvmm] {mode}");
        }
        if eff.fast_forward && tty {
            dlog!(
                "[dvmm][WARN] fast-forward is ON at an interactive console — it \
                 races the guest clock and pins a host core; pass `--ff off` for \
                 real-time."
            );
        }
    }

    // The EFFECTIVE-CONFIG line (spec item 3): the resolved knobs with per-knob
    // provenance (baked < flag). This is the future record-log preamble — every
    // run emits it, so a harness/log can reconstruct exactly what was run and why.
    dlog!("[dvmm] effective-config: {}", eff.provenance);

    // --- VM ---
    let vm = kvm.create_vm()?;
    vm.set_tss_address(arch::KVM_TSS_ADDRESS as usize)?;

    // --- Guest memory ---
    let guest_mem = memory::create_guest_memory(mem_size as usize)?;
    memory::register_with_kvm(&vm, &guest_mem)?;

    // No in-kernel irqchip and no in-kernel PIT: the userspace LAPIC + IOAPIC we
    // own serve all interrupt state, and the LAPIC one-shot/periodic timer (MMIO)
    // is the tick. (We deliberately do NOT route IA32_TSC_DEADLINE via an MSR
    // filter: on this host a KVM WRMSR fastpath no-ops 0x6E0 before the filter
    // when there is no in-kernel LAPIC, so TSC-deadline is unadvertised in CPUID.)

    // --- vCPU ---
    let vcpu = vm.create_vcpu(0)?;

    // CPUID: mask kvmclock/MWAIT/x2APIC/TSC-deadline, expose invariant TSC, pass
    // through the frequency leaves (0x15/0x16).
    let supported = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    let filtered = cpuid::filter_cpuid(&supported)?;
    vcpu.set_cpuid2(&filtered)?;

    // Boot MSRs (sets the guest's initial IA32_TSC, hence the TSC offset).
    vcpu.set_msrs(&msrs::boot_msrs()?)?;

    // --- Virtual clock authority (read the TSC offset + freq once) ---
    let clock = VirtualClock::from_vcpu(&vcpu)
        .map_err(|e| format!("virtual clock unavailable (need kernel >= 5.16): {e}"))?;
    {
        let a = clock.vtsc_now();
        let b = clock.vtsc_now();
        assert!(b >= a, "vtsc went backwards ({a} -> {b}) — clock is not sane");
        dlog!(
            "[dvmm] virtual clock: tsc_khz={} (~{} MHz) tsc_offset={} vtsc_now={}",
            clock.freq().khz(),
            clock.freq().hz() / 1_000_000,
            clock.tsc_offset(),
            b,
        );
    }

    // --- Load kernel + initrd (straight from the in-memory buffers) ---
    let entry = boot::load_kernel(&guest_mem, kernel)?;
    let initrd_cfg = boot::load_initrd(&guest_mem, initrd, mem_size)?;
    dlog!(
        "[dvmm] vmlinux entry {:#x}, initramfs {} bytes @ {:#x}",
        entry.0, initrd_cfg.size, initrd_cfg.address
    );

    // --- vCPU registers / segments / page tables ---
    regs::setup_fpu(&vcpu)?;
    regs::setup_regs(&vcpu, entry.0)?;
    regs::setup_sregs(&guest_mem, &vcpu)?;
    // (LINT routing is handled by the userspace LAPIC, which holds LINT0/1 as
    // register storage; no in-kernel LAPIC to program.)

    // --- System config (cmdline, MPTable, E820, zero page) ---
    boot::configure_system(&guest_mem, &eff.cmdline, Some(initrd_cfg), mem_size, 1)?;

    // --- Serial console ---
    let (serial, serial_drain) = serial::new_serial()?;
    let raw_term = serial::RawTerminal::enable(0);
    // From here our own log lines must snap to column 0 with CRLF (raw mode turns
    // off the tty's ONLCR). Flip the flag only if raw mode actually took effect
    // (a tty); the cooked-mode boot lines above already printed with a plain "\n".
    if raw_term.is_raw() {
        RAW_TTY.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    run_user_backend(
        vcpu,
        serial,
        serial_drain,
        clock,
        eff.fast_forward,
        eff.max_jump_secs,
        eff.max_virtual_time_secs,
        eff.metrics_out.clone(),
        scenario,
    )
}

/// Queue an interrupt `vector` for injection on the next KVM entry
/// (`KVM_INTERRUPT`). Userspace-irqchip only.
fn inject_interrupt(vcpu: &VcpuFd, vector: u8) -> std::io::Result<()> {
    let irq = kvm_interrupt {
        irq: u32::from(vector),
    };
    // SAFETY: valid vCPU fd; KVM_INTERRUPT reads the kvm_interrupt struct and
    // writes nothing back.
    let ret = unsafe { ioctl_with_ref(vcpu, KVM_INTERRUPT(), &irq) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// =====================================================================
// The vCPU run loop (userspace LAPIC/IOAPIC we own)
// =====================================================================

#[allow(clippy::too_many_arguments)]
fn run_user_backend(
    mut vcpu: VcpuFd,
    serial: serial::SharedSerial,
    serial_drain: serial::EventFdTrigger,
    clock: VirtualClock,
    fast_forward: bool,
    max_jump_secs: f64,
    max_virtual_time_secs: Option<f64>,
    metrics_out: Option<String>,
    scenario_setup: Option<ScenarioSetup>,
) -> Result<RunOutcome, Box<dyn std::error::Error>> {
    // The devices we now own, all on this thread. The LAPIC timer counts at the
    // core-crystal frequency the guest derives from CPUID 0x15 (which we pass
    // through). counts->TSC-cycles uses the EXACT CPUID-0x15 integer ratio
    // EBX/EAX (see apic_timer_tsc_ratio) — no float — so the tick fires at the
    // correct virtual time, bit-identically every run.
    let apic_bus_hz = apic_bus_hz_from_cpuid();
    let (ratio_num, ratio_den) = apic_timer_tsc_ratio(clock.freq().hz());
    dlog!(
        "[dvmm] userspace LAPIC timer: {ratio_num}/{ratio_den} TSC cycles/count \
         (CPUID 0x15 EBX/EAX), crystal ~{} MHz",
        apic_bus_hz / 1_000_000
    );
    // clock is cloned into the LAPIC and PIT; all clones share the offset cell,
    // so a fast-forward bump (below) moves every consumer's view at once.
    let mut lapic = Lapic::new(clock.clone(), ratio_num, ratio_den);
    let mut ioapic = Ioapic::new(mptable_ioapic_id());
    let mut pic = PicStub::new();
    // PIT stub: interrupt-silent calibration/counter backstop (serves 0x40-0x43
    // + ELCR now that the in-kernel PIT is gone).
    let mut pit = PitStub::new(clock.clone());
    // The one event queue: mirrors the LAPIC's single armed deadline so the park
    // knows when to wake. No timer state lives outside it + the LAPIC.
    let mut events: events::EventQueue<TimerKind> = events::EventQueue::new();
    let mut parker = park::Parker::new()?;

    // TEST-1a control channel: the 2nd 16550 (COM2 / ttyS1). Always present so the
    // guest agent's ttyS1 always works; the scenario engine is what is optional.
    let mut com2 = control::ControlChannel::new()?;
    // The scenario engine (only for `dvmm test`). Built here so it has the virtual
    // clock's frequency for its cycles<->duration math.
    let mut engine: Option<scenario::ScenarioEngine> = match scenario_setup {
        Some(s) => Some(scenario::ScenarioEngine::new(s.scenario, clock.freq(), s.meta)?),
        None => None,
    };

    // Fast-forward state (Step 4): the jump-cost/speedup accounting + the
    // single-jump sanity bound. `None` when FF is off (the 3b real-wait park).
    let mut ff_state = if fast_forward {
        Some(FfState::new(clock.freq().hz(), max_jump_secs))
    } else {
        None
    };

    // Interactive console: a human at a tty with no harness context (no metrics
    // sink and no virtual-time horizon). The periodic HLT-rate / fast-forward
    // rollup below is a perf/Step-4 metric for demo + harness runs, NOT interactive
    // console noise — so when interactive we suppress its PERIODIC emission (it
    // would interrupt a human's prompt every ~15s). isatty gates ONLY this
    // suppression, never any time behavior (Fable-locked); it is still emitted for
    // every harness path (non-tty, `--metrics-out`, or a horizon set), and the
    // on-stop summary + metrics file are unaffected either way.
    let interactive =
        serial::stdin_is_tty() && metrics_out.is_none() && max_virtual_time_secs.is_none();

    // Idle-observability (a Step-4 hop-cost input, not a diagnostic): how often
    // the guest HLTs. Reported on stop, and rolled up every ~15s of wall time so
    // it is observable during long-running workloads that never exit.
    let mut hlt_count: u64 = 0;
    let start = std::time::Instant::now();
    let mut last_report = start;
    let mut last_report_hlts: u64 = 0;
    const HLT_REPORT_PERIOD: std::time::Duration = std::time::Duration::from_secs(15);

    // Virtual-time span for the speedup metric (gate 2): virtual seconds elapsed
    // / real seconds elapsed. Sampled once here, again at stop.
    let vtsc_start = clock.vtsc_now();

    // `--max-virtual-time` horizon: the vtsc at which the run must stop, as an
    // absolute deadline `vtsc_start + budget`. Enforced NOT as a loop check but
    // as a `(vtsc, StopRun)` entry pushed into the one event queue each boundary
    // (see `service_timers`); when vtsc reaches it, `pop_due` fires StopRun and
    // the run terminates through the same drain path a timer does. This makes the
    // horizon deterministic + replayable (a pure function of vtsc), which the
    // future determinism phase needs. A wedged guest fast-forwards to any sane
    // horizon in seconds of real time; a legitimate long idle also reaches it,
    // which is correct — a run has a bounded virtual duration.
    let horizon_vtsc: Option<u64> = max_virtual_time_secs.map(|secs| {
        let cycles = (secs * clock.freq().hz() as f64) as u64;
        vtsc_start.wrapping_add(cycles)
    });
    if let Some(h) = horizon_vtsc {
        dlog!(
            "[dvmm] max-virtual-time horizon: {:.3}s of virtual time \
             (vtsc {vtsc_start} -> {h}), as a (vtsc, StopRun) queue event",
            max_virtual_time_secs.unwrap(),
        );
    }

    // Why we stopped; set at every break site (all breaks assign it), read after
    // the loop to pick the process exit code.
    let stop_reason: StopReason;

    dlog!(
        "[dvmm] starting vCPU on the USERSPACE irqchip, fast-forward {} \
         (Ctrl-A is passed to the guest; kill from another terminal to stop)\n",
        if fast_forward { "ON" } else { "OFF" }
    );

    // Arm the scenario (records run_start + the agent-ready backstop deadline).
    if let Some(e) = engine.as_mut() {
        e.start(vtsc_start);
    }

    loop {
        // (1) Fire any due guest timer + the horizon + the scenario deadline, then
        //     reconcile the queue to the LAPIC's current armed deadline. A fired
        //     StopRun stops the run; a fired ScenarioStep drives the engine (which
        //     may deliver a command as a queue event — never a side channel).
        let now = clock.vtsc_now();
        let scn_deadline = engine.as_ref().and_then(|e| e.next_deadline());
        let fired = service_timers(&mut lapic, &mut events, horizon_vtsc, scn_deadline, now);
        if fired.horizon {
            if let Some(e) = engine.as_mut() {
                e.record_abort(now, "scenario did not complete before the virtual-time horizon");
                stop_reason = StopReason::Scenario;
                break;
            }
            stop_reason = StopReason::Horizon;
            report_horizon(ff_state.as_ref(), start);
            break;
        }
        if fired.scenario {
            if let Some(e) = engine.as_mut() {
                let _ = e.on_due(now, &mut com2);
                com2.pump(&mut lapic, &ioapic);
                if e.is_finished() {
                    stop_reason = StopReason::Scenario;
                    break;
                }
            }
        }

        // (1b) Stream any in-flight command bytes to the guest, then drain the
        //      agent's reply lines and feed them to the engine (advancing steps /
        //      deciding the verdict). With no scenario, discard agent chatter to
        //      keep the capture buffer bounded.
        com2.pump(&mut lapic, &ioapic);
        if let Some(e) = engine.as_mut() {
            let mut finished = false;
            while let Some(line) = com2.poll_line() {
                let _ = e.on_reply(&line, clock.vtsc_now(), &mut com2);
                com2.pump(&mut lapic, &ioapic);
                if e.is_finished() {
                    finished = true;
                    break;
                }
            }
            if finished {
                stop_reason = StopReason::Scenario;
                break;
            }
        } else {
            while com2.poll_line().is_some() {}
        }

        // (2) Sync task priority from the guest's CR8 (mov %cr8 path).
        let cr8 = vcpu.get_kvm_run().cr8;
        lapic.sync_tpr_from_cr8(cr8);

        // (3) Injection: if the LAPIC has a deliverable vector and the guest can
        //     take it now, hand it to KVM; otherwise request an IRQ window.
        let deliverable = lapic.deliverable_vector();
        let (ready, if_flag) = {
            let r = vcpu.get_kvm_run();
            (r.ready_for_interrupt_injection, r.if_flag)
        };
        let mut injected = false;
        if let Some(vec) = deliverable {
            if ready != 0 && if_flag != 0 {
                inject_interrupt(&vcpu, vec)?;
                lapic.ack_injected(vec);
                injected = true;
            }
        }
        {
            let r = vcpu.get_kvm_run();
            r.request_interrupt_window = u8::from(deliverable.is_some() && !injected);
            r.cr8 = u64::from(lapic.tpr() >> 4);
        }

        // (4) Run.
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(err) => {
                let e = err.errno();
                if e == libc::EINTR || e == libc::EAGAIN {
                    continue;
                }
                return Err(format!("KVM_RUN failed: {err}").into());
            }
        };

        // (5) Handle the exit.
        match exit {
            VcpuExit::IoOut(port, data) => {
                if is_serial(port) {
                    let mut s = serial.lock().unwrap();
                    for &b in data {
                        let _ = s.write((port - arch::SERIAL_PORT_BASE) as u8, b);
                    }
                    drop(s);
                    if serial_drain.drain().is_ok() {
                        raise_irq(&mut lapic, &ioapic, arch::SERIAL_IRQ);
                    }
                } else if control::ControlChannel::handles(port) {
                    // COM2 / ttyS1 — the control channel (agent TX = its replies).
                    for &b in data {
                        com2.pio_write(port, b, &mut lapic, &ioapic);
                    }
                } else if PitStub::handles(port) {
                    for &b in data {
                        pit.write(port, b);
                    }
                } else if PicStub::handles(port) {
                    for &b in data {
                        pic.write(port, b);
                    }
                } else if port == arch::POST_PORT {
                    // POST checkpoint code: ignore.
                }
            }
            VcpuExit::IoIn(port, data) => {
                if is_serial(port) {
                    let mut s = serial.lock().unwrap();
                    for b in data.iter_mut() {
                        *b = s.read((port - arch::SERIAL_PORT_BASE) as u8);
                    }
                    drop(s);
                    if serial_drain.drain().is_ok() {
                        raise_irq(&mut lapic, &ioapic, arch::SERIAL_IRQ);
                    }
                } else if control::ControlChannel::handles(port) {
                    // COM2 / ttyS1 — the agent reading a command (RBR) or a status
                    // register. Draining the RX FIFO here frees room for `pump`.
                    for b in data.iter_mut() {
                        *b = com2.pio_read(port, &mut lapic, &ioapic);
                    }
                } else if PitStub::handles(port) {
                    for b in data.iter_mut() {
                        *b = pit.read(port);
                    }
                } else if PicStub::handles(port) {
                    for b in data.iter_mut() {
                        *b = pic.read(port);
                    }
                } else {
                    for b in data.iter_mut() {
                        *b = 0xff; // open bus
                    }
                }
            }
            VcpuExit::MmioRead(addr, data) => {
                let val = if in_lapic(addr) {
                    lapic.mmio_read((addr - XAPIC_BASE) as u32)
                } else if Ioapic::handles(addr) {
                    ioapic.mmio_read(addr)
                } else {
                    0
                };
                write_u32_le(data, val);
            }
            VcpuExit::MmioWrite(addr, data) => {
                let val = read_u32_le(data);
                if in_lapic(addr) {
                    lapic.mmio_write((addr - XAPIC_BASE) as u32, val);
                } else if Ioapic::handles(addr) {
                    ioapic.mmio_write(addr, val);
                }
            }
            VcpuExit::IrqWindowOpen => {
                // The window is open; the next loop iteration injects.
            }
            VcpuExit::Hlt => {
                // A HLT taken with interrupts disabled (IF=0) can NEVER wake: no
                // interrupt is deliverable, so it is a terminal halt — where the
                // guest's `poweroff` ends when there is no ACPI (the kernel
                // finishes in `cli; hlt`, "System halted"). Recognize it as a
                // clean guest-terminal stop (status 0), distinct from an ordinary
                // idle `sti; hlt` (IF=1) which parks and waits/jumps for its next
                // timer. Checked here, before the park, in BOTH FF modes.
                if vcpu.get_kvm_run().if_flag == 0 {
                    dlog!(
                        "\n[dvmm] STOP: guest halted (power off) — HLT with \
                         interrupts disabled (IF=0), a terminal halt that can \
                         never wake."
                    );
                    stop_reason = StopReason::GuestHalt;
                    break;
                }
                hlt_count += 1;
                // Periodic rollup: kept for harness/metrics/horizon runs; suppressed
                // when interactive (Task 4) so it never interrupts a human's prompt.
                if !interactive && last_report.elapsed() >= HLT_REPORT_PERIOD {
                    let win = last_report.elapsed().as_secs_f64();
                    let n = hlt_count - last_report_hlts;
                    dlog!(
                        "[dvmm] HLT-exit rate: {:.1}/s ({n} in {win:.0}s; {hlt_count} total)",
                        n as f64 / win
                    );
                    if let Some(ff) = ff_state.as_ref() {
                        let virt_s = ff.virtual_secs_since(vtsc_start, clock.vtsc_now());
                        let real_s = start.elapsed().as_secs_f64().max(1e-9);
                        dlog!(
                            "[dvmm] fast-forward: {} jumps, {:.0} virtual-s in {:.1} real-s \
                             = {:.0}x; per-hop mean {:.1}us max {:.1}us; max Δ {:.3}s",
                            ff.jumps,
                            virt_s,
                            real_s,
                            virt_s / real_s,
                            ff.mean_hop_ns() as f64 / 1000.0,
                            ff.hop_ns_max as f64 / 1000.0,
                            ff.max_delta_secs(),
                        );
                    }
                    last_report = std::time::Instant::now();
                    last_report_hlts = hlt_count;
                }
                let outcome = park_until_deliverable(
                    &mut lapic,
                    &ioapic,
                    &mut events,
                    &serial,
                    &serial_drain,
                    &mut parker,
                    &clock,
                    &vcpu,
                    horizon_vtsc,
                    ff_state.as_mut(),
                    &mut com2,
                    engine.as_mut(),
                )?;
                match outcome {
                    ParkOutcome::Horizon => {
                        if let Some(e) = engine.as_mut() {
                            e.record_abort(
                                clock.vtsc_now(),
                                "scenario did not complete before the virtual-time horizon",
                            );
                            stop_reason = StopReason::Scenario;
                            break;
                        }
                        stop_reason = StopReason::Horizon;
                        report_horizon(ff_state.as_ref(), start);
                        break;
                    }
                    ParkOutcome::ScenarioDone => {
                        stop_reason = StopReason::Scenario;
                        break;
                    }
                    ParkOutcome::Deliverable => {}
                }
            }
            VcpuExit::Shutdown => {
                // Guest-initiated: a triple fault (crash, or panic/`reboot=t`).
                // Distinct from the VMM's own horizon stop below — a testing
                // platform wants "guest panicked/rebooted" as a first-class
                // outcome (see StopReason -> exit code).
                dlog!(
                    "\n[dvmm] STOP: guest-initiated shutdown/reboot \
                     (KVM_EXIT_SHUTDOWN, triple fault — e.g. panic+reboot or `reboot -f`)."
                );
                stop_reason = StopReason::GuestShutdown;
                break;
            }
            VcpuExit::SystemEvent(type_, _) => {
                // Guest-initiated reset/shutdown/crash via a system event.
                dlog!(
                    "\n[dvmm] STOP: guest-initiated system event \
                     (reset/shutdown/crash, type {type_})."
                );
                stop_reason = StopReason::GuestSystemEvent;
                break;
            }
            VcpuExit::FailEntry(reason, cpu) => {
                return Err(
                    format!("KVM_EXIT_FAIL_ENTRY: reason={reason:#x} cpu={cpu}").into(),
                );
            }
            VcpuExit::InternalError => return Err("KVM_EXIT_INTERNAL_ERROR".into()),
            other => {
                dlog!("[dvmm] unhandled KVM exit: {other:?}");
            }
        }
    }

    let secs = start.elapsed().as_secs_f64();
    dlog!(
        "[dvmm] userspace backend stopped: {hlt_count} HLT exits over {secs:.1}s \
         ({:.1}/s)",
        hlt_count as f64 / secs.max(0.001)
    );
    if let Some(ff) = ff_state.as_ref() {
        let virt_s = ff.virtual_secs_since(vtsc_start, clock.vtsc_now());
        let real_s = secs.max(1e-9);
        dlog!(
            "[dvmm] FAST-FORWARD SUMMARY: {} jumps; virtual {:.1}s in real {:.1}s = \
             {:.1}x speedup; per-hop cost mean {:.1}us / max {:.1}us; \
             largest single jump Δ = {:.3}s (bound {}s)",
            ff.jumps,
            virt_s,
            real_s,
            virt_s / real_s,
            ff.mean_hop_ns() as f64 / 1000.0,
            ff.hop_ns_max as f64 / 1000.0,
            ff.max_delta_secs(),
            ff.max_jump_secs,
        );
        dlog!("[dvmm] {}", ff.hist.summary());
        dlog!(
            "[dvmm] per-hop cost p99 {:.1}us; real-vs-virtual: {:.1}% executing / \
             {:.3}% jumping ({:.1} real-exec ms per virtual-hour — busy-wait tripwire)",
            ff.hop_p99_ns() as f64 / 1000.0,
            {
                let jr = ff.jump_real_secs();
                (secs - jr).max(0.0) / secs.max(1e-9) * 100.0
            },
            ff.jump_real_secs() / secs.max(1e-9) * 100.0,
            {
                let jr = ff.jump_real_secs();
                let vh = (ff.virtual_secs_since(vtsc_start, clock.vtsc_now()) / 3600.0).max(1e-12);
                (secs - jr).max(0.0) * 1000.0 / vh
            },
        );

        // Machine-parseable per-run metrics for the comparison harness.
        if let Some(path) = metrics_out.as_deref() {
            let report =
                ff.metrics_report(stop_reason, vtsc_start, clock.vtsc_now(), secs, hlt_count);
            match std::fs::write(path, &report) {
                Ok(()) => dlog!("[dvmm] wrote per-run metrics to {path}"),
                Err(e) => dlog!("[dvmm][WARN] could not write --metrics-out {path}: {e}"),
            }
        }
    } else if let Some(path) = metrics_out.as_deref() {
        // FF off: no jumps to account for; leave a clear stub so a harness that
        // always passes --metrics-out gets a well-formed, unambiguous file.
        let _ = std::fs::write(
            path,
            format!(
                "# dvmm per-run metrics (fast-forward OFF — no jump accounting)\n\
                 schema 1\nstop_reason {}\nfast_forward off\n",
                stop_reason.as_str()
            ),
        );
    }

    // TEST-1a: finalize the scenario — emit ff_stats + run_end to the JSONL,
    // write the JSON report, print the human summary, and return the verdict's
    // exit code (0 pass / 1 assertion fail / 2 infra). If the guest died before
    // the scenario finished, that is an infrastructure error (exit 2).
    if let Some(mut e) = engine {
        let now = clock.vtsc_now();
        if !e.is_finished() {
            let reason = match stop_reason {
                StopReason::GuestShutdown | StopReason::GuestSystemEvent | StopReason::GuestHalt => {
                    "guest stopped before the scenario completed"
                }
                StopReason::Horizon => "scenario did not complete before the virtual-time horizon",
                _ => "run ended before the scenario reached a verdict",
            };
            e.record_abort(now, reason);
        }
        let ff_sum = ff_state
            .as_ref()
            .map(|ff| scenario::FfSummary {
                jumps: ff.jumps,
                virtual_seconds: ff.virtual_secs_since(vtsc_start, now),
                speedup: ff.virtual_secs_since(vtsc_start, now) / secs.max(1e-9),
                per_hop_mean_us: ff.mean_hop_ns() as f64 / 1000.0,
                max_delta_s: ff.max_delta_secs(),
            })
            .unwrap_or_default();
        let code = e.finalize(&ff_sum, secs, now);
        return Ok(RunOutcome {
            stop: stop_reason,
            exit_code: code,
        });
    }

    Ok(RunOutcome {
        stop: stop_reason,
        exit_code: stop_reason.exit_code(),
    })
}

/// Print the `--max-virtual-time` diagnostic dump at a horizon stop: total jump
/// count, jump rate, largest Δ, and the Δvtsc histogram (its tail is the wedge
/// signature). Distinguishes this VMM policy stop from a guest-initiated one.
fn report_horizon(ff: Option<&FfState>, start: std::time::Instant) {
    dlog!(
        "\n[dvmm] STOP: --max-virtual-time horizon reached — VMM virtual-time budget \
         (a deterministic (vtsc, StopRun) queue event, NOT a guest-initiated stop)."
    );
    if let Some(ff) = ff {
        let real_s = start.elapsed().as_secs_f64().max(1e-9);
        dlog!(
            "[dvmm] HORIZON DIAGNOSTIC: {} jumps in {:.2}s real = {:.0} jumps/s; \
             max single Δ {:.3}s; {}",
            ff.jumps,
            real_s,
            ff.jumps as f64 / real_s,
            ff.max_delta_secs(),
            ff.hist.summary(),
        );
    }
}

/// Reconcile the event queue to the LAPIC's single armed deadline (the LAPIC is
/// the authority) plus the optional virtual-time `horizon`, then fire everything
/// due through the queue. Keeping the fire path in the queue is the whole point
/// of `events.rs`: every guest timer — and the `--max-virtual-time` horizon — is
/// a `(vtsc, event)` entry, and Step 4 drains the same queue after a time-jump.
///
/// Which special (non-LAPIC) queue events fired this drain. `horizon` = the
/// `--max-virtual-time` StopRun; `scenario` = a `(vtsc, ScenarioStep)` deadline
/// (TEST-1a) — the caller then drives the scenario engine.
#[derive(Default, Clone, Copy)]
struct Fired {
    horizon: bool,
    scenario: bool,
}

fn service_timers(
    lapic: &mut Lapic,
    events: &mut events::EventQueue<TimerKind>,
    horizon: Option<u64>,
    scenario: Option<u64>,
    now: u64,
) -> Fired {
    events.clear();
    if let Some(dl) = lapic.timer_deadline() {
        events.push(dl, TimerKind::LapicDeadline);
    }
    if let Some(h) = horizon {
        events.push(h, TimerKind::StopRun);
    }
    if let Some(sd) = scenario {
        events.push(sd, TimerKind::ScenarioStep);
    }
    let mut fired = Fired::default();
    while let Some(ev) = events.pop_due(now) {
        // Queue-discipline assertion (gate 5): an event is only ever serviced at
        // or after its scheduled vtsc — never before. Always-on (release too).
        assert!(
            ev.deadline <= now,
            "queue discipline violated: event deadline {} fired at now {}",
            ev.deadline,
            now
        );
        match ev.payload {
            TimerKind::LapicDeadline => {
                lapic.fire_timer_if_due(now);
            }
            TimerKind::StopRun => {
                fired.horizon = true;
            }
            TimerKind::ScenarioStep => {
                fired.scenario = true;
            }
        }
    }
    fired
}

/// How a park returned: an interrupt became deliverable (wake the guest), the
/// `--max-virtual-time` horizon fired (stop), or the scenario reached its verdict
/// while parked (TEST-1a — stop).
enum ParkOutcome {
    Deliverable,
    Horizon,
    ScenarioDone,
}

/// Idle park: the guest HLTed, so make it wait until an interrupt becomes
/// deliverable. This is the one place that turns a virtual-time deadline into
/// either a real wait (FF off, 3b behavior) or a fast-forward JUMP (FF on).
/// Console input is read here (the vCPU thread owns it). The wake path itself —
/// IRR set, deliverable_vector, the caller's injection — is unchanged either way.
///
/// The `horizon` rides along so the FF jump target respects it and a horizon
/// reached while parked returns [`ParkOutcome::Horizon`] instead of spinning.
#[allow(clippy::too_many_arguments)]
fn park_until_deliverable(
    lapic: &mut Lapic,
    ioapic: &Ioapic,
    events: &mut events::EventQueue<TimerKind>,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    parker: &mut park::Parker,
    clock: &VirtualClock,
    vcpu: &VcpuFd,
    horizon: Option<u64>,
    ff: Option<&mut FfState>,
    com2: &mut control::ControlChannel,
    engine: Option<&mut scenario::ScenarioEngine>,
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    match ff {
        Some(ff) => fast_forward_until_deliverable(
            lapic, ioapic, events, serial, serial_drain, parker, clock, vcpu, horizon, ff, com2,
            engine,
        ),
        None => real_wait_until_deliverable(
            lapic, ioapic, events, serial, serial_drain, parker, clock, horizon, com2, engine,
        ),
    }
}

/// Handle a scenario deadline that fired inside the park: drive the engine (which
/// may deliver a command — raising IRQ3 makes an interrupt deliverable, so the
/// park then returns `Deliverable` via its usual check) and report if the run is
/// now decided. `Some(ScenarioDone)` means the caller must stop.
fn park_scenario_fired(
    fired: Fired,
    now: u64,
    lapic: &mut Lapic,
    ioapic: &Ioapic,
    com2: &mut control::ControlChannel,
    engine: &mut Option<&mut scenario::ScenarioEngine>,
) -> Option<ParkOutcome> {
    if fired.scenario {
        if let Some(e) = engine.as_deref_mut() {
            let _ = e.on_due(now, com2);
            com2.pump(lapic, ioapic);
            if e.is_finished() {
                return Some(ParkOutcome::ScenarioDone);
            }
        }
    }
    None
}

/// FF OFF: the 3b real-wait park — sleep in `ppoll` on a `timerfd` + stdin until
/// the next deadline elapses in REAL time or console input arrives. If the next
/// deadline is the horizon, the wait elapses to it and StopRun fires (a
/// legitimate long idle reaching the virtual-time budget — correct).
#[allow(clippy::too_many_arguments)]
fn real_wait_until_deliverable(
    lapic: &mut Lapic,
    ioapic: &Ioapic,
    events: &mut events::EventQueue<TimerKind>,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    parker: &mut park::Parker,
    clock: &VirtualClock,
    horizon: Option<u64>,
    com2: &mut control::ControlChannel,
    mut engine: Option<&mut scenario::ScenarioEngine>,
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    loop {
        let now = clock.vtsc_now();
        let scn_deadline = engine.as_deref().and_then(|e| e.next_deadline());
        let fired = service_timers(lapic, events, horizon, scn_deadline, now);
        if fired.horizon {
            return Ok(ParkOutcome::Horizon);
        }
        if let Some(o) = park_scenario_fired(fired, now, lapic, ioapic, com2, &mut engine) {
            return Ok(o);
        }
        if lapic.deliverable_vector().is_some() {
            return Ok(ParkOutcome::Deliverable);
        }
        // Real nanoseconds until the next deadline (None => wait on input only).
        let timeout_ns = events.peek_deadline().map(|dl| {
            let now2 = clock.vtsc_now();
            if dl <= now2 {
                0
            } else {
                clock.freq().cycles_to_ns(dl - now2)
            }
        });
        let wakes = parker.park(timeout_ns)?;
        if wakes.input {
            service_console_input(parker, serial, serial_drain, lapic, ioapic);
        }
        // A timer wake loops back: service_timers fires it and we re-check.
    }
}

/// FF ON: fast-forward park — instead of sleeping until the next deadline, JUMP
/// virtual time to it by bumping the cached TSC offset (write-through to KVM),
/// fire everything now due, and loop. The guest experiences the elapsed virtual
/// time instantly. Idle console input is serviced first (non-blocking) at the
/// top of every iteration, so a quiet console never blocks a jump.
#[allow(clippy::too_many_arguments)]
fn fast_forward_until_deliverable(
    lapic: &mut Lapic,
    ioapic: &Ioapic,
    events: &mut events::EventQueue<TimerKind>,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    parker: &mut park::Parker,
    clock: &VirtualClock,
    vcpu: &VcpuFd,
    horizon: Option<u64>,
    ff: &mut FfState,
    com2: &mut control::ControlChannel,
    mut engine: Option<&mut scenario::ScenarioEngine>,
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    loop {
        // stdin precedence: service any pending console input up-front, without
        // blocking, so an idle console can never stall a jump.
        if parker.stdin_open() && parker.stdin_ready()? {
            service_console_input(parker, serial, serial_drain, lapic, ioapic);
        }

        // Fire any due timers + the horizon + the scenario deadline, reconcile the
        // queue to the LAPIC's armed deadline. A scenario deadline that fires here
        // may deliver a command (raising IRQ3), which the deliverable check below
        // then catches — so a jumped-to `at:` wakes the agent exactly on time.
        let now = clock.vtsc_now();
        let scn_deadline = engine.as_deref().and_then(|e| e.next_deadline());
        let fired = service_timers(lapic, events, horizon, scn_deadline, now);
        if fired.horizon {
            return Ok(ParkOutcome::Horizon);
        }
        if let Some(o) = park_scenario_fired(fired, now, lapic, ioapic, com2, &mut engine) {
            return Ok(o);
        }
        if lapic.deliverable_vector().is_some() {
            return Ok(ParkOutcome::Deliverable); // unchanged 3b wake path.
        }

        // The next scheduled event decides the jump target. This is the min of
        // the LAPIC deadline and the horizon, so a horizon nearer than the next
        // tick becomes the jump target and StopRun fires on the next iteration.
        let next = match events.peek_deadline() {
            Some(dl) => dl,
            None => {
                // Nothing armed and nothing deliverable: there is no virtual-time
                // deadline to jump to. An idle guest always has its tick armed, so
                // this is a corner case (e.g. a console-only wait). Fall back to a
                // real wait on stdin rather than spin.
                let wakes = parker.park(None)?;
                if wakes.input {
                    service_console_input(parker, serial, serial_drain, lapic, ioapic);
                }
                continue;
            }
        };

        // JUMP. Sample the host TSC ONCE (h) so the post-condition is exact.
        let hop_start = std::time::Instant::now();
        let h = vtsc::host_rdtsc();
        let now_h = clock.vtsc_from_host(h);
        if next <= now_h {
            // Became due while we were working: loop and let service_timers fire it.
            continue;
        }
        let delta = next - now_h; // vtsc cycles to advance, > 0

        // Gate 3: single-jump sanity bound. Expected never to trip on a real guest
        // timer. The horizon is EXEMPT: it is an operator-set virtual-time budget,
        // not a guest deadline, so jumping straight to it (e.g. a deeply-idle guest
        // whose next tick is beyond both the horizon and the bound) is intended,
        // not an anomaly to abort on.
        let jumping_to_horizon = horizon == Some(next);
        if !jumping_to_horizon && delta > ff.max_jump_cycles {
            return Err(format!(
                "fast-forward jump Δ={delta} cycles (~{:.3}s) exceeds the sanity bound of {}s \
                 ({} cycles); aborting (use --max-jump-secs to raise)",
                delta as f64 / ff.tsc_hz as f64,
                ff.max_jump_secs,
                ff.max_jump_cycles,
            )
            .into());
        }

        // Advance virtual time: cached offset += delta, write-through to KVM. The
        // offset is monotonically non-decreasing (delta > 0). All clock clones
        // (LAPIC, PIT) observe the new offset immediately via the shared cell.
        clock.bump_offset(vcpu, delta as i64)?;

        // Post-condition (queue-discipline assert): landing is EXACT at the same
        // host sample h — vtsc_from_host(h) must now equal the event deadline.
        let landed = clock.vtsc_from_host(h);
        assert_eq!(
            landed, next,
            "post-bump vtsc {landed} != next event deadline {next} (Δ was {delta})"
        );

        ff.record_hop(delta, hop_start.elapsed());
        // WARN-only telemetry: surface a sustained high jump rate (the wedge
        // signature) with a histogram snapshot. NEVER stops the run.
        ff.maybe_warn_high_jump_rate();
        // Loop: service_timers fires the now-due event and the guest reprograms
        // its timer; the periodic re-arm is simply the next queue entry.
    }
}

/// Read whatever console input `ppoll`/poll signalled and feed it to the UART,
/// raising the serial RX IRQ iff the UART asserted one. EOF closes stdin so it is
/// no longer polled. Shared by the real-wait and fast-forward park paths.
fn service_console_input(
    parker: &mut park::Parker,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    lapic: &mut Lapic,
    ioapic: &Ioapic,
) {
    match read_console_input(serial) {
        // Real bytes: raise the serial RX IRQ iff the UART actually asserted an
        // interrupt (mirrors the serial-PIO path).
        Some(n) if n > 0 => {
            if serial_drain.drain().is_ok() {
                raise_irq(lapic, ioapic, arch::SERIAL_IRQ);
            }
        }
        // EOF (closed stdin / `</dev/null`): stop polling it so the park waits on
        // the timer alone instead of spinning.
        None => parker.close_stdin(),
        _ => {}
    }
}

/// Post an ISA IRQ line edge into the LAPIC via the IOAPIC RTE (masked/level
/// entries deliver nothing). Runs on the vCPU thread, at a loop boundary. Used by
/// the COM1 serial path here and the COM2 control channel (`control.rs`).
pub(crate) fn raise_irq(lapic: &mut Lapic, ioapic: &Ioapic, irq: u32) {
    let pin = isa_irq_to_ioapic_pin(irq as u8) as usize;
    if let Some(vector) = ioapic.edge_vector(pin) {
        lapic.raise(vector);
    }
}

/// Read whatever console input is ready (stdin was signalled by `ppoll`) into
/// the UART receive path. Never blocks meaningfully — data is already available.
/// Returns `Some(n)` bytes read, or `None` on EOF (closed stdin).
fn read_console_input(serial: &serial::SharedSerial) -> Option<usize> {
    let mut buf = [0u8; 64];
    // SAFETY: fd 0 is valid; `buf` is writable for its whole length.
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n > 0 {
        let mut s = serial.lock().unwrap();
        let _ = s.enqueue_raw_bytes(&buf[..n as usize]);
        Some(n as usize)
    } else if n == 0 {
        None // EOF
    } else {
        Some(0) // transient error (e.g. EAGAIN): treat as "no data"
    }
}

/// The IOAPIC id we advertised in the MP table (num_cpus + 1, single vCPU => 2).
fn mptable_ioapic_id() -> u8 {
    2
}

fn in_lapic(addr: u64) -> bool {
    (XAPIC_BASE..XAPIC_BASE + XAPIC_LEN).contains(&addr)
}

fn read_u32_le(data: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    for (i, dst) in b.iter_mut().enumerate() {
        if i < data.len() {
            *dst = data[i];
        }
    }
    u32::from_le_bytes(b)
}

fn write_u32_le(data: &mut [u8], val: u32) {
    let b = val.to_le_bytes();
    for (i, dst) in data.iter_mut().enumerate() {
        if i < 4 {
            *dst = b[i];
        }
    }
}

/// Print the effective guest clock/timer CPUID profile (userspace backend) as a
/// stable, diffable text block, then return — the manifest CPUID artifact
/// (`--dump-cpuid`).
///
/// Records exactly the leaves the determinism + fast-forward guarantee hangs on:
///   * `0x15`/`0x16` — the LAPIC-timer / TSC frequency the guest derives
///     (counts→TSC cycles uses `0x15` EBX/EAX exactly, see
///     [`lapic::apic_timer_tsc_ratio`]).
///   * `0x01` ECX/EDX and `0x8000_0007` EDX — the clock-policy masks (no
///     kvmclock/MWAIT/x2APIC/TSC-deadline; invariant TSC advertised).
///
/// So a host/CPU change surfaces here as a changed line instead of a silent
/// timing difference. Per-core-volatile fields — the leaf-1 initial-APIC-ID byte
/// (EBX[31:24]) and the topology x2APIC IDs (leaves 0x0b/0x1f) — are deliberately
/// EXCLUDED so the profile is byte-stable run-to-run on one host (they reflect
/// which physical core answered the ioctl, not the guest's clock).
fn dump_cpuid(kvm: &Kvm) -> Result<(), Box<dyn std::error::Error>> {
    let supported = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    let filtered = cpuid::filter_cpuid(&supported)?;
    let entries = filtered.as_slice();
    let find = |func: u32| entries.iter().find(|e| e.function == func && e.index == 0);

    println!("# deterministic-vmm effective guest clock/timer CPUID profile (userspace backend)");
    println!("#");
    println!("# The CPUID leaves the determinism + fast-forward guarantee depends on. A host or");
    println!("# CPU change surfaces here as a changed line. Per-core-volatile fields (leaf-1");
    println!("# initial-APIC-ID byte, topology x2APIC IDs) are excluded so this is stable");
    println!("# run-to-run on one host.");
    println!("# function:index eax        ebx        ecx        edx");

    if let Some(e) = find(0x01) {
        // Mask EBX[31:24] (initial APIC ID): per-core-volatile, not clock-relevant.
        let ebx = e.ebx & 0x00ff_ffff;
        println!(
            "{:#010x}:0x00 {:#010x} {:#010x} {:#010x} {:#010x}   # feature masks: ECX \
             hypervisor/MWAIT/x2APIC/TSC-deadline cleared (EBX APIC-ID byte masked)",
            0x01u32, e.eax, ebx, e.ecx, e.edx
        );
    }
    if let Some(e) = find(0x15) {
        let (num, den, crystal) = (e.ebx, e.eax, e.ecx);
        let ratio = if den != 0 { format!("{num}/{den}={}", num / den.max(1)) } else { "n/a".into() };
        println!(
            "{:#010x}:0x00 {:#010x} {:#010x} {:#010x} {:#010x}   # TSC:crystal EBX/EAX={} \
             cyc/count; crystal {} Hz",
            0x15u32, e.eax, e.ebx, e.ecx, e.edx, ratio, crystal
        );
    }
    if let Some(e) = find(0x16) {
        println!(
            "{:#010x}:0x00 {:#010x} {:#010x} {:#010x} {:#010x}   # CPU base {} / max {} / bus {} MHz",
            0x16u32, e.eax, e.ebx, e.ecx, e.edx, e.eax, e.ebx, e.ecx
        );
    }
    if let Some(e) = find(0x8000_0007) {
        println!(
            "{:#010x}:0x00 {:#010x} {:#010x} {:#010x} {:#010x}   # invariant TSC advertised (EDX bit 8)",
            0x8000_0007u32, e.eax, e.ebx, e.ecx, e.edx
        );
    }
    Ok(())
}

fn is_serial(port: u16) -> bool {
    (arch::SERIAL_PORT_BASE..arch::SERIAL_PORT_BASE + 8).contains(&port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ff_bound_arithmetic_and_trip() {
        let tsc_hz = 3_072_000_000u64;
        let ff = FfState::new(tsc_hz, 300.0);
        assert_eq!(ff.max_jump_cycles, 300 * tsc_hz); // 300 s -> cycles, exact.
        // A 0.512 s jump (the largest a real idle guest produces here) does NOT
        // exceed a 300 s bound, but DOES exceed a tight 0.1 s bound — which is
        // exactly the abort condition (gate 3).
        let jump = (0.512 * tsc_hz as f64) as u64;
        assert!(jump <= ff.max_jump_cycles, "0.512 s must be within 300 s");
        let tight = FfState::new(tsc_hz, 0.1);
        assert!(jump > tight.max_jump_cycles, "0.512 s must exceed a 0.1 s bound");
    }

    #[test]
    fn ff_records_max_and_mean_hop() {
        let mut ff = FfState::new(3_072_000_000, 300.0);
        ff.record_hop(100, std::time::Duration::from_nanos(200));
        ff.record_hop(300, std::time::Duration::from_nanos(400));
        assert_eq!(ff.jumps, 2);
        assert_eq!(ff.max_delta_cycles, 300);
        assert_eq!(ff.mean_hop_ns(), 300); // (200 + 400) / 2
    }

    #[test]
    fn hop_cost_p99_tracks_the_tail_not_the_bulk() {
        // 99 cheap hops (~300 ns) + 1 expensive hop (~2 ms). The mean is dragged
        // up a little, but the p99 must land in the expensive tail, not the bulk.
        let mut ff = FfState::new(1_000_000_000, 300.0);
        for _ in 0..99 {
            ff.record_hop(1, std::time::Duration::from_nanos(300));
        }
        ff.record_hop(1, std::time::Duration::from_millis(2));
        assert_eq!(ff.jumps, 100);
        // p99 selects the 99th-percentile sample: the last cheap bucket boundary,
        // well below the 2 ms outlier but above the 300 ns bulk floor.
        let p99 = ff.hop_p99_ns();
        assert!(p99 >= 200, "p99 {p99} should be at/above the 300 ns bulk bucket");
        // max is exact and catches the outlier.
        assert_eq!(ff.hop_ns_max, 2_000_000);
        // An all-cheap histogram has a cheap p99.
        let mut cheap = FfState::new(1_000_000_000, 300.0);
        for _ in 0..1000 {
            cheap.record_hop(1, std::time::Duration::from_nanos(120));
        }
        assert!(cheap.hop_p99_ns() <= 200, "all-cheap p99 must stay sub-bucket");
    }

    #[test]
    fn metrics_report_is_parseable_and_accounts_real_vs_virtual() {
        // 1 GHz so 1 cycle == 1 ns. Two jumps advancing 1s of virtual time each,
        // each costing 500 ns of real time; a 4s real run.
        let mut ff = FfState::new(1_000_000_000, 300.0);
        ff.record_hop(1_000_000_000, std::time::Duration::from_nanos(500));
        ff.record_hop(1_000_000_000, std::time::Duration::from_nanos(500));
        let start = 0u64;
        let now = 2_000_000_000u64; // 2s of virtual time elapsed
        let out = ff.metrics_report(StopReason::Horizon, start, now, 4.0, 7);
        // Well-formed, greppable key/value lines the harness keys off.
        assert!(out.contains("schema 1"));
        assert!(out.contains("stop_reason horizon"));
        assert!(out.contains("jumps 2"));
        assert!(out.contains("virtual_secs 2.000"));
        // speedup = virtual/real = 2/4 = 0.5
        assert!(out.contains("speedup 0.500"), "got:\n{out}");
        // jump_real = 2*500ns = 1us => executing_fraction ~= 1.0
        assert!(out.contains("jump_real_secs 0.000001"), "got:\n{out}");
        // histogram row present with all nine buckets.
        assert!(out.contains("hist_counts "));
        assert_eq!(out.matches(',').count() >= 8, true);
    }

    #[test]
    fn duration_parses_units_and_rejects_junk() {
        assert_eq!(parse_duration_secs("30"), Some(30.0)); // bare = seconds
        assert_eq!(parse_duration_secs("30s"), Some(30.0));
        assert_eq!(parse_duration_secs("500ms"), Some(0.5));
        assert_eq!(parse_duration_secs("5m"), Some(300.0));
        assert_eq!(parse_duration_secs("2h"), Some(7200.0));
        assert_eq!(parse_duration_secs("1.5s"), Some(1.5));
        assert_eq!(parse_duration_secs("  10s "), Some(10.0));
        // Rejections: non-positive, non-finite, unparseable.
        assert_eq!(parse_duration_secs("0"), None);
        assert_eq!(parse_duration_secs("-5s"), None);
        assert_eq!(parse_duration_secs("abc"), None);
        assert_eq!(parse_duration_secs(""), None);
    }

    #[test]
    fn histogram_buckets_by_delta_magnitude() {
        // At 1 GHz, 1 cycle == 1 ns, so delta_cycles == delta_ns for bucketing.
        let mut ff = FfState::new(1_000_000_000, 300.0);
        let z = std::time::Duration::ZERO;
        ff.record_hop(500, z); // 500 ns  -> <1us   (bucket 0)
        ff.record_hop(5_000, z); // 5 us   -> <10us  (bucket 1)
        ff.record_hop(500_000_000, z); // 0.5 s -> <1s (bucket 6)
        ff.record_hop(20_000_000_000, z); // 20 s -> >=10s (bucket 8)
        assert_eq!(ff.hist.buckets[0], 1);
        assert_eq!(ff.hist.buckets[1], 1);
        assert_eq!(ff.hist.buckets[6], 1);
        assert_eq!(ff.hist.buckets[8], 1);
        assert_eq!(ff.hist.buckets.iter().sum::<u64>(), 4);
        // The summary lists every labeled bucket.
        assert!(ff.hist.summary().contains("<1us:1"));
        assert!(ff.hist.summary().contains(">=10s:1"));
    }

    #[test]
    fn histogram_boundaries_land_in_upper_bucket() {
        // A delta exactly at an edge belongs to the *next* bucket (>= edge).
        let mut ff = FfState::new(1_000_000_000, 300.0);
        ff.record_hop(1_000, std::time::Duration::ZERO); // exactly 1us -> <10us
        assert_eq!(ff.hist.buckets[0], 0);
        assert_eq!(ff.hist.buckets[1], 1);
    }

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

    #[test]
    fn ff_mode_statement_reports_state_and_source() {
        // The default (no --ff) is ON, chosen by the binary default.
        assert_eq!(ff_mode_statement(true, false), "fast-forward: ON (default)");
        // Explicit flags report how it was chosen (run.sh passes `--ff off`).
        assert_eq!(ff_mode_statement(false, true), "fast-forward: OFF (--ff off)");
        assert_eq!(ff_mode_statement(true, true), "fast-forward: ON (--ff on)");
        // A default-off would still read "default" (documents the mechanism even
        // though the binary default is on).
        assert_eq!(ff_mode_statement(false, false), "fast-forward: OFF (default)");
    }

    #[test]
    fn warn_fires_once_when_high_rate_is_sustained_and_is_rate_limited() {
        // Drive the rate tracker with SYNTHETIC instants (no real sleeping) to
        // prove: (1) a sustained >10k/s rate warns after the 5s sustain window,
        // (2) it warns only ONCE until the cooldown, (3) it NEVER stops the run
        // (this returns a message; the caller only logs it).
        let t0 = std::time::Instant::now();
        let mut ff = FfState::new(1_000_000_000, 300.0);
        ff.rate_win_start = t0;
        ff.rate_win_jumps = 0;

        // Step 1s at a time at 20k jumps/s (> the 10k threshold).
        let mut warns = 0;
        let mut first_warn_at = None;
        for sec in 1..=7 {
            ff.jumps += 20_000; // 20k jumps this second
            let now = t0 + std::time::Duration::from_secs(sec);
            if ff.jump_rate_warn_at(now).is_some() {
                warns += 1;
                first_warn_at.get_or_insert(sec);
            }
        }
        // First WARN lands once the high rate has been sustained >= 5s (measured
        // from the start of the window in which the high rate was first seen), and
        // only ONE fires within the 30s cooldown.
        assert_eq!(warns, 1, "exactly one WARN within the cooldown");
        assert_eq!(first_warn_at, Some(5), "WARN after ~5s sustained high rate");

        // A quiet second (rate below threshold) resets the sustain clock: no WARN.
        ff.jumps += 10; // ~10 jumps over the next second => well under threshold
        let quiet = t0 + std::time::Duration::from_secs(8);
        assert!(ff.jump_rate_warn_at(quiet).is_none());
        assert!(ff.high_since.is_none(), "low rate must reset the sustain clock");
    }

    #[test]
    fn service_timers_fires_horizon_as_a_queue_event() {
        // The horizon is enforced purely as a (vtsc, StopRun) queue entry: before
        // the horizon vtsc, service_timers reports no stop; at/after it, it does.
        let clock = VirtualClock::new(0, vtsc::TscFrequency::from_hz(1_000_000_000));
        let mut lapic = Lapic::new(clock, 160, 2);
        let mut events: events::EventQueue<TimerKind> = events::EventQueue::new();
        let horizon = Some(10_000u64);
        assert!(!service_timers(&mut lapic, &mut events, horizon, None, 9_999).horizon);
        assert!(service_timers(&mut lapic, &mut events, horizon, None, 10_000).horizon); // == fires
        assert!(service_timers(&mut lapic, &mut events, horizon, None, 10_001).horizon); // past
        // No horizon set -> never a horizon stop.
        assert!(!service_timers(&mut lapic, &mut events, None, None, u64::MAX).horizon);
    }

    #[test]
    fn service_timers_fires_scenario_step_as_a_queue_event() {
        // TEST-1a: a scenario deadline fires through the SAME queue as the horizon.
        let clock = VirtualClock::new(0, vtsc::TscFrequency::from_hz(1_000_000_000));
        let mut lapic = Lapic::new(clock, 160, 2);
        let mut events: events::EventQueue<TimerKind> = events::EventQueue::new();
        let scn = Some(5_000u64);
        assert!(!service_timers(&mut lapic, &mut events, None, scn, 4_999).scenario);
        let f = service_timers(&mut lapic, &mut events, None, scn, 5_000);
        assert!(f.scenario && !f.horizon);
    }
}
