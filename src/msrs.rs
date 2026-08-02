//! Boot-time MSR values.
//!
//! KVM resets MSRs to architectural defaults on vCPU creation; this seeds the
//! handful the Linux 64-bit boot path expects to find sane. Values and set are
//! cribbed from Firecracker's `create_boot_msr_entries`. MSR indices are the
//! well-known fixed architectural values.

use kvm_bindings::{kvm_msr_entry, Msrs};

const MSR_IA32_SYSENTER_CS: u32 = 0x0000_0174;
const MSR_IA32_SYSENTER_ESP: u32 = 0x0000_0175;
const MSR_IA32_SYSENTER_EIP: u32 = 0x0000_0176;
const MSR_IA32_TSC: u32 = 0x0000_0010;
const MSR_IA32_MISC_ENABLE: u32 = 0x0000_01a0;
const MSR_MTRR_DEF_TYPE: u32 = 0x0000_02ff;
const MSR_STAR: u32 = 0xc000_0081;
const MSR_LSTAR: u32 = 0xc000_0082;
const MSR_CSTAR: u32 = 0xc000_0083;
const MSR_SYSCALL_MASK: u32 = 0xc000_0084;
const MSR_KERNEL_GS_BASE: u32 = 0xc000_0102;

// IA32_MISC_ENABLE bit 0: fast-string (REP MOVS/STOS) enable.
const MSR_IA32_MISC_ENABLE_FAST_STRING: u64 = 0x1;
// MTRRdefType: enable MTRRs (bit 11) with default memory type write-back (6).
const MTRR_ENABLE_WRITE_BACK: u64 = (1 << 11) | 0x6;

#[derive(Debug)]
pub struct MsrError(String);
impl std::fmt::Display for MsrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to build boot MSRs: {}", self.0)
    }
}
impl std::error::Error for MsrError {}

/// The MSR entries to program into the boot vCPU before first run.
pub fn boot_msrs() -> Result<Msrs, MsrError> {
    let z = |index: u32| kvm_msr_entry {
        index,
        data: 0,
        ..Default::default()
    };
    let v = |index: u32, data: u64| kvm_msr_entry {
        index,
        data,
        ..Default::default()
    };

    let entries = [
        z(MSR_IA32_SYSENTER_CS),
        z(MSR_IA32_SYSENTER_ESP),
        z(MSR_IA32_SYSENTER_EIP),
        z(MSR_STAR),
        z(MSR_CSTAR),
        z(MSR_KERNEL_GS_BASE),
        z(MSR_SYSCALL_MASK),
        z(MSR_LSTAR),
        z(MSR_IA32_TSC),
        v(MSR_IA32_MISC_ENABLE, MSR_IA32_MISC_ENABLE_FAST_STRING),
        v(MSR_MTRR_DEF_TYPE, MTRR_ENABLE_WRITE_BACK),
    ];

    Msrs::from_entries(&entries).map_err(|e| MsrError(format!("{e:?}")))
}
