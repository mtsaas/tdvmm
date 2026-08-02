//! `dvmm build <compose.yml>` (OP-1b) — the whole bake pipeline, folded into the
//! binary. This replaces the retired `guest/bake-stack.sh` (orchestrator),
//! `guest/bake_compose.py` (→ [`crate::compose`]), `guest/pack-dvmm.sh` (→
//! [`crate::artifact::pack`]), and `guest/initramfs-alpine/{build_rootfs.sh,
//! prebake_images.sh,zero_cpio_inodes.py}` (→ [`crate::cpio`] + this module).
//!
//! Per Fable's OP-1b design:
//!   * the compose WHITELIST parse/validate/reject + `compose.lock.yml` emission
//!     is Rust ([`crate::compose`]);
//!   * **podman stays the image engine** — we shell out to it for pull-by-digest,
//!     `--squash-all --timestamp` repackaging, and `build:` contexts (no OCI
//!     client is reimplemented);
//!   * the initramfs cpio is emitted **directly from Rust** ([`crate::cpio`])
//!     with the exact deterministic normalization;
//!   * the `.dvmm` is packed via the existing [`crate::artifact`] encoder.
//!
//! Two hidden helper subcommands run inside `podman unshare` (a user namespace)
//! where the assembled rootfs files are readable as uid 0: `__seed-build`
//! (assemble the seed store) and `__assemble-initramfs` (build the Alpine rootfs
//! and emit the cpio). `dvmm build` re-execs itself into them.
//!
//! The OP-1b acceptance is a **byte-identical** `.dvmm` versus the old scripts on
//! the corpus, so every command and file operation below mirrors them exactly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::artifact;
use crate::compose;
use crate::cpio;

// ---- pins (mirror bake-stack.sh + build_rootfs.sh) -------------------------

const BUILD_EPOCH: &str = "1785542400";
const BUSYBOX_REF: &str = "docker.io/library/busybox@sha256:dc2d74b28e4cf8984fa52af1f39bc7c3d9c73760b41a74d629f5d11b1ab28616";
const VMM_MAX_MEM_MIB: u64 = 3072;
const DEFAULT_MEM_MIB: u64 = 3072;
const DEFAULT_WORKING_SET_MIB: u64 = 512;
const DEFAULT_SQUASH_THRESHOLD_MIB: u64 = 100;

const ALPINE_BRANCH: &str = "v3.22";
const ALPINE_VER: &str = "3.22.5";
const MINIROOTFS: &str = "alpine-minirootfs-3.22.5-x86_64.tar.gz";
const MINIROOTFS_SHA256: &str = "4b4daa9fe2fc696c4919c4412a4c3d3e770d8fb70292a004a2c72f5096175282";
const DEFAULT_MIRROR: &str = "https://dl-cdn.alpinelinux.org/alpine";

/// Top-level pinned packages (transitive deps float within the branch), mirroring
/// `build_rootfs.sh`'s `PKGS`.
const PKGS: &[&str] = &[
    "podman=5.6.2-r3",
    "crun=1.23.1-r0",
    "conmon=2.1.13-r0",
    "netavark=1.16.1-r0",
    "aardvark-dns=1.16.0-r0",
    "nftables=1.1.3-r0",
    "iptables=1.8.11-r1",
    "iproute2=6.15.0-r0",
    "ca-certificates=20260611-r0",
    "fuse-overlayfs=1.15-r0",
];

/// The baked run-defaults (mirror `pack-dvmm.sh`). Fixed for the corpus (env can
/// override in the scripts; `dvmm build` keeps the same defaults).
const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable dvmm.stack=1 dvmm.interval=3600 dvmm.maxrows=1000 dvmm.hc_tick=2";

// ============================================================================
// CLI args
// ============================================================================

pub struct BuildArgs {
    pub compose: String,
    pub out: Option<String>,
    pub name: Option<String>,
    pub mem: Option<u64>,
    pub working_set: Option<u64>,
    pub squash_threshold: Option<u64>,
    pub validate_only: bool,
    /// Bypass the content-hash bake cache: force a full rebuild (still stores the
    /// result so later cached runs can hit). Nightly `bake_repeat` uses this.
    pub no_cache: bool,
}

// ============================================================================
// helpers
// ============================================================================

fn die_reject(msg: &str) -> ! {
    eprintln!("{}: {msg}", compose::REJECT);
    eprintln!("bake: REJECTED at validation (see {} above)", compose::REJECT);
    std::process::exit(3);
}

/// Run a command, returning stdout on success, or an Err(message) on failure.
fn capture(cmd: &mut Command) -> Result<String, String> {
    let out = cmd
        .output()
        .map_err(|e| format!("spawn {:?}: {e}", cmd.get_program()))?;
    if !out.status.success() {
        return Err(format!(
            "command {:?} failed ({}): {}",
            cmd.get_program(),
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a command for its side effect (inherit stderr), erroring on non-zero.
fn run(cmd: &mut Command) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| format!("spawn {:?}: {e}", cmd.get_program()))?;
    if !status.success() {
        return Err(format!("command {:?} failed ({status})", cmd.get_program()));
    }
    Ok(())
}

/// A podman invocation against a scratch vfs store (mirrors `bp()`), with the
/// clean CONTAINERS_CONF set.
fn podman(store: &Path, runroot: &Path, conf: &Path) -> Command {
    let mut c = Command::new("podman");
    c.env("CONTAINERS_CONF", conf)
        .arg("--root")
        .arg(store)
        .arg("--runroot")
        .arg(runroot)
        .arg("--storage-driver")
        .arg("vfs");
    c
}

fn sha256_file_hex(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

// ============================================================================
// provenance / image records (feed the manifest anchors + the lock digest map)
// ============================================================================

#[derive(Clone)]
struct ImgRecord {
    key: String,      // manifest/digest-map key: upstream ref (plain/squash) or build tag
    upstream: String, // manifest "upstream" field
    policy: String,   // plain | squash | build
    content_id: String,
    size_mib: u64,
    pinned: String, // filled after seed load (squash/build); == key for plain
}

// ============================================================================
// dvmm build
// ============================================================================

pub fn cmd_build(args: BuildArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let compose_path = std::fs::canonicalize(&args.compose)
        .map_err(|_| format!("compose file not found: {}", args.compose))?;
    let compose_dir = compose_path.parent().unwrap().to_path_buf();
    let stack_name = args
        .name
        .clone()
        .unwrap_or_else(|| compose_dir.file_name().unwrap().to_string_lossy().into_owned());
    let project = format!("dvmm_{stack_name}");

    // --- parse + validate (the loud static gate) ---
    let doc_str = std::fs::read_to_string(&compose_path)?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&doc_str)
        .map_err(|e| {
            eprintln!("{}: could not parse {}: {e}", "DVMM_BAKE_ERROR", compose_path.display());
            std::process::exit(2);
        })
        .unwrap();
    let validated = match compose::validate(&doc, &compose_path) {
        Ok(v) => v,
        Err(e) => {
            if e.exit_code == 3 {
                die_reject(&e.message);
            } else {
                eprintln!("DVMM_BAKE_ERROR: {}", e.message);
                std::process::exit(2);
            }
        }
    };
    for w in &validated.warnings {
        eprintln!("{}: {w}", compose::WARN);
    }
    if args.validate_only {
        // Emit the same JSON summary shape the Python `validate` printed, then stop.
        println!(
            "{{\"images\": {}, \"builds\": {}, \"binds\": {}}}",
            validated.images.len(),
            validated.builds.len(),
            validated.binds.len()
        );
        return Ok(0);
    }

    let mem_mib = args.mem.unwrap_or(DEFAULT_MEM_MIB);
    let working_set_mib = args.working_set.unwrap_or(DEFAULT_WORKING_SET_MIB);
    let squash_threshold_mib = args.squash_threshold.unwrap_or(DEFAULT_SQUASH_THRESHOLD_MIB);

    let self_exe = std::env::current_exe()?;
    let here = self_here()?; // the guest/ dir (repo layout), for overlay/agent/etc.
    let alpine_dir = here.join("initramfs-alpine");

    eprintln!("== dvmm build: stack={stack_name} project={project} mem={mem_mib}MiB ==");
    eprintln!("   compose: {}", compose_path.display());

    // Output destinations (needed early so a cache HIT can restore them). The `-o`
    // path is NOT part of the cache key: identical inputs bake identical bytes
    // regardless of where they land.
    let out_dvmm = match &args.out {
        Some(o) => PathBuf::from(o),
        None => alpine_dir.join(format!("{stack_name}.dvmm")),
    };
    let out_initramfs = alpine_dir.join(format!("initramfs-alpine-{stack_name}.cpio.gz"));
    let committed_lock = here.join("stacks").join(&stack_name).join("compose.lock.yml");
    let stack_lock_path = here.join("stacks").join(&stack_name).join("stack.lock");

    // ---- content-hash bake cache -------------------------------------------
    // The key covers EVERY input that affects the output bytes: the whole compose
    // dir tree (compose.yml + build contexts + bind sources + service source,
    // excluding this bake's own committed outputs), the kernel, the agent source,
    // the guest overlay tree, the pinned compose engine, the host podman
    // toolchain, the dvmm binary itself (all compiled-in pins + bake logic), and
    // the sizing knobs. `dvmm build` is deterministic (artifact_test gate 1), so a
    // hit reusing the prior `.dvmm` is byte-identical to a fresh bake.
    let cache = match compute_cache_key(
        &self_exe, &here, &alpine_dir, &compose_dir, &stack_name, mem_mib, working_set_mib,
        squash_threshold_mib,
    ) {
        Ok(c) => Some(c),
        Err(e) => {
            eprintln!("{}: bake cache disabled (key error: {e})", compose::WARN);
            None
        }
    };
    if let Some(c) = &cache {
        if !args.no_cache && cache_is_hit(c) {
            eprintln!("== BAKE CACHE HIT ==  key={}", &c.key[..16]);
            eprintln!("   reusing baked artifacts (skipped pull/squash/assemble): {}", c.dir.display());
            match cache_restore(c, &out_dvmm, &out_initramfs, &committed_lock, &stack_lock_path) {
                Ok(dvmm_sha) => {
                    eprintln!("   .dvmm:     {} (sha256 {dvmm_sha})", out_dvmm.display());
                    println!("{dvmm_sha}  {}", out_dvmm.display());
                    return Ok(0);
                }
                Err(e) => eprintln!("{}: cache restore failed ({e}); rebuilding", compose::WARN),
            }
        } else if args.no_cache {
            eprintln!("== bake cache BYPASSED (--no-cache): forcing full rebuild ==  key={}", &c.key[..16]);
        } else {
            eprintln!("== BAKE CACHE MISS ==  key={} (full bake) ==", &c.key[..16]);
        }
    }

    // --- scratch workdir + clean CONTAINERS_CONF ---
    let work = mkdtemp()?;
    let conf = work.join("containers.conf");
    std::fs::write(&conf, "[engine]\n")?;
    let podman_version = capture(Command::new("podman").arg("--version"))?
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .to_string();

    eprintln!("   images: {}", validated.images.join(" "));
    if !validated.builds.is_empty() {
        let tags: Vec<&str> = validated.builds.iter().map(|b| b.image_tag.as_str()).collect();
        eprintln!("   builds: {}", tags.join(" "));
    }

    // --- 2. bake each image into a scratch vfs store ---
    let bstore = work.join("build-storage");
    let brun = work.join("build-run");
    std::fs::create_dir_all(&bstore)?;
    std::fs::create_dir_all(&brun)?;

    let mut records: Vec<ImgRecord> = Vec::new(); // ordered, dupes allowed (RAM total)
    let mut plain_refs: Vec<String> = Vec::new();
    let mut plain_pin: HashMap<String, String> = HashMap::new();
    // squash/build outputs to load into the seed: (key, local_tag, tar_path)
    let mut squash_tars: Vec<(String, String, PathBuf)> = Vec::new();
    let mut total_img_mib: u64 = 0;

    for reff in &validated.images {
        bake_one(&bstore, &brun, &conf, reff, squash_threshold_mib, &work, &mut records, &mut plain_refs, &mut plain_pin, &mut squash_tars, &mut total_img_mib)?;
    }
    if !validated.builds.is_empty() {
        eprintln!("== build build: services (host-side) ==");
        for b in &validated.builds {
            build_one(&bstore, &brun, &conf, b, &work, &mut records, &mut squash_tars, &mut total_img_mib)?;
        }
    }
    eprintln!("== bake self-test image (busybox, plain) ==");
    bake_one(&bstore, &brun, &conf, BUSYBOX_REF, squash_threshold_mib, &work, &mut records, &mut plain_refs, &mut plain_pin, &mut squash_tars, &mut total_img_mib)?;
    let selftest_pin = plain_pin.get(BUSYBOX_REF).cloned().unwrap_or_default();

    // --- 3. build the seed store (podman unshare) ---
    eprintln!("== build seed store ==");
    let seed = work.join("seed");
    let store = seed.join("storage");
    let runroot = work.join("seedrun");
    std::fs::create_dir_all(&store)?;
    std::fs::create_dir_all(&runroot)?;

    let seed_cfg = SeedConfig {
        store: store.clone(),
        runroot: runroot.clone(),
        conf: conf.clone(),
        plains: plain_refs.clone(),
        squash: squash_tars.iter().map(|(k, t, p)| SeedSquash {
            key: k.clone(),
            local_tag: t.clone(),
            tar: p.clone(),
        }).collect(),
        seedpins_out: work.join("seedpins.json"),
    };
    let seed_cfg_path = work.join("seed-config.json");
    std::fs::write(&seed_cfg_path, serde_json::to_vec(&seed_cfg)?)?;
    run(Command::new("podman")
        .env("CONTAINERS_CONF", &conf)
        .arg("unshare")
        .arg(&self_exe)
        .arg("__seed-build")
        .arg("--config")
        .arg(&seed_cfg_path))?;
    // relocatable store: drop libpod state (records an absolute graphroot).
    let _ = std::fs::remove_dir_all(store.join("libpod"));
    let _ = std::fs::remove_file(store.join("db.sql"));

    // seed pins (key -> pinned repo@sha256), resolved inside the unshare.
    let seedpins: HashMap<String, String> =
        serde_json::from_slice(&std::fs::read(work.join("seedpins.json"))?)?;
    for r in records.iter_mut() {
        if r.policy == "squash" || r.policy == "build" {
            if let Some(pin) = seedpins.get(&r.key) {
                r.pinned = pin.clone();
            }
        }
    }

    // --- 4. emit compose.lock.yml + materialize binds ---
    eprintln!("== emit compose.lock.yml ==");
    let mut digests: HashMap<String, String> = HashMap::new();
    for reff in &plain_refs {
        if let Some(canon) = plain_pin.get(reff) {
            digests.insert(reff.clone(), canon.clone());
        }
    }
    for (k, pin) in &seedpins {
        digests.insert(k.clone(), pin.clone());
    }
    let binds_base = "/var/lib/dvmm-stack/binds";
    let lock = compose::emit_lock(&doc, &compose_path, &digests, binds_base, &project)
        .map_err(|e| e.message)?;
    let lock_path = work.join("compose.lock.yml");
    std::fs::write(&lock_path, &lock.lock_yaml)?;
    let lock_sha = sha256_file_hex(&lock_path)?;

    // materialize relative binds into a staging tree.
    let binds_stage = work.join("binds");
    std::fs::create_dir_all(&binds_stage)?;
    for (src, dest_rel) in &lock.bind_manifest {
        let dest = binds_stage.join(dest_rel);
        std::fs::create_dir_all(dest.parent().unwrap())?;
        run(Command::new("cp").arg("-a").arg(src).arg(&dest))?;
        eprintln!("   materialized  {src}  ->  {binds_base}/{dest_rel}");
    }

    // --- 5. RAM estimate ---
    let est_mib = ((2.5 * total_img_mib as f64) + working_set_mib as f64 + 512.0).ceil() as u64;
    eprintln!("== RAM estimate ==");
    eprintln!("   total image size: {total_img_mib} MiB;  estimate >= {est_mib} MiB (2.5x img + {working_set_mib} ws + 512 base)");
    if mem_mib < est_mib {
        eprintln!("{}: configured guest RAM {mem_mib} MiB is below the estimate {est_mib} MiB.", compose::WARN);
    } else {
        eprintln!("   configured {mem_mib} MiB >= estimate {est_mib} MiB (OK)");
    }
    if mem_mib > VMM_MAX_MEM_MIB {
        eprintln!("{}: {mem_mib} MiB exceeds the current VMM cap {VMM_MAX_MEM_MIB} MiB (32-bit MMIO gap);", compose::WARN);
    }

    // --- 6. assemble the per-stack initramfs (build_rootfs, stack mode) ---
    eprintln!("== assemble initramfs (Rust rootfs + cpio) ==");
    // (out_initramfs computed early, above, for the cache path)

    // build the dvmm-agent (static musl, reproducible) in the pinned builder
    // container, before the unshare. Returns the embedded build hash (the compat
    // oracle reported by ping/hello); its file sha256 goes in the ledger + anchors.
    let agent_bin = work.join("dvmm-agent");
    let agent_build_hash = build_agent(&here, &agent_bin)?;
    let agent_sha = sha256_file_hex(&agent_bin)?;
    eprintln!("   dvmm-agent: sha256 {agent_sha}  build {agent_build_hash}");

    // fetch + verify the pinned minirootfs + compose binary (cached in alpine_dir).
    let mirror = std::env::var("ALPINE_MIRROR").unwrap_or_else(|_| DEFAULT_MIRROR.to_string());
    let tarball = alpine_dir.join(MINIROOTFS);
    fetch_verify(
        &tarball,
        &format!("{mirror}/{ALPINE_BRANCH}/releases/x86_64/{MINIROOTFS}"),
        MINIROOTFS_SHA256,
    )?;
    let (compose_version, compose_sha256) = read_compose_lock(&alpine_dir)?;
    let compose_cache = alpine_dir.join(format!("docker-compose-{compose_version}"));
    fetch_verify(
        &compose_cache,
        &format!("https://github.com/docker/compose/releases/download/{compose_version}/docker-compose-linux-x86_64"),
        &compose_sha256,
    )?;

    let assemble_cfg = AssembleConfig {
        conf: conf.clone(),
        work: work.clone(),
        tarball,
        mirror,
        alpine_branch: ALPINE_BRANCH.to_string(),
        build_epoch: BUILD_EPOCH.to_string(),
        pkgs: PKGS.iter().map(|s| s.to_string()).collect(),
        overlay: here.join("initramfs-alpine/overlay"),
        compose_cache,
        agent_bin,
        seed_storage: store.clone(),
        selftest_image_ref: selftest_pin.clone(),
        stack_name: stack_name.clone(),
        stack_project: project.clone(),
        stack_lock: lock_path.clone(),
        stack_binds: binds_stage.clone(),
        stack_mem: mem_mib,
        out: out_initramfs.clone(),
        packages_lock_out: alpine_dir.join("packages.lock"),
    };
    let assemble_cfg_path = work.join("assemble-config.json");
    std::fs::write(&assemble_cfg_path, serde_json::to_vec(&assemble_cfg)?)?;
    run(Command::new("podman")
        .env("CONTAINERS_CONF", &conf)
        .arg("unshare")
        .arg(&self_exe)
        .arg("__assemble-initramfs")
        .arg("--config")
        .arg(&assemble_cfg_path))?;

    let art_sha = sha256_file_hex(&out_initramfs)?;

    // --- 7. write stack.lock (the reproducibility ledger) ---
    write_stack_lock(&here, &stack_name, &project, mem_mib, est_mib, &lock_sha, &art_sha, &agent_sha, &out_initramfs, &records, &plain_refs, &plain_pin, &seedpins, &validated, &podman_version)?;

    // stash the emitted lock next to the manifest.
    std::fs::copy(&lock_path, &committed_lock)?;

    // --- 8. pack the single-file .dvmm artifact ---
    eprintln!("== pack .dvmm artifact ==");
    let kernel = here.join("kernel/vmlinux-6.1.128");
    let dvmm_bytes = pack_dvmm(&self_exe, &records, &compose_version, &compose_sha256, &stack_name, &project, mem_mib, est_mib, &podman_version, &agent_sha, &agent_build_hash, &kernel, &out_initramfs, &lock_path)?;
    std::fs::write(&out_dvmm, &dvmm_bytes)?;
    let dvmm_sha = artifact::sha256_hex(&dvmm_bytes);
    // append the artifact identity to the ledger.
    append_stack_lock_dvmm(&here, &stack_name, &dvmm_sha, &out_dvmm)?;

    eprintln!();
    eprintln!("== dvmm build DONE ==");
    eprintln!("   initramfs: {}", out_initramfs.display());
    eprintln!("   sha256:    {art_sha}");
    eprintln!("   .dvmm:     {} (sha256 {dvmm_sha})", out_dvmm.display());
    // stdout: the artifact identity line (parity with the old pack-dvmm.sh).
    println!("{dvmm_sha}  {}", out_dvmm.display());

    // --- 9. populate the bake cache (best-effort; never fails the build) ---
    if let Some(c) = &cache {
        match cache_store(c, &out_dvmm, &out_initramfs, &committed_lock, &stack_lock_path, &dvmm_sha) {
            Ok(true) => eprintln!("   cached:    {} (key {})", c.dir.display(), &c.key[..16]),
            Ok(false) => {} // entry already present
            Err(e) => eprintln!("{}: could not populate bake cache ({e})", compose::WARN),
        }
    }

    let _ = std::fs::remove_dir_all(&work);
    Ok(0)
}

/// Resolve the repo `guest/` directory relative to the running binary (target/…).
/// Falls back to `guest/` under the current dir.
fn self_here() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // The binary lives at <repo>/target/{release,debug}/dvmm; guest is <repo>/guest.
    let exe = std::env::current_exe()?;
    let mut p = exe.clone();
    // .../target/release/dvmm -> .../ (repo root)
    for _ in 0..3 {
        p.pop();
    }
    let cand = p.join("guest");
    if cand.is_dir() {
        return Ok(cand);
    }
    let cwd = std::env::current_dir()?.join("guest");
    if cwd.is_dir() {
        return Ok(cwd);
    }
    Err("could not locate the repo guest/ directory (run from the repo, or keep target/ in place)".into())
}

fn mkdtemp() -> std::io::Result<PathBuf> {
    let base = std::env::temp_dir();
    let name = format!("dvmm-build-{}-{}", std::process::id(), now_nanos());
    let dir = base.join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

// ---- bake_one / build_one (mirror bake-stack.sh) ---------------------------

#[allow(clippy::too_many_arguments)]
fn bake_one(
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
) -> Result<(), Box<dyn std::error::Error>> {
    run(podman(bstore, brun, conf).args(["pull", "-q", reff]))?;
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
        eprintln!("   [plain]  {reff}  ({mib} MiB)");
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
    ]).arg(ctx.join("Containerfile")).arg(&ctx))?;
    // config-equivalence gate.
    for f in ["Entrypoint", "Cmd", "Env", "Volumes", "WorkingDir"] {
        let up = capture(podman(bstore, brun, conf).args(["image", "inspect", reff, "--format", &format!("{{{{json .Config.{f}}}}}")]))?;
        let sq = capture(podman(bstore, brun, conf).args(["image", "inspect", &tag, "--format", &format!("{{{{json .Config.{f}}}}}")]))?;
        if up != sq {
            eprintln!("{}: config-equivalence gate failed for {reff} (Config.{f} drifted)", compose::REJECT);
            return Err(format!("GATE FAIL: Config.{f} drifted during squash of {reff}").into());
        }
    }
    let sq_diffid = capture(podman(bstore, brun, conf).args(["image", "inspect", &tag, "--format", "{{range .RootFS.Layers}}{{println .}}{{end}}"]))?
        .split_whitespace()
        .collect::<String>();
    let tar = work.join(format!("squash-{}.tar", squash_tars.len()));
    run(podman(bstore, brun, conf).args(["save", "-o"]).arg(&tar).arg(&tag))?;
    squash_tars.push((reff.to_string(), tag.clone(), tar));
    records.push(ImgRecord {
        key: reff.to_string(),
        upstream: reff.to_string(),
        policy: "squash".into(),
        content_id: sq_diffid,
        size_mib: mib,
        pinned: String::new(), // filled after seed load
    });
    eprintln!("   [squash] {reff}  ({mib} MiB)  -> {tag}  (GATE ok)");
    Ok(())
}

fn build_one(
    bstore: &Path,
    brun: &Path,
    conf: &Path,
    b: &compose::BuildCtx,
    work: &Path,
    records: &mut Vec<ImgRecord>,
    squash_tars: &mut Vec<(String, String, PathBuf)>,
    total_img_mib: &mut u64,
) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("   [build]  service={}  context={}  dockerfile={}  -> {}", b.service, b.context, b.dockerfile, b.image_tag);
    run(podman(bstore, brun, conf).args(["build", "--squash-all", "--timestamp", BUILD_EPOCH, "-t", &b.image_tag, "-f", &b.dockerfile, &b.context]))?;
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
    run(podman(bstore, brun, conf).args(["save", "-o"]).arg(&tar).arg(&b.image_tag))?;
    squash_tars.push((b.image_tag.clone(), b.image_tag.clone(), tar));
    records.push(ImgRecord {
        key: b.image_tag.clone(),
        upstream: b.image_tag.clone(),
        policy: "build".into(),
        content_id: diffid,
        size_mib: mib,
        pinned: String::new(),
    });
    eprintln!("   [build]  {}  ({mib} MiB)  content_id set", b.image_tag);
    Ok(())
}

/// `echo ref | sed -E 's#[@:].*$##; s#.*/##'` — the image name (postgres).
fn squash_base_name(reff: &str) -> String {
    let cut = reff.split(['@', ':']).next().unwrap_or(reff);
    cut.rsplit('/').next().unwrap_or(cut).to_string()
}

/// First 64-hex run of the ref, first 12 chars (the digest short form).
fn squash_short(reff: &str) -> String {
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

// ---- agent build + fetch helpers ------------------------------------------

/// Read the pinned rust+musl builder image ref + digest from
/// `dvmm-agent/images.lock` (Fable §2). Returns `(image, digest)`.
pub fn read_builder_pin(repo_root: &Path) -> Result<(String, String), Box<dyn std::error::Error>> {
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

/// A deterministic identity of the agent's SOURCES — the `dvmm-agent` +
/// `dvmm-proto` crate trees + `Cargo.lock`. Embedded as the agent's build hash
/// (the compatibility oracle reported by `ping`/hello) and folded into the bake
/// cache key. First 16 hex of the sha256.
fn agent_src_id(repo_root: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let a = tree_hash(&repo_root.join("dvmm-agent"), &[])?;
    let p = tree_hash(&repo_root.join("dvmm-proto"), &[])?;
    let lock = sha256_file_hex(&repo_root.join("Cargo.lock"))?;
    Ok(artifact::sha256_hex(format!("{a}\n{p}\n{lock}\n").as_bytes())[..16].to_string())
}

/// Build the guest `dvmm-agent` as a static, reproducible `x86_64-unknown-linux-
/// musl` binary INSIDE the pinned builder container (Fable §2 — never on the host,
/// so rustc drift can't change the `.dvmm` bytes). Determinism knobs: the
/// `agent-release` profile (opt-level=z, lto, codegen-units=1, panic=abort,
/// strip=symbols); `SOURCE_DATE_EPOCH`; `--remap-path-prefix` for both the source
/// mount and CARGO_HOME; `--build-id=none`; and `rust-lld` + self-contained
/// linking so no external C toolchain is pulled. Returns the embedded build hash.
fn build_agent(here: &Path, out: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let repo_root = here
        .parent()
        .ok_or("could not locate the repo root above guest/")?
        .to_path_buf();
    let (image, digest) = read_builder_pin(&repo_root)?;
    let img_ref = format!("{image}@{digest}");
    let build_hash = agent_src_id(&repo_root)?;
    eprintln!("building dvmm-agent (static musl, reproducible) in {img_ref}");

    let confdir = mkdtemp()?;
    let conf = confdir.join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    let work = mkdtemp()?;

    // rust-lld consumes link args directly (no `cc` driver), so `--build-id=none`
    // is passed as-is. The remaps stabilize the two absolute paths that would
    // otherwise leak into the bytes: the /src source mount and CARGO_HOME.
    let rustflags = "-C linker=rust-lld -C link-self-contained=yes -C link-arg=--build-id=none \
                     --remap-path-prefix=/src=/dvmm --remap-path-prefix=/work/cargo=/cargo";
    let script = "set -e; cd /src && \
        cargo build -p dvmm-agent --profile agent-release \
            --target x86_64-unknown-linux-musl --locked && \
        cp /work/target/x86_64-unknown-linux-musl/agent-release/dvmm-agent /work/dvmm-agent";

    run(Command::new("podman")
        .env("CONTAINERS_CONF", &conf)
        .arg("run")
        .arg("--rm")
        .arg("-v")
        .arg(format!("{}:/src:ro", repo_root.display()))
        .arg("-v")
        .arg(format!("{}:/work", work.display()))
        .arg("-e")
        .arg(format!("SOURCE_DATE_EPOCH={BUILD_EPOCH}"))
        .arg("-e")
        .arg("CARGO_HOME=/work/cargo")
        .arg("-e")
        .arg("CARGO_TARGET_DIR=/work/target")
        .arg("-e")
        .arg(format!("RUSTFLAGS={rustflags}"))
        .arg("-e")
        .arg(format!("DVMM_AGENT_BUILD={build_hash}"))
        .arg(&img_ref)
        .args(["sh", "-c", script]))?;

    std::fs::copy(work.join("dvmm-agent"), out)
        .map_err(|e| format!("agent build produced no binary: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&confdir);
    Ok(build_hash)
}

/// `dvmm build-agent -o <path>`: build the reproducible musl agent standalone
/// (the size + double-build byte-identity gate scripts use this). Prints
/// `<sha256>  <path>` to stdout.
pub fn cmd_build_agent(out: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let here = self_here()?;
    let outp = PathBuf::from(out);
    if let Some(parent) = outp.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let build_hash = build_agent(&here, &outp)?;
    let sha = sha256_file_hex(&outp)?;
    let size = std::fs::metadata(&outp)?.len();
    eprintln!("   dvmm-agent: {size} bytes  build={build_hash}  sha256={sha}");
    println!("{sha}  {}", outp.display());
    Ok(0)
}

fn fetch_verify(path: &Path, url: &str, sha: &str) -> Result<(), Box<dyn std::error::Error>> {
    if !path.exists() {
        eprintln!("downloading {url} ...");
        run(Command::new("curl").args(["-sSL", "-o"]).arg(path).arg(url))?;
    }
    let got = sha256_file_hex(path)?;
    if got != sha {
        return Err(format!("sha256 mismatch for {}: got {got}, want {sha}", path.display()).into());
    }
    Ok(())
}

fn read_compose_lock(alpine_dir: &Path) -> Result<(String, String), Box<dyn std::error::Error>> {
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

// ============================================================================
// stack.lock ledger (mirror bake-stack.sh)
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn write_stack_lock(
    here: &Path,
    stack: &str,
    project: &str,
    mem_mib: u64,
    est_mib: u64,
    lock_sha: &str,
    art_sha: &str,
    agent_sha: &str,
    out_initramfs: &Path,
    records: &[ImgRecord],
    _plain_refs: &[String],
    _plain_pin: &HashMap<String, String>,
    seedpins: &HashMap<String, String>,
    validated: &compose::Validated,
    podman_version: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = here.join("stacks").join(stack).join("stack.lock");
    let mut prov: Vec<String> = Vec::new();
    // build_base lines (per build service, per base) come before image/pin (sorted).
    for b in &validated.builds {
        for base in &b.bases {
            prov.push(format!("build_base  service={}  base={base}", b.service));
        }
    }
    for r in records {
        match r.policy.as_str() {
            "plain" => prov.push(format!(
                "image  policy=plain   upstream={}  diffid={}  size_mib={}",
                r.upstream, r.content_id, r.size_mib
            )),
            "squash" => {
                let base = squash_base_name(&r.upstream);
                let short = squash_short(&r.upstream);
                prov.push(format!(
                    "image  policy=squash  upstream={}  seed_tag=localhost/dvmm-{base}-{short}:baked  src_diffid={}  size_mib={}  GATE=ok",
                    r.upstream, r.content_id, r.size_mib
                ));
            }
            "build" => {
                let svc = validated.builds.iter().find(|b| b.image_tag == r.key).map(|b| b.service.as_str()).unwrap_or("");
                prov.push(format!(
                    "image  policy=build   service={svc}  tag={}  content_id={}  size_mib={}",
                    r.key, r.content_id, r.size_mib
                ));
            }
            _ => {}
        }
    }
    for (key, pin) in seedpins {
        prov.push(format!("pin    upstream={key}  pinned={pin}"));
    }
    prov.sort();

    let mut out = String::new();
    out.push_str("# deterministic-vmm Phase-2a stack manifest (generated by bake-stack.sh).\n");
    out.push_str("# The reproducibility ledger for this stack: pinned image digests + the\n");
    out.push_str("# compose.lock.yml hash + the built initramfs artifact hash. Re-baking the\n");
    out.push_str("# same compose input reproduces the COMPARED lines below byte-for-byte\n");
    out.push_str("# (squashed images are pinned via --timestamp, so their digests are stable).\n");
    out.push_str("#\n");
    out.push_str("# build: services (2b) are built HOST-SIDE and judged by CONTENT-IDENTITY,\n");
    out.push_str("# not image ID: 'policy=build' lines carry content_id=<squashed-layer DiffID>\n");
    out.push_str("# (a SOURCE_DATE_EPOCH-normalized, reproducible filesystem hash) + the pinned\n");
    out.push_str("# build_base digests + the Go toolchain. Same source -> same content_id.\n");
    out.push_str("#\n");
    out.push_str(&format!("stack     {stack}\n"));
    out.push_str(&format!("project   {project}\n"));
    out.push_str(&format!("mem_mib   {mem_mib}\n"));
    out.push_str(&format!("ram_estimate_mib  {est_mib}\n"));
    out.push_str(&format!("compose_lock_sha256  {lock_sha}\n"));
    out.push_str(&format!("initramfs_sha256     {art_sha}  {}\n", out_initramfs.file_name().unwrap().to_string_lossy()));
    // The reproducible guest control-channel agent's own line (Fable §2).
    out.push_str(&format!("agent_sha256         {agent_sha}  dvmm-agent\n"));
    for line in &prov {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("# --- informational (NOT compared for repeatability) ---\n");
    out.push_str(&format!("# podman-version: {podman_version}\n"));
    out.push_str(&format!("# baked-at: {}\n", utc_now_iso()));
    std::fs::write(&path, out)?;
    Ok(())
}

fn append_stack_lock_dvmm(here: &Path, stack: &str, dvmm_sha: &str, out_dvmm: &Path) -> std::io::Result<()> {
    let path = here.join("stacks").join(stack).join("stack.lock");
    let mut s = std::fs::read_to_string(&path)?;
    s.push_str(&format!("dvmm_sha256          {dvmm_sha}  {}\n", out_dvmm.file_name().unwrap().to_string_lossy()));
    std::fs::write(&path, s)
}

fn utc_now_iso() -> String {
    // best-effort; informational only (NOT part of the byte-identity gate).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // crude UTC breakdown
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 based civil date
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ============================================================================
// .dvmm packing (mirror pack-dvmm.sh + artifact::pack)
// ============================================================================

#[allow(clippy::too_many_arguments)]
fn pack_dvmm(
    self_exe: &Path,
    records: &[ImgRecord],
    compose_version: &str,
    compose_sha256: &str,
    stack: &str,
    project: &str,
    mem_mib: u64,
    est_mib: u64,
    podman_version: &str,
    agent_sha: &str,
    agent_build_hash: &str,
    kernel_path: &Path,
    initramfs_path: &Path,
    lock_path: &Path,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    // dump-cpuid via the SAME binary (byte-identical to pack-dvmm.sh).
    let cpuid_profile = capture(Command::new(self_exe).arg("dump-cpuid"))?;
    let cpuid_sha = artifact::sha256_hex(cpuid_profile.as_bytes());

    // dedup image records by key (last wins — same values), sorted by key.
    let mut by_key: std::collections::BTreeMap<String, artifact::ImagePin> = std::collections::BTreeMap::new();
    for r in records {
        by_key.insert(
            r.key.clone(),
            artifact::ImagePin {
                upstream: r.upstream.clone(),
                pinned: if r.pinned.is_empty() { r.key.clone() } else { r.pinned.clone() },
                policy: r.policy.clone(),
                content_id: r.content_id.clone(),
                size_mib: r.size_mib,
            },
        );
    }
    let images: Vec<artifact::ImagePin> = by_key.into_values().collect();

    let manifest = artifact::Manifest {
        format_version: artifact::FORMAT_VERSION,
        stack: stack.to_string(),
        project: project.to_string(),
        members: vec![],
        anchors: artifact::Anchors {
            cpuid_sha256: cpuid_sha,
            cpuid_profile,
            compose_engine: artifact::ComposeEngine {
                version: compose_version.to_string(),
                sha256: compose_sha256.to_string(),
            },
            images,
            toolchain: artifact::Toolchain {
                podman: podman_version.to_string(),
                alpine: ALPINE_VER.to_string(),
                compose: compose_version.to_string(),
            },
            ram_estimate_mib: est_mib,
            // The baked control-channel agent: its file sha256 + the build hash
            // it reports over ping/hello (the compatibility oracle; Fable §2/§4).
            agent_sha256: agent_sha.to_string(),
            agent_build_hash: agent_build_hash.to_string(),
        },
        run_defaults: artifact::RunDefaults {
            mem_mib,
            cmdline: DEFAULT_CMDLINE.to_string(),
            fast_forward: true,
            max_virtual_time: None,
        },
    };
    let manifest_in = manifest.to_canonical_json()?;
    let kernel = std::fs::read(kernel_path)?;
    let initramfs = std::fs::read(initramfs_path)?;
    let compose_lock = std::fs::read(lock_path)?;
    Ok(artifact::pack(&manifest_in, &kernel, &initramfs, &compose_lock)?)
}

// ============================================================================
// __seed-build (runs inside `podman unshare`)
// ============================================================================

#[derive(Serialize, Deserialize)]
struct SeedSquash {
    key: String,
    local_tag: String,
    tar: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct SeedConfig {
    store: PathBuf,
    runroot: PathBuf,
    conf: PathBuf,
    plains: Vec<String>,
    squash: Vec<SeedSquash>,
    seedpins_out: PathBuf,
}

pub fn cmd_seed_build(config: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let cfg: SeedConfig = serde_json::from_slice(&std::fs::read(config)?)?;
    let sp = |args: &[&str]| -> Command {
        let mut c = Command::new("podman");
        c.env("CONTAINERS_CONF", &cfg.conf)
            .arg("--root")
            .arg(&cfg.store)
            .arg("--runroot")
            .arg(&cfg.runroot)
            .arg("--storage-driver")
            .arg("vfs")
            .args(args);
        c
    };
    for reff in &cfg.plains {
        run(&mut sp(&["pull", "-q", reff]))?;
    }
    for s in &cfg.squash {
        run(sp(&["load", "-q", "-i"]).arg(&s.tar))?;
    }
    // resolve the post-load SEED digest of each squashed/built image.
    let mut seedpins: HashMap<String, String> = HashMap::new();
    for s in &cfg.squash {
        let repo = s.local_tag.rsplit_once(':').map(|(r, _)| r).unwrap_or(&s.local_tag);
        let digests = capture(&mut sp(&["image", "inspect", &s.local_tag, "--format", "{{range .RepoDigests}}{{println .}}{{end}}"]))?;
        let pin = digests
            .lines()
            .find(|l| l.starts_with(&format!("{repo}@")))
            .map(|l| l.to_string())
            .unwrap_or_default();
        seedpins.insert(s.key.clone(), pin);
    }
    std::fs::write(&cfg.seedpins_out, serde_json::to_vec(&seedpins)?)?;
    Ok(0)
}

// ============================================================================
// __assemble-initramfs (runs inside `podman unshare`)
// ============================================================================

#[derive(Serialize, Deserialize)]
struct AssembleConfig {
    conf: PathBuf,
    work: PathBuf,
    tarball: PathBuf,
    mirror: String,
    alpine_branch: String,
    build_epoch: String,
    pkgs: Vec<String>,
    overlay: PathBuf,
    compose_cache: PathBuf,
    agent_bin: PathBuf,
    seed_storage: PathBuf,
    selftest_image_ref: String,
    stack_name: String,
    stack_project: String,
    stack_lock: PathBuf,
    stack_binds: PathBuf,
    stack_mem: u64,
    out: PathBuf,
    packages_lock_out: PathBuf,
}

pub fn cmd_assemble_initramfs(config: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let cfg: AssembleConfig = serde_json::from_slice(&std::fs::read(config)?)?;
    let rootfs = cfg.work.join("rootfs");
    std::fs::create_dir_all(&rootfs)?;

    // 1. extract the pinned minirootfs.
    run(Command::new("tar").arg("-C").arg(&rootfs).arg("-xzf").arg(&cfg.tarball))?;

    // 2. apk config: pinned branch + a resolver for the one-time install.
    std::fs::create_dir_all(rootfs.join("etc/apk"))?;
    std::fs::write(
        rootfs.join("etc/apk/repositories"),
        format!("{m}/{b}/main\n{m}/{b}/community\n", m = cfg.mirror, b = cfg.alpine_branch),
    )?;
    std::fs::write(rootfs.join("etc/resolv.conf"), "nameserver 1.1.1.1\nnameserver 8.8.8.8\n")?;

    // 3. install the pinned container stack (chroot works: CAP_SYS_CHROOT in userns).
    run(Command::new("chroot").arg(&rootfs).args(["/sbin/apk", "update"]))?;
    let mut add = Command::new("chroot");
    add.arg(&rootfs).args(["/sbin/apk", "add", "--no-progress"]);
    for p in &cfg.pkgs {
        add.arg(p);
    }
    run(&mut add)?;

    // record the FULL resolved version set (top-level + deps).
    let listing = capture(Command::new("chroot").arg(&rootfs).args(["/sbin/apk", "list", "-I"]))?;
    let mut pkgs: Vec<&str> = listing.lines().filter_map(|l| l.split_whitespace().next()).collect();
    pkgs.sort_unstable();
    let packages_lock = format!("{}\n", pkgs.join("\n"));
    std::fs::write(cfg.work.join("packages.lock"), &packages_lock)?;

    // 4. drop the overlay (init, self-test, compose launcher, podman config).
    run(Command::new("cp").arg("-a").arg(format!("{}/.", cfg.overlay.display())).arg(format!("{}/", rootfs.display())))?;
    for f in [
        "init",
        "usr/local/bin/container-selftest.sh",
        "usr/local/bin/compose-up.sh",
        "usr/local/bin/healthcheck-ticker.sh",
    ] {
        run(Command::new("chmod").arg("0755").arg(rootfs.join(f)))?;
    }
    // 4b. bake the genuine Docker Compose v2 CLI.
    run(Command::new("install").args(["-D", "-m", "0755"]).arg(&cfg.compose_cache).arg(rootfs.join("usr/local/bin/docker-compose")))?;
    // 4c. bake the control-channel agent.
    run(Command::new("install").args(["-D", "-m", "0755"]).arg(&cfg.agent_bin).arg(rootfs.join("usr/local/bin/dvmm-agent")))?;

    // 5. fixed clock epoch + self-test image ref.
    std::fs::write(rootfs.join("etc/dvmm-build-epoch"), format!("{}\n", cfg.build_epoch))?;
    std::fs::write(rootfs.join("etc/dvmm-image-ref"), format!("{}\n", cfg.selftest_image_ref))?;

    // 5b. STACK mode: compose.lock + materialized binds + pinned project.
    std::fs::create_dir_all(rootfs.join("var/lib/dvmm-stack/binds"))?;
    std::fs::copy(&cfg.stack_lock, rootfs.join("var/lib/dvmm-stack/compose.lock.yml"))?;
    run(Command::new("chmod").arg("0644").arg(rootfs.join("var/lib/dvmm-stack/compose.lock.yml")))?;
    if cfg.stack_binds.is_dir() {
        // cp -a "$STACK_BINDS/." binds/  (may be empty; ignore failure like the script)
        let _ = Command::new("cp")
            .arg("-a")
            .arg(format!("{}/.", cfg.stack_binds.display()))
            .arg(format!("{}/", rootfs.join("var/lib/dvmm-stack/binds").display()))
            .status();
    }
    std::fs::write(rootfs.join("etc/dvmm-stack-name"), format!("{}\n", cfg.stack_name))?;
    std::fs::write(rootfs.join("etc/dvmm-stack-project"), format!("{}\n", cfg.stack_project))?;
    std::fs::write(rootfs.join("etc/dvmm-stack-mem"), format!("{}\n", cfg.stack_mem))?;

    // 6. seed store: the pre-baked image graph the guest copies into its tmpfs.
    std::fs::create_dir_all(rootfs.join("var/lib/containers-seed"))?;
    run(Command::new("cp").arg("-a").arg(&cfg.seed_storage).arg(rootfs.join("var/lib/containers-seed/storage")))?;

    // 7. trim install-time cruft that would only bloat RAM.
    let _ = remove_glob(&rootfs.join("var/cache/apk"));
    let _ = std::fs::remove_file(rootfs.join("etc/resolv.conf"));
    let _ = std::fs::remove_dir_all(rootfs.join("root/.config/containers"));
    std::fs::write(rootfs.join("etc/resolv.conf"), "")?; // empty mount target

    // 7a0. normalize containers/storage "created" timestamps to the fixed epoch.
    normalize_created_json(&rootfs.join("var/lib/containers-seed"))?;

    // 7a. zero containers/storage lock files (random per-writer tokens).
    truncate_locks(&rootfs)?;

    // 7b. (mtime normalization is folded into the cpio emitter: it writes the
    //      fixed epoch for every entry, so no `touch` pass is needed.)

    // 8. emit the initramfs cpio (Rust) + gzip -9 -n.
    cpio::write_initramfs_gz(&rootfs, &cfg.out)?;

    // copy packages.lock next to the (now-retired) build script location.
    std::fs::copy(cfg.work.join("packages.lock"), &cfg.packages_lock_out)?;

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

/// Recursively list regular files under `dir` (no symlink following).
fn walk_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if !d.is_dir() {
            continue;
        }
        for e in std::fs::read_dir(&d)? {
            let p = e?.path();
            let meta = std::fs::symlink_metadata(&p)?;
            if meta.is_dir() {
                stack.push(p);
            } else if meta.is_file() {
                out.push(p);
            }
        }
    }
    Ok(out)
}

// ============================================================================
// content-hash bake cache
//
// The biggest e2e-speed win: `dvmm build` is deterministic (identical inputs ->
// byte-identical `.dvmm`; see artifact_test gate 1), so a build whose inputs are
// unchanged can REUSE the prior outputs and skip the whole pull/squash/assemble
// pipeline. The cache key hashes EVERY input that affects the output bytes; a hit
// restores the `.dvmm`, the per-stack initramfs (+ sha sidecar), and the committed
// compose.lock.yml + stack.lock. `--no-cache` forces a full rebuild (still stored,
// so later runs hit); nightly bake-repeatability uses it to re-bake unconditionally.
// ============================================================================

/// Cache-entry format version. Bump when the cached fileset or key inputs change
/// in a way older entries can't satisfy.
const CACHE_VERSION: u32 = 2;

struct CacheCtx {
    /// The per-key entry directory: <cache-root>/<key>.
    dir: PathBuf,
    /// The full hex key (sha256 over the input manifest below).
    key: String,
}

/// `$DVMM_CACHE_DIR`, else `<repo>/.dvmm-cache` (here == `<repo>/guest`).
fn cache_root(here: &Path) -> PathBuf {
    if let Ok(d) = std::env::var("DVMM_CACHE_DIR") {
        return PathBuf::from(d);
    }
    here.parent()
        .map(|r| r.join(".dvmm-cache"))
        .unwrap_or_else(|| PathBuf::from(".dvmm-cache"))
}

/// A stable content hash of a directory tree: for every regular file (recursively)
/// `<relpath>\0<sha256(content)>\n`, sorted by relpath, then sha256 of the whole.
/// `exclude` drops matching file BASENAMES (this bake's own committed outputs, so
/// the first bake does not bust its own key).
fn tree_hash(root: &Path, exclude: &[&str]) -> std::io::Result<String> {
    if !root.exists() {
        return Ok(format!("MISSING:{}", root.display()));
    }
    let mut entries: Vec<(String, String)> = Vec::new();
    for path in walk_files(root)? {
        let base = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if exclude.contains(&base) {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        entries.push((rel, sha256_file_hex(&path)?));
    }
    entries.sort();
    let mut buf = String::new();
    for (rel, sha) in entries {
        buf.push_str(&rel);
        buf.push('\0');
        buf.push_str(&sha);
        buf.push('\n');
    }
    Ok(artifact::sha256_hex(buf.as_bytes()))
}

/// Best-effort tool version string (empty on failure — still a stable key input).
fn tool_version(prog: &str, args: &[&str]) -> String {
    Command::new(prog)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn compute_cache_key(
    self_exe: &Path,
    here: &Path,
    alpine_dir: &Path,
    compose_dir: &Path,
    stack_name: &str,
    mem_mib: u64,
    working_set_mib: u64,
    squash_threshold_mib: u64,
) -> Result<CacheCtx, String> {
    let repo_root = here
        .parent()
        .ok_or_else(|| "no repo root above guest/".to_string())?;
    let self_sha = sha256_file_hex(self_exe).map_err(|x| format!("hashing dvmm binary: {x}"))?;
    let kernel_sha = sha256_file_hex(&here.join("kernel/vmlinux-6.1.128"))
        .map_err(|x| format!("hashing kernel: {x}"))?;
    // The agent is now a Rust crate built in a pinned musl container: its cache
    // input is the agent + proto crate trees + Cargo.lock + the builder image
    // digest (a toolchain/image bump MUST miss). Fable §2.
    let agent_tree =
        tree_hash(&repo_root.join("dvmm-agent"), &[]).map_err(|x| format!("hashing dvmm-agent: {x}"))?;
    let proto_tree =
        tree_hash(&repo_root.join("dvmm-proto"), &[]).map_err(|x| format!("hashing dvmm-proto: {x}"))?;
    let cargo_lock = sha256_file_hex(&repo_root.join("Cargo.lock"))
        .map_err(|x| format!("hashing Cargo.lock: {x}"))?;
    let builder_digest = read_builder_pin(repo_root)
        .map(|(_, d)| d)
        .map_err(|x| format!("reading builder pin: {x}"))?;
    let overlay_tree =
        tree_hash(&alpine_dir.join("overlay"), &[]).map_err(|x| format!("hashing overlay: {x}"))?;
    let engine_sha = sha256_file_hex(&alpine_dir.join("compose-engine.lock"))
        .map_err(|x| format!("hashing compose-engine.lock: {x}"))?;
    // the stack dir: compose.yml + build contexts + bind sources + service source,
    // EXCLUDING this bake's own committed outputs.
    let stack_tree = tree_hash(compose_dir, &["compose.lock.yml", "stack.lock"])
        .map_err(|x| format!("hashing stack dir: {x}"))?;
    let podman_v = tool_version("podman", &["--version"]);

    let manifest = format!(
        "dvmm-bake-cache v{CACHE_VERSION}\n\
         self:      {self_sha}\n\
         podman:    {podman_v}\n\
         engine:    {engine_sha}\n\
         kernel:    {kernel_sha}\n\
         agent:     {agent_tree}\n\
         proto:     {proto_tree}\n\
         cargolock: {cargo_lock}\n\
         builder:   {builder_digest}\n\
         overlay:   {overlay_tree}\n\
         stackdir:  {stack_tree}\n\
         name:      {stack_name}\n\
         mem:       {mem_mib}\n\
         ws:        {working_set_mib}\n\
         squash:    {squash_threshold_mib}\n"
    );
    let key = artifact::sha256_hex(manifest.as_bytes());
    let dir = cache_root(here).join(&key);
    Ok(CacheCtx { dir, key })
}

/// The baked files a cache entry holds (basenames within the entry dir).
const CACHE_FILES: [&str; 5] = [
    "artifact.dvmm",
    "initramfs.cpio.gz",
    "initramfs.cpio.gz.sha256",
    "compose.lock.yml",
    "stack.lock",
];

fn cache_is_hit(c: &CacheCtx) -> bool {
    c.dir.is_dir()
        && c.dir.join("dvmm_sha256").is_file()
        && CACHE_FILES.iter().all(|f| c.dir.join(f).is_file())
}

/// Restore a hit's outputs into place; returns the cached `.dvmm` sha256.
fn cache_restore(
    c: &CacheCtx,
    out_dvmm: &Path,
    out_initramfs: &Path,
    committed_lock: &Path,
    stack_lock: &Path,
) -> std::io::Result<String> {
    for p in [out_dvmm, out_initramfs, committed_lock] {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
    }
    std::fs::copy(c.dir.join("artifact.dvmm"), out_dvmm)?;
    std::fs::copy(c.dir.join("initramfs.cpio.gz"), out_initramfs)?;
    std::fs::copy(
        c.dir.join("initramfs.cpio.gz.sha256"),
        format!("{}.sha256", out_initramfs.display()),
    )?;
    std::fs::copy(c.dir.join("compose.lock.yml"), committed_lock)?;
    std::fs::copy(c.dir.join("stack.lock"), stack_lock)?;
    Ok(std::fs::read_to_string(c.dir.join("dvmm_sha256"))?
        .trim()
        .to_string())
}

/// Store the freshly-baked outputs under the key (atomic via temp-dir rename).
/// Returns `Ok(false)` if an entry already exists (nothing to do).
fn cache_store(
    c: &CacheCtx,
    out_dvmm: &Path,
    out_initramfs: &Path,
    committed_lock: &Path,
    stack_lock: &Path,
    dvmm_sha: &str,
) -> std::io::Result<bool> {
    if c.dir.exists() {
        return Ok(false);
    }
    let root = c.dir.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(root)?;
    let tmp = root.join(format!(".tmp-{}-{}", std::process::id(), now_nanos()));
    std::fs::create_dir_all(&tmp)?;
    std::fs::copy(out_dvmm, tmp.join("artifact.dvmm"))?;
    std::fs::copy(out_initramfs, tmp.join("initramfs.cpio.gz"))?;
    let sidecar = PathBuf::from(format!("{}.sha256", out_initramfs.display()));
    if sidecar.exists() {
        std::fs::copy(&sidecar, tmp.join("initramfs.cpio.gz.sha256"))?;
    } else {
        std::fs::write(tmp.join("initramfs.cpio.gz.sha256"), b"")?;
    }
    std::fs::copy(committed_lock, tmp.join("compose.lock.yml"))?;
    std::fs::copy(stack_lock, tmp.join("stack.lock"))?;
    std::fs::write(tmp.join("dvmm_sha256"), format!("{dvmm_sha}\n"))?;
    // atomic publish; if another builder won the race, drop our temp.
    match std::fs::rename(&tmp, &c.dir) {
        Ok(()) => Ok(true),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(false)
        }
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    #[test]
    fn tree_hash_is_stable_and_content_sensitive() {
        let base = std::env::temp_dir().join(format!("dvmm-th-{}-{}", std::process::id(), now_nanos()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("a.txt"), b"hello").unwrap();
        std::fs::write(base.join("sub/b.txt"), b"world").unwrap();

        let h1 = tree_hash(&base, &[]).unwrap();
        assert_eq!(h1, tree_hash(&base, &[]).unwrap(), "same tree -> same hash");

        // an excluded output file must not affect the key.
        std::fs::write(base.join("stack.lock"), b"ignored").unwrap();
        assert_eq!(h1, tree_hash(&base, &["stack.lock"]).unwrap(), "excluded file ignored");

        // a content change must flip the key.
        std::fs::write(base.join("a.txt"), b"HELLO").unwrap();
        assert_ne!(h1, tree_hash(&base, &["stack.lock"]).unwrap(), "content change -> new hash");

        let _ = std::fs::remove_dir_all(&base);
    }
}
