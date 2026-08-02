//! Masked 8259A PIC stub (ports 0x20/0x21 master, 0xA0/0xA1 slave).
//!
//! In symmetric-I/O mode the guest still *initializes and then masks* both
//! 8259s (Linux `init_8259A()`), even though it delivers all interrupts through
//! the IOAPIC/LAPIC. This stub accepts that ICW1..ICW4 init sequence and the
//! OCW1 mask writes, remembers the interrupt-mask register so the guest can read
//! it back, and **delivers nothing** — there is no PIC interrupt path in this
//! VMM. (The ELCR ports 0x4D0/0x4D1 are register storage served by
//! [`crate::pit::PitStub`], which already owned them since Step 3a.)

const PIC1_CMD: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_CMD: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;

/// One 8259's minimal init state machine.
#[derive(Clone, Copy)]
struct Pic8259 {
    /// Interrupt mask register (OCW1). Reset masks everything.
    imr: u8,
    /// How many ICW bytes are still expected on the data port (0 = idle).
    icw_step: u8,
    /// ICW1 requested ICW4 (bit0), so the init expects 4 words not 3.
    expect_icw4: bool,
}

impl Pic8259 {
    fn new() -> Self {
        Self {
            imr: 0xff,
            icw_step: 0,
            expect_icw4: false,
        }
    }

    fn write_cmd(&mut self, val: u8) {
        // ICW1 is any command byte with bit4 set; it starts the init sequence.
        if val & 0x10 != 0 {
            self.expect_icw4 = val & 0x01 != 0;
            self.icw_step = 1; // next data write is ICW2
        }
        // OCW2 (EOI, bit4 clear, bit3 clear) and OCW3 (bit3 set): no-ops here —
        // nothing is ever in service because we deliver nothing.
    }

    fn write_data(&mut self, val: u8) {
        match self.icw_step {
            1 => self.icw_step = 2,                          // ICW2 (vector base)
            2 => self.icw_step = 3,                          // ICW3 (cascade)
            3 => self.icw_step = if self.expect_icw4 { 4 } else { 0 }, // ICW3->ICW4?
            4 => self.icw_step = 0,                          // ICW4, init done
            _ => self.imr = val,                            // OCW1: set the mask
        }
    }

    fn read_data(&self) -> u8 {
        self.imr
    }
}

pub struct PicStub {
    master: Pic8259,
    slave: Pic8259,
}

impl PicStub {
    pub fn new() -> Self {
        Self {
            master: Pic8259::new(),
            slave: Pic8259::new(),
        }
    }

    pub fn handles(port: u16) -> bool {
        matches!(port, PIC1_CMD | PIC1_DATA | PIC2_CMD | PIC2_DATA)
    }

    pub fn write(&mut self, port: u16, val: u8) {
        match port {
            PIC1_CMD => self.master.write_cmd(val),
            PIC1_DATA => self.master.write_data(val),
            PIC2_CMD => self.slave.write_cmd(val),
            PIC2_DATA => self.slave.write_data(val),
            _ => {}
        }
    }

    pub fn read(&self, port: u16) -> u8 {
        match port {
            PIC1_DATA => self.master.read_data(),
            PIC2_DATA => self.slave.read_data(),
            // Command-port reads (IRR/ISR poll): nothing is ever pending.
            PIC1_CMD | PIC2_CMD => 0,
            _ => 0xff,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_sequence_then_mask_readback() {
        let mut pic = PicStub::new();
        // Linux init_8259A on the master: ICW1(0x11), ICW2(0x20), ICW3(0x04),
        // ICW4(0x01), then OCW1 mask 0xff.
        pic.write(PIC1_CMD, 0x11);
        pic.write(PIC1_DATA, 0x20);
        pic.write(PIC1_DATA, 0x04);
        pic.write(PIC1_DATA, 0x01);
        pic.write(PIC1_DATA, 0xff);
        assert_eq!(pic.read(PIC1_DATA), 0xff);
        // A later partial unmask is remembered (still delivers nothing).
        pic.write(PIC1_DATA, 0xfb);
        assert_eq!(pic.read(PIC1_DATA), 0xfb);
    }

    #[test]
    fn slave_is_independent() {
        let mut pic = PicStub::new();
        pic.write(PIC2_CMD, 0x11);
        pic.write(PIC2_DATA, 0x28);
        pic.write(PIC2_DATA, 0x02);
        pic.write(PIC2_DATA, 0x01);
        pic.write(PIC2_DATA, 0xff);
        assert_eq!(pic.read(PIC2_DATA), 0xff);
        assert_eq!(pic.read(PIC1_DATA), 0xff); // master untouched, reset mask
    }

    #[test]
    fn command_port_reads_zero() {
        let pic = PicStub::new();
        assert_eq!(pic.read(PIC1_CMD), 0);
        assert_eq!(pic.read(PIC2_CMD), 0);
    }

    #[test]
    fn handles_only_pic_ports() {
        for p in [0x20u16, 0x21, 0xa0, 0xa1] {
            assert!(PicStub::handles(p));
        }
        for p in [0x22u16, 0x40, 0x4d0, 0x3f8] {
            assert!(!PicStub::handles(p));
        }
    }
}
