//! The `.tdvmm` single-file artifact (format v1).
//!
//! A baked stack is one self-contained file: a plain, uncompressed USTAR archive
//! whose members appear in this fixed order:
//!
//!   1. `manifest.json`     — anchors, per-member hashes, and run-defaults
//!   2. `compose.lock.yml`  — the only compose file the guest sees
//!   3. `kernel`            — the ELF `vmlinux`
//!   4. `initramfs`         — the per-stack initramfs (gzip-compressed)
//!
//! `manifest.json` is first so `inspect` reads one header plus one small member and
//! stops, never touching the large `kernel`/`initramfs` payloads.
//!
//! ## Byte layout
//!
//! Each member is a 512-byte header, its content, then zero-padding to the next
//! 512-byte boundary; the archive ends with two all-zero blocks. Header fields are
//! pinned — mode 0644, uid/gid 0, mtime 0, `ustar\0` + `00`, no PAX/GNU extensions —
//! so a member's bytes depend only on its name, size, and content. ([`tar`] holds
//! the codec.)
//!
//! ## Identity
//!
//! The artifact's identity is the sha256 of the whole file. `manifest.json` records
//! a sha256 for every OTHER member but not itself (a self-hash would be circular);
//! the header checksums cover the one region member hashes can't — the headers,
//! including the manifest's own. `verify` recomputes the member hashes and reports
//! the whole-file identity.
//!
//! ## Determinism
//!
//! Identical inputs produce a byte-identical `.tdvmm`: the pinned header fields
//! above, plus canonical `manifest.json` (field order fixed by [`Manifest`](manifest::Manifest), pretty
//! JSON, trailing newline). `golden_sha256_is_stable` is the tripwire.
//!
//! ## Lifecycle
//!
//! `build` assembles the artifact through the type-state [`ArtifactBuilder`]
//! ([`builder`]) and streams it into the [store](store_dir). The readers ([`read`])
//! serve the verbs: `run`/`test` via [`read_for_run`], `inspect` via
//! [`read_manifest`], `verify` via [`verify`], `ls` via [`list_store`]. The writer
//! emits members in canonical order; the reader accepts any order and skips members
//! it does not recognize.
//!
//! ## Reserved members
//!
//! `scenario/`, `record.log`, and `snapshot/` are reserved for later phases and
//! unused in v1; the reader ignores any member it does not recognize.
//!
//! ## Versioning
//!
//! [`FORMAT_VERSION`] is bumped only on an incompatible change to the member set or
//! manifest schema; the readers reject any other version.

mod builder;
mod error;
mod manifest;
mod read;
mod store;
mod tar;

pub use builder::{ArtifactBuilder, SealedArtifact};
pub use manifest::{Anchors, ComposeEngine, ImagePin, RunDefaults, Toolchain};
pub use read::{read_for_run, read_manifest, verify, RunPayload};
pub use store::{list_store, resolve, store_dir};
pub use tar::{file_sha256_hex, sha256_hex};

/// Canonical member names, in tar order.
pub const MEMBER_MANIFEST: &str = "manifest.json";
pub const MEMBER_COMPOSE_LOCK: &str = "compose.lock.yml";
pub const MEMBER_KERNEL: &str = "kernel";
pub const MEMBER_INITRAMFS: &str = "initramfs";

/// The on-disk format version, bumped only on an incompatible change to the member
/// set or manifest schema. Readers reject any other version.
pub const FORMAT_VERSION: u32 = 1;

// Every member name must fit the USTAR 100-byte name field (no long-name extension).
const _: () = assert!(MEMBER_MANIFEST.len() <= 100);
const _: () = assert!(MEMBER_COMPOSE_LOCK.len() <= 100);
const _: () = assert!(MEMBER_KERNEL.len() <= 100);
const _: () = assert!(MEMBER_INITRAMFS.len() <= 100);

#[cfg(test)]
mod tests {
    use super::manifest::Manifest;
    use super::*;

    /// A fixed sample artifact, built through the type-state chain. Its whole-file
    /// sha256 is asserted below as the byte-identity contract.
    fn sample_sealed() -> SealedArtifact {
        let anchors = Anchors {
            cpuid_sha256: "abc".into(),
            cpuid_profile: "# profile\n0x1 ...".into(),
            compose_engine: ComposeEngine { version: "v5.3.1".into(), sha256: "deadbeef".into() },
            images: vec![ImagePin {
                upstream: "docker.io/library/postgres@sha256:57c7".into(),
                pinned: "localhost/tdvmm-postgres@sha256:cbf2".into(),
                policy: "squash".into(),
                content_id: "sha256:af5b".into(),
                size_mib: 283,
            }],
            toolchain: Toolchain {
                builders: vec![
                    "docker.io/library/debian@sha256:beef".into(),
                    "docker.io/library/rust@sha256:cafe".into(),
                ],
                alpine: "3.22.5".into(),
                compose: "v5.3.1".into(),
            },
            ram_estimate_mib: 1742,
            agent_sha256: "sha256agent".into(),
            agent_build_hash: "deadbeefcafe0001".into(),
        };
        let run_defaults = RunDefaults {
            mem_mib: 3072,
            cmdline: "console=ttyS0 tdvmm.stack=1".into(),
            fast_forward: true,
            max_virtual_time: None,
        };
        ArtifactBuilder::new("dogfood".into(), "tdvmm_dogfood".into(), anchors, run_defaults)
            .compose_lock(b"name: tdvmm_dogfood\n".to_vec())
            .kernel(b"\x7fELF fake kernel bytes".to_vec())
            .initramfs(b"\x1f\x8b fake gzip initramfs".to_vec())
            .expect("seal")
    }

    #[test]
    fn golden_sha256_is_stable() {
        let mut buf = Vec::new();
        let written = sample_sealed().write_to(&mut buf).unwrap();
        // The reproducibility contract: a change here means the artifact bytes
        // changed — rebuild the golden only when that change is intended.
        assert_eq!(
            written.sha256_hex,
            "044a6ca5696404578933d94559d31df512d36e73e3102f4691dccd9eb16ae523",
        );
        assert_eq!(written.len as usize, buf.len());
        assert_eq!(sha256_hex(&buf), written.sha256_hex);
        // manifest.json is the first member.
        let name_end = buf[..100].iter().position(|&b| b == 0).unwrap_or(100);
        assert_eq!(&buf[..name_end], MEMBER_MANIFEST.as_bytes());
    }

    #[test]
    fn roundtrips_via_readers() {
        let mut buf = Vec::new();
        sample_sealed().write_to(&mut buf).unwrap();
        std::fs::create_dir_all("target/test-artifacts").ok();
        let p = "target/test-artifacts/roundtrip.tdvmm";
        std::fs::write(p, &buf).unwrap();

        let man = read_manifest(p).unwrap();
        assert_eq!(man.stack, "dogfood");
        assert_eq!(man.members.len(), 3);
        assert_eq!(
            man.member(MEMBER_KERNEL).unwrap().sha256,
            sha256_hex(b"\x7fELF fake kernel bytes")
        );

        let payload = read_for_run(p).unwrap();
        assert_eq!(payload.kernel, b"\x7fELF fake kernel bytes");
        assert_eq!(payload.initramfs, b"\x1f\x8b fake gzip initramfs");
        assert_eq!(payload.compose_lock, b"name: tdvmm_dogfood\n");
        payload.verify_members().unwrap();

        let report = verify(p).unwrap();
        assert!(report.all_ok(), "fresh artifact must verify");
        assert_eq!(report.file_sha256, sha256_hex(&buf));
        assert_eq!(report.checks.len(), 3);
    }

    #[test]
    fn verify_catches_a_flipped_byte() {
        let mut buf = Vec::new();
        sample_sealed().write_to(&mut buf).unwrap();
        let kernel = b"\x7fELF fake kernel bytes";
        let pos = buf
            .windows(kernel.len())
            .position(|w| w == kernel)
            .expect("kernel bytes present");
        buf[pos + 3] ^= 0xff;
        std::fs::create_dir_all("target/test-artifacts").ok();
        let p = "target/test-artifacts/corrupt.tdvmm";
        std::fs::write(p, &buf).unwrap();

        let report = verify(p).unwrap();
        assert!(!report.all_ok(), "a flipped byte must fail verify");
        let k = report.checks.iter().find(|c| c.name == MEMBER_KERNEL).unwrap();
        assert!(!k.ok);
        assert_ne!(k.actual, k.expected);

        // The run-load path catches it too.
        assert!(read_for_run(p).unwrap().verify_members().is_err());
    }

    #[test]
    fn canonical_json_is_stable_regardless_of_input_order() {
        let ordered = r#"{"format_version":1,"stack":"s","project":"p",
            "anchors":{"cpuid_sha256":"h","cpuid_profile":"prof",
              "compose_engine":{"version":"v","sha256":"e"},
              "images":[],"toolchain":{},"ram_estimate_mib":10},
            "run_defaults":{"mem_mib":3072,"cmdline":"c","fast_forward":true,"max_virtual_time":null}}"#;
        let shuffled = r#"{"project":"p","stack":"s",
            "run_defaults":{"cmdline":"c","fast_forward":true,"mem_mib":3072},
            "anchors":{"ram_estimate_mib":10,"compose_engine":{"sha256":"e","version":"v"},
              "cpuid_profile":"prof","cpuid_sha256":"h"},
            "format_version":1}"#;
        let a = Manifest::from_bytes(ordered.as_bytes()).unwrap().to_canonical_json().unwrap();
        let b = Manifest::from_bytes(shuffled.as_bytes()).unwrap().to_canonical_json().unwrap();
        assert_eq!(a, b, "canonical JSON must not depend on input key order");
    }

    #[test]
    fn rejects_unsupported_format_version() {
        let m = r#"{"format_version":999,"stack":"s","project":"p",
            "anchors":{"cpuid_sha256":"h","cpuid_profile":"p",
              "compose_engine":{"version":"v","sha256":"e"},"ram_estimate_mib":10},
            "run_defaults":{"mem_mib":1,"cmdline":"c","fast_forward":false}}"#;
        let err = Manifest::from_bytes(m.as_bytes()).unwrap_err().to_string();
        assert!(err.contains("format_version"), "got: {err}");
    }
}
