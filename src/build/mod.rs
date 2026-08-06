//! `tdvmm build <name> <compose.yml>` (OP-1b) — the whole bake pipeline, folded into the
//! binary. It replaced the now-removed `guest/bake-stack.sh` (orchestrator),
//! `guest/bake_compose.py` (→ [`crate::compose`]), `guest/pack-tdvmm.sh` (→
//! [`crate::artifact`] via this module's `pack_tdvmm`), and `guest/initramfs-alpine/{build_rootfs.sh,
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
//!   * the `.tdvmm` is packed via the existing [`crate::artifact`] encoder.
//!
//! Two hidden helper subcommands run inside `podman unshare` (a user namespace)
//! where the assembled rootfs files are readable as uid 0: `__seed-build`
//! (assemble the seed store) and `__assemble-initramfs` (build the Alpine rootfs
//! and emit the cpio). `tdvmm build` re-execs itself into them.
//!
//! The OP-1b acceptance is a **byte-identical** `.tdvmm` versus the old scripts on
//! the corpus, so every command and file operation below mirrors them exactly.

mod agent;
mod base;
mod bake;
mod cache;
mod fsops;
mod images;
mod initramfs;
mod kernel;
mod overlay;
mod pack;
mod pins;
mod seed;
mod stack_lock;
mod util;
mod ux;

pub use agent::cmd_build_agent;
pub use bake::cmd_build;
pub use initramfs::cmd_assemble_initramfs;
pub use kernel::cmd_build_kernel;
pub use kernel::resolve_boot_inputs;
pub use seed::cmd_seed_build;
pub(crate) use util::civil_from_days;

// ---- pins (from the retired shell bake pipeline) ---------------------------

const BUILD_EPOCH: &str = "1785542400";
const BUSYBOX_REF: &str = "docker.io/library/busybox@sha256:dc2d74b28e4cf8984fa52af1f39bc7c3d9c73760b41a74d629f5d11b1ab28616";
/// Static sanity ceiling for `--mem`, in MiB (1 TiB). Matches the VMM's own
/// guest-memory cap ([`crate::memory`]): guest RAM now splits across the 32-bit
/// MMIO gap, so a bake asking for >3 GiB is fine and must NOT warn — only an
/// obviously bogus size (e.g. bytes passed as MiB) trips this. Nothing
/// host-probed feeds it.
const VMM_MAX_MEM_MIB: u64 = 1024 * 1024;
const DEFAULT_MEM_MIB: u64 = 3072;
const DEFAULT_WORKING_SET_MIB: u64 = 512;
const DEFAULT_SQUASH_THRESHOLD_MIB: u64 = 100;

/// The `tdvmm build` progress bar's step count (Fable CLI-UX ruling): resolve
/// inputs, bake cache, squash images, seed store, compose.lock + binds,
/// assemble initramfs, pack artifact, cache + diagnostics.
const TOTAL_STEPS: u32 = 8;

const ALPINE_BRANCH: &str = "v3.22";
const ALPINE_VER: &str = "3.22.5";
const MINIROOTFS: &str = "alpine-minirootfs-3.22.5-x86_64.tar.gz";
const MINIROOTFS_SHA256: &str = "4b4daa9fe2fc696c4919c4412a4c3d3e770d8fb70292a004a2c72f5096175282";
const DEFAULT_MIRROR: &str = "https://dl-cdn.alpinelinux.org/alpine";

/// Top-level pinned packages (transitive deps float within the branch); the sole
/// source of truth now that the shell rootfs builder is gone.
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

/// The baked run-defaults (mirror `pack-tdvmm.sh`). Fixed for the corpus (env can
/// override in the scripts; `tdvmm build` keeps the same defaults).
const DEFAULT_CMDLINE: &str = "console=ttyS0 reboot=t panic=1 pci=off no_timer_check tsc=reliable tdvmm.stack=1 tdvmm.interval=3600 tdvmm.maxrows=1000 tdvmm.hc_tick=2";

// ============================================================================
// CLI args
// ============================================================================

pub struct BuildArgs {
    pub compose: String,
    pub out: Option<String>,
    /// The required stack/artifact name (first positional). Validated at the CLI
    /// boundary by `parse_stack_name`.
    pub name: String,
    pub mem: Option<u64>,
    pub working_set: Option<u64>,
    pub squash_threshold: Option<u64>,
    pub validate_only: bool,
    /// Bypass the content-hash bake cache: force a full rebuild (still stores the
    /// result so later cached runs can hit). Nightly `bake_repeat` uses this.
    pub no_cache: bool,
    /// Cache directory override (Fable Part A). Precedence: this > `TDVMM_CACHE_DIR`
    /// > `$HOME/.tdvmm`. `None` falls through to env/default.
    pub cache_dir: Option<String>,
    /// Disable the progress spinner (Fable CLI-UX ruling): `--no-progress`, or
    /// implied by a non-terminal stderr / `CI` / `TERM=dumb` (decided in `ui`).
    pub no_progress: bool,
}

/// `tdvmm build-kernel` args.
pub struct BuildKernelArgs {
    pub out: Option<String>,
    pub cache_dir: Option<String>,
    pub force_build: bool,
    pub record: bool,
}

/// `tdvmm build-agent` args.
pub struct BuildAgentArgs {
    pub out: String,
    /// Record this build's identity (sha256 + build hash + release-asset URL for
    /// `--tag`) into `tdvmm-agent/agent.lock` — the agent mirror of
    /// `build-kernel --record`.
    pub record: bool,
    /// The release tag whose workflow published (or will publish) the agent
    /// asset; required with `--record`.
    pub tag: Option<String>,
}
