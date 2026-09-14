// Shared types for the history layer. Defined outside the ssr-only
// db module so the WASM client can deserialize the response of
// GET /api/irrigation/history.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunRecord {
    /// Durable watering job identity. None on legacy/unattributable rows.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Zone slug: back_yard, front_yard, side_yard, back_yard_shrubs.
    pub zone: String,
    /// UTC epoch the run started.
    pub start_epoch: i64,
    /// Run duration in seconds.
    pub duration_s: i64,
    /// Skip reason if this row represents a skip event rather than a
    /// completed run. None for actual runs.
    pub skip_reason: Option<String>,
    /// Row source ('ha_refresher' | 'manual' | 'manual:<id>' |
    /// 'smart_morning' | 'dry_run' | ...). Additive: empty on payloads
    /// from older builds; `history::rollup::is_watering_record` falls
    /// back to the skip_reason test then.
    #[serde(default)]
    pub source: String,
    /// Row status ('completed' | 'skipped' | 'aborted' | ...). Additive;
    /// empty on payloads from older builds.
    #[serde(default)]
    pub status: String,
    /// The controller the row belongs to. Additive; None on payloads
    /// from older builds and on rows written before the observer knew.
    #[serde(default)]
    pub controller_id: Option<String>,
    /// Gross depth applied at the head, mm: duration x throughput.
    /// Additive; None when the throughput was unknown at insert.
    #[serde(default)]
    pub applied_mm: Option<f64>,
    /// Metered volume, when the controller has a flow meter. Additive.
    #[serde(default)]
    pub volume_gal: Option<f64>,
    /// Why the row ended early, when it did ("ended by restart"). Additive.
    #[serde(default)]
    pub note: Option<String>,
    /// Which segment of a cycle-and-soak plan this row is. Additive.
    #[serde(default)]
    pub cycle_index: Option<u32>,
    #[serde(default)]
    pub cycle_count: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryWindow {
    /// Start of the window in UTC epoch (inclusive).
    pub from_epoch: i64,
    /// End of the window in UTC epoch (exclusive).
    pub to_epoch: i64,
    pub runs: Vec<RunRecord>,
    /// Scheduled morning decisions, including mornings that needed no water.
    /// Run records remain the authority for water actually delivered.
    #[serde(default)]
    pub daily: Vec<DailyDecision>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyDecision {
    pub date_local: String,
    pub epoch: i64,
    /// scheduled | scheduled_legacy | missed_window | recorded_decision.
    pub kind: String,
    pub zones: Vec<DailyZoneDecision>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyZoneDecision {
    pub zone: String,
    pub name: String,
    pub planned_seconds: u32,
    pub reason_code: String,
    pub reason: String,
    pub water_need: String,
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
    /// Full structured skip-ladder trace captured at decision time, if one
    /// was stored (M0007+). None for legacy rows. Powers the Rule Lab's
    /// historical view.
    #[serde(default)]
    pub trace: Option<crate::model::DecisionTrace>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionWindow {
    /// Start of the window in UTC epoch (inclusive).
    pub from_epoch: i64,
    /// End of the window in UTC epoch (exclusive).
    pub to_epoch: i64,
    pub decisions: Vec<DecisionRecord>,
}

// ---- Per-zone tuning report (GET /api/v1/irrigation/tuning) ----
//
// Wire types live here (not in engine::tuning, which is ssr-only) so the
// WASM client can deserialize the report, following the RunRecord /
// DecisionRecord precedent above. The pure rules that PRODUCE these live
// in src/engine/tuning.rs; null is the documented unknown value on every
// Option field (never a 0 sentinel).

/// Results-based tuning report: a 7 to 30 day window of outcomes reduced
/// to at most one plain-language recommendation per zone, plus one
/// install-wide forecast-skip scorecard line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TuningReport {
    /// UTC epoch this report was generated.
    pub generated_epoch: i64,
    /// The observation window the per-zone checks read (days).
    pub window_days: u32,
    pub zones: Vec<ZoneTuning>,
    pub scorecard: TuningScorecard,
}

/// One zone's tuning read-out.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneTuning {
    pub slug: String,
    pub display_name: String,
    /// "recommendation" | "ok" | "insufficient_data".
    pub status: String,
    /// Plain informational lines: the watering-cadence line, the
    /// probe-unlocks-more note on probe-less zones, and each check's
    /// specific not-enough-data state (honest-unknowns register).
    pub lines: Vec<String>,
    /// At most one recommendation per zone (priority-ranked server-side).
    #[serde(default)]
    pub recommendation: Option<TuningRecommendation>,
    /// True when the operator dismissed or snoozed at least one of this
    /// zone's suggestions. A silenced suggestion is stripped inside the
    /// ranked pick server-side (no pill, no count, no push for it), so
    /// the NEXT-ranked non-silenced suggestion may still appear in
    /// `recommendation`; the last entry of `lines` is the muted
    /// annotation the UI renders with Undo. Additive (1.21.0).
    #[serde(default)]
    pub dismissed: bool,
    /// The ZoneConfig field names currently silenced for this zone
    /// (the undismiss endpoint keys on zone_slug + field). Additive.
    #[serde(default)]
    pub dismissed_fields: Vec<String>,
}

/// One actionable recommendation. `field` + `suggested_value` are what the
/// Apply endpoint writes; `companion_fields` ride the same Apply (e.g. a
/// measured precipitation rate also stamps `precip_rate_source`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TuningRecommendation {
    /// Stable content hash of (zone, field, rounded suggestion) so Apply
    /// can detect that the recommendation it is acting on still derives.
    pub id: String,
    /// ZoneConfig field name the Apply writes (serde/JSON name).
    pub field: String,
    /// Current configured value as JSON; null = not set (using a default).
    #[serde(default)]
    pub current_value: serde_json::Value,
    /// Suggested value as JSON; null = clear the override (use the default).
    #[serde(default)]
    pub suggested_value: serde_json::Value,
    /// Fields written together with `field` by the same Apply.
    #[serde(default)]
    pub companion_fields: Vec<TuningCompanionField>,
    /// One plain sentence a non-expert can act on.
    pub headline: String,
    /// The evidence numbers behind the headline, one plain line each.
    pub evidence: Vec<String>,
    /// "low" | "medium" | "high".
    pub confidence: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TuningCompanionField {
    pub field: String,
    #[serde(default)]
    pub value: serde_json::Value,
}

/// Install-wide comparison of daily rain-family verdicts and observed rain.
/// Verdict history does not establish an actual automatic-run outcome.
/// Informational only (no Apply). Counts are null until at least
/// `min_scored_days` rain-family skip days could be judged.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TuningScorecard {
    /// The scorecard's own window (fixed 30 days; rain skips are sparse).
    pub window_days: u32,
    /// Rain-family skip days whose confirmation window completed with a
    /// recorded rain total. Null = not enough data yet.
    #[serde(default)]
    pub scored_days: Option<u32>,
    /// Of `scored_days`, how many the rain actually confirmed.
    #[serde(default)]
    pub confirmed_days: Option<u32>,
    /// Minimum scored days before the counts populate.
    pub min_scored_days: u32,
    /// The one plain line the UI renders.
    pub line: String,
    /// Days with a first recorded hold verdict for rain falling or on the ground
    /// (reactive codes: rain_now, observed_rain, already_wet). Counted,
    /// never confirmation-scored: those skips confirm themselves and
    /// would distort the forecast tally in both directions. Null until
    /// any such day exists. Additive field.
    #[serde(default)]
    pub reactive_days: Option<u32>,
    /// The reactive count's own line; empty when `reactive_days` is null.
    /// Additive field.
    #[serde(default)]
    pub reactive_line: String,
}

/// Recent observed weather series (oldest -> newest), one Vec per field.
/// Served by GET /api/v1/weather/history and consumed by the Weather home
/// telemetry strip for its sparklines.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WeatherHistory {
    pub air_temp_f: Vec<f64>,
    pub rh_pct: Vec<f64>,
    pub wind_avg_mph: Vec<f64>,
    pub pressure_inhg: Vec<f64>,
    pub solar_w_m2: Vec<f64>,
    pub uv_index: Vec<f64>,
}

/// A stored run as the history wire shape.
///
/// The table is the storage layer's (`persistence::runs`); this is what
/// /history has always answered with. Keeping the mapping here means the
/// endpoint's shape is decided in the module that documents it rather
/// than in a second query that happened to select the same columns.
#[cfg(feature = "ssr")]
impl From<crate::persistence::runs::RunRow> for RunRecord {
    fn from(r: crate::persistence::runs::RunRow) -> Self {
        Self {
            session_id: r.session_id,
            zone: r.zone_slug,
            start_epoch: r.start_epoch,
            // A row still running or merely intended has no duration yet;
            // the window has always shown those as zero-length until they
            // complete.
            duration_s: r.duration_s.unwrap_or(0) as i64,
            skip_reason: r.skip_reason,
            source: r.source,
            status: r.status,
            // The historical placeholder is not an attribution.
            controller_id: Some(r.controller_id).filter(|c| c != "unknown"),
            applied_mm: r.applied_mm,
            volume_gal: r.volume_gal,
            note: r.note,
            cycle_index: r.cycle_index,
            cycle_count: r.cycle_count,
        }
    }
}

/// A stored verdict transition as the decisions wire shape. A row whose
/// trace does not parse (a shape from an older build) carries no trace
/// rather than failing the window.
#[cfg(feature = "ssr")]
impl From<crate::persistence::verdict_history::VerdictRow> for DecisionRecord {
    fn from(r: crate::persistence::verdict_history::VerdictRow) -> Self {
        Self {
            epoch: r.epoch,
            verdict: r.verdict,
            reason: r.reason,
            trace: (!r.trace_json.is_empty())
                .then(|| serde_json::from_str(&r.trace_json).ok())
                .flatten(),
        }
    }
}
