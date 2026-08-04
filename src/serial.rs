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

/// Cap on the guest-console tee buffer: a runaway (never-drained) writer is
/// dropped rather than grown without bound. Mirrors [`crate::control`]'s TX cap.
/// Every console byte is a PIO exit and the scanner drains each loop boundary, so
/// this only trips pathologically.
const CONSOLE_TEE_CAP: usize = 1 << 20;

/// The COM1 console writer: passes every guest-TX byte straight through to host
/// stdout (fd 1), byte-by-byte, and — when `--logs-dir` is on — ALSO tees a copy
/// into a shared buffer for the console scanner ([`crate::conscan`]).
///
/// The value returned to the UART model is fd 1's own `write` result; the tee
/// append is a pure observer that can never change it. So the guest's serial
/// stream on stdout is byte-identical whether or not a tee is attached — the
/// "clean machine output" invariant holds by construction.
pub struct ConsoleOut {
    tee: Option<Arc<Mutex<Vec<u8>>>>,
    /// One-shot: an overflow warns once, not per byte.
    overflowed: bool,
}
impl ConsoleOut {
    pub fn new(tee: Option<Arc<Mutex<Vec<u8>>>>) -> Self {
        Self {
            tee,
            overflowed: false,
        }
    }
}
impl Write for ConsoleOut {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // Passthrough FIRST; its result is what we return.
        // SAFETY: fd 1 is valid; buf points to `buf.len()` readable bytes.
        let n = unsafe { libc::write(1, buf.as_ptr() as *const libc::c_void, buf.len()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let n = n as usize;
        if let Some(tee) = &self.tee {
            let mut b = tee.lock().unwrap();
            if b.len() + n > CONSOLE_TEE_CAP {
                b.clear();
                if !self.overflowed {
                    self.overflowed = true;
                    crate::log_line(format_args!(
                        "[tdvmm][WARN] console tee exceeded {CONSOLE_TEE_CAP} bytes \
                         between drains — dropping backlog"
                    ));
                }
            }
            b.extend_from_slice(&buf[..n]);
        }
        Ok(n)
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

/// Whether `port` is one of COM1's (ttyS0) 8 legacy 16550 PIO registers.
pub(crate) fn is_serial(port: u16) -> bool {
    (crate::arch::SERIAL_PORT_BASE..crate::arch::SERIAL_PORT_BASE + 8).contains(&port)
}

pub type SharedSerial = Arc<Mutex<Serial<EventFdTrigger, NoEvents, ConsoleOut>>>;

/// Build the shared UART. Returns the serial handle and a clone of its
/// interrupt eventfd for the vCPU thread to drain. `tee` is `Some` only under
/// `--logs-dir` (COM1 output is copied into it for the console scanner);
/// `None` is exactly the plain stdout passthrough.
pub fn new_serial(
    tee: Option<Arc<Mutex<Vec<u8>>>>,
) -> std::io::Result<(SharedSerial, EventFdTrigger)> {
    let trigger = EventFdTrigger::new()?;
    let drain_handle = trigger.try_clone()?;
    let serial = Serial::new(trigger, ConsoleOut::new(tee));
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
    /// changed. tdvmm's own log lines key their CRLF handling off this (see
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_out_tee_never_changes_the_write_result() {
        // With a tee attached, write() returns exactly the fd-1 byte count and
        // ALSO mirrors the bytes into the tee — the append can't alter the return.
        let tee = Arc::new(Mutex::new(Vec::new()));
        let mut out = ConsoleOut::new(Some(tee.clone()));
        assert_eq!(out.write(b"hello").unwrap(), 5);
        assert_eq!(&*tee.lock().unwrap(), b"hello");
        // Without a tee: same return, nothing captured (plain passthrough).
        let mut bare = ConsoleOut::new(None);
        assert_eq!(bare.write(b"xy").unwrap(), 2);
    }

    #[test]
    fn console_tee_is_bounded_under_a_runaway_writer() {
        // `ConsoleOut::write` always hits real fd 1, and the cap forces ~1 MiB
        // through it; redirect fd 1 to /dev/null so it doesn't spam test output.
        // The assertion is on the tee buffer, so this never depends on fd 1.
        let _redirect = Fd1ToDevNull::redirect();
        let tee = Arc::new(Mutex::new(Vec::new()));
        let mut out = ConsoleOut::new(Some(tee.clone()));
        let chunk = vec![b'a'; 4096];
        // Push just past the 1 MiB cap; the buffer must stay bounded.
        for _ in 0..(CONSOLE_TEE_CAP / chunk.len() + 8) {
            out.write(&chunk).unwrap();
        }
        assert!(tee.lock().unwrap().len() <= CONSOLE_TEE_CAP + chunk.len());
    }

    /// Redirects fd 1 to /dev/null for the current scope, restoring on drop, so a
    /// test that must write through the real fd-1 path doesn't pollute `cargo test`
    /// output. (The only raw fd-1 writer in this crate is `ConsoleOut`, and its
    /// assertions never read fd 1, so the process-global swap can't cause flakes.)
    struct Fd1ToDevNull {
        saved: i32,
        devnull: i32,
    }
    impl Fd1ToDevNull {
        fn redirect() -> Self {
            // SAFETY: standard fd dup/redirect; the saved fd 1 is restored on drop.
            unsafe {
                let saved = libc::dup(1);
                let devnull = libc::open(
                    b"/dev/null\0".as_ptr() as *const libc::c_char,
                    libc::O_WRONLY,
                );
                libc::dup2(devnull, 1);
                Self { saved, devnull }
            }
        }
    }
    impl Drop for Fd1ToDevNull {
        fn drop(&mut self) {
            // SAFETY: restore the original fd 1 and close the temporaries we own.
            unsafe {
                libc::dup2(self.saved, 1);
                libc::close(self.saved);
                libc::close(self.devnull);
            }
        }
    }
}

