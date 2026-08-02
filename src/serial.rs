//! 16550 UART (via `vm-superio`) on PIO.
//!
//! Guest serial TX bytes are handled on the vCPU thread (PIO exits) and written
//! straight to the host terminal. Guest serial RX is fed on the vCPU thread too:
//! it reads stdin while parked at a HLT exit (see `park.rs` / `main.rs`), so the
//! single-writer invariant holds — there is no off-thread input source.

use std::io::Write;
use std::sync::{Arc, Mutex};

use vm_superio::serial::NoEvents;
use vm_superio::{Serial, Trigger};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

/// A `vm-superio` `Trigger` backed by an eventfd. When the UART model wants to
/// raise an interrupt it writes the eventfd; the vCPU thread drains it after
/// serial PIO and converts it into an IRQ via the IrqChip.
pub struct EventFdTrigger(EventFd);

impl EventFdTrigger {
    pub fn new() -> std::io::Result<Self> {
        Ok(Self(EventFd::new(EFD_NONBLOCK)?))
    }
    pub fn try_clone(&self) -> std::io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }
    /// Non-blocking: `Ok` if the UART had signalled a pending interrupt.
    pub fn drain(&self) -> std::io::Result<u64> {
        self.0.read()
    }
}

impl Trigger for EventFdTrigger {
    type E = std::io::Error;
    fn trigger(&self) -> std::io::Result<()> {
        self.0.write(1)
    }
}

/// Unbuffered writer straight to host stdout (fd 1) so guest console output
/// appears byte-by-byte.
pub struct RawStdout;
impl Write for RawStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // SAFETY: fd 1 is valid; buf points to `buf.len()` readable bytes.
        let n = unsafe { libc::write(1, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Whether the VMM's stdin (fd 0) is a terminal. Used ONLY for the interactive
/// startup banner + the fast-forward advisory warning (telemetry) — never for
/// the fast-forward decision itself, which must not vary with the ambient
/// environment. `RawTerminal::enable` already gates on the same signal.
pub fn stdin_is_tty() -> bool {
    // SAFETY: isatty just queries an fd; fd 0 is always valid to query.
    unsafe { libc::isatty(0) == 1 }
}

pub type SharedSerial = Arc<Mutex<Serial<EventFdTrigger, NoEvents, RawStdout>>>;

/// Build the shared UART. Returns the serial handle and a clone of its
/// interrupt eventfd for the vCPU thread to drain.
pub fn new_serial() -> std::io::Result<(SharedSerial, EventFdTrigger)> {
    let trigger = EventFdTrigger::new()?;
    let drain_handle = trigger.try_clone()?;
    let serial = Serial::new(trigger, RawStdout);
    Ok((Arc::new(Mutex::new(serial)), drain_handle))
}

/// A `Write` sink that captures a UART's guest-TX bytes into a shared buffer
/// instead of the host terminal. This is how the TEST-1a **control channel**
/// (COM2 / ttyS1) reads the guest agent's line-delimited JSON replies: the guest
/// writes them to ttyS1 (THR PIO), `vm-superio` calls this writer, and the
/// scenario harness drains complete lines from the buffer. Single-threaded on the
/// vCPU thread; the `Arc<Mutex<..>>` mirrors the COM1 handle shape and is cheap
/// (tiny traffic).
pub struct ControlSink {
    buf: Arc<Mutex<Vec<u8>>>,
}
impl ControlSink {
    pub fn new(buf: Arc<Mutex<Vec<u8>>>) -> Self {
        Self { buf }
    }
}
impl Write for ControlSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The COM2 UART type: a `vm-superio` 16550 whose TX is captured (not printed).
pub type ControlSerial = Serial<EventFdTrigger, NoEvents, ControlSink>;

/// Build the control-channel UART (COM2 / ttyS1). Returns the UART, a clone of
/// its interrupt eventfd (drained on the vCPU thread to convert into IRQ3), and
/// the shared TX-capture buffer the harness reads replies from.
pub fn new_control_serial(
) -> std::io::Result<(ControlSerial, EventFdTrigger, Arc<Mutex<Vec<u8>>>)> {
    let trigger = EventFdTrigger::new()?;
    let drain = trigger.try_clone()?;
    let buf = Arc::new(Mutex::new(Vec::new()));
    let serial = Serial::new(trigger, ControlSink::new(buf.clone()));
    Ok((serial, drain, buf))
}

/// Restores terminal settings on drop; puts the tty in raw mode so guest
/// keystrokes pass through unmodified (no host echo/canonical processing).
pub struct RawTerminal {
    fd: i32,
    original: Option<libc::termios>,
}

impl RawTerminal {
    /// Whether raw mode is actually in effect (i.e. `enable` found a tty and
    /// applied `cfmakeraw`). `false` when stdin is not a tty, so nothing was
    /// changed. dvmm's own log lines key their CRLF handling off this (see
    /// `main.rs` `log_line`): raw mode turns OFF the terminal's ONLCR, so a bare
    /// "\n" would staircase our lines.
    pub fn is_raw(&self) -> bool {
        self.original.is_some()
    }

    pub fn enable(fd: i32) -> Self {
        // SAFETY: querying/modifying termios on a valid fd.
        unsafe {
            if libc::isatty(fd) != 1 {
                return Self { fd, original: None };
            }
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut termios) != 0 {
                return Self { fd, original: None };
            }
            let original = termios;
            libc::cfmakeraw(&mut termios);
            let _ = libc::tcsetattr(fd, libc::TCSANOW, &termios);
            Self {
                fd,
                original: Some(original),
            }
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        if let Some(original) = self.original {
            // SAFETY: restoring the previously saved termios on the same fd.
            unsafe {
                let _ = libc::tcsetattr(self.fd, libc::TCSANOW, &original);
            }
        }
    }
}

