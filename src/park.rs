//! The idle park: how a halted guest waits for its next event.
//!
//! When the guest executes `HLT` it is waiting for an interrupt. With the
//! userspace irqchip there is no in-kernel LAPIC to block on, so the vCPU thread
//! itself waits here — on exactly two sources: the next guest-timer **deadline**
//! (an armed `timerfd`) and **console input** (stdin). It sleeps in `ppoll`,
//! consuming ~0% host CPU while idle, and wakes the instant either fires.
//!
//! This is deliberately the ONE place that converts virtual-time deadlines into
//! a real wait. Step 4's fast-forward replaces the `timerfd`-arm-and-wait with a
//! jump of the TSC offset: instead of sleeping `(deadline - now)` real
//! nanoseconds, it advances virtual time to the deadline and returns
//! immediately. Nothing else in the VMM waits on wall-clock time, so that change
//! stays surgical.

use std::os::unix::io::RawFd;

/// Which sources were ready when the park returned. Both can be true.
#[derive(Clone, Copy, Debug, Default)]
pub struct Wakes {
    pub input: bool,
    /// Whether the timer deadline elapsed. Informational: the caller re-checks
    /// the LAPIC deadline against `vtsc_now()` regardless of which fd fired, so
    /// this is not load-bearing (the timerfd is still drained inside `park`).
    #[allow(dead_code)]
    pub timer: bool,
}

pub struct Parker {
    timer_fd: RawFd,
    stdin_fd: RawFd,
    /// Whether stdin is still worth polling. A closed/EOF stdin (e.g. the smoke
    /// tests run with `</dev/null`) is *permanently* POLLIN-ready and would spin
    /// the park; once we see EOF we stop polling it and wait on the timer alone.
    stdin_open: bool,
}

impl Parker {
    pub fn new() -> std::io::Result<Self> {
        // SAFETY: timerfd_create with valid flags returns a new fd or -1/errno.
        let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            timer_fd: fd,
            stdin_fd: 0,
            stdin_open: true,
        })
    }

    /// Stop polling stdin (call after a read returns EOF). Idempotent.
    pub fn close_stdin(&mut self) {
        self.stdin_open = false;
    }

    /// Whether stdin is still worth polling (not EOF/closed).
    pub fn stdin_open(&self) -> bool {
        self.stdin_open
    }

    /// Non-blocking check for pending console input (poll with a 0 timeout).
    ///
    /// Used by the Step-4 fast-forward loop so that a quiet/idle console never
    /// blocks a jump: input is serviced first if (and only if) some is already
    /// ready, and otherwise the jump proceeds immediately. Returns `false` if
    /// stdin is closed. `POLLHUP` counts as ready (the caller reads, sees EOF,
    /// and closes stdin), exactly as the blocking `park` path treats it.
    pub fn stdin_ready(&self) -> std::io::Result<bool> {
        if !self.stdin_open {
            return Ok(false);
        }
        let mut fds = [libc::pollfd {
            fd: self.stdin_fd,
            events: libc::POLLIN,
            revents: 0,
        }];
        // SAFETY: one valid pollfd for the duration of the call; timeout 0 =
        // return immediately (non-blocking readiness probe).
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 1, 0) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                return Ok(false);
            }
            return Err(err);
        }
        Ok(fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0)
    }

    /// Sleep until console input arrives (if stdin is still open), or
    /// `timeout_ns` from now elapses (whichever first). `None` means "no timer".
    /// A `Some(0)` deadline is already due and returns immediately as a timer
    /// wake. If stdin is closed and there is no timer, this blocks forever — but
    /// an idle guest always has its next tick armed, so that does not arise.
    pub fn park(&self, timeout_ns: Option<u64>) -> std::io::Result<Wakes> {
        match timeout_ns {
            Some(0) => {
                self.disarm();
                return Ok(Wakes {
                    input: false,
                    timer: true,
                });
            }
            Some(ns) => self.arm(ns),
            None => self.disarm(),
        }

        // Always wait on the timerfd; add stdin only while it is open.
        let mut fds = [
            libc::pollfd {
                fd: self.timer_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let nfds = if self.stdin_open { 2 } else { 1 };

        // Block indefinitely; the armed timerfd provides the timeout. NULL
        // sigmask == leave the signal mask unchanged.
        // SAFETY: `fds` has `nfds` valid pollfds for the duration of the call.
        let n = unsafe {
            libc::ppoll(
                fds.as_mut_ptr(),
                nfds as libc::nfds_t,
                std::ptr::null(),
                std::ptr::null(),
            )
        };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                // Interrupted before anything was ready: report no wake; the
                // caller re-evaluates and parks again if still idle.
                return Ok(Wakes::default());
            }
            return Err(err);
        }

        let timer = fds[0].revents & libc::POLLIN != 0;
        // POLLHUP flags a closed stdin (pipe writer gone); treat it as input so
        // the caller performs the read, sees EOF, and closes stdin for us.
        let input =
            self.stdin_open && (fds[1].revents & (libc::POLLIN | libc::POLLHUP)) != 0;
        if timer {
            self.drain_timer();
        }
        Ok(Wakes { input, timer })
    }

    fn arm(&self, ns: u64) {
        let spec = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: (ns / 1_000_000_000) as libc::time_t,
                tv_nsec: (ns % 1_000_000_000) as libc::c_long,
            },
        };
        // SAFETY: valid fd, relative one-shot arming, no old-value out pointer.
        unsafe {
            libc::timerfd_settime(self.timer_fd, 0, &spec, std::ptr::null_mut());
        }
    }

    fn disarm(&self) {
        let spec: libc::itimerspec = unsafe { std::mem::zeroed() };
        // SAFETY: valid fd; zeroed it_value disarms the timer.
        unsafe {
            libc::timerfd_settime(self.timer_fd, 0, &spec, std::ptr::null_mut());
        }
    }

    fn drain_timer(&self) {
        let mut buf = [0u8; 8];
        // SAFETY: reading up to 8 bytes (the expiration count) into a valid buf.
        unsafe {
            libc::read(self.timer_fd, buf.as_mut_ptr() as *mut libc::c_void, 8);
        }
    }
}

impl Drop for Parker {
    fn drop(&mut self) {
        // SAFETY: closing an fd we own.
        unsafe {
            libc::close(self.timer_fd);
        }
    }
}
