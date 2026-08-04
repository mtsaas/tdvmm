//! Wedge observability (opt-in via `TDVMM_WEDGE_SECS=<n>`).
//!
//! A guest can hard-wedge with the vCPU making no forward progress: no console
//! output, no HLT (so fast-forward can't collapse it), just a spinning core. This
//! module answers the only question that matters then — *what is the vCPU doing?*
//!
//! Two failure shapes, distinguished by whether KVM keeps trapping:
//!   - **vmexit livelock**: `KVM_RUN` keeps returning the same exit (some MMIO/PIO
//!     the VMM no-ops) and the guest retries forever. The exit histogram + the
//!     last-exits ring name the offending address.
//!   - **in-guest spin**: the guest spins on a memory location (a spinlock, a poll
//!     loop) so `KVM_RUN` never returns — zero exits during the stall. A signal
//!     kick breaks it out so we can read the guest RIP where it is stuck.
//!
//! A watchdog thread notices "no console output and no HLT for N seconds", logs
//! the histogram + ring (cross-thread, from atomics), then kicks the vCPU thread
//! (SIGUSR1 → `KVM_RUN` returns `EINTR`) so the guest RIP + interrupt state are
//! read ON the vCPU thread — the single-writer invariant is preserved (no KVM
//! ioctl is ever issued off-thread).
//!
//! Entirely off unless `TDVMM_WEDGE_SECS` is set: `record`/`note_*` early-return,
//! no watchdog thread, no signal handler installed.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// The exit buckets for the histogram + ring. `Eintr` is recorded by
/// [`Diag::note_eintr`] on a signal kick, not by [`classify`]; `Other` catches
/// every exit kind we don't name. The `name()` match is exhaustive, so adding a
/// variant is a single edit the compiler enforces.
#[derive(Clone, Copy)]
enum ExitKind {
    IoOut, IoIn, MmioRead, MmioWrite, IrqWindow, Hlt, Shutdown,
    SystemEvent, FailEntry, Internal, Eintr, Other,
}

impl ExitKind {
    /// All variants in discriminant order, so `ALL[k as usize] == k`.
    const ALL: [ExitKind; 12] = [
        ExitKind::IoOut, ExitKind::IoIn, ExitKind::MmioRead, ExitKind::MmioWrite,
        ExitKind::IrqWindow, ExitKind::Hlt, ExitKind::Shutdown, ExitKind::SystemEvent,
        ExitKind::FailEntry, ExitKind::Internal, ExitKind::Eintr, ExitKind::Other,
    ];

    fn name(self) -> &'static str {
        match self {
            ExitKind::IoOut => "IoOut",
            ExitKind::IoIn => "IoIn",
            ExitKind::MmioRead => "MmioRead",
            ExitKind::MmioWrite => "MmioWrite",
            ExitKind::IrqWindow => "IrqWindow",
            ExitKind::Hlt => "Hlt",
            ExitKind::Shutdown => "Shutdown",
            ExitKind::SystemEvent => "SystemEvent",
            ExitKind::FailEntry => "FailEntry",
            ExitKind::Internal => "Internal",
            ExitKind::Eintr => "Eintr",
            ExitKind::Other => "Other",
        }
    }
}

const N_KINDS: usize = ExitKind::ALL.len();

/// Ring of recent exits (packed `kind<<56 | addr`). 56 bits holds any MMIO addr.
const RING: usize = 64;
const ADDR_MASK: u64 = (1u64 << 56) - 1;

pub struct Diag {
    enabled: bool,
    /// Seconds of no console/HLT progress before the watchdog dumps.
    watchdog_secs: u64,
    /// The vCPU thread's tid, for the SIGUSR1 kick (`tgkill`).
    vcpu_tid: AtomicI32,
    /// Guest console (COM1) output bytes — "the guest is still talking".
    console_out: AtomicU64,
    /// Per-kind exit counts (index = `ExitKind as usize`). HLT liveness and the
    /// cumulative-exit total are both read straight off this.
    hist: [AtomicU64; N_KINDS],
    /// Recent exits, newest at `ring_head-1` (mod RING).
    ring: [AtomicU64; RING],
    ring_head: AtomicUsize,
    /// Set by the watchdog, serviced+cleared by the vCPU thread on the next EINTR.
    dump_req: AtomicBool,
    /// Set once the vCPU loop ends, so the watchdog goes quiet during post-run
    /// capture/finalize (which is idle by design, not a wedge).
    stopped: AtomicBool,
}

impl Diag {
    /// Build from `TDVMM_WEDGE_SECS`. When set (and > 0) the watchdog is armed and
    /// the SIGUSR1 handler installed; otherwise every method is a no-op.
    pub fn from_env() -> Arc<Diag> {
        let secs = std::env::var("TDVMM_WEDGE_SECS")
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|&n| n > 0);
        let enabled = secs.is_some();
        let d = Arc::new(Diag {
            enabled,
            watchdog_secs: secs.unwrap_or(0),
            vcpu_tid: AtomicI32::new(0),
            console_out: AtomicU64::new(0),
            hist: std::array::from_fn(|_| AtomicU64::new(0)),
            ring: std::array::from_fn(|_| AtomicU64::new(0)),
            ring_head: AtomicUsize::new(0),
            dump_req: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
        });
        if enabled {
            crate::util::install_signal_handler(libc::SIGUSR1, on_sigusr1);
            crate::log_line(format_args!(
                "[tdvmm][diag] wedge watchdog ARMED: will dump vCPU state after {}s \
                 with no console/HLT progress (TDVMM_WEDGE_SECS)",
                d.watchdog_secs
            ));
        }
        d
    }

    /// Record the calling (vCPU) thread's tid so the watchdog can kick it.
    pub fn note_tid(&self) {
        if !self.enabled {
            return;
        }
        self.vcpu_tid.store(crate::util::gettid(), Ordering::Relaxed);
    }

    /// Bucket one handled KVM exit. Classifying is deferred past the `enabled`
    /// check, so a diagnostics-off build does no work per exit.
    pub fn record(&self, exit: &kvm_ioctls::VcpuExit) {
        if !self.enabled {
            return;
        }
        let (kind, addr) = classify(exit);
        self.hist[kind as usize].fetch_add(1, Ordering::Relaxed);
        let h = self.ring_head.fetch_add(1, Ordering::Relaxed);
        self.ring[h % RING]
            .store(((kind as u64) << 56) | (addr & ADDR_MASK), Ordering::Relaxed);
    }

    /// Note `n` bytes of guest console output (a liveness signal).
    pub fn note_console_out(&self, n: u64) {
        if self.enabled {
            self.console_out.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Count an EINTR (a signal broke `KVM_RUN`). Cheap and always called; the
    /// state dump is a separate step so the (rare) dump builds its strings only
    /// when one was actually requested.
    pub fn note_eintr(&self) {
        if self.enabled {
            self.hist[ExitKind::Eintr as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Cumulative guest-driven exits: every bucket except the signal-kick `Eintr`
    /// (our own doing — it must not read as "the guest is still trapping").
    fn exits_total(&self) -> u64 {
        let mut sum = 0;
        for k in ExitKind::ALL {
            if !matches!(k, ExitKind::Eintr) {
                sum += self.hist[k as usize].load(Ordering::Relaxed);
            }
        }
        sum
    }

    /// True at most once per watchdog request: whether a state dump was asked for
    /// (clears the request). False when disabled.
    pub fn take_dump_request(&self) -> bool {
        self.enabled && self.dump_req.swap(false, Ordering::AcqRel)
    }

    /// Read + log the guest RIP/regs, virtual clock vs. next armed deadline, and
    /// the caller's interrupt-controller snapshots. Called ON the vCPU thread
    /// after [`take_dump_request`], so the KVM ioctls honor the single-writer
    /// invariant. An OVERDUE `next_deadline` with the guest still running is the
    /// LAPIC-timer-starvation signature.
    pub fn dump_guest(
        &self,
        vcpu: &mut kvm_ioctls::VcpuFd,
        now_vtsc: u64,
        next_deadline: Option<u64>,
        lapic_diag: &str,
        ioapic_diag: &str,
    ) {
        let (if_flag, ready, cr8) = {
            let run = vcpu.get_kvm_run();
            (run.if_flag, run.ready_for_interrupt_injection, run.cr8)
        };
        let (rip, rsp, rflags) = match vcpu.get_regs() {
            Ok(r) => (r.rip, r.rsp, r.rflags),
            Err(e) => {
                crate::log_line(format_args!("[tdvmm][diag] get_regs failed: {e}"));
                (0, 0, 0)
            }
        };
        let (cr2, cr3) = match vcpu.get_sregs() {
            Ok(s) => (s.cr2, s.cr3),
            Err(_) => (0, 0),
        };
        crate::log_line(format_args!(
            "[tdvmm][diag] guest @wedge: RIP={rip:#018x} RSP={rsp:#018x} RFLAGS={rflags:#x} \
             CR2={cr2:#x} CR3={cr3:#x} | if_flag={if_flag} ready_inj={ready} cr8={cr8}"
        ));
        let when = match next_deadline {
            Some(d) if d < now_vtsc => {
                format!(" (OVERDUE by {} cyc — a tick that was never serviced)", now_vtsc - d)
            }
            Some(d) => format!(" (in {} cyc)", d - now_vtsc),
            None => " (none armed)".into(),
        };
        crate::log_line(format_args!(
            "[tdvmm][diag]   vtsc_now={now_vtsc} next_deadline={next_deadline:?}{when}"
        ));
        crate::log_line(format_args!("[tdvmm][diag]   {lapic_diag}"));
        crate::log_line(format_args!("[tdvmm][diag]   {ioapic_diag}"));
    }

    /// Tell the watchdog the vCPU loop has ended: post-run capture/finalize is
    /// idle by design and must not be reported as a wedge. Idempotent.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Spawn the watchdog thread (no-op when disabled). Consumes an `Arc` clone.
    pub fn spawn_watchdog(self: Arc<Self>) {
        if !self.enabled {
            return;
        }
        std::thread::spawn(move || {
            const MAX_DUMPS: u32 = 6;
            let w = self.watchdog_secs.max(1); // dump after w stalled seconds, then every w
            let mut last_console = 0u64;
            let mut last_hlts = 0u64;
            let mut stalled: u64 = 0;
            let mut exits_at_stall: u64 = 0;
            let mut next_dump_at: u64 = w;
            let mut dumps: u32 = 0;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if self.stopped.load(Ordering::Relaxed) {
                    return;
                }
                let co = self.console_out.load(Ordering::Relaxed);
                let hl = self.hist[ExitKind::Hlt as usize].load(Ordering::Relaxed);
                let ex = self.exits_total();
                // "Progress" is deliberately console-output OR HLT — NOT raw exits,
                // which race during a vmexit livelock and would mask the wedge.
                if co != last_console || hl != last_hlts {
                    last_console = co;
                    last_hlts = hl;
                    stalled = 0;
                    next_dump_at = w;
                    dumps = 0;
                    continue;
                }
                stalled += 1;
                if stalled == 1 {
                    exits_at_stall = ex;
                }
                if stalled < next_dump_at || dumps >= MAX_DUMPS {
                    continue;
                }
                next_dump_at += w;
                dumps += 1;
                let delta = ex.wrapping_sub(exits_at_stall);
                let shape = if delta > 0 {
                    "vmexit LIVELOCK (KVM keeps trapping — see the histogram/ring below)"
                } else {
                    "in-guest SPIN (blocked inside KVM_RUN — 0 exits during the stall)"
                };
                crate::log_line(format_args!(
                    "[tdvmm][diag] WEDGE #{dumps}: {stalled}s with no console/HLT progress — \
                     {shape}; exits advanced {delta} during the stall (~{}/s)",
                    delta / stalled.max(1)
                ));
                self.log_histogram();
                self.log_ring();
                // Kick the vCPU thread to add RIP/regs on-thread.
                self.dump_req.store(true, Ordering::Release);
                let tid = self.vcpu_tid.load(Ordering::Relaxed);
                if tid != 0 {
                    // SAFETY: tgkill to our own process' vCPU thread; SIGUSR1 has a
                    // no-op handler, so the only effect is KVM_RUN returning EINTR.
                    unsafe {
                        libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGUSR1);
                    }
                }
            }
        });
    }

    fn log_histogram(&self) {
        use std::fmt::Write;
        let mut s = String::new();
        for k in ExitKind::ALL {
            let c = self.hist[k as usize].load(Ordering::Relaxed);
            if c > 0 {
                let _ = write!(s, "{}={} ", k.name(), c);
            }
        }
        crate::log_line(format_args!(
            "[tdvmm][diag]   exit histogram (cumulative): {}",
            s.trim_end()
        ));
    }

    fn log_ring(&self) {
        use std::fmt::Write;
        let head = self.ring_head.load(Ordering::Relaxed);
        let n = RING.min(head);
        let start = head - n;
        let mut s = String::new();
        for i in start..head {
            let v = self.ring[i % RING].load(Ordering::Relaxed);
            // `kind` is always an in-range index — every writer stores `k as usize`.
            let kind = ExitKind::ALL[(v >> 56) as usize];
            let _ = write!(s, "{}@{:#x} ", kind.name(), v & ADDR_MASK);
        }
        crate::log_line(format_args!(
            "[tdvmm][diag]   last {n} exits (oldest->newest): {}",
            s.trim_end()
        ));
    }
}

/// Classify a KVM exit into a bucket + a representative address (the IO port or
/// MMIO address; 0 where not applicable).
fn classify(exit: &kvm_ioctls::VcpuExit) -> (ExitKind, u64) {
    use kvm_ioctls::VcpuExit::*;
    match exit {
        IoOut(port, _) => (ExitKind::IoOut, *port as u64),
        IoIn(port, _) => (ExitKind::IoIn, *port as u64),
        MmioRead(addr, _) => (ExitKind::MmioRead, *addr),
        MmioWrite(addr, _) => (ExitKind::MmioWrite, *addr),
        IrqWindowOpen => (ExitKind::IrqWindow, 0),
        Hlt => (ExitKind::Hlt, 0),
        Shutdown => (ExitKind::Shutdown, 0),
        SystemEvent(t, _) => (ExitKind::SystemEvent, *t as u64),
        FailEntry(r, _) => (ExitKind::FailEntry, *r),
        InternalError => (ExitKind::Internal, 0),
        _ => (ExitKind::Other, 0),
    }
}

/// A no-op SIGUSR1 handler: the kick's only job is to make an in-flight
/// `KVM_RUN` return `EINTR` (installed without `SA_RESTART`); the default
/// SIGUSR1 disposition would terminate the process.
extern "C" fn on_sigusr1(_sig: libc::c_int) {}
