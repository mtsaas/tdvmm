//! Small build-time utilities: content hashing, temp-dir creation, locating the
//! repo `guest/` tree from the running binary, and the informational UTC clock
//! used by the lock ledger.

use std::path::{Path, PathBuf};

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
    // .../target/release/dvmm -> .../ (repo root)
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

/// Create a fresh, uniquely-named scratch directory under the system temp dir.
pub(super) fn mkdtemp() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let name = format!("dvmm-build-{}-{}", std::process::id(), now_nanos());
    let dir = base.join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
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
/// (Howard Hinnant's algorithm). Shared with `main.rs` for `dvmm ls` timestamps.
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
