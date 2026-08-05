//! Intel MultiProcessor (MP) table construction — CPU discovery without ACPI.
//!
//! Ported from Firecracker's `arch/x86_64/mptable.rs`, but written at a fixed
//! base (`SYSTEM_MEM_START`, the EBDA) instead of using a resource allocator.
//! For a single vCPU the boot processor comes up regardless, but we still lay
//! down a spec-correct table (the guest scans the EBDA for the `_MP_` pointer).

use vm_memory::{Bytes, GuestAddress};

use crate::arch::SYSTEM_MEM_START;
use crate::memory::GuestMemoryMmap;

// x86_64 legacy IRQs 0..=23 (Firecracker's GSI_LEGACY_END).
const GSI_LEGACY_END: u8 = 23;

// MP entry type tags (linux/asm/mpspec_def.h).
const MP_PROCESSOR: u8 = 0;
const MP_BUS: u8 = 1;
const MP_IOAPIC: u8 = 2;
const MP_INTSRC: u8 = 3;
const MP_LINTSRC: u8 = 4;

const CPU_ENABLED: u8 = 1;
const CPU_BOOTPROCESSOR: u8 = 2;
const MPC_APIC_USABLE: u8 = 1;
const MP_IRQ_SOURCE_INT: u8 = 0;
const MP_IRQ_SOURCE_NMI: u8 = 1;
const MP_IRQ_SOURCE_EXTINT: u8 = 3;

const APIC_VERSION: u8 = 0x14;
const CPU_STEPPING: u32 = 0x600;
const CPU_FEATURE_APIC: u32 = 0x200;
const CPU_FEATURE_FPU: u32 = 0x001;
const IO_APIC_DEFAULT_PHYS_BASE: u32 = 0xfec0_0000;
const APIC_DEFAULT_PHYS_BASE: u32 = 0xfee0_0000;

#[derive(Debug)]
pub struct MptableError(String);
impl std::fmt::Display for MptableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MPTable setup failed: {}", self.0)
    }
}
impl std::error::Error for MptableError {}

// ---- On-guest-memory structures (Intel MP Spec 1.4) -----------------------
// All fields are naturally aligned, so #[repr(C)] matches the C ABI exactly.

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MpfIntel {
    signature: [u8; 4],
    physptr: u32,
    length: u8,
    specification: u8,
    checksum: u8,
    feature1: u8,
    feature2: u8,
    feature3: u8,
    feature4: u8,
    feature5: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MpcTable {
    signature: [u8; 4],
    length: u16,
    spec: u8,
    checksum: u8,
    oem: [u8; 8],
    productid: [u8; 12],
    oemptr: u32,
    oemsize: u16,
    oemcount: u16,
    lapic: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MpcCpu {
    type_: u8,
    apicid: u8,
    apicver: u8,
    cpuflag: u8,
    cpufeature: u32,
    featureflag: u32,
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MpcBus {
    type_: u8,
    busid: u8,
    bustype: [u8; 6],
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MpcIoapic {
    type_: u8,
    apicid: u8,
    apicver: u8,
    flags: u8,
    apicaddr: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MpcIntsrc {
    type_: u8,
    irqtype: u8,
    irqflag: u16,
    srcbus: u8,
    srcbusirq: u8,
    dstapic: u8,
    dstirq: u8,
}

#[repr(C)]
#[derive(Copy, Clone, Default)]
struct MpcLintsrc {
    type_: u8,
    irqtype: u8,
    irqflag: u16,
    srcbusid: u8,
    srcbusirq: u8,
    destapic: u8,
    destapiclint: u8,
}

// SAFETY: every field is a plain integer or byte array; the structs are POD
// with no padding, so any byte pattern is a valid value.
unsafe impl vm_memory::ByteValued for MpfIntel {}
unsafe impl vm_memory::ByteValued for MpcTable {}
unsafe impl vm_memory::ByteValued for MpcCpu {}
unsafe impl vm_memory::ByteValued for MpcBus {}
unsafe impl vm_memory::ByteValued for MpcIoapic {}
unsafe impl vm_memory::ByteValued for MpcIntsrc {}
unsafe impl vm_memory::ByteValued for MpcLintsrc {}

fn checksum<T: vm_memory::ByteValued>(v: &T) -> u8 {
    v.as_slice().iter().fold(0u8, |a, &b| a.wrapping_add(b))
}

const fn size<T>() -> u64 {
    std::mem::size_of::<T>() as u64
}

fn mp_size(num_cpus: u8) -> u64 {
    size::<MpfIntel>()
        + size::<MpcTable>()
        + size::<MpcCpu>() * u64::from(num_cpus)
        + size::<MpcIoapic>()
        + size::<MpcBus>()
        + size::<MpcIntsrc>() * (u64::from(GSI_LEGACY_END) + 1)
        + size::<MpcLintsrc>() * 2
}

/// Map an ISA IRQ to its IO-APIC redirection-table pin, per the conventional PC
/// wiring the MP table advertises: ISA IRQ0 (the i8254 timer) -> pin 2, ISA IRQ2
/// (the 8259 cascade, never a real source in APIC mode) parked on pin 0, and
/// every other IRQ identity-mapped (so serial IRQ4 -> pin 4). This is the single
/// source of truth for the IRQ<->pin relationship; the Step-3b userspace IOAPIC
/// reuses it so its RTE indexing matches what this table told the guest.
pub fn isa_irq_to_ioapic_pin(irq: u8) -> u8 {
    match irq {
        0 => 2,
        2 => 0,
        other => other,
    }
}

/// Write the MP table for `num_cpus` vCPUs at the EBDA.
pub fn setup_mptable(mem: &GuestMemoryMmap, num_cpus: u8) -> Result<(), MptableError> {
    let err =
        |ctx: &'static str| move |e: vm_memory::GuestMemoryError| MptableError(format!("{ctx}: {e}"));

    let total = mp_size(num_cpus);
    let mut base = GuestAddress(SYSTEM_MEM_START);

    // Zero the whole region first.
    mem.write_slice(&vec![0u8; total as usize], base)
        .map_err(err("clear"))?;

    let ioapicid: u8 = num_cpus + 1;
    let mut mp_checksum: u8 = 0;

    // MP floating pointer.
    {
        let mut mpf = MpfIntel {
            signature: *b"_MP_",
            physptr: (base.0 + size::<MpfIntel>()) as u32,
            length: 1,
            specification: 4,
            ..Default::default()
        };
        // Whole-structure checksum must sum to zero.
        mpf.checksum = (!checksum(&mpf).wrapping_sub(mpf.checksum)).wrapping_add(1);
        mem.write_obj(mpf, base).map_err(err("mpf"))?;
        base = GuestAddress(base.0 + size::<MpfIntel>());
    }

    // Reserve space for the configuration table header; fill it in last.
    let table_base = base;
    base = GuestAddress(base.0 + size::<MpcTable>());
    let mut entries: u16 = 0;

    // One CPU entry per vCPU.
    for cpu_id in 0..num_cpus {
        let cpu = MpcCpu {
            type_: MP_PROCESSOR,
            apicid: cpu_id,
            apicver: APIC_VERSION,
            cpuflag: CPU_ENABLED | if cpu_id == 0 { CPU_BOOTPROCESSOR } else { 0 },
            cpufeature: CPU_STEPPING,
            featureflag: CPU_FEATURE_APIC | CPU_FEATURE_FPU,
            ..Default::default()
        };
        mem.write_obj(cpu, base).map_err(err("cpu"))?;
        mp_checksum = mp_checksum.wrapping_add(checksum(&cpu));
        base = GuestAddress(base.0 + size::<MpcCpu>());
        entries += 1;
    }

    // ISA bus.
    {
        let bus = MpcBus {
            type_: MP_BUS,
            busid: 0,
            bustype: *b"ISA   ",
        };
        mem.write_obj(bus, base).map_err(err("bus"))?;
        mp_checksum = mp_checksum.wrapping_add(checksum(&bus));
        base = GuestAddress(base.0 + size::<MpcBus>());
        entries += 1;
    }

    // IOAPIC.
    {
        let ioapic = MpcIoapic {
            type_: MP_IOAPIC,
            apicid: ioapicid,
            apicver: APIC_VERSION,
            flags: MPC_APIC_USABLE,
            apicaddr: IO_APIC_DEFAULT_PHYS_BASE,
        };
        mem.write_obj(ioapic, base).map_err(err("ioapic"))?;
        mp_checksum = mp_checksum.wrapping_add(checksum(&ioapic));
        base = GuestAddress(base.0 + size::<MpcIoapic>());
        entries += 1;
    }

    // One interrupt-source entry per legacy IRQ, ISA-bus source -> IO-APIC pin.
    //
    // The pin assignment is NOT the identity map. The i8254 timer on ISA IRQ0 is
    // conventionally wired to IO-APIC **pin 2** on a PC (pin 0 carries the 8259
    // ExtINT / through-local-APIC path), so an override moves IRQ0 -> pin 2 and
    // parks the (cascade-only, never-fired) ISA IRQ2 on the vacated pin 0. This
    // is exactly what SeaBIOS/QEMU emit and what the guest's APIC code and KVM's
    // GSI routing expect. Serial IRQ4 stays on pin 4 (identity). See
    // `isa_irq_to_ioapic_pin`.
    for i in 0..=GSI_LEGACY_END {
        let intsrc = MpcIntsrc {
            type_: MP_INTSRC,
            irqtype: MP_IRQ_SOURCE_INT,
            irqflag: 0, // default polarity/trigger
            srcbus: 0,
            srcbusirq: i,
            dstapic: ioapicid,
            dstirq: isa_irq_to_ioapic_pin(i),
        };
        mem.write_obj(intsrc, base).map_err(err("intsrc"))?;
        mp_checksum = mp_checksum.wrapping_add(checksum(&intsrc));
        base = GuestAddress(base.0 + size::<MpcIntsrc>());
        entries += 1;
    }

    // Local interrupt sources: ExtINT on LINT0, NMI on LINT1.
    for (irqtype, destapic, destlint) in [
        (MP_IRQ_SOURCE_EXTINT, 0u8, 0u8),
        (MP_IRQ_SOURCE_NMI, 0xffu8, 1u8),
    ] {
        let lintsrc = MpcLintsrc {
            type_: MP_LINTSRC,
            irqtype,
            irqflag: 0,
            srcbusid: 0,
            srcbusirq: 0,
            destapic,
            destapiclint: destlint,
        };
        mem.write_obj(lintsrc, base).map_err(err("lintsrc"))?;
        mp_checksum = mp_checksum.wrapping_add(checksum(&lintsrc));
        base = GuestAddress(base.0 + size::<MpcLintsrc>());
        entries += 1;
    }

    // Now fill in the configuration table header.
    let table_len = (base.0 - table_base.0) as u16;
    let mut table = MpcTable {
        signature: *b"PCMP",
        length: table_len,
        spec: 4,
        oem: *b"FC      ",
        productid: *b"000000000000",
        oemcount: entries,
        lapic: APIC_DEFAULT_PHYS_BASE,
        ..Default::default()
    };
    mp_checksum = mp_checksum.wrapping_add(checksum(&table));
    table.checksum = (!mp_checksum).wrapping_add(1);
    mem.write_obj(table, table_base).map_err(err("table"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn irq0_overrides_to_pin2_irq2_parks_on_pin0() {
        // The timer override the whole Step-3b MP-table fix hinges on.
        assert_eq!(isa_irq_to_ioapic_pin(0), 2);
        assert_eq!(isa_irq_to_ioapic_pin(2), 0);
    }

    #[test]
    fn serial_irq4_stays_on_pin4_and_rest_are_identity() {
        assert_eq!(isa_irq_to_ioapic_pin(4), 4); // serial console
        for irq in [1u8, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 23] {
            assert_eq!(isa_irq_to_ioapic_pin(irq), irq, "irq {irq} should be identity");
        }
    }

    #[test]
    fn pin_mapping_is_a_bijection_over_legacy_pins() {
        // Every legacy pin is covered exactly once (no duplicate/dropped pin),
        // so no two INTSRC entries collide on one IO-APIC input.
        let mut seen = [false; (GSI_LEGACY_END as usize) + 1];
        for irq in 0..=GSI_LEGACY_END {
            let pin = isa_irq_to_ioapic_pin(irq) as usize;
            assert!(!seen[pin], "pin {pin} assigned twice");
            seen[pin] = true;
        }
        assert!(seen.iter().all(|&s| s), "some pin left unassigned");
    }
}
