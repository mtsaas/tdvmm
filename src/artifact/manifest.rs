//! `manifest.json` — the artifact's typed model and its canonical serialization.
//!
//! The manifest is the first tar member. It records what the stack is (anchors),
//! how `run` should boot it (run-defaults), and a sha256 for every OTHER member
//! (never itself). [`Manifest::to_canonical_json`] fixes the byte encoding — field
//! order from the struct, pretty printing, trailing newline — so identical data
//! always serializes identically, which is what keeps the `.tdvmm` reproducible.

use serde::{Deserialize, Serialize};

use super::error::ArtifactError;
use super::FORMAT_VERSION;

fn default_format_version() -> u32 {
    FORMAT_VERSION
}

/// One non-manifest member's integrity record. `manifest.json` is never listed
/// here; `verify` recomputes each of these from the archive.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub size: u64,
    pub sha256: String,
}

/// The pinned Docker Compose v2 engine baked into the guest.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComposeEngine {
    pub version: String,
    pub sha256: String,
}

/// One image the bake resolved: its upstream digest ref, the pinned ref the guest
/// runs, the squash/build/plain policy, and (for squashed/built images) the
/// reproducible content identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImagePin {
    pub upstream: String,
    pub pinned: String,
    pub policy: String,
    #[serde(default)]
    pub content_id: String,
    #[serde(default)]
    pub size_mib: u64,
}

/// The bake toolchain that produced the artifact. Every field is a declared input
/// (identical on every host), never host-probed, so nothing host-specific enters
/// the hashed bytes.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Toolchain {
    /// The pinned builder-image refs (`image@sha256`) that produced the guest
    /// binaries — the musl agent builder and the kernel builder. Sorted, so the
    /// bytes are order-stable; a toolchain bump shows up as a changed digest here.
    #[serde(default)]
    pub builders: Vec<String>,
    #[serde(default)]
    pub alpine: String,
    #[serde(default)]
    pub compose: String,
}

/// Everything that pins WHAT this stack is, beyond the raw member hashes. A change
/// to any of it — a different image, engine, CPUID profile, or toolchain digest —
/// changes the manifest, and therefore the whole-file sha256.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Anchors {
    /// sha256 of the `cpuid_profile` text below (matches `guest/manifest.txt`).
    pub cpuid_sha256: String,
    /// The effective guest clock/timer CPUID profile (`tdvmm dump-cpuid` output).
    pub cpuid_profile: String,
    pub compose_engine: ComposeEngine,
    #[serde(default)]
    pub images: Vec<ImagePin>,
    #[serde(default)]
    pub toolchain: Toolchain,
    /// The bake's guest-RAM estimate (MiB).
    pub ram_estimate_mib: u64,
    /// sha256 of the baked static `tdvmm-agent` binary. `default` so pre-agent
    /// artifacts still deserialize.
    #[serde(default)]
    pub agent_sha256: String,
    /// The build hash the baked agent reports over `ping`/hello — the run-time
    /// compatibility oracle. Matches `agent_build_hash` in the ping log.
    #[serde(default)]
    pub agent_build_hash: String,
}

/// The baked run-defaults `tdvmm run` applies unless a CLI flag overrides them
/// (baked < flag).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunDefaults {
    pub mem_mib: u64,
    pub cmdline: String,
    pub fast_forward: bool,
    /// Virtual-time horizon (a duration string, e.g. `"36h"`); `null` = unbounded.
    #[serde(default)]
    pub max_virtual_time: Option<String>,
}

/// The whole `manifest.json`. Field order here IS the on-disk key order (canonical
/// JSON), so reordering these fields changes every artifact's bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default = "default_format_version")]
    pub format_version: u32,
    pub stack: String,
    pub project: String,
    /// Per-member integrity records (every member EXCEPT `manifest.json`).
    #[serde(default)]
    pub members: Vec<Member>,
    pub anchors: Anchors,
    pub run_defaults: RunDefaults,
}

impl Manifest {
    /// Parse `manifest.json` bytes, rejecting any unsupported `format_version`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Manifest, ArtifactError> {
        let m: Manifest = serde_json::from_slice(bytes)
            .map_err(|e| ArtifactError::manifest("parsing manifest.json", e))?;
        if m.format_version != FORMAT_VERSION {
            return Err(ArtifactError::malformed(format!(
                "unsupported .tdvmm format_version {} (this tdvmm speaks v{FORMAT_VERSION}); \
                 rebuild the artifact or upgrade tdvmm",
                m.format_version
            )));
        }
        Ok(m)
    }

    /// Serialize to canonical bytes: pretty JSON, field order fixed by the struct,
    /// trailing newline. Deterministic for identical data.
    pub fn to_canonical_json(&self) -> Result<Vec<u8>, ArtifactError> {
        let mut v = serde_json::to_vec_pretty(self)
            .map_err(|e| ArtifactError::manifest("serializing manifest.json", e))?;
        v.push(b'\n');
        Ok(v)
    }

    /// Look up a member's recorded hash by name.
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }
}
