//! Run metadata plus the JSON report structs — the STABLE, shared contract the
//! e2e runner consumes. `Report`/`StepReport` serialize in declaration order,
//! which IS the report's key order, so field order here is load-bearing.

use serde::Serialize;

use super::ledger::AssertionSummary;

pub struct RunMeta {
    pub stack: String,
    pub artifact_sha256: String,
    pub fast_forward: bool,
    /// Whether host-mediated egress was opened for this run. Surfaced in the JSON
    /// report so a run that touched the network is self-identifying.
    pub egress: bool,
    pub jsonl_path: String,
    pub report_path: String,
}

/// `skip_serializing_if` helper: a `false` egress flag is omitted, so a
/// closed-world run's report is byte-for-byte unchanged from before this feature.
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Default, Serialize)]
pub struct FfSummary {
    pub jumps: u64,
    pub speedup: f64,
    pub virtual_seconds: f64,
    pub per_hop_mean_us: f64,
    pub max_delta_s: f64,
}

#[derive(Serialize)]
pub(super) struct Report {
    pub(super) schema: u32,
    pub(super) verdict: String,
    pub(super) exit_code: i32,
    pub(super) stack: String,
    pub(super) artifact_sha256: String,
    pub(super) scenario: String,
    pub(super) scenario_sha256: String,
    pub(super) fast_forward: bool,
    /// Egress opened for this run. Omitted when false (closed-world runs keep
    /// their pre-feature report bytes); present as `true` only when egress was on.
    #[serde(skip_serializing_if = "is_false")]
    pub(super) egress: bool,
    pub(super) duration_wall_s: f64,
    pub(super) virtual_seconds: f64,
    pub(super) ff: FfSummary,
    pub(super) steps_total: usize,
    pub(super) steps_passed: usize,
    pub(super) steps: Vec<StepReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) failure: Option<String>,
    /// Guest-assertion summary (schema 3+). Omitted when no guest events were seen,
    /// so an event-free run's report is byte-for-byte unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) assertions: Option<AssertionSummary>,
}

#[derive(Clone, Serialize)]
pub(super) struct StepReport {
    pub(super) index: usize,
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) at_s: f64,
    pub(super) outcome: String, // "pass" | "fail" | "error" | "skipped"
    pub(super) detail: String,
}
