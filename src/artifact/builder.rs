//! The write path: a type-state builder where an incomplete or mis-ordered
//! `.tdvmm` is a compile error.
//!
//! Members are added in canonical order, each moving the builder to the next state:
//!
//! ```ignore
//! let sealed = ArtifactBuilder::new(stack, project, anchors, run_defaults)
//!     .compose_lock(lock_bytes)     // -> NeedsKernel
//!     .kernel(kernel_bytes)         // -> NeedsInitramfs
//!     .initramfs(initramfs_bytes)?; // seal -> SealedArtifact
//! let written = sealed.write_to(BufWriter::new(File::create(path)?))?;
//! ```
//!
//! There is no method to omit, reorder, or duplicate a member, or to supply a
//! manifest: the manifest is frozen at the [seal step](NeedsInitramfs::initramfs)
//! from the hashes of the actual payloads and serialized canonically. A
//! [`SealedArtifact`] is therefore consistent by construction — it cannot fail
//! `verify`.

use std::io::Write;

use super::error::ArtifactError;
use super::manifest::{Anchors, Manifest, Member, RunDefaults};
use super::tar::{self, HashingWriter, MAX_MEMBER_SIZE};
use super::{FORMAT_VERSION, MEMBER_COMPOSE_LOCK, MEMBER_INITRAMFS, MEMBER_KERNEL, MEMBER_MANIFEST};

/// Identity, anchors, and run-defaults are fixed; no payloads yet.
#[must_use]
pub struct ArtifactBuilder {
    stack: String,
    project: String,
    anchors: Anchors,
    run_defaults: RunDefaults,
}

impl ArtifactBuilder {
    pub fn new(stack: String, project: String, anchors: Anchors, run_defaults: RunDefaults) -> Self {
        ArtifactBuilder { stack, project, anchors, run_defaults }
    }

    /// Add `compose.lock.yml`.
    pub fn compose_lock(self, bytes: Vec<u8>) -> NeedsKernel {
        NeedsKernel {
            stack: self.stack,
            project: self.project,
            anchors: self.anchors,
            run_defaults: self.run_defaults,
            compose_lock: bytes,
        }
    }
}

/// `compose.lock.yml` is present.
#[must_use]
pub struct NeedsKernel {
    stack: String,
    project: String,
    anchors: Anchors,
    run_defaults: RunDefaults,
    compose_lock: Vec<u8>,
}

impl NeedsKernel {
    /// Add the `kernel` member.
    pub fn kernel(self, bytes: Vec<u8>) -> NeedsInitramfs {
        NeedsInitramfs {
            stack: self.stack,
            project: self.project,
            anchors: self.anchors,
            run_defaults: self.run_defaults,
            compose_lock: self.compose_lock,
            kernel: bytes,
        }
    }
}

/// `compose.lock.yml` + `kernel` are present.
#[must_use]
pub struct NeedsInitramfs {
    stack: String,
    project: String,
    anchors: Anchors,
    run_defaults: RunDefaults,
    compose_lock: Vec<u8>,
    kernel: Vec<u8>,
}

/// Reject a member whose byte length exceeds the USTAR size field, returning the
/// validated length. Every member — the three payloads and the serialized manifest —
/// passes through here at seal, so `tar::write_octal` never sees an out-of-range size.
fn cap_member(name: &str, len: usize) -> Result<u64, ArtifactError> {
    let len = len as u64;
    if len > MAX_MEMBER_SIZE {
        return Err(ArtifactError::malformed(format!(
            "member {name:?} is {len} bytes, over the USTAR {MAX_MEMBER_SIZE}-byte limit"
        )));
    }
    Ok(len)
}

impl NeedsInitramfs {
    /// Add the `initramfs` member and seal: cap-check every member against the
    /// USTAR size limit, then freeze the canonical manifest from the payload
    /// hashes. This is the only fallible transition.
    pub fn initramfs(self, bytes: Vec<u8>) -> Result<SealedArtifact, ArtifactError> {
        let members: [(&str, Vec<u8>); 3] = [
            (MEMBER_COMPOSE_LOCK, self.compose_lock),
            (MEMBER_KERNEL, self.kernel),
            (MEMBER_INITRAMFS, bytes),
        ];
        let mut records = Vec::with_capacity(members.len());
        for (name, data) in &members {
            records.push(Member {
                name: (*name).to_string(),
                size: cap_member(name, data.len())?,
                sha256: tar::sha256_hex(data),
            });
        }
        let manifest = Manifest {
            format_version: FORMAT_VERSION,
            stack: self.stack,
            project: self.project,
            members: records,
            anchors: self.anchors,
            run_defaults: self.run_defaults,
        };
        let manifest_json = manifest.to_canonical_json()?;
        cap_member(MEMBER_MANIFEST, manifest_json.len())?;
        let [(_, compose_lock), (_, kernel), (_, initramfs)] = members;
        Ok(SealedArtifact { manifest_json, compose_lock, kernel, initramfs })
    }
}

/// A complete, internally consistent `.tdvmm`. Its manifest was serialized from the
/// payload hashes, so it cannot fail `verify`. Stream it out with [`write_to`].
///
/// [`write_to`]: SealedArtifact::write_to
#[must_use]
pub struct SealedArtifact {
    manifest_json: Vec<u8>,
    compose_lock: Vec<u8>,
    kernel: Vec<u8>,
    initramfs: Vec<u8>,
}

impl SealedArtifact {
    /// Stream the deterministic archive (manifest first) into `w`, returning the
    /// whole-file identity and length computed in the same pass. The only failures
    /// are I/O from `w`.
    pub fn write_to<W: Write>(&self, w: W) -> Result<Written, ArtifactError> {
        let mut hw = HashingWriter::new(w);
        tar::write_member(&mut hw, MEMBER_MANIFEST, &self.manifest_json)?;
        tar::write_member(&mut hw, MEMBER_COMPOSE_LOCK, &self.compose_lock)?;
        tar::write_member(&mut hw, MEMBER_KERNEL, &self.kernel)?;
        tar::write_member(&mut hw, MEMBER_INITRAMFS, &self.initramfs)?;
        tar::write_trailer(&mut hw)?;
        hw.flush().map_err(|e| ArtifactError::io("flushing artifact", e))?;
        let (sha256_hex, len) = hw.finish();
        Ok(Written { sha256_hex, len })
    }
}

/// The result of writing a sealed artifact: its sha256 identity and byte length,
/// with no re-read of the output.
pub struct Written {
    pub sha256_hex: String,
    pub len: u64,
}
