// Results-based tuning: pure rules that read a window of outcomes (runs,
// probe readings, rain observations, live clamp state) and emit at most
// one plain-language recommendation per zone, plus one install-wide
// forecast-skip scorecard. skip_rules.rs style: typed inputs in, typed
// outcomes out, no I/O, no async, deterministic strings. The thin
// assembly layer that gathers inputs from the stores lives in
// src/tuning.rs; the serde wire types live in crate::history::types so
// the WASM client can deserialize the report (engine is ssr-only).
//
// Contracts the checks honor:
//   * Probe % is a RELATIVE calibration scale (soil_forecast.rs KNOWN
//     LIMITATION #4b): the drift check compares SLOPES only, never
//     absolute levels.
//   * Honest unknowns: every check that lacks data reports its specific
//     insufficiency; counts stay null until they are real.
//   * One knob per recommendation; ranking is cap > drift > backout >
//     interval, and the backout check is only consulted when the drift
//     check did not flag the zone in the same report.

use std::collections::BTreeMap;

use chrono::NaiveDate;
use serde_json::{json, Value};

use crate::config::schema::{GrassSpecies, SoilTexture};
use crate::engine::soil_catalog;
use crate::engine::species_catalog;
use crate::history::types::{
    TuningCompanionField, TuningRecommendation, TuningScorecard, ZoneTuning,
};

/// Report window bounds (days). The API clamps ?days into this range.
pub const MIN_WINDOW_DAYS: u32 = 7;
pub const MAX_WINDOW_DAYS: u32 = 30;
pub const DEFAULT_WINDOW_DAYS: u32 = 14;

/// Scorecard thresholds. WET/SIG mirror `assess_day` in
/// persistence::verdict_history (a test there pins the equality) so the
/// tuning scorecard and the accuracy scoreboard can never disagree on
/// what counts as rain.
pub const WET_IN: f64 = 0.05;
pub const SIG_IN: f64 = 0.10;
/// A local day with at least this much observed rain breaks a dry stretch
/// and disqualifies a backout event.
pub const RAIN_DAY_IN: f64 = 0.02;
/// The scorecard reads its own fixed window: rain-family skips are sparse
/// and a 7-day report window would rarely reach the minimum sample.
pub const SCORECARD_WINDOW_DAYS: u32 = 30;
/// Scored rain-skip days required before the scorecard states a tally.
pub const SCORECARD_MIN_SCORED: u32 = 3;

/// Same-morning run rows closer than this are one irrigation event
/// (cycle/soak + interleave produce several short observer rows).
pub const EVENT_CLUSTER_GAP_S: i64 = 2 * 3600;

/// Drift check gates.
pub const DRIFT_MIN_STRETCH_S: i64 = 48 * 3600;
pub const DRIFT_MIN_STRETCHES: usize = 2;
pub const DRIFT_MIN_READINGS: usize = 8;
pub const DRIFT_RATIO_HIGH: f64 = 1.6;
pub const DRIFT_RATIO_LOW: f64 = 0.6;

/// Backout check gates.
pub const BACKOUT_SETTLE_MIN_S: i64 = 45 * 60;
pub const BACKOUT_SETTLE_MAX_S: i64 = 3 * 3600;
pub const BACKOUT_MIN_EVENTS: usize = 3;
pub const BACKOUT_TOLERANCE: f64 = 0.30;

/// Interval plausibility bounds (days). Catches gross mis-config only.
pub const INTERVAL_MIN_DAYS: f64 = 1.5;
pub const INTERVAL_MAX_DAYS: f64 = 21.0;

/// Cap check: run days in the window required before "chronically" applies.
pub const CAP_MIN_RUN_DAYS: u32 = 3;

/// One completed watering interval (already filtered to watering evidence
/// rows: observer + manual completed rows, never skip markers).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RunSegment {
    pub start_epoch: i64,
    pub end_epoch: i64,
}

/// A same-morning cluster of run segments: one irrigation event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IrrigationEvent {
    /// First segment's start.
    pub start_epoch: i64,
    /// Last segment's end.
    pub end_epoch: i64,
    /// Sum of valve-open time across the clustered segments (seconds).
    pub valve_open_s: i64,
}

/// Outcome of one check for one zone.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckOutcome {
    /// The check fired and produced an actionable recommendation.
    Recommend(TuningRecommendation),
    /// The check could not be evaluated; the line states exactly why.
    Insufficient(String),
    /// The check evaluated cleanly and found nothing to flag.
    Pass,
}

// ---------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------

/// Stable content id for a recommendation: FNV-1a 64 over
/// (zone, field, suggested value). Apply regenerates the recommendation
/// server-side and compares ids, so a stale Apply (config or data moved
/// underneath the open page) is refused instead of writing a value the
/// evidence no longer supports.
pub fn recommendation_id(slug: &str, field: &str, suggested: &Value) -> String {
    let canon = format!("{slug}|{field}|{suggested}");
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in canon.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Median of a set of samples. `None` when empty. Shared by the drift
/// and backout checks and by the assembly's probe-scale cross-check so
/// every consumer summarizes the same way.
pub fn median_of(mut samples: Vec<f64>) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
    let n = samples.len();
    Some(if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2.0
    })
}

/// How closely the drift and backout ratios must agree (max/min) for the
/// probe-scale cross-check to suspect the probe rather than the config.
pub const CROSSCHECK_AGREEMENT: f64 = 1.25;

/// Probe-scale cross-check: a pure probe scale error (the sensor's 0 to
/// 100% spanning more or less water than the configured bucket) inflates
/// the drift ratio (measured/modeled drying) and the backout ratio
/// (median derived rate / configured rate) by the SAME factor in the
/// SAME direction. When both ratios exist, sit on the same side of 1.0,
/// and agree within `CROSSCHECK_AGREEMENT`, the probe's calibration is
/// the likelier single cause and neither check should recommend a config
/// write. Returns the shared insufficiency line in that case.
pub fn probe_scale_crosscheck(
    drift_ratio: Option<f64>,
    backout_ratio: Option<f64>,
) -> Option<String> {
    let (c, d) = (drift_ratio?, backout_ratio?);
    if !(c.is_finite() && d.is_finite() && c > 0.0 && d > 0.0) {
        return None;
    }
    let same_direction = (c > 1.0 && d > 1.0) || (c < 1.0 && d < 1.0);
    if !same_direction {
        return None;
    }
    let agreement = (c / d).max(d / c);
    if agreement > CROSSCHECK_AGREEMENT {
        return None;
    }
    let factor = (c + d) / 2.0;
    Some(format!(
        "Probe calibration suspected: drying and delivery both read about {factor:.1}x off \
         in the same direction, which a probe scale error alone produces. Calibrate the \
         probe before applying texture or rate changes."
    ))
}

/// Least-squares slope of a time series in value-units per DAY. `None`
/// when fewer than 2 readings or all readings share one epoch.
pub fn slope_per_day(readings: &[(i64, f64)]) -> Option<f64> {
    if readings.len() < 2 {
        return None;
    }
    let n = readings.len() as f64;
    let t0 = readings[0].0;
    let mean_t: f64 = readings.iter().map(|(e, _)| (*e - t0) as f64).sum::<f64>() / n;
    let mean_v: f64 = readings.iter().map(|(_, v)| *v).sum::<f64>() / n;
    let mut num = 0.0;
    let mut den = 0.0;
    for (e, v) in readings {
        let dt = (*e - t0) as f64 - mean_t;
        num += dt * (*v - mean_v);
        den += dt * dt;
    }
    if den <= 0.0 {
        return None;
    }
    Some(num / den * 86_400.0)
}

/// Cluster watering segments into same-morning irrigation events.
/// Segments closer than `EVENT_CLUSTER_GAP_S` (measured from the previous
/// segment's end to the next segment's start) merge into one event.
///
/// valve_open_s is the interval-UNION coverage of the cluster, not a raw
/// sum: a manual run is persisted twice (the manual completed row plus
/// the run-edge observer's row for the same physical valve activity),
/// and summing both would double the minutes and halve every backed-out
/// rate. Segments are sorted by start, so counting only the portion past
/// the cluster's current end is exactly the union length; disjoint
/// cycle/soak observer segments still sum as before.
pub fn cluster_events(segments: &[RunSegment]) -> Vec<IrrigationEvent> {
    let mut segs: Vec<RunSegment> = segments
        .iter()
        .copied()
        .filter(|s| s.end_epoch >= s.start_epoch)
        .collect();
    segs.sort_by_key(|s| s.start_epoch);
    let mut events: Vec<IrrigationEvent> = Vec::new();
    for s in segs {
        match events.last_mut() {
            Some(ev) if s.start_epoch - ev.end_epoch <= EVENT_CLUSTER_GAP_S => {
                ev.valve_open_s += (s.end_epoch - s.start_epoch.max(ev.end_epoch)).max(0);
                ev.end_epoch = ev.end_epoch.max(s.end_epoch);
            }
            _ => events.push(IrrigationEvent {
                start_epoch: s.start_epoch,
                end_epoch: s.end_epoch,
                valve_open_s: s.end_epoch - s.start_epoch,
            }),
        }
    }
    events
}

/// Maximal sub-intervals of `[window_start, window_end)` not covered by
/// any `busy` interval, at least `min_len_s` long. `busy` holds both
/// irrigation-event intervals (padded by the caller so post-run settle
/// time never reads as drying) and wet-day intervals.
pub fn dry_stretches(
    window_start: i64,
    window_end: i64,
    busy: &[(i64, i64)],
    min_len_s: i64,
) -> Vec<(i64, i64)> {
    let mut spans: Vec<(i64, i64)> = busy
        .iter()
        .copied()
        .filter(|(a, b)| *b > window_start && *a < window_end)
        .map(|(a, b)| (a.max(window_start), b.min(window_end)))
        .collect();
    spans.sort();
    let mut out = Vec::new();
    let mut cursor = window_start;
    for (a, b) in spans {
        if a > cursor && a - cursor >= min_len_s {
            out.push((cursor, a));
        }
        cursor = cursor.max(b);
    }
    if window_end > cursor && window_end - cursor >= min_len_s {
        out.push((cursor, window_end));
    }
    out
}

/// Resolve the zone's root depth and MAD in the SAME override order
/// `water_balance::summarize` uses: explicit override, else the species
/// default.
pub fn resolve_root_mad(
    species: GrassSpecies,
    root_depth_mm_override: Option<f64>,
    mad_pct_override: Option<f64>,
) -> (f64, f64) {
    let profile = species_catalog::lookup(species);
    (
        root_depth_mm_override.unwrap_or(profile.root_depth_mm),
        mad_pct_override.unwrap_or(profile.mad_pct),
    )
}

/// USDA texture ladder in the config enum's order, for one-step moves.
pub const TEXTURE_ORDER: [SoilTexture; 7] = [
    SoilTexture::Sand,
    SoilTexture::LoamySand,
    SoilTexture::SandyLoam,
    SoilTexture::Loam,
    SoilTexture::SiltLoam,
    SoilTexture::ClayLoam,
    SoilTexture::Clay,
];

fn aw_per_mm(t: SoilTexture) -> f64 {
    let p = soil_catalog::lookup(t);
    p.field_capacity - p.wilting_point
}

/// The ADJACENT texture (one ladder step) whose available water moves in
/// the wanted direction. When both neighbors qualify (the ladder's AW is
/// not monotone around loam), the smaller AW move wins so the suggestion
/// stays a one-step correction. `None` when neither neighbor moves AW
/// the right way.
pub fn adjacent_texture_toward(current: SoilTexture, want_higher_aw: bool) -> Option<SoilTexture> {
    let idx = TEXTURE_ORDER.iter().position(|t| *t == current)?;
    let cur_aw = aw_per_mm(current);
    let mut best: Option<(f64, SoilTexture)> = None;
    for cand_idx in [idx.checked_sub(1), idx.checked_add(1)] {
        let Some(ci) = cand_idx else { continue };
        let Some(t) = TEXTURE_ORDER.get(ci) else {
            continue;
        };
        let aw = aw_per_mm(*t);
        let qualifies = if want_higher_aw {
            aw > cur_aw
        } else {
            aw < cur_aw
        };
        if !qualifies {
            continue;
        }
        let dist = (aw - cur_aw).abs();
        if best.map(|(d, _)| dist < d).unwrap_or(true) {
            best = Some((dist, *t));
        }
    }
    best.map(|(_, t)| t)
}

/// serde snake_case slug for a texture, matching the config wire format.
pub fn texture_slug(t: SoilTexture) -> &'static str {
    match t {
        SoilTexture::Sand => "sand",
        SoilTexture::LoamySand => "loamy_sand",
        SoilTexture::SandyLoam => "sandy_loam",
        SoilTexture::Loam => "loam",
        SoilTexture::SiltLoam => "silt_loam",
        SoilTexture::ClayLoam => "clay_loam",
        SoilTexture::Clay => "clay",
    }
}

fn texture_label(t: SoilTexture) -> &'static str {
    match t {
        SoilTexture::Sand => "sand",
        SoilTexture::LoamySand => "loamy sand",
        SoilTexture::SandyLoam => "sandy loam",
        SoilTexture::Loam => "loam",
        SoilTexture::SiltLoam => "silt loam",
        SoilTexture::ClayLoam => "clay loam",
        SoilTexture::Clay => "clay",
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

// ---------------------------------------------------------------------
// Check A: cap-clamped runs
// ---------------------------------------------------------------------

/// Inputs for the cap-clamp check. Live clamp state comes from the
/// current snapshot (v1 contract: current config's clamp state plus run
/// frequency over the window).
///
/// The two clamp signals are deliberately separate because their
/// remedies differ: `session_capped` (the weekly allocator's per-session
/// slice exceeds the cap) is fixable by sessions/budget, while
/// `deficit_cap_binding` (ZoneMath.raw_seconds, a ONE-SHOT soil-deficit
/// refill, exceeds the cap) is not: neither sessions_per_week nor
/// weekly_budget_in feeds the deficit chain, so that state only earns an
/// informational line naming the real knob (the per-run duration cap).
#[derive(Debug, Clone, Default)]
pub struct CapClampInputs {
    /// WaterBudget.session_capped: the weekly allocator's per-session
    /// seconds exceed the cap.
    pub session_capped: bool,
    /// ZoneMath.cap_binding: the one-shot soil-deficit refill exceeds
    /// the cap.
    pub deficit_cap_binding: bool,
    /// The allocator's desired seconds per WEEKLY session before the cap
    /// (WaterBudget.seconds_per_session). Only meaningful alongside
    /// `session_capped`; never the one-shot deficit refill.
    pub desired_seconds: Option<u32>,
    /// The cap that clamps it.
    pub max_duration_s: Option<u32>,
    /// Distinct local days with completed watering in the window.
    pub run_days: u32,
    /// Effective sessions per week (config value or agronomic default).
    pub sessions_per_week: u32,
    /// Raw configured values (null on the wire when unset).
    pub configured_sessions: Option<u32>,
    pub configured_weekly_budget_in: Option<f64>,
    /// Effective weekly budget (inches) and delivery math inputs, for the
    /// budget fallback when sessions cannot go higher.
    pub weekly_budget_in: f64,
    pub throughput_mm_hr: f64,
    pub capture_efficiency: f64,
    pub heat_multiplier: f64,
}

pub fn check_cap_clamped(slug: &str, inp: &CapClampInputs) -> CheckOutcome {
    if inp.run_days == 0 {
        return CheckOutcome::Insufficient(
            "No completed runs in this window yet, so the cap check has nothing to measure."
                .to_string(),
        );
    }
    if inp.run_days < CAP_MIN_RUN_DAYS {
        return CheckOutcome::Pass;
    }
    if !inp.session_capped {
        // A one-shot deficit refill over the cap has no sessions/budget
        // remedy (those knobs never feed the deficit chain), so it earns
        // an informational line naming the real knob, never a
        // recommendation.
        if inp.deficit_cap_binding {
            return CheckOutcome::Insufficient(
                "The one-shot soil-deficit refill this zone wants exceeds its per-run \
                 duration cap, so runs are being trimmed; raising the maximum run duration \
                 is the change that would fit it. Session count and weekly budget do not \
                 shape this refill."
                    .to_string(),
            );
        }
        return CheckOutcome::Pass;
    }
    let (Some(desired), Some(max_dur)) = (inp.desired_seconds, inp.max_duration_s) else {
        return CheckOutcome::Insufficient(
            "The cap is binding but the planned-duration math is not available this refresh."
                .to_string(),
        );
    };
    if max_dur == 0 || desired <= max_dur {
        return CheckOutcome::Pass;
    }
    let sessions = inp.sessions_per_week.max(1);
    let weekly_desired_s = desired as u64 * sessions as u64;
    let sessions_needed = weekly_desired_s.div_ceil(max_dur as u64) as u32;
    let desired_min = (desired as f64 / 60.0).round();
    let cap_min = (max_dur as f64 / 60.0).round();
    let mut evidence = vec![
        format!(
            "The model wants {desired_min:.0} min per session; the {cap_min:.0} min cap trims \
             every one of them."
        ),
        format!(
            "Watered on {} day(s) in this window, each session at the cap.",
            inp.run_days
        ),
    ];
    if sessions_needed > sessions && sessions_needed <= 7 {
        evidence.push(format!(
            "{sessions_needed} sessions per week would fit the same weekly water under the cap \
             (currently {sessions})."
        ));
        let suggested = json!(sessions_needed);
        let id = recommendation_id(slug, "sessions_per_week", &suggested);
        return CheckOutcome::Recommend(TuningRecommendation {
            id,
            field: "sessions_per_week".to_string(),
            current_value: inp
                .configured_sessions
                .map(|v| json!(v))
                .unwrap_or(Value::Null),
            suggested_value: suggested,
            companion_fields: Vec::new(),
            headline: format!(
                "Split the weekly water across {sessions_needed} sessions so the \
                 {cap_min:.0} min cap stops shorting this zone."
            ),
            evidence,
            confidence: "medium".to_string(),
        });
    }
    // Sessions cannot go higher (7/week is daily). The honest remaining move
    // is aligning the weekly budget with what the system can deliver: raising
    // it can never unclamp a capped session.
    if inp.throughput_mm_hr <= 0.0 || inp.weekly_budget_in <= 0.0 {
        return CheckOutcome::Insufficient(
            "The cap is binding but the zone's delivery rate is not configured, so no budget \
             suggestion can be made."
                .to_string(),
        );
    }
    let heat = inp.heat_multiplier.max(1.0);
    let capture = inp.capture_efficiency.clamp(0.05, 1.0);
    let deliverable_week_mm =
        7.0 * (max_dur as f64 / 3600.0) * inp.throughput_mm_hr * capture / heat;
    let deliverable_week_in = round1(deliverable_week_mm / 25.4);
    if deliverable_week_in <= 0.0 || deliverable_week_in >= inp.weekly_budget_in {
        return CheckOutcome::Pass;
    }
    evidence.push(format!(
        "Even 7 capped sessions deliver at most {deliverable_week_in:.1} in per week; the \
         current target is {:.1} in.",
        inp.weekly_budget_in
    ));
    let suggested = json!(deliverable_week_in);
    let id = recommendation_id(slug, "weekly_budget_in", &suggested);
    CheckOutcome::Recommend(TuningRecommendation {
        id,
        field: "weekly_budget_in".to_string(),
        current_value: inp
            .configured_weekly_budget_in
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
        suggested_value: suggested,
        companion_fields: Vec::new(),
        headline: format!(
            "Set the weekly water target to {deliverable_week_in:.1} in, the most this zone \
             can deliver under its duration cap."
        ),
        evidence,
        confidence: "medium".to_string(),
    })
}

// ---------------------------------------------------------------------
// Check B: implausible configured interval
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct IntervalInputs {
    pub species: GrassSpecies,
    pub soil_texture: SoilTexture,
    pub root_depth_mm_override: Option<f64>,
    pub mad_pct_override: Option<f64>,
    /// Mean daily crop ET over the FORWARD forecast window (mm/day).
    /// None = no forecast data to estimate demand.
    pub mean_daily_etc_mm: Option<f64>,
}

/// Theoretical watering interval: readily available water / daily demand.
pub fn expected_interval_days(inp: &IntervalInputs) -> Option<f64> {
    let etc = inp.mean_daily_etc_mm?;
    if etc <= 0.0 {
        return None;
    }
    let (root, mad) = resolve_root_mad(
        inp.species,
        inp.root_depth_mm_override,
        inp.mad_pct_override,
    );
    let raw = soil_catalog::raw_mm(inp.soil_texture, root, mad);
    if raw <= 0.0 {
        return None;
    }
    Some(raw / etc)
}

pub fn check_interval(slug: &str, inp: &IntervalInputs) -> CheckOutcome {
    let Some(interval) = expected_interval_days(inp) else {
        return CheckOutcome::Insufficient(
            "No forecast-based water demand is available yet, so the configured bucket cannot \
             be sanity checked."
                .to_string(),
        );
    };
    if (INTERVAL_MIN_DAYS..=INTERVAL_MAX_DAYS).contains(&interval) {
        return CheckOutcome::Pass;
    }
    let too_small = interval < INTERVAL_MIN_DAYS;
    let profile = species_catalog::lookup(inp.species);
    let evidence_interval = format!(
        "With the current soil settings this zone's water bucket lasts about {interval:.1} \
         day(s) of demand; a plausible setup lands between {INTERVAL_MIN_DAYS} and \
         {INTERVAL_MAX_DAYS} days."
    );
    // Prefer restoring a root-depth override that pushes the bucket in the
    // implausible direction; otherwise take one texture step.
    if let Some(root) = inp.root_depth_mm_override {
        let default_root = profile.root_depth_mm;
        let override_explains = if too_small {
            root < default_root
        } else {
            root > default_root
        };
        if override_explains {
            let suggested = Value::Null;
            let id = recommendation_id(slug, "root_depth_mm", &suggested);
            return CheckOutcome::Recommend(TuningRecommendation {
                id,
                field: "root_depth_mm".to_string(),
                current_value: json!(root),
                suggested_value: suggested,
                companion_fields: Vec::new(),
                headline: format!(
                    "Restore the species-default root depth ({default_root:.0} mm); the \
                     {root:.0} mm override makes the water bucket implausibly {}.",
                    if too_small { "small" } else { "large" }
                ),
                evidence: vec![
                    evidence_interval,
                    format!(
                        "Root depth override {root:.0} mm vs the species default \
                         {default_root:.0} mm."
                    ),
                ],
                confidence: "medium".to_string(),
            });
        }
    }
    let Some(next) = adjacent_texture_toward(inp.soil_texture, too_small) else {
        // No one-step texture move can fix the bucket size; gross-misconfig
        // catching only, so stay quiet rather than suggest a knob that
        // cannot help.
        return CheckOutcome::Pass;
    };
    let suggested = json!(texture_slug(next));
    let id = recommendation_id(slug, "soil_texture", &suggested);
    CheckOutcome::Recommend(TuningRecommendation {
        id,
        field: "soil_texture".to_string(),
        current_value: json!(texture_slug(inp.soil_texture)),
        suggested_value: suggested,
        companion_fields: Vec::new(),
        headline: format!(
            "Try {} instead of {}; the configured soil holds an implausibly {} amount of \
             water for this zone's demand.",
            texture_label(next),
            texture_label(inp.soil_texture),
            if too_small { "small" } else { "large" }
        ),
        evidence: vec![evidence_interval],
        confidence: "medium".to_string(),
    })
}

// ---------------------------------------------------------------------
// Check C: probe-vs-model drying drift
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DriftInputs<'a> {
    /// Valid probe readings (0 < value <= 100), ascending by epoch.
    pub readings: &'a [(i64, f64)],
    /// Pre-computed dry stretches (>= 48h, no irrigation, no rain days).
    pub stretches: &'a [(i64, i64)],
    /// Modeled drying slope = mean_daily_etc / TAW * 100 (percent of
    /// bucket per day). None = no forecast demand available.
    pub modeled_slope_pct_per_day: Option<f64>,
    pub soil_texture: SoilTexture,
    pub species: GrassSpecies,
    pub root_depth_mm_override: Option<f64>,
}

/// Median measured drying rate (probe %/day, positive = drying) across
/// qualifying stretches, plus how many stretches qualified.
pub fn measured_drying_slope(
    readings: &[(i64, f64)],
    stretches: &[(i64, i64)],
) -> (Option<f64>, usize) {
    let mut rates: Vec<f64> = Vec::new();
    for (a, b) in stretches {
        let in_stretch: Vec<(i64, f64)> = readings
            .iter()
            .copied()
            .filter(|(e, _)| e >= a && e < b)
            .collect();
        if in_stretch.len() < DRIFT_MIN_READINGS {
            continue;
        }
        if let Some(slope) = slope_per_day(&in_stretch) {
            let drying = -slope;
            if drying > 0.0 {
                rates.push(drying);
            }
        }
    }
    let n = rates.len();
    (median_of(rates), n)
}

pub fn check_probe_drift(slug: &str, inp: &DriftInputs<'_>) -> CheckOutcome {
    let Some(modeled) = inp.modeled_slope_pct_per_day.filter(|m| *m > 0.0) else {
        return CheckOutcome::Insufficient(
            "No forecast-based drying estimate is available yet, so the probe cannot be \
             compared against the model."
                .to_string(),
        );
    };
    let (measured, stretch_count) = measured_drying_slope(inp.readings, inp.stretches);
    if stretch_count < DRIFT_MIN_STRETCHES {
        return CheckOutcome::Insufficient(format!(
            "Need at least {DRIFT_MIN_STRETCHES} dry stretches (48h or more with no watering \
             or rain, {DRIFT_MIN_READINGS}+ probe readings each) to judge drying; found \
             {stretch_count}."
        ));
    }
    let measured = measured.expect("stretch_count >= 1 implies a median");
    let ratio = measured / modeled;
    if (DRIFT_RATIO_LOW..=DRIFT_RATIO_HIGH).contains(&ratio) {
        return CheckOutcome::Pass;
    }
    let dries_faster = ratio > DRIFT_RATIO_HIGH;
    let evidence = vec![
        format!(
            "Probe dries at {measured:.1}% per day across {stretch_count} dry stretches; the \
             model expects {modeled:.1}% per day (ratio {ratio:.2})."
        ),
        "Slopes only: probe percent is a relative scale, so absolute levels are never \
         compared."
            .to_string(),
        "Assumes the probe's 0 to 100% span matches the configured water bucket; a probe \
         scale error of 1.6x or more alone reproduces this signal."
            .to_string(),
    ];
    // Capped at medium: the probe's percent scale is uncalibrated, and a
    // scale error alone can produce this ratio, so no stretch count earns
    // "high" here.
    let confidence = "medium";
    // A root-depth override that overstates (or understates) the bucket in
    // the drift's direction is the likelier single cause; prefer restoring
    // the species default over a texture step.
    if let Some(root) = inp.root_depth_mm_override {
        let default_root = species_catalog::lookup(inp.species).root_depth_mm;
        let override_explains = if dries_faster {
            root > default_root
        } else {
            root < default_root
        };
        if override_explains {
            let suggested = Value::Null;
            let id = recommendation_id(slug, "root_depth_mm", &suggested);
            let mut ev = evidence.clone();
            ev.push(format!(
                "Root depth override {root:.0} mm vs the species default {default_root:.0} mm."
            ));
            return CheckOutcome::Recommend(TuningRecommendation {
                id,
                field: "root_depth_mm".to_string(),
                current_value: json!(root),
                suggested_value: suggested,
                companion_fields: Vec::new(),
                headline: format!(
                    "Restore the species-default root depth ({default_root:.0} mm); the probe \
                     dries {} than the configured bucket predicts.",
                    if dries_faster { "faster" } else { "slower" }
                ),
                evidence: ev,
                confidence: confidence.to_string(),
            });
        }
    }
    // Texture step: drying faster than modeled means the real bucket is
    // smaller (want lower AW); slower means larger (want higher AW).
    let want_higher_aw = !dries_faster;
    let Some(next) = adjacent_texture_toward(inp.soil_texture, want_higher_aw) else {
        return CheckOutcome::Pass;
    };
    let suggested = json!(texture_slug(next));
    let id = recommendation_id(slug, "soil_texture", &suggested);
    CheckOutcome::Recommend(TuningRecommendation {
        id,
        field: "soil_texture".to_string(),
        current_value: json!(texture_slug(inp.soil_texture)),
        suggested_value: suggested,
        companion_fields: Vec::new(),
        headline: format!(
            "The probe dries {} than the model expects; {} matches the measured drying \
             better than {}.",
            if dries_faster { "faster" } else { "slower" },
            texture_label(next),
            texture_label(inp.soil_texture)
        ),
        evidence,
        confidence: confidence.to_string(),
    })
}

// ---------------------------------------------------------------------
// Check D: precipitation-rate backout
// ---------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BackoutInputs<'a> {
    /// Clustered irrigation events in the window.
    pub events: &'a [IrrigationEvent],
    /// Valid probe readings, ascending by epoch.
    pub readings: &'a [(i64, f64)],
    /// Local-day bounds of days with observed rain >= RAIN_DAY_IN.
    pub wet_day_intervals: &'a [(i64, i64)],
    pub taw_mm: f64,
    pub capture_efficiency: f64,
    /// effective_precip_rate_mm_hr(sprinkler_type, precip_rate_mm_hr).
    pub effective_rate_mm_hr: f64,
    /// The raw configured override (null on the wire when unset).
    pub configured_rate_mm_hr: Option<f64>,
}

/// Per-event derived rates from probe rise across clean events.
pub fn backout_rates(inp: &BackoutInputs<'_>) -> Vec<f64> {
    let mut rates = Vec::new();
    if inp.taw_mm <= 0.0 {
        return rates;
    }
    let capture = inp.capture_efficiency.clamp(0.05, 1.0);
    for ev in inp.events {
        if ev.valve_open_s <= 0 {
            continue;
        }
        // A rain day disqualifies the event: the probe rise cannot be
        // attributed to the sprinkler alone.
        let rainy = inp
            .wet_day_intervals
            .iter()
            .any(|(a, b)| ev.start_epoch < *b && ev.end_epoch >= *a);
        if rainy {
            continue;
        }
        let pre = inp
            .readings
            .iter()
            .rev()
            .find(|(e, _)| *e <= ev.start_epoch);
        let post = inp.readings.iter().find(|(e, _)| {
            *e >= ev.end_epoch + BACKOUT_SETTLE_MIN_S && *e <= ev.end_epoch + BACKOUT_SETTLE_MAX_S
        });
        let (Some((_, pre_pct)), Some((_, post_pct))) = (pre, post) else {
            continue;
        };
        let rise_pct = post_pct - pre_pct;
        if rise_pct <= 0.0 {
            // The probe recorded no rise: not attributable, not a clean event.
            continue;
        }
        let rise_mm = rise_pct / 100.0 * inp.taw_mm / capture;
        let hours = ev.valve_open_s as f64 / 3600.0;
        let rate = rise_mm / hours;
        if rate.is_finite() && rate > 0.0 {
            rates.push(rate);
        }
    }
    rates
}

pub fn check_precip_backout(slug: &str, inp: &BackoutInputs<'_>) -> CheckOutcome {
    let rates = backout_rates(inp);
    let n = rates.len();
    if n < BACKOUT_MIN_EVENTS {
        return CheckOutcome::Insufficient(format!(
            "Need at least {BACKOUT_MIN_EVENTS} watering events with a clean probe rise (no \
             rain that day, readings before and after) to back out the sprinkler rate; found \
             {n}."
        ));
    }
    let median = median_of(rates).expect("n >= BACKOUT_MIN_EVENTS implies a median");
    let effective = inp.effective_rate_mm_hr;
    if effective <= 0.0 {
        return CheckOutcome::Insufficient(
            "The zone's configured delivery rate is zero, so the measured rate has no \
             baseline to compare against."
                .to_string(),
        );
    }
    let rel = (median - effective).abs() / effective;
    if rel <= BACKOUT_TOLERANCE {
        return CheckOutcome::Pass;
    }
    // Clamp into the validator's accepted range, staying positive so the
    // override actually takes effect.
    let suggested_rate = round1(median.clamp(0.1, 200.0));
    let suggested = json!(suggested_rate);
    let id = recommendation_id(slug, "precip_rate_mm_hr", &suggested);
    // Capped at medium: the rise-to-mm conversion inherits the probe's
    // uncalibrated percent scale, so no event count earns "high" here.
    let confidence = "medium";
    CheckOutcome::Recommend(TuningRecommendation {
        id,
        field: "precip_rate_mm_hr".to_string(),
        current_value: inp
            .configured_rate_mm_hr
            .map(|v| json!(v))
            .unwrap_or(Value::Null),
        suggested_value: suggested,
        companion_fields: vec![TuningCompanionField {
            field: "precip_rate_source".to_string(),
            value: json!("measured"),
        }],
        headline: format!(
            "Set this zone's sprinkler rate to the measured {suggested_rate:.1} mm/hr; runs \
             are planned as if it were {effective:.1} mm/hr."
        ),
        evidence: vec![
            format!(
                "Median rate backed out of {n} clean watering events: {median:.1} mm/hr vs \
                 the configured {effective:.1} mm/hr ({:.0}% apart).",
                rel * 100.0
            ),
            "Each event's rate comes from the probe's rise between the last reading before \
             the run and the first settled reading after it."
                .to_string(),
            "Assumes the probe's 0 to 100% span matches the configured water bucket; a probe \
             scale error of 1.3x or more alone reproduces this signal."
                .to_string(),
        ],
        confidence: confidence.to_string(),
    })
}

// ---------------------------------------------------------------------
// Check E: forecast-skip scorecard (install-wide)
// ---------------------------------------------------------------------

/// One local day's morning verdict, reduced upstream exactly as
/// accuracy_window does (earliest transition per configured-tz day).
#[derive(Debug, Clone, PartialEq)]
pub struct SkipDayRecord {
    pub date: NaiveDate,
    pub verdict: String,
    /// Stable rule id: DecisionTrace.reason_code when present, else
    /// classify_reason_code on the reason string.
    pub reason_code: String,
}

/// Window-aware confirmation of FORECAST rain skips, plus a separate
/// count of reactive rain skips. Forecast codes: rain_next_4h confirms
/// same-day under the assess_day contract (scored only when rain was a
/// real factor), tomorrow_rain against the NEXT day's observed total,
/// rain_3day against the following 3-day observed sum. Days whose
/// confirmation window is not yet complete stay unscored (today's
/// observed total is partial until the day ends).
///
/// Reactive codes (rain_now, observed_rain, already_wet) are triggered
/// by rain already falling or already on the ground, so confirming them
/// against observed rain would be self-confirming and counting them as
/// forecast calls would distort the tally in both directions; they are
/// counted on their own line with no confirmation math.
pub fn score_forecast_skips(
    days: &[SkipDayRecord],
    obs: &BTreeMap<NaiveDate, (f64, f64)>,
    last_complete_day: NaiveDate,
    window_days: u32,
) -> TuningScorecard {
    let mut scored: u32 = 0;
    let mut confirmed: u32 = 0;
    let mut reactive: u32 = 0;
    for day in days {
        if !day.verdict.starts_with("skip") {
            continue;
        }
        match day.reason_code.as_str() {
            "rain_now" | "observed_rain" | "already_wet" => {
                reactive += 1;
            }
            "rain_next_4h" => {
                if day.date > last_complete_day {
                    continue;
                }
                let Some((pred, observed)) = obs.get(&day.date) else {
                    continue;
                };
                // assess_day contract: a dry day with no significant forecast
                // is not scored, so trivially-dry days cannot inflate the tally.
                if *observed < WET_IN && *pred < SIG_IN {
                    continue;
                }
                scored += 1;
                if *observed >= WET_IN {
                    confirmed += 1;
                }
            }
            "tomorrow_rain" => {
                let Some(next) = day.date.succ_opt() else {
                    continue;
                };
                if next > last_complete_day {
                    continue;
                }
                let Some((_, observed)) = obs.get(&next) else {
                    continue;
                };
                scored += 1;
                if *observed >= SIG_IN {
                    confirmed += 1;
                }
            }
            "rain_3day" => {
                let mut sum = 0.0;
                let mut all_present = true;
                let mut d = day.date;
                for _ in 0..3 {
                    let Some(next) = d.succ_opt() else {
                        all_present = false;
                        break;
                    };
                    d = next;
                    if d > last_complete_day {
                        all_present = false;
                        break;
                    }
                    match obs.get(&d) {
                        Some((_, observed)) => sum += observed,
                        None => {
                            all_present = false;
                            break;
                        }
                    }
                }
                if !all_present {
                    continue;
                }
                scored += 1;
                if sum >= SIG_IN {
                    confirmed += 1;
                }
            }
            _ => {}
        }
    }
    // Reactive skips carry their own additive line, counted only (rain
    // already falling or on the ground confirms itself).
    let (reactive_days, reactive_line) = if reactive > 0 {
        (
            Some(reactive),
            format!(
                "Skipped {reactive} day(s) for rain already falling or on the ground in the \
                 last {window_days}."
            ),
        )
    } else {
        (None, String::new())
    };
    if scored >= SCORECARD_MIN_SCORED {
        TuningScorecard {
            window_days,
            scored_days: Some(scored),
            confirmed_days: Some(confirmed),
            min_scored_days: SCORECARD_MIN_SCORED,
            line: format!(
                "Skipped {scored} days for forecast rain in the last {window_days}; rain came \
                 {confirmed} of {scored}."
            ),
            reactive_days,
            reactive_line,
        }
    } else {
        TuningScorecard {
            window_days,
            scored_days: None,
            confirmed_days: None,
            min_scored_days: SCORECARD_MIN_SCORED,
            line: format!(
                "Not enough forecast-rain skips to judge yet (need {SCORECARD_MIN_SCORED} \
                 scored days; have {scored})."
            ),
            reactive_days,
            reactive_line,
        }
    }
}

// ---------------------------------------------------------------------
// Ranking + per-zone assembly
// ---------------------------------------------------------------------

/// How the zone's soil sensor is bound, for the probe-dependent checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoilBinding {
    /// No probe configured.
    None,
    /// `source:<id>:<key>`: local history exists.
    SourceChannel,
    /// `ha:<entity>` or bare spec: live state only, no local history.
    HaEntity,
}

/// Per-zone check outcomes, in evaluation order.
#[derive(Debug, Clone)]
pub struct ZoneCheckOutcomes {
    pub cap: CheckOutcome,
    pub interval: CheckOutcome,
    /// None = not applicable (no source-bound probe).
    pub drift: Option<CheckOutcome>,
    /// None = not applicable, or suppressed because drift flagged.
    pub backout: Option<CheckOutcome>,
}

/// Reduce a zone's check outcomes to the final ZoneTuning: at most one
/// recommendation (priority cap > drift > backout > interval), the
/// cadence line, the probe framing, and every insufficiency line.
pub fn assemble_zone(
    slug: &str,
    display_name: &str,
    window_days: u32,
    run_day_count: u32,
    has_any_window_data: bool,
    binding: SoilBinding,
    outcomes: &ZoneCheckOutcomes,
) -> ZoneTuning {
    let mut lines: Vec<String> = Vec::new();
    if run_day_count > 0 {
        lines.push(format!(
            "Watered {run_day_count} time(s) in the last {window_days} days."
        ));
    }
    match binding {
        SoilBinding::None => lines.push(
            "A soil probe would unlock the drying-rate and sprinkler-rate checks for this \
             zone."
                .to_string(),
        ),
        SoilBinding::HaEntity => lines.push(
            "History is not available for this probe binding (live Home Assistant entity), so \
             the drying-rate and sprinkler-rate checks are unavailable."
                .to_string(),
        ),
        SoilBinding::SourceChannel => {}
    }

    // Priority pick: cap > drift > backout > interval.
    let ranked: [Option<&CheckOutcome>; 4] = [
        Some(&outcomes.cap),
        outcomes.drift.as_ref(),
        outcomes.backout.as_ref(),
        Some(&outcomes.interval),
    ];
    let mut recommendation: Option<TuningRecommendation> = None;
    for outcome in ranked.into_iter().flatten() {
        match outcome {
            CheckOutcome::Recommend(r) => {
                if recommendation.is_none() {
                    recommendation = Some(r.clone());
                }
            }
            // Dedupe: the probe-scale cross-check hands drift and backout
            // the SAME line; the reader needs it once.
            CheckOutcome::Insufficient(line) => {
                if !lines.contains(line) {
                    lines.push(line.clone());
                }
            }
            CheckOutcome::Pass => {}
        }
    }

    let status = if recommendation.is_some() {
        "recommendation"
    } else if run_day_count > 0 || has_any_window_data {
        "ok"
    } else {
        "insufficient_data"
    };
    ZoneTuning {
        slug: slug.to_string(),
        display_name: display_name.to_string(),
        status: status.to_string(),
        lines,
        recommendation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(start: i64, dur: i64) -> RunSegment {
        RunSegment {
            start_epoch: start,
            end_epoch: start + dur,
        }
    }

    // ---- id stability ----

    #[test]
    fn recommendation_id_is_stable_and_input_sensitive() {
        let a = recommendation_id("back_yard", "soil_texture", &json!("loam"));
        let b = recommendation_id("back_yard", "soil_texture", &json!("loam"));
        assert_eq!(a, b, "same inputs must hash identically");
        assert_eq!(a.len(), 16);
        let c = recommendation_id("back_yard", "soil_texture", &json!("clay"));
        assert_ne!(a, c, "a different suggestion must change the id");
        let d = recommendation_id("front_yard", "soil_texture", &json!("loam"));
        assert_ne!(a, d, "a different zone must change the id");
    }

    // ---- slope estimator ----

    #[test]
    fn slope_recovers_a_linear_decline() {
        // 2% per day decline sampled every 6 hours over 3 days.
        let readings: Vec<(i64, f64)> = (0..12)
            .map(|i| (i * 21_600, 50.0 - 2.0 * (i as f64) * 0.25))
            .collect();
        let slope = slope_per_day(&readings).unwrap();
        assert!((slope + 2.0).abs() < 1e-6, "got {slope}");
    }

    #[test]
    fn slope_needs_two_distinct_epochs() {
        assert_eq!(slope_per_day(&[]), None);
        assert_eq!(slope_per_day(&[(100, 50.0)]), None);
        assert_eq!(slope_per_day(&[(100, 50.0), (100, 40.0)]), None);
    }

    // ---- event clustering ----

    #[test]
    fn cluster_merges_cycle_soak_segments() {
        // Three ON segments of one morning (soak gaps of 20 min) plus a
        // separate morning two days later.
        let segments = [
            seg(1_000_000, 600),
            seg(1_001_800, 600),
            seg(1_003_600, 600),
            seg(1_172_800, 900),
        ];
        let events = cluster_events(&segments);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].valve_open_s, 1800, "sums valve-open time only");
        assert_eq!(events[0].start_epoch, 1_000_000);
        assert_eq!(events[0].end_epoch, 1_004_200);
        assert_eq!(events[1].valve_open_s, 900);
    }

    #[test]
    fn cluster_sorts_unordered_segments() {
        let segments = [seg(1_003_600, 600), seg(1_000_000, 600)];
        let events = cluster_events(&segments);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start_epoch, 1_000_000);
    }

    /// A manual run is persisted twice (manual completed row + the
    /// observer's row for the same valve activity). The cluster's
    /// valve-open time is the interval UNION, so the same minutes count
    /// once and the backed-out rate is not halved.
    #[test]
    fn cluster_union_counts_overlapping_manual_and_observer_rows_once() {
        let t = 1_000_000;
        let segments = [
            RunSegment {
                start_epoch: t,
                end_epoch: t + 1200,
            },
            RunSegment {
                start_epoch: t + 10,
                end_epoch: t + 1190,
            },
        ];
        let events = cluster_events(&segments);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].valve_open_s, 1200, "union, not the ~2390 sum");
        // A partially overlapping tail still contributes only its
        // non-overlapped portion.
        let segments = [
            RunSegment {
                start_epoch: t,
                end_epoch: t + 1200,
            },
            RunSegment {
                start_epoch: t + 600,
                end_epoch: t + 1800,
            },
        ];
        let events = cluster_events(&segments);
        assert_eq!(events[0].valve_open_s, 1800);
    }

    // ---- dry stretches ----

    #[test]
    fn dry_stretches_subtract_busy_intervals() {
        let day = 86_400;
        // Window of 10 days; an event on day 2 and a wet day on day 6.
        let busy = [(2 * day, 2 * day + 4 * 3600), (6 * day, 7 * day)];
        let stretches = dry_stretches(0, 10 * day, &busy, DRIFT_MIN_STRETCH_S);
        assert_eq!(stretches.len(), 3);
        assert_eq!(stretches[0], (0, 2 * day));
        assert_eq!(stretches[1], (2 * day + 4 * 3600, 6 * day));
        assert_eq!(stretches[2], (7 * day, 10 * day));
    }

    #[test]
    fn dry_stretches_drop_short_gaps() {
        let day = 86_400;
        let busy = [(day, 2 * day), (3 * day, 4 * day)];
        // The 1-day gap between the busy spans is under 48h and drops out.
        let stretches = dry_stretches(0, 4 * day, &busy, DRIFT_MIN_STRETCH_S);
        assert_eq!(stretches, vec![]);
    }

    // ---- texture ladder ----

    #[test]
    fn adjacent_texture_handles_nonmonotonic_aw() {
        // Sandy loam wanting more water moves to loam.
        assert_eq!(
            adjacent_texture_toward(SoilTexture::SandyLoam, true),
            Some(SoilTexture::Loam)
        );
        // Loam has the highest AW of its neighbors; wanting more goes nowhere.
        assert_eq!(adjacent_texture_toward(SoilTexture::Loam, true), None);
        // Loam wanting less prefers the closer neighbor (silt loam 0.17 is
        // closer to loam 0.22 than sandy loam 0.13).
        assert_eq!(
            adjacent_texture_toward(SoilTexture::Loam, false),
            Some(SoilTexture::SiltLoam)
        );
        // Ladder ends stay one-step.
        assert_eq!(adjacent_texture_toward(SoilTexture::Sand, false), None);
        assert_eq!(
            adjacent_texture_toward(SoilTexture::Sand, true),
            Some(SoilTexture::LoamySand)
        );
    }

    // ---- check A ----

    fn cap_inputs() -> CapClampInputs {
        CapClampInputs {
            session_capped: true,
            deficit_cap_binding: false,
            desired_seconds: Some(5400),
            max_duration_s: Some(3600),
            run_days: 4,
            sessions_per_week: 2,
            configured_sessions: Some(2),
            configured_weekly_budget_in: Some(1.0),
            weekly_budget_in: 1.0,
            throughput_mm_hr: 10.0,
            capture_efficiency: 0.70,
            heat_multiplier: 1.0,
        }
    }

    #[test]
    fn cap_check_suggests_more_sessions() {
        let out = check_cap_clamped("back_yard", &cap_inputs());
        let CheckOutcome::Recommend(rec) = out else {
            panic!("expected a recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "sessions_per_week");
        // 5400 * 2 = 10800 weekly desired; / 3600 cap = 3 sessions.
        assert_eq!(rec.suggested_value, json!(3));
        assert_eq!(rec.confidence, "medium");
        assert!(rec.companion_fields.is_empty());
    }

    #[test]
    fn cap_check_requires_chronic_run_days() {
        let mut inp = cap_inputs();
        inp.run_days = 2;
        assert_eq!(check_cap_clamped("back_yard", &inp), CheckOutcome::Pass);
    }

    #[test]
    fn cap_check_passes_when_not_clamped() {
        let mut inp = cap_inputs();
        inp.session_capped = false;
        assert_eq!(check_cap_clamped("back_yard", &inp), CheckOutcome::Pass);
    }

    /// A one-shot soil-deficit refill over the cap (ZoneMath.cap_binding
    /// without a session-capped budget) must never turn into a
    /// sessions/budget recommendation: neither knob feeds the deficit
    /// chain. It earns the informational line naming the duration cap.
    #[test]
    fn cap_check_deficit_binding_is_informational_only() {
        let mut inp = cap_inputs();
        inp.session_capped = false;
        inp.deficit_cap_binding = true;
        inp.desired_seconds = None;
        let out = check_cap_clamped("back_yard", &inp);
        let CheckOutcome::Insufficient(line) = out else {
            panic!("expected the informational line, got {out:?}");
        };
        assert!(line.contains("maximum run duration"), "{line}");
        assert!(line.contains("do not shape this refill"), "{line}");
        // Still gated on the chronic run-day floor.
        inp.run_days = 2;
        assert_eq!(check_cap_clamped("back_yard", &inp), CheckOutcome::Pass);
    }

    #[test]
    fn cap_check_reports_no_runs_as_insufficient() {
        let mut inp = cap_inputs();
        inp.run_days = 0;
        assert!(matches!(
            check_cap_clamped("back_yard", &inp),
            CheckOutcome::Insufficient(_)
        ));
    }

    #[test]
    fn cap_check_falls_back_to_budget_when_sessions_maxed() {
        let mut inp = cap_inputs();
        inp.sessions_per_week = 7;
        inp.configured_sessions = Some(7);
        inp.desired_seconds = Some(7200);
        inp.weekly_budget_in = 3.0;
        inp.configured_weekly_budget_in = Some(3.0);
        let out = check_cap_clamped("back_yard", &inp);
        let CheckOutcome::Recommend(rec) = out else {
            panic!("expected a budget recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "weekly_budget_in");
        // 7 sessions * 1h cap * 10 mm/hr * 0.7 capture = 49 mm = 1.9 in.
        assert_eq!(rec.suggested_value, json!(1.9));
    }

    // ---- check B ----

    fn interval_inputs() -> IntervalInputs {
        IntervalInputs {
            species: GrassSpecies::StAugustine,
            soil_texture: SoilTexture::SandyLoam,
            root_depth_mm_override: None,
            mad_pct_override: None,
            mean_daily_etc_mm: Some(5.0),
        }
    }

    #[test]
    fn interval_math_matches_summarize_resolution_order() {
        // St. Augustine default root/MAD on sandy loam: TAW = 0.13 * 150 =
        // 19.5 mm, RAW = 9.75 mm; at 5 mm/day the interval is ~1.95 days.
        let interval = expected_interval_days(&interval_inputs()).unwrap();
        assert!((interval - 1.95).abs() < 0.01, "got {interval}");
        assert_eq!(
            check_interval("back_yard", &interval_inputs()),
            CheckOutcome::Pass,
            "a plausible interval must not fire"
        );
    }

    #[test]
    fn interval_flags_a_shallow_root_override() {
        let mut inp = interval_inputs();
        inp.root_depth_mm_override = Some(40.0); // TAW 5.2 mm -> ~0.5 day
        let out = check_interval("back_yard", &inp);
        let CheckOutcome::Recommend(rec) = out else {
            panic!("expected a recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "root_depth_mm");
        assert_eq!(rec.suggested_value, Value::Null, "null clears the override");
        assert_eq!(rec.current_value, json!(40.0));
    }

    #[test]
    fn interval_flags_texture_when_no_override_set() {
        let mut inp = interval_inputs();
        // Sand at default depth: RAW = 0.06*150*0.5 = 4.5 mm -> 0.9 days.
        inp.soil_texture = SoilTexture::Sand;
        let out = check_interval("back_yard", &inp);
        let CheckOutcome::Recommend(rec) = out else {
            panic!("expected a recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "soil_texture");
        assert_eq!(rec.suggested_value, json!("loamy_sand"));
    }

    #[test]
    fn interval_reports_missing_forecast_as_insufficient() {
        let mut inp = interval_inputs();
        inp.mean_daily_etc_mm = None;
        assert!(matches!(
            check_interval("back_yard", &inp),
            CheckOutcome::Insufficient(_)
        ));
    }

    // ---- check C ----

    /// Readings declining at `pct_per_day` across the given stretches,
    /// 8+ samples per stretch.
    fn declining_readings(stretches: &[(i64, i64)], pct_per_day: f64) -> Vec<(i64, f64)> {
        let mut out = Vec::new();
        for (a, b) in stretches {
            let span = b - a;
            for i in 0..10 {
                let e = a + span * i / 10;
                let days = (e - a) as f64 / 86_400.0;
                out.push((e, 60.0 - pct_per_day * days));
            }
        }
        out.sort_by_key(|(e, _)| *e);
        out
    }

    #[test]
    fn drift_flags_slow_drying_toward_higher_aw() {
        let day = 86_400;
        let stretches = [(0, 3 * day), (5 * day, 8 * day)];
        // Model expects 10%/day; probe dries at 2%/day -> ratio 0.2.
        let readings = declining_readings(&stretches, 2.0);
        let inp = DriftInputs {
            readings: &readings,
            stretches: &stretches,
            modeled_slope_pct_per_day: Some(10.0),
            soil_texture: SoilTexture::SandyLoam,
            species: GrassSpecies::Bermuda,
            root_depth_mm_override: None,
        };
        let out = check_probe_drift("front_yard", &inp);
        let CheckOutcome::Recommend(rec) = out else {
            panic!("expected a recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "soil_texture");
        assert_eq!(
            rec.suggested_value,
            json!("loam"),
            "slower-than-modeled drying means the real bucket is larger"
        );
        assert_eq!(rec.confidence, "medium", "capped: probe scale uncalibrated");
        assert!(
            rec.evidence.iter().any(|e| e.contains("probe scale error")),
            "assumption line missing: {:?}",
            rec.evidence
        );
    }

    #[test]
    fn drift_in_band_passes() {
        let day = 86_400;
        let stretches = [(0, 3 * day), (5 * day, 8 * day)];
        let readings = declining_readings(&stretches, 9.0);
        let inp = DriftInputs {
            readings: &readings,
            stretches: &stretches,
            modeled_slope_pct_per_day: Some(10.0),
            soil_texture: SoilTexture::SandyLoam,
            species: GrassSpecies::Bermuda,
            root_depth_mm_override: None,
        };
        assert_eq!(check_probe_drift("front_yard", &inp), CheckOutcome::Pass);
    }

    #[test]
    fn drift_prefers_restoring_root_override() {
        let day = 86_400;
        let stretches = [(0, 3 * day), (5 * day, 8 * day)];
        // Fast drying (ratio 2.0) with a DEEP root override: the override
        // overstates the bucket, so restore it rather than step texture.
        let readings = declining_readings(&stretches, 20.0);
        let inp = DriftInputs {
            readings: &readings,
            stretches: &stretches,
            modeled_slope_pct_per_day: Some(10.0),
            soil_texture: SoilTexture::SandyLoam,
            species: GrassSpecies::Bermuda,
            root_depth_mm_override: Some(500.0),
        };
        let out = check_probe_drift("front_yard", &inp);
        let CheckOutcome::Recommend(rec) = out else {
            panic!("expected a recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "root_depth_mm");
        assert_eq!(rec.suggested_value, Value::Null);
    }

    #[test]
    fn drift_requires_two_stretches_with_enough_readings() {
        let day = 86_400;
        let stretches = [(0, 3 * day)];
        let readings = declining_readings(&stretches, 2.0);
        let inp = DriftInputs {
            readings: &readings,
            stretches: &stretches,
            modeled_slope_pct_per_day: Some(10.0),
            soil_texture: SoilTexture::SandyLoam,
            species: GrassSpecies::Bermuda,
            root_depth_mm_override: None,
        };
        assert!(matches!(
            check_probe_drift("front_yard", &inp),
            CheckOutcome::Insufficient(_)
        ));
    }

    // ---- check D ----

    /// One clean event: probe at `pre` before the run, `pre + rise` after
    /// the settle window.
    fn backout_fixture(rise_pct: f64, events: usize) -> (Vec<IrrigationEvent>, Vec<(i64, f64)>) {
        let day = 86_400;
        let mut evs = Vec::new();
        let mut readings = Vec::new();
        for i in 0..events {
            let start = i as i64 * 2 * day;
            let end = start + 3600;
            evs.push(IrrigationEvent {
                start_epoch: start,
                end_epoch: end,
                valve_open_s: 3600,
            });
            readings.push((start - 600, 40.0));
            readings.push((end + 3600, 40.0 + rise_pct));
        }
        readings.sort_by_key(|(e, _)| *e);
        (evs, readings)
    }

    #[test]
    fn backout_derives_rate_and_flags_mismatch() {
        // TAW 32.5 mm, capture 0.7: a 38.8% rise over a 1h event backs out
        // ~18 mm/hr. Configured 38 mm/hr differs by >30% -> flag.
        let (events, readings) = backout_fixture(38.8, 3);
        let inp = BackoutInputs {
            events: &events,
            readings: &readings,
            wet_day_intervals: &[],
            taw_mm: 32.5,
            capture_efficiency: 0.70,
            effective_rate_mm_hr: 38.0,
            configured_rate_mm_hr: None,
        };
        let out = check_precip_backout("back_yard", &inp);
        let CheckOutcome::Recommend(rec) = out else {
            panic!("expected a recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "precip_rate_mm_hr");
        let rate = rec.suggested_value.as_f64().unwrap();
        assert!((rate - 18.0).abs() < 0.1, "got {rate}");
        assert_eq!(rec.companion_fields.len(), 1);
        assert_eq!(rec.companion_fields[0].field, "precip_rate_source");
        assert_eq!(rec.companion_fields[0].value, json!("measured"));
        assert_eq!(rec.current_value, Value::Null);
    }

    #[test]
    fn backout_within_tolerance_passes() {
        let (events, readings) = backout_fixture(38.8, 3);
        let inp = BackoutInputs {
            events: &events,
            readings: &readings,
            wet_day_intervals: &[],
            taw_mm: 32.5,
            capture_efficiency: 0.70,
            effective_rate_mm_hr: 16.0,
            configured_rate_mm_hr: Some(16.0),
        };
        assert_eq!(check_precip_backout("back_yard", &inp), CheckOutcome::Pass);
    }

    #[test]
    fn backout_discards_rain_day_events_and_missing_readings() {
        let (events, readings) = backout_fixture(38.8, 3);
        // Wet interval covering the second event.
        let day = 86_400;
        let wet = [(2 * day - 3600, 3 * day)];
        let inp = BackoutInputs {
            events: &events,
            readings: &readings,
            wet_day_intervals: &wet,
            taw_mm: 32.5,
            capture_efficiency: 0.70,
            effective_rate_mm_hr: 38.0,
            configured_rate_mm_hr: None,
        };
        // Only 2 clean events remain -> insufficient, not a recommendation.
        assert!(matches!(
            check_precip_backout("back_yard", &inp),
            CheckOutcome::Insufficient(_)
        ));
    }

    /// The probe percent scale is uncalibrated, so no event count earns
    /// "high": confidence caps at medium and the assumption is stated in
    /// the evidence.
    #[test]
    fn backout_confidence_caps_at_medium_with_stated_assumption() {
        let (events, readings) = backout_fixture(38.8, 6);
        let inp = BackoutInputs {
            events: &events,
            readings: &readings,
            wet_day_intervals: &[],
            taw_mm: 32.5,
            capture_efficiency: 0.70,
            effective_rate_mm_hr: 38.0,
            configured_rate_mm_hr: None,
        };
        let CheckOutcome::Recommend(rec) = check_precip_backout("back_yard", &inp) else {
            panic!("expected a recommendation");
        };
        assert_eq!(rec.confidence, "medium");
        assert!(
            rec.evidence.iter().any(|e| e.contains("probe scale error")),
            "assumption line missing: {:?}",
            rec.evidence
        );
    }

    // ---- probe scale cross-check ----

    #[test]
    fn median_of_handles_odd_even_and_empty() {
        assert_eq!(median_of(vec![]), None);
        assert_eq!(median_of(vec![3.0]), Some(3.0));
        assert_eq!(median_of(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median_of(vec![4.0, 1.0, 2.0, 3.0]), Some(2.5));
    }

    #[test]
    fn crosscheck_fires_when_ratios_agree_in_direction() {
        // Both read ~2x high in the same direction: probe scale suspected.
        let line = probe_scale_crosscheck(Some(2.0), Some(2.1)).expect("should fire");
        assert!(line.contains("Probe calibration suspected"), "{line}");
        assert!(line.contains("2.1x") || line.contains("2.0x"), "{line}");
        // Both low works too.
        assert!(probe_scale_crosscheck(Some(0.5), Some(0.45)).is_some());
    }

    #[test]
    fn crosscheck_stays_quiet_without_agreement() {
        // Opposite directions: not a scale error.
        assert_eq!(probe_scale_crosscheck(Some(0.5), Some(2.0)), None);
        // Same direction but too far apart (2.6/1.7 > 1.25).
        assert_eq!(probe_scale_crosscheck(Some(1.7), Some(2.6)), None);
        // A ratio of exactly 1.0 has no direction.
        assert_eq!(probe_scale_crosscheck(Some(1.0), Some(0.9)), None);
        // Either side missing: nothing to cross-check.
        assert_eq!(probe_scale_crosscheck(None, Some(2.0)), None);
        assert_eq!(probe_scale_crosscheck(Some(2.0), None), None);
    }

    #[test]
    fn backout_ignores_events_with_no_probe_rise() {
        let (events, readings) = backout_fixture(0.0, 4);
        let inp = BackoutInputs {
            events: &events,
            readings: &readings,
            wet_day_intervals: &[],
            taw_mm: 32.5,
            capture_efficiency: 0.70,
            effective_rate_mm_hr: 38.0,
            configured_rate_mm_hr: None,
        };
        assert!(matches!(
            check_precip_backout("back_yard", &inp),
            CheckOutcome::Insufficient(_)
        ));
    }

    // ---- check E ----

    fn d(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn skip(date: &str, code: &str) -> SkipDayRecord {
        SkipDayRecord {
            date: d(date),
            verdict: "skip".to_string(),
            reason_code: code.to_string(),
        }
    }

    #[test]
    fn scorecard_windows_confirmation_by_reason_code() {
        let days = vec![
            // Same-day code, rain came.
            skip("2026-06-01", "rain_next_4h"),
            // tomorrow_rain: confirmed by the NEXT day's total.
            skip("2026-06-03", "tomorrow_rain"),
            // rain_3day: the following 3-day sum.
            skip("2026-06-05", "rain_3day"),
            // Not rain family: never scored.
            skip("2026-06-09", "wind_now"),
        ];
        let mut obs = BTreeMap::new();
        obs.insert(d("2026-06-01"), (0.30, 0.25)); // same day: confirmed
        obs.insert(d("2026-06-03"), (0.40, 0.00)); // skip day itself dry
        obs.insert(d("2026-06-04"), (0.00, 0.20)); // next day: confirmed
        obs.insert(d("2026-06-06"), (0.00, 0.03));
        obs.insert(d("2026-06-07"), (0.00, 0.03));
        obs.insert(d("2026-06-08"), (0.00, 0.02)); // 3-day sum 0.08 < SIG: miss
        let card = score_forecast_skips(&days, &obs, d("2026-06-30"), 30);
        assert_eq!(card.scored_days, Some(3));
        assert_eq!(card.confirmed_days, Some(2));
        assert!(card.line.contains("rain came 2 of 3"), "{}", card.line);
    }

    /// Reactive rain skips (rain already falling or on the ground) are
    /// self-confirming and must NEVER enter the forecast tally; they get
    /// their own counted line instead.
    #[test]
    fn scorecard_reactive_codes_never_enter_forecast_tally() {
        let days = vec![
            skip("2026-06-01", "observed_rain"),
            skip("2026-06-02", "already_wet"),
            skip("2026-06-03", "rain_now"),
            // One real forecast skip so the tallies are distinguishable.
            skip("2026-06-05", "rain_next_4h"),
        ];
        let mut obs = BTreeMap::new();
        // Rain-relevant observations on every reactive day: if they were
        // (wrongly) scored, scored_days would be 4.
        obs.insert(d("2026-06-01"), (0.30, 0.00)); // would count as a false miss
        obs.insert(d("2026-06-02"), (0.00, 0.25)); // would count as a false confirmation
        obs.insert(d("2026-06-03"), (0.30, 0.25));
        obs.insert(d("2026-06-05"), (0.30, 0.25));
        let card = score_forecast_skips(&days, &obs, d("2026-06-30"), 30);
        assert_eq!(
            card.scored_days, None,
            "only the one forecast skip is scorable (under the minimum)"
        );
        assert!(card.line.contains("have 1"), "{}", card.line);
        assert_eq!(card.reactive_days, Some(3));
        assert!(
            card.reactive_line
                .contains("rain already falling or on the ground"),
            "{}",
            card.reactive_line
        );
    }

    #[test]
    fn scorecard_reactive_line_absent_when_no_reactive_days() {
        let days = vec![skip("2026-06-01", "rain_next_4h")];
        let mut obs = BTreeMap::new();
        obs.insert(d("2026-06-01"), (0.30, 0.25));
        let card = score_forecast_skips(&days, &obs, d("2026-06-30"), 30);
        assert_eq!(card.reactive_days, None, "null until a reactive day exists");
        assert!(card.reactive_line.is_empty());
    }

    #[test]
    fn scorecard_leaves_incomplete_windows_unscored() {
        // A tomorrow_rain skip whose next day is not complete yet.
        let days = vec![
            skip("2026-06-10", "tomorrow_rain"),
            skip("2026-06-01", "rain_next_4h"),
            skip("2026-06-02", "rain_next_4h"),
        ];
        let mut obs = BTreeMap::new();
        obs.insert(d("2026-06-01"), (0.30, 0.25));
        obs.insert(d("2026-06-02"), (0.30, 0.25));
        obs.insert(d("2026-06-11"), (0.50, 5.00));
        // last complete day is the 10th: the 11th's total is still partial.
        let card = score_forecast_skips(&days, &obs, d("2026-06-10"), 30);
        assert_eq!(
            card.scored_days, None,
            "2 scored days is under the minimum; counts stay null"
        );
        assert!(card.line.contains("need 3"), "{}", card.line);
    }

    #[test]
    fn scorecard_skips_trivially_dry_same_day_calls() {
        // assess_day contract: obs < WET and pred < SIG is not scored.
        let days = vec![
            skip("2026-06-01", "rain_next_4h"),
            skip("2026-06-02", "rain_next_4h"),
            skip("2026-06-03", "rain_next_4h"),
        ];
        let mut obs = BTreeMap::new();
        obs.insert(d("2026-06-01"), (0.02, 0.00));
        obs.insert(d("2026-06-02"), (0.30, 0.00)); // scored, a miss
        obs.insert(d("2026-06-03"), (0.30, 0.25)); // scored, confirmed
        let card = score_forecast_skips(&days, &obs, d("2026-06-30"), 30);
        assert_eq!(card.scored_days, None, "only 2 scored: under the minimum");
    }

    // ---- ranking / assembly ----

    fn rec_for(field: &str) -> TuningRecommendation {
        TuningRecommendation {
            id: recommendation_id("z", field, &json!(1)),
            field: field.to_string(),
            current_value: Value::Null,
            suggested_value: json!(1),
            companion_fields: Vec::new(),
            headline: format!("set {field}"),
            evidence: vec![],
            confidence: "medium".to_string(),
        }
    }

    #[test]
    fn ranking_prefers_cap_then_drift_then_backout_then_interval() {
        let outcomes = ZoneCheckOutcomes {
            cap: CheckOutcome::Recommend(rec_for("sessions_per_week")),
            interval: CheckOutcome::Recommend(rec_for("soil_texture")),
            drift: Some(CheckOutcome::Recommend(rec_for("root_depth_mm"))),
            backout: Some(CheckOutcome::Recommend(rec_for("precip_rate_mm_hr"))),
        };
        let z = assemble_zone(
            "z",
            "Zone",
            14,
            5,
            true,
            SoilBinding::SourceChannel,
            &outcomes,
        );
        assert_eq!(z.status, "recommendation");
        assert_eq!(z.recommendation.unwrap().field, "sessions_per_week");

        let outcomes = ZoneCheckOutcomes {
            cap: CheckOutcome::Pass,
            interval: CheckOutcome::Recommend(rec_for("soil_texture")),
            drift: Some(CheckOutcome::Recommend(rec_for("root_depth_mm"))),
            backout: None,
        };
        let z = assemble_zone(
            "z",
            "Zone",
            14,
            5,
            true,
            SoilBinding::SourceChannel,
            &outcomes,
        );
        assert_eq!(z.recommendation.unwrap().field, "root_depth_mm");
    }

    #[test]
    fn assembly_carries_insufficiency_lines_and_cadence() {
        let outcomes = ZoneCheckOutcomes {
            cap: CheckOutcome::Pass,
            interval: CheckOutcome::Pass,
            drift: Some(CheckOutcome::Insufficient("need more dry stretches".into())),
            backout: Some(CheckOutcome::Insufficient("need more events".into())),
        };
        let z = assemble_zone(
            "z",
            "Zone",
            14,
            3,
            true,
            SoilBinding::SourceChannel,
            &outcomes,
        );
        assert_eq!(z.status, "ok");
        assert!(z.lines.iter().any(|l| l.contains("Watered 3 time(s)")));
        assert!(z.lines.iter().any(|l| l.contains("dry stretches")));
        assert!(z.lines.iter().any(|l| l.contains("more events")));
    }

    #[test]
    fn assembly_marks_probe_less_and_ha_bound_zones() {
        let outcomes = ZoneCheckOutcomes {
            cap: CheckOutcome::Pass,
            interval: CheckOutcome::Pass,
            drift: None,
            backout: None,
        };
        let z = assemble_zone("z", "Zone", 14, 2, true, SoilBinding::None, &outcomes);
        assert!(z
            .lines
            .iter()
            .any(|l| l.contains("soil probe would unlock")));
        let z = assemble_zone("z", "Zone", 14, 2, true, SoilBinding::HaEntity, &outcomes);
        assert!(z
            .lines
            .iter()
            .any(|l| l.contains("History is not available for this probe binding")));
    }

    #[test]
    fn assembly_flags_empty_windows_as_insufficient_data() {
        let outcomes = ZoneCheckOutcomes {
            cap: CheckOutcome::Insufficient("no runs".into()),
            interval: CheckOutcome::Pass,
            drift: None,
            backout: None,
        };
        let z = assemble_zone("z", "Zone", 14, 0, false, SoilBinding::None, &outcomes);
        assert_eq!(z.status, "insufficient_data");
    }
}
