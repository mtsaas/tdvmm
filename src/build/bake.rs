//! `tdvmm build` orchestrator — resolves inputs, drives the bake cache, images,
//! seed store, compose.lock, base-runtime + initramfs assembly, stack.lock, and
//! the `.tdvmm` pack, then populates the cache + writes side diagnostics. The
//! byte-identity acceptance lives here: every step mirrors the retired scripts.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::compose;
use crate::engine;
use crate::ui;
use super::agent::build_agent;
use super::base::{base_cache_store, build_base_rootfs, compute_base_key};
use super::cache::{cache_is_hit, cache_restore, cache_store, compute_cache_key, resolve_cache_dir};
use super::fsops::copy_tree;
use super::images::{bake_one, build_one, squash_base_name, ImgRecord};
use super::initramfs::AssembleConfig;
use super::kernel::ensure_kernel;
use super::pack::{pack_tdvmm, PackInputs};
use super::pins::{collect_builder_pins, fetch_verify, read_compose_lock, read_rootfs_builder_pin};
use super::seed::{SeedConfig, SeedSquash};
use super::stack_lock::{append_stack_lock_tdvmm, write_stack_lock};
use super::util::{self_here, sha256_file_hex, sweep_stale_scratch, utc_now_iso, ScratchDir};
use super::ux::{capture, run, Ux};
use super::{
    BuildArgs, ALPINE_BRANCH, BUILD_EPOCH, BUSYBOX_REF, DEFAULT_MEM_MIB, DEFAULT_MIRROR,
    DEFAULT_SQUASH_THRESHOLD_MIB, DEFAULT_WORKING_SET_MIB, MINIROOTFS, MINIROOTFS_SHA256, PKGS,
    TOTAL_STEPS, VMM_MAX_MEM_MIB,
};

fn die_reject(msg: &str) -> ! {
    eprintln!("{}: {msg}", compose::REJECT);
    eprintln!("bake: REJECTED at validation (see {} above)", compose::REJECT);
    std::process::exit(3);
}

pub fn cmd_build(args: BuildArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let compose_path = std::fs::canonicalize(&args.compose)
        .map_err(|_| format!("compose file not found: {}", args.compose))?;
    let compose_dir = compose_path.parent().unwrap().to_path_buf();
    // The name is the required first positional (validated at the CLI boundary by
    // `parse_stack_name`, so it is a safe single path component). No folder-derived
    // default: `tdvmm build <name> <compose>` states the store key outright.
    let stack_name = args.name.clone();
    let project = format!("tdvmm_{stack_name}");

    // --- parse + validate (the loud static gate) ---
    let doc_str = std::fs::read_to_string(&compose_path)?;
    let doc: serde_yaml::Value = serde_yaml::from_str(&doc_str)
        .map_err(|e| {
            eprintln!("{}: could not parse {}: {e}", "TDVMM_BAKE_ERROR", compose_path.display());
            std::process::exit(2);
        })
        .unwrap();
    let validated = match compose::validate(&doc, &compose_path) {
        Ok(v) => v,
        // Reject is the loud out-of-subset gate (exit 3); Io/Internal are exit 2.
        Err(compose::ValidateError::Reject(msg)) => die_reject(&msg),
        Err(e) => {
            eprintln!("TDVMM_BAKE_ERROR: {e}");
            std::process::exit(2);
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
    ux.progress.detail(format!("== tdvmm build: stack={stack_name} project={project} mem={mem_mib}MiB =="));
    ux.progress.detail(format!("   compose: {}", compose_path.display()));
    diag.push_str(&format!("compose_path: {}\n", compose_path.display()));

    // --- cache dir (Fable Part A): --cache-dir > $TDVMM_CACHE_DIR > $HOME/.tdvmm ---
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
    //
    // DEFAULT outputs land under the resolved cache root's `artifacts/` dir (NOT
    // the source tree): an installed `tdvmm` must not write build outputs into the
    // repo. `-o <path>` still fully overrides the `.tdvmm` destination. The
    // intermediate cpio is build debris, not a deliverable, so it lands under
    // `bake/` (beside the content-hash cache) — keeping `artifacts/` all `.tdvmm`.
    // Its basename is unchanged and stack.lock records only the basename, so the
    // `.tdvmm` bytes stay byte-identical.
    let artifacts_dir = cache_dir.join("artifacts");
    std::fs::create_dir_all(&artifacts_dir)?;
    let bake_out_dir = cache_dir.join("bake");
    std::fs::create_dir_all(&bake_out_dir)?;
    let out_tdvmm = match &args.out {
        Some(o) => PathBuf::from(o),
        None => artifacts_dir.join(format!("{stack_name}.tdvmm")),
    };
    let out_initramfs = bake_out_dir.join(format!("initramfs-alpine-{stack_name}.cpio.gz"));
    let committed_lock = here.join("stacks").join(&stack_name).join("compose.lock.yml");
    let stack_lock_path = here.join("stacks").join(&stack_name).join("stack.lock");

    // ---- content-hash bake cache -------------------------------------------
    // The key covers EVERY input that affects the output bytes: the whole compose
    // dir tree (compose.yml + build contexts + bind sources + service source,
    // excluding this bake's own committed outputs), the kernel, the agent source,
    // the guest overlay tree, the pinned compose engine, the DECLARED builder-image
    // digests (Fable Part B — replacing the host-probed podman version, which is
    // gone), the tdvmm binary itself (all compiled-in pins + bake logic), and the
    // sizing knobs. `tdvmm build` is deterministic (artifact_test gate 1), so a hit
    // reusing the prior `.tdvmm` is byte-identical to a fresh bake.
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
            match cache_restore(c, &out_tdvmm, &out_initramfs, &committed_lock, &stack_lock_path) {
                Ok(tdvmm_sha) => {
                    ux.progress.detail(format!("   .tdvmm:     {} (sha256 {tdvmm_sha})", out_tdvmm.display()));
                    let size = std::fs::metadata(&out_tdvmm).map(|m| m.len()).unwrap_or(0);
                    ux.progress.print_summary(&out_tdvmm, &tdvmm_sha, size, progress.elapsed(), None);
                    progress.finish();
                    // stdout: the artifact identity line (parity with the old pack-tdvmm.sh) —
                    // `suspend` just runs the closure directly once the bar is finished; kept
                    // for symmetry with the other stdout site below.
                    ux.progress.suspend(|| println!("{tdvmm_sha}  {}", out_tdvmm.display()));
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

    // Reap scratch dirs orphaned by earlier builds that were SIGKILLed before
    // their ScratchDir guard could run (best-effort).
    sweep_stale_scratch();

    // --- scratch workdir + clean CONTAINERS_CONF ---
    let work_guard = ScratchDir::new()?;
    let work = work_guard.path().to_path_buf();
    let conf = work.join("containers.conf");
    std::fs::write(&conf, "[engine]\n")?;

    // Host-probed engine version — Fable guardrail §3: it must NOT enter the hashed
    // artifact bytes OR the cache key (it breaks cross-host byte-identity). It is
    // captured for DEBUGGING ONLY and written to a side diagnostics file under the
    // (disposable) cache dir — never into the .tdvmm, the manifest, or stack.lock.
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
    let binds_base = "/var/lib/tdvmm-stack/binds";
    let lock = compose::emit_lock(compose::EmitLockRequest {
        doc: &doc,
        compose_path: &compose_path,
        digests: &digests,
        binds_base,
        project: &project,
    })?;
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
        ux.progress.println(format!("{}: {mem_mib} MiB exceeds the {VMM_MAX_MEM_MIB} MiB (1 TiB) sanity cap — did you pass bytes instead of MiB?", compose::WARN));
    }

    // --- 6. assemble the per-stack initramfs ---
    ux.progress.step(6, TOTAL_STEPS, "assemble initramfs");
    ux.progress.detail("== assemble initramfs (Rust rootfs + cpio) ==");
    // (out_initramfs computed early, above, for the cache path)

    // build the tdvmm-agent (static musl, reproducible) in the pinned builder
    // container, before the unshare. Returns the embedded build hash (the compat
    // oracle reported by ping/hello); its file sha256 goes in the ledger + anchors.
    let agent_bin = work.join("tdvmm-agent");
    let agent_build_hash = build_agent(&here, &agent_bin, &ux)?;
    let agent_sha = sha256_file_hex(&agent_bin)?;
    // TTY: deliberately NOT shown on the step line — it's diagnostic (relocated
    // to `diag` below), not routine build progress.
    ux.progress.detail(format!("   tdvmm-agent: sha256 {agent_sha}  build {agent_build_hash}"));
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

    // --- 8. pack the single-file .tdvmm artifact ---
    ux.progress.detail("== pack .tdvmm artifact ==");
    let sealed = pack_tdvmm(PackInputs {
        self_exe: &self_exe,
        records: &records,
        compose_version: &compose_version,
        compose_sha256: &compose_sha256,
        stack: &stack_name,
        project: &project,
        mem_mib,
        est_mib,
        builders: &builders,
        agent_sha: &agent_sha,
        agent_build_hash: &agent_build_hash,
        kernel_path: &kernel,
        initramfs_path: &out_initramfs,
        lock_path: &lock_path,
    })?;
    let out_file = std::fs::File::create(&out_tdvmm)
        .map_err(|e| format!("creating {}: {e}", out_tdvmm.display()))?;
    let written = sealed.write_to(std::io::BufWriter::new(out_file))?;
    let tdvmm_sha = written.sha256_hex;
    let tdvmm_len = written.len;
    // append the artifact identity to the ledger.
    append_stack_lock_tdvmm(&here, &stack_name, &tdvmm_sha, &out_tdvmm)?;
    diag.push_str(&format!(
        "initramfs: {} sha256={art_sha}\ntdvmm: {} sha256={tdvmm_sha}\n",
        out_initramfs.display(), out_tdvmm.display(),
    ));

    ux.progress.detail("");
    ux.progress.detail("== tdvmm build DONE ==");
    ux.progress.detail(format!("   initramfs: {}", out_initramfs.display()));
    ux.progress.detail(format!("   sha256:    {art_sha}"));
    ux.progress.detail(format!("   .tdvmm:     {} (sha256 {tdvmm_sha})", out_tdvmm.display()));
    // stdout: the artifact identity line (parity with the old pack-tdvmm.sh) —
    // UNCHANGED by the TTY redesign (progress/chrome is stderr-only). Routed
    // through `suspend` so a still-ticking step-7 spinner can't interleave
    // with this raw stdout write (the bar is cleared for the print, then
    // redrawn) — frozen/non-TTY: `suspend` just calls the closure directly.
    ux.progress.suspend(|| println!("{tdvmm_sha}  {}", out_tdvmm.display()));

    // --- 9. populate the bake cache (best-effort; never fails the build) ---
    ux.progress.step(8, TOTAL_STEPS, "cache");
    if let Some(c) = &cache {
        match cache_store(c, &out_tdvmm, &out_initramfs, &committed_lock, &stack_lock_path, &tdvmm_sha) {
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
    ux.progress.print_summary(&out_tdvmm, &tdvmm_sha, tdvmm_len, progress.elapsed(), diag_path.as_deref());

    Ok(0)
}

/// Write host-probed + relocated bake diagnostics to a side file under the
/// (disposable) cache dir. NOTHING here enters the `.tdvmm` bytes or the bake
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
        "# tdvmm bake diagnostics (host-probed + relocated detail; NOT in the artifact\n\
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
