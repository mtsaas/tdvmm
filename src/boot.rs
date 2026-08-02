//! Direct Linux boot: load an ELF `vmlinux`, lay down the command line, the
//! E820 map and the zero page (`boot_params`), and the MPTable.
//!
//! Ported from Firecracker's `arch/x86_64/mod.rs::configure_64bit_boot`, minus
//! ACPI: we build **no** ACPI tables and deliberately leave `acpi_rsdp_addr`
//! unset, so the guest falls back to the MPTable / boot-CPU path.

use std::io::Cursor;

use linux_loader::configurator::linux::LinuxBootConfigurator;
use linux_loader::configurator::{BootConfigurator, BootParams};
use linux_loader::loader::bootparam::{boot_params, setup_header};
use linux_loader::loader::elf::Elf;
use linux_loader::loader::{load_cmdline, Cmdline, KernelLoader};
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryRegion};

use crate::arch;
use crate::memory::GuestMemoryMmap;
use crate::mptable;

const E820_RAM: u32 = 1;
const E820_RESERVED: u32 = 2;

// Linux 64-bit boot protocol magic values.
const KERNEL_BOOT_FLAG_MAGIC: u16 = 0xaa55;
const KERNEL_HDR_MAGIC: u32 = 0x5372_6448; // "HdrS"
const KERNEL_LOADER_OTHER: u8 = 0xff;
const KERNEL_MIN_ALIGNMENT_BYTES: u32 = 0x0100_0000;

#[derive(Debug)]
pub struct BootError(String);
impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for BootError {}
fn be<E: std::fmt::Display>(ctx: &'static str) -> impl Fn(E) -> BootError {
    move |e| BootError(format!("{ctx}: {e}"))
}

/// Configuration of a loaded initrd/initramfs image.
#[derive(Clone, Copy)]
pub struct InitrdConfig {
    pub address: u64,
    pub size: usize,
}

/// Load an uncompressed ELF `vmlinux` into guest memory, returning the entry
/// point. The kernel is parsed straight from a byte buffer (`linux-loader` reads
/// the ELF via `Read + Seek`) — the same path whether it came from a file
/// (`dvmm boot`) or from a `.dvmm` member read into memory (`dvmm run`). No temp
/// files, no extraction.
pub fn load_kernel(mem: &GuestMemoryMmap, kernel: &[u8]) -> Result<GuestAddress, BootError> {
    let mut cursor = Cursor::new(kernel);
    let result = Elf::load(
        mem,
        None,
        &mut cursor,
        Some(GuestAddress(arch::HIMEM_START)),
    )
    .map_err(be("loading vmlinux ELF"))?;
    Ok(result.kernel_load)
}

/// Load a raw initramfs image near the top of low RAM (page-aligned), straight
/// from a byte buffer into guest RAM (no temp-dir extraction).
pub fn load_initrd(
    mem: &GuestMemoryMmap,
    data: &[u8],
    mem_size: u64,
) -> Result<InitrdConfig, BootError> {
    let size = data.len();

    // Highest low-RAM region determines placement.
    let lowmem_end = mem
        .find_region(GuestAddress(0))
        .map(|r| r.len())
        .unwrap_or(mem_size);
    if size as u64 > lowmem_end {
        return Err(BootError("initramfs larger than guest RAM".into()));
    }
    let address = (lowmem_end - size as u64) & !0xfffu64; // 4 KiB aligned

    mem.write_slice(data, GuestAddress(address))
        .map_err(be("writing initramfs to guest memory"))?;
    Ok(InitrdConfig {
        address,
        size,
    })
}

fn add_e820_entry(
    params: &mut boot_params,
    addr: u64,
    size: u64,
    mem_type: u32,
) -> Result<(), BootError> {
    let idx = params.e820_entries as usize;
    if idx >= params.e820_table.len() {
        return Err(BootError("too many E820 entries".into()));
    }
    params.e820_table[idx].addr = addr;
    params.e820_table[idx].size = size;
    params.e820_table[idx].r#type = mem_type;
    params.e820_entries += 1;
    Ok(())
}

/// Write cmdline, MPTable, E820 map and the zero page into guest memory.
pub fn configure_system(
    mem: &GuestMemoryMmap,
    cmdline_str: &str,
    initrd: Option<InitrdConfig>,
    mem_size: u64,
    num_cpus: u8,
) -> Result<(), BootError> {
    // --- Kernel command line ---
    let mut cmdline = Cmdline::new(arch::CMDLINE_MAX_SIZE).map_err(be("cmdline alloc"))?;
    cmdline
        .insert_str(cmdline_str)
        .map_err(be("cmdline insert"))?;
    let cmdline_size = cmdline
        .as_cstring()
        .map_err(be("cmdline cstring"))?
        .as_bytes_with_nul()
        .len();
    load_cmdline(mem, GuestAddress(arch::CMDLINE_START), &cmdline)
        .map_err(be("loading cmdline"))?;

    // --- MPTable (CPU discovery without ACPI) ---
    mptable::setup_mptable(mem, num_cpus).map_err(be("mptable"))?;

    // --- Zero page / boot_params ---
    let mut hdr = setup_header {
        type_of_loader: KERNEL_LOADER_OTHER,
        boot_flag: KERNEL_BOOT_FLAG_MAGIC,
        header: KERNEL_HDR_MAGIC,
        kernel_alignment: KERNEL_MIN_ALIGNMENT_BYTES,
        cmd_line_ptr: arch::CMDLINE_START as u32,
        cmdline_size: cmdline_size as u32,
        ..Default::default()
    };
    if let Some(cfg) = initrd {
        hdr.ramdisk_image = cfg.address as u32;
        hdr.ramdisk_size = cfg.size as u32;
    }

    let mut params = boot_params {
        hdr,
        // NOTE: acpi_rsdp_addr intentionally left 0 — no ACPI tables exist.
        ..Default::default()
    };

    // E820: low RAM, reserved system-data region (holds the MPTable), then
    // high RAM from 1 MiB to the top of guest memory.
    add_e820_entry(&mut params, 0, arch::SYSTEM_MEM_START, E820_RAM)?;
    add_e820_entry(
        &mut params,
        arch::SYSTEM_MEM_START,
        arch::SYSTEM_MEM_SIZE,
        E820_RESERVED,
    )?;
    add_e820_entry(
        &mut params,
        arch::HIMEM_START,
        mem_size - arch::HIMEM_START,
        E820_RAM,
    )?;

    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(arch::ZERO_PAGE_START)),
        mem,
    )
    .map_err(be("writing zero page"))?;

    Ok(())
}
