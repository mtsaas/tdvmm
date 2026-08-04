//! the reproducible guest control-channel agent (Fable §2): a static musl binary
//! built inside the pinned builder container, never on the host, so rustc drift
//! can't change the `.dvmm` bytes.

use std::path::{Path, PathBuf};

use crate::artifact;
use crate::engine;
use crate::ui;
use super::fsops::tree_hash;
use super::pins::read_builder_pin;
use super::util::{mkdtemp, self_here, sha256_file_hex};
use super::ux::{run, Ux};
use super::BUILD_EPOCH;

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
pub(super) fn build_agent(here: &Path, out: &Path, ux: &Ux) -> Result<String, Box<dyn std::error::Error>> {
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
