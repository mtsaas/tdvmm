//! Minimal userspace I/O APIC (82093AA), index/data window at `0xFEC0_0000`.
//!
//! Step 3b needs exactly enough of the IOAPIC for the guest to route the one
//! ISA interrupt it actually uses through it: the 16550 serial IRQ (ISA IRQ4 ->
//! pin 4, matching the MP table). The guest programs a redirection-table entry
//! per pin; when a device raises an edge on a pin, we resolve the RTE to a
//! vector and post it into the [`crate::lapic::Lapic`] (fixed delivery to our
//! single LAPIC).
//!
//! Scope: 24 redirection entries, **edge-triggered only**. A level-triggered RTE
//! is loudly logged and the interrupt dropped (`log-and-fail`); nothing in this
//! guest programs one (the ISA serial line is edge), so it never happens in
//! practice, and refusing keeps the remote-IRR/EOI machinery we don't model from
//! silently misbehaving.

pub const IOAPIC_BASE: u64 = 0xfec0_0000;
pub const IOAPIC_LEN: u64 = 0x20;

const NUM_RTES: usize = 24;

// Window registers (offsets from IOAPIC_BASE).
const IOREGSEL: u64 = 0x00;
const IOWIN: u64 = 0x10;

// Indirect register indices.
const IOAPIC_REG_ID: u32 = 0x00;
const IOAPIC_REG_VER: u32 = 0x01;
const IOAPIC_REG_ARB: u32 = 0x02;
const IOAPIC_REG_REDTBL_BASE: u32 = 0x10;

// RTE field bits (low dword).
const RTE_MASK: u64 = 1 << 16;
const RTE_TRIGGER_LEVEL: u64 = 1 << 15;

// Version 0x17 with max-redirection-entry = 23 (in bits 23:16) — this is what
// the guest logs as "IOAPIC[0]: ... version 17, ... GSI 0-23".
const IOAPIC_VERSION: u32 = 0x17 | (((NUM_RTES as u32) - 1) << 16);

pub struct Ioapic {
    /// Which indirect register IOWIN currently addresses.
    ioregsel: u32,
    id: u32,
    /// 24 redirection entries, each a 64-bit (vector, delivery, mask, ...) word.
    redtbl: [u64; NUM_RTES],
    /// APIC ID advertised to the guest (matches the MP-table IOAPIC id).
    apic_id: u32,
}

impl Ioapic {
    pub fn new(apic_id: u8) -> Self {
        Self {
            ioregsel: 0,
            id: u32::from(apic_id) << 24,
            // Power-on: every entry masked (bit 16), vector 0.
            redtbl: [RTE_MASK; NUM_RTES],
            apic_id: u32::from(apic_id),
        }
    }

    pub fn handles(addr: u64) -> bool {
        (IOAPIC_BASE..IOAPIC_BASE + IOAPIC_LEN).contains(&addr)
    }

    pub fn mmio_read(&mut self, addr: u64) -> u32 {
        match addr - IOAPIC_BASE {
            IOREGSEL => self.ioregsel,
            IOWIN => self.read_indirect(self.ioregsel),
            _ => 0,
        }
    }

    pub fn mmio_write(&mut self, addr: u64, val: u32) {
        match addr - IOAPIC_BASE {
            IOREGSEL => self.ioregsel = val & 0xff,
            IOWIN => self.write_indirect(self.ioregsel, val),
            _ => {}
        }
    }

    fn read_indirect(&self, index: u32) -> u32 {
        match index {
            IOAPIC_REG_ID => self.id,
            IOAPIC_REG_VER => IOAPIC_VERSION,
            IOAPIC_REG_ARB => self.id,
            i if (IOAPIC_REG_REDTBL_BASE..IOAPIC_REG_REDTBL_BASE + (NUM_RTES as u32) * 2)
                .contains(&i) =>
            {
                let rel = i - IOAPIC_REG_REDTBL_BASE;
                let entry = (rel / 2) as usize;
                let rte = self.redtbl[entry];
                if rel % 2 == 0 {
                    rte as u32
                } else {
                    (rte >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    fn write_indirect(&mut self, index: u32, val: u32) {
        match index {
            IOAPIC_REG_ID => self.id = val & 0x0f00_0000,
            IOAPIC_REG_VER | IOAPIC_REG_ARB => { /* read-only */ }
            i if (IOAPIC_REG_REDTBL_BASE..IOAPIC_REG_REDTBL_BASE + (NUM_RTES as u32) * 2)
                .contains(&i) =>
            {
                let rel = i - IOAPIC_REG_REDTBL_BASE;
                let entry = (rel / 2) as usize;
                let rte = &mut self.redtbl[entry];
                if rel % 2 == 0 {
                    *rte = (*rte & 0xffff_ffff_0000_0000) | u64::from(val);
                } else {
                    *rte = (*rte & 0x0000_0000_ffff_ffff) | (u64::from(val) << 32);
                }
            }
            _ => {}
        }
    }

    /// Resolve `pin` to the vector that should be posted to the LAPIC for an
    /// edge on that pin, or `None` if the entry is masked (or invalid). A
    /// level-triggered entry is loudly logged and dropped.
    pub fn edge_vector(&self, pin: usize) -> Option<u8> {
        if pin >= NUM_RTES {
            return None;
        }
        let rte = self.redtbl[pin];
        if rte & RTE_MASK != 0 {
            return None;
        }
        if rte & RTE_TRIGGER_LEVEL != 0 {
            crate::log_line(format_args!(
                "[tdvmm][ioapic] UNSUPPORTED level-triggered RTE on pin {pin} \
                 (rte={rte:#x}); dropping interrupt (edge-only model)"
            ));
            return None;
        }
        Some((rte & 0xff) as u8)
    }

    #[allow(dead_code)]
    pub fn apic_id(&self) -> u32 {
        self.apic_id
    }

    /// Compact RTE snapshot for the wedge dump ([`crate::diag`]): the unmasked
    /// redirection entries (pin -> vector / trigger). Read-only. A wedged guest
    /// waiting on an IRQ whose pin is masked here shows up as an empty list.
    pub fn diag_str(&self) -> String {
        let parts: Vec<String> = self
            .redtbl
            .iter()
            .enumerate()
            .filter(|(_, &rte)| rte & RTE_MASK == 0)
            .map(|(pin, &rte)| {
                let trig = if rte & RTE_TRIGGER_LEVEL != 0 { "level" } else { "edge" };
                format!("pin{pin}->{:#x}/{trig}", rte & 0xff)
            })
            .collect();
        format!(
            "ioapic: sel={:#x} unmasked[{}]",
            self.ioregsel,
            if parts.is_empty() { "none".into() } else { parts.join(",") }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn program_rte(io: &mut Ioapic, pin: usize, low: u32, high: u32) {
        let idx = IOAPIC_REG_REDTBL_BASE + (pin as u32) * 2;
        io.mmio_write(IOAPIC_BASE + IOREGSEL, idx);
        io.mmio_write(IOAPIC_BASE + IOWIN, low);
        io.mmio_write(IOAPIC_BASE + IOREGSEL, idx + 1);
        io.mmio_write(IOAPIC_BASE + IOWIN, high);
    }

    #[test]
    fn version_reports_24_entries() {
        let mut io = Ioapic::new(2);
        io.mmio_write(IOAPIC_BASE + IOREGSEL, IOAPIC_REG_VER);
        let v = io.mmio_read(IOAPIC_BASE + IOWIN);
        assert_eq!(v & 0xff, 0x17);
        assert_eq!((v >> 16) & 0xff, 23); // max redirection entry
    }

    #[test]
    fn masked_by_default_then_unmask_delivers_vector() {
        let mut io = Ioapic::new(2);
        assert_eq!(io.edge_vector(4), None); // reset = masked
        // Program pin 4 (serial): vector 0x31, unmasked, edge.
        program_rte(&mut io, 4, 0x31, 0);
        assert_eq!(io.edge_vector(4), Some(0x31));
    }

    #[test]
    fn masked_entry_delivers_nothing() {
        let mut io = Ioapic::new(2);
        program_rte(&mut io, 4, 0x31 | (1 << 16), 0); // mask bit set
        assert_eq!(io.edge_vector(4), None);
    }

    #[test]
    fn level_triggered_rte_is_refused() {
        let mut io = Ioapic::new(2);
        program_rte(&mut io, 5, 0x40 | (1 << 15), 0); // level trigger
        assert_eq!(io.edge_vector(5), None);
    }

    #[test]
    fn rte_readback_roundtrips_both_dwords() {
        let mut io = Ioapic::new(2);
        program_rte(&mut io, 7, 0xdead_00b1, 0x0f00_0000);
        io.mmio_write(IOAPIC_BASE + IOREGSEL, IOAPIC_REG_REDTBL_BASE + 14);
        assert_eq!(io.mmio_read(IOAPIC_BASE + IOWIN), 0xdead_00b1);
        io.mmio_write(IOAPIC_BASE + IOREGSEL, IOAPIC_REG_REDTBL_BASE + 15);
        assert_eq!(io.mmio_read(IOAPIC_BASE + IOWIN), 0x0f00_0000);
    }
}
