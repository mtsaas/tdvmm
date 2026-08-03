//! The tick doorbell: lets an armed virtual-time deadline preempt an EXIT-FREE
//! `KVM_RUN`.
//!
//! With a userspace irqchip and a single vCPU, guest timers are serviced only at
//! vCPU-loop boundaries (a device MMIO/PIO exit or a HLT). A guest that runs a
//! full tick period *without any exit* — a CPU burst that never touches an
//! emulated device and never HLTs (e.g. the fork/exec storm of `docker compose
//! up`) — would never get its tick serviced: jiffies freeze, and on one CPU the
//! scheduler can never preempt, so any task busy-waiting on a starved waker
//! wedges forever (no exits, no HLT). This is the container-start wedge.
//!
//! The doorbell closes that gap. A per-thread POSIX timer (`timer_create` with
//! `SIGEV_THREAD_ID`, no extra thread) is armed by the vCPU thread at each loop
//! boundary to the earliest pending deadline. When it fires:
//!   - during `KVM_RUN` → the signal makes `KVM_RUN` return `EINTR`;
//!   - in userspace (between boundaries) → the handler stores 1 into
//!     `kvm_run->immediate_exit`, so the *next* `KVM_RUN` returns immediately.
//! The loop clears `immediate_exit` at the top before `service_timers`, so a fire
//! anywhere after that is never lost. Either path reaches `service_timers`, which
//! fires the due tick and preempts the guest.
//!
//! Invariants: single-writer is preserved — the handler mutates no guest state
//! (only the KVM-owned `immediate_exit` doorbell byte); all expiry processing and
//! IRR sets stay in `service_timers` on the vCPU thread; the vCPU thread arms the
//! host timer. FF-neutral — FF ON/OFF are identical while running (the TSC offset
//! only moves while parked); a stale fire during a park is a benign `EINTR`
//! (`park.rs` tolerates it). Determinism is unaffected: this only adds
//! real-time-positioned exit boundaries, which every device exit already is, and
//! determinism is the separate future substrate.
//!
//! Default ON; `DVMM_NO_DOORBELL=1` disables it (A/B against the old behavior).

use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use crate::vtsc::VirtualClock;

/// Pointer to the mmapped `kvm_run.immediate_exit` byte, for the signal handler.
/// Set once by [`Doorbell::new`]; the handler stores 1 into it. The mmap address
/// is stable for the vCPU's lifetime, so this raw pointer stays valid.
static IMMEDIATE_EXIT: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());

/// Set by the handler on every fire, so the vCPU thread re-arms even when the
/// earliest deadline is unchanged (guards the timer-fires-slightly-early edge,
/// where `service_timers` did not consume the tick and the one-shot is now spent).
static FIRED: AtomicBool = AtomicBool::new(false);

pub struct Doorbell {
    enabled: bool,
    timer: libc::timer_t,
    /// The vtsc deadline the host timer is currently armed for (arming cache).
    last_armed: Option<u64>,
}

impl Doorbell {
    /// Install the doorbell for this (vCPU) thread. Reads `DVMM_NO_DOORBELL`;
    /// when enabled, records the `immediate_exit` pointer, installs the handler,
    /// and creates a per-thread one-shot timer. Must be called ON the vCPU thread
    /// (the timer targets `gettid()`).
    pub fn new(vcpu: &mut kvm_ioctls::VcpuFd) -> Self {
        let disabled = std::env::var("DVMM_NO_DOORBELL")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if disabled {
            crate::log_line(format_args!(
                "[dvmm][WARN] tick doorbell DISABLED (DVMM_NO_DOORBELL) — an \
                 exit-free guest cannot be preempted; for A/B testing only"
            ));
            return Self::off();
        }

        let sig = libc::SIGRTMIN();

        // Create the per-thread one-shot timer FIRST; only a live timer gets the
        // handler + the immediate_exit pointer, so a create failure leaves no
        // residue. Safe: SIGEV_THREAD_ID delivers nothing until the first arm().
        // SAFETY: a valid sigevent targeting this thread by tid.
        let timer = unsafe {
            let tid = crate::util::gettid();
            let mut sev: libc::sigevent = std::mem::zeroed();
            sev.sigev_notify = libc::SIGEV_THREAD_ID;
            sev.sigev_signo = sig;
            // libc models glibc/musl's `_sigev_un._tid` union arm as this field.
            sev.sigev_notify_thread_id = tid;
            let mut timerid: libc::timer_t = std::mem::zeroed();
            if libc::timer_create(libc::CLOCK_MONOTONIC, &mut sev, &mut timerid) != 0 {
                crate::log_line(format_args!(
                    "[dvmm][WARN] tick doorbell: timer_create failed ({}) — an \
                     exit-free guest may wedge",
                    std::io::Error::last_os_error()
                ));
                return Self::off();
            }
            timerid
        };

        // Record the immediate_exit byte's address for the handler and install the
        // handler — only now that the timer exists. The kvm_run mmap is stable for
        // the vCPU's lifetime.
        let ie_ptr = &mut vcpu.get_kvm_run().immediate_exit as *mut u8;
        IMMEDIATE_EXIT.store(ie_ptr, Ordering::SeqCst);
        crate::util::install_signal_handler(sig, on_doorbell);

        crate::log_line(format_args!(
            "[dvmm] tick doorbell ARMED (SIGRTMIN+{}): armed timers preempt an \
             exit-free vCPU; immediate_exit closes the wakeup race",
            sig - libc::SIGRTMIN()
        ));
        Self {
            enabled: true,
            timer,
            last_armed: None,
        }
    }

    fn off() -> Self {
        Self {
            enabled: false,
            timer: std::ptr::null_mut(),
            last_armed: None,
        }
    }

    /// Clear `immediate_exit` at the top of the loop, BEFORE `service_timers`.
    /// Any doorbell fire after this re-sets it, so the coming `KVM_RUN` bails.
    pub fn clear(&self, vcpu: &mut kvm_ioctls::VcpuFd) {
        if self.enabled {
            vcpu.get_kvm_run().immediate_exit = 0;
        }
    }

    /// Arm the host timer to fire at the earliest pending `deadline` (vtsc), so an
    /// exit-free `KVM_RUN` is broken in time to service it; `None` disarms.
    /// Re-arms only when the deadline changed or the timer has fired (cheap on the
    /// exit-dense normal path, where the pending deadline is stable between ticks).
    pub fn arm(&mut self, deadline: Option<u64>, clock: &VirtualClock) {
        if !self.enabled {
            return;
        }
        let fired = FIRED.swap(false, Ordering::AcqRel);
        if deadline == self.last_armed && !fired {
            return;
        }
        self.last_armed = deadline;
        let spec = match deadline {
            Some(dl) => {
                let now = clock.vtsc_now();
                // At least 1ns: an all-zero it_value would DISARM, not fire-now.
                let ns = if dl > now {
                    clock.freq().cycles_to_ns(dl - now).max(1)
                } else {
                    1
                };
                crate::util::one_shot_itimerspec(ns)
            }
            // SAFETY: a zeroed itimerspec disarms the timer.
            None => unsafe { std::mem::zeroed() },
        };
        // SAFETY: valid timer id; relative one-shot arming; no old-value pointer.
        unsafe {
            libc::timer_settime(self.timer, 0, &spec, std::ptr::null_mut());
        }
    }

}

impl Drop for Doorbell {
    fn drop(&mut self) {
        if !self.enabled {
            return;
        }
        // Neutralize the handler FIRST (null pointer → a late or pending fire is a
        // no-op), THEN delete the timer. The doorbell is a vCPU-thread local and
        // drops before the vCPU itself, so kvm_run is still mapped at this point;
        // nulling first also covers a signal that races timer_delete.
        IMMEDIATE_EXIT.store(std::ptr::null_mut(), Ordering::SeqCst);
        if !self.timer.is_null() {
            // SAFETY: deleting a timer we created and still own.
            unsafe {
                libc::timer_delete(self.timer);
            }
        }
    }
}

extern "C" fn on_doorbell(_sig: libc::c_int) {
    let p = IMMEDIATE_EXIT.load(Ordering::Relaxed);
    if !p.is_null() {
        // SAFETY: `p` addresses the mmapped kvm_run.immediate_exit byte, valid for
        // the vCPU's lifetime; a single volatile u8 store is async-signal-safe.
        unsafe {
            p.write_volatile(1);
        }
    }
    FIRED.store(true, Ordering::Relaxed);
}
