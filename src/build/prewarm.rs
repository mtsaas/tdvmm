//! `tdvmm doctor`'s cache pre-warm: make sure every guest build dependency is
//! already built/fetched so the first real `tdvmm build` is fast. Thin
//! orchestration over the bake pipeline's OWN primitives ([`ensure_kernel`],
//! [`ensure_agent`], [`fetch_verify`]) — nothing is built or fetched any other
//! way, and the bake itself is untouched.

use std::path::Path;

use crate::engine;
use crate::ui;
use super::agent::ensure_agent;
use super::kernel::ensure_kernel;
use super::pins::{compose_engine_pin, fetch_verify};
use super::ux::Ux;
use super::{ALPINE_BRANCH, DEFAULT_MIRROR, MINIROOTFS, MINIROOTFS_SHA256};

/// Pre-warm step count: kernel, agent, minirootfs, compose CLI.
const STEPS: u32 = 4;

/// Build/fetch the four cacheable guest build inputs into `cache_dir`, through
/// the same progress UI `tdvmm build` uses (the two container compiles stream
/// their live output into the viewport tail). Idempotent: warm entries are
/// sha-verified and reused, exactly as in a bake.
pub fn prewarm(cache_dir: &Path, progress: &ui::Progress) -> Result<(), Box<dyn std::error::Error>> {
    let mode = if progress.active() {
        engine::OutputMode::CaptureOnFailure
    } else {
        engine::OutputMode::Inherit
    };
    let ux = Ux { progress, mode };

    ux.progress.step(1, STEPS, "guest kernel");
    ensure_kernel(cache_dir, false, &ux)?;
    ux.progress.step(2, STEPS, "guest agent");
    ensure_agent(cache_dir, &ux)?;

    // The two pinned downloads, cached under <cache>/downloads exactly where
    // the bake looks for them (URL construction mirrors `cmd_build`).
    let downloads = cache_dir.join("downloads");
    ux.progress.step(3, STEPS, "alpine minirootfs");
    let mirror = std::env::var("ALPINE_MIRROR").unwrap_or_else(|_| DEFAULT_MIRROR.to_string());
    let mini_url = format!("{mirror}/{ALPINE_BRANCH}/releases/x86_64/{MINIROOTFS}");
    fetch_cached(&downloads.join(MINIROOTFS), &mini_url, MINIROOTFS_SHA256, &ux)?;
    ux.progress.step(4, STEPS, "compose cli");
    let (version, sha) = compose_engine_pin()?;
    let compose_url = format!(
        "https://github.com/docker/compose/releases/download/{version}/docker-compose-linux-x86_64"
    );
    fetch_cached(&downloads.join(format!("docker-compose-{version}")), &compose_url, &sha, &ux)?;
    ux.progress.finish_steps();
    Ok(())
}

/// [`fetch_verify`] plus a step note saying whether the file was already there.
fn fetch_cached(dest: &Path, url: &str, sha: &str, ux: &Ux) -> Result<(), Box<dyn std::error::Error>> {
    let cached = dest.is_file();
    fetch_verify(dest, url, sha, ux)?;
    ux.progress.note(if cached { "cached (sha verified)" } else { "fetched + sha verified" });
    ux.progress.detail(format!("   download: {} (sha256 verified)", dest.display()));
    Ok(())
}
