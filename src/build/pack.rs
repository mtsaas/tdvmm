//! `.tdvmm` packing: assemble the manifest anchors from the bake's outputs, then
//! hand the payloads to the type-state [`ArtifactBuilder`] to seal.

use std::path::Path;
use std::process::Command;

use crate::artifact::{self, ArtifactBuilder, SealedArtifact};
use super::images::ImgRecord;
use super::ux::capture;
use super::{ALPINE_VER, DEFAULT_CMDLINE};

/// The inputs [`pack_tdvmm`] needs, named rather than a long positional list.
pub(super) struct PackInputs<'a> {
    pub self_exe: &'a Path,
    pub records: &'a [ImgRecord],
    pub compose_version: &'a str,
    pub compose_sha256: &'a str,
    pub stack: &'a str,
    pub project: &'a str,
    pub mem_mib: u64,
    pub est_mib: u64,
    pub builders: &'a [String],
    pub agent_sha: &'a str,
    pub agent_build_hash: &'a str,
    pub kernel_path: &'a Path,
    pub initramfs_path: &'a Path,
    pub lock_path: &'a Path,
}

/// Build the manifest anchors, then seal a [`SealedArtifact`] from the kernel,
/// initramfs, and compose.lock payloads. The caller streams it to disk with
/// [`SealedArtifact::write_to`].
pub(super) fn pack_tdvmm(inputs: PackInputs) -> Result<SealedArtifact, Box<dyn std::error::Error>> {
    let PackInputs {
        self_exe,
        records,
        compose_version,
        compose_sha256,
        stack,
        project,
        mem_mib,
        est_mib,
        builders,
        agent_sha,
        agent_build_hash,
        kernel_path,
        initramfs_path,
        lock_path,
    } = inputs;

    // The effective guest CPUID profile, dumped by this same binary — a
    // reproducibility anchor recorded in the manifest.
    let cpuid_profile = capture(Command::new(self_exe).arg("dump-cpuid"))?;
    let cpuid_sha = artifact::sha256_hex(cpuid_profile.as_bytes());

    // Dedup image records by key (last wins), sorted by key for stable bytes.
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
    let images = by_key.into_values().collect();

    let anchors = artifact::Anchors {
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
        agent_sha256: agent_sha.to_string(),
        agent_build_hash: agent_build_hash.to_string(),
    };
    let run_defaults = artifact::RunDefaults {
        mem_mib,
        cmdline: DEFAULT_CMDLINE.to_string(),
        fast_forward: true,
        max_virtual_time: None,
    };

    let sealed = ArtifactBuilder::new(stack.to_string(), project.to_string(), anchors, run_defaults)
        .compose_lock(std::fs::read(lock_path)?)
        .kernel(std::fs::read(kernel_path)?)
        .initramfs(std::fs::read(initramfs_path)?)?;
    Ok(sealed)
}
