//! `tdvmm doctor` — host prerequisite checks + build-cache pre-warm.
//!
//! Probes everything a first `tdvmm build`/`run` needs (/dev/kvm, KVM incl. the
//! `KVM_VCPU_TSC_OFFSET` attribute, host kernel version, podman, network, cache
//! dir), printing one ✓/✗ line per check, then pre-warms the build cache
//! through the bake pipeline's own primitives ([`build::prewarm`], skippable
//! with `--skip-downloads`). Exit 0 = everything healthy; 1 = one or more
//! problems.

use std::path::Path;
use std::time::Duration;

use kvm_ioctls::Kvm;

use crate::build;
use crate::engine;
use crate::ui;
use crate::vtsc;

/// Minimum host kernel for the `KVM_VCPU_TSC_OFFSET` vCPU device attribute the
/// virtual clock depends on (see [`vtsc`]).
const MIN_KERNEL: (u32, u32) = (5, 16);

/// Free-disk floor for the cache dir: the kernel/agent container builds and
/// the bake outputs need real space.
const MIN_FREE_GIB: u64 = 10;

/// One check's outcome: `Ok(one-line detail)` / `Err(one-line reason)`.
type CheckResult = Result<String, String>;

pub(crate) fn cmd_doctor(
    skip_downloads: bool,
    no_progress: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let (cache_dir, cache_src) = build::resolve_cache_dir(None);
    let checks = [
        ("/dev/kvm", check_dev_kvm()),
        ("kvm", check_kvm()),
        ("host kernel", check_host_kernel()),
        ("podman", check_podman()),
        ("network", check_network()),
        ("cache dir", check_cache_dir(&cache_dir, cache_src)),
    ];
    println!("tdvmm doctor");
    for (name, result) in &checks {
        println!("{}", render_check(name, result));
    }
    let mut problems = problem_count(&checks);

    if skip_downloads {
        println!("  - {:<12} skipped (--skip-downloads)", "pre-warm");
    } else {
        // The pre-warm runs through the same stepped progress UI as `tdvmm
        // build` (the kernel/agent container compiles stream into the live
        // tail); non-TTY falls back to the plain build lines, as always.
        let progress = ui::Progress::new(no_progress);
        let result = build::prewarm(&cache_dir, &progress);
        progress.finish();
        match result {
            Ok(()) => {
                println!("  ✓ {:<12} guest kernel + agent + pinned downloads cached", "pre-warm");
            }
            Err(e) => {
                problems += 1;
                println!("{}", render_check("pre-warm", &Err(e.to_string())));
            }
        }
    }

    if problems == 0 {
        println!("doctor: all good");
    } else {
        println!("doctor: {problems} problem(s) found");
    }
    Ok(exit_code(problems))
}

// ---- the probes (environment-dependent; deliberately thin) -----------------

fn check_dev_kvm() -> CheckResult {
    let path = Path::new("/dev/kvm");
    if !path.exists() {
        return Err("not found (is the kvm module loaded?)".into());
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map(|_| "readable + writable".to_string())
        .map_err(|e| format!("cannot open read+write: {e} (add your user to the kvm group?)"))
}

/// KVM actually usable: create a scratch VM + vCPU, then probe the
/// `KVM_VCPU_TSC_OFFSET` attribute — the exact read the VMM boots with.
fn check_kvm() -> CheckResult {
    let kvm = Kvm::new().map_err(|e| format!("KVM unavailable: {e}"))?;
    let vm = kvm
        .create_vm()
        .map_err(|e| format!("KVM_CREATE_VM failed: {e} (virtualization disabled in firmware?)"))?;
    let vcpu = vm.create_vcpu(0).map_err(|e| format!("KVM_CREATE_VCPU failed: {e}"))?;
    vtsc::read_tsc_offset(&vcpu).map_err(|e| e.to_string())?;
    Ok(format!(
        "virtualization ok (api v{}), KVM_VCPU_TSC_OFFSET available",
        kvm.get_api_version()
    ))
}

fn check_host_kernel() -> CheckResult {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map_err(|e| format!("reading /proc/sys/kernel/osrelease: {e}"))?;
    kernel_release_check(release.trim())
}

/// Pure decision for the host-kernel check: `release` (uname -r form, e.g.
/// `6.9.3-arch1-1`) must be >= [`MIN_KERNEL`].
fn kernel_release_check(release: &str) -> CheckResult {
    let (want_major, want_minor) = MIN_KERNEL;
    let mut nums = release.split(['.', '-']).map_while(|s| s.parse::<u32>().ok());
    match (nums.next(), nums.next()) {
        (Some(major), Some(minor)) if (major, minor) >= MIN_KERNEL => {
            Ok(format!("{release} (>= {want_major}.{want_minor})"))
        }
        (Some(_), Some(_)) => Err(format!(
            "{release} is older than {want_major}.{want_minor} (KVM_VCPU_TSC_OFFSET needs it)"
        )),
        _ => Err(format!("cannot parse kernel release {release:?}")),
    }
}

fn check_podman() -> CheckResult {
    let out = engine::command()
        .arg("--version")
        .output()
        .map_err(|e| format!("{} not runnable: {e}", engine::ENGINE))?;
    if !out.status.success() {
        return Err(format!("`{} --version` failed ({})", engine::ENGINE, out.status));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// TCP-reach the Alpine mirror a first, cold-cache build fetches from (a warm
/// cache never touches the network).
fn check_network() -> CheckResult {
    use std::net::{TcpStream, ToSocketAddrs};
    const HOST: &str = "dl-cdn.alpinelinux.org:443";
    let addr = HOST
        .to_socket_addrs()
        .map_err(|e| format!("DNS for {HOST}: {e}"))?
        .next()
        .ok_or_else(|| format!("no address for {HOST}"))?;
    TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map(|_| format!("{HOST} reachable"))
        .map_err(|e| format!("cannot reach {HOST}: {e}"))
}

fn check_cache_dir(cache_dir: &Path, source: &str) -> CheckResult {
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("cannot create {}: {e}", cache_dir.display()))?;
    let probe = cache_dir.join(".doctor-write-probe");
    std::fs::write(&probe, b"").map_err(|e| format!("{} not writable: {e}", cache_dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    let free_gib = free_bytes(cache_dir)
        .map_err(|e| format!("statvfs {}: {e}", cache_dir.display()))?
        >> 30;
    if free_gib < MIN_FREE_GIB {
        return Err(format!(
            "{} ({source}) has only {free_gib} GiB free (need >= {MIN_FREE_GIB} GiB)",
            cache_dir.display()
        ));
    }
    Ok(format!("{} ({source}), writable, {free_gib} GiB free", cache_dir.display()))
}

/// Free bytes available to unprivileged writes on `path`'s filesystem.
fn free_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    // SAFETY: `c` is a valid NUL-terminated path; `st` is a plain-integer struct
    // (all-zeroes is valid) that statvfs fully fills on success.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

// ---- rendering + aggregation (pure; unit-tested) ---------------------------

/// One report line: `  ✓ name  detail` / `  ✗ name  reason`.
fn render_check(name: &str, result: &CheckResult) -> String {
    match result {
        Ok(detail) => format!("  ✓ {name:<12} {detail}"),
        Err(reason) => format!("  ✗ {name:<12} {reason}"),
    }
}

fn problem_count(checks: &[(&str, CheckResult)]) -> usize {
    checks.iter().filter(|(_, r)| r.is_err()).count()
}

/// The doctor exit convention: 0 = every check (and the pre-warm) passed;
/// 1 = one or more problems.
fn exit_code(problems: usize) -> i32 {
    i32::from(problems > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_release_gate_honors_min_version() {
        assert!(kernel_release_check("5.16.0").is_ok());
        assert!(kernel_release_check("6.9.3-arch1-1").is_ok());
        assert!(kernel_release_check("5.15.148").is_err());
        assert!(kernel_release_check("4.19.0-25-generic").is_err());
        assert!(kernel_release_check("mystery").is_err());
    }

    #[test]
    fn report_lines_and_exit_convention() {
        let checks = [
            ("ok", Ok("fine".to_string())),
            ("bad", Err("broken".to_string())),
        ];
        let good = render_check(checks[0].0, &checks[0].1);
        assert!(good.starts_with("  ✓ ok") && good.ends_with(" fine"), "{good}");
        let bad = render_check(checks[1].0, &checks[1].1);
        assert!(bad.starts_with("  ✗ bad") && bad.ends_with(" broken"), "{bad}");
        assert_eq!(problem_count(&checks), 1);
        assert_eq!(problem_count(&checks[..1]), 0);
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(2), 1);
    }
}
