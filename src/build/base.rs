//! shared base-runtime segment cache (Fable Part D)
//!
//! The base runtime (Alpine + podman/crun/conmon/netavark + the agent + the
//! compose CLI) is common to EVERY stack. Its emitted cpio segment is cached here,
//! keyed on DECLARED base pins only, so per-stack bakes concatenate a reused base
//! segment + a fresh stack segment instead of rebuilding the base every time. The
//! base rootfs itself is assembled INSIDE the pinned rootfs-builder container
//! (`apk --root`, NO chroot) by [`build_base_rootfs`].

use std::path::Path;

use crate::artifact;
use crate::engine;
use super::cache::CACHE_VERSION;
use super::fsops::tree_hash;
use super::util::{now_nanos, ScratchDir};
use super::ux::{run, Ux};
use super::{ALPINE_VER, BUILD_EPOCH, MINIROOTFS_SHA256, PKGS};

/// Build the base Alpine rootfs INSIDE the pinned rootfs-builder container (Move 3
/// Step C — replaces the in-`unshare` chroot apk). Extracts the (already
/// sha-verified) minirootfs, writes the pinned repositories, installs `PKGS` via
/// `apk --root` (NO chroot anywhere), records the resolved package set, clears
/// `/dev` (the cpio packer supplies device nodes), and tars the rootfs to
/// `<out_dir>/base-rootfs.tar` (+ `<out_dir>/packages.lock`). The tar is untrusted
/// TRANSPORT (Fable guardrail §2): `__assemble` re-packs it via the normalizing
/// cpio emitter, so container ownership / order / mtime are all discarded. Routed
/// through the engine choke point; OutputMode comes from the orchestrator.
pub(super) fn build_base_rootfs(
    img_ref: &str,
    minirootfs: &Path,
    mirror: &str,
    alpine_branch: &str,
    pkgs: &[&str],
    out_dir: &Path,
    ux: &Ux,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(out_dir)?;
    let confdir = ScratchDir::new()?;
    let conf = confdir.path().join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    let mini_name = minirootfs.file_name().unwrap().to_string_lossy().into_owned();
    let pkg_args = pkgs.join(" ");
    let script = format!(
        "set -e\n\
         mkdir -p /rootfs\n\
         tar -C /rootfs -xzf /in/{mini_name}\n\
         mkdir -p /rootfs/etc/apk\n\
         printf '%s/%s/main\\n%s/%s/community\\n' '{mirror}' '{alpine_branch}' '{mirror}' '{alpine_branch}' > /rootfs/etc/apk/repositories\n\
         printf 'nameserver 1.1.1.1\\nnameserver 8.8.8.8\\n' > /rootfs/etc/resolv.conf\n\
         apk --root /rootfs update\n\
         apk --root /rootfs add --no-progress {pkg_args}\n\
         apk --root /rootfs list -I | awk '{{print $1}}' | LC_ALL=C sort > /out/packages.lock\n\
         rm -rf /rootfs/dev && mkdir -p /rootfs/dev\n\
         tar -cf /out/base-rootfs.tar -C /rootfs .\n"
    );
    run(engine::command()
        .env("CONTAINERS_CONF", &conf)
        .arg("run")
        .arg("--rm")
        .arg("-v")
        .arg(format!("{}:/in/{}:ro", minirootfs.display(), mini_name))
        .arg("-v")
        .arg(format!("{}:/out", out_dir.display()))
        .arg(img_ref)
        .args(["sh", "-c", &script]), ux.mode)?;
    Ok(())
}

/// The base-runtime cache key: DECLARED base pins only (never the stack).
pub(super) fn compute_base_key(
    alpine_dir: &Path,
    agent_sha: &str,
    compose_version: &str,
    compose_sha256: &str,
    selftest_pin: &str,
    rootfs_builder: &str,
) -> String {
    let overlay_tree = tree_hash(&alpine_dir.join("overlay"), &[]).unwrap_or_default();
    let pkgs = PKGS.join(",");
    let manifest = format!(
        "dvmm-base-runtime v{CACHE_VERSION}\n\
         alpine:     {ALPINE_VER}\n\
         minirootfs: {MINIROOTFS_SHA256}\n\
         epoch:      {BUILD_EPOCH}\n\
         pkgs:       {pkgs}\n\
         overlay:    {overlay_tree}\n\
         agent:      {agent_sha}\n\
         compose:    {compose_version} {compose_sha256}\n\
         builder:    {rootfs_builder}\n\
         selftest:   {selftest_pin}\n"
    );
    artifact::sha256_hex(manifest.as_bytes())
}

/// Store a freshly-emitted base segment (+ its package set) under the key (atomic
/// temp-dir rename). No-op if an entry already exists.
pub(super) fn base_cache_store(entry: &Path, base_seg: &Path, packages_lock: &Path) -> std::io::Result<()> {
    if entry.exists() {
        return Ok(());
    }
    let root = entry.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(root)?;
    let tmp = root.join(format!(".tmp-{}-{}", std::process::id(), now_nanos()));
    std::fs::create_dir_all(&tmp)?;
    std::fs::copy(base_seg, tmp.join("base.cpio"))?;
    if packages_lock.is_file() {
        std::fs::copy(packages_lock, tmp.join("packages.lock"))?;
    }
    match std::fs::rename(&tmp, entry) {
        Ok(()) => Ok(()),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(())
        }
    }
}
