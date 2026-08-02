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
mod boot;
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
mod serial;
mod vtsc;

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
}

impl StopReason {
    fn exit_code(self) -> i32 {
        match self {
            StopReason::GuestShutdown
            | StopReason::GuestSystemEvent
            | StopReason::GuestHalt => EXIT_GUEST_STOP,
            StopReason::Horizon => EXIT_HORIZON,
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
        }
    }
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

struct Config {
    kernel: Option<String>,
    initrd: Option<String>,
    mem_mib: u64,
    cmdline: String,
    /// Fast-forward on idle (Step 4). Default ON; `--ff off` restores the 3b
    /// real-wait park (A/B for timing bugs; the right mode for an interactive
    /// console).
    fast_forward: bool,
    /// Whether `--ff`/`--fast-forward` was explicitly passed (vs. the binary
    /// default). Only drives the startup mode statement's "how it was chosen"
    /// text — never the FF decision itself.
    ff_explicit: bool,
    /// Single-jump sanity bound in seconds (gate 3); abort if a jump exceeds it.
    max_jump_secs: f64,
    /// Virtual-time horizon in seconds: if set, the run terminates (distinct exit
    /// status + diagnostic dump) when vtsc reaches `vtsc_start + this`. Enforced
    /// as a `(vtsc, StopRun)` queue event, so it is deterministic and replayable.
    /// A wedged guest hits any sane horizon in seconds of real time; a legitimate
    /// long idle also hits it (correct — a run has a bounded virtual duration).
    max_virtual_time_secs: Option<f64>,
    /// If set, print the effective guest CPUID profile and exit (no VM run).
    dump_cpuid: bool,
    /// If set (FF on), write the machine-parseable per-run fast-forward metrics
    /// block to this path at stop (consumed by the comparison harness). No effect
    /// when FF is off (no jumps to account for).
    metrics_out: Option<String>,
}

fn usage() -> ! {
    dlog!(
        "usage: dvmm --kernel <vmlinux> --initrd <initramfs> [--mem <MiB>] \
         [--cmdline <str>] [--ff on|off] [--max-jump-secs <n>] \
         [--max-virtual-time <dur>] [--metrics-out <path>] [--dump-cpuid]\n\
         \n\
         <dur> is a duration: a bare number is seconds, or use a suffix\n\
         (ms, s, m, h), e.g. 500ms, 30s, 5m, 2h."
    );
    std::process::exit(2);
}

/// Parse a duration to seconds (f64). A bare number is seconds; suffixes `ms`,
/// `s`, `m`, `h` are honored. Returns `None` on anything unparseable or <= 0.
fn parse_duration_secs(s: &str) -> Option<f64> {
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

fn parse_args() -> Config {
    let mut kernel = None;
    let mut initrd = None;
    let mut mem_mib = DEFAULT_MEM_MIB;
    let mut cmdline = DEFAULT_CMDLINE.to_string();
    let mut fast_forward = true;
    let mut ff_explicit = false;
    let mut max_jump_secs = DEFAULT_MAX_JUMP_SECS;
    let mut max_virtual_time_secs = None;
    let mut dump_cpuid = false;
    let mut metrics_out = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--kernel" => kernel = args.next(),
            "--initrd" => initrd = args.next(),
            "--mem" => {
                mem_mib = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage());
            }
            "--cmdline" => cmdline = args.next().unwrap_or_else(|| usage()),
            "--ff" | "--fast-forward" => {
                fast_forward = match args.next().as_deref() {
                    Some("on") | Some("1") | Some("true") => true,
                    Some("off") | Some("0") | Some("false") => false,
                    _ => usage(),
                };
                ff_explicit = true;
            }
            "--max-jump-secs" => {
                max_jump_secs = args
                    .next()
                    .and_then(|v| v.parse::<f64>().ok())
                    .filter(|&n| n.is_finite() && n > 0.0)
                    .unwrap_or_else(|| usage());
            }
            "--max-virtual-time" => {
                max_virtual_time_secs = Some(
                    args.next()
                        .as_deref()
                        .and_then(parse_duration_secs)
                        .unwrap_or_else(|| usage()),
                );
            }
            "--dump-cpuid" => dump_cpuid = true,
            "--metrics-out" => metrics_out = Some(args.next().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            other => {
                dlog!("unknown argument: {other}");
                usage();
            }
        }
    }

    Config {
        kernel,
        initrd,
        mem_mib,
        cmdline,
        fast_forward,
        ff_explicit,
        max_jump_secs,
        max_virtual_time_secs,
        dump_cpuid,
        metrics_out,
    }
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
    match run() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            dlog!("dvmm: fatal: {err}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32, Box<dyn std::error::Error>> {
    let cfg = parse_args();
    let mem_size = cfg.mem_mib * 1024 * 1024;

    // --- KVM ---
    let kvm = Kvm::new()?;

    // `--dump-cpuid`: emit the effective guest CPUID profile (the manifest
    // artifact) and exit — no VM, no kernel/initrd needed.
    if cfg.dump_cpuid {
        dump_cpuid(&kvm)?;
        return Ok(0);
    }

    // Startup mode statement (item 1) + interactive banner (item 4). The mode
    // statement is ALWAYS printed. When stdin is a tty we additionally append a
    // quit hint (one banner line) and, if FF is on, emit an advisory warning.
    // isatty gates ONLY the banner/advisory wording, never the FF decision.
    {
        let tty = serial::stdin_is_tty();
        let mode = ff_mode_statement(cfg.fast_forward, cfg.ff_explicit);
        if tty {
            dlog!(
                "[dvmm] {mode} — quit the guest with `poweroff` or `reboot` \
                 (`exit` now gives a fresh shell)"
            );
        } else {
            dlog!("[dvmm] {mode}");
        }
        // Advisory (telemetry, NOT behavior): fast-forward at an interactive
        // console races the guest clock and pins a host core.
        if cfg.fast_forward && tty {
            dlog!(
                "[dvmm][WARN] fast-forward is ON at an interactive console — it \
                 races the guest clock and pins a host core; pass `--ff off` for \
                 real-time."
            );
        }
    }

    let kernel_path = cfg.kernel.clone().unwrap_or_else(|| usage());
    let initrd_path = cfg.initrd.clone().unwrap_or_else(|| usage());
    let mut kernel_file = std::fs::File::open(&kernel_path)
        .map_err(|e| format!("opening kernel {kernel_path}: {e}"))?;
    let mut initrd_file = std::fs::File::open(&initrd_path)
        .map_err(|e| format!("opening initrd {initrd_path}: {e}"))?;

    dlog!(
        "[dvmm] kernel={} initrd={} mem={} MiB ff={} cmdline={:?}",
        kernel_path,
        initrd_path,
        cfg.mem_mib,
        if cfg.fast_forward { "on" } else { "off" },
        cfg.cmdline
    );

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

    // --- Load kernel + initrd ---
    let entry = boot::load_kernel(&guest_mem, &mut kernel_file)?;
    let initrd_cfg = boot::load_initrd(&guest_mem, &mut initrd_file, mem_size)?;
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
    boot::configure_system(&guest_mem, &cfg.cmdline, Some(initrd_cfg), mem_size, 1)?;

    // --- Serial console ---
    let (serial, serial_drain) = serial::new_serial()?;
    let raw_term = serial::RawTerminal::enable(0);
    // From here our own log lines must snap to column 0 with CRLF (raw mode turns
    // off the tty's ONLCR). Flip the flag only if raw mode actually took effect
    // (a tty); the cooked-mode boot lines above already printed with a plain "\n".
    if raw_term.is_raw() {
        RAW_TTY.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let stop = run_user_backend(
        vcpu,
        serial,
        serial_drain,
        clock,
        cfg.fast_forward,
        cfg.max_jump_secs,
        cfg.max_virtual_time_secs,
        cfg.metrics_out.clone(),
    )?;
    Ok(stop.exit_code())
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
) -> Result<StopReason, Box<dyn std::error::Error>> {
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

    loop {
        // (1) Fire any due guest timer, then reconcile the queue to the LAPIC's
        //     current armed deadline (there is at most one). Also carries the
        //     virtual-time horizon; a fired StopRun (guest busy-looping past the
        //     horizon without HLTing) stops the run here.
        if service_timers(&mut lapic, &mut events, horizon_vtsc, clock.vtsc_now()) {
            stop_reason = StopReason::Horizon;
            report_horizon(ff_state.as_ref(), start);
            break;
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
                )?;
                if let ParkOutcome::Horizon = outcome {
                    stop_reason = StopReason::Horizon;
                    report_horizon(ff_state.as_ref(), start);
                    break;
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
    Ok(stop_reason)
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
/// Returns `true` iff the horizon's `StopRun` event fired (vtsc reached the
/// budget), i.e. the run should stop.
fn service_timers(
    lapic: &mut Lapic,
    events: &mut events::EventQueue<TimerKind>,
    horizon: Option<u64>,
    now: u64,
) -> bool {
    events.clear();
    if let Some(dl) = lapic.timer_deadline() {
        events.push(dl, TimerKind::LapicDeadline);
    }
    if let Some(h) = horizon {
        events.push(h, TimerKind::StopRun);
    }
    let mut horizon_reached = false;
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
                horizon_reached = true;
            }
        }
    }
    horizon_reached
}

/// How a park returned: either an interrupt became deliverable (wake the guest)
/// or the `--max-virtual-time` horizon fired inside the park (stop the run).
enum ParkOutcome {
    Deliverable,
    Horizon,
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
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    match ff {
        Some(ff) => fast_forward_until_deliverable(
            lapic, ioapic, events, serial, serial_drain, parker, clock, vcpu, horizon, ff,
        ),
        None => real_wait_until_deliverable(
            lapic, ioapic, events, serial, serial_drain, parker, clock, horizon,
        ),
    }
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
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    loop {
        let now = clock.vtsc_now();
        if service_timers(lapic, events, horizon, now) {
            return Ok(ParkOutcome::Horizon);
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
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    loop {
        // stdin precedence: service any pending console input up-front, without
        // blocking, so an idle console can never stall a jump.
        if parker.stdin_open() && parker.stdin_ready()? {
            service_console_input(parker, serial, serial_drain, lapic, ioapic);
        }

        // Fire any due timers + the horizon, reconcile the queue to the LAPIC's
        // armed deadline (and the horizon StopRun entry).
        let now = clock.vtsc_now();
        if service_timers(lapic, events, horizon, now) {
            return Ok(ParkOutcome::Horizon);
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
/// entries deliver nothing). Runs on the vCPU thread, at a loop boundary.
fn raise_irq(lapic: &mut Lapic, ioapic: &Ioapic, irq: u32) {
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
        assert!(!service_timers(&mut lapic, &mut events, horizon, 9_999));
        assert!(service_timers(&mut lapic, &mut events, horizon, 10_000)); // == fires
        assert!(service_timers(&mut lapic, &mut events, horizon, 10_001)); // past fires
        // No horizon set -> never a horizon stop.
        assert!(!service_timers(&mut lapic, &mut events, None, u64::MAX));
    }
}
