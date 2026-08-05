//! provenance / image records (feed the manifest anchors + the lock digest map)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::artifact;
use crate::compose;
use super::ux::{capture, podman, run, Ux};
use super::BUILD_EPOCH;

#[derive(Clone)]
pub(super) struct ImgRecord {
    pub(super) key: String,      // manifest/digest-map key: upstream ref (plain/squash) or build tag
    pub(super) upstream: String, // manifest "upstream" field
    pub(super) policy: String,   // plain | squash | build
    pub(super) content_id: String,
    pub(super) size_mib: u64,
    pub(super) pinned: String, // filled after seed load (squash/build); == key for plain
}

/// Everything one image bake/build produces. The orchestrator merges these into
/// its ordered accumulators by STABLE input position — never completion order —
/// which is what keeps the record/seed/lock byte order identical to a serial
/// bake.
pub(super) struct BakedImage {
    pub(super) record: ImgRecord,
    /// plain policy: `(upstream ref, canonical pin)` for the lock digest map.
    pub(super) plain: Option<(String, String)>,
    /// squash/build policy: `(key, local tag, saved tar)` to seed-load.
    pub(super) squash: Option<(String, String, PathBuf)>,
}

/// One bake worker's haul — `(slot index, output)` pairs, or its first error.
type WorkerHaul = Result<Vec<(usize, BakedImage)>, Box<dyn std::error::Error + Send + Sync>>;

/// Bake every user-declared image concurrently and return one [`BakedImage`]
/// per input, in INPUT ORDER. Each bake runs against its own scratch vfs store
/// (`<work>/build-storage-<i>`): podman locks a store per child process, so
/// concurrent writes into one shared `--root` would serialize or corrupt.
/// Outputs land in fixed slots keyed by image index, so completion order never
/// reaches the caller. The worker pool is capped at `available_parallelism`,
/// bounding the podman child processes with it.
pub(super) fn bake_all(
    images: &[String],
    conf: &Path,
    squash_threshold_mib: u64,
    work: &Path,
    ux: &Ux,
) -> Result<Vec<BakedImage>, Box<dyn std::error::Error + Send + Sync>> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(images.len());
    let next = AtomicUsize::new(0);
    let hauls: Vec<WorkerHaul> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| -> WorkerHaul {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(reff) = images.get(i) else { break };
                        let bstore = work.join(format!("build-storage-{i}"));
                        let brun = work.join(format!("build-run-{i}"));
                        std::fs::create_dir_all(&bstore)?;
                        std::fs::create_dir_all(&brun)?;
                        let baked = bake_one(&bstore, &brun, conf, reff, squash_threshold_mib, i, work, ux)
                            .map_err(|e| format!("baking {reff}: {e}"))?;
                        done.push((i, baked));
                    }
                    Ok(done)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("image bake worker panicked".into())))
            .collect()
    });
    let mut slots: Vec<Option<BakedImage>> = images.iter().map(|_| None).collect();
    for haul in hauls {
        for (i, baked) in haul? {
            slots[i] = Some(baked);
        }
    }
    slots
        .into_iter()
        .enumerate()
        .map(|(i, slot)| slot.ok_or_else(|| format!("image bake dropped slot {i}").into()))
        .collect()
}

// ---- bake_one / build_one (mirror bake-stack.sh) ---------------------------

/// Bake one image against the given scratch store: pull + inspect, and for an
/// over-threshold image the reproducible squash + config-equivalence gate +
/// save. `idx` is the image's stable input position — it names the scratch
/// context dir and saved tar (`ctx-<idx>`, `squash-<idx>.tar`), so concurrent
/// bakes never collide and the names don't depend on completion order.
#[allow(clippy::too_many_arguments)]
pub(super) fn bake_one(
    bstore: &Path,
    brun: &Path,
    conf: &Path,
    reff: &str,
    squash_threshold_mib: u64,
    idx: usize,
    work: &Path,
    ux: &Ux,
) -> Result<BakedImage, Box<dyn std::error::Error + Send + Sync>> {
    run(podman(bstore, brun, conf).args(["pull", "-q", reff]), ux.mode)?;
    let bytes: u64 = capture(podman(bstore, brun, conf).args(["image", "inspect", reff, "--format", "{{.Size}}"]))?
        .trim()
        .parse()
        .unwrap_or(0);
    let mib = bytes / 1048576;
    let diffid = capture(podman(bstore, brun, conf).args(["image", "inspect", reff, "--format", "{{range .RootFS.Layers}}{{println .}}{{end}}"]))?
        .split_whitespace()
        .collect::<String>();

    if mib <= squash_threshold_mib {
        // plain: prefer the requested digest ref; else RepoDigests[0]; else Id.
        let canon = if reff.contains("@sha256:") {
            reff.to_string()
        } else {
            capture(podman(bstore, brun, conf).args(["image", "inspect", reff, "--format", "{{if .RepoDigests}}{{index .RepoDigests 0}}{{else}}{{.Id}}{{end}}"]))?
                .trim()
                .to_string()
        };
        ux.progress.detail(format!("   [plain]  {reff}  ({mib} MiB)"));
        return Ok(BakedImage {
            record: ImgRecord {
                key: reff.to_string(),
                upstream: reff.to_string(),
                policy: "plain".into(),
                content_id: diffid,
                size_mib: mib,
                pinned: reff.to_string(),
            },
            plain: Some((reff.to_string(), canon)),
            squash: None,
        });
    }

    // squash: reproducible single-FROM repackage + config-equivalence gate.
    let base = squash_base_name(reff);
    let short = squash_short(reff);
    let tag = format!("localhost/tdvmm-{base}-{short}:baked");
    let ctx = work.join(format!("ctx-{idx}"));
    std::fs::create_dir_all(&ctx)?;
    std::fs::write(ctx.join("Containerfile"), format!("FROM {reff}\n"))?;
    run(podman(bstore, brun, conf).args([
        "build", "--squash-all", "--pull=never", "--timestamp", BUILD_EPOCH, "-t", &tag, "-f",
    ]).arg(ctx.join("Containerfile")).arg(&ctx), ux.mode)?;
    // config-equivalence gate.
    for f in ["Entrypoint", "Cmd", "Env", "Volumes", "WorkingDir"] {
        let up = capture(podman(bstore, brun, conf).args(["image", "inspect", reff, "--format", &format!("{{{{json .Config.{f}}}}}")]))?;
        let sq = capture(podman(bstore, brun, conf).args(["image", "inspect", &tag, "--format", &format!("{{{{json .Config.{f}}}}}")]))?;
        if up != sq {
            ux.progress.println(format!("{}: config-equivalence gate failed for {reff} (Config.{f} drifted)", compose::REJECT));
            return Err(format!("GATE FAIL: Config.{f} drifted during squash of {reff}").into());
        }
    }
    let sq_diffid = capture(podman(bstore, brun, conf).args(["image", "inspect", &tag, "--format", "{{range .RootFS.Layers}}{{println .}}{{end}}"]))?
        .split_whitespace()
        .collect::<String>();
    let tar = work.join(format!("squash-{idx}.tar"));
    run(podman(bstore, brun, conf).args(["save", "-o"]).arg(&tar).arg(&tag), ux.mode)?;
    ux.progress.detail(format!("   [squash] {reff}  ({mib} MiB)  -> {tag}  (GATE ok)"));
    Ok(BakedImage {
        record: ImgRecord {
            key: reff.to_string(),
            upstream: reff.to_string(),
            policy: "squash".into(),
            content_id: sq_diffid,
            size_mib: mib,
            pinned: String::new(), // filled after seed load
        },
        plain: None,
        squash: Some((reff.to_string(), tag, tar)),
    })
}

/// Build one `build:` service image (host-side) against the given scratch
/// store. `idx` follows the same stable numbering as [`bake_one`] (it names
/// `squash-<idx>.tar`).
pub(super) fn build_one(
    bstore: &Path,
    brun: &Path,
    conf: &Path,
    b: &compose::BuildCtx,
    idx: usize,
    work: &Path,
    ux: &Ux,
) -> Result<BakedImage, Box<dyn std::error::Error + Send + Sync>> {
    ux.progress.detail(format!("   [build]  service={}  context={}  dockerfile={}  -> {}", b.service, b.context, b.dockerfile, b.image_tag));
    run(podman(bstore, brun, conf).args(["build", "--squash-all", "--timestamp", BUILD_EPOCH, "-t", &b.image_tag, "-f", &b.dockerfile, &b.context]), ux.mode)?;
    let bytes: u64 = capture(podman(bstore, brun, conf).args(["image", "inspect", &b.image_tag, "--format", "{{.Size}}"]))?
        .trim()
        .parse()
        .unwrap_or(0);
    let mib = bytes / 1048576;
    let diffid = capture(podman(bstore, brun, conf).args(["image", "inspect", &b.image_tag, "--format", "{{range .RootFS.Layers}}{{println .}}{{end}}"]))?
        .split_whitespace()
        .collect::<String>();
    let tar = work.join(format!("squash-{idx}.tar"));
    run(podman(bstore, brun, conf).args(["save", "-o"]).arg(&tar).arg(&b.image_tag), ux.mode)?;
    ux.progress.detail(format!("   [build]  {}  ({mib} MiB)  content_id set", b.image_tag));
    Ok(BakedImage {
        record: ImgRecord {
            key: b.image_tag.clone(),
            upstream: b.image_tag.clone(),
            policy: "build".into(),
            content_id: diffid,
            size_mib: mib,
            pinned: String::new(),
        },
        plain: None,
        squash: Some((b.image_tag.clone(), b.image_tag.clone(), tar)),
    })
}

/// `echo ref | sed -E 's#[@:].*$##; s#.*/##'` — the image name (postgres).
pub(super) fn squash_base_name(reff: &str) -> String {
    let cut = reff.split(['@', ':']).next().unwrap_or(reff);
    cut.rsplit('/').next().unwrap_or(cut).to_string()
}

/// First 64-hex run of the ref, first 12 chars (the digest short form).
pub(super) fn squash_short(reff: &str) -> String {
    let bytes = reff.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_hexdigit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i - start >= 64 {
                return reff[start..start + 12].to_string();
            }
        } else {
            i += 1;
        }
    }
    // fallback: sha256(ref) first 12 (matches the shell fallback)
    let s = artifact::sha256_hex(reff.as_bytes());
    s[..12].to_string()
}
