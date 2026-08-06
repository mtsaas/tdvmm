//! content-hash bake cache
//!
//! The biggest e2e-speed win: `tdvmm build` is deterministic (identical inputs ->
//! byte-identical `.tdvmm`; see artifact_test gate 1), so a build whose inputs are
//! unchanged can REUSE the prior outputs and skip the whole pull/squash/assemble
//! pipeline. The cache key hashes EVERY input that affects the output bytes; a hit
//! restores the `.tdvmm`, the per-stack initramfs (+ sha sidecar), and the per-stack
//! compose.lock.yml + stack.lock ledgers. `--no-cache` forces a full rebuild (still stored,
//! so later runs hit); nightly bake-repeatability uses it to re-bake unconditionally.

use std::path::{Path, PathBuf};

use crate::artifact;
use super::fsops::tree_hash;
use super::overlay;
use super::pins::COMPOSE_ENGINE_LOCK;
use super::util::{now_nanos, sha256_file_hex};

/// Cache-entry format version. Bump when the cached fileset or key inputs change
/// in a way older entries can't satisfy. v6: the `agent:` line is always the
/// embedded source identity (the release-sha alternative is gone — the agent is
/// only ever built from the embedded source).
pub(super) const CACHE_VERSION: u32 = 6;

pub(super) struct CacheCtx {
    /// The per-key entry directory: <cache-root>/<key>.
    pub(super) dir: PathBuf,
    /// The full hex key (sha256 over the input manifest below).
    pub(super) key: String,
}

/// Resolve the cache directory (Fable Part A). Precedence:
///   `--cache-dir <path>` (the `flag`)  >  `$TDVMM_CACHE_DIR`  >  `$HOME/.tdvmm`.
/// Returns `(dir, source)` where `source` is the provenance word for the log line.
/// Cache entries are disposable, so no migration between locations is needed.
pub(crate) fn resolve_cache_dir(flag: Option<&str>) -> (PathBuf, &'static str) {
    if let Some(f) = flag {
        if !f.is_empty() {
            return (PathBuf::from(f), "--cache-dir flag");
        }
    }
    if let Ok(d) = std::env::var("TDVMM_CACHE_DIR") {
        if !d.is_empty() {
            return (PathBuf::from(d), "TDVMM_CACHE_DIR env");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    (PathBuf::from(home).join(".tdvmm"), "default $HOME/.tdvmm")
}

#[allow(clippy::too_many_arguments)]
pub(super) fn compute_cache_key(
    self_exe: &Path,
    compose_dir: &Path,
    stack_name: &str,
    mem_mib: u64,
    working_set_mib: u64,
    squash_threshold_mib: u64,
    cache_dir: &Path,
    kernel: &Path,
    builders: &[String],
    agent_pin: &str,
) -> Result<CacheCtx, String> {
    let self_sha = sha256_file_hex(self_exe).map_err(|x| format!("hashing tdvmm binary: {x}"))?;
    let kernel_sha = sha256_file_hex(kernel).map_err(|x| format!("hashing kernel: {x}"))?;
    // The agent input is its embedded source identity (`agent_src_id`) — the
    // identity of the agent that ends up in the artifact. The overlay +
    // compose-engine pins are embedded in the binary (also covered by
    // `self_sha`, but kept as named lines so the key form stays legible).
    let overlay_id = overlay::overlay_id();
    let engine_sha = artifact::sha256_hex(COMPOSE_ENGINE_LOCK.as_bytes());
    // the stack dir: compose.yml + build contexts + bind sources + service source,
    // EXCLUDING the lock ledgers (the bake writes those to the cache, but a corpus
    // stack keeps committed compose.lock.yml/stack.lock fixtures in this same dir —
    // they must not perturb the key).
    let stack_tree = tree_hash(compose_dir, &["compose.lock.yml", "stack.lock"])
        .map_err(|x| format!("hashing stack dir: {x}"))?;
    // Fable Part B/guardrail §3: the DECLARED builder-image digests replace the old
    // host-probed `podman --version` — NOTHING host-probed enters the key.
    let builders_line = builders.join(",");

    let manifest = format!(
        "tdvmm-bake-cache v{CACHE_VERSION}\n\
         self:      {self_sha}\n\
         builders:  {builders_line}\n\
         engine:    {engine_sha}\n\
         kernel:    {kernel_sha}\n\
         agent:     {agent_pin}\n\
         overlay:   {overlay_id}\n\
         stackdir:  {stack_tree}\n\
         name:      {stack_name}\n\
         mem:       {mem_mib}\n\
         ws:        {working_set_mib}\n\
         squash:    {squash_threshold_mib}\n"
    );
    let key = artifact::sha256_hex(manifest.as_bytes());
    let dir = cache_dir.join("bake").join(&key);
    Ok(CacheCtx { dir, key })
}

/// The baked files a cache entry holds (basenames within the entry dir).
const CACHE_FILES: [&str; 5] = [
    "artifact.tdvmm",
    "initramfs.cpio.gz",
    "initramfs.cpio.gz.sha256",
    "compose.lock.yml",
    "stack.lock",
];

pub(super) fn cache_is_hit(c: &CacheCtx) -> bool {
    c.dir.is_dir()
        && c.dir.join("tdvmm_sha256").is_file()
        && CACHE_FILES.iter().all(|f| c.dir.join(f).is_file())
}

/// Restore a hit's outputs into place; returns the cached `.tdvmm` sha256.
pub(super) fn cache_restore(
    c: &CacheCtx,
    out_tdvmm: &Path,
    out_initramfs: &Path,
    lock_out: &Path,
    stack_lock: &Path,
) -> std::io::Result<String> {
    for p in [out_tdvmm, out_initramfs, lock_out, stack_lock] {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::copy(c.dir.join("artifact.tdvmm"), out_tdvmm)?;
    std::fs::copy(c.dir.join("initramfs.cpio.gz"), out_initramfs)?;
    std::fs::copy(
        c.dir.join("initramfs.cpio.gz.sha256"),
        format!("{}.sha256", out_initramfs.display()),
    )?;
    std::fs::copy(c.dir.join("compose.lock.yml"), lock_out)?;
    std::fs::copy(c.dir.join("stack.lock"), stack_lock)?;
    Ok(std::fs::read_to_string(c.dir.join("tdvmm_sha256"))?
        .trim()
        .to_string())
}

/// Store the freshly-baked outputs under the key (atomic via temp-dir rename).
/// Returns `Ok(false)` if an entry already exists (nothing to do).
pub(super) fn cache_store(
    c: &CacheCtx,
    out_tdvmm: &Path,
    out_initramfs: &Path,
    lock_out: &Path,
    stack_lock: &Path,
    tdvmm_sha: &str,
) -> std::io::Result<bool> {
    if c.dir.exists() {
        return Ok(false);
    }
    let root = c.dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(root)?;
    let tmp = root.join(format!(".tmp-{}-{}", std::process::id(), now_nanos()));
    std::fs::create_dir_all(&tmp)?;
    std::fs::copy(out_tdvmm, tmp.join("artifact.tdvmm"))?;
    std::fs::copy(out_initramfs, tmp.join("initramfs.cpio.gz"))?;
    let sidecar = PathBuf::from(format!("{}.sha256", out_initramfs.display()));
    if sidecar.exists() {
        std::fs::copy(&sidecar, tmp.join("initramfs.cpio.gz.sha256"))?;
    } else {
        std::fs::write(tmp.join("initramfs.cpio.gz.sha256"), b"")?;
    }
    std::fs::copy(lock_out, tmp.join("compose.lock.yml"))?;
    std::fs::copy(stack_lock, tmp.join("stack.lock"))?;
    std::fs::write(tmp.join("tdvmm_sha256"), format!("{tdvmm_sha}\n"))?;
    // atomic publish; if another builder won the race, drop our temp.
    match std::fs::rename(&tmp, &c.dir) {
        Ok(()) => Ok(true),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(false)
        }
    }
}
