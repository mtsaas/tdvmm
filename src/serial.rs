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

/// Restores terminal settings on drop; puts the tty in raw mode so guest
/// keystrokes pass through unmodified (no host echo/canonical processing).
pub struct RawTerminal {
    fd: i32,
    original: Option<libc::termios>,
}

impl RawTerminal {
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

