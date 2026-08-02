//! Userspace local APIC (xAPIC MMIO model), driven by the virtual clock.
//!
//! This is the interrupt controller the guest actually programs in Step 3b:
//! with no in-kernel irqchip, every access to the LAPIC MMIO window at
//! `0xFEE0_0000` traps to the VMM and lands here, and the LAPIC's **one-shot /
//! periodic** timer (MMIO TMICT/TMCCT/TDCR) is the guest's tick. Everything
//! time-derived is a pure function of [`VirtualClock`] (the guest TSC) so Step
//! 4's fast-forward moves the whole LAPIC with the offset and nothing else.
//!
//! TSC-deadline mode is modeled + tested but NOT used on this host: a KVM WRMSR
//! fastpath no-ops IA32_TSC_DEADLINE (MSR `0x6E0`) before the MSR filter when
//! there is no in-kernel LAPIC, so the userspace backend leaves the deadline
//! CPUID bit unadvertised and the guest falls to the one-shot/periodic timer.
//! The timer counts at the core-crystal frequency the guest reads from CPUID
//! 0x15 (see [`apic_bus_hz_from_cpuid`]), so its ticks land at the right real
//! time without the guest measuring our rate.
//!
//! Scope — **xAPIC MMIO only**, x2APIC is masked in CPUID:
//!   * ID / VERSION(maxlvt) / TPR(+cr8 sync) / APR / PPR / SVR(soft-enable) /
//!     EOI / ESR(read-0, write-clear) / full IRR+ISR with textbook PPR priority.
//!   * LVT Timer one-shot + periodic (+ deadline model), TMICT/TMCCT/TDCR. LVT
//!     LINT0/LINT1/ERROR/thermal/perf = storage.
//!   * ICR fixed-delivery **to self** (Linux self-IPIs irq_work); DFR/LDR stored
//!     (flat logical), delivery is accept-all.
//!
//! Loudly logged on use, never silent (things we deliberately do NOT implement):
//! INIT/SIPI, NMI delivery, broadcast shorthands, remote read (RRD), x2APIC.

use crate::vtsc::VirtualClock;

/// Base of the xAPIC MMIO window (guest physical). x2APIC is masked, so the
/// guest always uses this MMIO surface, never MSR-based APIC registers.
pub const XAPIC_BASE: u64 = 0xfee0_0000;
pub const XAPIC_LEN: u64 = 0x1000;

/// Whether a guest-physical `addr` falls inside the xAPIC MMIO window.
pub(crate) fn in_lapic(addr: u64) -> bool {
    (XAPIC_BASE..XAPIC_BASE + XAPIC_LEN).contains(&addr)
}

/// IA32_TSC_DEADLINE MSR index. Retained for the (tested) deadline-mode model,
/// but NOT wired on this host: a KVM WRMSR fastpath no-ops 0x6E0 before the MSR
/// filter when there is no in-kernel LAPIC, so the userspace backend uses the
/// one-shot/periodic timer instead and never advertises TSC-deadline in CPUID.
#[allow(dead_code)]
pub const MSR_IA32_TSC_DEADLINE: u32 = 0x6e0;

/// Fallback APIC-timer input ("bus") frequency if CPUID gives us nothing to
/// derive it from. See [`apic_bus_hz_from_cpuid`].
pub const DEFAULT_APIC_BUS_HZ: u64 = 25_000_000;

/// The APIC-timer input ("bus") frequency the guest expects, derived exactly the
/// way the guest derives it (`arch/x86/kernel/tsc.c`): the **core-crystal clock**
/// from CPUID leaf `0x15`. We pass `0x15`/`0x16` through from the host (Step 3a,
/// for TSC calibration), so the guest sets `lapic_timer_period = crystal_khz *
/// 1000 / HZ` and programs the LAPIC timer assuming the count decrements at the
/// crystal frequency — it does NOT measure our countdown. Our one-shot/periodic
/// timer therefore counts at *this* frequency (as a pure function of vtsc), so
/// the tick fires at the correct real time and is identical every run. On this
/// host CPUID 0x15 reports a 38.4 MHz crystal.
pub fn apic_bus_hz_from_cpuid() -> u64 {
    // `__cpuid` is a safe intrinsic on the x86_64 baseline (CPUID always exists).
    let l15 = core::arch::x86_64::__cpuid(0x15);
    let denom = u64::from(l15.eax);
    let numer = u64::from(l15.ebx);
    let crystal_hz = u64::from(l15.ecx);
    if crystal_hz != 0 {
        return crystal_hz; // ECX reports the crystal directly (this host: 38.4 MHz)
    }
    // Skylake/Kaby-lake style: crystal not reported, derive from 0x16 base MHz
    // and the 0x15 crystal:TSC ratio — the same fallback the guest uses.
    if denom != 0 && numer != 0 {
        let l16 = core::arch::x86_64::__cpuid(0x16);
        let base_mhz = u64::from(l16.eax);
        if base_mhz != 0 {
            return base_mhz * 1_000_000 * denom / numer;
        }
    }
    DEFAULT_APIC_BUS_HZ
}

/// TSC cycles per one APIC-timer count (at divisor 1), as an EXACT integer ratio
/// `(num, den)` taken straight from CPUID leaf `0x15`: `num = EBX` (numerator of
/// the TSC : core-crystal ratio), `den = EAX` (denominator). One APIC count is
/// one crystal tick, and one crystal tick is `EBX/EAX` TSC cycles, so a count of
/// `c` spans exactly `c * EBX / EAX` TSC cycles.
///
/// This is the SAME ratio the guest uses when it sets `lapic_timer_period` from
/// the crystal (it does not measure our countdown), so programming a count and
/// reading it back is bit-exact — **no floating point**, identical every run,
/// and independent of the kHz-rounded `KVM_GET_TSC_KHZ` value. On this host
/// CPUID 0x15 reports `EAX=2, EBX=160` => 80 TSC cycles/count (38.4 MHz crystal,
/// 3.072 GHz TSC). Falls back to `(tsc_hz, crystal_hz)` only if a host does not
/// enumerate the 0x15 ratio (EBX or EAX zero).
pub fn apic_timer_tsc_ratio(tsc_hz: u64) -> (u64, u64) {
    let l15 = core::arch::x86_64::__cpuid(0x15);
    let den = u64::from(l15.eax); // denominator of the TSC : crystal ratio
    let num = u64::from(l15.ebx); // numerator
    if num != 0 && den != 0 {
        (num, den)
    } else {
        // No enumerated ratio: fall back to the (kHz-granular) frequencies.
        (tsc_hz, apic_bus_hz_from_cpuid())
    }
}

// Register offsets (from the MMIO base).
const REG_ID: u32 = 0x020;
const REG_VERSION: u32 = 0x030;
const REG_TPR: u32 = 0x080;
const REG_APR: u32 = 0x090;
const REG_PPR: u32 = 0x0a0;
const REG_EOI: u32 = 0x0b0;
const REG_RRD: u32 = 0x0c0;
const REG_LDR: u32 = 0x0d0;
const REG_DFR: u32 = 0x0e0;
const REG_SVR: u32 = 0x0f0;
const REG_ISR0: u32 = 0x100; // 0x100..0x170, stride 0x10
const REG_TMR0: u32 = 0x180; // 0x180..0x1f0
const REG_IRR0: u32 = 0x200; // 0x200..0x270
const REG_ESR: u32 = 0x280;
const REG_ICR_LOW: u32 = 0x300;
const REG_ICR_HIGH: u32 = 0x310;
const REG_LVT_TIMER: u32 = 0x320;
const REG_LVT_THERMAL: u32 = 0x330;
const REG_LVT_PERF: u32 = 0x340;
const REG_LVT_LINT0: u32 = 0x350;
const REG_LVT_LINT1: u32 = 0x360;
const REG_LVT_ERROR: u32 = 0x370;
const REG_TIMER_ICT: u32 = 0x380; // initial count
const REG_TIMER_CCT: u32 = 0x390; // current count
const REG_TIMER_DCR: u32 = 0x3e0; // divide config

// SVR bit 8: APIC software enable.
const SVR_ENABLE: u32 = 1 << 8;
// LVT bit 16: mask. LVT timer bits 18:17: mode.
const LVT_MASK: u32 = 1 << 16;
const LVT_TIMER_MODE_SHIFT: u32 = 17;
const LVT_TIMER_MODE_ONESHOT: u32 = 0b00;
const LVT_TIMER_MODE_PERIODIC: u32 = 0b01;
const LVT_TIMER_MODE_TSCDEADLINE: u32 = 0b10;

// Version: xAPIC version 0x14 with "max LVT entry" = 5 (Timer, Thermal, Perf,
// LINT0, LINT1, Error all addressable) — matches what KVM's in-kernel LAPIC
// advertises, so the guest's lapic_get_maxlvt() enables the same set of LVTs.
const LAPIC_VERSION: u32 = 0x14 | (5 << 16);

/// The 256-bit vector bitmaps (IRR/ISR/TMR) as eight 32-bit words, exactly the
/// xAPIC register layout: word `i` is the MMIO register at `base + 0x10*i`.
#[derive(Clone, Copy, Debug, Default)]
struct VecBits {
    words: [u32; 8],
}

impl VecBits {
    fn set(&mut self, vec: u8) {
        self.words[(vec >> 5) as usize] |= 1 << (vec & 31);
    }
    fn clear(&mut self, vec: u8) {
        self.words[(vec >> 5) as usize] &= !(1 << (vec & 31));
    }
    #[cfg(test)]
    fn get(&self, vec: u8) -> bool {
        self.words[(vec >> 5) as usize] & (1 << (vec & 31)) != 0
    }
    /// Highest set vector, or `None` if empty.
    fn highest(&self) -> Option<u8> {
        for i in (0..8).rev() {
            let w = self.words[i];
            if w != 0 {
                let bit = 31 - w.leading_zeros();
                return Some((i as u8) * 32 + bit as u8);
            }
        }
        None
    }
}

/// A once-per-use "loud" log for a deliberately-unimplemented feature. Never
/// silent: the whole point is that if the guest ever exercises one of these,
/// it shows up in the log instead of mystifying us later.
fn log_unsupported(what: &str, detail: std::fmt::Arguments<'_>) {
    crate::log_line(format_args!("[dvmm][lapic] UNSUPPORTED {what}: {detail} (ignored)"));
}

pub struct Lapic {
    clock: VirtualClock,
    /// TSC cycles per APIC-timer count at divisor 1, as an EXACT integer ratio
    /// `num/den` from CPUID 0x15 (EBX/EAX). See [`apic_timer_tsc_ratio`]. The
    /// count decrements at `crystal/divisor`, i.e. one count == `num/den` TSC
    /// cycles at divisor 1. No float anywhere in the timer conversions.
    tsc_per_count_num: u64,
    tsc_per_count_den: u64,

    id: u32,
    tpr: u32,
    svr: u32,
    ldr: u32,
    dfr: u32,
    esr: u32,

    lvt_timer: u32,
    lvt_thermal: u32,
    lvt_perf: u32,
    lvt_lint0: u32,
    lvt_lint1: u32,
    lvt_error: u32,

    irr: VecBits,
    isr: VecBits,

    // Timer state. `deadline` is the vtsc at which the timer next fires; the
    // event queue mirrors it (see main.rs). `period` (vtsc span of one count-
    // down) is non-zero only in periodic mode, where firing re-arms to
    // deadline+period. No timer state lives anywhere else.
    timer_deadline: Option<u64>,
    timer_period: u64,
    timer_dcr: u32,
    timer_ict: u32,
}

impl Lapic {
    pub fn new(clock: VirtualClock, tsc_per_count_num: u64, tsc_per_count_den: u64) -> Self {
        assert!(
            tsc_per_count_num > 0 && tsc_per_count_den > 0,
            "APIC-timer TSC ratio must be non-zero (num={tsc_per_count_num} den={tsc_per_count_den})"
        );
        Self {
            clock,
            tsc_per_count_num,
            tsc_per_count_den,
            id: 0,
            tpr: 0,
            // Reset: APIC soft-disabled, spurious vector 0xff. Linux enables it.
            svr: 0xff,
            ldr: 0,
            dfr: 0xffff_ffff, // flat model
            esr: 0,
            lvt_timer: LVT_MASK,
            lvt_thermal: LVT_MASK,
            lvt_perf: LVT_MASK,
            lvt_lint0: LVT_MASK,
            lvt_lint1: LVT_MASK,
            lvt_error: LVT_MASK,
            irr: VecBits::default(),
            isr: VecBits::default(),
            timer_deadline: None,
            timer_period: 0,
            timer_dcr: 0,
            timer_ict: 0,
        }
    }

    fn enabled(&self) -> bool {
        self.svr & SVR_ENABLE != 0
    }

    pub fn tpr(&self) -> u32 {
        self.tpr
    }

    /// Sync the task priority from KVM's `run->cr8` (the guest may set priority
    /// with `mov %cr8`; CR8 == TPR>>4). MMIO TPR writes go the other way (we push
    /// `tpr>>4` back into `run->cr8` before each entry, see main.rs).
    pub fn sync_tpr_from_cr8(&mut self, cr8: u64) {
        let tpr = ((cr8 & 0xf) as u32) << 4;
        if tpr != self.tpr {
            self.tpr = tpr;
        }
    }

    // ---- priority / delivery (textbook PPR) --------------------------------

    fn highest_isr(&self) -> u32 {
        self.isr.highest().map(u32::from).unwrap_or(0)
    }
    fn highest_irr(&self) -> u32 {
        self.irr.highest().map(u32::from).unwrap_or(0)
    }

    /// Processor Priority Register: the max of the task priority and the
    /// priority class of the highest in-service vector.
    fn ppr(&self) -> u32 {
        let isrv = self.highest_isr();
        let tpr = self.tpr;
        if (tpr & 0xf0) >= (isrv & 0xf0) {
            tpr
        } else {
            isrv & 0xf0
        }
    }

    /// The vector that should be injected now, or `None`. A pending IRR vector
    /// is deliverable iff the APIC is enabled and its priority class strictly
    /// exceeds the current PPR class.
    pub fn deliverable_vector(&self) -> Option<u8> {
        if !self.enabled() {
            return None;
        }
        let irrv = self.highest_irr();
        if irrv == 0 {
            return None;
        }
        if (irrv & 0xf0) > (self.ppr() & 0xf0) {
            Some(irrv as u8)
        } else {
            None
        }
    }

    /// Accept an injection of `vec` into service (IRR -> ISR). Called right after
    /// the vector is handed to KVM_INTERRUPT.
    pub fn ack_injected(&mut self, vec: u8) {
        self.irr.clear(vec);
        self.isr.set(vec);
    }

    /// Raise vector `vec` in the IRR (from the IOAPIC, a self-IPI, or the timer).
    pub fn raise(&mut self, vec: u8) {
        self.irr.set(vec);
    }

    // ---- MMIO ---------------------------------------------------------------

    pub fn mmio_read(&mut self, off: u32) -> u32 {
        match off {
            REG_ID => self.id << 24,
            REG_VERSION => LAPIC_VERSION,
            REG_TPR => self.tpr,
            REG_APR => self.ppr(), // arbitration ~ processor priority here
            REG_PPR => self.ppr(),
            REG_LDR => self.ldr,
            REG_DFR => self.dfr,
            REG_SVR => self.svr,
            REG_ESR => 0, // read-as-zero (write clears; see mmio_write)
            REG_ICR_LOW => 0,
            REG_ICR_HIGH => 0,
            REG_LVT_TIMER => self.lvt_timer,
            REG_LVT_THERMAL => self.lvt_thermal,
            REG_LVT_PERF => self.lvt_perf,
            REG_LVT_LINT0 => self.lvt_lint0,
            REG_LVT_LINT1 => self.lvt_lint1,
            REG_LVT_ERROR => self.lvt_error,
            REG_TIMER_ICT => self.timer_ict,
            REG_TIMER_CCT => self.timer_current_count(),
            REG_TIMER_DCR => self.timer_dcr,
            REG_RRD => {
                log_unsupported("remote-read register (RRD)", format_args!("read"));
                0
            }
            o if (REG_ISR0..REG_ISR0 + 0x80).contains(&o) && o % 0x10 == 0 => {
                self.isr.words[((o - REG_ISR0) / 0x10) as usize]
            }
            o if (REG_TMR0..REG_TMR0 + 0x80).contains(&o) && o % 0x10 == 0 => 0, // all edge
            o if (REG_IRR0..REG_IRR0 + 0x80).contains(&o) && o % 0x10 == 0 => {
                self.irr.words[((o - REG_IRR0) / 0x10) as usize]
            }
            _ => 0,
        }
    }

    pub fn mmio_write(&mut self, off: u32, val: u32) {
        match off {
            REG_ID => self.id = val >> 24,
            REG_TPR => self.tpr = val & 0xff,
            REG_EOI => self.eoi(),
            REG_LDR => self.ldr = val,
            REG_DFR => self.dfr = val,
            REG_SVR => self.svr = val,
            REG_ESR => self.esr = 0, // write-to-clear
            REG_ICR_HIGH => { /* destination field; delivery is accept-all-to-self */ }
            REG_ICR_LOW => self.write_icr(val),
            REG_LVT_TIMER => self.write_lvt_timer(val),
            REG_LVT_THERMAL => self.lvt_thermal = val,
            REG_LVT_PERF => self.lvt_perf = val,
            REG_LVT_LINT0 => self.lvt_lint0 = val,
            REG_LVT_LINT1 => self.lvt_lint1 = val,
            REG_LVT_ERROR => self.lvt_error = val,
            REG_TIMER_ICT => self.write_timer_ict(val),
            REG_TIMER_DCR => self.timer_dcr = val,
            REG_VERSION | REG_PPR | REG_APR => { /* read-only */ }
            o if (REG_ISR0..REG_IRR0 + 0x80).contains(&o) => { /* IRR/ISR/TMR read-only */ }
            _ => {}
        }
    }

    /// End-of-interrupt: clear the highest in-service vector and re-evaluate.
    fn eoi(&mut self) {
        if let Some(vec) = self.isr.highest() {
            self.isr.clear(vec);
        }
        // Nothing else to do here: the main loop re-checks deliverable_vector()
        // at the next boundary, so any now-unblocked IRR vector gets injected.
    }

    /// ICR write: send an IPI. We support only fixed delivery to self (Linux
    /// self-IPIs irq_work / the reschedule path onto the single CPU); everything
    /// else is loudly logged and dropped.
    fn write_icr(&mut self, val: u32) {
        let vector = (val & 0xff) as u8;
        let delivery_mode = (val >> 8) & 0x7;
        let dest_shorthand = (val >> 18) & 0x3;

        if delivery_mode != 0 {
            log_unsupported(
                "ICR delivery mode",
                format_args!("mode={delivery_mode} (only fixed=0 supported)"),
            );
            return;
        }
        // dest_shorthand: 0=field, 1=self, 2=all-incl-self, 3=all-excl-self.
        // We have exactly one CPU, so self / all-incl-self both mean "us"; a
        // field destination also resolves to us (accept-all). all-excl-self
        // (3) targets nobody here.
        match dest_shorthand {
            0 | 1 | 2 => self.raise(vector),
            3 => {}
            _ => unreachable!(),
        }
    }

    // ---- timer --------------------------------------------------------------

    fn timer_mode(&self) -> u32 {
        (self.lvt_timer >> LVT_TIMER_MODE_SHIFT) & 0x3
    }
    fn timer_vector(&self) -> u8 {
        (self.lvt_timer & 0xff) as u8
    }
    fn timer_masked(&self) -> bool {
        self.lvt_timer & LVT_MASK != 0
    }

    /// TDCR divide value (1,2,4,...,128).
    fn timer_divisor(&self) -> u64 {
        // Bits 0,1,3 form the code; 0b111 == divide-by-1.
        let code = ((self.timer_dcr & 0x8) >> 1) | (self.timer_dcr & 0x3);
        match code {
            0b000 => 2,
            0b001 => 4,
            0b010 => 8,
            0b011 => 16,
            0b100 => 32,
            0b101 => 64,
            0b110 => 128,
            0b111 => 1,
            _ => 1,
        }
    }

    /// vtsc span for `count` timer decrements at the current divisor, as the
    /// EXACT integer product `count * divisor * (EBX/EAX)` (CPUID 0x15 ratio),
    /// in 128-bit to avoid overflow. No floating point: the numerator and
    /// denominator are the raw CPUID 0x15 integers, so this is bit-identical to
    /// how the guest reasons about the crystal — deterministic every run.
    fn span_from_count(&self, count: u32) -> u64 {
        (u128::from(count)
            * u128::from(self.timer_divisor())
            * u128::from(self.tsc_per_count_num)
            / u128::from(self.tsc_per_count_den)) as u64
    }

    /// Live count remaining until `deadline` at vtsc `now` (inverse of the
    /// above): `(deadline - now) * EAX / (divisor * EBX)`, integer-only.
    fn count_until(&self, deadline: u64, now: u64) -> u32 {
        if now >= deadline {
            return 0;
        }
        let c = u128::from(deadline - now) * u128::from(self.tsc_per_count_den)
            / (u128::from(self.timer_divisor()) * u128::from(self.tsc_per_count_num));
        c.min(u128::from(u32::MAX)) as u32
    }

    fn write_lvt_timer(&mut self, val: u32) {
        let old_mode = self.timer_mode();
        self.lvt_timer = val;
        // Changing the timer mode disarms any pending timer; the guest re-arms
        // (TMICT for one-shot/periodic, or WRMSR for deadline). A same-mode write
        // (e.g. just toggling the mask) leaves the armed timer running.
        if self.timer_mode() != old_mode {
            self.timer_deadline = None;
            self.timer_period = 0;
        }
    }

    fn write_timer_ict(&mut self, val: u32) {
        self.timer_ict = val;
        let mode = self.timer_mode();
        // In TSC-deadline mode TMICT is ignored by hardware.
        if mode == LVT_TIMER_MODE_TSCDEADLINE {
            return;
        }
        if val == 0 {
            self.timer_deadline = None;
            self.timer_period = 0;
            return;
        }
        let now = self.clock.vtsc_now();
        let span = self.span_from_count(val);
        self.timer_deadline = Some(now.wrapping_add(span));
        // Periodic re-arms by this span on each fire; one-shot fires once.
        self.timer_period = if mode == LVT_TIMER_MODE_PERIODIC {
            span
        } else {
            0
        };
    }

    fn timer_current_count(&self) -> u32 {
        match self.timer_mode() {
            LVT_TIMER_MODE_ONESHOT | LVT_TIMER_MODE_PERIODIC => match self.timer_deadline {
                Some(dl) => self.count_until(dl, self.clock.vtsc_now()),
                None => 0,
            },
            // Deadline mode: TMCCT reads 0 (the timer is MSR-driven).
            _ => 0,
        }
    }

    /// WRMSR IA32_TSC_DEADLINE: arm (or, with 0, disarm) the TSC-deadline timer.
    /// The value is an absolute guest-TSC (== vtsc) deadline. Not wired on this
    /// host (see [`MSR_IA32_TSC_DEADLINE`]); retained + tested for completeness.
    #[allow(dead_code)]
    pub fn write_tsc_deadline(&mut self, deadline: u64) {
        self.timer_deadline = if deadline == 0 { None } else { Some(deadline) };
        self.timer_period = 0;
    }

    /// RDMSR IA32_TSC_DEADLINE: the pending absolute deadline (0 if disarmed or
    /// already elapsed/fired).
    #[allow(dead_code)]
    pub fn read_tsc_deadline(&self) -> u64 {
        match self.timer_deadline {
            Some(dl) if self.timer_mode() == LVT_TIMER_MODE_TSCDEADLINE => dl,
            _ => 0,
        }
    }

    /// The next timer deadline (vtsc), for the event queue / park timeout.
    pub fn timer_deadline(&self) -> Option<u64> {
        self.timer_deadline
    }

    /// If the timer is armed and due at `now`, fire it: raise the LVT timer
    /// vector (unless masked). One-shot/deadline disarm; periodic re-arms to the
    /// next future boundary (missed periods coalesce into one interrupt — a real
    /// APIC does not queue ticks). Returns whether it fired.
    pub fn fire_timer_if_due(&mut self, now: u64) -> bool {
        match self.timer_deadline {
            Some(dl) if now >= dl => {
                if self.timer_period > 0 {
                    // Periodic: advance past `now` so we do not spin.
                    let mut next = dl;
                    while now >= next {
                        next = next.wrapping_add(self.timer_period);
                    }
                    self.timer_deadline = Some(next);
                } else {
                    self.timer_deadline = None;
                }
                if !self.timer_masked() {
                    let vec = self.timer_vector();
                    self.raise(vec);
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtsc::TscFrequency;

    const TEST_BUS_HZ: u64 = 25_000_000;

    // The tests model an APIC input ("bus") crystal of TEST_BUS_HZ, so one count
    // at divisor 1 is `tsc_hz / TEST_BUS_HZ` TSC cycles — expressed as the exact
    // integer ratio (num=tsc_hz, den=TEST_BUS_HZ), the same shape production uses
    // with CPUID 0x15's (EBX, EAX).
    fn lapic() -> Lapic {
        Lapic::new(
            VirtualClock::new(0, TscFrequency::from_hz(3_000_000_000)),
            3_000_000_000,
            TEST_BUS_HZ,
        )
    }

    fn lapic_tsc(tsc_hz: u64) -> Lapic {
        Lapic::new(VirtualClock::new(0, TscFrequency::from_hz(tsc_hz)), tsc_hz, TEST_BUS_HZ)
    }

    #[test]
    fn vecbits_highest_and_set_clear() {
        let mut v = VecBits::default();
        assert_eq!(v.highest(), None);
        v.set(0x30);
        v.set(0xa1);
        v.set(0x20);
        assert_eq!(v.highest(), Some(0xa1));
        assert!(v.get(0x30));
        v.clear(0xa1);
        assert_eq!(v.highest(), Some(0x30));
    }

    #[test]
    fn disabled_apic_delivers_nothing() {
        let mut l = lapic();
        l.raise(0x40);
        // SVR reset has enable bit clear.
        assert_eq!(l.deliverable_vector(), None);
    }

    #[test]
    fn ppr_priority_gates_delivery() {
        let mut l = lapic();
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff); // enable
        l.raise(0x30);
        assert_eq!(l.deliverable_vector(), Some(0x30));
        // Raise TPR to class 4 (0x40): a class-3 vector is masked.
        l.mmio_write(REG_TPR, 0x40);
        assert_eq!(l.deliverable_vector(), None);
        // A class-5 vector gets through.
        l.raise(0x50);
        assert_eq!(l.deliverable_vector(), Some(0x50));
    }

    #[test]
    fn inservice_blocks_equal_class_until_eoi() {
        let mut l = lapic();
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        l.raise(0x51);
        let v = l.deliverable_vector().unwrap();
        l.ack_injected(v); // 0x51 in service, PPR now class 5
        l.raise(0x52); // same class -> not > PPR class
        assert_eq!(l.deliverable_vector(), None);
        l.mmio_write(REG_EOI, 0); // clear 0x51
        assert_eq!(l.deliverable_vector(), Some(0x52));
    }

    #[test]
    fn irr_isr_mmio_reflect_bits() {
        let mut l = lapic();
        l.raise(0x21); // word 1, bit 1
        assert_eq!(l.mmio_read(REG_IRR0 + 0x10), 1 << 1);
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        let v = l.deliverable_vector().unwrap();
        l.ack_injected(v);
        assert_eq!(l.mmio_read(REG_ISR0 + 0x10), 1 << 1);
        assert_eq!(l.mmio_read(REG_IRR0 + 0x10), 0);
    }

    #[test]
    fn tsc_deadline_arm_fire_disarm() {
        let mut l = lapic();
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        // deadline mode, vector 0xec, unmasked
        l.mmio_write(REG_LVT_TIMER, (LVT_TIMER_MODE_TSCDEADLINE << LVT_TIMER_MODE_SHIFT) | 0xec);
        l.write_tsc_deadline(1_000_000);
        assert_eq!(l.timer_deadline(), Some(1_000_000));
        assert_eq!(l.read_tsc_deadline(), 1_000_000);
        assert!(!l.fire_timer_if_due(999_999));
        assert!(l.fire_timer_if_due(1_000_000));
        assert_eq!(l.timer_deadline(), None); // disarmed
        assert_eq!(l.read_tsc_deadline(), 0);
        assert_eq!(l.deliverable_vector(), Some(0xec)); // fired into IRR
        assert!(!l.fire_timer_if_due(2_000_000)); // idempotent after fire
    }

    #[test]
    fn masked_timer_does_not_raise() {
        let mut l = lapic();
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        l.mmio_write(
            REG_LVT_TIMER,
            LVT_MASK | (LVT_TIMER_MODE_TSCDEADLINE << LVT_TIMER_MODE_SHIFT) | 0xec,
        );
        l.write_tsc_deadline(500);
        assert!(l.fire_timer_if_due(500));
        assert_eq!(l.deliverable_vector(), None);
    }

    #[test]
    fn self_ipi_fixed_sets_irr() {
        let mut l = lapic();
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        // fixed delivery (mode 0), self shorthand (1), vector 0xf7
        l.mmio_write(REG_ICR_LOW, (1 << 18) | 0xf7);
        assert_eq!(l.deliverable_vector(), Some(0xf7));
    }

    // TDCR value for divide-by-1: code 0b111 == bits {3,1,0} set == 0b1011.
    const TDCR_DIV1: u32 = 0b1011;
    // TDCR value for divide-by-16: code 0b011 == 0b0011.
    const TDCR_DIV16: u32 = 0b0011;

    #[test]
    fn timer_divisor_decodes_tdcr() {
        let mut l = lapic();
        l.timer_dcr = TDCR_DIV1;
        assert_eq!(l.timer_divisor(), 1);
        l.timer_dcr = TDCR_DIV16;
        assert_eq!(l.timer_divisor(), 16);
    }

    #[test]
    fn oneshot_current_count_counts_down() {
        // 1 GHz TSC, divide-by-1: 1 APIC tick = 1e9/25e6 = 40 vtsc cycles.
        let mut l = lapic_tsc(1_000_000_000);
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        l.mmio_write(REG_TIMER_DCR, TDCR_DIV1);
        l.mmio_write(REG_LVT_TIMER, (LVT_TIMER_MODE_ONESHOT << LVT_TIMER_MODE_SHIFT) | 0x40);
        l.mmio_write(REG_TIMER_ICT, 1000);
        // deadline is now+span; current count immediately ~1000 (never above).
        assert!(l.timer_deadline().is_some());
        assert!(l.mmio_read(REG_TIMER_CCT) <= 1000);
    }

    #[test]
    fn span_uses_cpuid_0x15_ratio_exactly() {
        // This host's CPUID 0x15: EAX=2 (den), EBX=160 (num) => 80 TSC cycles per
        // count at divisor 1. The span must be the EXACT integer product
        // count*divisor*EBX/EAX, with no dependence on tsc_hz/crystal_hz rounding.
        let clock = VirtualClock::new(0, TscFrequency::from_hz(3_072_000_000));
        let mut l = Lapic::new(clock, 160, 2); // (num=EBX, den=EAX)
        l.timer_dcr = TDCR_DIV1;
        assert_eq!(l.span_from_count(1), 80);
        assert_eq!(l.span_from_count(1_000), 80_000);
        assert_eq!(l.count_until(80_000, 0), 1_000); // exact inverse
        // divisor 16 (Linux APIC_DIVISOR): 80*16 = 1280 cycles/count.
        l.timer_dcr = TDCR_DIV16;
        assert_eq!(l.span_from_count(1), 1_280);
        assert_eq!(l.count_until(1_280, 0), 1); // exact inverse
    }

    #[test]
    fn timer_span_and_count_are_inverse_at_25mhz() {
        // 1 GHz TSC, divide-by-1: one APIC tick = 1e9/25e6 = 40 vtsc cycles.
        let mut l = lapic_tsc(1_000_000_000);
        l.timer_dcr = TDCR_DIV1;
        assert_eq!(l.span_from_count(1), 40);
        assert_eq!(l.span_from_count(1000), 40_000);
        assert_eq!(l.count_until(40_000, 0), 1000);
        assert_eq!(l.count_until(40_000, 20_000), 500);
        assert_eq!(l.count_until(40_000, 40_000), 0);
        assert_eq!(l.count_until(40_000, 999_999), 0); // past deadline -> 0
    }

    #[test]
    fn calibration_recovers_the_declared_bus_frequency() {
        // Emulate Linux calibrate_APIC_clock: program a big count (divide-by-16,
        // Linux's APIC_DIVISOR), sample TMCCT over a known vtsc interval, derive
        // decrements/sec * divisor == the declared bus Hz — every run.
        for &tsc_hz in &[1_000_000_000u64, 2_500_000_000, 3_072_000_000] {
            let mut l = lapic_tsc(tsc_hz);
            l.timer_dcr = TDCR_DIV16;
            let span_total = l.span_from_count(1_000_000);
            let dt = tsc_hz / 100; // 10 ms of vtsc
            let c1 = l.count_until(span_total, 0);
            let c2 = l.count_until(span_total, dt);
            let decrements = u128::from(c1 - c2);
            // rate = decrements * (tsc_hz / dt); * divisor(16) = bus frequency.
            let measured_bus = (decrements * u128::from(tsc_hz) / u128::from(dt)) * 16;
            assert!(
                (measured_bus as i128 - TEST_BUS_HZ as i128).abs() < 50_000,
                "tsc_hz={tsc_hz}: measured {measured_bus} Hz vs declared {TEST_BUS_HZ}"
            );
        }
    }

    #[test]
    fn periodic_timer_refires_and_counts_within_period() {
        let mut l = lapic_tsc(1_000_000_000);
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        l.mmio_write(REG_TIMER_DCR, TDCR_DIV1);
        // periodic, vector 0x40, unmasked
        l.mmio_write(REG_LVT_TIMER, (LVT_TIMER_MODE_PERIODIC << LVT_TIMER_MODE_SHIFT) | 0x40);
        l.mmio_write(REG_TIMER_ICT, 1000); // period = 40_000 vtsc
        let dl0 = l.timer_deadline().unwrap();
        // Not due before the deadline.
        assert!(!l.fire_timer_if_due(dl0 - 1));
        // Due at the deadline: fires, re-arms one period later.
        assert!(l.fire_timer_if_due(dl0));
        assert_eq!(l.deliverable_vector(), Some(0x40));
        assert_eq!(l.timer_deadline(), Some(dl0 + 40_000));
        // Coalesce: jumping several periods ahead still re-arms to the future.
        l.mmio_write(REG_EOI, 0);
        let far = dl0 + 40_000 * 5 + 7;
        assert!(l.fire_timer_if_due(far));
        assert!(l.timer_deadline().unwrap() > far);
    }

    #[test]
    fn masked_periodic_rearms_without_delivering() {
        let mut l = lapic_tsc(1_000_000_000);
        l.mmio_write(REG_SVR, SVR_ENABLE | 0xff);
        l.mmio_write(REG_TIMER_DCR, TDCR_DIV1);
        l.mmio_write(
            REG_LVT_TIMER,
            LVT_MASK | (LVT_TIMER_MODE_PERIODIC << LVT_TIMER_MODE_SHIFT) | 0x40,
        );
        l.mmio_write(REG_TIMER_ICT, 1000);
        let dl0 = l.timer_deadline().unwrap();
        assert!(l.fire_timer_if_due(dl0)); // fired (re-armed) but masked
        assert_eq!(l.deliverable_vector(), None);
        assert_eq!(l.timer_deadline(), Some(dl0 + 40_000));
    }
}
