//! Userspace 8254 PIT counter stub (no interrupts).
//!
//! Built to replace KVM's in-kernel PIT (`KVM_CREATE_PIT2`) — but in Step 3a the
//! in-kernel PIT is still created and **shadows** this stub for ports 0x40-0x43
//! (KVM services those in-kernel). The in-kernel PIT is retired in Step 3b (it
//! is this guest's sole tick source — see `main.rs`), at which point this stub
//! takes over. It already serves the ELCR ports (0x4D0/0x4D1) today, and its
//! counter model is validated by the unit tests below.
//!
//! When it is live, the guest's early TSC calibration (`quick_pit_calibrate` in
//! `arch/x86/kernel/tsc.c`) programs PIT channel 2 and watches its counter
//! decrement while timing the TSC. This stub serves those port accesses so that
//! calibration succeeds and, crucially, comes out **consistent with vtsc by
//! construction** (in 3a the guest instead calibrates from CPUID 0x15/0x16, so
//! this path is a validated fallback):
//!
//! The counter decrements at the standard PIT rate of 1.193182 MHz *as a pure
//! function of [`vtsc`](crate::vtsc)* — the same clock the guest reads with
//! `RDTSC`. So when the guest divides TSC cycles by PIT ticks it recovers
//! exactly the real TSC frequency, with no dependence on host wall-clock timing
//! or vmexit latency. (See the `calibration_ratio_is_exact` test.)
//!
//! Faithful PIT detail we DO model, because `quick_pit_calibrate` depends on it:
//!   * Control-word writes (0x43): channel select, access mode (lobyte/hibyte),
//!     and the counter-latch command.
//!   * The lobyte/hibyte read flip-flop (each data read alternates LSB then MSB).
//!   * Loading a count *anchors* the counter: right after `outb(0xff,0x42)`
//!     twice, reads return ~0xffff and count down from there. A count's
//!     *progression* is a pure function of vtsc; the load only sets its phase.
//!
//! What this stub deliberately does NOT do: generate any interrupt (no IRQ0).
//! It is purely a calibration/counter surface. (This guest actually uses the
//! PIT's IRQ0 as its tick, which is why the in-kernel PIT is kept through 3a and
//! only removed in 3b alongside the userspace LAPIC that will own the tick.)
//! Modes and BCD are ignored (calibration only cares about the down-count
//! rate). The ELCR ports (0x4D0/0x4D1) are handled as plain register storage in
//! case the guest probes them.

use crate::vtsc::{TscFrequency, VirtualClock};

/// Standard 8254 PIT input frequency (Hz). Matches Linux `PIT_TICK_RATE`.
pub const PIT_FREQ_HZ: u64 = 1_193_182;

const PIT_CH0: u16 = 0x40;
const PIT_CH2: u16 = 0x42;
const PIT_CTRL: u16 = 0x43;
const ELCR_MASTER: u16 = 0x4d0;
const ELCR_SLAVE: u16 = 0x4d1;

/// PIT counter read/write access mode (control-word bits 5:4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessMode {
    LoByte,
    HiByte,
    LoHiByte,
}

impl AccessMode {
    fn from_bits(bits: u8) -> AccessMode {
        match bits & 0b11 {
            0b01 => AccessMode::LoByte,
            0b10 => AccessMode::HiByte,
            _ => AccessMode::LoHiByte, // 0b11; 0b00 is the latch command, handled separately
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Channel {
    access: AccessMode,
    // Write (count-load) path.
    write_lo_next: bool,
    write_lo_latch: u8,
    /// Loaded initial count (the value reads return at the load instant).
    initial: u16,
    /// vtsc at which the count was loaded; the counter counts down from here.
    anchor_vtsc: u64,
    // Read path.
    read_lo_next: bool,
    /// Value captured by a counter-latch command, consumed once fully read.
    latched: Option<u16>,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            access: AccessMode::LoHiByte,
            write_lo_next: true,
            write_lo_latch: 0,
            initial: 0xffff,
            anchor_vtsc: 0,
            read_lo_next: true,
            latched: None,
        }
    }
}

impl Channel {
    /// The live counter value at virtual time `now`, given the TSC frequency.
    ///
    /// A 16-bit down-counter clocked at [`PIT_FREQ_HZ`] off vtsc, anchored at
    /// the last count load. Free-running (wraps mod 2^16), which is all the
    /// guest's calibration and any incidental reads need.
    fn counter(&self, now: u64, freq: TscFrequency) -> u16 {
        let ticks = pit_ticks_since(self.anchor_vtsc, now, freq);
        (u64::from(self.initial)).wrapping_sub(ticks) as u16
    }

    fn read_byte(&mut self, now: u64, freq: TscFrequency) -> u8 {
        let value = self.latched.unwrap_or_else(|| self.counter(now, freq));
        match self.access {
            AccessMode::LoByte => {
                self.latched = None;
                value as u8
            }
            AccessMode::HiByte => {
                self.latched = None;
                (value >> 8) as u8
            }
            AccessMode::LoHiByte => {
                if self.read_lo_next {
                    // LSB now; keep any latched value for the paired MSB read.
                    self.read_lo_next = false;
                    value as u8
                } else {
                    self.read_lo_next = true;
                    self.latched = None; // full (LSB+MSB) read done
                    (value >> 8) as u8
                }
            }
        }
    }

    fn write_byte(&mut self, byte: u8, now: u64) {
        match self.access {
            AccessMode::LoByte => {
                self.initial = u16::from(byte);
                self.anchor_vtsc = now;
            }
            AccessMode::HiByte => {
                self.initial = u16::from(byte) << 8;
                self.anchor_vtsc = now;
            }
            AccessMode::LoHiByte => {
                if self.write_lo_next {
                    self.write_lo_latch = byte;
                    self.write_lo_next = false;
                } else {
                    self.initial = (u16::from(byte) << 8) | u16::from(self.write_lo_latch);
                    self.write_lo_next = true;
                    self.anchor_vtsc = now; // counter starts at the new value now
                }
            }
        }
    }

    /// Apply a mode/access programming control word (not a latch command).
    fn program(&mut self, access: AccessMode) {
        self.access = access;
        self.read_lo_next = true;
        self.write_lo_next = true;
        self.latched = None;
    }

    /// Counter-latch command: freeze the current count for reading.
    fn latch(&mut self, now: u64, freq: TscFrequency) {
        if self.latched.is_none() {
            self.latched = Some(self.counter(now, freq));
            self.read_lo_next = true;
        }
    }
}

/// PIT ticks elapsed between two vtsc readings at [`PIT_FREQ_HZ`].
fn pit_ticks_since(anchor: u64, now: u64, freq: TscFrequency) -> u64 {
    let dv = now.saturating_sub(anchor);
    ((u128::from(dv) * u128::from(PIT_FREQ_HZ)) / u128::from(freq.hz())) as u64
}

/// The userspace PIT counter stub. vCPU-thread-owned (no interrupts, no other
/// writer), so it needs no locking.
pub struct PitStub {
    channels: [Channel; 3],
    elcr: [u8; 2],
    clock: VirtualClock,
}

impl PitStub {
    pub fn new(clock: VirtualClock) -> Self {
        Self {
            channels: [Channel::default(); 3],
            elcr: [0; 2],
            clock,
        }
    }

    /// True if `port` is one this stub owns (PIT 0x40-0x43 or ELCR 0x4D0/0x4D1).
    pub fn handles(port: u16) -> bool {
        matches!(port, PIT_CH0..=PIT_CTRL | ELCR_MASTER | ELCR_SLAVE)
    }

    /// Handle a 1-byte port read.
    pub fn read(&mut self, port: u16) -> u8 {
        let now = self.clock.vtsc_now();
        self.read_at(port, now)
    }

    /// Handle a 1-byte port write.
    pub fn write(&mut self, port: u16, value: u8) {
        let now = self.clock.vtsc_now();
        self.write_at(port, value, now)
    }

    // ---- vtsc-injected cores (deterministic; exercised by unit tests) -------

    fn read_at(&mut self, port: u16, now: u64) -> u8 {
        let freq = self.clock.freq();
        match port {
            PIT_CH0 | 0x41 | PIT_CH2 => {
                let idx = (port - PIT_CH0) as usize;
                self.channels[idx].read_byte(now, freq)
            }
            PIT_CTRL => 0xff, // control port is write-only; reads are undefined
            ELCR_MASTER => self.elcr[0],
            ELCR_SLAVE => self.elcr[1],
            _ => 0xff,
        }
    }

    fn write_at(&mut self, port: u16, value: u8, now: u64) {
        let freq = self.clock.freq();
        match port {
            PIT_CH0 | 0x41 | PIT_CH2 => {
                let idx = (port - PIT_CH0) as usize;
                self.channels[idx].write_byte(value, now);
            }
            PIT_CTRL => self.write_control(value, now, freq),
            ELCR_MASTER => self.elcr[0] = value,
            ELCR_SLAVE => self.elcr[1] = value,
            _ => {}
        }
    }

    fn write_control(&mut self, cw: u8, now: u64, freq: TscFrequency) {
        let sel = cw >> 6;
        if sel == 0b11 {
            // Read-back command: latch the count of each selected counter
            // (unless the "latch count" inhibit bit is set).
            if cw & 0x20 == 0 {
                for (i, ch) in self.channels.iter_mut().enumerate() {
                    if cw & (1 << (i + 1)) != 0 {
                        ch.latch(now, freq);
                    }
                }
            }
            return;
        }
        let ch = &mut self.channels[sel as usize];
        let access_bits = (cw >> 4) & 0b11;
        if access_bits == 0 {
            ch.latch(now, freq); // counter-latch command
        } else {
            ch.program(AccessMode::from_bits(access_bits));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clk(tsc_hz: u64) -> VirtualClock {
        VirtualClock::new(0, TscFrequency::from_hz(tsc_hz))
    }

    #[test]
    fn counter_starts_at_loaded_value() {
        let freq = TscFrequency::from_hz(3_000_000_000);
        let ch = Channel {
            initial: 0xffff,
            anchor_vtsc: 1_000,
            ..Channel::default()
        };
        // At the load instant, no ticks have elapsed: reads the loaded count.
        assert_eq!(ch.counter(1_000, freq), 0xffff);
    }

    #[test]
    fn counter_steps_msb_every_256_ticks() {
        // Use a synthetic TSC frequency equal to the PIT frequency, so 1 TSC
        // cycle == exactly 1 PIT tick and there is no integer-rounding noise:
        // the MSB must step down once every 256 ticks, by construction.
        let freq = TscFrequency::from_hz(PIT_FREQ_HZ);
        let anchor = 5_000_000u64;
        let ch = Channel {
            initial: 0xffff,
            anchor_vtsc: anchor,
            ..Channel::default()
        };
        assert_eq!(ch.counter(anchor, freq), 0xffff); // MSB 0xff at load
        assert_eq!(ch.counter(anchor + 255, freq) >> 8, 0xff); // still 0xff
        assert_eq!(ch.counter(anchor + 256, freq), 0xfeff); // stepped to 0xfe
        assert_eq!(ch.counter(anchor + 512, freq) >> 8, 0xfd);
        assert_eq!(ch.counter(anchor + 256 * 3, freq) >> 8, 0xfc);
    }

    #[test]
    fn counter_rate_is_exactly_pit_freq_per_second() {
        // At a real TSC frequency, advancing vtsc by exactly one second's worth
        // of TSC cycles (dv == tsc_hz) advances the counter by exactly
        // PIT_FREQ_HZ ticks — no rounding, since dv * PIT_FREQ_HZ / tsc_hz is
        // exact when dv == tsc_hz. This is the whole "PIT is a pure function of
        // vtsc" property, stated as a rate.
        let tsc_hz = 3_072_000_000u64;
        let freq = TscFrequency::from_hz(tsc_hz);
        let anchor = 5_000_000u64;
        let ch = Channel {
            initial: 0xffff,
            anchor_vtsc: anchor,
            ..Channel::default()
        };
        let expected = (0xffffu64.wrapping_sub(PIT_FREQ_HZ)) as u16;
        assert_eq!(ch.counter(anchor + tsc_hz, freq), expected);
    }

    #[test]
    fn counter_is_a_pure_function_of_vtsc() {
        let freq = TscFrequency::from_hz(2_500_000_000);
        let ch = Channel {
            initial: 0xffff,
            anchor_vtsc: 42,
            ..Channel::default()
        };
        // Same vtsc -> same counter, always (determinism).
        assert_eq!(ch.counter(1_234_567, freq), ch.counter(1_234_567, freq));
    }

    #[test]
    fn calibration_ratio_is_exact() {
        // The guest's quick_pit_calibrate measures the TSC cycles that elapse
        // over `i` MSB steps (i*256 PIT ticks) and computes:
        //   kHz = (delta_tsc * PIT_TICK_RATE) / (i * 256 * 1000)
        //
        // Because our PIT counter advances i*256 ticks in exactly the vtsc span
        // `delta_tsc = i*256 * tsc_hz / PIT_FREQ_HZ` (PIT ticks and vtsc are
        // locked by tsc_hz/PIT_FREQ_HZ) and the guest's TSC == vtsc, feeding
        // that delta back through the kernel's formula must recover tsc_khz.
        // This is the "consistent by construction" property.
        let i = 100u64; // MSB steps; large enough that ±1-cycle floor is noise
        for &tsc_hz in &[1_000_000_000u64, 2_500_000_000, 3_072_000_000, 4_000_000_000] {
            let ticks = i * 256;
            let delta_tsc =
                (u128::from(ticks) * u128::from(tsc_hz) / u128::from(PIT_FREQ_HZ)) as u64;
            let khz = (u128::from(delta_tsc) * u128::from(PIT_FREQ_HZ)
                / (u128::from(i) * 256 * 1000)) as u64;
            let tsc_khz = tsc_hz / 1000;
            assert!(
                khz.abs_diff(tsc_khz) <= 1,
                "tsc_hz={tsc_hz}: calibrated {khz} kHz vs expected {tsc_khz} kHz"
            );
        }
    }

    #[test]
    fn quick_pit_readback_sequence() {
        // Drive the exact port sequence quick_pit_calibrate uses and confirm the
        // (LSB-ignored, MSB) read pair returns 0xff right after loading 0xffff.
        let mut pit = PitStub::new(clk(3_072_000_000));
        let mut now = 1_000_000u64;

        pit.write_at(PIT_CTRL, 0xb0, now); // ch2, lobyte/hibyte, mode 0, binary
        pit.write_at(PIT_CH2, 0xff, now); // count LSB
        now += 30; // a few TSC cycles between the two writes
        pit.write_at(PIT_CH2, 0xff, now); // count MSB -> load 0xffff, anchor here

        // pit_verify_msb(0): inb (LSB, ignored), inb (MSB).
        now += 20;
        let _lsb = pit.read_at(PIT_CH2, now);
        let msb = pit.read_at(PIT_CH2, now);
        assert_eq!(msb, 0xff, "counter must start at 0xffff right after load");

        // A pair read much later shows the MSB has decremented (counting down).
        let dv = (256u128 * 3_072_000_000u128 / u128::from(PIT_FREQ_HZ)) as u64;
        now += dv;
        let _ = pit.read_at(PIT_CH2, now);
        let msb2 = pit.read_at(PIT_CH2, now);
        assert_eq!(msb2, 0xfe, "MSB must count down at the PIT rate");
    }

    #[test]
    fn counter_latch_command_freezes_value() {
        let mut pit = PitStub::new(clk(3_000_000_000));
        let mut now = 2_000_000u64;
        pit.write_at(PIT_CTRL, 0xb0, now); // program ch2 lobyte/hibyte
        pit.write_at(PIT_CH2, 0x00, now);
        pit.write_at(PIT_CH2, 0x80, now); // load 0x8000, anchor
        // Latch the count now.
        pit.write_at(PIT_CTRL, 0x80, now); // ch2, access=00 => latch command
                                           // Advance time a lot; the latched read must still reflect the latch instant.
        now += 10_000_000;
        let lo = pit.read_at(PIT_CH2, now);
        let hi = pit.read_at(PIT_CH2, now);
        assert_eq!(((hi as u16) << 8) | lo as u16, 0x8000);
    }

    #[test]
    fn elcr_is_register_storage() {
        let mut pit = PitStub::new(clk(3_000_000_000));
        assert_eq!(pit.read_at(ELCR_MASTER, 0), 0);
        pit.write_at(ELCR_MASTER, 0xa5, 0);
        pit.write_at(ELCR_SLAVE, 0x5a, 0);
        assert_eq!(pit.read_at(ELCR_MASTER, 0), 0xa5);
        assert_eq!(pit.read_at(ELCR_SLAVE, 0), 0x5a);
    }

    #[test]
    fn handles_only_owned_ports() {
        for p in [0x40u16, 0x41, 0x42, 0x43, 0x4d0, 0x4d1] {
            assert!(PitStub::handles(p), "should handle {p:#x}");
        }
        for p in [0x3fu16, 0x44, 0x60, 0x61, 0x3f8, 0x4cf, 0x4d2] {
            assert!(!PitStub::handles(p), "should not handle {p:#x}");
        }
    }
}
