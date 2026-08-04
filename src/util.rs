//! Small shared helpers: byte<->integer conversions for the MMIO exit handlers
//! (LAPIC/IOAPIC register reads/writes hand back/take raw little-endian byte
//! slices), plus a few thin libc wrappers used by the timer/signal machinery.

/// This thread's kernel tid — for per-thread timer targeting and `tgkill`.
pub(crate) fn gettid() -> i32 {
    // SAFETY: gettid takes no arguments and always succeeds.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// Install `handler` for `sig` WITHOUT `SA_RESTART`, so a delivery makes an
/// in-flight `KVM_RUN` return `EINTR` rather than silently restarting. Callers'
/// handlers must be async-signal-safe.
pub(crate) fn install_signal_handler(sig: libc::c_int, handler: extern "C" fn(libc::c_int)) {
    // SAFETY: standard sigaction install with an empty mask and no flags.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = handler as *const () as libc::sighandler_t;
        sa.sa_flags = 0;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

/// A one-shot `itimerspec` firing `ns` from now (no repeat interval), for
/// `timer_settime`/`timerfd_settime`. A zero `ns` disarms rather than fires, so
/// callers that need "fire now" floor it to at least 1ns themselves.
pub(crate) fn one_shot_itimerspec(ns: u64) -> libc::itimerspec {
    libc::itimerspec {
        it_interval: libc::timespec { tv_sec: 0, tv_nsec: 0 },
        it_value: libc::timespec {
            tv_sec: (ns / 1_000_000_000) as libc::time_t,
            tv_nsec: (ns % 1_000_000_000) as libc::c_long,
        },
    }
}

pub(crate) fn read_u32_le(data: &[u8]) -> u32 {
    let mut b = [0u8; 4];
    for (i, dst) in b.iter_mut().enumerate() {
        if i < data.len() {
            *dst = data[i];
        }
    }
    u32::from_le_bytes(b)
}

pub(crate) fn write_u32_le(data: &mut [u8], val: u32) {
    let b = val.to_le_bytes();
    for (i, dst) in data.iter_mut().enumerate() {
        if i < 4 {
            *dst = b[i];
        }
    }
}
