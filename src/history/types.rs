// Shared types for the history layer. Defined outside the ssr-only
// db module so the WASM client can deserialize the response of
// GET /api/irrigation/history.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Zone slug: back_yard, front_yard, side_yard, back_yard_shrubs.
    pub zone: String,
    /// UTC epoch the run started.
    pub start_epoch: i64,
    /// Run duration in seconds.
    pub duration_s: i64,
    /// Skip reason if this row represents a skip event rather than a
    /// completed run. None for actual runs.
    pub skip_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryWindow {
    /// Start of the window in UTC epoch (inclusive).
    pub from_epoch: i64,
    /// End of the window in UTC epoch (exclusive).
    pub to_epoch: i64,
    pub runs: Vec<RunRecord>,
}

/// One row per verdict transition: written when the skip-check engine's
/// verdict string changes (e.g. "run" -> "skip"). Lets the dashboard answer
/// "did we actually skip on day X, and why" weeks later, instead of having
/// to scroll back through HA logbook.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// UTC epoch the verdict transitioned to this value.
    pub epoch: i64,
    /// "run" | "skip" | "run_extended" | "unknown".
    pub verdict: String,
    /// Human-readable reason from skip_logic::evaluate. Empty when verdict == "run".
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionWindow {
    /// Start of the window in UTC epoch (inclusive).
    pub from_epoch: i64,
    /// End of the window in UTC epoch (exclusive).
    pub to_epoch: i64,
    pub decisions: Vec<DecisionRecord>,
}
