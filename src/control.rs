//! The modeled control channel: a second 16550 UART (COM2 / ttyS1).
//!
//! TEST-1a adds a control channel between the VMM (host) and a small guest-side
//! `dvmm-agent`, so a developer can drive virtual time, probe guest state, and
//! assert on it. Fable locked the transport: **reuse the serial model, second
//! port**. This is that second UART.
//!
//! ## Why a UART, and why it is fast-forward-transparent
//!
//! The agent BLOCKS reading `/dev/ttyS1`. A blocked read on an idle UART parks
//! the guest process in the kernel with no timer armed — so it generates **no
//! wakes**, and an idle guest with the agent baked in fast-forwards exactly as it
//! would without it. When the VMM has a command to deliver it enqueues the bytes
//! into this UART's RX FIFO and raises IRQ3; the guest wakes, the agent reads one
//! line, executes it, writes a reply line back (guest TX, captured here), and
//! blocks again.
//!
//! ## The single-writer law for commands
//!
//! Every command is delivered by the VMM at its **scheduled vtsc** as a queue
//! event (see `TimerKind::ScenarioStep` in `main.rs`) — never an ad-hoc side
//! channel. [`ControlChannel::send_line`] only *queues* bytes; [`ControlChannel::pump`]
//! moves them into the FIFO and raises the IRQ, and both run on the vCPU thread
//! at loop boundaries, exactly like every other guest-state effect.
//!
//! Line-delimited JSON in both directions. The RX FIFO is 64 bytes, so a long
//! command is fed in chunks across loop iterations as the guest drains it — the
//! command still *starts* at its scheduled vtsc; delivery just streams as the
//! agent reads.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::arch;
use crate::ioapic::Ioapic;
use crate::lapic::Lapic;
use crate::serial::{self, ControlSerial, EventFdTrigger};

/// Cap on the captured-TX buffer; a runaway (non-newline) writer is truncated
/// rather than grown without bound. Replies are tiny, so this never trips in
/// practice.
const TX_BUF_CAP: usize = 1 << 20;

pub struct ControlChannel {
    serial: ControlSerial,
    drain: EventFdTrigger,
    /// Guest -> VMM captured bytes (line buffer of the agent's replies).
    tx: Arc<Mutex<Vec<u8>>>,
    /// VMM -> guest bytes awaiting RX-FIFO space (command backpressure).
    pending_rx: VecDeque<u8>,
}

impl ControlChannel {
    pub fn new() -> std::io::Result<Self> {
        let (serial, drain, tx) = serial::new_control_serial()?;
        Ok(Self {
            serial,
            drain,
            tx,
            pending_rx: VecDeque::new(),
        })
    }

    /// Whether `port` is one of COM2's eight I/O ports.
    pub fn handles(port: u16) -> bool {
        (arch::SERIAL2_PORT_BASE..arch::SERIAL2_PORT_BASE + 8).contains(&port)
    }

    /// Service a guest PIO write to COM2 (e.g. THR: the agent emitting a reply).
    pub fn pio_write(&mut self, port: u16, byte: u8, lapic: &mut Lapic, ioapic: &Ioapic) {
        let _ = self.serial.write((port - arch::SERIAL2_PORT_BASE) as u8, byte);
        self.after_uart_io(lapic, ioapic);
    }

    /// Service a guest PIO read from COM2 (e.g. RBR: the agent reading a command).
    pub fn pio_read(&mut self, port: u16, lapic: &mut Lapic, ioapic: &Ioapic) -> u8 {
        let v = self.serial.read((port - arch::SERIAL2_PORT_BASE) as u8);
        self.after_uart_io(lapic, ioapic);
        v
    }

    /// After any UART register access, deliver the IRQ3 edge iff the model
    /// asserted an interrupt (mirrors the COM1 serial path in `main.rs`).
    fn after_uart_io(&mut self, lapic: &mut Lapic, ioapic: &Ioapic) {
        if self.drain.drain().is_ok() {
            crate::raise_irq(lapic, ioapic, arch::SERIAL2_IRQ);
        }
    }

    /// Queue one command line (VMM -> guest), appending the `\n` delimiter. Does
    /// NOT touch the FIFO or raise an IRQ — call [`pump`](Self::pump) for that.
    pub fn send_line(&mut self, line: &[u8]) {
        self.pending_rx.extend(line.iter().copied());
        self.pending_rx.push_back(b'\n');
    }

    /// Feed as many queued bytes as the RX FIFO will accept and raise IRQ3 if any
    /// moved. Called at loop boundaries (and after queueing a command inside the
    /// park), so a long command streams to the agent as it drains the FIFO.
    pub fn pump(&mut self, lapic: &mut Lapic, ioapic: &Ioapic) {
        if self.pending_rx.is_empty() {
            return;
        }
        let mut sent = false;
        while !self.pending_rx.is_empty() {
            let cap = self.serial.fifo_capacity();
            if cap == 0 {
                break;
            }
            let take = cap.min(self.pending_rx.len());
            let chunk: Vec<u8> = self.pending_rx.iter().take(take).copied().collect();
            match self.serial.enqueue_raw_bytes(&chunk) {
                Ok(n) if n > 0 => {
                    for _ in 0..n {
                        self.pending_rx.pop_front();
                    }
                    sent = true;
                }
                _ => break,
            }
        }
        if sent {
            // enqueue_raw_bytes already asserted RDA + its interrupt via the
            // trigger; drain it and convert to an IRQ3 edge.
            self.after_uart_io(lapic, ioapic);
        }
    }

    /// Pop one complete reply line (guest -> VMM) without its trailing newline,
    /// or `None` if no full line is buffered yet. `\r\n` is tolerated.
    pub fn poll_line(&mut self) -> Option<Vec<u8>> {
        let mut buf = self.tx.lock().unwrap();
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let mut line: Vec<u8> = buf.drain(..=pos).collect();
            line.pop(); // trailing '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            Some(line)
        } else {
            if buf.len() > TX_BUF_CAP {
                buf.clear();
            }
            None
        }
    }
}
