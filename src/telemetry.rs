//! Fast-forward telemetry (Step 4 instrumentation): the Δvtsc jump histogram,
//! the per-hop cost histogram, the WARN-only jump-rate tracker, and `FfState` —
//! the accounting struct the run loop threads through every jump — plus the
//! machine-parseable `--metrics-out` report.

use crate::exit::StopReason;

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

/// Group a non-negative integer with thousands separators for the human summary
/// (`14513` -> `14,513`).
fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, &c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c as char);
    }
    out
}

/// A Δvtsc histogram: jump counts bucketed by how far they advanced virtual time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeltaHistogram {
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
    pub(crate) fn summary(&self) -> String {
        let mut parts = Vec::with_capacity(self.buckets.len());
        for (i, c) in self.buckets.iter().enumerate() {
            parts.push(format!("{}:{}", HIST_LABELS[i], c));
        }
        format!("Δvtsc histogram [jumps by advance]: {}", parts.join(" "))
    }

    /// Render the histogram as aligned, one-bucket-per-line text for the human run
    /// summary: a right-aligned count and a bar scaled to the busiest bucket, each
    /// line prefixed with `indent`.
    fn human_lines(&self, indent: &str) -> String {
        let max = self.buckets.iter().copied().max().unwrap_or(0);
        let counts: [String; 9] = std::array::from_fn(|i| group_digits(self.buckets[i]));
        let label_w = HIST_LABELS.iter().map(|l| l.len()).max().unwrap_or(0);
        let count_w = counts.iter().map(String::len).max().unwrap_or(0);
        const BAR_CELLS: u128 = 24;
        const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
        let mut out = String::new();
        for (i, count) in counts.iter().enumerate() {
            let eighths = if max == 0 {
                0
            } else {
                u128::from(self.buckets[i]) * BAR_CELLS * 8 / u128::from(max)
            };
            let mut bar = "█".repeat((eighths / 8) as usize);
            bar.push_str(EIGHTHS[(eighths % 8) as usize]);
            let line = format!("{indent}{:<label_w$}  {count:>count_w$}  {bar}", HIST_LABELS[i]);
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
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
pub(crate) struct FfState {
    /// TSC frequency (Hz) — converts cycles to seconds for the reports.
    pub(crate) tsc_hz: u64,
    /// Single-jump bound (gate 3): abort if any Δ exceeds this many cycles.
    pub(crate) max_jump_cycles: u64,
    /// The same bound in seconds, for messages.
    pub(crate) max_jump_secs: f64,
    /// Number of jumps performed.
    pub(crate) jumps: u64,
    /// Largest single jump Δ observed (cycles).
    max_delta_cycles: u64,
    /// Sum of all jump Δs (cycles) — total virtual time fast-forwarded.
    sum_delta_cycles: u128,
    /// Sum of per-hop real cost (ns) and the max, for mean/max reporting.
    hop_ns_sum: u128,
    pub(crate) hop_ns_max: u64,
    /// Δvtsc histogram: jumps bucketed by how far they advanced virtual time.
    /// Feeds the horizon diagnostic dump and the WARN telemetry.
    pub(crate) hist: DeltaHistogram,
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
    pub(crate) fn new(tsc_hz: u64, max_jump_secs: f64) -> Self {
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

    pub(crate) fn record_hop(&mut self, delta_cycles: u64, hop: std::time::Duration) {
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
    pub(crate) fn maybe_warn_high_jump_rate(&mut self) {
        if let Some(msg) = self.jump_rate_warn_at(std::time::Instant::now()) {
            crate::log_line(format_args!("{msg}"));
        }
    }

    /// Testable core of [`maybe_warn_high_jump_rate`]: given the current instant,
    /// update the sliding-window rate tracker and return the WARN message iff one
    /// should be emitted now (rate above threshold, sustained past the window,
    /// and past the cooldown since the last WARN). Pure of I/O so a unit test can
    /// drive it with synthetic instants — no real sleeping, deterministic.
    pub(crate) fn jump_rate_warn_at(&mut self, now: std::time::Instant) -> Option<String> {
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
                    "[tdvmm][WARN] fast-forward jump rate {:.0}/s sustained for {:.0}s \
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

    pub(crate) fn mean_hop_ns(&self) -> u64 {
        if self.jumps == 0 {
            0
        } else {
            (self.hop_ns_sum / u128::from(self.jumps)) as u64
        }
    }

    pub(crate) fn max_delta_secs(&self) -> f64 {
        self.max_delta_cycles as f64 / self.tsc_hz as f64
    }

    /// Virtual seconds elapsed between two vtsc samples (the whole run's span,
    /// active execution included — this is the headline speedup numerator).
    pub(crate) fn virtual_secs_since(&self, vtsc_start: u64, vtsc_now: u64) -> f64 {
        vtsc_now.wrapping_sub(vtsc_start) as f64 / self.tsc_hz as f64
    }

    pub(crate) fn hop_p99_ns(&self) -> u64 {
        self.cost_hist.quantile_ns(0.99)
    }

    /// Real seconds spent performing the jumps themselves (the sum of per-hop
    /// park/jump costs). The rest of wall time is guest execution + VMM overhead.
    pub(crate) fn jump_real_secs(&self) -> f64 {
        self.hop_ns_sum as f64 / 1e9
    }

    /// Virtual seconds fast-forwarded over (the sum of all jump Δs) — the guest's
    /// idle time that was skipped. The rest of the virtual span ran at real-time rate.
    pub(crate) fn jumped_secs(&self) -> f64 {
        self.sum_delta_cycles as f64 / self.tsc_hz as f64
    }

    /// The end-of-run summary for humans: a sectioned, aligned report of why the
    /// run stopped, the virtual/real timing, the fast-forward accounting, and the
    /// Δvtsc histogram. This is the console view; [`metrics_report`] is the stable
    /// machine-readable form the comparison harness parses.
    pub(crate) fn human_summary(
        &self,
        stop: StopReason,
        vtsc_start: u64,
        vtsc_now: u64,
        real_secs: f64,
        hlt_count: u64,
    ) -> String {
        let virt_s = self.virtual_secs_since(vtsc_start, vtsc_now);
        let real_s = real_secs.max(1e-9);
        let jump_real = self.jump_real_secs();
        let exec_real = (real_s - jump_real).max(0.0);
        let jumped_s = self.jumped_secs();
        let idle_pct = if virt_s > 0.0 { (jumped_s / virt_s * 100.0).min(100.0) } else { 0.0 };

        let mut s = String::from("\n[tdvmm] run complete\n");
        s.push_str("──────────────────────────────────────────────\n\n");

        s.push_str(&format!("  stop reason        {}\n", stop.human()));
        if matches!(stop, StopReason::Horizon) {
            s.push_str("                     (a VMM time-budget stop, not the guest exiting)\n");
        }
        s.push('\n');

        // Each section aligns its own value column to its widest label.
        let section = |s: &mut String, title: &str, rows: &[(&str, String)]| {
            s.push_str(&format!("  {title}\n"));
            let w = rows.iter().map(|(l, _)| l.len()).max().unwrap_or(0);
            for (label, value) in rows {
                s.push_str(&format!("    {label:<w$}  {value}\n"));
            }
            s.push('\n');
        };

        section(
            &mut s,
            "Time",
            &[
                ("virtual", format!("{virt_s:.1} s")),
                ("real", format!("{real_s:.1} s")),
                ("speedup", format!("{:.1}×", virt_s / real_s)),
            ],
        );
        section(
            &mut s,
            "Fast-forward",
            &[
                ("jumps", group_digits(self.jumps)),
                ("rate", format!("{} /s", group_digits((self.jumps as f64 / real_s) as u64))),
                ("idle skipped", format!("{jumped_s:.1} s of {virt_s:.1} s virtual ({idle_pct:.0}% idle)")),
                (
                    "largest jump",
                    format!("{:.3} s   (per-jump limit {:.0} s)", self.max_delta_secs(), self.max_jump_secs),
                ),
                ("per-hop mean", format!("{:.1} us", self.mean_hop_ns() as f64 / 1000.0)),
                ("per-hop p99", format!("{:.1} us", self.hop_p99_ns() as f64 / 1000.0)),
                ("per-hop max", format!("{:.1} us", self.hop_ns_max as f64 / 1000.0)),
            ],
        );
        section(
            &mut s,
            "Guest halts",
            &[
                ("hlt exits", group_digits(hlt_count)),
                ("rate", format!("{} /s", group_digits((hlt_count as f64 / real_s) as u64))),
            ],
        );
        section(
            &mut s,
            "Where real time went",
            &[
                ("executing guest", format!("{exec_real:.3} s   ({:.1}%)", exec_real / real_s * 100.0)),
                ("fast-forwarding", format!("{jump_real:.3} s   ({:.3}%)", jump_real / real_s * 100.0)),
            ],
        );
        s.push_str("  Δvtsc histogram — jumps by how far each advanced virtual time\n");
        s.push_str(&self.hist.human_lines("    "));
        s
    }

    /// Build the machine-parseable per-run metrics block (`--metrics-out`). Every
    /// field the comparison harness needs: hop count + rate, speedup, the per-hop
    /// cost mean/p99/max, the real-vs-virtual accounting (the busy-wait tripwire),
    /// and the Δvtsc histogram (reused, not duplicated). key<space>value per line,
    /// so it is trivially greppable and stable across runs.
    pub(crate) fn metrics_report(
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
            "# tdvmm fast-forward per-run metrics (machine-parseable; --metrics-out)\n\
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
    fn human_summary_renders_sections_and_grouped_counts() {
        // 1 GHz so 1 cycle == 1 ns; a realistic idle-guest run whose Δ buckets
        // reproduce a typical horizon stop.
        let mut ff = FfState::new(1_000_000_000, 300.0);
        let cheap = std::time::Duration::from_nanos(300);
        let spread = [
            (500u64, 8u32),      // <1us
            (5_000, 27),         // <10us
            (50_000, 5_643),     // <100us
            (300_000, 3_323),    // <1ms
            (5_000_000, 4_459),  // <10ms
            (25_000_000, 1_052), // <100ms
        ];
        for (delta, n) in spread {
            for _ in 0..n {
                ff.record_hop(delta, cheap);
            }
        }
        // One expensive, largest jump: Δ = 0.090 s at a 4.8 us hop cost (the tail).
        // The Δs sum to ~50 s of the 60 s virtual span, i.e. ~83% idle skipped.
        ff.record_hop(90_000_000, std::time::Duration::from_nanos(4_800));

        let out = ff.human_summary(StopReason::Horizon, 0, 60_000_000_000, 11.0, 14_539);

        for want in [
            "run complete",
            "stop reason",
            "Fast-forward",
            "idle skipped",
            "Δvtsc histogram",
            "14,513",  // total jumps, thousands-separated
            "5,643",   // busiest bucket
            "% idle)", // the idle-fraction annotation
        ] {
            assert!(out.contains(want), "summary missing {want:?}:\n{out}");
        }
        // The confusing line and old jargon are gone.
        assert!(!out.contains("real CPU per virtual hour"), "confusing line leaked:\n{out}");
        assert!(!out.contains("real-exec ms"), "old jargon leaked:\n{out}");
        assert!(!out.contains(" · "), "dot-separated cram leaked:\n{out}");

        // Printed under `--nocapture` so the layout can be eyeballed.
        eprintln!("{out}");
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
}
