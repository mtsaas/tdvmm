//! builder-image pins (Fable Part B) — the DECLARED, host-identical toolchain
//! anchors that go into the hashed manifest + the cache key (replacing the
//! host-probed podman version), plus the in-container fetch helpers (Move 3) that
//! acquire+verify the pinned rootfs/compose/kernel transport bytes. The pin
//! ledgers are embedded at compile time, so none of this needs a checkout.

use std::path::Path;

use crate::engine;
use super::kernel::embedded_kernel_lock;
use super::util::{sha256_file_hex, ScratchDir};
use super::ux::{run, Ux};

/// The pinned Alpine rootfs-builder ledger (`guest/initramfs-alpine/
/// rootfs-builder.lock`), embedded. This image assembles the base rootfs
/// (`apk --root`) AND serves as the fetch container (busybox `wget`).
const ROOTFS_BUILDER_LOCK: &str = include_str!("../../guest/initramfs-alpine/rootfs-builder.lock");

/// The pinned rust+musl agent-builder ledger (`tdvmm-agent/images.lock`),
/// embedded. Deliberately INSIDE the tree `agent_src_id` hashes: a builder
/// bump is a real toolchain change and must change the agent's build hash.
const AGENT_IMAGES_LOCK: &str = include_str!("../../tdvmm-agent/images.lock");

/// The pinned Docker Compose CLI ledger (`guest/initramfs-alpine/
/// compose-engine.lock`), embedded.
pub(super) const COMPOSE_ENGINE_LOCK: &str =
    include_str!("../../guest/initramfs-alpine/compose-engine.lock");

/// Parse `KEY=value` lines out of a pin ledger, returning the values for `keys`
/// in order. Missing keys come back as empty strings — callers decide what is
/// required.
pub(super) fn lock_values<const N: usize>(text: &str, keys: [&str; N]) -> [String; N] {
    let mut out = std::array::from_fn(|_| String::new());
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let Some((key, val)) = l.split_once('=') else { continue };
        if let Some(i) = keys.iter().position(|k| *k == key.trim()) {
            out[i] = val.trim().to_string();
        }
    }
    out
}

/// The pinned Alpine rootfs-builder image ref + digest (Move 3 Step C), from the
/// embedded ledger. Returns `(image, digest)`.
pub(super) fn rootfs_builder_pin() -> Result<(String, String), Box<dyn std::error::Error>> {
    let [image, digest] = lock_values(ROOTFS_BUILDER_LOCK, ["BUILDER_IMAGE", "BUILDER_DIGEST"]);
    if image.is_empty() || digest.is_empty() {
        return Err("rootfs-builder.lock missing BUILDER_IMAGE / BUILDER_DIGEST".into());
    }
    Ok((image, digest))
}

/// The pinned rust+musl agent-builder image ref + digest (Fable §2), from the
/// embedded ledger. Returns `(image, digest)`.
pub(super) fn agent_builder_pin() -> Result<(String, String), Box<dyn std::error::Error>> {
    let [image, digest] = lock_values(AGENT_IMAGES_LOCK, ["BUILDER_IMAGE", "BUILDER_DIGEST"]);
    if image.is_empty() || digest.is_empty() {
        return Err("tdvmm-agent/images.lock missing BUILDER_IMAGE / BUILDER_DIGEST".into());
    }
    Ok((image, digest))
}

/// The pinned Docker Compose CLI `(version, sha256)` from the embedded ledger.
pub(super) fn compose_engine_pin() -> Result<(String, String), Box<dyn std::error::Error>> {
    let [version, sha] = lock_values(COMPOSE_ENGINE_LOCK, ["COMPOSE_VERSION", "COMPOSE_SHA256"]);
    if version.is_empty() || sha.is_empty() {
        return Err("compose-engine.lock missing COMPOSE_VERSION / COMPOSE_SHA256".into());
    }
    Ok((version, sha))
}

/// Download `url` into `dest` from INSIDE the pinned Alpine container (Move 3
/// Step A — replaces host `curl`). `dest`'s parent dir is bind-mounted at `/cache`
/// and busybox `wget` writes the file there over HTTPS; the CALLER sha256-verifies
/// it in-process afterward (Fable guardrail §6 — no host TLS stack is linked, and
/// the container transport is never trusted). Routed through the engine choke
/// point (guardrail §3); the child's OutputMode comes from the orchestrator.
pub(super) fn fetch_in_container(dest: &Path, url: &str, ux: &Ux) -> Result<(), Box<dyn std::error::Error>> {
    let (image, digest) = rootfs_builder_pin()?;
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

/// The pinned builder-image refs (`image@sha256`) for the guest binaries: the
/// musl agent builder (images.lock) + the kernel builder (kernel.lock) + the
/// rootfs builder — all from the embedded ledgers. Sorted.
pub(super) fn collect_builder_pins() -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let (aimg, adig) = agent_builder_pin()?;
    let kl = embedded_kernel_lock()?;
    if kl.builder_digest.is_empty() {
        return Err(
            "kernel.lock has no BUILDER_DIGEST; run `tdvmm build-kernel --record` first".into(),
        );
    }
    let (rimg, rdig) = rootfs_builder_pin()?;
    let mut v = vec![
        format!("{aimg}@{adig}"),
        format!("{}@{}", kl.builder_image, kl.builder_digest),
        format!("{rimg}@{rdig}"),
    ];
    v.sort();
    Ok(v)
}
