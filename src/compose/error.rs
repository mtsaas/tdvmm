//! [`ValidateError`] — the error type for the compose validate + emit-lock pipeline.
//!
//! Three modes, mapped to the process exit codes the CLI boundary expects. A
//! [`Reject`](ValidateError::Reject) is a user-facing out-of-subset compose (exit 3,
//! the loud `TDVMM_BAKE_REJECT:` gate); an [`Io`](ValidateError::Io) is a filesystem
//! failure that keeps the underlying [`std::io::Error`] as its `source`; an
//! [`Internal`](ValidateError::Internal) is a pipeline invariant that a prior
//! `validate` should already have guaranteed. Io and Internal both surface as exit 2.
//! Values are built through the context-attaching constructors ([`reject`], [`io`],
//! [`internal`]) so the exit-code decision stays a match on the variant, not a magic
//! number carried in the struct.

use std::fmt;

#[derive(Debug)]
pub enum ValidateError {
    /// The compose is outside the supported subset — the loud, user-facing reject.
    /// The message is a full sentence explaining what to change; the CLI prefixes it
    /// with `TDVMM_BAKE_REJECT:` and exits 3.
    Reject(String),
    /// Reading a file needed to validate failed (e.g. a service's Dockerfile). `what`
    /// names the operation; the underlying [`std::io::Error`] is the `source`.
    Io { what: String, source: std::io::Error },
    /// A pipeline invariant was violated — a shape `validate` should already have
    /// enforced, or a digest the bake promised to supply but did not. Not a user
    /// error in the compose subset.
    Internal(String),
}

/// A [`Reject`](ValidateError::Reject) from any displayable message.
pub(super) fn reject(msg: impl Into<String>) -> ValidateError {
    ValidateError::Reject(msg.into())
}

/// An [`Io`](ValidateError::Io) with `what` context attached.
pub(super) fn io(what: impl Into<String>, source: std::io::Error) -> ValidateError {
    ValidateError::Io { what: what.into(), source }
}

/// An [`Internal`](ValidateError::Internal) from any displayable message.
pub(super) fn internal(msg: impl Into<String>) -> ValidateError {
    ValidateError::Internal(msg.into())
}

impl fmt::Display for ValidateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidateError::Reject(msg) | ValidateError::Internal(msg) => write!(f, "{msg}"),
            ValidateError::Io { what, source } => write!(f, "{what}: {source}"),
        }
    }
}

impl std::error::Error for ValidateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ValidateError::Io { source, .. } => Some(source),
            ValidateError::Reject(_) | ValidateError::Internal(_) => None,
        }
    }
}
