//! Raw Linux syscalls without libc (the crate's dependency rule): termios via
//! `ioctl` for raw mode on ttyS1, and `poll(2)` to block on ttyS1 + the event FIFO
//! at once. x86_64 inline-asm, std/core only.

// ============================================================================
// Raw termios via a std-only inline-asm ioctl (no libc).
// ============================================================================

const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
// c_iflag
const F_IGNBRK: u32 = 0x1;
const F_BRKINT: u32 = 0x2;
const F_PARMRK: u32 = 0x8;
const F_ISTRIP: u32 = 0x20;
const F_INLCR: u32 = 0x40;
const F_IGNCR: u32 = 0x80;
const F_ICRNL: u32 = 0x100;
const F_IXON: u32 = 0x400;
// c_oflag
const F_OPOST: u32 = 0x1;
// c_lflag
const F_ECHO: u32 = 0x8;
const F_ECHONL: u32 = 0x40;
const F_ICANON: u32 = 0x2;
const F_ISIG: u32 = 0x1;
const F_IEXTEN: u32 = 0x8000;
// c_cflag
const F_CSIZE: u32 = 0x30;
const F_PARENB: u32 = 0x100;
const F_CS8: u32 = 0x30;
// c_cc indices (kernel `struct termios`)
const I_VTIME: usize = 5;
const I_VMIN: usize = 6;

/// The kernel `struct termios` (asm-generic): 4 flag words + c_line + c_cc[19].
#[repr(C)]
#[derive(Default)]
struct Termios {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
}

/// `ioctl(fd, request, argp)` via the raw x86_64 syscall (nr 16). Returns the
/// kernel return value (negative errno on failure). std/core only.
///
/// # Safety
///
/// `fd` must be a valid open file descriptor and `argp` a valid, aligned,
/// non-null pointer to a `Termios` that stays live for the call. The kernel
/// writes through it for `TCGETS` and reads it for `TCSETS`, so it must be
/// writable for the caller's chosen `request`.
#[cfg(target_arch = "x86_64")]
unsafe fn ioctl(fd: i32, request: u64, argp: *mut Termios) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") 16i64 => ret, // __NR_ioctl
        in("rdi") fd as i64,
        in("rsi") request,
        in("rdx") argp,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

// poll(2): wait on ttyS1 + the event FIFO at once. POSIX `pollfd`; POLLIN=input
// ready, POLLHUP=hangup, POLLERR=error. EINTR is retried.
pub(crate) const POLLIN: i16 = 0x0001;
pub(crate) const POLLERR: i16 = 0x0008;
pub(crate) const POLLHUP: i16 = 0x0010;
pub(crate) const EINTR: i64 = 4;

#[repr(C)]
pub(crate) struct PollFd {
    pub(crate) fd: i32,
    pub(crate) events: i16,
    pub(crate) revents: i16,
}

/// `poll(fds, nfds, timeout)` via the raw x86_64 syscall (nr 7). `timeout = -1`
/// blocks indefinitely (no timer armed → fast-forward-transparent). Returns the
/// kernel return value (negative errno on failure). std/core only.
///
/// # Safety
///
/// `fds` must point to `nfds` contiguous, valid `PollFd`s that stay live for the
/// call, and `nfds` must not exceed that array's length. The kernel writes each
/// entry's `revents`, so the array must be writable.
#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") 7i64 => ret, // __NR_poll
        in("rdi") fds,
        in("rsi") nfds,
        in("rdx") timeout as i64,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack),
    );
    ret
}

/// cfmakeraw-equivalent on `fd`: no echo, no canonical mode, no signal
/// generation, no I/O post-processing; VMIN=1/VTIME=0 blocking read.
///
/// # Errors
///
/// Returns `Err(errno)` if the `TCGETS` or `TCSETS` `ioctl` fails.
pub(crate) fn set_raw(fd: i32) -> Result<(), i64> {
    let mut t = Termios::default();
    let r = unsafe { ioctl(fd, TCGETS, &mut t) };
    if r < 0 {
        return Err(-r);
    }
    t.c_iflag &= !(F_IGNBRK | F_BRKINT | F_PARMRK | F_ISTRIP | F_INLCR | F_IGNCR | F_ICRNL | F_IXON);
    t.c_oflag &= !F_OPOST;
    t.c_lflag &= !(F_ECHO | F_ECHONL | F_ICANON | F_ISIG | F_IEXTEN);
    t.c_cflag &= !(F_CSIZE | F_PARENB);
    t.c_cflag |= F_CS8;
    t.c_cc[I_VMIN] = 1;
    t.c_cc[I_VTIME] = 0;
    let r = unsafe { ioctl(fd, TCSETS, &mut t) };
    if r < 0 {
        return Err(-r);
    }
    Ok(())
}
