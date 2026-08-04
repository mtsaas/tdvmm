//! .dvmm packing (mirror pack-dvmm.sh + artifact::pack)

use std::path::Path;
use std::process::Command;

use crate::artifact;
use super::images::ImgRecord;
use super::ux::capture;
use super::{ALPINE_VER, DEFAULT_CMDLINE};

#[allow(clippy::too_many_arguments)]
pub(super) fn pack_dvmm(
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
