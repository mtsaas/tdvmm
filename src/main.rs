//! tdvmm — fast-forward virtual time on idle.
//!
//! A single-vCPU, Firecracker-shaped KVM VMM. The guest runs on a **userspace**
//! interrupt controller we own: no in-kernel irqchip, no in-kernel PIT. The
//! LAPIC's one-shot/periodic timer (driven by [`vtsc`]) is the tick; LAPIC/IOAPIC
//! register accesses are MMIO exits; and a halted guest parks at its HLT exit.
//!
//! ## The jump (fast-forward)
//!
//! When the guest is idle (HLTed) waiting for a future timer, the parker no
//! longer *waits* real time for that deadline — it **jumps** virtual time to it:
//! compute `Δ = next_event_vtsc − vtsc_now()`, bump the cached TSC offset by `Δ`
//! (write-through to `KVM_VCPU_TSC_OFFSET`), fire everything now due, and loop.
//! The guest experiences hours passing in seconds of wall clock. This is a
//! runtime flag (`--ff on|off`, default ON); with FF off the real-wait park
//! (`ppoll` on a `timerfd` + stdin) is used instead — the A/B for timing bugs
//! and the right mode for an interactive console. Only the *wait* changes; the
//! wake path (IRR → injection window → RUNNABLE) is unchanged.
//!
//! ## Single-writer invariant
//!
//! ALL guest-state effects — LAPIC/IOAPIC register state, interrupt raises, the
//! TSC-offset bump, and every KVM vcpu ioctl — happen on the vCPU thread at loop
//! boundaries. The offset is written ONLY while parked at a HLT exit, between
//! `KVM_RUN`s, never concurrent with a running vCPU. The vCPU thread owns console
//! input (it reads stdin while parked at HLT), so there is no off-thread writer
//! at all.
//!
//! ## vCPU loop shape
//!
//! `service_timers(); sync_tpr(); inject(); run(); handle_exit()` — timers fire
//! at loop boundaries (or at the HLT park), and the park is the one place that
//! converts a virtual-time deadline into either a real wait or a jump.

mod arch;
mod artifact;
mod boot;
mod build;
mod cli;
mod compose;
mod conscan;
mod control;
mod cpio;
mod cpuid;
mod diag;
mod doctor;
mod doorbell;
mod egress;
mod egress_test_server;
mod engine;
mod events;
mod exit;
mod ioapic;
mod lapic;
mod memory;
mod msrs;
mod mptable;
mod park;
mod pic;
mod pit;
mod regs;
mod driver;
mod serial;
mod telemetry;
mod ui;
mod util;
mod vtsc;

use clap::Parser;
use kvm_bindings::{kvm_interrupt, KVM_MAX_CPUID_ENTRIES};
use kvm_ioctls::{Kvm, VcpuExit, VcpuFd};
use vmm_sys_util::ioctl::ioctl_with_ref;
use vmm_sys_util::ioctl_iow_nr;

use crate::cli::{BootArgs, Cli, Cmd, EffectiveConfig, LsArgs, RunArgs};
use crate::exit::{RunOutcome, StopReason, EXIT_INFRA};
use crate::ioapic::Ioapic;
use crate::lapic::{apic_bus_hz_from_cpuid, apic_timer_tsc_ratio, in_lapic, Lapic, XAPIC_BASE};
use crate::mptable::isa_irq_to_ioapic_pin;
use crate::pic::PicStub;
use crate::pit::PitStub;
use crate::telemetry::FfState;
use crate::vtsc::VirtualClock;

// KVM_INTERRUPT = _IOW(KVMIO, 0x86, struct kvm_interrupt): queue one interrupt
// vector for injection on the next entry. Valid only without an in-kernel LAPIC
// (our userspace-irqchip backend). kvm-ioctls 0.25 does not wrap it, so we issue
// it directly on the vCPU fd, exactly as `vtsc.rs` does for the TSC device
// attributes. (Re-check on kvm-ioctls upgrades: drop this if a later release
// exposes `VcpuFd::interrupt`.)
const KVMIO: u32 = 0xAE;
ioctl_iow_nr!(KVM_INTERRUPT, KVMIO, 0x86, kvm_interrupt);

// ---- tdvmm's own stderr logging (raw-tty aware) -----------------------------
//
// At an interactive console tdvmm puts the tty in RAW mode (see
// `serial::RawTerminal`) so the GUEST owns the byte stream verbatim — which also
// turns OFF the terminal's newline->CRLF translation (ONLCR). A bare "\n" on OUR
// OWN log lines would then only drop down a row, not return to column 0, so our
// telemetry/startup/WARN lines would staircase across the guest's output. When
// raw mode is active we therefore terminate our log lines with CRLF and prepend a
// CR to snap to column 0 (embedded newlines get the same treatment). In cooked
// mode the terminal itself adds the CR, so a plain "\n" is already correct. This
// changes ONLY tdvmm's own log lines — the guest's byte stream is untouched.
//
// The flag starts false and is set true only once `RawTerminal::enable` has put
// the tty in raw mode (see `run`), so lines emitted during cooked-mode boot setup
// still use a plain "\n".
static RAW_TTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Emit one tdvmm log line to stderr with raw-tty-aware line endings (see the
/// module note above). Used via the [`dlog!`] macro (and directly by the device
/// modules); not for guest output.
pub(crate) fn log_line(args: std::fmt::Arguments) {
    use std::io::Write;
    let body = format!("{args}");
    let stderr = std::io::stderr();
    let mut h = stderr.lock();
    let _ = if RAW_TTY.load(std::sync::atomic::Ordering::Relaxed) {
        // Snap to column 0 and turn every embedded newline into a CRLF too.
        write!(h, "\r{}\r\n", body.replace('\n', "\r\n"))
    } else {
        writeln!(h, "{body}")
    };
    let _ = h.flush();
}

/// tdvmm's own stderr log line, raw-tty aware (see [`log_line`]). A drop-in for
/// `eprintln!` for tdvmm's OWN diagnostics — never for guest console bytes.
macro_rules! dlog {
    ($($arg:tt)*) => { crate::log_line(format_args!($($arg)*)) };
}

// `no_timer_check`: the userspace backend emits no PIT IRQ0, so the kernel must
// not run its "does the timer IRQ reach the CPU?" probe. `tsc=reliable`: trust
// the invariant TSC and skip the clocksource watchdog.
//
// `reboot=t` (triple fault), NOT `reboot=k`: on a guest reboot/panic the kernel
// resets the machine. We do NOT emulate an i8042 keyboard controller, so the
// `reboot=k` (keyboard-controller reset) method never completes here — the guest
// falls into a halt/re-arm-timer loop that fast-forward would advance FOREVER
// (the VMM never exits). `reboot=t` forces a triple fault, which surfaces as
// KVM_EXIT_SHUTDOWN and stops the VMM cleanly, so a guest that reboots/panics
// (e.g. `exit` at the PID-1 shell) actually terminates the run. The tested smoke
// paths already used `reboot=t`; this makes the interactive default match them.
pub(crate) const DEFAULT_CMDLINE: &str =
    "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable";
pub(crate) const DEFAULT_MEM_MIB: u64 = 2048;
/// Default fast-forward single-jump sanity bound (seconds). A jump larger than
/// this aborts the run (gate 3) — expected never to trip in normal operation.
/// A float so the bound can be set below the sub-second jumps a real workload
/// produces (this is a config threshold, not a timer/vtsc conversion).
pub(crate) const DEFAULT_MAX_JUMP_SECS: f64 = 300.0;

/// Opt-in per-service log capture (`--logs-dir`): where to write, and the service
/// set to pull (the run's compose.lock service names). Off unless the flag is set.
struct LogsCapture {
    dir: std::path::PathBuf,
    /// Compose service names, sorted for a deterministic per-service file order.
    services: Vec<String>,
    /// The compose project (`tdvmm_<stack>`), for mapping `<project>-<service>-<n>`
    /// container-log prefixes back to services in the console scanner.
    project: String,
}

/// Create `<dir>` (idempotent) and prove it is writable by round-tripping a probe
/// file — BEFORE boot. Returns a human error so the caller can fail fast (exit 2):
/// a `--logs-dir` that cannot be written must never surface as a late mid-run
/// surprise. This is the ONLY `--logs-dir` failure that stops a run; every later
/// capture error merely warns and skips (verdict-safety, Fable guardrail 4).
fn prepare_logs_dir(dir: &str) -> Result<std::path::PathBuf, String> {
    let path = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&path).map_err(|e| format!("creating --logs-dir {dir}: {e}"))?;
    let probe = path.join(".tdvmm-logs-probe");
    std::fs::write(&probe, b"ok").map_err(|e| format!("--logs-dir {dir} is not writable: {e}"))?;
    let _ = std::fs::remove_file(&probe);
    Ok(path)
}

/// A scheduled event in the one [`events::EventQueue`]. Every guest timer is an
/// entry here (today that is the LAPIC deadline, the tick); the fast-forward path adds the
/// virtual-time horizon as a first-class queue event so the run terminates
/// through the same drain path rather than a bolted-on loop check.
#[derive(Clone, Copy, Debug)]
enum TimerKind {
    /// The LAPIC one-shot/periodic timer deadline (the guest's tick).
    LapicDeadline,
    /// `--max-virtual-time`: when vtsc reaches the horizon, stop the run. A
    /// deterministic virtual-time event, not a real-time policy.
    StopRun,
}

/// Parse a duration to seconds (f64). A bare number is seconds; suffixes `ms`,
/// `s`, `m`, `h` are honored. Returns `None` on anything unparseable or <= 0.
pub(crate) fn parse_duration_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    let (num, mult) = if let Some(v) = s.strip_suffix("ms") {
        (v, 0.001)
    } else if let Some(v) = s.strip_suffix('s') {
        (v, 1.0)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 60.0)
    } else if let Some(v) = s.strip_suffix('h') {
        (v, 3600.0)
    } else {
        (s, 1.0)
    };
    num.trim()
        .parse::<f64>()
        .ok()
        .map(|n| n * mult)
        .filter(|&secs| secs.is_finite() && secs > 0.0)
}

/// The startup fast-forward **mode statement** (spec item 1): the FF state plus
/// how it was chosen. Rendered identically whether or not stdin is a tty, and
/// ALWAYS printed at startup, so the effective default is visible to a human and
/// can be mechanically asserted by the test suite. isatty NEVER feeds this — the
/// FF decision must not vary with the ambient environment (Fable-locked).
fn ff_mode_statement(fast_forward: bool, ff_explicit: bool) -> String {
    let state = if fast_forward { "ON" } else { "OFF" };
    let how = match (ff_explicit, fast_forward) {
        (true, true) => "--ff on",
        (true, false) => "--ff off",
        (false, _) => "default",
    };
    format!("fast-forward: {state} ({how})")
}

fn main() {
    match dispatch() {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            dlog!("tdvmm: fatal: {err}");
            std::process::exit(1);
        }
    }
}

/// Parse the CLI and dispatch to a subcommand handler.
fn dispatch() -> Result<i32, Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    let no_progress = cli.no_progress;
    match cli.cmd {
        Cmd::Build(args) => build::cmd_build(build::BuildArgs {
            compose: args.compose,
            out: args.out,
            name: args.name,
            mem: args.mem,
            working_set: args.working_set,
            squash_threshold: args.squash_threshold,
            validate_only: args.validate_only,
            no_cache: args.no_cache,
            cache_dir: args.cache_dir,
            no_progress,
        }),
        Cmd::BuildAgent(args) => build::cmd_build_agent(build::BuildAgentArgs { out: args.out }),
        Cmd::BuildKernel(args) => build::cmd_build_kernel(build::BuildKernelArgs {
            out: args.out,
            cache_dir: args.cache_dir,
            force_build: args.force_build,
            record: args.record,
        }),
        Cmd::SeedBuild { config } => build::cmd_seed_build(&config),
        Cmd::AssembleInitramfs { config } => build::cmd_assemble_initramfs(&config),
        Cmd::EgressTestServer(args) => Ok(egress_test_server::run(&args.behavior, &args.args)),
        Cmd::Boot(args) => cmd_boot(args),
        Cmd::Doctor(args) => doctor::cmd_doctor(args.skip_downloads, no_progress),
        Cmd::Run(args) => cmd_run(args),
        Cmd::Inspect(a) => cmd_inspect(&a.artifact),
        Cmd::Verify(a) => cmd_verify(&a.artifact),
        Cmd::Ls(args) => cmd_ls(args),
        Cmd::DumpCpuid => {
            cpuid::dump_cpuid(&Kvm::new()?)?;
            Ok(0)
        }
    }
}

// ---- `tdvmm boot`: raw kernel + initramfs (low-level dev verb) ---------------

fn cmd_boot(args: BootArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let eff = EffectiveConfig::from_boot(&args.common)?;
    let (kernel_path, initrd_path) =
        build::resolve_boot_inputs(args.kernel.as_deref(), args.initrd.as_deref())?;
    let kernel = std::fs::read(&kernel_path)
        .map_err(|e| format!("opening kernel {}: {e}", kernel_path.display()))?;
    let initrd = std::fs::read(&initrd_path)
        .map_err(|e| format!("opening initrd {}: {e}", initrd_path.display()))?;
    dlog!(
        "[tdvmm] boot: kernel={} initrd={}",
        kernel_path.display(),
        initrd_path.display()
    );
    let out = boot_and_run(&kernel, &initrd, &eff, None)?;
    Ok(out.exit_code)
}

// ---- `tdvmm run`: a .tdvmm stack artifact (baked defaults + overrides) --------

fn cmd_run(args: RunArgs) -> Result<i32, Box<dyn std::error::Error>> {
    // Resolve a store NAME (e.g. `tigerbeetle`) or a path to a concrete file.
    let artifact_path = artifact::resolve(&args.artifact)?;

    // Load the artifact's members into memory (NO temp-dir extraction): manifest
    // + kernel + initramfs + compose.lock (the last for the on-load verify).
    let payload = artifact::read_for_run(&artifact_path)?;

    // Member-hash verify on load is DEFAULT-ON (`--no-verify` to skip): recompute
    // each payload member's sha256 and compare to the manifest, so a corrupted or
    // tampered artifact is caught before we boot it.
    if args.no_verify {
        dlog!("[tdvmm] run: member-hash verify SKIPPED (--no-verify)");
    } else {
        verify_payload_or_bail(&payload)?;
        dlog!(
            "[tdvmm] run: {} member hashes verified against manifest (identity {})",
            payload.manifest.members.len(),
            &artifact::file_sha256_hex(&artifact_path)?[..16],
        );
    }

    let eff = EffectiveConfig::from_run(&args.common, &payload.manifest.run_defaults)?;
    dlog!(
        "[tdvmm] run: stack={} project={} (format v{})",
        payload.manifest.stack,
        payload.manifest.project,
        payload.manifest.format_version,
    );

    // `--logs-dir` (secondary on `run`): pre-create + writability-check before
    // boot; fail fast (exit 2) if impossible. Services come from the artifact's
    // compose.lock. Capture happens on the graceful stop paths only.
    let logs = match args.logs_dir.as_deref() {
        Some(dir) => {
            let path = match prepare_logs_dir(dir) {
                Ok(p) => p,
                Err(e) => {
                    dlog!("[tdvmm] run: {e}");
                    return Ok(EXIT_INFRA);
                }
            };
            let svc = compose::lock_service_names(&payload.compose_lock).unwrap_or_default();
            Some(LogsCapture {
                dir: path,
                services: svc,
                project: payload.manifest.project.clone(),
            })
        }
        None => None,
    };

    // Wall-clock safety net (opt-in): a genuinely wedged guest that busy-loops
    // (never HLTs) is bounded by the virtual-time horizon in seconds of wall
    // time, but a hard hang — a stuck host ioctl, or a driver container that died
    // without calling `finish` — is caught here. This is the fallback the driver
    // model relies on instead of watching container lifecycles.
    let _watchdog = args.wall_timeout.filter(|s| *s > 0).map(spawn_wall_timeout);

    let out = boot_and_run(&payload.kernel, &payload.initramfs, &eff, logs)?;
    Ok(out.exit_code)
}

/// Arm the wall-clock safety timeout. Returns a guard whose drop disarms it, so
/// a run that finishes normally is never killed by a late timer. On expiry the
/// process exits [`EXIT_INFRA`] — the tool (or the test) wedged, which is not a
/// verdict about the stack.
fn spawn_wall_timeout(secs: u64) -> WallTimeout {
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = done.clone();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if !flag.load(std::sync::atomic::Ordering::Relaxed) {
            crate::log_line(format_args!(
                "[tdvmm] WALL-CLOCK TIMEOUT after {secs}s — aborting (exit {EXIT_INFRA})"
            ));
            std::process::exit(EXIT_INFRA);
        }
    });
    WallTimeout { done }
}

/// Disarms the wall-clock watchdog when the run ends.
struct WallTimeout {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for WallTimeout {
    fn drop(&mut self) {
        self.done.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The default-ON on-load integrity check for `run`: recompute the payload's
/// member hashes against the manifest and refuse to boot on any mismatch.
fn verify_payload_or_bail(payload: &artifact::RunPayload) -> Result<(), Box<dyn std::error::Error>> {
    payload.verify_members().map_err(|e| {
        format!("{e} — artifact is corrupt or tampered; refusing to boot (pass --no-verify to override)")
            .into()
    })
}

// ---- `tdvmm inspect`: print manifest.json (manifest member only) -------------

fn cmd_inspect(path: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let resolved = artifact::resolve(path)?;
    // Reads ONLY the first member (manifest.json) — never the big kernel/initramfs.
    let manifest = artifact::read_manifest(&resolved)?;
    let json = manifest.to_canonical_json()?;
    // manifest.json to stdout verbatim (a machine can pipe it to jq).
    use std::io::Write;
    std::io::stdout().write_all(&json)?;
    Ok(0)
}

// ---- `tdvmm verify`: member hashes vs manifest + the file identity -----------

fn cmd_verify(path: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let resolved = artifact::resolve(path)?;
    let report = artifact::verify(&resolved)?;
    // Identity first (always printed, even on failure — it names the file checked).
    println!("tdvmm-artifact: {}", resolved.display());
    println!("sha256 (identity): {}", report.file_sha256);
    for c in &report.checks {
        println!(
            "  {:<16} {}  {}",
            c.name,
            if c.ok { "OK  " } else { "FAIL" },
            if c.ok {
                c.actual.clone()
            } else {
                format!("expected {} got {}", c.expected, c.actual)
            }
        );
    }
    for name in &report.missing {
        println!("  {name:<16} MISSING (in manifest, absent from archive)");
    }
    if report.all_ok() {
        println!("VERIFY OK: all {} member hashes match the manifest", report.checks.len());
        Ok(0)
    } else {
        println!("VERIFY FAIL: member-hash mismatch or missing member");
        Ok(1)
    }
}

// ---- `tdvmm ls`: list the local artifact store -------------------------------

fn cmd_ls(args: LsArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let entries = artifact::list_store()?;
    if entries.is_empty() {
        println!(
            "no artifacts in {} (build one with `tdvmm build <name> <compose.yml>`)",
            artifact::store_dir().display()
        );
        return Ok(0);
    }
    // Stat-only by default: hashing every artifact reads hundreds of MB each, so
    // the sha256 identity is `--digest`-only.
    let name_w = entries.iter().map(|e| e.name.len()).max().unwrap_or(4).max(4);
    if args.digest {
        println!("{:<name_w$}  {:>8}  {:<16}  {}", "NAME", "SIZE", "MODIFIED (UTC)", "SHA256");
    } else {
        println!("{:<name_w$}  {:>8}  {}", "NAME", "SIZE", "MODIFIED (UTC)");
    }
    for e in &entries {
        let when = fmt_mtime(e.modified);
        if args.digest {
            let sha = artifact::file_sha256_hex(&e.path)?;
            println!(
                "{:<name_w$}  {:>8}  {when:<16}  {}",
                e.name,
                human_size(e.size),
                &sha[..12]
            );
        } else {
            println!("{:<name_w$}  {:>8}  {when}", e.name, human_size(e.size));
        }
    }
    Ok(0)
}

/// Compact human size (`362M`, `1.2G`) for the `ls` table.
fn human_size(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1}G", b / GIB)
    } else if b >= MIB {
        format!("{:.0}M", b / MIB)
    } else if b >= 1024.0 {
        format!("{:.0}K", b / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// Format a mtime as compact UTC `YYYY-MM-DD HH:MM` (dependency-free; the project
/// pulls in no date crate). Shares [`build::civil_from_days`] (Howard Hinnant's
/// algorithm) so the civil-date math lives in exactly one place.
fn fmt_mtime(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, mi) = (rem / 3600, (rem % 3600) / 60);
    let (y, m, d) = build::civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

// ============================================================================
// The shared boot path — used by BOTH `boot` (raw) and `run` (artifact).
// ============================================================================

/// Set up the VM from in-memory kernel + initramfs byte buffers and the resolved
/// [`EffectiveConfig`], then hand off to the vCPU loop. The kernel is parsed from
/// bytes and the initramfs written straight into guest RAM — no temp files.
fn boot_and_run(
    kernel: &[u8],
    initrd: &[u8],
    eff: &EffectiveConfig,
    logs: Option<LogsCapture>,
) -> Result<RunOutcome, Box<dyn std::error::Error>> {
    let mem_size = eff.mem_mib * 1024 * 1024;
    let kvm = Kvm::new()?;

    // Startup FF mode statement (item 1) + interactive banner. Always printed;
    // isatty gates ONLY the banner/advisory wording, never the FF decision.
    {
        let tty = serial::stdin_is_tty();
        let mode = ff_mode_statement(eff.fast_forward, eff.ff_explicit);
        if tty {
            dlog!(
                "[tdvmm] {mode} — quit the guest with `poweroff` or `reboot` \
                 (`exit` now gives a fresh shell)"
            );
        } else {
            dlog!("[tdvmm] {mode}");
        }
        if eff.fast_forward && tty {
            dlog!(
                "[tdvmm][WARN] fast-forward is ON at an interactive console — it \
                 races the guest clock and pins a host core; pass `--ff off` for \
                 real-time."
            );
        }
    }

    // The EFFECTIVE-CONFIG line (spec item 3): the resolved knobs with per-knob
    // provenance (baked < flag). This is the future record-log preamble — every
    // run emits it, so a harness/log can reconstruct exactly what was run and why.
    dlog!("[tdvmm] effective-config: {}", eff.provenance);

    // Egress mode statement — printed ONLY when opened (so a closed-world run's
    // startup output is byte-identical). Names the one enumerable channel + the
    // phase-gate contract, and warns that fast-forward is now conditional.
    if eff.allow_egress {
        dlog!(
            "[tdvmm] egress: ON — host-mediated SOCKS5h proxy over COM4/ttyS3 for \
             guest-initiated TCP; no NIC, no default route (the closed-world topology \
             is preserved — the only exit is this one channel). Fast-forward is \
             PHASE-GATED: virtual time advances 1:1 (real rate) whenever a connection \
             is open and only jumps once egress is quiescent. Wall-clock is the fixed \
             baked epoch and diverges per jump (TLS/token/time-sensitive flows may fail)."
        );
    }

    // --- VM ---
    let vm = kvm.create_vm()?;
    vm.set_tss_address(arch::KVM_TSS_ADDRESS as usize)?;

    // --- Guest memory ---
    let guest_mem = memory::create_guest_memory(mem_size as usize)?;
    memory::register_with_kvm(&vm, &guest_mem)?;

    // No in-kernel irqchip and no in-kernel PIT: the userspace LAPIC + IOAPIC we
    // own serve all interrupt state, and the LAPIC one-shot/periodic timer (MMIO)
    // is the tick. (We deliberately do NOT route IA32_TSC_DEADLINE via an MSR
    // filter: on this host a KVM WRMSR fastpath no-ops 0x6E0 before the filter
    // when there is no in-kernel LAPIC, so TSC-deadline is unadvertised in CPUID.)

    // --- vCPU ---
    let vcpu = vm.create_vcpu(0)?;

    // CPUID: mask kvmclock/MWAIT/x2APIC/TSC-deadline, expose invariant TSC, pass
    // through the frequency leaves (0x15/0x16).
    let supported = kvm.get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)?;
    let filtered = cpuid::filter_cpuid(&supported)?;
    vcpu.set_cpuid2(&filtered)?;

    // Boot MSRs (sets the guest's initial IA32_TSC, hence the TSC offset).
    vcpu.set_msrs(&msrs::boot_msrs()?)?;

    // --- Virtual clock authority (read the TSC offset + freq once) ---
    let clock = VirtualClock::from_vcpu(&vcpu)
        .map_err(|e| format!("virtual clock unavailable (need kernel >= 5.16): {e}"))?;
    {
        let a = clock.vtsc_now();
        let b = clock.vtsc_now();
        assert!(b >= a, "vtsc went backwards ({a} -> {b}) — clock is not sane");
        dlog!(
            "[tdvmm] virtual clock: tsc_khz={} (~{} MHz) tsc_offset={} vtsc_now={}",
            clock.freq().khz(),
            clock.freq().hz() / 1_000_000,
            clock.tsc_offset(),
            b,
        );
    }

    // --- Load kernel + initrd (straight from the in-memory buffers) ---
    let entry = boot::load_kernel(&guest_mem, kernel)?;
    let initrd_cfg = boot::load_initrd(&guest_mem, initrd, mem_size)?;
    dlog!(
        "[tdvmm] vmlinux entry {:#x}, initramfs {} bytes @ {:#x}",
        entry.0, initrd_cfg.size, initrd_cfg.address
    );

    // --- vCPU registers / segments / page tables ---
    regs::setup_fpu(&vcpu)?;
    regs::setup_regs(&vcpu, entry.0)?;
    regs::setup_sregs(&guest_mem, &vcpu)?;
    // (LINT routing is handled by the userspace LAPIC, which holds LINT0/1 as
    // register storage; no in-kernel LAPIC to program.)

    // --- System config (cmdline, MPTable, E820, zero page) ---
    // Append ` tdvmm.egress=1` to the EFFECTIVE cmdline only when egress is opened,
    // so the guest init starts the forwarder; off, the cmdline is untouched (INV-E0).
    let cmdline = if eff.allow_egress {
        format!("{} tdvmm.egress=1", eff.cmdline)
    } else {
        eff.cmdline.clone()
    };
    boot::configure_system(&guest_mem, &cmdline, Some(initrd_cfg), mem_size, 1)?;

    // --- Serial console ---
    // Under `--logs-dir`, COM1 output is teed into this shared buffer so the
    // console scanner can demux it per service. `None` = plain stdout passthrough.
    let console_tee: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> =
        logs.as_ref().map(|_| std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
    let (serial, serial_drain) = serial::new_serial(console_tee.clone())?;
    let raw_term = serial::RawTerminal::enable(0);
    // From here our own log lines must snap to column 0 with CRLF (raw mode turns
    // off the tty's ONLCR). Flip the flag only if raw mode actually took effect
    // (a tty); the cooked-mode boot lines above already printed with a plain "\n".
    if raw_term.is_raw() {
        RAW_TTY.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    run_user_backend(
        vcpu,
        serial,
        serial_drain,
        clock,
        eff.fast_forward,
        eff.max_jump_secs,
        eff.max_virtual_time_secs,
        eff.allow_egress,
        eff.metrics_out.clone(),
        logs,
        console_tee,
    )
}

/// Queue an interrupt `vector` for injection on the next KVM entry
/// (`KVM_INTERRUPT`). Userspace-irqchip only.
fn inject_interrupt(vcpu: &VcpuFd, vector: u8) -> std::io::Result<()> {
    let irq = kvm_interrupt {
        irq: u32::from(vector),
    };
    // SAFETY: valid vCPU fd; KVM_INTERRUPT reads the kvm_interrupt struct and
    // writes nothing back.
    let ret = unsafe { ioctl_with_ref(vcpu, KVM_INTERRUPT(), &irq) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

// =====================================================================
// The vCPU run loop (userspace LAPIC/IOAPIC we own)
// =====================================================================

#[allow(clippy::too_many_arguments)]
fn run_user_backend(
    mut vcpu: VcpuFd,
    serial: serial::SharedSerial,
    serial_drain: serial::EventFdTrigger,
    clock: VirtualClock,
    fast_forward: bool,
    max_jump_secs: f64,
    max_virtual_time_secs: Option<f64>,
    allow_egress: bool,
    metrics_out: Option<String>,
    logs: Option<LogsCapture>,
    console_tee: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
) -> Result<RunOutcome, Box<dyn std::error::Error>> {
    // The devices we now own, all on this thread. The LAPIC timer counts at the
    // core-crystal frequency the guest derives from CPUID 0x15 (which we pass
    // through). counts->TSC-cycles uses the EXACT CPUID-0x15 integer ratio
    // EBX/EAX (see apic_timer_tsc_ratio) — no float — so the tick fires at the
    // correct virtual time, bit-identically every run.
    let apic_bus_hz = apic_bus_hz_from_cpuid();
    let (ratio_num, ratio_den) = apic_timer_tsc_ratio(clock.freq().hz());
    dlog!(
        "[tdvmm] userspace LAPIC timer: {ratio_num}/{ratio_den} TSC cycles/count \
         (CPUID 0x15 EBX/EAX), crystal ~{} MHz",
        apic_bus_hz / 1_000_000
    );
    // clock is cloned into the LAPIC and PIT; all clones share the offset cell,
    // so a fast-forward bump (below) moves every consumer's view at once.
    let mut lapic = Lapic::new(clock.clone(), ratio_num, ratio_den);
    let mut ioapic = Ioapic::new(mptable_ioapic_id());
    let mut pic = PicStub::new();
    // PIT stub: interrupt-silent calibration/counter backstop (serves 0x40-0x43
    // + ELCR now that the in-kernel PIT is gone).
    let mut pit = PitStub::new(clock.clone());
    // The one event queue: mirrors the LAPIC's single armed deadline so the park
    // knows when to wake. No timer state lives outside it + the LAPIC.
    let mut events: events::EventQueue<TimerKind> = events::EventQueue::new();
    let mut parker = park::Parker::new()?;

    // Test control channel: the 2nd 16550 (COM2 / ttyS1). Always present so the
    // guest agent's ttyS1 always works; the scenario engine is what is optional.
    let mut com2 = control::ControlChannel::new()?;
    // Egress transport: COM4 / ttyS3, instantiated ONLY under `--allow-egress`.
    // When off this is `None`, the PIO dispatch never touches 0x2e8 (open bus),
    // and the phase gate/park never see an egress fd — byte-identical to the
    // closed world (INV-E0). When on, it owns the host-side backend + the COM4
    // UART; the phase gate below holds fast-forward at real rate while any session
    // is live, and the always-on assert guards every jump.
    let mut egress: Option<egress::EgressChannel> = if allow_egress {
        dlog!(
            "[tdvmm] egress: COM4/ttyS3 @ {:#x} IRQ{} (shared with COM2) instantiated; \
             fast-forward is phase-gated while a connection is open",
            arch::SERIAL4_PORT_BASE,
            arch::SERIAL4_IRQ
        );
        Some(egress::EgressChannel::new()?)
    } else {
        None
    };
    // The driver-run state: what containers do over the guest control socket. It
    // is always present and always inert until a container actually drives — a
    // plain `run` never leaves the default, so its behavior is unchanged.
    let mut driver = driver::DriverRun::default();

    // Fast-forward state: the jump-cost/speedup accounting + the
    // single-jump sanity bound. `None` when FF is off (the real-wait park).
    let mut ff_state = if fast_forward {
        Some(FfState::new(clock.freq().hz(), max_jump_secs))
    } else {
        None
    };

    // Interactive console: a human at a tty with no harness context (no metrics
    // sink and no virtual-time horizon). The periodic HLT-rate / fast-forward
    // rollup below is a perf metric for demo + harness runs, NOT interactive
    // console noise — so when interactive we suppress its PERIODIC emission (it
    // would interrupt a human's prompt every ~15s). isatty gates ONLY this
    // suppression, never any time behavior (Fable-locked); it is still emitted for
    // every harness path (non-tty, `--metrics-out`, or a horizon set), and the
    // on-stop summary + metrics file are unaffected either way.
    let interactive =
        serial::stdin_is_tty() && metrics_out.is_none() && max_virtual_time_secs.is_none();

    // Idle-observability (a hop-cost input, not a diagnostic): how often
    // the guest HLTs. Reported on stop, and rolled up every ~15s of wall time so
    // it is observable during long-running workloads that never exit.
    let mut hlt_count: u64 = 0;
    let start = std::time::Instant::now();
    let mut last_report = start;
    let mut last_report_hlts: u64 = 0;
    const HLT_REPORT_PERIOD: std::time::Duration = std::time::Duration::from_secs(15);

    // Virtual-time span for the speedup metric (gate 2): virtual seconds elapsed
    // / real seconds elapsed. Sampled once here, again at stop.
    let vtsc_start = clock.vtsc_now();

    // Console scanner (`--logs-dir`): demux the teed COM1 stream into per-service
    // files live as the run proceeds, so crash logs survive. Passive + FF-neutral.
    let mut conscan = match (&logs, &console_tee) {
        (Some(l), Some(tee)) => Some(conscan::ConsoleScan::new(
            tee.clone(),
            l.dir.clone(),
            &l.project,
            l.services.clone(),
            vtsc_start,
            clock.freq(),
        )),
        _ => None,
    };

    // `--max-virtual-time` horizon: the vtsc at which the run must stop, as an
    // absolute deadline `vtsc_start + budget`. Enforced NOT as a loop check but
    // as a `(vtsc, StopRun)` entry pushed into the one event queue each boundary
    // (see `service_timers`); when vtsc reaches it, `pop_due` fires StopRun and
    // the run terminates through the same drain path a timer does. This makes the
    // horizon a pure function of vtsc. A wedged guest fast-forwards to any sane
    // horizon in seconds of real time; a legitimate long idle also reaches it,
    // which is correct — a run has a bounded virtual duration.
    let horizon_vtsc: Option<u64> = max_virtual_time_secs.map(|secs| {
        let cycles = (secs * clock.freq().hz() as f64) as u64;
        vtsc_start.wrapping_add(cycles)
    });
    if let Some(h) = horizon_vtsc {
        dlog!(
            "[tdvmm] max-virtual-time horizon: {:.3}s of virtual time \
             (vtsc {vtsc_start} -> {h}), as a (vtsc, StopRun) queue event",
            max_virtual_time_secs.unwrap(),
        );
    }

    // Why we stopped; set at every break site (all breaks assign it), read after
    // the loop to pick the process exit code.
    let stop_reason: StopReason;

    dlog!(
        "[tdvmm] starting vCPU on the USERSPACE irqchip, fast-forward {} \
         (Ctrl-A is passed to the guest; kill from another terminal to stop)\n",
        if fast_forward { "ON" } else { "OFF" }
    );

    // Wedge observability (opt-in: TDVMM_WEDGE_SECS): a watchdog that dumps the
    // vCPU's exit histogram + guest RIP/interrupt state when the guest makes no
    // console/HLT progress — the tool for a guest that hard-spins with no output.
    let diag = diag::Diag::from_env();
    diag.note_tid();
    std::sync::Arc::clone(&diag).spawn_watchdog();

    // Tick doorbell: lets an armed virtual-time deadline preempt an exit-free
    // KVM_RUN — Fable's fix for the container-start wedge (armed timers must be
    // able to interrupt a guest that runs a full tick with no exit). ON by
    // default; TDVMM_NO_DOORBELL=1 disables it for A/B.
    let mut doorbell = doorbell::Doorbell::new(&mut vcpu);

    loop {
        // Doorbell hygiene: clear immediate_exit BEFORE service_timers, so any
        // fire from here on re-sets it and the coming KVM_RUN bails (no lost
        // wakeup). Also service a watchdog dump requested without an EINTR — the
        // livelock case, where the SIGUSR1 kick is consumed in userspace and the
        // EINTR arm below never runs (diag guardrail).
        doorbell.clear(&mut vcpu);
        maybe_dump_wedge(&diag, &mut vcpu, &clock, &events, &lapic, &ioapic);

        // (1) Fire any due guest timer + the horizon, then reconcile the queue to
        //     the LAPIC's current armed deadline. A fired StopRun stops the run.
        let now = clock.vtsc_now();
        let fired = service_timers(&mut lapic, &mut events, horizon_vtsc, now);
        if fired.horizon {
            stop_reason = StopReason::Horizon;
            report_horizon();
            break;
        }

        // (1b) Drain the agent's lines off ttyS1 and fold them into the driver
        //      state: the control-socket fault trace, and the terminal `finish`
        //      that ends the run with a verdict. A run nobody drives simply
        //      discards the agent's chatter, keeping the capture buffer bounded.
        com2.pump(&mut lapic, &ioapic);
        // Egress boundary pump (beside COM2): absorb guest mux bytes, run sockets/
        // resolves, feed the COM4 FIFO + raise IRQ3. Same single-writer discipline
        // as COM2. A framing error is a guest protocol violation → abort the run.
        if let Some(eg) = egress.as_mut() {
            eg.pump(&mut lapic, &ioapic)?;
        }
        let mut finished = false;
        while let Some(line) = com2.poll_line() {
            if driver.on_agent_line(&line, virtual_secs(&clock, vtsc_start)) {
                finished = true;
                break;
            }
        }
        if finished {
            stop_reason = StopReason::DriverFinish;
            break;
        }

        // (1c) Demux the teed COM1 console into per-service log files (--logs-dir).
        //      Passive: reads bytes the guest already emitted; no guest state, no
        //      clock, no queue touched — so it adds zero wakes and is FF-neutral.
        if let Some(sc) = conscan.as_mut() {
            sc.drain(clock.vtsc_now());
        }

        // (2) Sync task priority from the guest's CR8 (mov %cr8 path).
        let cr8 = vcpu.get_kvm_run().cr8;
        lapic.sync_tpr_from_cr8(cr8);

        // (3) Injection: if the LAPIC has a deliverable vector and the guest can
        //     take it now, hand it to KVM; otherwise request an IRQ window.
        let deliverable = lapic.deliverable_vector();
        let (ready, if_flag) = {
            let r = vcpu.get_kvm_run();
            (r.ready_for_interrupt_injection, r.if_flag)
        };
        let mut injected = false;
        if let Some(vec) = deliverable {
            if ready != 0 && if_flag != 0 {
                inject_interrupt(&vcpu, vec)?;
                lapic.ack_injected(vec);
                injected = true;
            }
        }
        {
            let r = vcpu.get_kvm_run();
            r.request_interrupt_window = u8::from(deliverable.is_some() && !injected);
            r.cr8 = u64::from(lapic.tpr() >> 4);
        }

        // (4) Run. Arm the doorbell to the earliest pending deadline first, so an
        // exit-free guest is broken out in time to service it (else its tick never
        // fires, jiffies freeze, and the single vCPU can never be preempted). Fold
        // in the LAPIC's live deadline (see min_deadline): a tick that fired in
        // service_timers this boundary re-armed the LAPIC but is not in the queue
        // until the next boundary, so peek_deadline() alone would briefly disarm.
        doorbell.arm(min_deadline(events.peek_deadline(), lapic.timer_deadline()), &clock);
        let exit = match vcpu.run() {
            Ok(exit) => exit,
            Err(err) => {
                let e = err.errno();
                if e == libc::EINTR {
                    // A signal broke KVM_RUN — a doorbell tick or a watchdog kick.
                    // Count it; service a requested state dump here on the vCPU
                    // thread (KVM reads stay single-writer).
                    diag.note_eintr();
                    maybe_dump_wedge(&diag, &mut vcpu, &clock, &events, &lapic, &ioapic);
                    continue;
                }
                if e == libc::EAGAIN {
                    continue;
                }
                return Err(format!("KVM_RUN failed: {err}").into());
            }
        };

        // Wedge observability: bucket every exit (a no-op unless TDVMM_WEDGE_SECS).
        diag.record(&exit);

        // (5) Handle the exit.
        match exit {
            VcpuExit::IoOut(port, data) => {
                if serial::is_serial(port) {
                    diag.note_console_out(data.len() as u64);
                    let mut s = serial.lock().unwrap();
                    for &b in data {
                        let _ = s.write((port - arch::SERIAL_PORT_BASE) as u8, b);
                    }
                    drop(s);
                    if serial_drain.drain().is_ok() {
                        raise_irq(&mut lapic, &ioapic, arch::SERIAL_IRQ);
                    }
                } else if control::ControlChannel::handles(port) {
                    // COM2 / ttyS1 — the control channel (agent TX = its replies).
                    for &b in data {
                        com2.pio_write(port, b, &mut lapic, &ioapic);
                    }
                } else if let Some(eg) =
                    egress.as_mut().filter(|_| egress::EgressChannel::handles(port))
                {
                    // COM4 / ttyS3 — the guest forwarder writing a mux frame. This
                    // arm is reached only when `--allow-egress` instantiated the
                    // channel AND the port is COM4's (else the chain falls through).
                    for &b in data {
                        eg.pio_write(port, b, &mut lapic, &ioapic);
                    }
                } else if PitStub::handles(port) {
                    for &b in data {
                        pit.write(port, b);
                    }
                } else if PicStub::handles(port) {
                    for &b in data {
                        pic.write(port, b);
                    }
                } else if port == arch::POST_PORT {
                    // POST checkpoint code: ignore.
                }
            }
            VcpuExit::IoIn(port, data) => {
                if serial::is_serial(port) {
                    let mut s = serial.lock().unwrap();
                    for b in data.iter_mut() {
                        *b = s.read((port - arch::SERIAL_PORT_BASE) as u8);
                    }
                    drop(s);
                    if serial_drain.drain().is_ok() {
                        raise_irq(&mut lapic, &ioapic, arch::SERIAL_IRQ);
                    }
                } else if control::ControlChannel::handles(port) {
                    // COM2 / ttyS1 — the agent reading a command (RBR) or a status
                    // register. Draining the RX FIFO here frees room for `pump`.
                    for b in data.iter_mut() {
                        *b = com2.pio_read(port, &mut lapic, &ioapic);
                    }
                } else if let Some(eg) =
                    egress.as_mut().filter(|_| egress::EgressChannel::handles(port))
                {
                    // COM4 / ttyS3 — the guest forwarder reading a mux frame (RBR)
                    // or a status/autoconfig register.
                    for b in data.iter_mut() {
                        *b = eg.pio_read(port, &mut lapic, &ioapic);
                    }
                } else if PitStub::handles(port) {
                    for b in data.iter_mut() {
                        *b = pit.read(port);
                    }
                } else if PicStub::handles(port) {
                    for b in data.iter_mut() {
                        *b = pic.read(port);
                    }
                } else {
                    for b in data.iter_mut() {
                        *b = 0xff; // open bus
                    }
                }
            }
            VcpuExit::MmioRead(addr, data) => {
                let val = if in_lapic(addr) {
                    lapic.mmio_read((addr - XAPIC_BASE) as u32)
                } else if Ioapic::handles(addr) {
                    ioapic.mmio_read(addr)
                } else {
                    0
                };
                util::write_u32_le(data, val);
            }
            VcpuExit::MmioWrite(addr, data) => {
                let val = util::read_u32_le(data);
                if in_lapic(addr) {
                    lapic.mmio_write((addr - XAPIC_BASE) as u32, val);
                } else if Ioapic::handles(addr) {
                    ioapic.mmio_write(addr, val);
                }
            }
            VcpuExit::IrqWindowOpen => {
                // The window is open; the next loop iteration injects.
            }
            VcpuExit::Hlt => {
                // Flush console logs before a (possibly hour-long, FF-off) park, so
                // `tail -f <logs-dir>/<svc>.log` stays live and a line printed just
                // before the halt is on disk before we sleep.
                if let Some(sc) = conscan.as_mut() {
                    sc.drain(clock.vtsc_now());
                }
                // A HLT taken with interrupts disabled (IF=0) can NEVER wake: no
                // interrupt is deliverable, so it is a terminal halt — where the
                // guest's `poweroff` ends when there is no ACPI (the kernel
                // finishes in `cli; hlt`, "System halted"). Recognize it as a
                // clean guest-terminal stop (status 0), distinct from an ordinary
                // idle `sti; hlt` (IF=1) which parks and waits/jumps for its next
                // timer. Checked here, before the park, in BOTH FF modes.
                if vcpu.get_kvm_run().if_flag == 0 {
                    dlog!(
                        "\n[tdvmm] STOP: guest halted (power off) — HLT with \
                         interrupts disabled (IF=0), a terminal halt that can \
                         never wake."
                    );
                    stop_reason = StopReason::GuestHalt;
                    break;
                }
                hlt_count += 1;
                // Periodic rollup: kept for harness/metrics/horizon runs; suppressed
                // when interactive (Task 4) so it never interrupts a human's prompt.
                if !interactive && last_report.elapsed() >= HLT_REPORT_PERIOD {
                    let win = last_report.elapsed().as_secs_f64();
                    let n = hlt_count - last_report_hlts;
                    dlog!(
                        "[tdvmm] HLT-exit rate: {:.1}/s ({n} in {win:.0}s; {hlt_count} total)",
                        n as f64 / win
                    );
                    if let Some(ff) = ff_state.as_ref() {
                        let virt_s = ff.virtual_secs_since(vtsc_start, clock.vtsc_now());
                        let real_s = start.elapsed().as_secs_f64().max(1e-9);
                        dlog!(
                            "[tdvmm] fast-forward: {} jumps, {:.0} virtual-s in {:.1} real-s \
                             = {:.0}x; per-hop mean {:.1}us max {:.1}us; max Δ {:.3}s",
                            ff.jumps,
                            virt_s,
                            real_s,
                            virt_s / real_s,
                            ff.mean_hop_ns() as f64 / 1000.0,
                            ff.hop_ns_max as f64 / 1000.0,
                            ff.max_delta_secs(),
                        );
                    }
                    last_report = std::time::Instant::now();
                    last_report_hlts = hlt_count;
                }
                let outcome = park_until_deliverable(
                    &mut lapic,
                    &ioapic,
                    &mut events,
                    &serial,
                    &serial_drain,
                    &mut parker,
                    &clock,
                    &vcpu,
                    horizon_vtsc,
                    ff_state.as_mut(),
                    &mut com2,
                    egress.as_mut(),
                    &mut driver,
                    vtsc_start,
                )?;
                match outcome {
                    ParkOutcome::Horizon => {
                        stop_reason = StopReason::Horizon;
                        report_horizon();
                        break;
                    }
                    ParkOutcome::DriverFinish => {
                        stop_reason = StopReason::DriverFinish;
                        break;
                    }
                    ParkOutcome::Deliverable => {}
                }
            }
            VcpuExit::Shutdown => {
                // Guest-initiated: a triple fault (crash, or panic/`reboot=t`).
                // Distinct from the VMM's own horizon stop below — a testing
                // platform wants "guest panicked/rebooted" as a first-class
                // outcome (see StopReason -> exit code).
                dlog!(
                    "\n[tdvmm] STOP: guest-initiated shutdown/reboot \
                     (KVM_EXIT_SHUTDOWN, triple fault — e.g. panic+reboot or `reboot -f`)."
                );
                stop_reason = StopReason::GuestShutdown;
                break;
            }
            VcpuExit::SystemEvent(type_, _) => {
                // Guest-initiated reset/shutdown/crash via a system event.
                dlog!(
                    "\n[tdvmm] STOP: guest-initiated system event \
                     (reset/shutdown/crash, type {type_})."
                );
                stop_reason = StopReason::GuestSystemEvent;
                break;
            }
            VcpuExit::FailEntry(reason, cpu) => {
                return Err(
                    format!("KVM_EXIT_FAIL_ENTRY: reason={reason:#x} cpu={cpu}").into(),
                );
            }
            VcpuExit::InternalError => return Err("KVM_EXIT_INTERNAL_ERROR".into()),
            other => {
                dlog!("[tdvmm] unhandled KVM exit: {other:?}");
            }
        }
    }

    // The vCPU loop has ended; silence the wedge watchdog so post-run log capture
    // (idle by design) isn't misread as a stall. The doorbell stays LIVE: capture
    // drives the still-running guest through KVM_RUN and needs the same tick
    // preemption (see drive_until_reply). It is torn down on Doorbell::drop.
    diag.stop();

    let secs = start.elapsed().as_secs_f64();
    // Egress counters snapshot for the reports (None when `--allow-egress` is off,
    // so the summary/metrics keep their closed-world shape — INV-E0).
    let egress_stats = egress.as_ref().map(egress::EgressChannel::stats);
    if let Some(ff) = ff_state.as_ref() {
        dlog!(
            "{}",
            ff.human_summary(stop_reason, vtsc_start, clock.vtsc_now(), secs, hlt_count, egress_stats)
        );

        // Machine-parseable per-run metrics for the comparison harness.
        if let Some(path) = metrics_out.as_deref() {
            let report =
                ff.metrics_report(stop_reason, vtsc_start, clock.vtsc_now(), secs, hlt_count, egress_stats);
            match std::fs::write(path, report + &driver.metrics_block()) {
                Ok(()) => dlog!("[tdvmm] wrote per-run metrics to {path}"),
                Err(e) => dlog!("[tdvmm][WARN] could not write --metrics-out {path}: {e}"),
            }
        }
    } else {
        dlog!(
            "[tdvmm] backend stopped: {hlt_count} HLT exits over {secs:.1}s ({:.1}/s)",
            hlt_count as f64 / secs.max(0.001)
        );
        // Egress under `--ff off` runs at real rate throughout (no gate), but still
        // report its counters when present so the observability is symmetric.
        if let Some(eg) = egress_stats {
            dlog!(
                "[tdvmm] egress: {} session(s), {} open failure(s), {}B up / {}B down",
                eg.sessions_total, eg.opens_failed, eg.bytes_up, eg.bytes_down
            );
        }
        // FF off: no jumps to account for; leave a clear stub so a harness that
        // always passes --metrics-out gets a well-formed, unambiguous file.
        if let Some(path) = metrics_out.as_deref() {
            let _ = std::fs::write(
                path,
                format!(
                    "# tdvmm per-run metrics (fast-forward OFF — no jump accounting)\n\
                     schema 1\nstop_reason {}\nfast_forward off\n",
                    stop_reason.as_str()
                ),
            );
        }
    }

    // ---- Opt-in per-service log capture (--logs-dir) ---------------------------
    // Post-verdict / end-of-run ONLY, on THIS (vCPU/control) thread — never a
    // background thread, never concurrent with the run — driving the still-alive
    // guest to page each container's k8s-file log out via the agent's `logs` op.
    // Runs only on graceful stops (a driver verdict, or the horizon) where the
    // guest is still responsive; a guest-death stop is skipped with one warning.
    // Every capture error warns + skips — it never changes the verdict, exit
    // code, JSONL, or report.
    if let Some(cap) = logs.as_ref() {
        if matches!(stop_reason, StopReason::DriverFinish | StopReason::Horizon) {
            capture_logs(
                &mut vcpu,
                &mut doorbell,
                &serial,
                &serial_drain,
                &clock,
                &mut lapic,
                &mut ioapic,
                &mut pit,
                &mut pic,
                &mut events,
                &mut parker,
                ff_state.as_mut(),
                &mut com2,
                egress.as_mut(),
                cap,
            );
        } else {
            dlog!(
                "[tdvmm][WARN] --logs-dir: guest stopped ({}) before logs could be \
                 pulled — skipping log capture",
                stop_reason.as_str()
            );
        }
    }

    // Flush any remaining teed console bytes (including those emitted during the
    // capture above) plus the trailing partial line. On a guest-death stop this is
    // the ONLY per-service output — the headline win: crash logs survive.
    if let Some(sc) = conscan.as_mut() {
        sc.finish(clock.vtsc_now()); // finish() drains first, then flushes the trailing partial
    }

    // ---- The verdict --------------------------------------------------------
    // A run some container drove prints its summary and returns THAT verdict's
    // code; a run nobody drove is byte-identically the `run` of before and returns
    // the stop reason's code.
    if let Some(line) = driver.summary() {
        dlog!("{}", line);
    }
    let exit_code = match driver.verdict() {
        Some(v) => v.exit_code(),
        None => stop_reason.exit_code(),
    };
    Ok(RunOutcome { stop: stop_reason, exit_code })
}

/// Announce a `--max-virtual-time` horizon stop at the moment it fires, so the log
/// marks the transition. The full jump/timing breakdown and the Δvtsc histogram
/// are in the end-of-run summary that follows.
fn report_horizon() {
    dlog!(
        "\n[tdvmm] stopping: --max-virtual-time horizon reached \
         (a deterministic (vtsc, StopRun) queue event, not a guest-initiated stop)."
    );
}

/// Reconcile the event queue to the LAPIC's single armed deadline (the LAPIC is
/// the authority) plus the optional virtual-time `horizon`, then fire everything
/// due through the queue. Keeping the fire path in the queue is the whole point
/// of `events.rs`: every guest timer — and the `--max-virtual-time` horizon — is
/// a `(vtsc, event)` entry, and fast-forward drains the same queue after a time-jump.
///
/// Which special (non-LAPIC) queue events fired this drain: today only the
/// `--max-virtual-time` StopRun.
#[derive(Default, Clone, Copy)]
struct Fired {
    horizon: bool,
}

/// The earlier of the event queue's next deadline and the LAPIC's live timer
/// deadline. Folding in the LAPIC deadline covers the one-boundary window right
/// after a periodic tick fires: `fire_timer_if_due` re-arms the LAPIC immediately,
/// but that new deadline is only mirrored into the queue at the NEXT boundary — so
/// `peek_deadline()` alone would briefly disarm the doorbell (and make a wedge
/// dump misreport "none armed"). Used for arming the doorbell and for the dumps.
fn min_deadline(queue: Option<u64>, lapic: Option<u64>) -> Option<u64> {
    match (queue, lapic) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Service a watchdog-requested wedge dump (a no-op unless one is pending). Reads
/// the guest state ON the vCPU thread, so the KVM ioctls stay single-writer.
fn maybe_dump_wedge(
    diag: &diag::Diag,
    vcpu: &mut VcpuFd,
    clock: &VirtualClock,
    events: &events::EventQueue<TimerKind>,
    lapic: &Lapic,
    ioapic: &Ioapic,
) {
    if diag.take_dump_request() {
        diag.dump_guest(
            vcpu,
            clock.vtsc_now(),
            min_deadline(events.peek_deadline(), lapic.timer_deadline()),
            &lapic.diag_str(),
            &ioapic.diag_str(),
        );
    }
}

fn service_timers(
    lapic: &mut Lapic,
    events: &mut events::EventQueue<TimerKind>,
    horizon: Option<u64>,
    now: u64,
) -> Fired {
    events.clear();
    if let Some(dl) = lapic.timer_deadline() {
        events.push(dl, TimerKind::LapicDeadline);
    }
    if let Some(h) = horizon {
        events.push(h, TimerKind::StopRun);
    }
    let mut fired = Fired::default();
    while let Some(ev) = events.pop_due(now) {
        // Queue-discipline assertion (gate 5): an event is only ever serviced at
        // or after its scheduled vtsc — never before. Always-on (release too).
        assert!(
            ev.deadline <= now,
            "queue discipline violated: event deadline {} fired at now {}",
            ev.deadline,
            now
        );
        match ev.payload {
            TimerKind::LapicDeadline => {
                lapic.fire_timer_if_due(now);
            }
            TimerKind::StopRun => {
                fired.horizon = true;
            }
        }
    }
    fired
}

/// How a park returned: an interrupt became deliverable (wake the guest), the
/// `--max-virtual-time` horizon fired (stop), or a container finished the run
/// while parked (stop).
enum ParkOutcome {
    Deliverable,
    Horizon,
    DriverFinish,
}

/// Idle park: the guest HLTed, so make it wait until an interrupt becomes
/// deliverable. This is the one place that turns a virtual-time deadline into
/// either a real wait (FF off) or a fast-forward JUMP (FF on).
/// Console input is read here (the vCPU thread owns it). The wake path itself —
/// IRR set, deliverable_vector, the caller's injection — is unchanged either way.
///
/// The `horizon` rides along so the FF jump target respects it and a horizon
/// reached while parked returns [`ParkOutcome::Horizon`] instead of spinning.
#[allow(clippy::too_many_arguments)]
fn park_until_deliverable(
    lapic: &mut Lapic,
    ioapic: &Ioapic,
    events: &mut events::EventQueue<TimerKind>,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    parker: &mut park::Parker,
    clock: &VirtualClock,
    vcpu: &VcpuFd,
    horizon: Option<u64>,
    ff: Option<&mut FfState>,
    com2: &mut control::ControlChannel,
    egress: Option<&mut egress::EgressChannel>,
    driver: &mut driver::DriverRun,
    vtsc_start: u64,
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    match ff {
        Some(ff) => fast_forward_until_deliverable(
            lapic, ioapic, events, serial, serial_drain, parker, clock, vcpu, horizon, ff, com2,
            egress, driver, vtsc_start,
        ),
        None => real_wait_until_deliverable(
            lapic, ioapic, events, serial, serial_drain, parker, clock, horizon, com2, egress,
            driver, vtsc_start,
        ),
    }
}

/// Drain the agent's ttyS1 lines from inside the park and fold them into the
/// driver state. `Some(DriverFinish)` means a container ended the run. Draining
/// here ends the run the moment the verdict lands rather than a guest tick later.
/// Passive (reads already-emitted bytes, touches no clock or queue), so
/// FF-neutral.
fn park_drain_agent(
    clock: &VirtualClock,
    vtsc_start: u64,
    com2: &mut control::ControlChannel,
    driver: &mut driver::DriverRun,
) -> Option<ParkOutcome> {
    while let Some(line) = com2.poll_line() {
        if driver.on_agent_line(&line, virtual_secs(clock, vtsc_start)) {
            return Some(ParkOutcome::DriverFinish);
        }
    }
    None
}

/// Virtual seconds elapsed since the run started — the timestamp on the driver's
/// fault trace, and the only clock that means anything to a fast-forwarded run.
fn virtual_secs(clock: &VirtualClock, vtsc_start: u64) -> f64 {
    let now = clock.vtsc_now();
    clock.freq().cycles_to_ns(now.saturating_sub(vtsc_start)) as f64 / 1e9
}

/// FF OFF: the real-wait park — sleep in `ppoll` on a `timerfd` + stdin until
/// the next deadline elapses in REAL time or console input arrives. If the next
/// deadline is the horizon, the wait elapses to it and StopRun fires (a
/// legitimate long idle reaching the virtual-time budget — correct).
#[allow(clippy::too_many_arguments)]
fn real_wait_until_deliverable(
    lapic: &mut Lapic,
    ioapic: &Ioapic,
    events: &mut events::EventQueue<TimerKind>,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    parker: &mut park::Parker,
    clock: &VirtualClock,
    horizon: Option<u64>,
    com2: &mut control::ControlChannel,
    mut egress: Option<&mut egress::EgressChannel>,
    driver: &mut driver::DriverRun,
    vtsc_start: u64,
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    loop {
        let now = clock.vtsc_now();
        let fired = service_timers(lapic, events, horizon, now);
        if fired.horizon {
            return Ok(ParkOutcome::Horizon);
        }
        if let Some(o) = park_drain_agent(clock, vtsc_start, com2, driver) {
            return Ok(o);
        }
        // Egress boundary pump: service sockets/resolves and feed COM4 + raise IRQ3
        // BEFORE the deliverable check, so a just-delivered egress frame wakes the
        // guest through the same unchanged deliverable path. FF is off here, so
        // there is no jump to gate — egress simply runs at real rate.
        if let Some(eg) = egress.as_deref_mut() {
            eg.pump(lapic, ioapic)?;
        }
        if lapic.deliverable_vector().is_some() {
            return Ok(ParkOutcome::Deliverable);
        }
        // Real nanoseconds until the next deadline (None => wait on input only).
        let timeout_ns = events.peek_deadline().map(|dl| {
            let now2 = clock.vtsc_now();
            if dl <= now2 {
                0
            } else {
                clock.freq().cycles_to_ns(dl - now2)
            }
        });
        // Add the egress epoll fd to the park's poll set so a late socket/resolver
        // readiness wakes the wait (the same one extra source the FF gate uses).
        let egress_fd = egress.as_deref().map(egress::EgressChannel::epoll_fd);
        let wakes = parker.park(timeout_ns, egress_fd)?;
        if wakes.input {
            service_console_input(parker, serial, serial_drain, lapic, ioapic);
        }
        // A timer OR egress wake loops back: the boundary pump above services
        // egress next iteration and service_timers fires any due deadline.
    }
}

/// FF ON: fast-forward park — instead of sleeping until the next deadline, JUMP
/// virtual time to it by bumping the cached TSC offset (write-through to KVM),
/// fire everything now due, and loop. The guest experiences the elapsed virtual
/// time instantly. Idle console input is serviced first (non-blocking) at the
/// top of every iteration, so a quiet console never blocks a jump.
#[allow(clippy::too_many_arguments)]
fn fast_forward_until_deliverable(
    lapic: &mut Lapic,
    ioapic: &Ioapic,
    events: &mut events::EventQueue<TimerKind>,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    parker: &mut park::Parker,
    clock: &VirtualClock,
    vcpu: &VcpuFd,
    horizon: Option<u64>,
    ff: &mut FfState,
    com2: &mut control::ControlChannel,
    mut egress: Option<&mut egress::EgressChannel>,
    driver: &mut driver::DriverRun,
    vtsc_start: u64,
) -> Result<ParkOutcome, Box<dyn std::error::Error>> {
    loop {
        // stdin precedence: service any pending console input up-front, without
        // blocking, so an idle console can never stall a jump.
        if parker.stdin_open() && parker.stdin_ready()? {
            service_console_input(parker, serial, serial_drain, lapic, ioapic);
        }

        // Fire any due timers + the horizon, reconcile the queue to the LAPIC's
        // armed deadline.
        let now = clock.vtsc_now();
        let fired = service_timers(lapic, events, horizon, now);
        if fired.horizon {
            return Ok(ParkOutcome::Horizon);
        }
        if let Some(o) = park_drain_agent(clock, vtsc_start, com2, driver) {
            return Ok(o);
        }
        // Egress boundary pump (beside COM2, at the FF park boundary): service
        // sockets/resolves, feed COM4, raise IRQ3 — BEFORE the deliverable check
        // so a delivered egress frame wakes the guest via the unchanged path. This
        // is the host half of the park-wake chain: a late socket readiness woke the
        // gate park below; this pump turns it into an IRQ3 the guest services.
        if let Some(eg) = egress.as_deref_mut() {
            eg.pump(lapic, ioapic)?;
        }
        if lapic.deliverable_vector().is_some() {
            return Ok(ParkOutcome::Deliverable); // unchanged wake path.
        }

        // THE PHASE GATE (INV-E1/E3). While egress holds live external state a
        // fast-forward jump would skip real time out from under an open connection,
        // so we must NOT jump: park at REAL rate (the SAME timeout computation as
        // the FF-off wait, plus the egress fd) and re-evaluate at the next boundary.
        // Quiescence is checked ONLY here — at the boundary, AFTER the full pump
        // above — never mid-service. `TDVMM_EGRESS_UNSAFE_JUMPS=1` skips THIS park
        // (not the always-on assert below), so the negative-control test can prove
        // the tripwire is live.
        if let Some(eg) = egress.as_deref_mut() {
            if !eg.unsafe_jumps() && !eg.is_quiescent() {
                let timeout_ns = events.peek_deadline().map(|dl| {
                    let now2 = clock.vtsc_now();
                    if dl <= now2 { 0 } else { clock.freq().cycles_to_ns(dl - now2) }
                });
                let gate_start = std::time::Instant::now();
                let wakes = parker.park(timeout_ns, Some(eg.epoll_fd()))?;
                if wakes.input {
                    service_console_input(parker, serial, serial_drain, lapic, ioapic);
                }
                // Stamp the real-rate interval the gate imposed (feeds the report +
                // the >30s long-gate WARN). The wake itself is serviced by the
                // boundary pump at the top of the next iteration (after `continue`).
                let gated_ns = gate_start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
                eg.note_gated_interval(gated_ns);
                eg.maybe_warn_long_gate();
                continue;
            }
        }

        // The next scheduled event decides the jump target. This is the min of
        // the LAPIC deadline and the horizon, so a horizon nearer than the next
        // tick becomes the jump target and StopRun fires on the next iteration.
        let next = match events.peek_deadline() {
            Some(dl) => dl,
            None => {
                // Nothing armed and nothing deliverable: there is no virtual-time
                // deadline to jump to. An idle guest always has its tick armed, so
                // this is a corner case (e.g. a console-only wait). Fall back to a
                // real wait on stdin (plus the egress fd) rather than spin.
                let egress_fd = egress.as_deref().map(egress::EgressChannel::epoll_fd);
                let wakes = parker.park(None, egress_fd)?;
                if wakes.input {
                    service_console_input(parker, serial, serial_drain, lapic, ioapic);
                }
                continue;
            }
        };

        // JUMP. Sample the host TSC ONCE (h) so the post-condition is exact.
        let hop_start = std::time::Instant::now();
        let h = vtsc::host_rdtsc();
        let now_h = clock.vtsc_from_host(h);
        if next <= now_h {
            // Became due while we were working: loop and let service_timers fire it.
            continue;
        }
        let delta = next - now_h; // vtsc cycles to advance, > 0

        // Gate 3: single-jump sanity bound. Expected never to trip on a real guest
        // timer. The horizon is EXEMPT: it is an operator-set virtual-time budget,
        // not a guest deadline, so jumping straight to it (e.g. a deeply-idle guest
        // whose next tick is beyond both the horizon and the bound) is intended,
        // not an anomaly to abort on.
        let jumping_to_horizon = horizon == Some(next);
        if !jumping_to_horizon && delta > ff.max_jump_cycles {
            return Err(format!(
                "fast-forward jump Δ={delta} cycles (~{:.3}s) exceeds the sanity bound of {}s \
                 ({} cycles); aborting (use --max-jump-secs to raise)",
                delta as f64 / ff.tsc_hz as f64,
                ff.max_jump_secs,
                ff.max_jump_cycles,
            )
            .into());
        }

        // ALWAYS-ON phase-gate assert (INV-E1), release included exactly like the
        // queue-discipline assert below it. A jump is legal only when egress is off
        // or quiescent. The gating branch above guarantees quiescence here under
        // normal operation; this is the last-line tripwire — LIVE even under
        // TDVMM_EGRESS_UNSAFE_JUMPS (which skips the gate park, not this check).
        egress::assert_ff_jump_legal(egress.as_deref().map(egress::EgressChannel::backend));

        // Advance virtual time: cached offset += delta, write-through to KVM. The
        // offset is monotonically non-decreasing (delta > 0). All clock clones
        // (LAPIC, PIT) observe the new offset immediately via the shared cell.
        clock.bump_offset(vcpu, delta as i64)?;

        // Post-condition (queue-discipline assert): landing is EXACT at the same
        // host sample h — vtsc_from_host(h) must now equal the event deadline.
        let landed = clock.vtsc_from_host(h);
        assert_eq!(
            landed, next,
            "post-bump vtsc {landed} != next event deadline {next} (Δ was {delta})"
        );

        ff.record_hop(delta, hop_start.elapsed());
        // WARN-only telemetry: surface a sustained high jump rate (the wedge
        // signature) with a histogram snapshot. NEVER stops the run.
        ff.maybe_warn_high_jump_rate();
        // Loop: service_timers fires the now-due event and the guest reprograms
        // its timer; the periodic re-arm is simply the next queue entry.
    }
}

/// Read whatever console input `ppoll`/poll signalled and feed it to the UART,
/// raising the serial RX IRQ iff the UART asserted one. EOF closes stdin so it is
/// no longer polled. Shared by the real-wait and fast-forward park paths.
fn service_console_input(
    parker: &mut park::Parker,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    lapic: &mut Lapic,
    ioapic: &Ioapic,
) {
    match read_console_input(serial) {
        // Real bytes: raise the serial RX IRQ iff the UART actually asserted an
        // interrupt (mirrors the serial-PIO path).
        Some(n) if n > 0 => {
            if serial_drain.drain().is_ok() {
                raise_irq(lapic, ioapic, arch::SERIAL_IRQ);
            }
        }
        // EOF (closed stdin / `</dev/null`): stop polling it so the park waits on
        // the timer alone instead of spinning.
        None => parker.close_stdin(),
        _ => {}
    }
}

/// Post an ISA IRQ line edge into the LAPIC via the IOAPIC RTE (masked/level
/// entries deliver nothing). Runs on the vCPU thread, at a loop boundary. Used by
/// the COM1 serial path here and the COM2 control channel (`control.rs`).
pub(crate) fn raise_irq(lapic: &mut Lapic, ioapic: &Ioapic, irq: u32) {
    let pin = isa_irq_to_ioapic_pin(irq as u8) as usize;
    if let Some(vector) = ioapic.edge_vector(pin) {
        lapic.raise(vector);
    }
}

/// Read whatever console input is ready (stdin was signalled by `ppoll`) into
/// the UART receive path. Never blocks meaningfully — data is already available.
/// Returns `Some(n)` bytes read, or `None` on EOF (closed stdin).
fn read_console_input(serial: &serial::SharedSerial) -> Option<usize> {
    let mut buf = [0u8; 64];
    // SAFETY: fd 0 is valid; `buf` is writable for its whole length.
    let n = unsafe { libc::read(0, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if n > 0 {
        let mut s = serial.lock().unwrap();
        let _ = s.enqueue_raw_bytes(&buf[..n as usize]);
        Some(n as usize)
    } else if n == 0 {
        None // EOF
    } else {
        Some(0) // transient error (e.g. EAGAIN): treat as "no data"
    }
}

// =====================================================================
// --logs-dir: post-verdict per-service log capture
// =====================================================================

/// True when `s` is a single safe path component: non-empty, not `.`/`..`, and
/// every byte in `[A-Za-z0-9._-]` (so never a path separator). The shared rule
/// behind both `sanitize_service_filename` (guest log files) and the build's
/// `parse_stack_name` (the stack name).
pub(crate) fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Validate a guest-supplied service name for use as a `<name>.log` filename.
/// Container/service names are HOSTILE guest data, so accept only a strict
/// `[A-Za-z0-9._-]+` single path component. Returns `None` (skip that service)
/// for anything else.
pub(crate) fn sanitize_service_filename(name: &str) -> Option<String> {
    is_safe_path_component(name).then(|| name.to_string())
}

/// Reformat raw k8s-file log bytes into a readable `<ts> <stream> <message>` per
/// line, dropping podman's `F`/`P` full/partial tag but keeping the RFC3339
/// timestamp and the stdout/stderr tag. A line that does not parse as a k8s-file
/// entry is passed through verbatim (robustness over strictness).
fn format_k8s_logs(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        if line.is_empty() {
            continue;
        }
        // k8s-file entry: "<rfc3339-ts> <stdout|stderr> <F|P> <message...>".
        let mut it = line.splitn(4, ' ');
        let ts = it.next().unwrap_or("");
        let stream = it.next().unwrap_or("");
        let pf = it.next().unwrap_or("");
        let msg = it.next().unwrap_or("");
        if (stream == "stdout" || stream == "stderr") && (pf == "F" || pf == "P") {
            out.push_str(ts);
            out.push(' ');
            out.push_str(stream);
            out.push(' ');
            out.push_str(msg);
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Real-time + virtual-time bounds on the WHOLE post-verdict capture. Generous
/// (capture is normally sub-second) but hard: they guarantee the pull can never
/// hang the run — a wedged agent gives up here, warns, and finalize proceeds.
const CAPTURE_WALL_BUDGET_S: u64 = 30;
const CAPTURE_VTSC_BUDGET_S: f64 = 120.0;

/// Drive the STILL-ALIVE vCPU on this thread to fetch each service's k8s-file log
/// via the agent's `logs` op, writing `<dir>/<service>.log`. Post-verdict /
/// end-of-run only. This NEVER affects the verdict: a schema-too-old agent, a
/// missing container, an unreadable log, a timeout, or a guest death all warn
/// once and skip (partial output at worst). No `-f`/follow — a single bounded
/// read per chunk, the host loops via `next_cursor`/`eof`.
#[allow(clippy::too_many_arguments)]
fn capture_logs(
    vcpu: &mut VcpuFd,
    doorbell: &mut doorbell::Doorbell,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    clock: &VirtualClock,
    lapic: &mut Lapic,
    ioapic: &mut Ioapic,
    pit: &mut PitStub,
    pic: &mut PicStub,
    events: &mut events::EventQueue<TimerKind>,
    parker: &mut park::Parker,
    mut ff: Option<&mut FfState>,
    com2: &mut control::ControlChannel,
    mut egress: Option<&mut egress::EgressChannel>,
    cap: &LogsCapture,
) {
    use tdvmm_proto::{encode_line, Request, MAX_LOGS_CHUNK_BYTES};

    let wall_deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(CAPTURE_WALL_BUDGET_S);
    let horizon = clock
        .vtsc_now()
        .wrapping_add((CAPTURE_VTSC_BUDGET_S * clock.freq().hz() as f64) as u64);
    let mut next_id: u64 = 1;
    let new_id = |n: &mut u64| -> u64 {
        let id = *n;
        *n += 1;
        id
    };

    // Schema negotiation (Fable): a ping reports the baked agent's wire schema. An
    // OLD artifact (schema < 2) has no `logs` op — warn ONCE and skip, never error.
    let ping = Request {
        id: new_id(&mut next_id),
        op: "ping".into(),
        ..Default::default()
    };
    let ping_id = ping.id;
    let framed = encode_line(&ping).unwrap();
    match drive_until_reply(
        vcpu, doorbell, serial, serial_drain, clock, lapic, ioapic, pit, pic, events, parker,
        ff.as_deref_mut(), com2, egress.as_deref_mut(), &framed, ping_id, horizon, wall_deadline,
    ) {
        Ok(Some(rep)) => match rep.schema {
            Some(s) if s >= 2 => {}
            other => {
                dlog!(
                    "[tdvmm][WARN] --logs-dir: agent proto schema {} < 2 (old artifact) — the \
                     `logs` op is unavailable; skipping log capture",
                    other.map(|v| v.to_string()).unwrap_or_else(|| "?".into())
                );
                return;
            }
        },
        Ok(None) => {
            dlog!("[tdvmm][WARN] --logs-dir: agent did not answer a schema ping — skipping log capture");
            return;
        }
        Err(()) => {
            dlog!("[tdvmm][WARN] --logs-dir: guest became unresponsive — skipping log capture");
            return;
        }
    }

    for service in &cap.services {
        let fname = match sanitize_service_filename(service) {
            Some(f) => f,
            None => {
                dlog!("[tdvmm][WARN] --logs-dir: refusing unsafe service name {service:?} — skipping");
                continue;
            }
        };
        let mut raw = String::new();
        let mut cursor: u64 = 0;
        let mut aborted = false;
        loop {
            let req = Request {
                id: new_id(&mut next_id),
                op: "logs".into(),
                container: Some(service.clone()),
                cursor: Some(cursor),
                max_bytes: Some(MAX_LOGS_CHUNK_BYTES),
                ..Default::default()
            };
            let id = req.id;
            let framed = encode_line(&req).unwrap();
            match drive_until_reply(
                vcpu, doorbell, serial, serial_drain, clock, lapic, ioapic, pit, pic, events, parker,
                ff.as_deref_mut(), com2, egress.as_deref_mut(), &framed, id, horizon, wall_deadline,
            ) {
                Ok(Some(rep)) => {
                    if rep.ok != Some(true) {
                        dlog!(
                            "[tdvmm][WARN] --logs-dir: agent could not read {service} log: {} — \
                             writing what was captured",
                            rep.error.as_deref().unwrap_or("unknown error")
                        );
                        break;
                    }
                    if let Some(d) = rep.data.as_deref() {
                        raw.push_str(d);
                    }
                    let next = rep.next_cursor.unwrap_or(cursor);
                    // Stop at EOF, or defensively if the cursor did not advance.
                    if rep.eof == Some(true) || next <= cursor {
                        break;
                    }
                    cursor = next;
                }
                Ok(None) => {
                    dlog!(
                        "[tdvmm][WARN] --logs-dir: timed out pulling {service} log — writing what \
                         was captured"
                    );
                    break;
                }
                Err(()) => {
                    dlog!("[tdvmm][WARN] --logs-dir: guest became unresponsive during capture — stopping");
                    aborted = true;
                    break;
                }
            }
        }
        let formatted = format_k8s_logs(&raw);
        let path = cap.dir.join(format!("{fname}.log"));
        match std::fs::write(&path, formatted.as_bytes()) {
            Ok(()) => dlog!("[tdvmm] --logs-dir: wrote {}", path.display()),
            Err(e) => dlog!("[tdvmm][WARN] --logs-dir: could not write {}: {e}", path.display()),
        }
        if aborted {
            return;
        }
    }
}

/// One post-verdict control round-trip: queue `framed` (a `logs`/`ping` request),
/// then drive the still-alive vCPU until the agent's reply with `want_id` returns
/// (`Ok(Some)`), a bounded budget elapses (`Ok(None)` — give up this request), or
/// the guest dies / the vCPU errors (`Err(())` — abort all capture). The
/// capture-local `horizon` guarantees the idle park never blocks forever, and
/// `wall_deadline` bounds it in real time too. Deliberately self-contained (it
/// mirrors the main loop's inject/run/handle-exit shape) so the main loop stays
/// byte-for-byte unchanged — FF-neutrality and frozen contracts (Fable).
#[allow(clippy::too_many_arguments)]
fn drive_until_reply(
    vcpu: &mut VcpuFd,
    doorbell: &mut doorbell::Doorbell,
    serial: &serial::SharedSerial,
    serial_drain: &serial::EventFdTrigger,
    clock: &VirtualClock,
    lapic: &mut Lapic,
    ioapic: &mut Ioapic,
    pit: &mut PitStub,
    pic: &mut PicStub,
    events: &mut events::EventQueue<TimerKind>,
    parker: &mut park::Parker,
    mut ff: Option<&mut FfState>,
    com2: &mut control::ControlChannel,
    mut egress: Option<&mut egress::EgressChannel>,
    framed: &[u8],
    want_id: u64,
    horizon: u64,
    wall_deadline: std::time::Instant,
) -> Result<Option<tdvmm_proto::Reply>, ()> {
    // Drop any stale reply still buffered from the run, then queue our request.
    while com2.poll_line().is_some() {}
    com2.send_frame(framed);
    com2.pump(lapic, ioapic);

    loop {
        // Doorbell hygiene (mirrors the main loop): clear immediate_exit before
        // service_timers so any fire from here on re-sets it and the coming
        // KVM_RUN bails — capture must preempt an exit-free guest too (a container
        // may still be busy-looping while we pull logs).
        doorbell.clear(vcpu);
        if std::time::Instant::now() >= wall_deadline {
            return Ok(None);
        }
        let now = clock.vtsc_now();
        // Capture-local horizon only (no scenario step here). If it fires, give up.
        if service_timers(lapic, events, Some(horizon), now).horizon {
            return Ok(None);
        }
        // Stream any in-flight command bytes, then drain replies looking for ours.
        com2.pump(lapic, ioapic);
        while let Some(line) = com2.poll_line() {
            if let Ok(rep) = tdvmm_proto::decode_line::<tdvmm_proto::Reply>(&line) {
                if rep.id == Some(want_id) {
                    return Ok(Some(rep));
                }
            }
            com2.pump(lapic, ioapic);
        }
        // Egress boundary pump during capture: a session may still be live while we
        // pull logs, so service it (feed COM4 + raise IRQ3) each iteration. A
        // framing error aborts capture (Err) — never a panic. The egress fd is also
        // in the park below, so a late readiness wakes the capture wait too.
        if let Some(eg) = egress.as_deref_mut() {
            if eg.pump(lapic, ioapic).is_err() {
                return Err(());
            }
        }

        // Sync TPR + inject a deliverable vector (mirrors the main loop exactly).
        let cr8 = vcpu.get_kvm_run().cr8;
        lapic.sync_tpr_from_cr8(cr8);
        let deliverable = lapic.deliverable_vector();
        let (ready, if_flag) = {
            let r = vcpu.get_kvm_run();
            (r.ready_for_interrupt_injection, r.if_flag)
        };
        let mut injected = false;
        if let Some(vec) = deliverable {
            if ready != 0 && if_flag != 0 {
                if inject_interrupt(vcpu, vec).is_err() {
                    return Err(());
                }
                lapic.ack_injected(vec);
                injected = true;
            }
        }
        {
            let r = vcpu.get_kvm_run();
            r.request_interrupt_window = u8::from(deliverable.is_some() && !injected);
            r.cr8 = u64::from(lapic.tpr() >> 4);
        }

        // Arm the doorbell so an exit-free guest (e.g. a container still busy-
        // looping while we pull its logs) is broken out in time — same as the main
        // loop; fold in the LAPIC's live deadline (see min_deadline). `clock` is
        // already a reference here (unlike the main loop's owned clock).
        doorbell.arm(min_deadline(events.peek_deadline(), lapic.timer_deadline()), clock);
        let exit = match vcpu.run() {
            Ok(e) => e,
            Err(err) => {
                let e = err.errno();
                if e == libc::EINTR || e == libc::EAGAIN {
                    continue;
                }
                return Err(());
            }
        };
        match exit {
            VcpuExit::IoOut(port, data) => {
                if serial::is_serial(port) {
                    let mut s = serial.lock().unwrap();
                    for &b in data {
                        let _ = s.write((port - arch::SERIAL_PORT_BASE) as u8, b);
                    }
                    drop(s);
                    if serial_drain.drain().is_ok() {
                        raise_irq(lapic, ioapic, arch::SERIAL_IRQ);
                    }
                } else if control::ControlChannel::handles(port) {
                    for &b in data {
                        com2.pio_write(port, b, lapic, ioapic);
                    }
                } else if PitStub::handles(port) {
                    for &b in data {
                        pit.write(port, b);
                    }
                } else if PicStub::handles(port) {
                    for &b in data {
                        pic.write(port, b);
                    }
                }
            }
            VcpuExit::IoIn(port, data) => {
                if serial::is_serial(port) {
                    let mut s = serial.lock().unwrap();
                    for b in data.iter_mut() {
                        *b = s.read((port - arch::SERIAL_PORT_BASE) as u8);
                    }
                    drop(s);
                    if serial_drain.drain().is_ok() {
                        raise_irq(lapic, ioapic, arch::SERIAL_IRQ);
                    }
                } else if control::ControlChannel::handles(port) {
                    for b in data.iter_mut() {
                        *b = com2.pio_read(port, lapic, ioapic);
                    }
                } else if PitStub::handles(port) {
                    for b in data.iter_mut() {
                        *b = pit.read(port);
                    }
                } else if PicStub::handles(port) {
                    for b in data.iter_mut() {
                        *b = pic.read(port);
                    }
                } else {
                    for b in data.iter_mut() {
                        *b = 0xff; // open bus
                    }
                }
            }
            VcpuExit::MmioRead(addr, data) => {
                let val = if in_lapic(addr) {
                    lapic.mmio_read((addr - XAPIC_BASE) as u32)
                } else if Ioapic::handles(addr) {
                    ioapic.mmio_read(addr)
                } else {
                    0
                };
                util::write_u32_le(data, val);
            }
            VcpuExit::MmioWrite(addr, data) => {
                let val = util::read_u32_le(data);
                if in_lapic(addr) {
                    lapic.mmio_write((addr - XAPIC_BASE) as u32, val);
                } else if Ioapic::handles(addr) {
                    ioapic.mmio_write(addr, val);
                }
            }
            VcpuExit::IrqWindowOpen => {}
            VcpuExit::Hlt => {
                // A HLT with IF=0 can never wake: the guest powered off mid-capture.
                if vcpu.get_kvm_run().if_flag == 0 {
                    return Err(());
                }
                // Log capture runs AFTER the verdict, so a `finish` here would be
                // a second one — which the agent refuses to emit. A throwaway
                // DriverRun keeps the park's drain from touching the real one.
                let mut sink = driver::DriverRun::default();
                match park_until_deliverable(
                    lapic, ioapic, events, serial, serial_drain, parker, clock, vcpu,
                    Some(horizon), ff.as_deref_mut(), com2, egress.as_deref_mut(),
                    &mut sink, 0,
                ) {
                    Ok(ParkOutcome::Deliverable) => {}
                    // The capture budget elapsed while idle: give up this request.
                    Ok(ParkOutcome::Horizon) => return Ok(None),
                    // Cannot arise post-verdict; treat as "keep going" defensively.
                    Ok(ParkOutcome::DriverFinish) => {}
                    // e.g. an FF jump exceeded the sanity bound: abort capture.
                    Err(_) => return Err(()),
                }
            }
            VcpuExit::Shutdown | VcpuExit::SystemEvent(_, _) => return Err(()),
            VcpuExit::FailEntry(_, _) | VcpuExit::InternalError => return Err(()),
            _ => {}
        }
    }
}

/// The IOAPIC id we advertised in the MP table (num_cpus + 1, single vCPU => 2).
fn mptable_ioapic_id() -> u8 {
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ff_mode_statement_reports_state_and_source() {
        // The default (no --ff) is ON, chosen by the binary default.
        assert_eq!(ff_mode_statement(true, false), "fast-forward: ON (default)");
        // Explicit flags report how it was chosen (run.sh passes `--ff off`).
        assert_eq!(ff_mode_statement(false, true), "fast-forward: OFF (--ff off)");
        assert_eq!(ff_mode_statement(true, true), "fast-forward: ON (--ff on)");
        // A default-off would still read "default" (documents the mechanism even
        // though the binary default is on).
        assert_eq!(ff_mode_statement(false, false), "fast-forward: OFF (default)");
    }

    #[test]
    fn service_timers_fires_horizon_as_a_queue_event() {
        // The horizon is enforced purely as a (vtsc, StopRun) queue entry: before
        // the horizon vtsc, service_timers reports no stop; at/after it, it does.
        let clock = VirtualClock::new(0, vtsc::TscFrequency::from_hz(1_000_000_000));
        let mut lapic = Lapic::new(clock, 160, 2);
        let mut events: events::EventQueue<TimerKind> = events::EventQueue::new();
        let horizon = Some(10_000u64);
        assert!(!service_timers(&mut lapic, &mut events, horizon, 9_999).horizon);
        assert!(service_timers(&mut lapic, &mut events, horizon, 10_000).horizon); // == fires
        assert!(service_timers(&mut lapic, &mut events, horizon, 10_001).horizon); // past
        // No horizon set -> never a horizon stop.
        assert!(!service_timers(&mut lapic, &mut events, None, u64::MAX).horizon);
    }

    #[test]
    fn sanitize_service_filename_rejects_hostile_names() {
        // Ordinary compose service names pass through unchanged.
        assert_eq!(sanitize_service_filename("postgres").as_deref(), Some("postgres"));
        assert_eq!(sanitize_service_filename("web-app_1.v2").as_deref(), Some("web-app_1.v2"));
        // Path separators, traversal, and other metacharacters are rejected.
        for bad in [
            "", ".", "..", "../etc/passwd", "a/b", "a\\b", "/abs", "name with space",
            "a\0b", "évil", "a;b", "..\\x",
        ] {
            assert!(
                sanitize_service_filename(bad).is_none(),
                "expected {bad:?} to be rejected"
            );
        }
    }

    #[test]
    fn format_k8s_logs_keeps_ts_and_stream_drops_tag() {
        let raw = "2026-08-01T00:00:00.1Z stdout F hello world\n\
                   2026-08-01T00:00:01.2Z stderr F oops: a spaced message\n\
                   2026-08-01T00:00:02.3Z stdout P partial-chunk\n\
                   not a k8s line at all\n\
                   \n";
        let out = format_k8s_logs(raw);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "2026-08-01T00:00:00.1Z stdout hello world");
        assert_eq!(lines[1], "2026-08-01T00:00:01.2Z stderr oops: a spaced message");
        assert_eq!(lines[2], "2026-08-01T00:00:02.3Z stdout partial-chunk");
        // A non-conforming line is passed through verbatim; blanks are dropped.
        assert_eq!(lines[3], "not a k8s line at all");
        assert_eq!(lines.len(), 4);
    }
}
