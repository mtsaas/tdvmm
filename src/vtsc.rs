//! Virtual-clock authority + the one TSC-frequency module.
//!
//! ## The single clock (the invariant)
//!
//! There is exactly ONE virtual clock in this VMM, and it is the guest's own
//! TSC. KVM programs each vCPU with a *TSC offset* so that, inside the guest,
//!
//! ```text
//!     guest RDTSC  ==  host RDTSC + tsc_offset
//! ```
//!
//! [`VirtualClock::vtsc_now`] returns exactly that same value from the host
//! side. So `vtsc_now()` and what the guest reads with `RDTSC` are the SAME
//! clock — same offset, same frequency, no second source of truth, ever. Every
//! other time-derived thing in the VMM (the userspace PIT counter today; the
//! userspace LAPIC timer and the event queue) is a pure function of
//! this one clock. That is what makes later TSC fast-forward sound: move the
//! offset and *everything* moves with it, atomically, because there is nothing
//! else to move.
//!
//! The offset is read ONCE, via the `KVM_VCPU_TSC_OFFSET` vCPU device attribute
//! (group `KVM_VCPU_TSC_CTRL`), which needs kernel >= 5.16. We do not cache a
//! host-time sample or anything else — `vtsc_now()` re-reads the host TSC every
//! call and adds the *cached* offset, so it stays exactly in step with the guest.
//!
//! ## The cached offset (fast-forward)
//!
//! The offset is not fixed: when the guest is idle the parker
//! JUMPs virtual time forward by bumping the offset (see [`VirtualClock::bump_offset`]).
//! The offset lives in a single shared cell ([`std::cell::Cell`] behind an
//! [`std::rc::Rc`]) so that every `VirtualClock` clone — the authority in the
//! vCPU loop, and the copies the LAPIC and PIT hold — reads the SAME value. A
//! bump is **write-through**: it updates KVM's `KVM_VCPU_TSC_OFFSET` (so the
//! guest's own RDTSC moves in lockstep) AND the cached cell, in that order. The
//! hot path (`vtsc_now`) only reads the cell — it never issues an ioctl. The
//! whole VMM is single-threaded on the vCPU thread, so a plain `Rc<Cell<..>>`
//! (no atomics/locks) is sufficient and the offset is only ever written while
//! parked at a HLT exit, between `KVM_RUN`s.

use std::cell::Cell;
use std::rc::Rc;

use kvm_bindings::{kvm_device_attr, KVM_VCPU_TSC_CTRL, KVM_VCPU_TSC_OFFSET};
use kvm_ioctls::VcpuFd;
use vmm_sys_util::ioctl::{ioctl_with_mut_ref, ioctl_with_ref};
use vmm_sys_util::ioctl_iow_nr;

// KVM device-attribute ioctls, issued straight against the *vCPU* fd. Both are
// `_IOW` in the KVM UAPI (the struct is passed in; GET returns its data through
// the `addr` pointer, so the ioctl direction is still write-only on the struct):
//   KVM_GET_DEVICE_ATTR = _IOW(KVMIO, 0xe2, struct kvm_device_attr)
//   KVM_HAS_DEVICE_ATTR = _IOW(KVMIO, 0xe3, struct kvm_device_attr)
//
// The KVM_VCPU_TSC_CTRL group lives on the vCPU fd, but kvm-ioctls 0.25 exposes
// `has_device_attr` / `get_device_attr` only for aarch64 (VcpuFd) or DeviceFd —
// not for an x86_64 vCPU. The ioctl numbers are identical regardless of which
// fd they target, so we invoke them directly on the vCPU fd (which is
// `AsRawFd`), exactly as the crate does internally (see `kvm_ioctls.rs`, which
// likewise defines KVM_GET_DEVICE_ATTR with `ioctl_iow_nr!`).
//
// HOUSEKEEPING: this raw-ioctl workaround is deliberately confined to this
// module. Re-check on future kvm-ioctls upgrades — if a later release wraps the
// device-attr ioctls on x86_64 `VcpuFd`, drop these two definitions and the
// `ioctl_with_ref`/`ioctl_with_mut_ref` calls in favor of the crate methods.
const KVMIO: u32 = 0xAE;
ioctl_iow_nr!(KVM_HAS_DEVICE_ATTR, KVMIO, 0xe3, kvm_device_attr);
ioctl_iow_nr!(KVM_GET_DEVICE_ATTR, KVMIO, 0xe2, kvm_device_attr);
// KVM_SET_DEVICE_ATTR = _IOW(KVMIO, 0xe1, struct kvm_device_attr): the write-side
// of the same attribute interface, used to bump KVM_VCPU_TSC_OFFSET.
ioctl_iow_nr!(KVM_SET_DEVICE_ATTR, KVMIO, 0xe1, kvm_device_attr);

#[derive(Debug)]
pub struct VtscError(String);
impl std::fmt::Display for VtscError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for VtscError {}

/// The one TSC-frequency module: converts between TSC cycles and nanoseconds.
///
/// Sourced ONCE (from `KVM_GET_TSC_KHZ`, i.e. the guest's TSC frequency, which
/// on this host equals the host TSC frequency — no TSC scaling) and then used
/// by everything that needs a cycles<->ns conversion. One frequency, one place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TscFrequency {
    hz: u64,
}

impl TscFrequency {
    /// Build from a raw Hz value. Panics on zero — a zero TSC frequency is a
    /// programming error, never a legitimate runtime value.
    #[allow(dead_code)] // used by tests today; a convenience constructor
    pub fn from_hz(hz: u64) -> Self {
        assert!(hz > 0, "TSC frequency must be non-zero");
        Self { hz }
    }

    /// Build from kHz (as `KVM_GET_TSC_KHZ` reports).
    pub fn from_khz(khz: u32) -> Self {
        Self::from_hz(u64::from(khz) * 1_000)
    }

    pub fn hz(&self) -> u64 {
        self.hz
    }

    pub fn khz(&self) -> u64 {
        self.hz / 1_000
    }

    /// Convert a TSC-cycle count to nanoseconds (rounded down). Uses 128-bit
    /// intermediate math so it does not overflow for any 64-bit cycle count.
    #[allow(dead_code)] // the cycles<->ns module; drives event scheduling
    pub fn cycles_to_ns(&self, cycles: u64) -> u64 {
        ((u128::from(cycles) * 1_000_000_000u128) / u128::from(self.hz)) as u64
    }

    /// Convert a nanosecond duration to TSC cycles (rounded down). 128-bit
    /// intermediate math; no overflow for any 64-bit nanosecond count.
    #[allow(dead_code)] // the cycles<->ns module; drives event scheduling
    pub fn ns_to_cycles(&self, ns: u64) -> u64 {
        ((u128::from(ns) * u128::from(self.hz)) / 1_000_000_000u128) as u64
    }
}

/// Read the host TSC directly. This is the *same* counter the guest reads with
/// `RDTSC`; adding the KVM TSC offset yields the guest's view.
#[inline]
pub fn host_rdtsc() -> u64 {
    // SAFETY: `_rdtsc` is always safe to execute on x86_64 in user mode.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// The virtual clock: the guest TSC, observable from the host.
///
/// Holds the TSC offset (in a shared cell — see the module docs) and the one
/// TSC-frequency module. `Clone` shares the offset cell, so the authority in the
/// vCPU loop and the copies the LAPIC/PIT hold all observe the same value; a
/// [`bump_offset`](Self::bump_offset) is therefore seen by every clone.
/// Deliberately NOT `Copy`: an accidental by-value copy would silently detach a
/// snapshot of the offset from future bumps.
#[derive(Clone, Debug)]
pub struct VirtualClock {
    /// guest_tsc = host_tsc + tsc_offset. Signed: KVM's offset can be negative.
    /// Shared + interior-mutable so all clones track fast-forward bumps; only
    /// ever written on the vCPU thread while parked (single-writer).
    tsc_offset: Rc<Cell<i64>>,
    freq: TscFrequency,
}

impl VirtualClock {
    /// Construct from an explicit offset + frequency. Used by unit tests and by
    /// [`VirtualClock::from_vcpu`].
    #[allow(dead_code)] // used by tests; explicit constructor
    pub fn new(tsc_offset: i64, freq: TscFrequency) -> Self {
        Self {
            tsc_offset: Rc::new(Cell::new(tsc_offset)),
            freq,
        }
    }

    /// Establish the virtual clock for `vcpu`: read the TSC offset ONCE via the
    /// `KVM_VCPU_TSC_OFFSET` device attribute and take the TSC frequency from
    /// `KVM_GET_TSC_KHZ`. Must be called after the boot MSRs are programmed (so
    /// the offset reflects the guest's initial `IA32_TSC`).
    ///
    /// Returns an error (caller should STOP and report) if the kernel does not
    /// expose the attribute — there is no fallback clock by design.
    pub fn from_vcpu(vcpu: &VcpuFd) -> Result<Self, VtscError> {
        let khz = vcpu
            .get_tsc_khz()
            .map_err(|e| VtscError(format!("KVM_GET_TSC_KHZ failed: {e}")))?;
        if khz == 0 {
            return Err(VtscError("KVM reported a zero TSC frequency".into()));
        }
        let freq = TscFrequency::from_khz(khz);
        let tsc_offset = read_tsc_offset(vcpu)?;
        Ok(Self {
            tsc_offset: Rc::new(Cell::new(tsc_offset)),
            freq,
        })
    }

    /// The current virtual time in TSC cycles: `host RDTSC + tsc_offset`.
    ///
    /// INVARIANT: this is bit-for-bit the value the guest gets from `RDTSC` at
    /// the same instant. Same offset, same frequency, one clock. Nothing in the
    /// VMM may derive guest time from any other source.
    #[inline]
    pub fn vtsc_now(&self) -> u64 {
        self.vtsc_from_host(host_rdtsc())
    }

    /// Pure form of [`vtsc_now`]: map a specific host-TSC reading to virtual
    /// time. Factored out so the offset arithmetic is unit-testable without KVM.
    #[inline]
    pub fn vtsc_from_host(&self, host_tsc: u64) -> u64 {
        // guest_tsc = host_tsc + tsc_offset, in wrapping u64 space (exactly how
        // the CPU computes the guest TSC). Reads the cached offset cell only —
        // never an ioctl, so this stays cheap in the hot loop.
        (host_tsc as i128 + self.tsc_offset.get() as i128) as u64
    }

    pub fn tsc_offset(&self) -> i64 {
        self.tsc_offset.get()
    }

    pub fn freq(&self) -> TscFrequency {
        self.freq
    }

    /// Fast-forward the virtual clock by `delta` cycles (the JUMP primitive).
    ///
    /// Bumps `tsc_offset` by `delta` **write-through**: it programs KVM's
    /// `KVM_VCPU_TSC_OFFSET` first (so the guest's own RDTSC advances by exactly
    /// `delta` too) and only then updates the cached cell, so the cache never
    /// runs ahead of KVM. Every clone of this clock (LAPIC, PIT) sees the new
    /// value immediately via the shared cell.
    ///
    /// INVARIANTS (asserted): `delta >= 0` — the offset is monotonically
    /// non-decreasing forever; virtual time never runs backwards. Must be called
    /// ONLY on the vCPU thread while parked at a HLT exit, between `KVM_RUN`s.
    pub fn bump_offset(&self, vcpu: &VcpuFd, delta: i64) -> Result<(), VtscError> {
        assert!(delta >= 0, "TSC offset must be monotonically non-decreasing (delta={delta})");
        let old = self.tsc_offset.get();
        let new = old
            .checked_add(delta)
            .ok_or_else(|| VtscError(format!("TSC offset overflow: {old} + {delta}")))?;
        // Write-through to KVM first: the guest TSC must move with our cache.
        write_tsc_offset(vcpu, new)?;
        self.tsc_offset.set(new);
        debug_assert!(new >= old, "offset went backwards: {old} -> {new}");
        Ok(())
    }

    /// Update only the cached offset cell (no KVM write). For unit tests that
    /// exercise the cache/clone semantics without a live vCPU.
    #[cfg(test)]
    pub fn bump_cached_for_test(&self, delta: i64) {
        self.tsc_offset.set(self.tsc_offset.get() + delta);
    }
}

/// Read the current TSC offset for `vcpu` via KVM_HAS/GET_DEVICE_ATTR on the
/// vCPU fd (group `KVM_VCPU_TSC_CTRL`, attr `KVM_VCPU_TSC_OFFSET`).
/// `pub(crate)` for `doctor`'s KVM check only — the boot path goes through
/// [`VirtualClock::from_vcpu`].
pub(crate) fn read_tsc_offset(vcpu: &VcpuFd) -> Result<i64, VtscError> {
    let probe = kvm_device_attr {
        group: KVM_VCPU_TSC_CTRL,
        attr: u64::from(KVM_VCPU_TSC_OFFSET),
        addr: 0,
        flags: 0,
    };
    // SAFETY: valid vCPU fd; KVM_HAS_DEVICE_ATTR only reads `probe` and returns
    // a status, writing nothing back.
    let has = unsafe { ioctl_with_ref(vcpu, KVM_HAS_DEVICE_ATTR(), &probe) };
    if has != 0 {
        return Err(VtscError(format!(
            "KVM_VCPU_TSC_OFFSET attribute unavailable (need kernel >= 5.16): {}",
            std::io::Error::last_os_error()
        )));
    }

    let mut offset: i64 = 0;
    let mut attr = kvm_device_attr {
        group: KVM_VCPU_TSC_CTRL,
        attr: u64::from(KVM_VCPU_TSC_OFFSET),
        addr: &mut offset as *mut i64 as u64,
        flags: 0,
    };
    // SAFETY: `attr.addr` points at a live, writable i64 that outlives the call;
    // the ioctl writes exactly 8 bytes (the offset) there. The vCPU fd is valid.
    let ret = unsafe { ioctl_with_mut_ref(vcpu, KVM_GET_DEVICE_ATTR(), &mut attr) };
    if ret != 0 {
        return Err(VtscError(format!(
            "KVM_GET_DEVICE_ATTR(KVM_VCPU_TSC_OFFSET) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(offset)
}

/// Write a new absolute TSC offset for `vcpu` via KVM_SET_DEVICE_ATTR on the vCPU
/// fd (group `KVM_VCPU_TSC_CTRL`, attr `KVM_VCPU_TSC_OFFSET`). This is the write
/// side used by [`VirtualClock::bump_offset`]; scaling (KVM_SET_TSC_KHZ) is left
/// untouched, so the guest TSC stays 1:1 with the host rate — only the offset
/// moves.
fn write_tsc_offset(vcpu: &VcpuFd, offset: i64) -> Result<(), VtscError> {
    let attr = kvm_device_attr {
        group: KVM_VCPU_TSC_CTRL,
        attr: u64::from(KVM_VCPU_TSC_OFFSET),
        addr: &offset as *const i64 as u64,
        flags: 0,
    };
    // SAFETY: `attr.addr` points at a live i64 that outlives the call; the ioctl
    // reads exactly 8 bytes (the new offset) from it. The vCPU fd is valid.
    let ret = unsafe { ioctl_with_ref(vcpu, KVM_SET_DEVICE_ATTR(), &attr) };
    if ret != 0 {
        return Err(VtscError(format!(
            "KVM_SET_DEVICE_ATTR(KVM_VCPU_TSC_OFFSET) failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freq_cycles_ns_roundtrip_exact_multiples() {
        // 1 GHz: 1 cycle == 1 ns, exactly.
        let f = TscFrequency::from_hz(1_000_000_000);
        assert_eq!(f.cycles_to_ns(1), 1);
        assert_eq!(f.cycles_to_ns(1_000_000_000), 1_000_000_000);
        assert_eq!(f.ns_to_cycles(1), 1);
        assert_eq!(f.ns_to_cycles(1_000_000_000), 1_000_000_000);
    }

    #[test]
    fn freq_khz_constructor() {
        let f = TscFrequency::from_khz(3_072_000); // ~3.072 GHz, this host
        assert_eq!(f.hz(), 3_072_000_000);
        assert_eq!(f.khz(), 3_072_000);
    }

    #[test]
    fn freq_ns_to_cycles_scales_with_frequency() {
        // At 3 GHz, 1 second == 3e9 cycles; 1 ms == 3e6 cycles.
        let f = TscFrequency::from_hz(3_000_000_000);
        assert_eq!(f.ns_to_cycles(1_000_000_000), 3_000_000_000);
        assert_eq!(f.ns_to_cycles(1_000_000), 3_000_000);
        // And back (exact here).
        assert_eq!(f.cycles_to_ns(3_000_000_000), 1_000_000_000);
    }

    #[test]
    fn freq_no_overflow_at_u64_extremes() {
        // 128-bit intermediates must not overflow for huge cycle/ns counts.
        let f = TscFrequency::from_hz(4_000_000_000);
        let _ = f.cycles_to_ns(u64::MAX); // must not panic
        let _ = f.ns_to_cycles(u64::MAX); // must not panic
    }

    #[test]
    fn vtsc_offset_positive() {
        let clk = VirtualClock::new(1_000, TscFrequency::from_hz(3_000_000_000));
        assert_eq!(clk.vtsc_from_host(500), 1_500);
        assert_eq!(clk.tsc_offset(), 1_000);
    }

    #[test]
    fn vtsc_offset_negative() {
        // KVM offsets are signed; a guest started "behind" host TSC is normal.
        let clk = VirtualClock::new(-1_000, TscFrequency::from_hz(3_000_000_000));
        assert_eq!(clk.vtsc_from_host(5_000), 4_000);
    }

    #[test]
    fn vtsc_now_is_monotonic_nondecreasing() {
        // Real host RDTSC: two successive reads never go backwards.
        let clk = VirtualClock::new(0, TscFrequency::from_hz(3_000_000_000));
        let a = clk.vtsc_now();
        let b = clk.vtsc_now();
        assert!(b >= a, "vtsc went backwards: {a} -> {b}");
    }

    #[test]
    fn clones_share_the_cached_offset() {
        // The invariant: a bump through one handle is visible through every
        // clone (this is what makes the LAPIC/PIT copies track a fast-forward).
        let a = VirtualClock::new(1_000, TscFrequency::from_hz(3_072_000_000));
        let b = a.clone();
        a.bump_cached_for_test(500);
        assert_eq!(b.tsc_offset(), 1_500, "clone did not see the bump");
        assert_eq!(b.vtsc_from_host(0), 1_500);
    }

    #[test]
    fn bump_lands_exactly_on_target() {
        // Post-bump exactness (the park assert), evaluated at a FROZEN host sample:
        // vtsc_from_host(h) must equal the target after adding (target - now).
        let clk = VirtualClock::new(-1_234, TscFrequency::from_hz(3_072_000_000));
        let h = 10_000_000u64;
        let now = clk.vtsc_from_host(h);
        let target = now + 5_000;
        let delta = (target - now) as i64;
        clk.bump_cached_for_test(delta);
        assert_eq!(clk.vtsc_from_host(h), target, "did not land exactly on target");
    }
}
