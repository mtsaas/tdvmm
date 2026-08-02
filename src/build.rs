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
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::artifact;
use crate::compose;
use crate::cpio;
use crate::engine;
use crate::ui;

// ---- pins (mirror bake-stack.sh + build_rootfs.sh) -------------------------

const BUILD_EPOCH: &str = "1785542400";
const BUSYBOX_REF: &str = "docker.io/library/busybox@sha256:dc2d74b28e4cf8984fa52af1f39bc7c3d9c73760b41a74d629f5d11b1ab28616";
const VMM_MAX_MEM_MIB: u64 = 3072;
const DEFAULT_MEM_MIB: u64 = 3072;
const DEFAULT_WORKING_SET_MIB: u64 = 512;
const DEFAULT_SQUASH_THRESHOLD_MIB: u64 = 100;

/// The `dvmm build` progress bar's step count (Fable CLI-UX ruling): resolve
/// inputs, bake cache, squash images, seed store, compose.lock + binds,
/// assemble initramfs, pack artifact, cache + diagnostics.
const TOTAL_STEPS: u32 = 8;

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
    /// Cache directory override (Fable Part A). Precedence: this > `DVMM_CACHE_DIR`
    /// > `$HOME/.dvmm`. `None` falls through to env/default.
    pub cache_dir: Option<String>,
    /// Disable the progress spinner (Fable CLI-UX ruling): `--no-progress`, or
    /// implied by a non-terminal stderr / `CI` / `TERM=dumb` (decided in `ui`).
    pub no_progress: bool,
}

/// `dvmm build-kernel` args.
pub struct BuildKernelArgs {
    pub out: Option<String>,
    pub cache_dir: Option<String>,
    pub force_build: bool,
    pub record: bool,
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
/// `.output()` already captures both streams, so this needs no `OutputMode`
/// knob — the full stderr is already in the error on failure.
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

/// Run a command for its side effect, per `mode` (Fable CLI-UX ruling — see
/// `engine::run`). A thin pass-through: the engine choke point does the work.
fn run(cmd: &mut Command, mode: engine::OutputMode) -> Result<(), String> {
    engine::run(cmd, mode)
}

/// Bundles the progress handle + child-output mode threaded through the bake
/// pipeline's helper functions. A plain borrowed local — never a global
/// (Fable coexistence rule) — so it cannot outlive `cmd_build`'s call.
/// `build-agent` / `build-kernel` construct a permanently-inherit instance
/// (via [`Ux::inherit`]) over a [`ui::Progress::disabled`], so their output
/// stays a plain inherited passthrough, unaffected by the progress bar.
struct Ux<'a> {
    progress: &'a ui::Progress,
    mode: engine::OutputMode,
}

impl<'a> Ux<'a> {
    /// For commands that share bake-pipeline helpers but must never show
    /// progress UI or capture child output (scope lock — progress is
    /// `build`-only): `build-agent`, `build-kernel`.
    fn inherit(progress: &'a ui::Progress) -> Ux<'a> {
        Ux { progress, mode: engine::OutputMode::Inherit }
    }
}

/// A container-engine invocation against a scratch vfs store (mirrors `bp()`),
/// with the clean CONTAINERS_CONF set. Routes through the single engine choke
/// point (Fable guardrail §2).
fn podman(store: &Path, runroot: &Path, conf: &Path) -> Command {
    engine::scratch(store, runroot, conf)
}

fn sha256_file_hex(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    let data = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&data);
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

// ============================================================================
// in-process filesystem helpers (Move 3, Step D) — replace the host `cp -a` /
// `chmod` / `install -D -m` shell-outs. The `.dvmm` bytes come ONLY from the
// normalizing cpio/artifact packers (Fable guardrail §2), so these helpers only
// need to reproduce {file type, content, symlink target, permission bits}:
// ownership, mtime, hardlink identity and sparseness are all normalized (or, for
// hardlinks, reconstructed from dev/inode) by the packer and are NOT preserved
// here. Seed layers + overlay + bind trees are plain files (Fable-locked).
// ============================================================================

/// `cp -a <src> <dst>` — recursively copy the single filesystem entity at `src`
/// to the path `dst` (directories recurse; symlinks are recreated as symlinks,
/// never followed; permission bits preserved). Directory modes are set only when
/// the directory is freshly created, so merging into an existing tree leaves that
/// tree's directory modes untouched (cp -a parity).
fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    let ft = meta.file_type();
    if ft.is_symlink() {
        let target = std::fs::read_link(src)?;
        let _ = std::fs::remove_file(dst);
        std::os::unix::fs::symlink(target, dst)?;
    } else if ft.is_dir() {
        if !dst.exists() {
            std::fs::create_dir(dst)?;
            std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.mode() & 0o7777))?;
        }
        for de in std::fs::read_dir(src)? {
            let de = de?;
            copy_tree(&de.path(), &dst.join(de.file_name()))?;
        }
    } else {
        // regular file (block/char/fifo nodes never occur in our copied trees).
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// `cp -a <src>/. <dst>/` — merge the CONTENTS of directory `src` into the
/// existing directory `dst` (recursively). Used for the overlay + bind trees.
fn copy_dir_contents(src: &Path, dst: &Path) -> std::io::Result<()> {
    for de in std::fs::read_dir(src)? {
        let de = de?;
        copy_tree(&de.path(), &dst.join(de.file_name()))?;
    }
    Ok(())
}

/// `chmod <mode> <path>` — set exactly the given permission bits.
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// `install -D -m <mode> <src> <dst>` — create `dst`'s parent dirs, copy `src`'s
/// contents to `dst`, then set the mode.
fn install_file(src: &Path, dst: &Path, mode: u32) -> std::io::Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(src, dst)?;
    set_mode(dst, mode)?;
    Ok(())
}

/// Extract an UNCOMPRESSED tar archive into `dest`, in-process via the pure-Rust
/// `tar` crate (Move 3 — replaces the host `tar`). The archive is untrusted
/// TRANSPORT input (Fable guardrail §2): the `.dvmm` bytes come solely from the
/// normalizing cpio packer, which re-derives uid/gid 0 + epoch + sorted order +
/// hardlink groups; only {file type, content, link target, permission bits} are
/// consumed here. Overwrites so a re-extract into a populated dir is idempotent.
fn extract_tar(tarball: &Path, dest: &Path) -> std::io::Result<()> {
    let f = std::fs::File::open(tarball)?;
    let mut ar = tar::Archive::new(f);
    ar.set_preserve_permissions(true);
    ar.set_overwrite(true);
    ar.unpack(dest)?;
    Ok(())
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

    // ---- progress UI (Fable CLI-UX ruling) ---------------------------------
    // A LOCAL value, never a global; `.finish()`/`Drop` clear it on every exit
    // path below, including `?`-propagated errors. `mode` picks whether the
    // noisy podman child processes stream live (today's behavior) or are
    // captured and dumped only on failure — chosen HERE, from whether the bar
    // is actually active, never inside the UI-free `engine` choke point.
    let progress = ui::Progress::new(args.no_progress);
    let mode = if progress.active() {
        engine::OutputMode::CaptureOnFailure
    } else {
        engine::OutputMode::Inherit
    };
    let ux = Ux { progress: &progress, mode };

    // TTY-only banner; a no-op in frozen mode. Everything below that used to
    // print a routine `== section ==`/detail line now goes through
    // `ux.progress.detail()` instead of `.println()`: byte-identical in frozen
    // mode (same `eprintln!` fallback), but suppressed on the TTY (relocated
    // into `diag`, folded into the diagnostics file at the end).
    ux.progress.title(&stack_name);
    let mut diag = String::new();

    ux.progress.step(1, TOTAL_STEPS, "resolve inputs");
    ux.progress.detail(format!("== dvmm build: stack={stack_name} project={project} mem={mem_mib}MiB =="));
    ux.progress.detail(format!("   compose: {}", compose_path.display()));
    diag.push_str(&format!("compose_path: {}\n", compose_path.display()));

    // --- cache dir (Fable Part A): --cache-dir > $DVMM_CACHE_DIR > $HOME/.dvmm ---
    let (cache_dir, cache_src) = resolve_cache_dir(args.cache_dir.as_deref());
    ux.progress.detail(format!("   cache-dir: {} (source: {cache_src})", cache_dir.display()));
    diag.push_str(&format!("cache_dir: {} (source: {cache_src})\n", cache_dir.display()));

    // --- kernel (Fable Part C): fetch the pinned release asset, else reproducibly
    //     build it in the pinned container; verified against kernel.lock. Done
    //     BEFORE the cache key so the kernel bytes are a present, hashable input. ---
    let repo_root = here
        .parent()
        .ok_or("could not locate the repo root above guest/")?
        .to_path_buf();
    let kernel = ensure_kernel(&here, &cache_dir, false, &ux)?;
    diag.push_str(&format!("kernel: {} sha256={}\n", kernel.display(), sha256_file_hex(&kernel).unwrap_or_default()));

    // --- builder-image pins (Fable Part B): the DECLARED, host-identical toolchain
    //     anchors that REPLACE the host-probed `podman --version` in both the hashed
    //     artifact bytes and the cache key. Sorted for order-stability. ---
    let builders = collect_builder_pins(&repo_root, &here)?;
    ux.progress.detail(format!("   builders: {}", builders.join(" ")));

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
    // the guest overlay tree, the pinned compose engine, the DECLARED builder-image
    // digests (Fable Part B — replacing the host-probed podman version, which is
    // gone), the dvmm binary itself (all compiled-in pins + bake logic), and the
    // sizing knobs. `dvmm build` is deterministic (artifact_test gate 1), so a hit
    // reusing the prior `.dvmm` is byte-identical to a fresh bake.
    ux.progress.step(2, TOTAL_STEPS, "bake cache");
    let cache = match compute_cache_key(
        &self_exe, &here, &alpine_dir, &compose_dir, &stack_name, mem_mib, working_set_mib,
        squash_threshold_mib, &cache_dir, &kernel, &builders,
    ) {
        Ok(c) => Some(c),
        Err(e) => {
            ux.progress.println(format!("{}: bake cache disabled (key error: {e})", compose::WARN));
            None
        }
    };
    if let Some(c) = &cache {
        diag.push_str(&format!("bake_cache_key: {}\n", c.key));
        if !args.no_cache && cache_is_hit(c) {
            ux.progress.note("hit → reuse");
            diag.push_str("bake_cache_status: HIT (reused)\n");
            ux.progress.detail(format!("== BAKE CACHE HIT ==  key={}", &c.key[..16]));
            ux.progress.detail(format!("   reusing baked artifacts (skipped pull/squash/assemble): {}", c.dir.display()));
            match cache_restore(c, &out_dvmm, &out_initramfs, &committed_lock, &stack_lock_path) {
                Ok(dvmm_sha) => {
                    ux.progress.detail(format!("   .dvmm:     {} (sha256 {dvmm_sha})", out_dvmm.display()));
                    let size = std::fs::metadata(&out_dvmm).map(|m| m.len()).unwrap_or(0);
                    ux.progress.print_summary(&out_dvmm, &dvmm_sha, size, progress.elapsed(), None);
                    progress.finish();
                    // stdout: the artifact identity line (parity with the old pack-dvmm.sh) —
                    // `suspend` just runs the closure directly once the bar is finished; kept
                    // for symmetry with the other stdout site below.
                    ux.progress.suspend(|| println!("{dvmm_sha}  {}", out_dvmm.display()));
                    return Ok(0);
                }
                Err(e) => {
                    ux.progress.note("restore failed → full bake");
                    ux.progress.println(format!("{}: cache restore failed ({e}); rebuilding", compose::WARN));
                }
            }
        } else if args.no_cache {
            ux.progress.note("bypassed → full bake");
            diag.push_str("bake_cache_status: BYPASSED (--no-cache)\n");
            ux.progress.detail(format!("== bake cache BYPASSED (--no-cache): forcing full rebuild ==  key={}", &c.key[..16]));
        } else {
            ux.progress.note("miss → full bake");
            diag.push_str("bake_cache_status: MISS\n");
            ux.progress.detail(format!("== BAKE CACHE MISS ==  key={} (full bake) ==", &c.key[..16]));
        }
    }

    // --- scratch workdir + clean CONTAINERS_CONF ---
    let work = mkdtemp()?;
    let conf = work.join("containers.conf");
    std::fs::write(&conf, "[engine]\n")?;

    // Host-probed engine version — Fable guardrail §3: it must NOT enter the hashed
    // artifact bytes OR the cache key (it breaks cross-host byte-identity). It is
    // captured for DEBUGGING ONLY and written to a side diagnostics file under the
    // (disposable) cache dir — never into the .dvmm, the manifest, or stack.lock.
    let host_podman_version = capture(engine::command().arg("--version"))
        .ok()
        .and_then(|s| s.split_whitespace().nth(2).map(|v| v.to_string()))
        .unwrap_or_default();

    ux.progress.step(3, TOTAL_STEPS, "pull + build images");
    ux.progress.detail(format!("   images: {}", validated.images.join(" ")));
    if !validated.builds.is_empty() {
        let tags: Vec<&str> = validated.builds.iter().map(|b| b.image_tag.as_str()).collect();
        ux.progress.detail(format!("   builds: {}", tags.join(" ")));
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
        bake_one(&bstore, &brun, &conf, reff, squash_threshold_mib, &work, &mut records, &mut plain_refs, &mut plain_pin, &mut squash_tars, &mut total_img_mib, &ux)?;
        // TTY: the aligned sub-list entry (name + size) for this user-declared
        // image; a no-op in frozen mode. The self-test busybox pull (below)
        // deliberately has no entry — it isn't part of the stack the user asked
        // to build.
        if let Some(r) = records.last() {
            ux.progress.item(squash_base_name(reff), r.size_mib);
        }
    }
    if !validated.builds.is_empty() {
        ux.progress.detail("== build build: services (host-side) ==");
        for b in &validated.builds {
            build_one(&bstore, &brun, &conf, b, &work, &mut records, &mut squash_tars, &mut total_img_mib, &ux)?;
            if let Some(r) = records.last() {
                ux.progress.item(b.service.clone(), r.size_mib);
            }
        }
    }
    ux.progress.detail("== bake self-test image (busybox, plain) ==");
    bake_one(&bstore, &brun, &conf, BUSYBOX_REF, squash_threshold_mib, &work, &mut records, &mut plain_refs, &mut plain_pin, &mut squash_tars, &mut total_img_mib, &ux)?;
    let selftest_pin = plain_pin.get(BUSYBOX_REF).cloned().unwrap_or_default();
    for r in &records {
        diag.push_str(&format!(
            "image: policy={} upstream={} content_id={} size_mib={}\n",
            r.policy, r.upstream, r.content_id, r.size_mib
        ));
    }

    // --- 3. build the seed store (podman unshare) ---
    ux.progress.step(4, TOTAL_STEPS, "seed store");
    ux.progress.detail("== build seed store ==");
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
    run(engine::unshare(&conf)
        .arg(&self_exe)
        .arg("__seed-build")
        .arg("--config")
        .arg(&seed_cfg_path), ux.mode)?;
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
    ux.progress.step(5, TOTAL_STEPS, "compose.lock + binds");
    ux.progress.detail("== emit compose.lock.yml ==");
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
    diag.push_str(&format!("compose_lock_sha256: {lock_sha}\n"));

    // materialize relative binds into a staging tree.
    let binds_stage = work.join("binds");
    std::fs::create_dir_all(&binds_stage)?;
    for (src, dest_rel) in &lock.bind_manifest {
        let dest = binds_stage.join(dest_rel);
        std::fs::create_dir_all(dest.parent().unwrap())?;
        copy_tree(Path::new(src), &dest)
            .map_err(|e| format!("materialize bind {src}: {e}"))?;
        ux.progress.detail(format!("   materialized  {src}  ->  {binds_base}/{dest_rel}"));
        diag.push_str(&format!("bind: {src} -> {binds_base}/{dest_rel}\n"));
    }

    // --- 5. RAM estimate ---
    let est_mib = ((2.5 * total_img_mib as f64) + working_set_mib as f64 + 512.0).ceil() as u64;
    ux.progress.detail("== RAM estimate ==");
    ux.progress.detail(format!("   total image size: {total_img_mib} MiB;  estimate >= {est_mib} MiB (2.5x img + {working_set_mib} ws + 512 base)"));
    diag.push_str(&format!("ram_estimate: total_img_mib={total_img_mib} estimate_mib={est_mib} configured_mib={mem_mib}\n"));
    if mem_mib < est_mib {
        ux.progress.println(format!("{}: configured guest RAM {mem_mib} MiB is below the estimate {est_mib} MiB.", compose::WARN));
    } else {
        ux.progress.detail(format!("   configured {mem_mib} MiB >= estimate {est_mib} MiB (OK)"));
    }
    if mem_mib > VMM_MAX_MEM_MIB {
        ux.progress.println(format!("{}: {mem_mib} MiB exceeds the current VMM cap {VMM_MAX_MEM_MIB} MiB (32-bit MMIO gap);", compose::WARN));
    }

    // --- 6. assemble the per-stack initramfs (build_rootfs, stack mode) ---
    ux.progress.step(6, TOTAL_STEPS, "assemble initramfs");
    ux.progress.detail("== assemble initramfs (Rust rootfs + cpio) ==");
    // (out_initramfs computed early, above, for the cache path)

    // build the dvmm-agent (static musl, reproducible) in the pinned builder
    // container, before the unshare. Returns the embedded build hash (the compat
    // oracle reported by ping/hello); its file sha256 goes in the ledger + anchors.
    let agent_bin = work.join("dvmm-agent");
    let agent_build_hash = build_agent(&here, &agent_bin, &ux)?;
    let agent_sha = sha256_file_hex(&agent_bin)?;
    // TTY: deliberately NOT shown on the step line — it's diagnostic (relocated
    // to `diag` below), not routine build progress.
    ux.progress.detail(format!("   dvmm-agent: sha256 {agent_sha}  build {agent_build_hash}"));
    diag.push_str(&format!("agent_sha256: {agent_sha}\nagent_build_hash: {agent_build_hash}\n"));

    // fetch + verify the pinned minirootfs + compose binary (cached in alpine_dir).
    let mirror = std::env::var("ALPINE_MIRROR").unwrap_or_else(|_| DEFAULT_MIRROR.to_string());
    let tarball = alpine_dir.join(MINIROOTFS);
    fetch_verify(
        &tarball,
        &format!("{mirror}/{ALPINE_BRANCH}/releases/x86_64/{MINIROOTFS}"),
        MINIROOTFS_SHA256,
        &ux,
    )?;
    let (compose_version, compose_sha256) = read_compose_lock(&alpine_dir)?;
    let compose_cache = alpine_dir.join(format!("docker-compose-{compose_version}"));
    fetch_verify(
        &compose_cache,
        &format!("https://github.com/docker/compose/releases/download/{compose_version}/docker-compose-linux-x86_64"),
        &compose_sha256,
        &ux,
    )?;

    // --- Fable Part D: the shared base-runtime segment cache. Keyed on DECLARED
    //     base pins only (Alpine + package set + overlay + agent + compose engine +
    //     epoch + self-test pin) — NOT the stack. A hit lets `__assemble` skip the
    //     apk install/overlay entirely and reuse the cached cpio segment. --no-cache
    //     forces a base rebuild (still stored). ---
    // The pinned rootfs-builder image (Move 3 Step C) — folded into the base key so
    // an image bump busts the base-runtime cache and re-resolves the package set.
    let (rb_img, rb_dig) = read_rootfs_builder_pin(&here)?;
    let rootfs_builder = format!("{rb_img}@{rb_dig}");
    let base_key = compute_base_key(&alpine_dir, &agent_sha, &compose_version, &compose_sha256, &selftest_pin, &rootfs_builder);
    let base_entry = cache_dir.join("base-runtime").join(&base_key);
    let base_seg_cached = base_entry.join("base.cpio");
    let base_pl_cached = base_entry.join("packages.lock");
    let base_hit = !args.no_cache && base_seg_cached.is_file();
    let base_status = if base_hit {
        "HIT"
    } else if args.no_cache {
        "BYPASSED (--no-cache)"
    } else {
        "MISS"
    };
    diag.push_str(&format!("base_runtime_cache_key: {base_key}\nbase_runtime_cache_status: {base_status} ({})\n", base_entry.display()));
    if base_hit {
        ux.progress.detail(format!("== base-runtime cache HIT ==  key={} ({})", &base_key[..16], base_entry.display()));
    } else if args.no_cache {
        ux.progress.detail(format!("== base-runtime cache BYPASSED (--no-cache) ==  key={}", &base_key[..16]));
    } else {
        ux.progress.detail(format!("== base-runtime cache MISS ==  key={} (building base layer)", &base_key[..16]));
    }
    // MISS: __assemble writes the fresh base segment to work/base.cpio; the host
    // stores it after the unshare. HIT: __assemble reads the cached segment.
    let base_segment = if base_hit { base_seg_cached.clone() } else { work.join("base.cpio") };

    // MISS: build the base Alpine rootfs in the pinned rootfs-builder container
    // (apk --root, NO chroot) BEFORE the unshare — build_agent's slot. Produces
    // base-rootfs.tar (untrusted transport) + packages.lock (the resolution
    // ledger). On a HIT this is skipped entirely (cached cpio segment reused).
    let base_build_dir = work.join("base-build");
    let base_rootfs_tar = base_build_dir.join("base-rootfs.tar");
    if !base_hit {
        build_base_rootfs(&rootfs_builder, &tarball, &mirror, ALPINE_BRANCH, PKGS, &base_build_dir, &ux)?;
        // stage the container-emitted package ledger for __assemble + the base cache.
        std::fs::copy(base_build_dir.join("packages.lock"), work.join("packages.lock"))?;
    }

    let assemble_cfg = AssembleConfig {
        conf: conf.clone(),
        work: work.clone(),
        base_rootfs_tar: base_rootfs_tar.clone(),
        build_epoch: BUILD_EPOCH.to_string(),
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
        base_hit,
        base_segment: base_segment.clone(),
        base_packages_lock: base_pl_cached.clone(),
    };
    let assemble_cfg_path = work.join("assemble-config.json");
    std::fs::write(&assemble_cfg_path, serde_json::to_vec(&assemble_cfg)?)?;
    run(engine::unshare(&conf)
        .arg(&self_exe)
        .arg("__assemble-initramfs")
        .arg("--config")
        .arg(&assemble_cfg_path), ux.mode)?;

    // MISS: store the freshly-emitted base segment (+ its package set) for reuse.
    if !base_hit {
        if let Err(e) = base_cache_store(&base_entry, &base_segment, &alpine_dir.join("packages.lock")) {
            ux.progress.println(format!("{}: could not populate base-runtime cache ({e})", compose::WARN));
        } else {
            ux.progress.detail(format!("   base cached: {} (key {})", base_entry.display(), &base_key[..16]));
        }
    }

    let art_sha = sha256_file_hex(&out_initramfs)?;

    // --- 7. write stack.lock (the reproducibility ledger — declared inputs only,
    //        NO host-probed values; Fable guardrail §3) ---
    ux.progress.step(7, TOTAL_STEPS, "pack artifact");
    write_stack_lock(&here, &stack_name, &project, mem_mib, est_mib, &lock_sha, &art_sha, &agent_sha, &out_initramfs, &records, &plain_refs, &plain_pin, &seedpins, &validated, &builders)?;

    // stash the emitted lock next to the manifest.
    std::fs::copy(&lock_path, &committed_lock)?;

    // --- 8. pack the single-file .dvmm artifact ---
    ux.progress.detail("== pack .dvmm artifact ==");
    let dvmm_bytes = pack_dvmm(&self_exe, &records, &compose_version, &compose_sha256, &stack_name, &project, mem_mib, est_mib, &builders, &agent_sha, &agent_build_hash, &kernel, &out_initramfs, &lock_path)?;
    std::fs::write(&out_dvmm, &dvmm_bytes)?;
    let dvmm_sha = artifact::sha256_hex(&dvmm_bytes);
    // append the artifact identity to the ledger.
    append_stack_lock_dvmm(&here, &stack_name, &dvmm_sha, &out_dvmm)?;
    diag.push_str(&format!(
        "initramfs: {} sha256={art_sha}\ndvmm: {} sha256={dvmm_sha}\n",
        out_initramfs.display(), out_dvmm.display(),
    ));

    ux.progress.detail("");
    ux.progress.detail("== dvmm build DONE ==");
    ux.progress.detail(format!("   initramfs: {}", out_initramfs.display()));
    ux.progress.detail(format!("   sha256:    {art_sha}"));
    ux.progress.detail(format!("   .dvmm:     {} (sha256 {dvmm_sha})", out_dvmm.display()));
    // stdout: the artifact identity line (parity with the old pack-dvmm.sh) —
    // UNCHANGED by the TTY redesign (progress/chrome is stderr-only). Routed
    // through `suspend` so a still-ticking step-7 spinner can't interleave
    // with this raw stdout write (the bar is cleared for the print, then
    // redrawn) — frozen/non-TTY: `suspend` just calls the closure directly.
    ux.progress.suspend(|| println!("{dvmm_sha}  {}", out_dvmm.display()));

    // --- 9. populate the bake cache (best-effort; never fails the build) ---
    ux.progress.step(8, TOTAL_STEPS, "cache");
    if let Some(c) = &cache {
        match cache_store(c, &out_dvmm, &out_initramfs, &committed_lock, &stack_lock_path, &dvmm_sha) {
            Ok(true) => {
                ux.progress.detail(format!("   cached:    {} (key {})", c.dir.display(), &c.key[..16]));
                diag.push_str(&format!("bake_cache_stored: {} (key {})\n", c.dir.display(), c.key));
            }
            Ok(false) => {} // entry already present
            Err(e) => ux.progress.println(format!("{}: could not populate bake cache ({e})", compose::WARN)),
        }
    }

    // --- 10. side diagnostics (Fable guardrail §3): host-probed values, cache
    //         mechanics, full digests, and absolute paths live ONLY here, never
    //         in the artifact, stdout, the cache key, or the clean TTY view —
    //         relocated, not lost. Disposable; best-effort. ---
    let diag_path = write_bake_diagnostics(&cache_dir, &stack_name, &host_podman_version, &builders, &diag, ux.progress);

    // TTY: the aligned final summary block (Fix: the bar is cleared FIRST, so
    // the step counter deterministically reaches [8/8] with no stale spinner
    // frame). A no-op in frozen mode — the plain `DONE` block above already
    // covers it byte-for-byte.
    ux.progress.print_summary(&out_dvmm, &dvmm_sha, dvmm_bytes.len() as u64, progress.elapsed(), diag_path.as_deref());

    let _ = std::fs::remove_dir_all(&work);
    Ok(0)
}

/// Write host-probed + relocated bake diagnostics to a side file under the
/// (disposable) cache dir. NOTHING here enters the `.dvmm` bytes or the bake
/// cache key. `extra` is the run's accumulated diagnostic detail (full digests,
/// cache keys, the agent sha/build hash, absolute paths, …) — the ROUTINE
/// detail that used to wall the TTY, now relocated here instead of lost
/// (`build.rs`'s `diag` accumulator). Returns the written path on success (used
/// for the TTY summary's `details` line); `None` is a best-effort failure.
fn write_bake_diagnostics(cache_dir: &Path, stack: &str, podman_version: &str, builders: &[String], extra: &str, progress: &ui::Progress) -> Option<PathBuf> {
    let dir = cache_dir.join("diagnostics");
    if std::fs::create_dir_all(&dir).is_err() {
        return None; // best-effort only
    }
    let body = format!(
        "# dvmm bake diagnostics (host-probed + relocated detail; NOT in the artifact\n\
         # or cache key — see stack.lock for the compared, reproducible ledger).\n\
         stack: {stack}\n\
         host_podman_version: {podman_version}\n\
         baked_at: {}\n\
         builder_images:\n{}\n\
         {extra}",
        utc_now_iso(),
        builders.iter().map(|b| format!("  - {b}")).collect::<Vec<_>>().join("\n"),
    );
    let path = dir.join(format!("{stack}.txt"));
    if std::fs::write(&path, body).is_err() {
        return None;
    }
    // Frozen/non-TTY: unchanged (byte-identical `println` fallback). TTY: this
    // routine line is suppressed — the summary's `details` line replaces it.
    progress.detail(format!("   diagnostics: {} (host-probed, not hashed)", path.display()));
    Some(path)
}

/// Resolve the repo `guest/` directory relative to the running binary (target/…).
/// Falls back to `guest/` under the current dir.
fn self_here() -> Result<PathBuf, Box<dyn std::error::Error>> {
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
fn build_one(
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

/// Read the pinned Alpine rootfs-builder image ref + digest from
/// `guest/initramfs-alpine/rootfs-builder.lock` (Move 3 Step C). This image
/// assembles the base rootfs (`apk --root`) AND serves as the fetch container
/// (busybox `wget`). Returns `(image, digest)`.
fn read_rootfs_builder_pin(here: &Path) -> Result<(String, String), Box<dyn std::error::Error>> {
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
fn fetch_in_container(dest: &Path, url: &str, ux: &Ux) -> Result<(), Box<dyn std::error::Error>> {
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
    let confdir = mkdtemp()?;
    let conf = confdir.join("containers.conf");
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
    let _ = std::fs::remove_dir_all(&confdir);
    Ok(())
}

/// Build the base Alpine rootfs INSIDE the pinned rootfs-builder container (Move 3
/// Step C — replaces the in-`unshare` chroot apk). Extracts the (already
/// sha-verified) minirootfs, writes the pinned repositories, installs `PKGS` via
/// `apk --root` (NO chroot anywhere), records the resolved package set, clears
/// `/dev` (the cpio packer supplies device nodes), and tars the rootfs to
/// `<out_dir>/base-rootfs.tar` (+ `<out_dir>/packages.lock`). The tar is untrusted
/// TRANSPORT (Fable guardrail §2): `__assemble` re-packs it via the normalizing
/// cpio emitter, so container ownership / order / mtime are all discarded. Routed
/// through the engine choke point; OutputMode comes from the orchestrator.
fn build_base_rootfs(
    img_ref: &str,
    minirootfs: &Path,
    mirror: &str,
    alpine_branch: &str,
    pkgs: &[&str],
    out_dir: &Path,
    ux: &Ux,
) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(out_dir)?;
    let confdir = mkdtemp()?;
    let conf = confdir.join("containers.conf");
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
    let _ = std::fs::remove_dir_all(&confdir);
    Ok(())
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
fn build_agent(here: &Path, out: &Path, ux: &Ux) -> Result<String, Box<dyn std::error::Error>> {
    let repo_root = here
        .parent()
        .ok_or("could not locate the repo root above guest/")?
        .to_path_buf();
    let (image, digest) = read_builder_pin(&repo_root)?;
    let img_ref = format!("{image}@{digest}");
    let build_hash = agent_src_id(&repo_root)?;
    ux.progress.detail(format!("building dvmm-agent (static musl, reproducible) in {img_ref}"));

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

    run(engine::command()
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
        .args(["sh", "-c", script]), ux.mode)?;

    std::fs::copy(work.join("dvmm-agent"), out)
        .map_err(|e| format!("agent build produced no binary: {e}"))?;
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&confdir);
    Ok(build_hash)
}

/// `dvmm build-agent -o <path>`: build the reproducible musl agent standalone
/// (the size + double-build byte-identity gate scripts use this). Prints
/// `<sha256>  <path>` to stdout. Shares `build_agent()` with `dvmm build`'s
/// pipeline, but ALWAYS with progress UI disabled / output inherited (scope
/// lock — progress is `build`-only): a plain inherited passthrough.
pub fn cmd_build_agent(out: &str) -> Result<i32, Box<dyn std::error::Error>> {
    let here = self_here()?;
    let outp = PathBuf::from(out);
    if let Some(parent) = outp.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let progress = ui::Progress::disabled();
    let ux = Ux::inherit(&progress);
    let build_hash = build_agent(&here, &outp, &ux)?;
    let sha = sha256_file_hex(&outp)?;
    let size = std::fs::metadata(&outp)?.len();
    eprintln!("   dvmm-agent: {size} bytes  build={build_hash}  sha256={sha}");
    println!("{sha}  {}", outp.display());
    Ok(0)
}

fn fetch_verify(path: &Path, url: &str, sha: &str, ux: &Ux) -> Result<(), Box<dyn std::error::Error>> {
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
// builder-image pins (Fable Part B) — the DECLARED, host-identical toolchain
// anchors that go into the hashed manifest + the cache key (replacing the
// host-probed podman version). Sorted for order-stability.
// ============================================================================

/// The pinned builder-image refs (`image@sha256`) for the guest binaries: the
/// musl agent builder (`dvmm-agent/images.lock`) + the kernel builder
/// (`guest/kernel/kernel.lock`). Sorted.
fn collect_builder_pins(
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

// ============================================================================
// kernel (Fable Part C) — reproducible containerized build + fetch-with-fallback
//
// The guest kernel is acquired EITHER by fetching the pinned GitHub release asset
// (PRIMARY, sha256-verified against kernel.lock) OR by a reproducible build inside
// the pinned builder container (FALLBACK). Both paths MUST yield the byte-identical
// vmlinux recorded in kernel.lock. No host kernel toolchain is required.
// ============================================================================

/// The kernel config baked into the guest (Firecracker microvm config, HPET off,
/// all built-in). Lives in `guest/kernel/`; hashed into kernel.lock.
const KERNEL_CONFIG_NAME: &str = "microvm-kernel-x86_64-6.1.config";

/// The reproducibility ledger for the guest kernel (`guest/kernel/kernel.lock`).
/// Empty `sha256`/`source_sha256`/`builder_digest` mean "not yet recorded" — the
/// `--record` bootstrap fills them.
#[derive(Default, Clone)]
struct KernelLock {
    version: String,
    sha256: String,
    config_sha256: String,
    source_url: String,
    source_sha256: String,
    builder_image: String,
    builder_digest: String,
    release_asset_url: String,
    release_asset_name: String,
}

fn kernel_lock_path(here: &Path) -> PathBuf {
    here.join("kernel/kernel.lock")
}

fn read_kernel_lock(here: &Path) -> Result<KernelLock, Box<dyn std::error::Error>> {
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
        "# deterministic-vmm guest kernel pin (Fable Part C).\n\
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
fn ensure_kernel(
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
    let confdir = mkdtemp()?;
    let conf = confdir.join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    let work = mkdtemp()?;
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
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&confdir);
    Ok(())
}

/// Resolve an image's pinned digest by pulling the ref and reading RepoDigests.
/// `--record`-only (never in `dvmm build`'s pipeline): always inherits, like
/// every other command outside the `build` orchestrator.
fn resolve_image_digest(image: &str) -> Result<String, Box<dyn std::error::Error>> {
    let confdir = mkdtemp()?;
    let conf = confdir.join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    run(engine::command().env("CONTAINERS_CONF", &conf).args(["pull", "-q", image]), engine::OutputMode::Inherit)?;
    let repo = image.split(':').next().unwrap_or(image);
    let digests = capture(engine::command().env("CONTAINERS_CONF", &conf).args([
        "image", "inspect", image, "--format", "{{range .RepoDigests}}{{println .}}{{end}}",
    ]))?;
    let _ = std::fs::remove_dir_all(&confdir);
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
    builders: &[String],
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
    // Declared, host-identical builder-image pins (Fable Part B): reproducible
    // across hosts, so they ARE part of the compared/repeatable portion (unlike
    // the host-probed diagnostics line, which lives outside this ledger).
    for b in builders {
        out.push_str(&format!("builder_image        {b}\n"));
    }
    for line in &prov {
        out.push_str(&format!("  {line}\n"));
    }
    out.push_str("# --- informational (NOT compared for repeatability) ---\n");
    out.push_str("# host-probed values (podman version, baked-at) are written to the\n");
    out.push_str("# cache dir's diagnostics/ side file, NEVER here (Fable guardrail §3).\n");
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
    builders: &[String],
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
                builders: builders.to_vec(),
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
        let mut c = engine::scratch(&cfg.store, &cfg.runroot, &cfg.conf);
        c.args(args);
        c
    };
    for reff in &cfg.plains {
        run(&mut sp(&["pull", "-q", reff]), engine::OutputMode::Inherit)?;
    }
    for s in &cfg.squash {
        run(sp(&["load", "-q", "-i"]).arg(&s.tar), engine::OutputMode::Inherit)?;
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
    /// The base rootfs tarball produced by the pinned rootfs-builder container
    /// (Move 3 Step C). Extracted in-process on a base-cache MISS; unused on a HIT
    /// (the cached cpio segment is read instead). Untrusted transport (§2).
    base_rootfs_tar: PathBuf,
    build_epoch: String,
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
    // --- Fable Part D: shared base-runtime cpio segment ---
    /// True when the base runtime segment (Alpine + podman/crun/... + agent +
    /// compose) is already cached: skip the expensive base BUILD and reuse the
    /// cached segment. False forces a full base build (also stored, for later).
    base_hit: bool,
    /// Base segment path. HIT: the cached `base.cpio` to READ. MISS: where to WRITE
    /// the freshly-emitted base segment (the host then stores it to the cache).
    base_segment: PathBuf,
    /// On a HIT, the cached `packages.lock` to restore (the base's package set is
    /// stack-independent, so it is part of the base segment's cache entry).
    base_packages_lock: PathBuf,
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
const CACHE_VERSION: u32 = 3;

struct CacheCtx {
    /// The per-key entry directory: <cache-root>/<key>.
    dir: PathBuf,
    /// The full hex key (sha256 over the input manifest below).
    key: String,
}

/// Resolve the cache directory (Fable Part A). Precedence:
///   `--cache-dir <path>` (the `flag`)  >  `$DVMM_CACHE_DIR`  >  `$HOME/.dvmm`.
/// Returns `(dir, source)` where `source` is the provenance word for the log line.
/// Cache entries are disposable, so no migration between locations is needed.
fn resolve_cache_dir(flag: Option<&str>) -> (PathBuf, &'static str) {
    if let Some(f) = flag {
        if !f.is_empty() {
            return (PathBuf::from(f), "--cache-dir flag");
        }
    }
    if let Ok(d) = std::env::var("DVMM_CACHE_DIR") {
        if !d.is_empty() {
            return (PathBuf::from(d), "DVMM_CACHE_DIR env");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    (PathBuf::from(home).join(".dvmm"), "default $HOME/.dvmm")
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
    cache_dir: &Path,
    kernel: &Path,
    builders: &[String],
) -> Result<CacheCtx, String> {
    let repo_root = here
        .parent()
        .ok_or_else(|| "no repo root above guest/".to_string())?;
    let self_sha = sha256_file_hex(self_exe).map_err(|x| format!("hashing dvmm binary: {x}"))?;
    let kernel_sha = sha256_file_hex(kernel).map_err(|x| format!("hashing kernel: {x}"))?;
    // The agent is now a Rust crate built in a pinned musl container: its cache
    // input is the agent + proto crate trees + Cargo.lock (a toolchain/image bump
    // is captured via the `builders` digests below). Fable §2.
    let agent_tree =
        tree_hash(&repo_root.join("dvmm-agent"), &[]).map_err(|x| format!("hashing dvmm-agent: {x}"))?;
    let proto_tree =
        tree_hash(&repo_root.join("dvmm-proto"), &[]).map_err(|x| format!("hashing dvmm-proto: {x}"))?;
    let cargo_lock = sha256_file_hex(&repo_root.join("Cargo.lock"))
        .map_err(|x| format!("hashing Cargo.lock: {x}"))?;
    let overlay_tree =
        tree_hash(&alpine_dir.join("overlay"), &[]).map_err(|x| format!("hashing overlay: {x}"))?;
    let engine_sha = sha256_file_hex(&alpine_dir.join("compose-engine.lock"))
        .map_err(|x| format!("hashing compose-engine.lock: {x}"))?;
    // the stack dir: compose.yml + build contexts + bind sources + service source,
    // EXCLUDING this bake's own committed outputs.
    let stack_tree = tree_hash(compose_dir, &["compose.lock.yml", "stack.lock"])
        .map_err(|x| format!("hashing stack dir: {x}"))?;
    // Fable Part B/guardrail §3: the DECLARED builder-image digests replace the old
    // host-probed `podman --version` — NOTHING host-probed enters the key.
    let builders_line = builders.join(",");

    let manifest = format!(
        "dvmm-bake-cache v{CACHE_VERSION}\n\
         self:      {self_sha}\n\
         builders:  {builders_line}\n\
         engine:    {engine_sha}\n\
         kernel:    {kernel_sha}\n\
         agent:     {agent_tree}\n\
         proto:     {proto_tree}\n\
         cargolock: {cargo_lock}\n\
         overlay:   {overlay_tree}\n\
         stackdir:  {stack_tree}\n\
         name:      {stack_name}\n\
         mem:       {mem_mib}\n\
         ws:        {working_set_mib}\n\
         squash:    {squash_threshold_mib}\n"
    );
    let key = artifact::sha256_hex(manifest.as_bytes());
    let dir = cache_dir.join("bake").join(&key);
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

// ============================================================================
// shared base-runtime segment cache (Fable Part D)
//
// The base runtime (Alpine + podman/crun/conmon/netavark + the agent + the
// compose CLI) is common to EVERY stack. Its emitted cpio segment is cached here,
// keyed on DECLARED base pins only, so per-stack bakes concatenate a reused base
// segment + a fresh stack segment instead of rebuilding the base every time.
// ============================================================================

/// The base-runtime cache key: DECLARED base pins only (never the stack).
fn compute_base_key(
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
fn base_cache_store(entry: &Path, base_seg: &Path, packages_lock: &Path) -> std::io::Result<()> {
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
