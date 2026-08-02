//! CPUID filter.
//!
//! This module exists now, not later, because it protects the whole point of
//! the project: deterministic, fast-forwardable virtual time. The edits to
//! the CPUID KVM reports to the guest:
//!
//! 1. **Mask the KVM paravirt-clock leaves** (the `0x4000_00xx` hypervisor
//!    range: `KVM_CPUID_SIGNATURE` @ `0x4000_0000`, `KVM_CPUID_FEATURES` @
//!    `0x4000_0001`). Without the signature the guest never discovers
//!    `kvmclock` and so never adopts it. If it did, a later TSC fast-forward
//!    would be silently undone by the guest re-reading host wall-clock time.
//!    We also clear the "hypervisor present" bit in leaf 1 so the guest does
//!    not go looking for a paravirt surface at all.
//!
//! 2. **Mask MONITOR/MWAIT** (leaf 1, ECX bit 3) so a guest idle instruction
//!    traps to the VMM as HLT instead of MWAIT-idling inside the guest.
//!
//! 3. **Expose Invariant TSC** (leaf `0x8000_0007`, EDX bit 8) so the guest
//!    treats the TSC as a stable, non-stop clocksource — the anchor the later
//!    TSC fast-forward stands on.
//!
//! 4. **Pass through the frequency leaves** `0x15` (TSC / core-crystal ratio)
//!    and `0x16` (processor base / bus frequency) from the *host* CPUID. With
//!    the in-kernel PIT gone (Step 3a), these give the guest a direct, correct
//!    TSC frequency without any timer hardware. They are read straight off the
//!    host CPU: the guest runs on that same CPU and (no TSC scaling here) its
//!    TSC ticks at the host rate, so the host values are exactly right.
//!
//! 5. **Mask x2APIC** (leaf 1, ECX bit 21). x2APIC mode needs interrupt
//!    remapping / ACPI to enumerate, neither of which we provide; masking it
//!    keeps the guest on the xAPIC MMIO window at `0xFEE00xxx` — the surface the
//!    Step-3b userspace LAPIC serves — and makes the MP table alone sufficient.
//!
//! 6. **Clear the LAPIC TSC-deadline timer bit** (leaf 1, ECX bit 24). A KVM
//!    WRMSR fastpath no-ops IA32_TSC_DEADLINE before the MSR filter when there is
//!    no in-kernel LAPIC, so the userspace LAPIC would never see the arming
//!    write; with the bit cleared the guest instead uses the LAPIC one-shot/
//!    periodic timer (MMIO), which we serve. See [`filter_cpuid`].

use kvm_bindings::{kvm_cpuid_entry2, CpuId, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::Kvm;

const HYPERVISOR_LEAF_LOW: u32 = 0x4000_0000;
const HYPERVISOR_LEAF_HIGH: u32 = 0x4000_00ff;
const LEAF_FEATURE_INFO: u32 = 0x1;
const LEAF_TSC_FREQ: u32 = 0x15;
const LEAF_CPU_FREQ: u32 = 0x16;
const LEAF_EXT_POWER_MGMT: u32 = 0x8000_0007;

// Leaf 1 ECX bits.
const ECX_MONITOR: u32 = 1 << 3;
const ECX_TSC_DEADLINE: u32 = 1 << 24;
const ECX_X2APIC: u32 = 1 << 21;
const ECX_HYPERVISOR: u32 = 1 << 31;
// Leaf 0x8000_0007 EDX bit.
const EDX_INVARIANT_TSC: u32 = 1 << 8;

#[derive(Debug)]
pub struct CpuidError(String);
impl std::fmt::Display for CpuidError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to build filtered CPUID: {}", self.0)
    }
}
impl std::error::Error for CpuidError {}

/// Produce the CPUID we hand to the vCPU from KVM's supported set — see the
/// module docs above for the per-edit rationale (the README has more on the
/// TSC-deadline WRMSR-fastpath quirk behind edit 6).
pub fn filter_cpuid(supported: &CpuId) -> Result<CpuId, CpuidError> {
    let mut entries: Vec<kvm_cpuid_entry2> = supported.as_slice().to_vec();

    // (1) Drop the entire KVM/hypervisor paravirt range -> no kvmclock.
    entries.retain(|e| !(HYPERVISOR_LEAF_LOW..=HYPERVISOR_LEAF_HIGH).contains(&e.function));

    let mut has_ext_power = false;
    for e in entries.iter_mut() {
        match e.function {
            LEAF_FEATURE_INFO => {
                // (1b) hide the hypervisor and (2) MONITOR/MWAIT bits.
                e.ecx &= !ECX_HYPERVISOR;
                e.ecx &= !ECX_MONITOR;
                // (5) Mask x2APIC (see module docs above for why).
                e.ecx &= !ECX_X2APIC;
                // (6) Clear the LAPIC TSC-deadline timer bit (see module docs
                //     above for why).
                e.ecx &= !ECX_TSC_DEADLINE;
            }
            LEAF_EXT_POWER_MGMT => {
                // (3) advertise Invariant TSC.
                e.edx |= EDX_INVARIANT_TSC;
                has_ext_power = true;
            }
            _ => {}
        }
    }

    // (4) Pass the host frequency leaves 0x15 and 0x16 through verbatim, so the
    // guest can derive tsc_khz directly now that the in-kernel PIT is gone.
    // Overwrite any KVM-provided (often zeroed) entry, or add one if absent.
    for &leaf in &[LEAF_TSC_FREQ, LEAF_CPU_FREQ] {
        let host = host_cpuid(leaf);
        if let Some(e) = entries
            .iter_mut()
            .find(|e| e.function == leaf && e.index == 0)
        {
            e.flags = 0;
            e.eax = host.eax;
            e.ebx = host.ebx;
            e.ecx = host.ecx;
            e.edx = host.edx;
        } else {
            entries.push(kvm_cpuid_entry2 {
                function: leaf,
                index: 0,
                flags: 0,
                eax: host.eax,
                ebx: host.ebx,
                ecx: host.ecx,
                edx: host.edx,
                padding: [0; 3],
            });
        }
    }

    // If the host somehow omitted leaf 0x8000_0007, synthesize it so Invariant
    // TSC is still advertised. (This host reports it; kept for portability.)
    if !has_ext_power {
        entries.push(kvm_cpuid_entry2 {
            function: LEAF_EXT_POWER_MGMT,
            index: 0,
            flags: 0,
            eax: 0,
            ebx: 0,
            ecx: 0,
            edx: EDX_INVARIANT_TSC,
            padding: [0; 3],
        });
    }

    CpuId::from_entries(&entries).map_err(|e| CpuidError(format!("{e:?}")))
}

/// Read a CPUID leaf (subleaf 0) off the host CPU. `__cpuid_count` is a safe
/// intrinsic on the x86_64 baseline (CPUID is always available); it just reads
/// the four result registers.
fn host_cpuid(leaf: u32) -> core::arch::x86_64::CpuidResult {
    core::arch::x86_64::__cpuid_count(leaf, 0)
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
pub(crate) fn dump_cpuid(kvm: &Kvm) -> Result<(), Box<dyn std::error::Error>> {
    let supported = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    let filtered = filter_cpuid(&supported)?;
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
