//! The compose pipeline for `tdvmm build`: the boundary between an untrusted,
//! user-authored `compose.yml` and the closed-world guest.
//!
//! It does two jobs:
//!
//!   * [`validate`] — parse a `compose.yml`, enforce the supported subset, and reject
//!     everything outside it with loud `TDVMM_BAKE_REJECT:` diagnostics (published
//!     ports warn and are stripped). Returns the images to bake, the host-side
//!     `build:` contexts, the relative binds to materialize, and any warnings.
//!
//!   * [`emit_lock`] — given the resolved image digests, the in-guest bind base, and
//!     the pinned project name ([`EmitLockRequest`]), transform the document into the
//!     deterministic `compose.lock.yml` and the bind copy-manifest.
//!
//! ## Byte-identical YAML
//!
//! `compose.lock.yml` is embedded in both the initramfs and the `.tdvmm`, and its
//! bytes are a byte-identity acceptance gate against the retired Python producer,
//! which emitted the lock with `yaml.safe_dump(sort_keys=True,
//! default_flow_style=False)`. libyaml-based crates do not reproduce PyYAML's exact
//! wrapping and quoting, so [`yaml_emitter`] hand-ports the emitter and reproduces its
//! output byte-for-byte. Parsing stays on `serde_yaml`; emission is ours.
//!
//! ## The gate is total
//!
//! Any input that passes [`validate`] can be emitted without a panic or a silent
//! divergence: validation walks the whole document and rejects the two shapes the
//! emitter cannot faithfully render — custom YAML tags (no plain scalar rendering) and
//! floats whose magnitude forces scientific notation — and rejects non-string mapping
//! keys rather than coercing them. No `unwrap`/`expect`/`panic` runs on the
//! validate-or-emit path over untrusted input.
//!
//! ## Errors and exit codes
//!
//! Both entry points fail with [`ValidateError`], whose variants the CLI maps to the
//! process exit codes the reject gate promises: a [`Reject`](ValidateError::Reject) is
//! exit 3 (the loud out-of-subset gate), an [`Io`](ValidateError::Io) or
//! [`Internal`](ValidateError::Internal) is exit 2.

mod emit;
mod error;
mod validate;
mod yaml_emitter;

pub use emit::{emit_lock, EmitLockRequest};
pub use error::ValidateError;
pub use validate::{validate, BuildCtx, Validated};

/// Extract the compose service names from a `compose.lock.yml` byte buffer (the
/// keys under `services:`). The run-time reader for the artifact's embedded lock,
/// used to populate `--logs-dir` capture.
///
/// # Errors
///
/// [`ValidateError::Reject`] if the lock does not parse or declares no services.
pub fn lock_service_names(compose_lock: &[u8]) -> Result<Vec<String>, ValidateError> {
    let doc: serde_yaml::Value = serde_yaml::from_slice(compose_lock)
        .map_err(|e| error::reject(format!("parsing compose.lock.yml: {e}")))?;
    let mut names: Vec<String> = doc
        .get("services")
        .and_then(|v| v.as_mapping())
        .map(|m| m.keys().filter_map(|k| k.as_str().map(String::from)).collect())
        .unwrap_or_default();
    if names.is_empty() {
        return Err(error::reject("no services found in compose.lock.yml"));
    }
    // Sorted so every consumer (log capture, the console scanner) sees a
    // deterministic order.
    names.sort();
    Ok(names)
}

/// The diagnostic prefix on a hard rejection (out-of-subset compose); the CLI exits 3.
pub const REJECT: &str = "TDVMM_BAKE_REJECT";
/// The diagnostic prefix on a non-fatal warning (e.g. stripped published ports).
pub const WARN: &str = "TDVMM_BAKE_WARN";

#[cfg(test)]
mod tests {
    use super::*;
    use super::emit::{CONTROL_DIR, EVENT_FIFO};

    #[test]
    fn lock_service_names_reads_the_artifacts_lock() {
        let lock = b"services:\n  api: {}\n  db: {}\n";
        assert_eq!(lock_service_names(lock).unwrap(), vec!["api", "db"]);
        assert!(lock_service_names(b"services: {}\n").is_err());
        assert!(lock_service_names(b"not: yaml: [").is_err());
    }

    use serde_yaml::Value;
    use std::collections::HashMap;
    use std::path::Path;

    /// End-to-end: parse the ORIGINAL compose.yml, run the emit-lock transforms with
    /// the known pinned digests, and require the output to equal the committed
    /// compose.lock.yml byte-for-byte. Validates parse + transform + emit together
    /// across the whole pipeline (the image-based corpus stacks).
    #[test]
    fn emit_lock_matches_committed_locks() {
        const PG: &str =
            "docker.io/library/postgres@sha256:57c72fd2a128e416c7fcc499958864df5301e940bca0a56f58fddf30ffc07777";
        const PG_PIN: &str =
            "localhost/tdvmm-postgres-57c72fd2a128@sha256:cbf217007d0742829dc120c3ea9cd2621e90eb3adfeaf6684e87ce268a2ca368";
        for stack in ["faultlab", "svcchain", "configpipeline"] {
            let compose_path = format!("testdata/stacks/{stack}/compose.yml");
            let raw = std::fs::read_to_string(&compose_path).unwrap();
            let doc: Value = serde_yaml::from_str(&raw).unwrap();
            let mut digests: HashMap<String, String> = HashMap::new();
            digests.insert(PG.into(), PG_PIN.into());
            // configpipeline: a build: service (worker) pinned by its output tag.
            digests.insert(
                "localhost/tdvmm-configpipeline-worker:corpus".into(),
                "localhost/tdvmm-configpipeline-worker@sha256:c05937c63df870e5f337543187d64a50975ec54794df45ed06e4182348dc4422".into(),
            );
            let project = format!("tdvmm_{}", stack.replace('-', "_"));
            let out = emit_lock(EmitLockRequest {
                doc: &doc,
                compose_path: Path::new(&compose_path),
                digests: &digests,
                binds_base: "/var/lib/tdvmm-stack/binds",
                project: &project,
            })
            .unwrap_or_else(|e| panic!("emit_lock {stack}: {e}"));
            let got = String::from_utf8_lossy(&out.lock_yaml).into_owned();

            // Every service carries BOTH harness binds: the schema-3 event FIFO
            // (fire-and-forget assertions) and the schema-4 control-socket
            // directory (drive the harness / end the run).
            let locked: Value = serde_yaml::from_str(&got).unwrap();
            let services = locked.get("services").and_then(|v| v.as_mapping()).unwrap();
            for (want, what) in [
                (format!("{EVENT_FIFO}:{EVENT_FIFO}:rw"), "event FIFO"),
                (format!("{CONTROL_DIR}:{CONTROL_DIR}:rw"), "control socket"),
            ] {
                for (name, cfg) in services {
                    let present = cfg
                        .get("volumes")
                        .and_then(|v| v.as_sequence())
                        .map(|vs| vs.iter().any(|e| e.as_str() == Some(&want)))
                        .unwrap_or(false);
                    assert!(present, "{stack}: service {name:?} missing the {what} bind");
                }
            }

            // The committed lock is a generated file; TDVMM_REGEN_LOCKS=1 rewrites it
            // (mirrors the proto-goldens regen).
            let lock_path = format!("testdata/stacks/{stack}/compose.lock.yml");
            if std::env::var("TDVMM_REGEN_LOCKS").is_ok() {
                std::fs::write(&lock_path, got.as_bytes()).unwrap();
                continue;
            }
            let want = std::fs::read_to_string(&lock_path).unwrap();
            assert_eq!(got, want, "emit_lock mismatch for {stack}");
        }
    }
}
