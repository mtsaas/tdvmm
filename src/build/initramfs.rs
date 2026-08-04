//! __assemble-initramfs (runs inside `podman unshare`)

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cpio;
use super::fsops::{copy_dir_contents, copy_tree, extract_tar, install_file, set_mode, walk_files};
use super::util::sha256_file_hex;

#[derive(Serialize, Deserialize)]
pub(super) struct AssembleConfig {
    pub(super) conf: PathBuf,
    pub(super) work: PathBuf,
    /// The base rootfs tarball produced by the pinned rootfs-builder container
    /// (Move 3 Step C). Extracted in-process on a base-cache MISS; unused on a HIT
    /// (the cached cpio segment is read instead). Untrusted transport (§2).
    pub(super) base_rootfs_tar: PathBuf,
    pub(super) build_epoch: String,
    pub(super) overlay: PathBuf,
    pub(super) compose_cache: PathBuf,
    pub(super) agent_bin: PathBuf,
    pub(super) seed_storage: PathBuf,
    pub(super) selftest_image_ref: String,
    pub(super) stack_name: String,
    pub(super) stack_project: String,
    pub(super) stack_lock: PathBuf,
    pub(super) stack_binds: PathBuf,
    pub(super) stack_mem: u64,
    pub(super) out: PathBuf,
    pub(super) packages_lock_out: PathBuf,
    // --- Fable Part D: shared base-runtime cpio segment ---
    /// True when the base runtime segment (Alpine + podman/crun/... + agent +
    /// compose) is already cached: skip the expensive base BUILD and reuse the
    /// cached segment. False forces a full base build (also stored, for later).
    pub(super) base_hit: bool,
    /// Base segment path. HIT: the cached `base.cpio` to READ. MISS: where to WRITE
    /// the freshly-emitted base segment (the host then stores it to the cache).
    pub(super) base_segment: PathBuf,
    /// On a HIT, the cached `packages.lock` to restore (the base's package set is
    /// stack-independent, so it is part of the base segment's cache entry).
    pub(super) base_packages_lock: PathBuf,
}

/// Build the BASE runtime tree (Fable Part D): Alpine minirootfs + the pinned
/// container stack (apk) + overlay + compose CLI + agent + fixed epoch/self-test
/// ref, MINUS every stack-specific path. This is the expensive, stack-independent
/// layer whose emitted cpio segment is cached and reused across stacks.
fn assemble_base_tree(cfg: &AssembleConfig, rootfs: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(rootfs)?;

    // 1-3. extract the container-built base rootfs: the pinned Alpine minirootfs +
    //       the apk-installed package set (PKGS), produced by the pinned rootfs-
    //       builder container via `apk --root` on the host (Move 3 Step C — NO
    //       chroot, no host apk). The tar is untrusted TRANSPORT (Fable guardrail
    //       §2): the cpio packer below re-derives uid/gid 0 + epoch + sort order +
    //       hardlink groups, so nothing about the container tar's bytes is trusted.
    //       The resolved package ledger (packages.lock) is emitted by that same
    //       container and staged to work/ by the host before this runs.
    extract_tar(&cfg.base_rootfs_tar, rootfs)?;

    // 4. drop the overlay (init, self-test, compose launcher, podman config).
    copy_dir_contents(&cfg.overlay, rootfs)?;
    for f in [
        "init",
        "usr/local/bin/container-selftest.sh",
        "usr/local/bin/compose-up.sh",
        "usr/local/bin/healthcheck-ticker.sh",
    ] {
        set_mode(&rootfs.join(f), 0o755)?;
    }
    // 4b. bake the genuine Docker Compose v2 CLI.
    install_file(&cfg.compose_cache, &rootfs.join("usr/local/bin/docker-compose"), 0o755)?;
    // 4c. bake the control-channel agent.
    install_file(&cfg.agent_bin, &rootfs.join("usr/local/bin/dvmm-agent"), 0o755)?;

    // 5. fixed clock epoch + self-test image ref (both stack-independent = base).
    std::fs::write(rootfs.join("etc/dvmm-build-epoch"), format!("{}\n", cfg.build_epoch))?;
    std::fs::write(rootfs.join("etc/dvmm-image-ref"), format!("{}\n", cfg.selftest_image_ref))?;

    // 7. trim install-time cruft that would only bloat RAM (base part).
    let _ = remove_glob(&rootfs.join("var/cache/apk"));
    let _ = std::fs::remove_file(rootfs.join("etc/resolv.conf"));
    let _ = std::fs::remove_dir_all(rootfs.join("root/.config/containers"));
    std::fs::write(rootfs.join("etc/resolv.conf"), "")?; // empty mount target

    // 7a. zero base lock files (apk db lock, etc — random per-writer tokens).
    truncate_locks(rootfs)?;
    Ok(())
}

/// Build the STACK-specific tree (Fable Part D) in a dedicated dir: ONLY the
/// per-stack paths — the seed image store, the compose.lock + materialized binds,
/// and the stack env files. Its emitted cpio segment is concatenated AFTER the
/// (possibly cached) base segment; on extraction the two segments merge into the
/// same rootfs. Built identically whether or not the base was a cache hit, so the
/// final initramfs is byte-identical either way.
fn assemble_stack_tree(cfg: &AssembleConfig, rootfs: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(rootfs)?;

    // Scaffolding dirs (explicit 0755 so modes never vary with the ambient umask).
    for d in ["var", "var/lib", "etc", "var/lib/dvmm-stack", "var/lib/dvmm-stack/binds", "var/lib/containers-seed"] {
        std::fs::create_dir_all(rootfs.join(d))?;
        set_mode(&rootfs.join(d), 0o755)?;
    }

    // compose.lock + materialized binds + pinned project.
    std::fs::copy(&cfg.stack_lock, rootfs.join("var/lib/dvmm-stack/compose.lock.yml"))?;
    set_mode(&rootfs.join("var/lib/dvmm-stack/compose.lock.yml"), 0o644)?;
    if cfg.stack_binds.is_dir() {
        // cp -a "$STACK_BINDS/." binds/  (may be empty; ignore failure like the script)
        let _ = copy_dir_contents(&cfg.stack_binds, &rootfs.join("var/lib/dvmm-stack/binds"));
    }
    std::fs::write(rootfs.join("etc/dvmm-stack-name"), format!("{}\n", cfg.stack_name))?;
    std::fs::write(rootfs.join("etc/dvmm-stack-project"), format!("{}\n", cfg.stack_project))?;
    std::fs::write(rootfs.join("etc/dvmm-stack-mem"), format!("{}\n", cfg.stack_mem))?;

    // the seed store: the pre-baked image graph the guest copies into its tmpfs.
    copy_tree(&cfg.seed_storage, &rootfs.join("var/lib/containers-seed/storage"))?;

    // normalize containers/storage "created" timestamps + zero its lock files.
    normalize_created_json(&rootfs.join("var/lib/containers-seed"))?;
    truncate_locks(rootfs)?;
    Ok(())
}

pub fn cmd_assemble_initramfs(config: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let cfg: AssembleConfig = serde_json::from_slice(&std::fs::read(config)?)?;

    // --- base runtime segment (Fable Part D): reuse if cached, else build+emit ---
    let base_seg: Vec<u8> = if cfg.base_hit {
        eprintln!("   [base] cache HIT — reusing base-runtime segment (skipped apk/overlay)");
        // restore the base's package set (stack-independent).
        if cfg.base_packages_lock.is_file() {
            let _ = std::fs::copy(&cfg.base_packages_lock, cfg.work.join("packages.lock"));
        }
        std::fs::read(&cfg.base_segment)?
    } else {
        eprintln!("   [base] cache MISS — building base-runtime tree");
        let base_root = cfg.work.join("rootfs-base");
        assemble_base_tree(&cfg, &base_root)?;
        let seg = cpio::rootfs_segment(&base_root)?;
        // hand the fresh segment to the host to store in the base cache.
        std::fs::write(&cfg.base_segment, &seg)?;
        seg
    };

    // --- stack-specific segment (always built fresh; cheap relative to the base) ---
    let stack_root = cfg.work.join("rootfs-stack");
    assemble_stack_tree(&cfg, &stack_root)?;
    let stack_seg = cpio::rootfs_segment(&stack_root)?;

    // --- assemble: nodes + base + stack, then gzip -9 -n (Fable guardrail §4:
    //     the bytes come ONLY from dvmm's own normalizing cpio emitter) ---
    let mut combined = cpio::nodes_segment();
    combined.extend_from_slice(&base_seg);
    combined.extend_from_slice(&stack_seg);
    cpio::gzip_to(&combined, &cfg.out)?;

    // copy packages.lock next to the (now-retired) build script location.
    let pl = cfg.work.join("packages.lock");
    if pl.is_file() {
        std::fs::copy(&pl, &cfg.packages_lock_out)?;
    }

    // artifact sha sidecar (build_rootfs.sh parity).
    let art_sha = sha256_file_hex(&cfg.out)?;
    std::fs::write(
        format!("{}.sha256", cfg.out.display()),
        format!("{art_sha}  {}\n", cfg.out.file_name().unwrap().to_string_lossy()),
    )?;

    Ok(0)
}

fn remove_glob(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
    }
    Ok(())
}

/// sed -i -E 's/"created":"<ts>"/"created":"2026-08-01T00:00:00Z"/g' on *.json.
fn normalize_created_json(dir: &Path) -> std::io::Result<()> {
    let re = regex::Regex::new(
        r#""created":"[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9:.]+Z""#,
    )
    .unwrap();
    for path in walk_files(dir)? {
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if re.is_match(&text) {
                    let new = re.replace_all(&text, r#""created":"2026-08-01T00:00:00Z""#);
                    std::fs::write(&path, new.as_bytes())?;
                }
            }
        }
    }
    Ok(())
}

/// truncate -s 0 every *.lock file under root.
fn truncate_locks(root: &Path) -> std::io::Result<()> {
    for path in walk_files(root)? {
        if path.extension().and_then(|s| s.to_str()) == Some("lock") {
            std::fs::write(&path, b"")?;
        }
    }
    Ok(())
}
