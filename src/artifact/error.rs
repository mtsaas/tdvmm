//! [`ArtifactError`] — the error type for every `.tdvmm` operation.
//!
//! Four categories, each carrying a `source()` where one exists so callers can
//! walk the chain or match on the kind. The I/O and manifest variants are built
//! through context-attaching helpers ([`ArtifactError::io`],
//! [`ArtifactError::manifest`]), so every error names the operation that produced
//! it.

use std::fmt;

#[derive(Debug)]
pub enum ArtifactError {
    /// An I/O failure, tagged with the operation (e.g. `"opening /…/x.tdvmm"`).
    /// The underlying [`std::io::Error`] is kept as the `source`.
    Io {
        what: String,
        source: std::io::Error,
    },
    /// `manifest.json` failed to (de)serialize; the `serde_json::Error` is the
    /// `source`.
    Manifest {
        what: &'static str,
        source: serde_json::Error,
    },
    /// A structurally invalid artifact: bad tar magic/checksum, a truncated or
    /// oversized member, a missing required member, an unsupported `format_version`,
    /// or a member-hash mismatch.
    Malformed(String),
    /// A store-name / path lookup that matched nothing: a user lookup error, not
    /// corrupt data. Distinct from [`Malformed`](ArtifactError::Malformed) so a
    /// caller can tell a lookup miss from a corrupt artifact.
    NoSuchArtifact(String),
}

impl ArtifactError {
    /// An [`Io`](ArtifactError::Io) with `what` context attached.
    pub(crate) fn io(what: impl Into<String>, source: std::io::Error) -> Self {
        ArtifactError::Io { what: what.into(), source }
    }
    /// A [`Manifest`](ArtifactError::Manifest) wrapping a serde failure.
    pub(crate) fn manifest(what: &'static str, source: serde_json::Error) -> Self {
        ArtifactError::Manifest { what, source }
    }
    /// A [`Malformed`](ArtifactError::Malformed) from any displayable message.
    pub(crate) fn malformed(msg: impl Into<String>) -> Self {
        ArtifactError::Malformed(msg.into())
    }
    /// A [`NoSuchArtifact`](ArtifactError::NoSuchArtifact) from any message.
    pub(crate) fn no_such(msg: impl Into<String>) -> Self {
        ArtifactError::NoSuchArtifact(msg.into())
    }
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtifactError::Io { what, source } => write!(f, "{what}: {source}"),
            ArtifactError::Manifest { what, source } => write!(f, "{what}: {source}"),
            ArtifactError::Malformed(msg) | ArtifactError::NoSuchArtifact(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ArtifactError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArtifactError::Io { source, .. } => Some(source),
            ArtifactError::Manifest { source, .. } => Some(source),
            ArtifactError::Malformed(_) | ArtifactError::NoSuchArtifact(_) => None,
        }
    }
}
