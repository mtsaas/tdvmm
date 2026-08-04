//! kernel (Fable Part C) — reproducible containerized build + fetch-with-fallback
//!
//! The guest kernel is acquired EITHER by fetching the pinned GitHub release asset
//! (PRIMARY, sha256-verified against kernel.lock) OR by a reproducible build inside
//! the pinned builder container (FALLBACK). Both paths MUST yield the byte-identical
//! vmlinux recorded in kernel.lock. No host kernel toolchain is required.

use std::path::{Path, PathBuf};

use crate::compose;
use crate::engine;
use crate::ui;
use super::cache::resolve_cache_dir;
use super::pins::{fetch_in_container, fetch_verify};
use super::util::{self_here, sha256_file_hex, ScratchDir};
use super::ux::{capture, run, Ux};
use super::{BuildKernelArgs, BUILD_EPOCH};

/// The kernel config baked into the guest (Firecracker microvm config, HPET off,
/// all built-in). Lives in `guest/kernel/`; hashed into kernel.lock.
const KERNEL_CONFIG_NAME: &str = "microvm-kernel-x86_64-6.1.config";

/// The reproducibility ledger for the guest kernel (`guest/kernel/kernel.lock`).
/// Empty `sha256`/`source_sha256`/`builder_digest` mean "not yet recorded" — the
/// `--record` bootstrap fills them.
#[derive(Default, Clone)]
pub(super) struct KernelLock {
    version: String,
    sha256: String,
    config_sha256: String,
    source_url: String,
    source_sha256: String,
    pub(super) builder_image: String,
    pub(super) builder_digest: String,
    release_asset_url: String,
    release_asset_name: String,
}

fn kernel_lock_path(here: &Path) -> PathBuf {
    here.join("kernel/kernel.lock")
}

pub(super) fn read_kernel_lock(here: &Path) -> Result<KernelLock, Box<dyn std::error::Error>> {
    let path = kernel_lock_path(here);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading {}: {e} (run `dvmm build-kernel --record`)", path.display()))?;
    let mut k = KernelLock::default();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        let Some((key, val)) = l.split_once('=') else { continue };
        let val = val.trim().to_string();
        match key.trim() {
            "KERNEL_VERSION" => k.version = val,
            "KERNEL_SHA256" => k.sha256 = val,
            "KERNEL_CONFIG_SHA256" => k.config_sha256 = val,
            "KERNEL_SOURCE_URL" => k.source_url = val,
            "KERNEL_SOURCE_SHA256" => k.source_sha256 = val,
            "BUILDER_IMAGE" => k.builder_image = val,
            "BUILDER_DIGEST" => k.builder_digest = val,
            "RELEASE_ASSET_URL" => k.release_asset_url = val,
            "RELEASE_ASSET_NAME" => k.release_asset_name = val,
            _ => {}
        }
    }
    if k.version.is_empty() {
        return Err(format!("{} missing KERNEL_VERSION", path.display()).into());
    }
    Ok(k)
}

fn write_kernel_lock(here: &Path, k: &KernelLock) -> Result<(), Box<dyn std::error::Error>> {
    let body = format!(
        "# dvmm guest kernel pin (Fable Part C).\n\
         #\n\
         # The guest vmlinux is acquired EITHER by fetching the pinned GitHub release\n\
         # asset (PRIMARY, verified against KERNEL_SHA256) OR by a reproducible build in\n\
         # the pinned builder container (FALLBACK, also verified). Both paths yield the\n\
         # byte-identical kernel recorded here. No host kernel toolchain is required.\n\
         #\n\
         # Regenerate with:  dvmm build-kernel --record\n\
         KERNEL_VERSION={}\n\
         KERNEL_SHA256={}\n\
         KERNEL_CONFIG_SHA256={}\n\
         KERNEL_SOURCE_URL={}\n\
         KERNEL_SOURCE_SHA256={}\n\
         BUILDER_IMAGE={}\n\
         BUILDER_DIGEST={}\n\
         RELEASE_ASSET_URL={}\n\
         RELEASE_ASSET_NAME={}\n",
        k.version, k.sha256, k.config_sha256, k.source_url, k.source_sha256,
        k.builder_image, k.builder_digest, k.release_asset_url, k.release_asset_name,
    );
    std::fs::write(kernel_lock_path(here), body)?;
    Ok(())
}

/// Ensure the guest kernel is present at `guest/kernel/vmlinux-<version>` and
/// matches kernel.lock. PRIMARY: fetch the pinned release asset (sha-verified).
/// FALLBACK: reproducible container build (sha-verified). Returns the path.
pub(super) fn ensure_kernel(
    here: &Path,
    cache_dir: &Path,
    force_build: bool,
    ux: &Ux,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let kl = read_kernel_lock(here)?;
    let out = here.join(format!("kernel/vmlinux-{}", kl.version));
    if kl.sha256.is_empty() {
        return Err(
            "kernel.lock has no KERNEL_SHA256; run `dvmm build-kernel --record` first".into(),
        );
    }

    // Already present + verified? (the common case for every bake after the first.)
    if !force_build && out.exists() {
        if let Ok(got) = sha256_file_hex(&out) {
            if got == kl.sha256 {
                ux.progress.detail(format!("   kernel: {} (present, sha256 verified)", out.display()));
                return Ok(out);
            }
            ux.progress.println(format!(
                "{}: kernel at {} sha256 {} != kernel.lock {}; re-acquiring",
                compose::WARN, out.display(), &got[..16], &kl.sha256[..16]
            ));
        }
    }

    // PRIMARY: pinned release asset.
    if !force_build && !kl.release_asset_url.is_empty() {
        ux.progress.detail(format!("   kernel: fetching pinned release asset {} ...", kl.release_asset_url));
        let _ = std::fs::remove_file(&out);
        match fetch_verify(&out, &kl.release_asset_url, &kl.sha256, ux) {
            Ok(()) => {
                ux.progress.detail("   kernel: fetched + sha256 verified from release");
                return Ok(out);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&out);
                ux.progress.println(format!(
                    "{}: release fetch failed ({e}); falling back to reproducible container build",
                    compose::WARN
                ));
            }
        }
    }

    // FALLBACK: reproducible container build.
    build_kernel_container(here, cache_dir, &kl, &out, ux)?;
    let got = sha256_file_hex(&out)?;
    if got != kl.sha256 {
        return Err(format!(
            "container-built kernel sha256 {got} != kernel.lock {}; the build is not reproducing \
             the recorded kernel (re-run `dvmm build-kernel --record` if inputs changed)",
            kl.sha256
        )
        .into());
    }
    ux.progress.detail("   kernel: container build sha256 verified against kernel.lock");
    Ok(out)
}

/// Reproducibly build vmlinux inside the pinned builder container (no host kernel
/// toolchain). Faithfully ports `build_kernel.sh` — including the `-std=gnu11` CC
/// wrapper — with build_agent's determinism knobs (pinned image, SOURCE_DATE_EPOCH,
/// fixed KBUILD_BUILD_* + build-id). Source is fetched+verified on the host and
/// bind-mounted; the compiler is pinned by the image digest.
fn build_kernel_container(
    here: &Path,
    cache_dir: &Path,
    kl: &KernelLock,
    out: &Path,
    ux: &Ux,
) -> Result<(), Box<dyn std::error::Error>> {
    if kl.builder_digest.is_empty() {
        return Err("kernel.lock has no BUILDER_DIGEST; run `dvmm build-kernel --record`".into());
    }
    let img_ref = format!("{}@{}", kl.builder_image, kl.builder_digest);
    ux.progress.detail(format!("   kernel: reproducible container build in {img_ref}"));

    // 1. source tarball: fetch + verify on the host, cached in the cache dir.
    let src_dir = cache_dir.join("kernel-src");
    std::fs::create_dir_all(&src_dir)?;
    let tarball = src_dir.join(format!("linux-{}.tar.xz", kl.version));
    if kl.source_sha256.is_empty() {
        // --record bootstrap: fetch without a pre-known sha (recorded afterwards).
        if !tarball.exists() {
            ux.progress.detail(format!("downloading {} ...", kl.source_url));
            fetch_in_container(&tarball, &kl.source_url, ux)?;
        }
    } else {
        fetch_verify(&tarball, &kl.source_url, &kl.source_sha256, ux)?;
    }

    // 2. runc conf (host default runtime is misconfigured — Fable host fact) + work.
    let confdir = ScratchDir::new()?;
    let conf = confdir.path().join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    let work_guard = ScratchDir::new()?;
    let work = work_guard.path();
    let config_src = here.join("kernel").join(KERNEL_CONFIG_NAME);

    // 3. the in-container build script — a faithful port of build_kernel.sh with
    //    reproducibility knobs. KBUILD_BUILD_TIMESTAMP/USER/HOST + SOURCE_DATE_EPOCH
    //    + no build-id pin every timestamp/identity that would otherwise leak.
    let ver = &kl.version;
    let script = format!(
        "set -e\n\
         export DEBIAN_FRONTEND=noninteractive\n\
         apt-get update -qq\n\
         apt-get install -y --no-install-recommends build-essential bc bison flex libelf-dev libssl-dev xz-utils >/dev/null\n\
         cd /work\n\
         tar -xf /src/linux-{ver}.tar.xz\n\
         cd linux-{ver}\n\
         # Modern GCC defaults to C23 (bool/true/false keywords); Linux 6.1's real-\n\
         # mode boot stub predates that. Force -std=gnu11 for EVERY TU via a CC wrapper\n\
         # (build_kernel.sh parity), so the build works regardless of the image's GCC.\n\
         printf '#!/bin/sh\\nexec gcc -std=gnu11 \"$@\"\\n' > .cc-gnu11\n\
         chmod +x .cc-gnu11\n\
         cp /config .config\n\
         make CC=$PWD/.cc-gnu11 olddefconfig\n\
         make -j\"$(nproc)\" CC=$PWD/.cc-gnu11 vmlinux\n\
         cp vmlinux /work/vmlinux-out\n"
    );

    // 4. run. Fixed build identity for byte-reproducibility.
    let ts = "Thu Jan  1 00:00:00 UTC 1970"; // stable KBUILD banner timestamp
    run(engine::command()
        .env("CONTAINERS_CONF", &conf)
        .arg("run")
        .arg("--rm")
        .arg("-v")
        .arg(format!("{}:/src:ro", src_dir.display()))
        .arg("-v")
        .arg(format!("{}:/config:ro", config_src.display()))
        .arg("-v")
        .arg(format!("{}:/work", work.display()))
        .arg("-e")
        .arg(format!("SOURCE_DATE_EPOCH={BUILD_EPOCH}"))
        .arg("-e")
        .arg(format!("KBUILD_BUILD_TIMESTAMP={ts}"))
        .arg("-e")
        .arg("KBUILD_BUILD_USER=dvmm")
        .arg("-e")
        .arg("KBUILD_BUILD_HOST=dvmm")
        .arg("-e")
        .arg("KCONFIG_NOTIMESTAMP=1")
        .arg(&img_ref)
        .args(["sh", "-c", &script]), ux.mode)?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(work.join("vmlinux-out"), out)
        .map_err(|e| format!("kernel build produced no vmlinux: {e}"))?;
    Ok(())
}

/// Resolve an image's pinned digest by pulling the ref and reading RepoDigests.
/// `--record`-only (never in `dvmm build`'s pipeline): always inherits, like
/// every other command outside the `build` orchestrator.
fn resolve_image_digest(image: &str) -> Result<String, Box<dyn std::error::Error>> {
    let confdir = ScratchDir::new()?;
    let conf = confdir.path().join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    run(engine::command().env("CONTAINERS_CONF", &conf).args(["pull", "-q", image]), engine::OutputMode::Inherit)?;
    let repo = image.split(':').next().unwrap_or(image);
    let digests = capture(engine::command().env("CONTAINERS_CONF", &conf).args([
        "image", "inspect", image, "--format", "{{range .RepoDigests}}{{println .}}{{end}}",
    ]))?;
    let pin = digests
        .lines()
        .find(|l| l.starts_with(&format!("{repo}@")))
        .and_then(|l| l.split_once('@').map(|(_, d)| d.to_string()))
        .ok_or_else(|| format!("could not resolve a digest for {image}"))?;
    Ok(pin)
}

/// `dvmm build-kernel`: acquire the pinned kernel (fetch/fallback), or `--record`
/// to bootstrap kernel.lock from a fresh reproducible container build. Shares
/// `ensure_kernel`/`build_kernel_container` with `dvmm build`'s pipeline, but
/// ALWAYS with progress UI disabled / output inherited (scope lock — progress
/// is `build`-only): a plain inherited passthrough.
pub fn cmd_build_kernel(args: BuildKernelArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let here = self_here()?;
    let (cache_dir, cache_src) = resolve_cache_dir(args.cache_dir.as_deref());
    eprintln!("== dvmm build-kernel ==  cache-dir: {} (source: {cache_src})", cache_dir.display());
    let progress = ui::Progress::disabled();
    let ux = Ux::inherit(&progress);

    if args.record {
        // Bootstrap/update kernel.lock: resolve digests, container-build, record.
        let mut kl = read_kernel_lock(&here).unwrap_or_default();
        if kl.version.is_empty() {
            return Err(
                "guest/kernel/kernel.lock must exist with at least KERNEL_VERSION + \
                 KERNEL_SOURCE_URL + BUILDER_IMAGE + RELEASE_ASSET_URL before --record".into(),
            );
        }
        // config sha (declared input).
        kl.config_sha256 = sha256_file_hex(&here.join("kernel").join(KERNEL_CONFIG_NAME))?;
        // resolve the builder image digest if not pinned yet.
        if kl.builder_digest.is_empty() {
            eprintln!("   resolving builder image digest for {} ...", kl.builder_image);
            kl.builder_digest = resolve_image_digest(&kl.builder_image)?;
        }
        // fetch source (record its sha), then container-build.
        let out = args
            .out
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| here.join(format!("kernel/vmlinux-{}", kl.version)));
        build_kernel_container(&here, &cache_dir, &kl, &out, &ux)?;
        // record source + kernel shas.
        let tarball = cache_dir.join("kernel-src").join(format!("linux-{}.tar.xz", kl.version));
        kl.source_sha256 = sha256_file_hex(&tarball)?;
        kl.sha256 = sha256_file_hex(&out)?;
        write_kernel_lock(&here, &kl)?;
        eprintln!("== kernel.lock RECORDED ==");
        eprintln!("   KERNEL_SHA256={}", kl.sha256);
        eprintln!("   KERNEL_SOURCE_SHA256={}", kl.source_sha256);
        eprintln!("   BUILDER_DIGEST={}", kl.builder_digest);
        eprintln!("   CONFIG_SHA256={}", kl.config_sha256);
        println!("{}  {}", kl.sha256, out.display());
        return Ok(0);
    }

    let out = ensure_kernel(&here, &cache_dir, args.force_build, &ux)?;
    // If a custom -o was requested, copy the acquired kernel there too.
    if let Some(o) = &args.out {
        let op = PathBuf::from(o);
        if op != out {
            if let Some(parent) = op.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&out, &op)?;
        }
    }
    let sha = sha256_file_hex(&out)?;
    eprintln!("== kernel ready ==  {} (sha256 {sha})", out.display());
    println!("{sha}  {}", out.display());
    Ok(0)
}
