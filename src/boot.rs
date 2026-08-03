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

    // Placement is against the LOW region only: `ramdisk_image` in the boot
    // header is a u32, so the initramfs must stay below 4 GiB. On a split guest
    // `find_region(0)` is the 3 GiB low region; on a single-region guest it is
    // all of RAM — the same value as before the high-memory split.
    let lowmem_end = mem
        .find_region(GuestAddress(0))
        .map(|r| r.len())
        .unwrap_or(mem_size);
    if size as u64 > lowmem_end {
        return Err(BootError("initramfs larger than low guest RAM".into()));
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

/// Build the E820 memory map for `mem_size` bytes of guest RAM.
///
/// Low RAM runs from 0 up to the top of guest RAM, but never past the 32-bit
/// MMIO gap ([`arch::MMIO_MEM_START`], 3 GiB) — the reserved system-data region
/// (holding the MPTable) is punched out of it. Any RAM beyond 3 GiB becomes a
/// SEPARATE high-RAM entry based at exactly 4 GiB ([`arch::FIRST_ADDR_PAST_32BITS`]);
/// the `[3 GiB, 4 GiB)` gap is simply absent from the map. A guest whose RAM
/// fits below the gap gets EXACTLY the three entries it did before the split
/// (no high entry, no zero-length entry).
fn build_e820_map(params: &mut boot_params, mem_size: u64) -> Result<(), BootError> {
    let low_ram_end = mem_size.min(arch::MMIO_MEM_START);
    add_e820_entry(params, 0, arch::SYSTEM_MEM_START, E820_RAM)?;
    add_e820_entry(
        params,
        arch::SYSTEM_MEM_START,
        arch::SYSTEM_MEM_SIZE,
        E820_RESERVED,
    )?;
    add_e820_entry(
        params,
        arch::HIMEM_START,
        low_ram_end - arch::HIMEM_START,
        E820_RAM,
    )?;
    if mem_size > arch::MMIO_MEM_START {
        add_e820_entry(
            params,
            arch::FIRST_ADDR_PAST_32BITS,
            mem_size - arch::MMIO_MEM_START,
            E820_RAM,
        )?;
    }
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

    build_e820_map(&mut params, mem_size)?;

    LinuxBootConfigurator::write_bootparams(
        &BootParams::new(&params, GuestAddress(arch::ZERO_PAGE_START)),
        mem,
    )
    .map_err(be("writing zero page"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// (addr, size, type) for each populated E820 entry, in order.
    fn e820(mem_size: u64) -> Vec<(u64, u64, u32)> {
        let mut params = boot_params::default();
        build_e820_map(&mut params, mem_size).unwrap();
        params.e820_table[..params.e820_entries as usize]
            .iter()
            .map(|e| (e.addr, e.size, e.r#type))
            .collect()
    }

    #[test]
    fn e820_below_gap_is_unchanged() {
        // A 2 GiB guest: exactly the three entries present before the split,
        // no high entry, no zero-length entry.
        let map = e820(2 * GIB);
        assert_eq!(
            map,
            vec![
                (0, arch::SYSTEM_MEM_START, E820_RAM),
                (arch::SYSTEM_MEM_START, arch::SYSTEM_MEM_SIZE, E820_RESERVED),
                (arch::HIMEM_START, 2 * GIB - arch::HIMEM_START, E820_RAM),
            ]
        );
    }

    #[test]
    fn e820_exactly_3gib_has_no_high_entry() {
        let map = e820(3 * GIB);
        assert_eq!(map.len(), 3);
        // Low RAM ends exactly at the gap; nothing above.
        assert_eq!(map[2], (arch::HIMEM_START, arch::MMIO_MEM_START - arch::HIMEM_START, E820_RAM));
    }

    #[test]
    fn e820_above_gap_splits_low_and_high() {
        // A 6 GiB guest: low RAM clamped at 3 GiB, high RAM [4 GiB, 4+3 GiB).
        let map = e820(6 * GIB);
        assert_eq!(
            map,
            vec![
                (0, arch::SYSTEM_MEM_START, E820_RAM),
                (arch::SYSTEM_MEM_START, arch::SYSTEM_MEM_SIZE, E820_RESERVED),
                (arch::HIMEM_START, arch::MMIO_MEM_START - arch::HIMEM_START, E820_RAM),
                (arch::FIRST_ADDR_PAST_32BITS, 6 * GIB - arch::MMIO_MEM_START, E820_RAM),
            ]
        );
        // The [3 GiB, 4 GiB) gap is never described as RAM.
        for (addr, size, ty) in &map {
            if *ty == E820_RAM {
                let end = addr + size;
                assert!(
                    *addr >= arch::FIRST_ADDR_PAST_32BITS || end <= arch::MMIO_MEM_START,
                    "RAM entry [{addr:#x}, {end:#x}) overlaps the 32-bit MMIO gap"
                );
            }
        }
    }
}
