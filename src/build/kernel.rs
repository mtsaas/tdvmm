//! kernel (Fable Part C) — reproducible containerized build, from source only
//!
//! The guest kernel is compiled FROM SOURCE inside the pinned builder container
//! (owner ruling: no prebuilt binaries — the kernel IS the guest): the sha-pinned
//! tarball is fetched, built with the embedded config, verified byte-for-byte
//! against kernel.lock's KERNEL_SHA256, and cached. No host kernel toolchain is
//! required, and nothing precompiled is ever downloaded.
//!
//! The kernel.lock PIN and the kernel CONFIG are embedded at compile time, so a
//! standalone binary can build the exact recorded kernel with no checkout
//! present. The `--record` bootstrap is the maintainer flow that reads the
//! config from (and writes the lock into) a source checkout; a rebuild of tdvmm
//! then embeds the recorded pin.

use std::path::{Path, PathBuf};

use crate::artifact;
use crate::compose;
use crate::engine;
use crate::ui;
use super::cache::resolve_cache_dir;
use super::pins::{fetch_in_container, fetch_verify, lock_values};
use super::util::{find_testdata_dir, sha256_file_hex, ScratchDir};
use super::ux::{capture, run, run_build, Ux};
use super::{BuildKernelArgs, BUILD_EPOCH};

/// The kernel config baked into the guest (Firecracker microvm config, HPET off,
/// all built-in). The checkout copy in `testdata/kernel/` is the source of truth
/// `--record` reads/hashes; the embedded copy below is what every build uses.
const KERNEL_CONFIG_NAME: &str = "microvm-kernel-x86_64-6.1.config";

/// The committed kernel pin (`testdata/kernel/kernel.lock`), embedded so `tdvmm
/// build` can build + verify the pinned vmlinux with no checkout present.
const KERNEL_LOCK: &str = include_str!("../../testdata/kernel/kernel.lock");

/// The committed kernel config, embedded beside the pin: the container build's
/// required input, tripwired against the lock's KERNEL_CONFIG_SHA256 (see
/// [`embedded_kernel_config`]) so a config edit can't silently build an
/// unrecorded kernel.
const KERNEL_CONFIG: &str =
    include_str!("../../testdata/kernel/microvm-kernel-x86_64-6.1.config");

/// The reproducibility ledger for the guest kernel (`testdata/kernel/kernel.lock`).
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
}

fn kernel_lock_path(testdata_dir: &Path) -> PathBuf {
    testdata_dir.join("kernel/kernel.lock")
}

fn parse_kernel_lock(text: &str, origin: &str) -> Result<KernelLock, Box<dyn std::error::Error>> {
    let [version, sha256, config_sha256, source_url, source_sha256, builder_image, builder_digest] =
        lock_values(text, [
            "KERNEL_VERSION",
            "KERNEL_SHA256",
            "KERNEL_CONFIG_SHA256",
            "KERNEL_SOURCE_URL",
            "KERNEL_SOURCE_SHA256",
            "BUILDER_IMAGE",
            "BUILDER_DIGEST",
        ]);
    if version.is_empty() {
        return Err(format!("{origin} missing KERNEL_VERSION").into());
    }
    Ok(KernelLock {
        version,
        sha256,
        config_sha256,
        source_url,
        source_sha256,
        builder_image,
        builder_digest,
    })
}

/// The compiled-in kernel pin (what `tdvmm build` uses — never a checkout read).
pub(super) fn embedded_kernel_lock() -> Result<KernelLock, Box<dyn std::error::Error>> {
    parse_kernel_lock(KERNEL_LOCK, "embedded kernel.lock")
}

/// Read the CHECKOUT's kernel.lock (maintainer `--record` flow only: the on-disk
/// file is the source of truth being edited; a rebuild embeds it afterwards).
fn read_kernel_lock(testdata_dir: &Path) -> Result<KernelLock, Box<dyn std::error::Error>> {
    let path = kernel_lock_path(testdata_dir);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("reading {}: {e} (run `tdvmm build-kernel --record`)", path.display()))?;
    parse_kernel_lock(&text, &path.display().to_string())
}

fn write_kernel_lock(testdata_dir: &Path, k: &KernelLock) -> Result<(), Box<dyn std::error::Error>> {
    let body = format!(
        "# tdvmm guest kernel pin (Fable Part C).\n\
         #\n\
         # The guest vmlinux is compiled FROM SOURCE in the pinned builder container:\n\
         # the sha-pinned tarball below is built with the recorded config, and the\n\
         # result must match KERNEL_SHA256 byte-for-byte before it is cached. Nothing\n\
         # precompiled is ever downloaded; no host kernel toolchain is required. This\n\
         # file and the config are embedded into the tdvmm binary at compile time.\n\
         #\n\
         # Regenerate with:  tdvmm build-kernel --record\n\
         KERNEL_VERSION={}\n\
         KERNEL_SHA256={}\n\
         KERNEL_CONFIG_SHA256={}\n\
         KERNEL_SOURCE_URL={}\n\
         KERNEL_SOURCE_SHA256={}\n\
         BUILDER_IMAGE={}\n\
         BUILDER_DIGEST={}\n",
        k.version, k.sha256, k.config_sha256, k.source_url, k.source_sha256,
        k.builder_image, k.builder_digest,
    );
    std::fs::write(kernel_lock_path(testdata_dir), body)?;
    Ok(())
}

/// The embedded kernel config, tripwired against the embedded lock: a config
/// edit that skipped `tdvmm build-kernel --record` fails HERE, loudly and
/// instantly, instead of minutes later at the built-sha gate.
fn embedded_kernel_config(kl: &KernelLock) -> Result<&'static str, Box<dyn std::error::Error>> {
    let got = artifact::sha256_hex(KERNEL_CONFIG.as_bytes());
    if got != kl.config_sha256 {
        return Err(format!(
            "embedded kernel config sha256 {got} != kernel.lock KERNEL_CONFIG_SHA256 {}; \
             re-run `tdvmm build-kernel --record`, then rebuild tdvmm",
            kl.config_sha256
        )
        .into());
    }
    Ok(KERNEL_CONFIG)
}

/// Ensure the guest kernel is present at `<cache>/kernel/vmlinux-<version>` and
/// matches the embedded kernel.lock pin: sha-verified cache hit, else the
/// reproducible container build from the sha-pinned source + embedded config —
/// the ONLY acquisition path (nothing precompiled is ever downloaded). Returns
/// the cached vmlinux path.
pub(super) fn ensure_kernel(
    cache_dir: &Path,
    force_build: bool,
    ux: &Ux,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let kl = embedded_kernel_lock()?;
    let kernel_dir = cache_dir.join("kernel");
    std::fs::create_dir_all(&kernel_dir)?;
    let out = kernel_dir.join(format!("vmlinux-{}", kl.version));
    if kl.sha256.is_empty() {
        return Err(
            "kernel.lock has no KERNEL_SHA256; run `tdvmm build-kernel --record` first".into(),
        );
    }

    // Already present + verified? (the common case for every bake after the first.)
    if !force_build && out.exists() {
        if let Ok(got) = sha256_file_hex(&out) {
            if got == kl.sha256 {
                ux.progress.note("cached (sha verified)");
                ux.progress.detail(format!("   kernel: {} (present, sha256 verified)", out.display()));
                return Ok(out);
            }
            ux.progress.println(format!(
                "{}: kernel at {} sha256 {} != kernel.lock {}; rebuilding",
                compose::WARN, out.display(), &got[..16], &kl.sha256[..16]
            ));
        }
    }

    let config = embedded_kernel_config(&kl)?;
    ux.progress.relabel(format!("guest kernel · compiling {} (first run)", kl.version));
    build_kernel_container(cache_dir, &kl, config, &out, ux)?;
    ux.progress.relabel("guest kernel");
    let got = sha256_file_hex(&out)?;
    if got != kl.sha256 {
        return Err(format!(
            "container-built kernel sha256 {got} != kernel.lock {}; the build is not reproducing \
             the recorded kernel (re-run `tdvmm build-kernel --record` if inputs changed)",
            kl.sha256
        )
        .into());
    }
    ux.progress.note("compiled + sha verified");
    ux.progress.detail("   kernel: container build sha256 verified against kernel.lock");
    Ok(out)
}

/// Reproducibly build vmlinux inside the pinned builder container (no host kernel
/// toolchain). Faithfully ports `build_kernel.sh` — including the `-std=gnu11` CC
/// wrapper — with build_agent's determinism knobs (pinned image, SOURCE_DATE_EPOCH,
/// fixed KBUILD_BUILD_* + build-id). Source is fetched+verified on the host and
/// bind-mounted; the compiler is pinned by the image digest. `config` is the
/// kernel config text (the embedded copy, or `--record`'s checkout read).
fn build_kernel_container(
    cache_dir: &Path,
    kl: &KernelLock,
    config: &str,
    out: &Path,
    ux: &Ux,
) -> Result<(), Box<dyn std::error::Error>> {
    if kl.builder_digest.is_empty() {
        return Err("kernel.lock has no BUILDER_DIGEST; run `tdvmm build-kernel --record`".into());
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

    // 2. runc conf (host default runtime is misconfigured — Fable host fact) +
    //    the config staged into the same scratch dir + work.
    let confdir = ScratchDir::new()?;
    let conf = confdir.path().join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    let config_src = confdir.path().join(KERNEL_CONFIG_NAME);
    std::fs::write(&config_src, config)?;
    let work_guard = ScratchDir::new()?;
    let work = work_guard.path();

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

    // 4. run — streamed into the live tail when the viewport is up, with the
    //    full transcript logged under the cache diagnostics dir. Fixed build
    //    identity for byte-reproducibility.
    let log = cache_dir.join("diagnostics").join(format!("kernel-build-{}.log", kl.version));
    let ts = "Thu Jan  1 00:00:00 UTC 1970"; // stable KBUILD banner timestamp
    run_build(engine::command()
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
        .arg("KBUILD_BUILD_USER=tdvmm")
        .arg("-e")
        .arg("KBUILD_BUILD_HOST=tdvmm")
        .arg("-e")
        .arg("KCONFIG_NOTIMESTAMP=1")
        .arg(&img_ref)
        .args(["sh", "-c", &script]), ux, &log)?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(work.join("vmlinux-out"), out)
        .map_err(|e| format!("kernel build produced no vmlinux: {e}"))?;
    Ok(())
}

/// Resolve an image's pinned digest by pulling the ref and reading RepoDigests.
/// `--record`-only (never in `tdvmm build`'s pipeline): always inherits, like
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

/// `tdvmm build-kernel`: build the pinned kernel into the cache, or `--record`
/// to bootstrap kernel.lock from a fresh reproducible container build. Shares
/// `ensure_kernel`/`build_kernel_container` with `tdvmm build`'s pipeline, but
/// ALWAYS with progress UI disabled / output inherited (scope lock — progress
/// is `build`-only): a plain inherited passthrough.
pub fn cmd_build_kernel(args: BuildKernelArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let (cache_dir, cache_src) = resolve_cache_dir(args.cache_dir.as_deref());
    eprintln!("== tdvmm build-kernel ==  cache-dir: {} (source: {cache_src})", cache_dir.display());
    let progress = ui::Progress::disabled();
    let ux = Ux::inherit(&progress);

    if args.record {
        // Bootstrap/update kernel.lock IN THE CHECKOUT: resolve digests,
        // container-build, record. The edited on-disk lock + config are the
        // inputs; a `cargo build` afterwards embeds the recorded pin.
        let testdata_dir = find_testdata_dir()
            .ok_or("`build-kernel --record` needs a source checkout (it rewrites testdata/kernel/kernel.lock)")?;
        let mut kl = read_kernel_lock(&testdata_dir).unwrap_or_default();
        if kl.version.is_empty() {
            return Err(
                "testdata/kernel/kernel.lock must exist with at least KERNEL_VERSION + \
                 KERNEL_SOURCE_URL + BUILDER_IMAGE before --record".into(),
            );
        }
        // the checkout config is the declared input being recorded.
        let config = std::fs::read_to_string(testdata_dir.join("kernel").join(KERNEL_CONFIG_NAME))?;
        kl.config_sha256 = artifact::sha256_hex(config.as_bytes());
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
            .unwrap_or_else(|| cache_dir.join("kernel").join(format!("vmlinux-{}", kl.version)));
        build_kernel_container(&cache_dir, &kl, &config, &out, &ux)?;
        // record source + kernel shas.
        let tarball = cache_dir.join("kernel-src").join(format!("linux-{}.tar.xz", kl.version));
        kl.source_sha256 = sha256_file_hex(&tarball)?;
        kl.sha256 = sha256_file_hex(&out)?;
        write_kernel_lock(&testdata_dir, &kl)?;
        eprintln!("== kernel.lock RECORDED ==");
        eprintln!("   KERNEL_SHA256={}", kl.sha256);
        eprintln!("   KERNEL_SOURCE_SHA256={}", kl.source_sha256);
        eprintln!("   BUILDER_DIGEST={}", kl.builder_digest);
        eprintln!("   CONFIG_SHA256={}", kl.config_sha256);
        println!("{}  {}", kl.sha256, out.display());
        return Ok(0);
    }

    let out = ensure_kernel(&cache_dir, args.force_build, &ux)?;
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

/// Resolve `tdvmm boot`'s kernel + initramfs, filling in a default for any input
/// the caller omitted: the pinned guest kernel (ensured/built like `tdvmm
/// build` — no checkout needed) and the committed busybox clock-guest initramfs
/// (checkout-only; error if absent). A provided path is used verbatim.
pub fn resolve_boot_inputs(
    kernel: Option<&str>,
    initrd: Option<&str>,
) -> Result<(PathBuf, PathBuf), Box<dyn std::error::Error>> {
    let kernel = match kernel {
        Some(k) => PathBuf::from(k),
        None => {
            let (cache_dir, _src) = resolve_cache_dir(None);
            let progress = ui::Progress::disabled();
            let ux = Ux::inherit(&progress);
            ensure_kernel(&cache_dir, false, &ux)?
        }
    };
    let initrd = match initrd {
        Some(i) => PathBuf::from(i),
        None => {
            let testdata_dir = find_testdata_dir().ok_or(
                "no --initrd given and no source checkout found: the default busybox \
                 clock-guest initramfs lives in the repo (testdata/initramfs/); pass \
                 --initrd <path> or run from a checkout",
            )?;
            testdata_dir.join("initramfs/initramfs.cpio.gz")
        }
    };
    Ok((kernel, initrd))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The embedded config and the embedded lock must agree — editing the
    /// checkout config without `build-kernel --record` (then rebuilding) would
    /// otherwise ship a binary that builds an unrecorded kernel.
    #[test]
    fn embedded_kernel_config_matches_lock_sha() {
        let kl = embedded_kernel_lock().unwrap();
        assert_eq!(
            artifact::sha256_hex(KERNEL_CONFIG.as_bytes()),
            kl.config_sha256,
            "embedded kernel config drifted from kernel.lock's KERNEL_CONFIG_SHA256; \
             re-run `tdvmm build-kernel --record`"
        );
    }

    /// The build path must refuse a drifted config (the tripwire fails loudly).
    #[test]
    fn embedded_kernel_config_tripwire_rejects_mismatch() {
        let mut kl = embedded_kernel_lock().unwrap();
        kl.config_sha256 = "0".repeat(64);
        let err = embedded_kernel_config(&kl).unwrap_err().to_string();
        assert!(err.contains("KERNEL_CONFIG_SHA256"), "unexpected error: {err}");
    }
}
