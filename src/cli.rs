//! CLI surface: the clap arg structs for every `dvmm` subcommand, plus
//! `EffectiveConfig` — the resolved run configuration (baked < scenario < flag)
//! every boot path hands to the vCPU loop.

use clap::{Args, Parser, Subcommand};

use crate::{artifact, scenario};
use crate::{DEFAULT_CMDLINE, DEFAULT_MAX_JUMP_SECS, DEFAULT_MEM_MIB};

#[derive(Parser)]
#[command(
    name = "dvmm",
    about = "deterministic KVM VMM — run/inspect/verify a .dvmm stack, or boot raw artifacts",
    long_about = "A single-vCPU, fast-forwardable KVM VMM. `run` boots a self-contained \
                  .dvmm stack artifact (baked defaults, overridable by flags); `boot` is \
                  the low-level raw kernel+initramfs verb for VMM development.\n\n\
                  Durations (--max-virtual-time): a bare number is seconds, or use a \
                  suffix (ms, s, m, h), e.g. 500ms, 30s, 5m, 2h.",
    after_help = "Examples:\n  \
                  dvmm build guest/stacks/dogfood/compose.yml\n  \
                  dvmm run guest/stacks/dogfood/dogfood.dvmm\n  \
                  dvmm test dogfood.dvmm --scenario guest/stacks/dogfood/dogfood.yml",
    version
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
    /// Disable the `dvmm build` progress spinner; emit plain `== step ==`
    /// lines (also forced by a non-terminal stderr, `CI`, or `TERM=dumb`). No
    /// effect on any other subcommand.
    #[arg(long, global = true)]
    pub(crate) no_progress: bool,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Bake a compose stack into a self-contained .dvmm (host tool: podman + network).
    #[command(after_help = "Example:\n  dvmm build guest/stacks/dogfood/compose.yml -o dogfood.dvmm")]
    Build(BuildCliArgs),
    /// Build the reproducible static-musl dvmm-agent standalone (pinned builder
    /// container). Prints `<sha256>  <path>`. Used by the size / double-build gates.
    #[command(name = "build-agent")]
    BuildAgent(BuildAgentArgs),
    /// Acquire the pinned guest kernel: fetch the pinned GitHub release asset
    /// (PRIMARY, sha256-verified against kernel.lock) or reproducibly build it in
    /// the pinned builder container (FALLBACK). `--record` bootstraps kernel.lock.
    #[command(name = "build-kernel")]
    BuildKernel(BuildKernelArgs),
    /// Boot a raw kernel + initramfs (the low-level VMM-dev / smoke verb).
    Boot(BootArgs),
    /// Run a .dvmm stack artifact: apply its baked run-defaults, then boot (offline).
    #[command(after_help = "Example:\n  dvmm run dogfood.dvmm --ff on")]
    Run(RunArgs),
    /// Test a .dvmm stack against a scenario: drive virtual time, assert, verdict.
    #[command(
        long_about = "Test a .dvmm stack against a scenario: drive virtual time, assert, \
                      verdict.\n\nExit codes (the shared dvmm 0-3 contract; `test` itself only \
                      ever produces 0/1/2 — 3 is `build`/`boot`/`run`'s REJECTED/horizon code):\n  \
                      0  PASS — every assertion held\n  \
                      1  FAIL — a scenario assertion failed\n  \
                      2  ERROR — test infrastructure fault: bad/rejected scenario (including \
                      static validation before boot), agent unreachable, wall-clock timeout, ...",
        after_help = "Example:\n  dvmm test dogfood.dvmm --scenario guest/stacks/dogfood/dogfood.yml"
    )]
    Test(TestArgs),
    /// Print a .dvmm artifact's manifest.json (reads ONLY the manifest member).
    Inspect(ArtifactArg),
    /// Verify a .dvmm: recompute member hashes vs the manifest; print its sha256 identity.
    Verify(ArtifactArg),
    /// Print the effective guest clock/timer CPUID profile (the manifest artifact).
    DumpCpuid,
    /// [internal] Build the seed store inside `podman unshare` (used by `dvmm build`).
    #[command(name = "__seed-build", hide = true)]
    SeedBuild {
        #[arg(long, value_name = "PATH")]
        config: String,
    },
    /// [internal] Assemble the rootfs + emit the cpio inside `podman unshare`.
    #[command(name = "__assemble-initramfs", hide = true)]
    AssembleInitramfs {
        #[arg(long, value_name = "PATH")]
        config: String,
    },
}

/// `dvmm build` args (clap). Mirrors bake-stack.sh's flags.
#[derive(Args)]
pub(crate) struct BuildCliArgs {
    /// Path to the compose.yml to bake.
    #[arg(value_name = "compose.yml")]
    pub(crate) compose: String,
    /// Output .dvmm path (default guest/initramfs-alpine/<stack>.dvmm).
    #[arg(short, long, value_name = "PATH")]
    pub(crate) out: Option<String>,
    /// Stack name (default: the compose file's parent directory name).
    #[arg(long, value_name = "STR")]
    pub(crate) name: Option<String>,
    /// Guest RAM in MiB (default 3072).
    #[arg(long, value_name = "MiB")]
    pub(crate) mem: Option<u64>,
    /// Workload working-set allowance for the RAM estimate (MiB, default 512).
    #[arg(long, value_name = "MiB")]
    pub(crate) working_set: Option<u64>,
    /// Squash images larger than this many MiB to one vfs layer (default 100).
    #[arg(long, value_name = "MiB")]
    pub(crate) squash_threshold: Option<u64>,
    /// Only run the static compose validation (no pulls/boot); print + exit.
    #[arg(long)]
    pub(crate) validate_only: bool,
    /// Bypass the content-hash bake cache: force a full rebuild. The cache is keyed
    /// on ALL bake inputs, so an unchanged stack normally HITS (near-instant, skips
    /// pull/squash/assemble). Nightly bake-repeatability uses this to re-bake.
    #[arg(long)]
    pub(crate) no_cache: bool,
    /// Cache directory (holds the bake cache, the shared base-runtime segment, and
    /// the fetched/built kernel). Precedence: this flag > $DVMM_CACHE_DIR >
    /// $HOME/.dvmm. The resolved dir is logged at build start.
    #[arg(long, value_name = "PATH")]
    pub(crate) cache_dir: Option<String>,
}

/// `dvmm build-agent` args.
#[derive(Args)]
pub(crate) struct BuildAgentArgs {
    /// Output path for the built static-musl agent binary.
    #[arg(short, long, value_name = "PATH", default_value = "dvmm-agent.bin")]
    pub(crate) out: String,
}

/// `dvmm build-kernel` args.
#[derive(Args)]
pub(crate) struct BuildKernelArgs {
    /// Output path for the vmlinux (default: guest/kernel/vmlinux-<version>).
    #[arg(short, long, value_name = "PATH")]
    pub(crate) out: Option<String>,
    /// Cache directory (kernel source tarball + built kernel land here).
    /// Precedence: this flag > $DVMM_CACHE_DIR > $HOME/.dvmm.
    #[arg(long, value_name = "PATH")]
    pub(crate) cache_dir: Option<String>,
    /// Force the reproducible container build even if a release asset is available
    /// (used by the two-build byte-identity gate).
    #[arg(long)]
    pub(crate) force_build: bool,
    /// Bootstrap/update kernel.lock: run the container build, then WRITE the
    /// resolved kernel + source + builder digests into kernel.lock (no verify).
    #[arg(long)]
    pub(crate) record: bool,
}

/// Flags shared by `boot` and `run`. On `boot` the `Option`s fall back to the
/// binary defaults; on `run` a `None` means "use the artifact's baked default"
/// and `Some` means the flag overrides it (baked < flag, Fable-locked).
#[derive(Args, Clone)]
#[command(next_help_heading = "run options")]
pub(crate) struct CommonRunFlags {
    /// Guest RAM in MiB.
    #[arg(long, value_name = "MiB")]
    mem: Option<u64>,
    /// Kernel command line.
    #[arg(long, value_name = "STR")]
    cmdline: Option<String>,
    /// Fast-forward idle time: on|off.
    #[arg(long, value_parser = parse_onoff, value_name = "on|off")]
    ff: Option<bool>,
    /// Single-jump sanity bound (seconds); a larger jump aborts the run.
    #[arg(long, value_name = "SECS")]
    max_jump_secs: Option<f64>,
    /// Virtual-time horizon (duration); stop with exit 3 when reached.
    #[arg(long, value_name = "DUR")]
    max_virtual_time: Option<String>,
    /// Write the per-run fast-forward metrics block to this path at stop.
    #[arg(long, value_name = "PATH")]
    metrics_out: Option<String>,
}

#[derive(Args)]
pub(crate) struct BootArgs {
    /// Path to the uncompressed ELF vmlinux.
    #[arg(long, value_name = "PATH")]
    pub(crate) kernel: String,
    /// Path to the initramfs.
    #[arg(long, value_name = "PATH")]
    pub(crate) initrd: String,
    #[command(flatten)]
    pub(crate) common: CommonRunFlags,
}

#[derive(Args)]
pub(crate) struct RunArgs {
    /// Path to the .dvmm stack artifact.
    #[arg(value_name = "stack.dvmm")]
    pub(crate) artifact: String,
    /// Skip the default-ON member-hash verification on load.
    #[arg(long)]
    pub(crate) no_verify: bool,
    /// Pull each service's container log into `<dir>/<service>.log` at end-of-run
    /// (graceful stop paths only). Off by default; opt-in developer output.
    #[arg(long, value_name = "DIR")]
    pub(crate) logs_dir: Option<String>,
    #[command(flatten)]
    pub(crate) common: CommonRunFlags,
}

#[derive(Args)]
pub(crate) struct TestArgs {
    /// Path to the .dvmm stack artifact.
    #[arg(value_name = "stack.dvmm")]
    pub(crate) artifact: String,
    /// The scenario YAML (steps + assertions).
    #[arg(long, value_name = "PATH")]
    pub(crate) scenario: String,
    /// Skip the default-ON member-hash verification on load.
    #[arg(long)]
    pub(crate) no_verify: bool,
    /// JSONL run-log path (default `<artifact>.jsonl`).
    #[arg(long, value_name = "PATH")]
    pub(crate) jsonl: Option<String>,
    /// JSON report path (default `<artifact>.report.json`).
    #[arg(long, value_name = "PATH")]
    pub(crate) report: Option<String>,
    /// Wall-clock safety timeout (seconds); a run exceeding it fails with exit 2.
    #[arg(long, value_name = "SECS", default_value_t = 600)]
    pub(crate) wall_timeout: u64,
    /// Pull each service's container log into `<dir>/<service>.log` at scenario
    /// finalize (after the verdict, before the VM stops). Off by default; a
    /// sibling output that never affects the verdict, JSONL, or report.
    #[arg(long, value_name = "DIR")]
    pub(crate) logs_dir: Option<String>,
    #[command(flatten)]
    pub(crate) common: CommonRunFlags,
}

#[derive(Args)]
pub(crate) struct ArtifactArg {
    /// Path to the .dvmm stack artifact.
    #[arg(value_name = "stack.dvmm")]
    pub(crate) artifact: String,
}

/// clap value parser for `--ff on|off` (also accepts 1/0/true/false).
fn parse_onoff(s: &str) -> Result<bool, String> {
    match s {
        "on" | "1" | "true" => Ok(true),
        "off" | "0" | "false" => Ok(false),
        _ => Err(format!("expected on|off (got {s:?})")),
    }
}

/// The resolved run configuration + a per-knob provenance string for the
/// EFFECTIVE-CONFIG line (the future record-log preamble). Provenance is
/// `baked` (from the artifact), `flag` (a CLI override), or `default` (binary
/// default). Override precedence is LOCKED: baked < flag.
pub(crate) struct EffectiveConfig {
    pub(crate) mem_mib: u64,
    pub(crate) cmdline: String,
    pub(crate) fast_forward: bool,
    /// Whether `--ff` was explicitly passed — feeds ONLY the FF mode statement's
    /// "how chosen" wording, never the FF decision.
    pub(crate) ff_explicit: bool,
    pub(crate) max_jump_secs: f64,
    pub(crate) max_virtual_time_secs: Option<f64>,
    pub(crate) metrics_out: Option<String>,
    /// The formatted per-knob provenance, e.g.
    /// `mem=3072 (baked) ff=off (flag) horizon=36h (baked) ...`.
    pub(crate) provenance: String,
}

impl EffectiveConfig {
    /// Resolve for `dvmm boot`: no baked defaults; each knob is a flag override of
    /// the binary default.
    pub(crate) fn from_boot(f: &CommonRunFlags) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, None, None)
    }

    /// Resolve for `dvmm run`: the artifact's baked run-defaults, each overridable
    /// by the corresponding CLI flag (baked < flag).
    pub(crate) fn from_run(
        f: &CommonRunFlags,
        baked: &artifact::RunDefaults,
    ) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, Some(baked), None)
    }

    /// Resolve for `dvmm test`: baked run-defaults, overridable by the scenario's
    /// `run:` block, overridable by CLI flags. Precedence: baked < scenario < flag.
    pub(crate) fn from_test(
        f: &CommonRunFlags,
        baked: &artifact::RunDefaults,
        scn: &scenario::ScenarioRun,
    ) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, Some(baked), Some(scn))
    }

    fn resolve(
        f: &CommonRunFlags,
        baked: Option<&artifact::RunDefaults>,
        scn: Option<&scenario::ScenarioRun>,
    ) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        let mut prov: Vec<String> = Vec::new();

        // mem: flag > scenario > baked > default.
        let (mem_mib, mem_src) = match (f.mem, scn.and_then(|s| s.mem), baked) {
            (Some(v), _, _) => (v, "flag"),
            (None, Some(v), _) => (v, "scenario"),
            (None, None, Some(b)) => (b.mem_mib, "baked"),
            (None, None, None) => (DEFAULT_MEM_MIB, "default"),
        };
        prov.push(format!("mem={mem_mib} ({mem_src})"));

        // cmdline
        let (cmdline, cl_src) = match (&f.cmdline, scn.and_then(|s| s.cmdline.as_ref()), baked) {
            (Some(v), _, _) => (v.clone(), "flag"),
            (None, Some(v), _) => (v.clone(), "scenario"),
            (None, None, Some(b)) => (b.cmdline.clone(), "baked"),
            (None, None, None) => (DEFAULT_CMDLINE.to_string(), "default"),
        };
        prov.push(format!("cmdline={cmdline:?} ({cl_src})"));

        // fast-forward
        let ff_explicit = f.ff.is_some();
        let (fast_forward, ff_src) = match (f.ff, scn.and_then(|s| s.ff), baked) {
            (Some(v), _, _) => (v, "flag"),
            (None, Some(v), _) => (v, "scenario"),
            (None, None, Some(b)) => (b.fast_forward, "baked"),
            (None, None, None) => (true, "default"),
        };
        prov.push(format!(
            "ff={} ({ff_src})",
            if fast_forward { "on" } else { "off" }
        ));

        // max-virtual-time (horizon)
        let scn_mvt = scn.and_then(|s| s.max_virtual_time.as_ref());
        let (max_virtual_time_secs, mvt_disp, mvt_src) = match (&f.max_virtual_time, scn_mvt, baked) {
            (Some(s), _, _) => (Some(parse_dur(s)?), s.clone(), "flag"),
            (None, Some(s), _) => (Some(parse_dur(s)?), s.clone(), "scenario"),
            (None, None, Some(b)) => match &b.max_virtual_time {
                Some(s) => (Some(parse_dur(s)?), s.clone(), "baked"),
                None => (None, "unset".to_string(), "baked"),
            },
            (None, None, None) => (None, "unset".to_string(), "default"),
        };
        prov.push(format!("max-virtual-time={mvt_disp} ({mvt_src})"));

        // max-jump-secs (no baked value)
        let (max_jump_secs, mj_src) = match f.max_jump_secs {
            Some(v) if v.is_finite() && v > 0.0 => (v, "flag"),
            Some(_) => return Err("--max-jump-secs must be finite and > 0".into()),
            None => (DEFAULT_MAX_JUMP_SECS, "default"),
        };
        prov.push(format!("max-jump-secs={max_jump_secs} ({mj_src})"));
        // The control-channel wire schema (Fable §4): the effective-config
        // preamble records the proto version so a run log is self-describing.
        prov.push(format!("proto-schema={} (built-in)", dvmm_proto::SCHEMA));

        Ok(EffectiveConfig {
            mem_mib,
            cmdline,
            fast_forward,
            ff_explicit,
            max_jump_secs,
            max_virtual_time_secs,
            metrics_out: f.metrics_out.clone(),
            provenance: prov.join(" "),
        })
    }
}

/// Parse a duration string to seconds, erroring (not exiting) on junk — for the
/// resolution path, which propagates errors.
fn parse_dur(s: &str) -> Result<f64, Box<dyn std::error::Error>> {
    crate::parse_duration_secs(s).ok_or_else(|| format!("invalid duration {s:?}").into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn duration_parses_units_and_rejects_junk() {
        use crate::parse_duration_secs;
        assert_eq!(parse_duration_secs("30"), Some(30.0)); // bare = seconds
        assert_eq!(parse_duration_secs("30s"), Some(30.0));
        assert_eq!(parse_duration_secs("500ms"), Some(0.5));
        assert_eq!(parse_duration_secs("5m"), Some(300.0));
        assert_eq!(parse_duration_secs("2h"), Some(7200.0));
        assert_eq!(parse_duration_secs("1.5s"), Some(1.5));
        assert_eq!(parse_duration_secs("  10s "), Some(10.0));
        // Rejections: non-positive, non-finite, unparseable.
        assert_eq!(parse_duration_secs("0"), None);
        assert_eq!(parse_duration_secs("-5s"), None);
        assert_eq!(parse_duration_secs("abc"), None);
        assert_eq!(parse_duration_secs(""), None);
    }
}
