//! provenance / image records (feed the manifest anchors + the lock digest map)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

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

// ---- bake_one / build_one (mirror bake-stack.sh) ---------------------------

#[allow(clippy::too_many_arguments)]
pub(super) fn bake_one(
    bstore: &Path,
    brun: &Path,
    conf: &Path,
    reff: &str,
    squash_threshold_mib: u64,
    work: &Path,
    records: &mut Vec<ImgRecord>,
    plain_refs: &mut Vec<String>,
    plain_pin: &mut HashMap<String, String>,
    squash_tars: &mut Vec<(String, String, PathBuf)>,
    total_img_mib: &mut u64,
    ux: &Ux,
) -> Result<(), Box<dyn std::error::Error>> {
    run(podman(bstore, brun, conf).args(["pull", "-q", reff]), ux.mode)?;
    let bytes: u64 = capture(podman(bstore, brun, conf).args(["image", "inspect", reff, "--format", "{{.Size}}"]))?
        .trim()
        .parse()
        .unwrap_or(0);
    let mib = bytes / 1048576;
    *total_img_mib += mib;
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
        plain_refs.push(reff.to_string());
        plain_pin.insert(reff.to_string(), canon.clone());
        records.push(ImgRecord {
            key: reff.to_string(),
            upstream: reff.to_string(),
            policy: "plain".into(),
            content_id: diffid,
            size_mib: mib,
            pinned: reff.to_string(),
        });
        ux.progress.detail(format!("   [plain]  {reff}  ({mib} MiB)"));
        return Ok(());
    }

    // squash: reproducible single-FROM repackage + config-equivalence gate.
    let base = squash_base_name(reff);
    let short = squash_short(reff);
    let tag = format!("localhost/dvmm-{base}-{short}:baked");
    let ctx = work.join(format!("ctx-{}", squash_tars.len()));
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
    let tar = work.join(format!("squash-{}.tar", squash_tars.len()));
    run(podman(bstore, brun, conf).args(["save", "-o"]).arg(&tar).arg(&tag), ux.mode)?;
    squash_tars.push((reff.to_string(), tag.clone(), tar));
    records.push(ImgRecord {
        key: reff.to_string(),
        upstream: reff.to_string(),
        policy: "squash".into(),
        content_id: sq_diffid,
        size_mib: mib,
        pinned: String::new(), // filled after seed load
    });
    ux.progress.detail(format!("   [squash] {reff}  ({mib} MiB)  -> {tag}  (GATE ok)"));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_one(
    bstore: &Path,
    brun: &Path,
    conf: &Path,
    b: &compose::BuildCtx,
    work: &Path,
    records: &mut Vec<ImgRecord>,
    squash_tars: &mut Vec<(String, String, PathBuf)>,
    total_img_mib: &mut u64,
    ux: &Ux,
) -> Result<(), Box<dyn std::error::Error>> {
    ux.progress.detail(format!("   [build]  service={}  context={}  dockerfile={}  -> {}", b.service, b.context, b.dockerfile, b.image_tag));
    run(podman(bstore, brun, conf).args(["build", "--squash-all", "--timestamp", BUILD_EPOCH, "-t", &b.image_tag, "-f", &b.dockerfile, &b.context]), ux.mode)?;
    let bytes: u64 = capture(podman(bstore, brun, conf).args(["image", "inspect", &b.image_tag, "--format", "{{.Size}}"]))?
        .trim()
        .parse()
        .unwrap_or(0);
    let mib = bytes / 1048576;
    *total_img_mib += mib;
    let diffid = capture(podman(bstore, brun, conf).args(["image", "inspect", &b.image_tag, "--format", "{{range .RootFS.Layers}}{{println .}}{{end}}"]))?
        .split_whitespace()
        .collect::<String>();
    let tar = work.join(format!("squash-{}.tar", squash_tars.len()));
    run(podman(bstore, brun, conf).args(["save", "-o"]).arg(&tar).arg(&b.image_tag), ux.mode)?;
    squash_tars.push((b.image_tag.clone(), b.image_tag.clone(), tar));
    records.push(ImgRecord {
        key: b.image_tag.clone(),
        upstream: b.image_tag.clone(),
        policy: "build".into(),
        content_id: diffid,
        size_mib: mib,
        pinned: String::new(),
    });
    ux.progress.detail(format!("   [build]  {}  ({mib} MiB)  content_id set", b.image_tag));
    Ok(())
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
