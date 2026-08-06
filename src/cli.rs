//! CLI surface: the clap arg structs for every `tdvmm` subcommand, plus
//! `EffectiveConfig` — the resolved run configuration (baked < scenario < flag)
//! every boot path hands to the vCPU loop.

use clap::{Args, Parser, Subcommand};

use crate::{artifact, scenario};
use crate::{DEFAULT_CMDLINE, DEFAULT_MAX_JUMP_SECS, DEFAULT_MEM_MIB};

#[derive(Parser)]
#[command(
    name = "tdvmm",
    about = "Fast-forward KVM VMM for testing compose stacks",
    long_about = "A single-vCPU, fast-forwardable KVM VMM. `run` boots a self-contained \
                  .tdvmm stack artifact (baked defaults, overridable by flags); `boot` is \
                  the low-level raw kernel+initramfs verb for VMM development.\n\n\
                  Durations (--max-virtual-time): a bare number is seconds, or use a \
                  suffix (ms, s, m, h), e.g. 500ms, 30s, 5m, 2h.",
    after_help = "Examples:\n  \
                  tdvmm doctor\n  \
                  tdvmm build insert-trim guest/stacks/insert-trim/compose.yml\n  \
                  tdvmm run insert-trim\n  \
                  tdvmm test insert-trim --scenario guest/stacks/insert-trim/insert-trim.yml\n  \
                  tdvmm ls",
    version
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
    /// Disable the build progress spinner (plain `== step ==` lines).
    ///
    /// Also forced by a non-terminal stderr, `CI`, or `TERM=dumb`. Affects only
    /// `tdvmm build` and `tdvmm doctor`'s pre-warm.
    #[arg(long, global = true)]
    pub(crate) no_progress: bool,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Bake a compose stack into a .tdvmm artifact.
    ///
    /// A host build tool (needs podman + network): pulls and digest-pins each image,
    /// squashes them into an offline seed store, and packs a self-contained .tdvmm.
    /// The content-hash cache makes an unchanged stack near-instant to re-bake.
    #[command(after_help = "Example:\n  tdvmm build insert-trim guest/stacks/insert-trim/compose.yml -o insert-trim.tdvmm")]
    Build(BuildCliArgs),
    /// Run a .tdvmm stack (offline).
    ///
    /// Loads the artifact (a store name or a path), applies its baked run-defaults,
    /// then boots. Flags override the baked defaults.
    #[command(after_help = "Example:\n  tdvmm run insert-trim --ff on   (a store name, or a path to a .tdvmm)")]
    Run(RunArgs),
    /// Test a .tdvmm stack against a scenario.
    #[command(
        long_about = "Test a .tdvmm stack against a scenario: drive virtual time, assert, \
                      verdict.\n\nExit codes (the shared tdvmm 0-3 contract; `test` itself only \
                      ever produces 0/1/2 — 3 is `build`/`boot`/`run`'s REJECTED/horizon code):\n  \
                      0  PASS — every assertion held\n  \
                      1  FAIL — a scenario assertion failed\n  \
                      2  ERROR — test infrastructure fault: bad/rejected scenario (including \
                      static validation before boot), agent unreachable, wall-clock timeout, ...",
        after_help = "Example:\n  tdvmm test insert-trim.tdvmm --scenario guest/stacks/insert-trim/insert-trim.yml"
    )]
    Test(TestArgs),
    /// List the .tdvmm artifacts in the local store.
    Ls(LsArgs),
    /// Print a .tdvmm artifact's manifest.
    ///
    /// Reads ONLY the manifest.json member — never the big kernel/initramfs payloads.
    Inspect(ArtifactArg),
    /// Verify a .tdvmm artifact and print its sha256 identity.
    ///
    /// Recomputes every member hash and checks it against the manifest.
    Verify(ArtifactArg),
    /// Boot a kernel + initramfs directly (low-level).
    ///
    /// The VMM-dev / smoke verb: boots a raw vmlinux + initramfs, no .tdvmm artifact.
    /// Both default to the managed guest artifacts — the pinned kernel (auto-fetched)
    /// and the busybox clock guest — so a bare `tdvmm boot` smoke-boots the minimal
    /// guest; pass --kernel / --initrd to override.
    Boot(BootArgs),
    /// Check host prerequisites and pre-warm the build cache.
    ///
    /// Probes everything a first `tdvmm build`/`run` needs — /dev/kvm, KVM
    /// (incl. the KVM_VCPU_TSC_OFFSET attribute), host kernel >= 5.16, podman,
    /// network, and the cache dir — then builds/fetches the guest kernel,
    /// agent, and pinned downloads into the cache so later builds are fast.
    /// Exit 0 = everything healthy; 1 = one or more problems.
    Doctor(DoctorArgs),
    /// Build the pinned guest kernel from source.
    ///
    /// Reproducibly compiles the sha-pinned kernel source in the pinned builder
    /// container, verifies the result against kernel.lock, and caches it.
    /// `--record` bootstraps kernel.lock.
    #[command(name = "build-kernel", hide = true)]
    BuildKernel(BuildKernelArgs),
    /// Build the reproducible guest agent binary.
    ///
    /// Builds the static-musl tdvmm-agent from the embedded source in the pinned
    /// builder container and prints `<sha256>  <path>`. Used by the size /
    /// double-build gates.
    #[command(name = "build-agent", hide = true)]
    BuildAgent(BuildAgentArgs),
    /// Print the effective guest clock/timer CPUID profile.
    #[command(hide = true)]
    DumpCpuid,
    /// [internal] Build the seed store inside `podman unshare` (used by `tdvmm build`).
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
    /// [internal] A scripted loopback endpoint for the egress safety suite: binds an
    /// ephemeral `127.0.0.1` port, prints it, and serves the given behavior.
    #[command(name = "__egress-test-server", hide = true)]
    EgressTestServer(EgressTestServerArgs),
}

/// `tdvmm __egress-test-server` args: a behavior name and its positional params
/// (`delay-then-respond <secs>` | `dribble <bytes> <interval_ms>` | `hold-open <secs>`).
#[derive(Args)]
pub(crate) struct EgressTestServerArgs {
    /// The scripted behavior to serve.
    #[arg(value_name = "BEHAVIOR")]
    pub(crate) behavior: String,
    /// The behavior's positional parameters.
    #[arg(value_name = "ARG", num_args = 0..)]
    pub(crate) args: Vec<String>,
}

/// `tdvmm build` args (clap).
#[derive(Args)]
pub(crate) struct BuildCliArgs {
    /// Artifact / stack name (the store key): written as `<name>.tdvmm` and the
    /// name `tdvmm run/test/inspect/verify <name>` later resolves. Must be a single
    /// path component (`[A-Za-z0-9._-]`, never `.`/`..`).
    #[arg(value_name = "name", value_parser = parse_stack_name)]
    pub(crate) name: String,
    /// Path to the compose.yml to bake.
    #[arg(value_name = "compose.yml")]
    pub(crate) compose: String,
    /// Output .tdvmm path (default <cache-dir>/artifacts/<name>.tdvmm, where
    /// <cache-dir> is --cache-dir > $TDVMM_CACHE_DIR > $HOME/.tdvmm). Overrides
    /// only the output PATH; the stored/manifest name stays `<name>`.
    #[arg(short, long, value_name = "PATH")]
    pub(crate) out: Option<String>,
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
    /// the fetched/built kernel). Precedence: this flag > $TDVMM_CACHE_DIR >
    /// $HOME/.tdvmm. The resolved dir is logged at build start.
    #[arg(long, value_name = "PATH")]
    pub(crate) cache_dir: Option<String>,
}

/// `tdvmm doctor` args.
#[derive(Args)]
pub(crate) struct DoctorArgs {
    /// Run only the prerequisite checks; skip the cache pre-warm (the
    /// kernel/agent container builds and the pinned downloads).
    #[arg(long)]
    pub(crate) skip_downloads: bool,
}

/// `tdvmm build-agent` args.
#[derive(Args)]
pub(crate) struct BuildAgentArgs {
    /// Output path for the built static-musl agent binary.
    #[arg(short, long, value_name = "PATH", default_value = "tdvmm-agent.bin")]
    pub(crate) out: String,
}

/// `tdvmm build-kernel` args.
#[derive(Args)]
pub(crate) struct BuildKernelArgs {
    /// Output path for the vmlinux (default: <cache-dir>/kernel/vmlinux-<version>).
    #[arg(short, long, value_name = "PATH")]
    pub(crate) out: Option<String>,
    /// Cache directory (kernel source tarball + built kernel land here).
    /// Precedence: this flag > $TDVMM_CACHE_DIR > $HOME/.tdvmm.
    #[arg(long, value_name = "PATH")]
    pub(crate) cache_dir: Option<String>,
    /// Force the reproducible container build even if a sha-verified kernel is
    /// already cached (used by the byte-identity gates).
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
    /// Open host-mediated egress: a SOCKS5h proxy over COM4/ttyS3 for
    /// guest-initiated TCP. Off by default (the guest stays closed-world). While a
    /// connection is open, fast-forward is phase-gated (real-rate); the clock only
    /// jumps once egress is quiescent. NEVER baked into an artifact.
    #[arg(long)]
    allow_egress: bool,
}

#[derive(Args)]
pub(crate) struct BootArgs {
    /// Uncompressed ELF vmlinux (default: the pinned guest kernel, auto-fetched).
    #[arg(long, value_name = "PATH")]
    pub(crate) kernel: Option<String>,
    /// Initramfs to boot (default: the committed busybox clock guest).
    #[arg(long, value_name = "PATH")]
    pub(crate) initrd: Option<String>,
    #[command(flatten)]
    pub(crate) common: CommonRunFlags,
}

#[derive(Args)]
pub(crate) struct RunArgs {
    /// A store name (e.g. `tigerbeetle`) or a path to a .tdvmm artifact.
    #[arg(value_name = "stack")]
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
    /// A store name (e.g. `tigerbeetle`) or a path to a .tdvmm artifact.
    #[arg(value_name = "stack")]
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
    /// A store name (e.g. `tigerbeetle`) or a path to a .tdvmm artifact.
    #[arg(value_name = "stack")]
    pub(crate) artifact: String,
}

#[derive(Args)]
pub(crate) struct LsArgs {
    /// Also compute and show each artifact's sha256 identity (first 12 hex).
    /// Off by default: hashing every artifact reads hundreds of MB each.
    #[arg(short, long)]
    pub(crate) digest: bool,
}

/// clap value parser for `--ff on|off` (also accepts 1/0/true/false).
fn parse_onoff(s: &str) -> Result<bool, String> {
    match s {
        "on" | "1" | "true" => Ok(true),
        "off" | "0" | "false" => Ok(false),
        _ => Err(format!("expected on|off (got {s:?})")),
    }
}

/// clap value parser for `tdvmm build`'s required `<name>` positional: the value
/// becomes a `<name>.tdvmm` filename in the store and a store-resolvable name
/// (`tdvmm run <name>`), so it must be a single safe path component. Accepts a
/// non-empty `[A-Za-z0-9._-]+` that is not `.` or `..`; rejects everything else
/// (path separators, spaces, …) via the shared `is_safe_path_component` rule
/// (also behind `sanitize_service_filename`). Rejecting a path here doubles as the
/// migration guard for the retired one-arg `tdvmm build ./compose.yml` form.
fn parse_stack_name(s: &str) -> Result<String, String> {
    if crate::is_safe_path_component(s) {
        return Ok(s.to_owned());
    }
    let rule = "a stack name must be a single path component ([A-Za-z0-9._-], not '.'/'..')";
    // A value containing a separator is almost certainly a forgotten or transposed
    // name — the compose path landing where the name now belongs. Point the user at
    // the new argument order. (A bare `compose.yaml` with no `/` is a valid name.)
    if s.contains('/') {
        Err(format!("{rule} — did you mean `tdvmm build <name> {s}`?"))
    } else {
        Err(rule.to_owned())
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
    /// Whether host-mediated egress is opened for this run. Resolved with
    /// precedence `scenario < flag` and DELIBERATELY no baked tier — an artifact
    /// must never gain network access merely by being re-baked (see [`resolve`]).
    pub(crate) allow_egress: bool,
    /// The formatted per-knob provenance, e.g.
    /// `mem=3072 (baked) ff=off (flag) horizon=36h (baked) ...`. Gains an
    /// `egress=on (flag|scenario)` token ONLY when egress is opened, so a
    /// closed-world run's line is byte-identical to before this feature.
    pub(crate) provenance: String,
}

impl EffectiveConfig {
    /// Resolve for `tdvmm boot`: no baked defaults; each knob is a flag override of
    /// the binary default.
    pub(crate) fn from_boot(f: &CommonRunFlags) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, None, None)
    }

    /// Resolve for `tdvmm run`: the artifact's baked run-defaults, each overridable
    /// by the corresponding CLI flag (baked < flag).
    pub(crate) fn from_run(
        f: &CommonRunFlags,
        baked: &artifact::RunDefaults,
    ) -> Result<EffectiveConfig, Box<dyn std::error::Error>> {
        Self::resolve(f, Some(baked), None)
    }

    /// Resolve for `tdvmm test`: baked run-defaults, overridable by the scenario's
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
        prov.push(format!("proto-schema={} (built-in)", tdvmm_proto::SCHEMA));

        // allow-egress: precedence `scenario < flag`, with DELIBERATELY no baked
        // tier. Egress opens the closed world to guest-initiated network I/O; an
        // artifact must never gain that merely by being re-baked, so a baked
        // default is intentionally unrepresentable here (there is no
        // `RunDefaults.allow_egress` to consult). The `--allow-egress` presence
        // flag can only turn it ON, never override a scenario's ON back to off.
        let allow_egress = if f.allow_egress {
            true // flag
        } else {
            scn.map(|s| s.allow_egress).unwrap_or(false) // scenario, else default off
        };
        // Provenance token ONLY when opened, so a closed-world run's line is
        // byte-identical to before this feature (INV-E0).
        if allow_egress {
            let src = if f.allow_egress { "flag" } else { "scenario" };
            prov.push(format!("egress=on ({src})"));
        }

        Ok(EffectiveConfig {
            mem_mib,
            cmdline,
            fast_forward,
            ff_explicit,
            max_jump_secs,
            max_virtual_time_secs,
            metrics_out: f.metrics_out.clone(),
            allow_egress,
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

    #[test]
    fn stack_name_accepts_safe_and_rejects_unsafe() {
        use super::parse_stack_name;
        // Safe single path components pass through unchanged — dots are allowed, so
        // a bare `compose.yaml` (no separator) is a VALID name.
        assert_eq!(parse_stack_name("insert-trim").unwrap(), "insert-trim");
        assert_eq!(parse_stack_name("web_app.v2").unwrap(), "web_app.v2");
        assert_eq!(parse_stack_name("compose.yaml").unwrap(), "compose.yaml");
        // Empty, dot-dirs, separators, and stray characters are rejected.
        for bad in ["", ".", "..", "a/b", "../x", "has space", "tab\tx"] {
            assert!(parse_stack_name(bad).is_err(), "should reject {bad:?}");
        }
        // Migration guard: a stale one-arg `tdvmm build ./compose.yml` puts a PATH
        // (with a `/`) where the name now belongs — rejected, with a hint toward the
        // new `tdvmm build <name> <compose>` form.
        for path in ["./compose.yml", "guest/stacks/demo/compose.yml"] {
            let err = parse_stack_name(path).unwrap_err();
            assert!(err.contains("tdvmm build <name>"), "hint missing for {path:?}: {err}");
        }
        // A plain invalid name (not a path) gets the rule but NOT the path hint.
        let err = parse_stack_name("has space").unwrap_err();
        assert!(!err.contains("did you mean"), "no path hint for a non-path name: {err}");
    }

    #[test]
    fn build_takes_name_then_compose_positionally() {
        use super::{Cli, Cmd};
        use clap::Parser;
        // Correct order: name first, compose path second.
        let cli = Cli::try_parse_from(["tdvmm", "build", "demo", "x.yml"]).unwrap();
        let Cmd::Build(args) = cli.cmd else { panic!("expected build subcommand") };
        assert_eq!(args.name, "demo");
        assert_eq!(args.compose, "x.yml");
        // Stale one-arg form: the path lands in the name slot and is rejected.
        assert!(Cli::try_parse_from(["tdvmm", "build", "./x.yml"]).is_err());
    }
}
