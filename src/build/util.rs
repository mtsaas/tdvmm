//! Small build-time utilities: content hashing, temp-dir creation, locating the
//! repo `guest/` tree from the running binary, and the informational UTC clock
//! used by the lock ledger.

use std::path::{Path, PathBuf};

use crate::engine;

/// The sha256 of a file's contents, hex-encoded.
pub(super) fn sha256_file_hex(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Resolve the repo `guest/` directory relative to the running binary (target/…).
/// Falls back to `guest/` under the current dir.
pub(super) fn self_here() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    let mut p = exe.clone();
    // .../target/release/tdvmm -> .../ (repo root)
    for _ in 0..3 {
        p.pop();
    }
    let cand = p.join("guest");
    if cand.is_dir() {
        return Ok(cand);
    }
    let cwd = std::env::current_dir()?.join("guest");
    if cwd.is_dir() {
        return Ok(cwd);
    }
    Err("could not locate the repo guest/ directory (run from the repo, or keep target/ in place)".into())
}

/// Filename prefix for every build scratch dir (`tdvmm-build-<pid>-<nanos>`). The
/// pid is the first hyphen-field after it — [`sweep_stale_scratch`] parses it, so
/// keep the two in sync.
const SCRATCH_PREFIX: &str = "tdvmm-build-";

/// Create a fresh, uniquely-named scratch directory under the system temp dir.
fn mkdtemp() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let name = format!("{SCRATCH_PREFIX}{}-{}", std::process::id(), now_nanos());
    let dir = base.join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A build scratch directory that removes itself when dropped — on every exit
/// path, including `?` early-returns and unwinds.
///
/// Cleanup is **unshare-aware**: some build steps fill the dir with files owned by
/// subordinate UIDs (rootless podman seed stores / bind-mount output) that a plain
/// `remove_dir_all` can't unlink (`EPERM`). On that failure it retries inside the
/// podman user namespace, and it *logs* — never silently swallows — a removal that
/// still fails. A swallowed `let _ = remove_dir_all` on those subuid-owned stores
/// is what leaked tens of GB of scratch into `$TMPDIR`.
pub(super) struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// Create a fresh, uniquely-named scratch dir under the system temp dir.
    pub(super) fn new() -> std::io::Result<Self> {
        Ok(Self { path: mkdtemp()? })
    }

    /// The directory path. Borrowed: the dir exists until the guard is dropped.
    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        remove_build_scratch(&self.path);
    }
}

/// Remove a build scratch tree, tolerating the subuid-owned files rootless podman
/// leaves behind. Plain recursive remove first; only on `EPERM` fall back to
/// removing the tree inside the podman user namespace. A still-failing removal is
/// logged, not swallowed — a leaked seed store is multiple GB.
pub(super) fn remove_build_scratch(path: &Path) {
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            if let Err(msg) = unshare_remove(path) {
                eprintln!(
                    "warning: leaked build scratch {} (unshare cleanup failed: {msg})",
                    path.display()
                );
            }
        }
        Err(e) => eprintln!("warning: leaked build scratch {} ({e})", path.display()),
    }
}

/// Remove `path` from inside the podman user namespace, where the subordinate UIDs
/// owning a rootless seed store map back to ones we can unlink. A minimal clean
/// `CONTAINERS_CONF` (a sibling file, cleaned up after) keeps us off the host
/// default runtime — see [`crate::engine`].
fn unshare_remove(path: &Path) -> Result<(), String> {
    let conf = path.with_extension("cleanup-conf");
    std::fs::write(&conf, "[engine]\n").map_err(|e| format!("write cleanup conf: {e}"))?;
    let result = engine::run(
        engine::unshare(&conf).arg("rm").arg("-rf").arg(path),
        engine::OutputMode::CaptureOnFailure,
    );
    let _ = std::fs::remove_file(&conf);
    result
}

/// Sweep scratch dirs orphaned by tdvmm processes no longer alive — e.g. a
/// SIGKILL/OOM where [`ScratchDir`]'s Drop never ran. Best-effort; call once at
/// `tdvmm build` start. A dir whose embedded pid is still alive is left untouched,
/// so concurrent bakes never reap each other's scratch.
pub(super) fn sweep_stale_scratch() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(SCRATCH_PREFIX))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if !pid_is_alive(pid) {
            remove_build_scratch(&entry.path());
        }
    }
}

/// Whether a pid currently exists (Linux `/proc`). Conservative: a reused pid
/// reads as alive, so we never reap a running bake's scratch.
fn pid_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// Nanoseconds since the Unix epoch (best-effort; used only to name scratch dirs).
pub(super) fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Current UTC time as an ISO-8601 string. Best-effort and informational only
/// (NOT part of the byte-identity gate).
pub(super) fn utc_now_iso() -> String {
    // best-effort; informational only (NOT part of the byte-identity gate).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // crude UTC breakdown
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 based civil date
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Convert a day count since the Unix epoch to a `(year, month, day)` civil date
/// (Howard Hinnant's algorithm). Shared with `main.rs` for `tdvmm ls` timestamps.
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
