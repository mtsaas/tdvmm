//! vCPU register / segment / page-table / LAPIC setup for the Linux 64-bit
//! boot protocol. Ported (Linux-boot path only) from Firecracker's
//! `arch/x86_64/{regs,gdt,interrupts}.rs`.

use kvm_bindings::{kvm_fpu, kvm_regs, kvm_segment, kvm_sregs};
use kvm_ioctls::VcpuFd;
use vm_memory::{Bytes, GuestAddress};

use crate::arch;
use crate::memory::GuestMemoryMmap;

#[derive(Debug)]
pub struct RegsError(String);
impl std::fmt::Display for RegsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "register setup failed: {}", self.0)
    }
}
impl std::error::Error for RegsError {}

fn e<E: std::fmt::Display>(ctx: &str) -> impl Fn(E) -> RegsError + '_ {
    move |err| RegsError(format!("{ctx}: {err}"))
}

// ---- GDT helpers (from gdt.rs) --------------------------------------------

fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    ((u64::from(base) & 0xff00_0000) << (56 - 24))
        | ((u64::from(flags) & 0x0000_f0ff) << 40)
        | ((u64::from(limit) & 0x000f_0000) << (48 - 16))
        | ((u64::from(base) & 0x00ff_ffff) << 16)
        | (u64::from(limit) & 0x0000_ffff)
}

fn get_base(entry: u64) -> u64 {
    ((entry & 0xFF00_0000_0000_0000) >> 32)
        | ((entry & 0x0000_00FF_0000_0000) >> 16)
        | ((entry & 0x0000_0000_FFFF_0000) >> 16)
}

fn get_limit(entry: u64) -> u32 {
    let limit =
        (((entry & 0x000F_0000_0000_0000) >> 32) | (entry & 0x0000_0000_0000_FFFF)) as u32;
    match (entry & 0x0080_0000_0000_0000) >> 55 {
        0 => limit,
        _ => (limit << 12) | 0xFFF,
    }
}

fn kvm_segment_from_gdt(entry: u64, table_index: u8) -> kvm_segment {
    let present = ((entry & 0x0000_8000_0000_0000) >> 47) as u8;
    kvm_segment {
        base: get_base(entry),
        limit: get_limit(entry),
        selector: u16::from(table_index) * 8,
        type_: ((entry & 0x0000_0F00_0000_0000) >> 40) as u8,
        present,
        dpl: ((entry & 0x0000_6000_0000_0000) >> 45) as u8,
        db: ((entry & 0x0040_0000_0000_0000) >> 54) as u8,
        s: ((entry & 0x0000_1000_0000_0000) >> 44) as u8,
        l: ((entry & 0x0020_0000_0000_0000) >> 53) as u8,
        g: ((entry & 0x0080_0000_0000_0000) >> 55) as u8,
        avl: ((entry & 0x0010_0000_0000_0000) >> 52) as u8,
        padding: 0,
        unusable: if present == 0 { 1 } else { 0 },
    }
}

// ---- FPU ------------------------------------------------------------------

pub fn setup_fpu(vcpu: &VcpuFd) -> Result<(), RegsError> {
    let fpu = kvm_fpu {
        fcw: 0x37f,
        mxcsr: 0x1f80,
        ..Default::default()
    };
    vcpu.set_fpu(&fpu).map_err(e("set_fpu"))
}

// ---- General-purpose registers --------------------------------------------

pub fn setup_regs(vcpu: &VcpuFd, boot_ip: u64) -> Result<(), RegsError> {
    let regs = kvm_regs {
        rflags: 0x0000_0000_0000_0002,
        rip: boot_ip,
        rsp: arch::BOOT_STACK_POINTER,
        rbp: arch::BOOT_STACK_POINTER,
        // Linux ABI: %rsi must point at the zero page (boot_params).
        rsi: arch::ZERO_PAGE_START,
        ..Default::default()
    };
    vcpu.set_regs(&regs).map_err(e("set_regs"))
}

// ---- Special registers, segments, and page tables -------------------------

const BOOT_GDT_MAX: usize = 4;
const EFER_LMA: u64 = 0x400;
const EFER_LME: u64 = 0x100;
const X86_CR0_PE: u64 = 0x1;
const X86_CR0_PG: u64 = 0x8000_0000;
const X86_CR4_PAE: u64 = 0x20;

fn write_gdt_table(table: &[u64], mem: &GuestMemoryMmap) -> Result<(), RegsError> {
    for (index, entry) in table.iter().enumerate() {
        let addr = GuestAddress(arch::BOOT_GDT_OFFSET + (index * 8) as u64);
        mem.write_obj(*entry, addr).map_err(e("write GDT"))?;
    }
    Ok(())
}

pub fn setup_sregs(mem: &GuestMemoryMmap, vcpu: &VcpuFd) -> Result<(), RegsError> {
    let mut sregs: kvm_sregs = vcpu.get_sregs().map_err(e("get_sregs"))?;

    // GDT entries per the Linux 64-bit boot protocol.
    let gdt_table: [u64; BOOT_GDT_MAX] = [
        gdt_entry(0, 0, 0),            // NULL
        gdt_entry(0xa09b, 0, 0xfffff), // CODE (64-bit, long mode)
        gdt_entry(0xc093, 0, 0xfffff), // DATA
        gdt_entry(0x808b, 0, 0xfffff), // TSS
    ];
    let code_seg = kvm_segment_from_gdt(gdt_table[1], 1);
    let data_seg = kvm_segment_from_gdt(gdt_table[2], 2);
    let tss_seg = kvm_segment_from_gdt(gdt_table[3], 3);

    write_gdt_table(&gdt_table, mem)?;
    sregs.gdt.base = arch::BOOT_GDT_OFFSET;
    sregs.gdt.limit = (std::mem::size_of_val(&gdt_table) - 1) as u16;

    // Empty IDT.
    mem.write_obj(0u64, GuestAddress(arch::BOOT_IDT_OFFSET))
        .map_err(e("write IDT"))?;
    sregs.idt.base = arch::BOOT_IDT_OFFSET;
    sregs.idt.limit = (std::mem::size_of::<u64>() - 1) as u16;

    sregs.cs = code_seg;
    sregs.ds = data_seg;
    sregs.es = data_seg;
    sregs.fs = data_seg;
    sregs.gs = data_seg;
    sregs.ss = data_seg;
    sregs.tr = tss_seg;

    // Enter 64-bit long mode.
    sregs.cr0 |= X86_CR0_PE;
    sregs.efer |= EFER_LME | EFER_LMA;

    setup_page_tables(mem, &mut sregs)?;

    vcpu.set_sregs(&sregs).map_err(e("set_sregs"))
}

fn setup_page_tables(mem: &GuestMemoryMmap, sregs: &mut kvm_sregs) -> Result<(), RegsError> {
    let pml4 = GuestAddress(arch::PML4_START);
    let pdpte = GuestAddress(arch::PDPTE_START);

    // PML4[0] -> PDPTE, PDPTE[0] -> PDE, both present+writable (0x03).
    mem.write_obj(arch::PDPTE_START | 0x03, pml4)
        .map_err(e("write PML4"))?;
    mem.write_obj(arch::PDE_START | 0x03, pdpte)
        .map_err(e("write PDPTE"))?;
    // 512 * 2 MiB pages identity-mapping [0, 1 GiB). 0x83 = present|rw|PS.
    for i in 0..512u64 {
        mem.write_obj((i << 21) | 0x83, GuestAddress(arch::PDE_START + i * 8))
            .map_err(e("write PDE"))?;
    }

    sregs.cr3 = arch::PML4_START;
    sregs.cr4 |= X86_CR4_PAE;
    sregs.cr0 |= X86_CR0_PG;
    Ok(())
}

// The userspace LAPIC holds LINT0/1 as register storage; nothing here
// programs an in-kernel local APIC.
