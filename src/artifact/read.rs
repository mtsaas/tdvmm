//! The read path: pulling members back out of a `.tdvmm`.
//!
//! `inspect` reads only the manifest ([`read_manifest`]); `run`/`test` read the
//! manifest plus the three payload members ([`read_for_run`]); `verify` recomputes
//! every member hash against the manifest and reports the whole-file identity
//! ([`verify`]). All three iterate the archive with [`Entries`](super::tar), which
//! skips any member it does not recognize, so reserved members added within v1 are
//! ignored rather than fatal.

use std::path::Path;

use super::error::ArtifactError;
use super::manifest::Manifest;
use super::tar::{self, Entries};
use super::{MEMBER_COMPOSE_LOCK, MEMBER_INITRAMFS, MEMBER_KERNEL, MEMBER_MANIFEST};

fn open(path: &Path) -> Result<std::fs::File, ArtifactError> {
    std::fs::File::open(path).map_err(|e| ArtifactError::io(format!("opening {}", path.display()), e))
}

/// Read ONLY the `manifest.json` member. Canonical order puts it first, so this
/// scans to the first member and stops — it never touches the large payloads.
pub fn read_manifest(path: impl AsRef<Path>) -> Result<Manifest, ArtifactError> {
    let path = path.as_ref();
    let mut entries = Entries::new(open(path)?)?;
    while let Some(e) = entries.next().transpose()? {
        if e.name == MEMBER_MANIFEST {
            return Manifest::from_bytes(&entries.read(&e)?);
        }
    }
    Err(ArtifactError::malformed(format!(
        "{}: no {MEMBER_MANIFEST} member (not a .tdvmm artifact?)",
        path.display()
    )))
}

/// What `run` needs: the manifest plus the kernel + initramfs bytes in memory (no
/// extraction to disk — the caller feeds these to the loader). `compose.lock.yml`
/// comes back too, so `run` can hash-verify it on load.
pub struct RunPayload {
    pub manifest: Manifest,
    pub kernel: Vec<u8>,
    pub initramfs: Vec<u8>,
    pub compose_lock: Vec<u8>,
}

impl RunPayload {
    /// Recompute the payload members' hashes and check them against the manifest,
    /// returning the first mismatch (or missing record) as an error.
    pub fn verify_members(&self) -> Result<(), ArtifactError> {
        for (name, data) in [
            (MEMBER_KERNEL, &self.kernel),
            (MEMBER_INITRAMFS, &self.initramfs),
            (MEMBER_COMPOSE_LOCK, &self.compose_lock),
        ] {
            let want = self.manifest.member(name).ok_or_else(|| {
                ArtifactError::malformed(format!("manifest has no record for member {name:?}"))
            })?;
            let got = tar::sha256_hex(data);
            if got != want.sha256 {
                return Err(ArtifactError::malformed(format!(
                    "member {name:?} hash mismatch: manifest {}, actual {got}",
                    want.sha256
                )));
            }
        }
        Ok(())
    }
}

/// Read the members `run` needs. Captures the manifest + the three payload members
/// and skips anything else (reserved prefixes).
pub fn read_for_run(path: impl AsRef<Path>) -> Result<RunPayload, ArtifactError> {
    let path = path.as_ref();
    let mut entries = Entries::new(open(path)?)?;
    let mut manifest = None;
    let mut kernel = None;
    let mut initramfs = None;
    let mut compose_lock = None;
    while let Some(e) = entries.next().transpose()? {
        match e.name.as_str() {
            MEMBER_MANIFEST => manifest = Some(Manifest::from_bytes(&entries.read(&e)?)?),
            MEMBER_KERNEL => kernel = Some(entries.read(&e)?),
            MEMBER_INITRAMFS => initramfs = Some(entries.read(&e)?),
            MEMBER_COMPOSE_LOCK => compose_lock = Some(entries.read(&e)?),
            _ => {} // reserved / unknown; the iterator skips its content
        }
    }
    let missing = |m: &str| ArtifactError::malformed(format!("{}: missing {m}", path.display()));
    Ok(RunPayload {
        manifest: manifest.ok_or_else(|| missing(MEMBER_MANIFEST))?,
        kernel: kernel.ok_or_else(|| missing(MEMBER_KERNEL))?,
        initramfs: initramfs.ok_or_else(|| missing(MEMBER_INITRAMFS))?,
        compose_lock: compose_lock.ok_or_else(|| missing(MEMBER_COMPOSE_LOCK))?,
    })
}

/// One member's verification result.
pub struct MemberCheck {
    pub name: String,
    pub expected: String,
    pub actual: String,
    pub ok: bool,
}

/// The result of verifying an artifact: the whole-file identity plus a per-member
/// hash check against the manifest.
pub struct VerifyReport {
    pub file_sha256: String,
    pub checks: Vec<MemberCheck>,
    /// Members named in `manifest.members` that were absent from the archive.
    pub missing: Vec<String>,
}

impl VerifyReport {
    pub fn all_ok(&self) -> bool {
        self.missing.is_empty() && self.checks.iter().all(|c| c.ok)
    }
}

/// Recompute every non-manifest member's sha256 (streamed, never buffered) and
/// compare it to the value recorded in `manifest.json`; also compute the whole-file
/// sha256 — the identity.
pub fn verify(path: impl AsRef<Path>) -> Result<VerifyReport, ArtifactError> {
    let path = path.as_ref();
    let file_sha256 = tar::file_sha256_hex(path)?;
    let mut entries = Entries::new(open(path)?)?;
    let mut manifest = None;
    let mut actual: Vec<(String, String)> = Vec::new();
    while let Some(e) = entries.next().transpose()? {
        if e.name == MEMBER_MANIFEST {
            manifest = Some(Manifest::from_bytes(&entries.read(&e)?)?);
        } else {
            let sha = entries.sha256(&e)?;
            actual.push((e.name, sha));
        }
    }
    let manifest = manifest.ok_or_else(|| {
        ArtifactError::malformed(format!("{}: missing {MEMBER_MANIFEST}", path.display()))
    })?;

    let mut checks = Vec::new();
    let mut missing = Vec::new();
    for m in &manifest.members {
        match actual.iter().find(|(n, _)| n == &m.name) {
            Some((_, act)) => checks.push(MemberCheck {
                name: m.name.clone(),
                expected: m.sha256.clone(),
                actual: act.clone(),
                ok: *act == m.sha256,
            }),
            None => missing.push(m.name.clone()),
        }
    }
    Ok(VerifyReport { file_sha256, checks, missing })
}
