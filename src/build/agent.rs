//! the reproducible guest control-channel agent (Fable §2): a static musl binary,
//! acquired like the kernel — the pinned release asset first (sha-verified into
//! the cache), else built from source inside the pinned builder container, never
//! on the host, so rustc drift can't change the `.tdvmm` bytes.

use std::path::{Path, PathBuf};

use crate::artifact;
use crate::engine;
use crate::ui;
use super::fsops::tree_hash;
use super::pins::{agent_builder_pin, fetch_verify, lock_values};
use super::util::{find_repo_root, sha256_file_hex, ScratchDir};
use super::ux::{run, Ux};
use super::{BuildAgentArgs, BUILD_EPOCH};

/// The release asset's basename (what the agent-release workflow uploads).
const AGENT_ASSET_NAME: &str = "tdvmm-agent-x86_64-unknown-linux-musl";

/// The committed agent pin (`tdvmm-agent/agent.lock`), embedded so `tdvmm build`
/// can fetch + verify the pinned agent with no checkout present. Empty pin
/// fields mean "no release recorded yet" — the from-source fallback applies.
/// (The builder-image pin lives separately in images.lock — see `pins`.)
const AGENT_LOCK: &str = include_str!("../../tdvmm-agent/agent.lock");

/// The agent pin ledger (mirrors [`super::kernel::KernelLock`]). All fields stay
/// empty until the first agent release is recorded via `build-agent --record`.
struct AgentLock {
    version: String,
    sha256: String,
    build_hash: String,
    release_asset_url: String,
    release_asset_name: String,
}

fn embedded_agent_lock() -> AgentLock {
    parse_agent_lock(AGENT_LOCK)
}

fn parse_agent_lock(text: &str) -> AgentLock {
    let [version, sha256, build_hash, release_asset_url, release_asset_name] = lock_values(text, [
        "AGENT_VERSION",
        "AGENT_SHA256",
        "AGENT_BUILD_HASH",
        "RELEASE_ASSET_URL",
        "RELEASE_ASSET_NAME",
    ]);
    AgentLock { version, sha256, build_hash, release_asset_url, release_asset_name }
}

fn write_agent_lock(repo_root: &Path, a: &AgentLock) -> Result<(), Box<dyn std::error::Error>> {
    let body = format!(
        "# tdvmm guest agent pin (mirrors guest/kernel/kernel.lock).\n\
         #\n\
         # The guest tdvmm-agent (a static, reproducible x86_64-unknown-linux-musl binary)\n\
         # is acquired EITHER by fetching the pinned GitHub release asset (PRIMARY,\n\
         # verified against AGENT_SHA256) OR by the reproducible from-source build inside\n\
         # the pinned builder container (FALLBACK — needs a source checkout; the builder\n\
         # image pin lives in images.lock beside this file). Both paths yield the\n\
         # byte-identical agent recorded here; the fallback is sha-verified against\n\
         # AGENT_SHA256 whenever one is recorded. This file is embedded into the tdvmm\n\
         # binary at compile time.\n\
         #\n\
         # This file is EXCLUDED from the agent source-identity hash (agent_src_id), so\n\
         # recording a pin here never changes the agent's own bytes. NOTE: the root\n\
         # Cargo.toml [profile.agent-release] section is NOT part of that hash either —\n\
         # editing the profile changes the agent bytes without changing the build hash,\n\
         # so re-record after any profile change.\n\
         #\n\
         # Release flow (record-BEFORE-tag, so nothing lags):\n\
         #   1. tdvmm build-agent --record --tag agent-<version>   # writes this file\n\
         #   2. commit this file, then tag that commit `agent-<version>` and push the tag\n\
         #   3. .github/workflows/agent-release.yml rebuilds the agent (double-build),\n\
         #      verifies it matches AGENT_SHA256 below, and publishes the asset\n\
         #   4. rebuild/re-release tdvmm so the recorded pin is embedded\n\
         AGENT_VERSION={}\n\
         AGENT_SHA256={}\n\
         AGENT_BUILD_HASH={}\n\
         RELEASE_ASSET_URL={}\n\
         RELEASE_ASSET_NAME={}\n",
        a.version, a.sha256, a.build_hash, a.release_asset_url, a.release_asset_name,
    );
    std::fs::write(repo_root.join("tdvmm-agent/agent.lock"), body)?;
    Ok(())
}

/// A deterministic identity of the agent's SOURCES — the `tdvmm-agent` +
/// `tdvmm-proto` crate trees + `Cargo.lock`. Embedded as the agent's build hash
/// (the compatibility oracle reported by `ping`/hello) and folded into the bake
/// cache key. First 16 hex of the sha256.
///
/// `agent.lock` is EXCLUDED and must stay excluded: this hash is compiled into
/// the agent binary (`TDVMM_AGENT_BUILD`), so if recording a pin into agent.lock
/// perturbed it, the recorded sha could never match a rebuild of the same tree
/// (self-reference) and the `.tdvmm` bytes would drift. `images.lock` is
/// deliberately INCLUDED — a builder bump is a real toolchain change.
fn agent_src_id(repo_root: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let a = tree_hash(&repo_root.join("tdvmm-agent"), &["agent.lock"])?;
    let p = tree_hash(&repo_root.join("tdvmm-proto"), &[])?;
    let lock = sha256_file_hex(&repo_root.join("Cargo.lock"))?;
    Ok(artifact::sha256_hex(format!("{a}\n{p}\n{lock}\n").as_bytes())[..16].to_string())
}

/// The agent identity that feeds the bake cache key: the recorded release sha
/// when agent.lock has one, else the from-source identity (which needs a
/// checkout, exactly like the source-build fallback it keys).
pub(super) fn agent_cache_input() -> Result<String, Box<dyn std::error::Error>> {
    let lock = embedded_agent_lock();
    if !lock.sha256.is_empty() {
        return Ok(lock.sha256);
    }
    let repo_root = find_repo_root()
        .ok_or("no recorded agent release (agent.lock) and no source checkout to hash")?;
    agent_src_id(&repo_root)
}

/// Ensure the guest agent binary is available, mirroring `ensure_kernel`:
/// (a) the sha-verified copy already in `<cache>/agent/`; (b) the pinned release
/// asset, fetched + sha-verified into the cache; (c) the reproducible from-source
/// container build into `source_out` — the only path until the first agent
/// release is recorded, and the only one needing a checkout. Whenever agent.lock
/// records a sha, EVERY path is verified against it — a from-source build that
/// stops matching the pin is an error, never silent drift. Returns the binary
/// path + its embedded build hash.
pub(super) fn ensure_agent(
    cache_dir: &Path,
    source_out: &Path,
    ux: &Ux,
) -> Result<(PathBuf, String), Box<dyn std::error::Error>> {
    let lock = embedded_agent_lock();
    if !lock.sha256.is_empty() {
        let cached = cache_dir.join("agent").join(format!("tdvmm-agent-{}", lock.version));
        if cached.exists() {
            if let Ok(got) = sha256_file_hex(&cached) {
                if got == lock.sha256 {
                    ux.progress.detail(format!("   agent: {} (present, sha256 verified)", cached.display()));
                    return Ok((cached, lock.build_hash));
                }
            }
            let _ = std::fs::remove_file(&cached);
        }
        if !lock.release_asset_url.is_empty() {
            ux.progress.detail(format!("   agent: fetching pinned release asset {} ...", lock.release_asset_url));
            match fetch_verify(&cached, &lock.release_asset_url, &lock.sha256, ux) {
                Ok(()) => {
                    ux.progress.detail("   agent: fetched + sha256 verified from release");
                    return Ok((cached, lock.build_hash));
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&cached);
                    ux.progress.println(format!(
                        "{}: agent release fetch failed ({e}); falling back to from-source build",
                        crate::compose::WARN
                    ));
                }
            }
        }
    }

    // FALLBACK: reproducible from-source container build (needs a checkout).
    let repo_root = find_repo_root().ok_or(
        "tdvmm-agent unavailable: agent.lock records no published release asset yet \
         and no source checkout is present for the from-source build. Until the first \
         agent release is published + recorded (see tdvmm-agent/agent.lock), \
         `tdvmm build` must run from a checkout",
    )?;
    let build_hash = build_agent(&repo_root, source_out, ux)?;
    if !lock.sha256.is_empty() {
        let got = sha256_file_hex(source_out)?;
        if got != lock.sha256 {
            return Err(format!(
                "from-source tdvmm-agent sha256 {got} != agent.lock {}; the build is not \
                 reproducing the recorded agent (source drift since the pinned release? \
                 re-run `tdvmm build-agent --record` + cut a new agent release)",
                lock.sha256
            )
            .into());
        }
        ux.progress.detail("   agent: from-source build sha256 verified against agent.lock");
    }
    Ok((source_out.to_path_buf(), build_hash))
}

/// Build the guest `tdvmm-agent` as a static, reproducible `x86_64-unknown-linux-
/// musl` binary INSIDE the pinned builder container (Fable §2 — never on the host,
/// so rustc drift can't change the `.tdvmm` bytes). Determinism knobs: the
/// `agent-release` profile (opt-level=z, lto, codegen-units=1, panic=abort,
/// strip=symbols); `SOURCE_DATE_EPOCH`; `--remap-path-prefix` for both the source
/// mount and CARGO_HOME; `--build-id=none`; and `rust-lld` + self-contained
/// linking so no external C toolchain is pulled. Returns the embedded build hash.
fn build_agent(repo_root: &Path, out: &Path, ux: &Ux) -> Result<String, Box<dyn std::error::Error>> {
    let (image, digest) = agent_builder_pin()?;
    let img_ref = format!("{image}@{digest}");
    let build_hash = agent_src_id(repo_root)?;
    ux.progress.detail(format!("building tdvmm-agent (static musl, reproducible) in {img_ref}"));

    let confdir = ScratchDir::new()?;
    let conf = confdir.path().join("containers.conf");
    std::fs::write(&conf, "[engine]\nruntime=\"runc\"\n")?;
    let work_guard = ScratchDir::new()?;
    let work = work_guard.path();

    // rust-lld consumes link args directly (no `cc` driver), so `--build-id=none`
    // is passed as-is. The remaps stabilize the two absolute paths that would
    // otherwise leak into the bytes: the /src source mount and CARGO_HOME.
    let rustflags = "-C linker=rust-lld -C link-self-contained=yes -C link-arg=--build-id=none \
                     --remap-path-prefix=/src=/tdvmm --remap-path-prefix=/work/cargo=/cargo";
    let script = "set -e; cd /src && \
        cargo build -p tdvmm-agent --profile agent-release \
            --target x86_64-unknown-linux-musl --locked && \
        cp /work/target/x86_64-unknown-linux-musl/agent-release/tdvmm-agent /work/tdvmm-agent";

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
        .arg(format!("TDVMM_AGENT_BUILD={build_hash}"))
        .arg(&img_ref)
        .args(["sh", "-c", script]), ux.mode)?;

    std::fs::copy(work.join("tdvmm-agent"), out)
        .map_err(|e| format!("agent build produced no binary: {e}"))?;
    Ok(build_hash)
}

/// `tdvmm build-agent -o <path>`: build the reproducible musl agent standalone
/// (the size + double-build byte-identity gate scripts use this), or `--record`
/// to pin this build into agent.lock (mirrors `build-kernel --record`; step 1 of
/// the record-before-tag release flow documented there). Prints
/// `<sha256>  <path>` to stdout. Shares `build_agent()` with `tdvmm build`'s
/// pipeline, but ALWAYS with progress UI disabled / output inherited (scope
/// lock — progress is `build`-only).
pub fn cmd_build_agent(args: BuildAgentArgs) -> Result<i32, Box<dyn std::error::Error>> {
    let repo_root = find_repo_root()
        .ok_or("`build-agent` builds from source and needs a checkout (run from the repo)")?;
    let outp = PathBuf::from(&args.out);
    if let Some(parent) = outp.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let progress = ui::Progress::disabled();
    let ux = Ux::inherit(&progress);
    let build_hash = build_agent(&repo_root, &outp, &ux)?;
    let sha = sha256_file_hex(&outp)?;
    let size = std::fs::metadata(&outp)?.len();
    eprintln!("   tdvmm-agent: {size} bytes  build={build_hash}  sha256={sha}");

    if args.record {
        // Record-before-tag: pin THIS build, commit, then tag `agent-<version>`.
        // The tag's workflow rebuilds from the tagged tree and refuses to publish
        // unless its bytes match this recorded sha (agent.lock is excluded from
        // the source hash, so recording cannot perturb the build it records).
        let tag = args
            .tag
            .as_deref()
            .ok_or("`build-agent --record` needs --tag <tag> (the `agent-<version>` tag you will push)")?;
        let lock = AgentLock {
            version: tag.to_string(),
            sha256: sha.clone(),
            build_hash,
            release_asset_name: AGENT_ASSET_NAME.to_string(),
            release_asset_url: format!(
                "https://github.com/clarkmcc/tdvmm/releases/download/{tag}/{AGENT_ASSET_NAME}"
            ),
        };
        write_agent_lock(&repo_root, &lock)?;
        eprintln!("== agent.lock RECORDED ==");
        eprintln!("   AGENT_VERSION={}", lock.version);
        eprintln!("   AGENT_SHA256={}", lock.sha256);
        eprintln!("   AGENT_BUILD_HASH={}", lock.build_hash);
        eprintln!("   RELEASE_ASSET_URL={}", lock.release_asset_url);
        eprintln!("   commit agent.lock, tag that commit `{tag}`, push the tag; then rebuild tdvmm");
    }

    println!("{sha}  {}", outp.display());
    Ok(0)
}
