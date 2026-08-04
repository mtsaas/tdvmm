//! builder-image pins (Fable Part B) — the DECLARED, host-identical toolchain
//! anchors that go into the hashed manifest + the cache key (replacing the
//! host-probed podman version), plus the in-container fetch helpers (Move 3) that
//! acquire+verify the pinned rootfs/compose/kernel transport bytes.

use std::path::Path;

use crate::engine;
use super::kernel::read_kernel_lock;
use super::util::{self_here, sha256_file_hex, ScratchDir};
use super::ux::{run, Ux};

/// Read the pinned rust+musl builder image ref + digest from
/// `dvmm-agent/images.lock` (Fable §2). Returns `(image, digest)`.
pub(super) fn read_builder_pin(repo_root: &Path) -> Result<(String, String), Box<dyn std::error::Error>> {
    let lock = repo_root.join("dvmm-agent/images.lock");
    let text =
        std::fs::read_to_string(&lock).map_err(|e| format!("reading {}: {e}", lock.display()))?;
    let mut image = String::new();
    let mut digest = String::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("BUILDER_IMAGE=") {
            image = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("BUILDER_DIGEST=") {
            digest = v.trim().to_string();
        }
    }
    if image.is_empty() || digest.is_empty() {
        return Err("dvmm-agent/images.lock missing BUILDER_IMAGE / BUILDER_DIGEST".into());
    }
    Ok((image, digest))
}

/// Read the pinned Alpine rootfs-builder image ref + digest from
/// `guest/initramfs-alpine/rootfs-builder.lock` (Move 3 Step C). This image
/// assembles the base rootfs (`apk --root`) AND serves as the fetch container
/// (busybox `wget`). Returns `(image, digest)`.
pub(super) fn read_rootfs_builder_pin(here: &Path) -> Result<(String, String), Box<dyn std::error::Error>> {
    let lock = here.join("initramfs-alpine/rootfs-builder.lock");
    let text =
        std::fs::read_to_string(&lock).map_err(|e| format!("reading {}: {e}", lock.display()))?;
    let mut image = String::new();
    let mut digest = String::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("BUILDER_IMAGE=") {
            image = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("BUILDER_DIGEST=") {
            digest = v.trim().to_string();
        }
    }
    if image.is_empty() || digest.is_empty() {
        return Err("rootfs-builder.lock missing BUILDER_IMAGE / BUILDER_DIGEST".into());
    }
    Ok((image, digest))
}

/// Download `url` into `dest` from INSIDE the pinned Alpine container (Move 3
/// Step A — replaces host `curl`). `dest`'s parent dir is bind-mounted at `/cache`
/// and busybox `wget` writes the file there over HTTPS; the CALLER sha256-verifies
/// it in-process afterward (Fable guardrail §6 — no host TLS stack is linked, and
/// the container transport is never trusted). Routed through the engine choke
/// point (guardrail §3); the child's OutputMode comes from the orchestrator.
pub(super) fn fetch_in_container(dest: &Path, url: &str, ux: &Ux) -> Result<(), Box<dyn std::error::Error>> {
    let here = self_here()?;
    let (image, digest) = read_rootfs_builder_pin(&here)?;
    let img_ref = format!("{image}@{digest}");
    let dir = dest
        .parent()
        .ok_or_else(|| format!("fetch destination {} has no parent dir", dest.display()))?;
    let name = dest
        .file_name()
        .ok_or_else(|| format!("fetch destination {} has no file name", dest.display()))?
        .to_string_lossy()
        .into_owned();
    std::fs::create_dir_all(dir)?;
    let confdir = ScratchDir::new()?;
    let conf = confdir.path().join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    let target = format!("/cache/{name}");
    run(engine::command()
        .env("CONTAINERS_CONF", &conf)
        .arg("run")
        .arg("--rm")
        .arg("-v")
        .arg(format!("{}:/cache", dir.display()))
        .arg(&img_ref)
        .args(["wget", "-q", "-O", &target, url]), ux.mode)?;
    Ok(())
}

pub(super) fn fetch_verify(path: &Path, url: &str, sha: &str, ux: &Ux) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        ux.progress.detail(format!("downloading {url} ..."));
        fetch_in_container(path, url, ux)?;
    }
    let got = sha256_file_hex(path)?;
    if got != sha {
        return Err(format!("sha256 mismatch for {}: got {got}, want {sha}", path.display()).into());
    }
    Ok(())
}

pub(super) fn read_compose_lock(alpine_dir: &Path) -> Result<(String, String), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(alpine_dir.join("compose-engine.lock"))?;
    let mut version = String::new();
    let mut sha = String::new();
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("COMPOSE_VERSION=") {
            version = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("COMPOSE_SHA256=") {
            sha = v.trim().to_string();
        }
    }
    Ok((version, sha))
}

/// The pinned builder-image refs (`image@sha256`) for the guest binaries: the
/// musl agent builder (`dvmm-agent/images.lock`) + the kernel builder
/// (`guest/kernel/kernel.lock`). Sorted.
pub(super) fn collect_builder_pins(
    repo_root: &Path,
    here: &Path,
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let (aimg, adig) = read_builder_pin(repo_root)?;
    let kl = read_kernel_lock(here)?;
    if kl.builder_digest.is_empty() {
        return Err(
            "kernel.lock has no BUILDER_DIGEST; run `dvmm build-kernel --record` first".into(),
        );
    }
    let (rimg, rdig) = read_rootfs_builder_pin(here)?;
    let mut v = vec![
        format!("{aimg}@{adig}"),
        format!("{}@{}", kl.builder_image, kl.builder_digest),
        format!("{rimg}@{rdig}"),
    ];
    v.sort();
    Ok(v)
}
