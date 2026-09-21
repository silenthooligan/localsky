// LocalSky's irrigation recommendation engine. Single source of truth
// for the morning skip decision: the dashboard renders the verdict from
// here, and HA's automation reads the same verdict via REST sensor and
// acts on it.
//
// The thresholds are `SkipRuleParams`, read from `config.engine.skip_rules`
// at runtime rather than compiled in, so an operator can move a gate
// without a rebuild. The defaults are the values that were once consts.

use std::collections::HashSet;

use crate::config::schema::{AddressParity, SkipRuleParams, WateringRestriction};
use crate::engine::conditions::{apply_zone_rules, ConditionCtx, ConditionRule};
use crate::engine::restrictions;
#[cfg(feature = "ssr")]
use crate::engine::scripting::CompiledScripts;
use crate::model::{DecisionTrace, RainNature, RuleEval, SkipCheck, ZoneVerdict};

/// Inputs the engine needs. Caller fills these from HA states +
/// ForecastSnapshot helpers + TempestStore.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    // ── Live readings ──
    pub temp_now_f: f64,
    pub wind_now_mph: f64,
    pub rain_today_in: f64,
    pub rain_intensity_now_in_hr: Option<f64>,
    /// The HONEST nature of the current-rain reading driving
    /// `rain_intensity_now_in_hr`, derived by the refresher's 3-tier rain gate
    /// from the merge owner: `Measured` (a LAN gauge or NWS observation),
    /// `RadarQpe` (NOAA MRMS radar), or `Model` (a forecast fill). The
    /// "currently raining" gate HARD-skips (binds every zone, beats the
    /// soil_floor moat) ONLY when this is observation-grade (Measured | RadarQpe);
    /// a Model rain rate may only SOFT-skip (demotable, so a measured-dry zone or
    /// the soil_floor can override it). Defaults to `Model` (the honest fallback),
    /// so any caller that doesn't set it keeps the safe demotable behavior.
    pub rain_nature: RainNature,
    pub humidity_now_pct: f64,

    // ── Open-Meteo forecast ──
    pub forecast_in: Option<f64>,
    /// Tomorrow's max precipitation probability, percent. `None` when the
    /// forecast provider reports no probability series: the tomorrow-rain
    /// gate then weights `forecast_in` at FULL value (see
    /// `tomorrow_prob_weight`) and the reason strings omit the confidence
    /// claim. The old bare u32 collapsed "not reported" into 0%, which
    /// zeroed the expected rain and watered ahead of forecast storms.
    pub rain_tomorrow_prob_pct: Option<u32>,
    pub rain_3day_weighted_in: Option<f64>,
    pub rain_7day_weighted_in: Option<f64>,
    pub rain_next_4h_in: Option<f64>,
    /// OBSERVED rain over the recent window: today's measured total plus the
    /// last `rain_observed_window_days` of past observed daily rain. Drives the
    /// sensor-independent observed-rain SKIP backstop (a hard skip that binds
    /// every zone and beats the per-zone soil_floor override, so heavy past rain
    /// is honored even when a soil probe is bad/offline). Computed in the
    /// refresher from the live `rain_today_in` + `past_n_day_precip_in(window)`.
    pub rain_observed_recent_in: f64,
    /// The day's forecast peak wind. Informational once a run window is
    /// known; the gate's fallback when one is not.
    pub wind_max_today_mph: f64,
    /// Forecast peak wind across the minutes the yard plans to water,
    /// from the hourly series. `None` when the hour is unknowable (no
    /// location, no sunrise) or the hourly series does not reach it.
    ///
    /// The wind gate used to judge the daily peak, which is the
    /// afternoon's figure, against a run that finishes before sunrise. A
    /// 25 mph afternoon refused a 5 mph dawn.
    pub wind_window_max_mph: Option<f64>,
    /// Which window the yard is planned into. A post-sunrise window is
    /// chosen only when the pre-dawn hours were below the freeze
    /// threshold, and it carries its own temperatures: the freeze gates
    /// judge `window_min_temp_f` for it rather than the pre-dawn "now"
    /// and the coming night's low.
    pub run_window: crate::engine::dispatch_window::WindowKind,
    /// Forecast minimum across the planned window and the hours after
    /// it, when the hourly series covers them.
    pub window_min_temp_f: Option<f64>,
    /// Forecast overnight low for the next 24h. `None` when the hourly
    /// forecast window is unavailable, so the overnight-freeze gate can
    /// distinguish "no data" from a genuine 0 °F (or colder) low. The
    /// old representation used 0.0 as a missing-data sentinel, which
    /// silently disabled the rule in real sub-zero cold snaps.
    pub temp_min_24h_f: Option<f64>,
    pub temp_max_3day_f: f64,
    /// 3-day forecast peak heat index ("feels-like"), °F, computed PER-DAY so
    /// each day's high temp pairs with THAT day's humidity (via
    /// `ForecastSnapshot::max_heat_index_n_day`). This is the corrected value
    /// the SkipCheck surfaces and that feeds the ET heat multiplier. It replaces
    /// the old, physically-impossible pairing of the 3-day MAX temp with the
    /// CURRENT (often saturated post-rain) humidity. 0.0 = no forecast data;
    /// the heat-advisory RULE still keys on `temp_max_3day_f`, not this value.
    pub heat_index_max_3day_f: f64,
    pub days_since_significant_rain: u32,

    // ── User-tunable thresholds (HA input_number / config.engine.skip_rules) ──
    pub max_wind_mph: f64,
    pub min_temp_f: f64,
    pub rain_skip_in: f64,

    // ── Soil sensor inputs ──
    /// Per-zone soil readings + thresholds, in config order. One entry per
    /// configured zone; `pct: None` = probe offline / unassigned. Empty =
    /// a weather-only deployment (no soil-aware zones). Replaces the former
    /// four hardcoded `soil_*_pct`/`saturation_*_pct` fields.
    pub soil_zones: Vec<ZoneSoil>,
    /// Yard-wide minimum soil temperature (°F), if a soil-temp probe
    /// exists. Drives the global soil-frost gate.
    pub soil_temp_yard_min_f: Option<f64>,
    pub soil_temp_yard_max_f: Option<f64>,
    pub frost_skip_soil_f: f64,

    // ── Live-readings provenance ──
    /// Where `temp_now_f` / `wind_now_mph` (and live humidity) came from.
    /// `Station` = a fresh local station packet (normal). `ForecastFallback`
    /// = the station is stale/absent and the current-hour forecast is
    /// standing in; rules still evaluate but the decision trace is marked
    /// degraded. `Unavailable` = no station AND no forecast; the ladder
    /// fails safe with a skip rather than deciding on fabricated values.
    pub live_readings: LiveReadings,

    /// True when the forecast snapshot is older than the trust horizon (the
    /// Open-Meteo store kept re-emitting its last-good payload during an
    /// outage). Orthogonal to `live_readings`, which is about CURRENT
    /// conditions: a fresh station with a 12-hour-old forecast is
    /// `live_readings == Station` but `forecast_stale == true`. When set, the
    /// forward-looking rain SKIP gates do not fire (a stale "rain coming"
    /// must not starve the yard) and the trace is marked degraded.
    pub forecast_stale: bool,

    // ── Toggles ──
    /// The shared runtime reports startup wiring that cannot be applied live.
    /// Unlike a control override, this is never waived or disabled.
    pub restart_required: bool,
    pub is_paused: bool,
    /// Today's rain as the FORECAST MODEL has it, kept apart from
    /// `rain_today_in`, which is what a gauge actually caught. They used
    /// to be blended with `max`, and the blend fed a gate whose message
    /// says "Already wet", so on a morning the model expected an
    /// afternoon storm the yard hard-skipped and the dashboard reported
    /// rain that had not fallen. Two numbers, two gates, two messages.
    pub rain_today_forecast_in: Option<f64>,
    pub is_dry_run: bool,

    // ── Phase 4 control surfaces ──
    pub pause_until_epoch: i64,
    /// The deployment's calendar.
    ///
    /// A deployment property, like the address parity beside it. Gates
    /// that render or compare a time OTHER than `when` need it: a
    /// vacation pause expiring in December cannot be rendered with the
    /// offset in force today, which is what it used to do.
    pub calendar: crate::engine::calendar::Calendar,
    /// WHEN this decision is about.
    ///
    /// This was two fields, `now_epoch` and `utc_offset_seconds`, and
    /// that is precisely how the 7-day strip came to judge a US Eastern
    /// yard's morning in UTC: one writer filled the instant honestly and
    /// left the offset at its Default, which is zero, which reads as UTC.
    /// Nothing objected, because a pair of integers is a pair of integers.
    ///
    /// An instant and the offset in force at it are ONE fact, so they are
    /// one value now, and only the deployment Calendar can mint it. The
    /// pair also had a state nobody meant: both zero, which claimed a real
    /// instant in 1970 and was indistinguishable from "unset". `Unknown`
    /// says that out loud, and gates abstain rather than guess.
    pub when: crate::engine::clock::DecisionTime,
    pub override_tomorrow: String,
    pub is_tomorrow: bool,
    /// Sticky global override: "auto" | "skip" | "run". A run overrides rain
    /// and soil recommendations, while safety, restrictions and operator holds
    /// remain binding. "" / "auto" = follow the engine.
    pub global_override: String,
    /// Sticky per-zone overrides: zone slug -> "skip" | "run". Absent = auto.
    /// Applied in `decide_per_zone`, beating both the global override and the
    /// engine's per-zone verdict.
    pub zone_overrides: std::collections::HashMap<String, String>,

    // ── Jurisdictional watering restrictions (Phase C) ──
    /// Operator-configured restrictions from `cfg.engine.watering_restrictions`.
    /// Default empty = no enforcement.
    pub watering_restrictions: Vec<WateringRestriction>,
    /// Operator's address parity from `cfg.deployment.address_parity`.
    pub address_parity: AddressParity,
    /// Days in the trailing week on which any zone watered, for a
    /// restriction that allows at most N days a week. Empty when no
    /// history is in hand, which reads as nothing spent.
    pub watered_days: Vec<crate::engine::clock::CivilDay>,
}

/// Health/provenance of the live "now" readings feeding the engine.
/// Default `Station` preserves the historical behavior for every caller
/// that doesn't track provenance (simulator, verdict strip, tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveReadings {
    /// Fresh measured fields, including remote station observations.
    #[default]
    Station,
    /// At least one current field is estimated (selected model or forecast fallback).
    ForecastFallback,
    /// No station data and no forecast. Fail safe: skip, don't fabricate.
    Unavailable,
}

/// One zone's live soil reading + its per-zone thresholds, sourced from
/// `ZoneConfig` (saturation/target) and the assigned sensor. `pct: None`
/// means the probe is offline or no sensor is assigned.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ZoneSoil {
    pub slug: String,
    pub name: String,
    pub pct: Option<f64>,
    /// A bound probe whose reading is absent is a data fault. An unbound zone
    /// can still use weather and its soil model without claiming a measurement.
    pub probe_configured: bool,
    pub saturation_pct: f64,
    pub target_min_pct: f64,
    /// Whether the soil model governs this zone.
    ///
    /// A soil-governed zone already counts forward rain against its own
    /// deficit, so the yard-wide forward-rain gates are inert for it: it
    /// waters anyway, and its deficit is what decides. That is an
    /// irrigation rule, so the engine applies it and the trace says so.
    /// It used to be applied by the refresher, after the fact, by
    /// string-matching reason codes and rewriting verdicts the engine had
    /// already produced, which left the decision trace describing a
    /// morning that did not happen.
    pub governed_by_soil_model: bool,
    /// The automatic planner has no complete next-24h rain evidence. Scoped
    /// per zone and binding for either sizing strategy; Force cannot create
    /// a duration from an unavailable automatic plan.
    pub planning_forecast_unavailable: bool,
    /// The zone's head, so a restriction that exempts drip or bubbler
    /// irrigation can stand aside for it.
    pub sprinkler_type: crate::config::schema::SprinklerType,
}

/// Yard-wide gates a soil-governed zone rides through.
///
/// All three are forward-looking rain: rain later today, rain tomorrow,
/// rain over three days. The soil model has already credited that rain
/// against the zone's deficit, so skipping on it would double-count and
/// leave the zone dry. Gates about right now (rain falling, already wet,
/// saturated soil) and gates about safety or law are NOT inert and still
/// bind every zone.
pub const SOIL_MODEL_INERT_GATES: &[&str] = &[
    "rain_next_4h",
    "tomorrow_rain",
    "rain_3day",
    "rain_today_forecast",
];

// ─────────────────────────────────────────────────────────────────────
// Soil-probe quarantine. A sibling median can identify an implausible reading;
// it cannot establish that this zone is dry. Quarantined probes become absent
// for eligibility and the protected soil_probe gate holds their zone. Raw
// readings remain available for diagnosis. Zones with no bound probe never
// acquire a synthetic reading or a probe-fault hold from their neighbors.
// ─────────────────────────────────────────────────────────────────────

/// One zone's quarantine outcome, produced by `quarantine_plan` and parallel
/// to `Inputs::soil_zones`. `Some` means the zone's probe was distrusted AND a
/// trustworthy sibling median existed to explain the fault. Missing configured
/// probes are held even without siblings, independently of outlier detection.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ZoneQuarantine {
    /// The raw reading the probe reported. `None` when the probe was offline.
    raw_pct: Option<f64>,
    /// Median of the trustworthy siblings, for diagnosis only.
    sibling_median: f64,
}

/// Median of a slice of readings (sorted copy; average of the two middles for
/// an even count). Caller guarantees non-empty.
fn median(vals: &[f64]) -> f64 {
    let mut v = vals.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Compute the per-zone quarantine plan (parallel to `zones`). For each zone:
///   * OFFLINE (None) configured probes are always UNTRUSTWORTHY.
///   * A PRESENT reading is UNTRUSTWORTHY only when >= 3 zones report AND it
///     deviates from the median of all present readings by more than
///     `soil_outlier_threshold_pct` (catches a wildly-low bad-spot probe and a
///     wildly-high one alike).
/// The trustworthy median is diagnostic evidence only. Without a median, no
/// outlier comparison is possible; a configured missing probe still fails the
/// soil_probe gate. Disabling outlier detection never waives missing data.
///
/// `enabled = false` returns an all-`None` plan (exact pre-quarantine behavior).
fn quarantine_plan(zones: &[ZoneSoil], p: &SkipRuleParams) -> Vec<Option<ZoneQuarantine>> {
    let none_plan = || vec![None; zones.len()];
    if !p.soil_quarantine_enabled || zones.is_empty() {
        return none_plan();
    }
    let present: Vec<f64> = zones.iter().filter_map(|z| z.pct).collect();
    // Outliers can only be judged with >= 3 present readings; with fewer, no
    // PRESENT reading is ever distrusted.
    let can_judge_outliers = present.len() >= 3;
    let present_median = if present.is_empty() {
        0.0
    } else {
        median(&present)
    };
    let is_outlier = |pct: f64| {
        can_judge_outliers && (pct - present_median).abs() > p.soil_outlier_threshold_pct
    };
    // Trustworthy = present AND not an outlier. Its median explains the fault.
    let trustworthy: Vec<f64> = zones
        .iter()
        .filter_map(|z| z.pct)
        .filter(|&pct| !is_outlier(pct))
        .collect();
    if trustworthy.is_empty() {
        // No sibling comparison; missing configured probes are still held.
        return none_plan();
    }
    let trust_median = median(&trustworthy);
    zones
        .iter()
        .map(|z| {
            let untrustworthy = match z.pct {
                None => z.probe_configured,
                Some(pct) => is_outlier(pct), // wild outlier vs siblings
            };
            untrustworthy.then_some(ZoneQuarantine {
                raw_pct: z.pct,
                sibling_median: trust_median,
            })
        })
        .collect()
}

/// The effective soil zones: each quarantined zone's reading is unavailable
/// and its configured identity is retained for the data hold. Shared
/// by `decide`, `decide_per_zone`, and
/// `decide_traced` so all soil-gate paths judge identical effective soil.
fn effective_soil_zones(zones: &[ZoneSoil], plan: &[Option<ZoneQuarantine>]) -> Vec<ZoneSoil> {
    zones
        .iter()
        .zip(plan)
        .map(|(z, q)| match q {
            Some(_) => ZoneSoil {
                pct: None,
                probe_configured: true,
                ..z.clone()
            },
            None => z.clone(),
        })
        .collect()
}

/// An `Inputs` clone whose `soil_zones` expose quarantined readings as unknown
/// to the data hold and soil gates.
/// (The raw `i.soil_zones` is still what `evaluate_with` serializes into the
/// SkipCheck `soil_<slug>_pct` fields and what `decide_per_zone` feeds the
/// condition rules, so transparency + condition semantics are unchanged.)
fn with_effective_soil(i: &Inputs, p: &SkipRuleParams) -> Inputs {
    let plan = quarantine_plan(&i.soil_zones, p);
    if plan.iter().all(Option::is_none) {
        return i.clone();
    }
    Inputs {
        soil_zones: effective_soil_zones(&i.soil_zones, &plan),
        ..i.clone()
    }
}

/// REPORTING-ONLY: the per-zone suspect-probe indicator, parallel to
/// `i.soil_zones`. `Some(reason)` for every zone whose probe the quarantine
/// logic distrusts (offline, or a wild outlier vs trustworthy siblings) AND a
/// trustworthy sibling median exists to compare against; `None` otherwise.
///
/// This is the verdict-INDEPENDENT surface for the soil-anomaly banner. It runs
/// the SAME `quarantine_plan` the engine uses, but reads OUT the distrust signal
/// without touching any decision: a bad probe is reported even when a global gate
/// (forecast rain, freeze, ...) ultimately decided the zone and so masked the
/// per-zone `verdict.source` away from "soil_quarantine". The reason carries the
/// canonical "Soil probe suspect (28% vs yard 73%)" shape (no verdict tail) so
/// the UI renders the numbers. All-`None` when quarantine is disabled, no zones
/// report, or no trustworthy median exists (matching the engine's fallback to raw
/// readings). ADDITIVE; never affects a watering decision.
pub fn suspect_probes(i: &Inputs, p: &SkipRuleParams) -> Vec<Option<String>> {
    quarantine_plan(&i.soil_zones, p)
        .iter()
        .map(|q| q.as_ref().map(suspect_reason))
        .collect()
}

/// The canonical suspect-probe reason WITHOUT a verdict tail, e.g.
/// "Soil probe suspect (28% vs yard 73%)" (offline case: "(offline vs ...)").
/// Same prefix/parens shape `quarantine_reason` emits, so the AnomalyBanner's
/// `suspect_line` parser reads it identically.
fn suspect_reason(q: &ZoneQuarantine) -> String {
    let probe = match q.raw_pct {
        Some(raw) => format!("{raw:.0}%"),
        None => "offline".to_string(),
    };
    format!(
        "Soil probe suspect ({} vs yard {:.0}%)",
        probe, q.sibling_median
    )
}

fn quarantine_reason(q: &ZoneQuarantine) -> String {
    format!(
        "{}; watering held until the probe is reliable",
        suspect_reason(q)
    )
}

const SOIL_PROBE_HOLD_REASON: &str =
    "Soil probe unavailable or untrusted; watering held until the probe is reliable";

/// Inputs are scoped to one zone for its decision. The aggregate only holds
/// here when every zone has a probe fault; final per-zone projection handles
/// mixed yards without denying an unbound or healthy sibling permission.
fn soil_probe_unavailable(i: &Inputs) -> bool {
    !i.soil_zones.is_empty()
        && i.soil_zones
            .iter()
            .all(|z| z.probe_configured && z.pct.is_none())
}

/// Data holds are returned even when an earlier operator/weather gate wins.
/// A manual schedule may waive weather; it must still see this independent
/// data-integrity decision without rebuilding the quarantine rules itself.
fn probe_data_holds(i: &Inputs, p: &SkipRuleParams) -> std::collections::BTreeMap<String, String> {
    let plan = quarantine_plan(&i.soil_zones, p);
    let effective = effective_soil_zones(&i.soil_zones, &plan);
    effective
        .iter()
        .zip(&plan)
        .filter(|(z, _)| z.probe_configured && z.pct.is_none())
        .map(|(z, q)| {
            let reason = q
                .as_ref()
                .map(quarantine_reason)
                .unwrap_or_else(|| SOIL_PROBE_HOLD_REASON.to_string());
            (z.slug.clone(), reason)
        })
        .collect()
}

// Bridge the generalized per-zone `soil_zones` Vec to/from the flattened
// `SkipCheck.soil_fields` map ("soil_<slug>_pct" + "saturation_<slug>_pct" +
// "target_<slug>_pct" for EVERY zone), which the manifest's per-zone soil
// descriptor reads. Generalizes the old fixed four-yard-slug fields to any
// number of zones with any slug. target_min_pct (the per-zone soil floor)
// is serialized too, so the Simulator's what-if round-trip preserves a custom
// floor instead of silently snapping every zone back to the 30% default.
fn build_soil_fields(zones: &[ZoneSoil]) -> std::collections::BTreeMap<String, Option<f64>> {
    let mut m = std::collections::BTreeMap::new();
    for z in zones {
        m.insert(format!("soil_{}_pct", z.slug), z.pct);
        m.insert(format!("saturation_{}_pct", z.slug), Some(z.saturation_pct));
        m.insert(format!("target_{}_pct", z.slug), Some(z.target_min_pct));
    }
    m
}

/// Rebuild `soil_zones` from a serialized SkipCheck's flattened soil map (used
/// by the Simulator's what-if round-trip). Recovers every zone present, not a
/// fixed set.
fn rebuild_soil_zones(s: &crate::model::SkipCheck) -> Vec<ZoneSoil> {
    s.soil_fields
        .keys()
        .filter_map(|k| k.strip_prefix("soil_").and_then(|r| r.strip_suffix("_pct")))
        .map(|slug| {
            let pct = s
                .soil_fields
                .get(&format!("soil_{slug}_pct"))
                .copied()
                .flatten();
            let saturation_pct = s
                .soil_fields
                .get(&format!("saturation_{slug}_pct"))
                .copied()
                .flatten()
                .unwrap_or(crate::config::schema::DEFAULT_SATURATION_PCT);
            // Recover the per-zone floor; 30.0 only when absent (an older
            // serialized SkipCheck or demo fixture written before target_*).
            let target_min_pct = s
                .soil_fields
                .get(&format!("target_{slug}_pct"))
                .copied()
                .flatten()
                .unwrap_or(crate::config::schema::DEFAULT_TARGET_MIN_PCT);
            ZoneSoil {
                name: slug.replace('_', " "),
                slug: slug.to_string(),
                pct,
                probe_configured: s.soil_probe_configured.get(slug).copied().unwrap_or(false),
                saturation_pct,
                target_min_pct,
                governed_by_soil_model: false,
                planning_forecast_unavailable: s
                    .planning_forecast_unavailable
                    .iter()
                    .any(|zone| zone == slug),
                sprinkler_type: Default::default(),
            }
        })
        .collect()
}

fn format_pause_until(epoch: i64, cal: crate::engine::calendar::Calendar) -> String {
    // The vacation-pause "until" timestamp, in the deployment's own frame
    // and on a 24-hour clock.
    //
    // The offset now comes from the calendar AT THE PAUSE EXPIRY, not at
    // "now". A pause set in October and expiring in December was rendered
    // with October's offset, so it read an hour early across the November
    // transition, and for an evening expiry it named the wrong weekday.
    match cal.at(epoch) {
        Some(z) => z.format("%a %b %-d, %H:%M"),
        None => format!("epoch {epoch}"),
    }
}

/// NOAA Steadman simplified heat index, °F. Returns the input
/// temperature unchanged below 80 °F (where the Steadman regression is
/// unreliable / not meaningful).
pub fn heat_index_f(temp_f: f64, humidity_pct: f64) -> f64 {
    if temp_f < 80.0 {
        return temp_f;
    }
    let t = temp_f;
    let r = humidity_pct;
    -42.379 + 2.04901523 * t + 10.14333127 * r
        - 0.22475541 * t * r
        - 0.00683783 * t * t
        - 0.05481717 * r * r
        + 0.00122874 * t * t * r
        + 0.00085282 * t * r * r
        - 0.00000199 * t * t * r * r
}

/// ET multiplier from vapor pressure deficit, the atmosphere's drying
/// power.
///
/// 1.00 at or below 1.0 kPa, scaling linearly to 1.30 at 3.0 kPa.
///
/// This used to key off the NOAA heat index, which is a HUMAN COMFORT
/// measure and rises with humidity. Evapotranspiration does the
/// opposite: humid air is already close to saturated, so it pulls less
/// water out of a leaf. The old multiplier therefore pushed hardest
/// exactly where demand was lowest, and a dry 105 F afternoon in Phoenix
/// scored a LOWER heat index than a muggy 95 F one in Jacksonville while
/// evaporating considerably more.
///
/// VPD is the quantity FAO-56 puts in the Penman-Monteith numerator, and
/// it is what actually drives the demand this multiplier is reaching
/// for.
pub fn et_demand_multiplier(vpd_kpa: f64) -> f64 {
    let bonus = (((vpd_kpa - 1.0) / 2.0) * 0.30).clamp(0.0, 0.30);
    1.0 + bonus
}

/// The old heat-index multiplier, kept only so a caller that genuinely
/// has no VPD can still ask.
///
/// Prefer [`et_demand_multiplier`]. This one is wrong about humidity by
/// construction, for the reasons above.
pub fn et_heat_multiplier(heat_idx_f: f64) -> f64 {
    let bonus = (((heat_idx_f - 85.0) / 20.0) * 0.30).clamp(0.0, 0.30);
    1.0 + bonus
}

// ─────────────────────────────────────────────────────────────────────
// Operator-controllable built-in rules.
//
// `SkipRuleParams::disabled_rules` lists built-in rule ids the operator
// has switched off. A disabled rule still appears in the decision trace
// (transparency) but never decides. Operator-control and compliance
// gates are PROTECTED: the engine hard-enforces them regardless of
// config, so a hand-edited config can never disable a vacation pause,
// a manual override, dry-run, or a legal watering restriction.
// ─────────────────────────────────────────────────────────────────────

/// Rule ids that can never be disabled via `disabled_rules`. These are
/// the operator-control gates (override / pauses / dry-run) plus the
/// jurisdictional watering-restrictions compliance gate. Entries naming
/// them in config are silently ignored.
pub const PROTECTED_RULES: &[&str] = &[
    "restart_required",
    "override",
    "pause_until",
    "paused",
    "restrictions",
    "dry_run",
    // The live-data fail-safe (skip when no station AND no forecast) must
    // not be operator-disableable, or disabling it reintroduces deciding on
    // fabricated values. Hard-enforced like dry_run.
    "live_data",
    "soil_probe",
    "planning_forecast",
];

// builtin_rule_catalog lives in crate::gates_catalog (plain data, no
// ssr-only deps) so the WASM Rule Lab UI renders the same source of
// truth; re-exported here for the engine and its tests.
pub use crate::gates_catalog::builtin_rule_catalog;

/// The effective disable set: operator-listed ids minus the protected
/// ones. Unknown ids are harmless (they never match a gate).
fn disabled_set(p: &SkipRuleParams) -> HashSet<&str> {
    p.disabled_rules
        .iter()
        .map(String::as_str)
        .filter(|id| !PROTECTED_RULES.contains(id))
        .collect()
}

/// Back-compat entrypoint using `SkipRuleParams::default()`. Defaults
/// reproduce the v0.1 hardcoded thresholds.
impl Inputs {
    /// The instant this decision is about, or 0 when none is known.
    ///
    /// Derived, not stored. The old shape kept this next to an offset that
    /// could disagree with it; there is nothing left to disagree with.
    pub fn now_epoch(&self) -> i64 {
        self.when
            .zoned()
            .map(crate::engine::clock::Zoned::epoch)
            .unwrap_or(0)
    }
}

pub fn evaluate(i: &Inputs) -> SkipCheck {
    evaluate_with(i, &SkipRuleParams::default())
}

/// Full entrypoint with explicit rule parameters from config. The v2
/// scheduler passes `&cfg.engine.skip_rules` here.
pub fn evaluate_with(i: &Inputs, params: &SkipRuleParams) -> SkipCheck {
    let heat_index_now = heat_index_f(i.temp_now_f, i.humidity_now_pct);
    // The 3-day peak heat index is a PER-DAY forecast-derived input (each day's
    // high temp paired with THAT day's humidity), set by the refresher from
    // ForecastSnapshot::max_heat_index_n_day. Do NOT recompute it here as
    // heat_index_f(temp_max_3day_f, humidity_now_pct): that pairs the 3-day MAX
    // temp with the CURRENT (often saturated post-rain) humidity, a combination
    // that never co-occurs, and the Rothfusz regression overshoots to a bogus
    // ~147°F that then inflates the ET heat multiplier and the hero display.
    let heat_index_3day = i.heat_index_max_3day_f;

    let (verdict, reason, reason_code) = decide_with_code(i, params);

    SkipCheck {
        temp_now_f: i.temp_now_f,
        wind_now_mph: i.wind_now_mph,
        rain_today_in: i.rain_today_in,
        rain_today_forecast_in: i.rain_today_forecast_in,
        rain_intensity_now_in_hr: i.rain_intensity_now_in_hr,
        humidity_now_pct: i.humidity_now_pct,

        forecast_in: i.forecast_in,
        rain_tomorrow_prob_pct: i.rain_tomorrow_prob_pct,
        rain_3day_weighted_in: i.rain_3day_weighted_in,
        rain_7day_weighted_in: i.rain_7day_weighted_in,
        rain_next_4h_in: i.rain_next_4h_in,
        rain_observed_recent_in: i.rain_observed_recent_in,
        wind_max_today_mph: i.wind_max_today_mph,
        wind_window_max_mph: i.wind_window_max_mph,
        run_window: i.run_window,
        window_min_temp_f: i.window_min_temp_f,
        // Wire shape stays f64 for /api/v1 back-compat: missing data keeps
        // the historical 0.0 placeholder, with the new (additive) validity
        // flag alongside so consumers can tell 0 °F from "no forecast".
        temp_min_24h_f: i.temp_min_24h_f.unwrap_or(0.0),
        temp_min_24h_valid: i.temp_min_24h_f.is_some(),
        temp_max_3day_f: i.temp_max_3day_f,
        days_since_significant_rain: i.days_since_significant_rain,
        heat_index_now_f: heat_index_now,
        heat_index_max_3day_f: heat_index_3day,

        max_wind_mph: i.max_wind_mph,
        min_temp_f: i.min_temp_f,
        rain_skip_in: i.rain_skip_in,
        already_wet_in: params.already_wet_in,
        wind_forecast_slack_mph: params.wind_forecast_slack_mph,
        rain_observed_window_days: params.rain_observed_window_days,
        rain_next_4h_skip_in: params.rain_next_4h_skip_in,

        // Generalized per-zone soil: one "soil_<slug>_pct" + "saturation_
        // <slug>_pct" entry per configured zone (any slug, any count). The
        // manifest's per-zone soil descriptor reads these.
        soil_fields: build_soil_fields(&i.soil_zones),
        soil_probe_holds: probe_data_holds(i, params),
        planning_forecast_unavailable: i
            .soil_zones
            .iter()
            .filter(|zone| zone.planning_forecast_unavailable)
            .map(|zone| zone.slug.clone())
            .collect(),
        soil_probe_configured: i
            .soil_zones
            .iter()
            .map(|z| (z.slug.clone(), z.probe_configured))
            .collect(),
        soil_temp_yard_min_f: i.soil_temp_yard_min_f,
        soil_temp_yard_max_f: i.soil_temp_yard_max_f,
        frost_skip_soil_f: i.frost_skip_soil_f,

        is_paused: i.is_paused,
        is_dry_run: i.is_dry_run,

        script_hold: None,
        will_skip: verdict == "skip",
        verdict: verdict.to_string(),
        reason,
        // P1 (units architecture): the firing rule's stable id, additive +
        // invisible. "run" on a clean run; mirrors the verdict/reason above.
        reason_code: reason_code.to_string(),
    }
}

/// A complete engine answer. Live consumers receive built-in gates, scoped
/// conditions and user scripts together, so a new consumer cannot accidentally
/// treat a partially evaluated per-zone run as permission to water.
#[cfg(feature = "ssr")]
pub struct Decisions {
    pub skip_check: SkipCheck,
    pub trace: DecisionTrace,
    pub zones: Vec<ZoneVerdict>,
    pub force_overrode_guard: Option<String>,
}

#[cfg(feature = "ssr")]
pub fn evaluate_decisions(
    i: &Inputs,
    params: &SkipRuleParams,
    rules: &[ConditionRule],
    scripts: &CompiledScripts,
) -> Decisions {
    let mut answer = Decisions {
        skip_check: evaluate_with(i, params),
        trace: decide_traced(i, params),
        zones: decide_per_zone(i, params, rules),
        force_overrode_guard: force_overrode_guard(i, params),
    };
    let runnable = |verdict: &str| matches!(verdict, "run" | "run_extended");
    // Rhai reads yard-wide facts. Compute its first hold once, including when
    // a built-in gate already holds every zone: a manual weather waiver must
    // still receive the owner's independent script decision.
    answer.skip_check.script_hold = scripts.apply_user_skip(i);
    if let Some(user_skip) = answer.skip_check.script_hold.as_ref() {
        for zone in &mut answer.zones {
            if runnable(&zone.verdict) {
                zone.verdict = "skip".into();
                zone.reason = user_skip.reason.clone();
                zone.reason_code = user_skip.id.clone();
                zone.source = "script".into();
                zone.multiplier = 1.0;
                zone.value = None;
                zone.threshold = None;
            }
        }
        let decides_aggregate = runnable(&answer.skip_check.verdict);
        // Copy before updating the aggregate, which also owns the typed hold.
        let hold = user_skip.clone();
        if decides_aggregate {
            answer
                .skip_check
                .decide("skip", hold.reason.clone(), hold.id.clone());
            answer.trace.verdict = "skip".into();
            answer.trace.reason = hold.reason.clone();
            answer.trace.reason_code = hold.id.clone();
            for rule in &mut answer.trace.rules {
                if rule.outcome == "fired" && rule.overridden_by.is_none() {
                    rule.overridden_by = Some(hold.id.clone());
                    rule.overridden_detail = Some(hold.reason.clone());
                    rule.detail.push_str("; watering held by user script");
                }
            }
        }
        answer.trace.rules.push(RuleEval {
            id: hold.id,
            label: hold.name,
            category: "script".into(),
            detail: if decides_aggregate {
                hold.reason
            } else {
                format!(
                    "Additional script hold; an earlier gate already holds watering. {}",
                    hold.reason
                )
            },
            outcome: if decides_aggregate {
                "fired"
            } else {
                "skipped"
            }
            .into(),
            verdict: Some("skip".into()),
            ..Default::default()
        });
    }
    // The yard headline is a summary of the completed zone answers. A gate
    // that applies to sprinkler heads must not claim exempt beds are held,
    // and a runnable yard must not hide conditions that hold every zone.
    if !answer.zones.is_empty() {
        let running = answer.zones.iter().filter(|z| runnable(&z.verdict)).count();
        let held = answer.zones.len() - running;
        let extended = answer.zones.iter().any(|z| z.verdict == "run_extended");
        let aggregate_gate_holds_a_zone = answer
            .zones
            .iter()
            .any(|z| z.verdict == "skip" && z.reason_code == answer.skip_check.reason_code);
        if running == 0 && (runnable(&answer.skip_check.verdict) || !aggregate_gate_holds_a_zone) {
            if let Some(zone) = answer.zones.iter().find(|z| z.verdict == "skip") {
                answer.skip_check.decide(
                    "skip",
                    format!("All zones are holding. {}", zone.reason),
                    zone.reason_code.clone(),
                );
            }
        } else if running > 0 && (answer.skip_check.will_skip || held > 0) {
            let reason = if held > 0 {
                format!("{running} of {} zones can water; {held} remain on hold. Check each zone for its reason.", answer.zones.len())
            } else {
                "All zones can water after their own safety checks.".into()
            };
            answer.skip_check.decide(
                if extended { "run_extended" } else { "run" },
                reason,
                "run".into(),
            );
        } else if running > 0 && answer.skip_check.verdict == "run_extended" && !extended {
            answer.skip_check.decide(
                "run",
                "Zones keep their planned watering time.".into(),
                "run".into(),
            );
        }
        if answer.trace.verdict != answer.skip_check.verdict
            || answer.trace.reason != answer.skip_check.reason
        {
            for rule in &mut answer.trace.rules {
                if rule.outcome == "fired" && rule.overridden_by.is_none() {
                    rule.overridden_by = Some(answer.skip_check.reason_code.clone());
                    rule.overridden_detail = Some(answer.skip_check.reason.clone());
                    rule.detail.push_str("; applicability resolved per zone");
                }
            }
            answer.trace.verdict = answer.skip_check.verdict.clone();
            answer.trace.reason = answer.skip_check.reason.clone();
            answer.trace.reason_code = answer.skip_check.reason_code.clone();
            if answer.skip_check.will_skip {
                // A script that initially sat behind a yard forecast gate may
                // become the deciding hold after scoped soil-model evaluation.
                // Promote its existing row rather than duplicate the rule id.
                if let Some(script_row) = answer
                    .trace
                    .rules
                    .iter_mut()
                    .find(|r| r.category == "script" && r.id == answer.skip_check.reason_code)
                {
                    script_row.outcome = "fired".into();
                    script_row.detail = answer.skip_check.reason.clone();
                } else {
                    answer.trace.rules.push(RuleEval {
                        id: answer.skip_check.reason_code.clone(),
                        label: "All zones are holding".into(),
                        category: "condition".into(),
                        detail: answer.skip_check.reason.clone(),
                        outcome: "fired".into(),
                        verdict: Some("skip".into()),
                        ..Default::default()
                    });
                }
            }
        }
    }
    answer
}

/// Aggregate rule ladder. Order matters: first matching rule wins. Order
/// is override > paused > restriction > weather-safety > soil-saturation >
/// rain-forecast > heat-advisory > dry-run > run. Composed from three
/// pieces so the per-zone path (`decide_per_zone`) can reuse the global
/// gates while substituting its own per-zone soil logic.
/// The canonical (verdict, reason) decision. Production reads `decide_with_code`
/// (which also yields the P1 reason_code); `decide` drops the code and is the
/// (verdict, reason) twin the parity tests assert `decide_traced` against. It is
/// only referenced by tests now, so it is gated to test builds, keeping it as the
/// stable parity anchor without a dead-code warning in the binary.
#[cfg(test)]
fn decide(i: &Inputs, p: &SkipRuleParams) -> (&'static str, String) {
    let (v, r, _code) = decide_with_code(i, p);
    (v, r)
}

/// `decide` + the stable id of the FIRING rule (P1 units architecture). The
/// reason_code is `"run"` on a clean run, `"soil_floor"` when the dry-soil moat
/// demotes a soft rain skip to a run, else the firing gate's id (mirroring
/// `RuleEval.id` / the gates catalog). ADDITIVE: byte-identical verdict + reason
/// to `decide`; the code is the only new output and never affects the decision.
/// `decide` delegates here so the two can never drift.
fn decide_with_code(i: &Inputs, p: &SkipRuleParams) -> (&'static str, String, &'static str) {
    let disabled = disabled_set(p);
    let effective = with_effective_soil(i, p);
    decide_ladder(&effective, p, &disabled)
}

/// The one deterministic ladder, for both the yard and an individual zone.
/// The caller supplies effective soil and the restrictions/gates applicable to
/// its scope. Changing scope never bypasses the rest of the ladder.
fn decide_ladder(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
) -> (&'static str, String, &'static str) {
    if let Some(v) = pre_soil(i, p, disabled) {
        return v;
    }
    if let Some(v) = soil_saturation(i, disabled) {
        return v;
    }
    // Soil-floor (the moat): a soft forecast-rain skip is demoted to a run when a
    // measured-dry zone needs water. `will_skip` then becomes false, bypassing the
    // dispatcher's blanket-skip early-return; the per-zone layer (decide_per_zone)
    // runs the dry zones and skips the wet ones. dry_run / hard skips are never
    // demotable (soil_floor_demotes is false for them). Code is "soil_floor" (the
    // moat rung), matching the soil_floor gate that fires in decide_traced.
    if soil_floor_demotes(i, p, disabled) {
        return ("run", String::new(), "soil_floor");
    }
    post_soil(i, p, disabled, false)
}

/// This zone's OWN soil-saturation skip, judged on the effective (post-
/// quarantine) soil, or `None` when the probe is silent, the operator
/// disabled the gate, or the ground is not saturated.
///
/// Extracted so both paths through `decide_per_zone` that end in a run can
/// consult it. The exempt path used to return "run" without it, which was
/// invisible while the dispatcher blanket-held the yard on any aggregate
/// skip and became a saturated-ground dispatch the moment the dispatcher
/// started honoring exemptions.
///
/// `z` supplies identity (slug, name), `eff_z` the reading being judged;
/// they are the same zone from `i.soil_zones` and the effective-soil copy.
fn zone_saturation_skip(
    z: &ZoneSoil,
    eff_z: &ZoneSoil,
    disabled: &HashSet<&str>,
) -> Option<ZoneVerdict> {
    let pct = eff_z
        .pct
        .filter(|_| !disabled.contains("soil_saturation"))?;
    if pct < eff_z.saturation_pct {
        return None;
    }
    let reason = format!(
        "Soil saturated ({:.0}% \u{2265} {:.0}% threshold)",
        pct, eff_z.saturation_pct
    );
    let source = "soil_saturation";
    Some(ZoneVerdict {
        zone_slug: z.slug.clone(),
        zone_name: z.name.clone(),
        verdict: "skip".into(),
        reason,
        source: source.into(),
        multiplier: 1.0,
        reason_code: source.into(),
        value: Some(pct),
        threshold: Some(eff_z.saturation_pct),
    })
}

/// Per-zone decisions run the same ladder as the yard. The scope supplies one
/// zone's effective soil and only the restrictions that bind it. A soil-model
/// zone removes already-accounted forecast-rain gates, then re-runs the entire
/// ladder, including holds and its own soil saturation. Every ordinary run
/// reaches the owner's condition rules before it can leave the engine.
///
/// Explicit overrides retain their separate owner-control semantics. This
/// built-in-only entrypoint serves simulations/tests; live assembly must use
/// `evaluate_decisions` so user scripts reach the same verdicts as dispatch.
pub fn decide_per_zone(
    i: &Inputs,
    p: &SkipRuleParams,
    rules: &[ConditionRule],
) -> Vec<ZoneVerdict> {
    let disabled = disabled_set(p);
    let plan = quarantine_plan(&i.soil_zones, p);
    let effective = effective_soil_zones(&i.soil_zones, &plan);
    let (_, yard_reason, yard_code) = decide_with_code(i, p);
    i.soil_zones
        .iter()
        .zip(effective.iter())
        .zip(plan.iter())
        .map(|((z, eff_z), quarantine)| {
            let scope = restrictions::ZoneScope {
                slug: &z.slug,
                sprinkler: z.sprinkler_type,
            };
            let mut own = Inputs {
                soil_zones: vec![eff_z.clone()],
                ..i.clone()
            };
            own.watering_restrictions
                .retain(|r| restrictions::applies_to_zone(r, Some(scope)));
            let override_scope = match i.zone_overrides.get(&z.slug).map(String::as_str) {
                Some("run") if i.global_override == "skip" => "global",
                Some(value @ ("skip" | "run")) => {
                    own.global_override = value.into();
                    "this zone"
                }
                _ => "global",
            };
            let original = decide_ladder(&own, p, &disabled);
            let mut applicable = disabled.clone();
            if z.governed_by_soil_model {
                applicable.extend(
                    SOIL_MODEL_INERT_GATES
                        .iter()
                        .copied()
                        .filter(|gate| soil_forecast_accounted(&own, gate)),
                );
                applicable.insert("heat_advisory");
            }
            let (verdict, reason, code) = decide_ladder(&own, p, &applicable);
            let mut result = ZoneVerdict {
                zone_slug: z.slug.clone(),
                zone_name: z.name.clone(),
                verdict: verdict.into(),
                reason,
                source: "global".into(),
                multiplier: 1.0,
                reason_code: code.into(),
                value: None,
                threshold: None,
            };
            if code == "override" {
                result.source = "override".into();
                if matches!(own.global_override.as_str(), "skip" | "run") {
                    let action = if verdict == "skip" {
                        "skip"
                    } else {
                        "force run"
                    };
                    result.reason = format!("Override: {action} ({override_scope})");
                }
                return result;
            }
            if code == "soil_saturation" {
                // Preserve the zone's measured provenance and operands;
                // eligibility was decided by the same saturation rung as the yard.
                if let Some(saturated) = zone_saturation_skip(z, eff_z, &applicable) {
                    return saturated;
                }
            }
            if code == "soil_probe" {
                result.source = "soil_quarantine".into();
                if let Some(q) = quarantine {
                    result.reason = quarantine_reason(q);
                }
            }
            if verdict == "skip" {
                return result;
            }
            if code == "soil_floor" {
                if let Some(pct) = zone_healthy_dry(eff_z) {
                    let soft_id =
                        demotable_soft_skip_id(&own, p, &applicable).unwrap_or("rain_next_4h");
                    result.reason = format!(
                        "Soil {:.0}% < {:.0}% minimum; {} skip overridden",
                        pct,
                        z.target_min_pct,
                        soft_rain_label(soft_id)
                    );
                    result.source = "soil_floor".into();
                    result.value = Some(pct);
                    result.threshold = Some(z.target_min_pct);
                }
            } else if yard_code == "restrictions" {
                result.reason =
                    format!("Exempt from the restriction holding the yard. ({yard_reason})");
                result.source = "exempt".into();
                result.reason_code = "restrictions".into();
            } else if z.governed_by_soil_model
                && original.0 == "skip"
                && SOIL_MODEL_INERT_GATES.contains(&original.2)
            {
                result.reason = format!(
                    "Waters anyway: soil zones already count this forecast rain \
                     against their deficit. ({})",
                    original.1
                );
                result.source = "soil_model".into();
                result.reason_code = original.2.into();
            } else if z.governed_by_soil_model && original.2 == "heat_advisory" {
                result.reason = format!(
                    "Runs normally: measured water use already charges hot days into \
                     the soil deficit. ({})",
                    original.1
                );
                result.source = "soil_model".into();
                result.reason_code = "soil_model".into();
            }
            let outcome = apply_zone_rules(rules, &ConditionCtx { i, zone: z });
            if let Some((_, reason)) = outcome.skip {
                result.verdict = "skip".into();
                result.reason = reason;
                result.source = "condition".into();
                result.reason_code = "condition".into();
                result.value = None;
                result.threshold = None;
                return result;
            }
            if outcome.extend {
                result.verdict = "run_extended".into();
            }
            result.multiplier = outcome.multiplier;
            if result.source == "global"
                && (outcome.extend || (outcome.multiplier - 1.0).abs() > 1e-9)
            {
                result.source = "condition".into();
                result.reason_code = "condition".into();
            }
            result
        })
        .collect()
}

/// Rain/soil recommendations a convenience force-run can set aside. The list
/// is deliberately positive: a future safety or operator-control gate remains
/// binding without a caller remembering to protect it.
const FORCE_RUN_BYPASSED_GATES: &[&str] = &[
    "rain_now",
    "already_wet",
    "rain_today_forecast",
    "observed_rain",
    "soil_saturation",
    "rain_next_4h",
    "tomorrow_rain",
    "rain_3day",
    "soil_floor",
];

fn without_force_run(i: &Inputs) -> Inputs {
    let mut probe = i.clone();
    if probe.global_override == "run" {
        probe.global_override = "auto".into();
    }
    if probe.override_tomorrow == "run" {
        probe.override_tomorrow = "auto".into();
    }
    probe
}

fn force_run_block(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
) -> Option<(&'static str, String, &'static str)> {
    let probe = without_force_run(i);
    let mut applicable = disabled.clone();
    applicable.extend(FORCE_RUN_BYPASSED_GATES.iter().copied());
    let decision = decide_ladder(&probe, p, &applicable);
    (decision.0 == "skip").then_some(decision)
}

/// Gates that run before the soil-saturation block: override, pause,
/// restriction, rain-now, freeze, soil-frost, wind, already-wet. `Some`
/// = a gate fired (first wins); `None` = fall through to soil/weather.
/// The control + restriction gates ignore `disabled` (PROTECTED_RULES,
/// hard-enforced); every weather/safety gate consults it.
const PLANNING_FORECAST_HOLD_REASON: &str =
    "Rain forecast unavailable for the watering plan's next 24 hours; watering held";
fn planning_forecast_unavailable(i: &Inputs) -> bool {
    !i.soil_zones.is_empty()
        && i.soil_zones
            .iter()
            .all(|zone| zone.planning_forecast_unavailable)
}

fn pre_soil(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
) -> Option<(&'static str, String, &'static str)> {
    if i.restart_required {
        return Some((
            "skip",
            crate::gates_catalog::RESTART_REQUIRED_REASON.to_string(),
            "restart_required",
        ));
    }
    // Force is a rain/soil convenience override, not an implicit safety waiver.
    // Re-evaluate the same ladder's safety and hold gates before allowing it.
    match i.global_override.as_str() {
        "skip" => return Some(("skip", "Manual override: skip".to_string(), "override")),
        "run" => {
            return force_run_block(i, p, disabled)
                .or_else(|| Some(("run", "Manual override: force run".to_string(), "override")))
        }
        _ => {}
    }
    if i.is_tomorrow {
        match i.override_tomorrow.as_str() {
            "skip" => {
                return Some((
                    "skip",
                    "Manual override (skip tomorrow)".to_string(),
                    "override",
                ))
            }
            "run" => {
                return force_run_block(i, p, disabled)
                    .or_else(|| Some(("run", String::new(), "override")))
            }
            _ => {}
        }
    }
    if i.pause_until_epoch > 0 && i.now_epoch() > 0 && i.now_epoch() < i.pause_until_epoch {
        let until = format_pause_until(i.pause_until_epoch, i.calendar);
        return Some((
            "skip",
            format!("Paused (vacation until {until})"),
            "pause_until",
        ));
    }
    if i.is_paused {
        return Some(("skip", "Paused (vacation mode)".to_string(), "paused"));
    }
    // Phase C: regulatory / HOA watering restrictions. Evaluated against
    // `now_epoch` interpreted as local time so the DST-vs-EST window math
    // matches the operator's clock. Runs before all weather gates so the
    // verdict reason explains the legal block, not the weather.
    // TZ: interpret now_epoch in the CONFIGURED deployment timezone (via
    // timeutil), not chrono::Local (the container TZ). A container left at
    // UTC would otherwise shift every weekday/parity/forbidden-hours window
    // and could water on a legally banned day or block every legal morning.
    // Same fix as format_pause_until above.
    if !i.watering_restrictions.is_empty() && i.when.is_known() {
        {
            let v = restrictions::evaluate_for(
                i.when,
                &i.watering_restrictions,
                i.address_parity,
                &i.watered_days,
                None,
            );
            if v.skip {
                return Some((
                    "skip",
                    v.reason
                        .unwrap_or_else(|| "Watering restriction".to_string()),
                    "restrictions",
                ));
            }
        }
    }
    // Live-data integrity. When neither the station nor the forecast can
    // supply current conditions, the freeze/wind gates below would be
    // judging fabricated numbers. Prefer a skip over a phantom run.
    if !disabled.contains("live_data") && i.live_readings == LiveReadings::Unavailable {
        return Some((
            "skip",
            "Live weather unavailable (no station data or forecast); failing safe".to_string(),
            "live_data",
        ));
    }
    if soil_probe_unavailable(i) {
        return Some(("skip", SOIL_PROBE_HOLD_REASON.to_string(), "soil_probe"));
    }
    if planning_forecast_unavailable(i) {
        return Some((
            "skip",
            PLANNING_FORECAST_HOLD_REASON.into(),
            "planning_forecast",
        ));
    }
    // Currently raining, HARD tier: an OBSERVATION-GRADE rain reading (a LAN
    // gauge, NWS observation, or MRMS radar QPE) actively over the threshold is
    // ground truth. It binds every zone and is ordered here in pre_soil BEFORE the
    // soil_floor moat, so a dry zone cannot run while it is measurably raining. A
    // MODEL rain rate does NOT fire here; it is routed to the demotable soft tier
    // (rain_now_model_fires, post_soil + SOIL_FLOOR_DEMOTABLE) so a measured-dry
    // zone / soil_floor can override a mere forecast estimate. The reason string
    // stays the stable "Currently raining (...)" the unit-aware renderer mirrors;
    // the honest rain NATURE travels on the snapshot's Forecast.rain_nature badge.
    if rain_now_hard_fires(i, p, disabled) {
        return Some(("skip", rain_now_reason(i), "rain_now"));
    }
    if !disabled.contains("freeze_now") {
        let (t, when) = freeze_on_trial(i);
        if t < i.min_temp_f {
            return Some((
                "skip",
                format!("Freeze risk {when} ({t:.0}°F < {:.0}°F)", i.min_temp_f),
                "freeze_now",
            ));
        }
    }
    // Applicability is "do we have a forecast low at all" (Option), not a
    // numeric sentinel: a genuine low of 0 °F or colder must still skip.
    //
    // A post-sunrise window is exempt: it was chosen because the morning
    // was freezing, and it was chosen for clearing the threshold through
    // the run and the hours after it. The coming night's low is not what
    // a mid-morning run is exposed to.
    if let Some(t24) = i
        .temp_min_24h_f
        .filter(|_| !disabled.contains("overnight_freeze") && overnight_freeze_applies(i))
    {
        if t24 < i.min_temp_f {
            return Some((
                "skip",
                format!(
                    "Overnight freeze ({:.0}°F low next 24h < {:.0}°F)",
                    t24, i.min_temp_f
                ),
                "overnight_freeze",
            ));
        }
    }
    if let Some(t) = i
        .soil_temp_yard_min_f
        .filter(|_| !disabled.contains("soil_frost"))
    {
        if t < i.frost_skip_soil_f {
            return Some((
                "skip",
                format!(
                    "Soil frost ({:.1}°F < {:.0}°F threshold)",
                    t, i.frost_skip_soil_f
                ),
                "soil_frost",
            ));
        }
    }
    if !disabled.contains("wind_now") && i.wind_now_mph > i.max_wind_mph {
        return Some((
            "skip",
            format!(
                "Wind too high now ({:.1} mph > {:.0} mph)",
                i.wind_now_mph, i.max_wind_mph
            ),
            "wind_now",
        ));
    }
    if !disabled.contains("wind_forecast") {
        let (peak, scope) = wind_on_trial(i);
        if peak > i.max_wind_mph + p.wind_forecast_slack_mph {
            return Some((
                "skip",
                wind_forecast_reason(scope, peak, i.max_wind_mph, p.wind_forecast_slack_mph),
                "wind_forecast",
            ));
        }
    }
    // MEASURED rain only. This gate is reactive: it answers "did enough
    // water already land on this yard", and the only thing that can
    // answer it is something that caught rain. The modelled figure gets
    // its own rung below so a gaugeless install still skips, but says
    // what kind of number it is skipping on.
    if !disabled.contains("already_wet") && i.rain_today_in >= p.already_wet_in {
        return Some((
            "skip",
            format!("Already wet ({:.2}\" measured today)", i.rain_today_in),
            "already_wet",
        ));
    }
    // The same threshold against the model's own day total, for an
    // install with no gauge. Named and worded as a forecast, and listed
    // in SOIL_MODEL_INERT_GATES, because a soil-governed zone has
    // already counted this rain against its own deficit and must not be
    // held twice for it.
    if !disabled.contains("rain_today_forecast")
        && forecast_rain_fires(i.rain_today_forecast_in, p.already_wet_in, false)
    {
        return Some(("skip", rain_today_forecast_reason(i), "rain_today_forecast"));
    }
    // OBSERVED-recent-rain backstop (sensor-independent). A HARD skip ordered
    // here in pre_soil, BEFORE soil_saturation and the soil_floor moat, so heavy
    // measured rain over the recent window binds every zone and a dry zone cannot
    // run right after it (the soil_floor override only demotes the three
    // forward-looking forecast-rain gates, never this measured one).
    if rain_observed_recent_fires(i, p, disabled) {
        return Some((
            "skip",
            format!(
                "Already wet ({:.2}\" rain in the last {} day(s))",
                i.rain_observed_recent_in,
                p.rain_observed_window_days + 1
            ),
            "observed_rain",
        ));
    }
    None
}

/// The yard-wide soil-saturation gate (aggregate view): skip only when
/// EVERY configured zone has a soil reading AND all are at/above their
/// saturation threshold. Generalized from the former hardcoded 4-zone
/// array to iterate `i.soil_zones`. `None` when not all zones report or
/// any zone is below threshold.
fn soil_saturation(
    i: &Inputs,
    disabled: &HashSet<&str>,
) -> Option<(&'static str, String, &'static str)> {
    if disabled.contains("soil_saturation") {
        return None;
    }
    if i.soil_zones.is_empty() || i.soil_zones.iter().any(|z| z.pct.is_none()) {
        return None;
    }
    if i.soil_zones
        .iter()
        .all(|z| z.pct.unwrap() >= z.saturation_pct)
    {
        let tightest = i
            .soil_zones
            .iter()
            .min_by(|a, b| {
                let am = a.pct.unwrap() - a.saturation_pct;
                let bm = b.pct.unwrap() - b.saturation_pct;
                am.partial_cmp(&bm).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        return Some((
            "skip",
            format!(
                "All zones soil-saturated (tightest: {} {:.0}% ≥ {:.0}% threshold)",
                tightest.name,
                tightest.pct.unwrap(),
                tightest.saturation_pct
            ),
            "soil_saturation",
        ));
    }
    None
}

/// Soft forecast-rain skips that a measured-dry zone may demote to a run (the
/// "soil floor" / moat). Structurally these are EXACTLY the skips reachable in
/// `post_soil` (every hard skip returns from pre_soil/soil_saturation first); the
/// explicit list makes the moat's scope test-pinnable and impossible to silently
/// widen. Pinned by `soil_floor_demotable_is_post_soil_rain_and_not_protected`.
/// `rain_now` leads the list: a MODEL-grade "currently raining" estimate is the
/// most immediate soft rain skip, demotable exactly like the forward-looking
/// forecast-rain gates (an observation-grade rain_now is a HARD pre_soil skip and
/// is NOT in this set, so it is never demoted). The three forecast gates follow.
const SOIL_FLOOR_DEMOTABLE: &[&str] = &["rain_now", "rain_next_4h", "tomorrow_rain", "rain_3day"];

/// The temperature the freeze gate judges, and when it is for.
///
/// The pre-dawn window is judged on the live reading: the run is now.
/// A post-sunrise window was planned ahead for clearing the threshold,
/// so it is judged on the forecast minimum across its own hours; the
/// live reading at the pre-dawn refresh is exactly the freezing hour the
/// window was moved to avoid.
fn freeze_on_trial(i: &Inputs) -> (f64, &'static str) {
    use crate::engine::dispatch_window::WindowKind;
    match (i.run_window, i.window_min_temp_f) {
        (WindowKind::PostSunrise, Some(t)) => (t, "during the run"),
        _ => (i.temp_now_f, "now"),
    }
}

/// The coming night's low binds a pre-dawn run. A post-sunrise window
/// was chosen for clearing the threshold through the run and the hours
/// after it, and a mid-morning run is not exposed to the next night.
fn overnight_freeze_applies(i: &Inputs) -> bool {
    i.run_window == crate::engine::dispatch_window::WindowKind::PreDawn
}

/// Which minutes the wind forecast gate is judging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindScope {
    /// The hours the yard plans to water, from the hourly series.
    RunWindow,
    /// The whole day, because no run window is knowable.
    Day,
}

impl WindScope {
    fn detail(self) -> &'static str {
        match self {
            WindScope::RunWindow => "in the run window",
            WindScope::Day => "today",
        }
    }
}

/// The wind figure the forecast gate judges, and where it came from.
///
/// The run window's own peak when the hourly series covers it, the day's
/// peak otherwise. The daily peak is the whole day's worst hour, almost
/// always an afternoon, and a yard that finishes before sunrise should
/// not be refused for it.
fn wind_on_trial(i: &Inputs) -> (f64, WindScope) {
    match i.wind_window_max_mph {
        Some(w) => (w, WindScope::RunWindow),
        None => (i.wind_max_today_mph, WindScope::Day),
    }
}

fn wind_forecast_reason(scope: WindScope, peak: f64, max: f64, slack: f64) -> String {
    match scope {
        WindScope::RunWindow => format!(
            "Windy while the yard waters (peak {peak:.0} mph in the run window > {max:.0} + {slack:.0})"
        ),
        WindScope::Day => {
            format!("Windy day forecast (peak {peak:.0} mph > {max:.0} + {slack:.0})")
        }
    }
}

/// The three forward-looking rain SKIP conditions, factored out so `post_soil`,
/// `decide_traced`, and the soil-floor classifier all read ONE source of truth
/// (no drift between the aggregate ladder and the per-zone veto). Each is the
/// exact condition the matching gate fires on, `disabled` membership included so
/// the predicates are self-contained.
// Missing forecast amounts are an unavailable hold in the SAME rain rung.
// The existing per-zone soil/force scopes can therefore waive that rung
// deliberately; an absent forecast never turns into a new global safety gate.
fn soil_forecast_accounted(i: &Inputs, gate: &str) -> bool {
    if i.forecast_stale || planning_forecast_unavailable(i) {
        return false;
    }
    let amount = match gate {
        "rain_today_forecast" => i.rain_today_forecast_in,
        "rain_next_4h" => i.rain_next_4h_in,
        "tomorrow_rain" => i.forecast_in,
        "rain_3day" => i.rain_3day_weighted_in,
        _ => None,
    };
    amount.is_some_and(|value| value.is_finite() && value >= 0.0)
}

fn forecast_rain_fires(amount: Option<f64>, threshold: f64, stale: bool) -> bool {
    amount.is_none_or(|amount| !stale && amount >= threshold)
}

struct RainAmount(Option<f64>);
impl std::fmt::Display for RainAmount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(value) => std::fmt::Display::fmt(&value, f),
            None => f.write_str("unknown"),
        }
    }
}
fn rain_amount(value: Option<f64>) -> RainAmount {
    RainAmount(value)
}

fn rain_now_reason(i: &Inputs) -> String {
    match i.rain_intensity_now_in_hr {
        Some(rate) => format!("Currently raining ({rate:.2} in/hr)"),
        None => "Current rain estimate unavailable; watering held".into(),
    }
}
fn rain_today_forecast_reason(i: &Inputs) -> String {
    match i.rain_today_forecast_in {
        Some(amount) => format!("Rain forecast today ({amount:.2}\" expected, not measured)"),
        None => "Today's rain forecast unavailable; watering held".into(),
    }
}
fn rain_next_4h_reason(i: &Inputs) -> String {
    match i.rain_next_4h_in {
        Some(amount) => format!("Rain expected within 4h ({amount:.2}\" forecast)"),
        None => "Rain forecast unavailable for the next 4 hours; watering held".into(),
    }
}
fn rain_3day_reason(i: &Inputs) -> String {
    match i.rain_3day_weighted_in {
        Some(amount) => format!("Heavy rain in next 3 days ({amount:.2}\" weighted)"),
        None => "Rain forecast unavailable for the next 3 days; watering held".into(),
    }
}

fn rain_next_4h_fires(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    !disabled.contains("rain_next_4h")
        && forecast_rain_fires(i.rain_next_4h_in, p.rain_next_4h_skip_in, i.forecast_stale)
}
/// Probability weight for tomorrow's forecast rain, 0.0..=1.0. `None` (the
/// provider reports no probability) weights the amount at FULL value:
/// treating forecast rain as certain is the conservative direction for a
/// skip decision (hold water ahead of a forecast storm), where the old
/// missing-equals-0 zeroed the expected rain and watered ahead of it. A
/// reported 0 stays a real "the model says dry" and still zeroes the gate.
fn tomorrow_prob_weight(i: &Inputs) -> f64 {
    i.rain_tomorrow_prob_pct
        .map(|p| f64::from(p) / 100.0)
        .unwrap_or(1.0)
}

/// The tomorrow-rain skip reason. Quotes a confidence only when the provider
/// actually reported one; with no probability the string claims only the
/// forecast amount (which the gate weighted at full value).
fn tomorrow_rain_reason(i: &Inputs) -> String {
    let Some(amount) = i.forecast_in else {
        return "Tomorrow's rain forecast unavailable; watering held".into();
    };
    match i.rain_tomorrow_prob_pct {
        Some(p) => format!("Tomorrow rain ({amount:.2}\" × {p}% confidence)"),
        None => format!("Tomorrow rain ({amount:.2}\" forecast)"),
    }
}

fn tomorrow_rain_fires(i: &Inputs, _p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    let weighted = i.forecast_in.map(|amount| amount * tomorrow_prob_weight(i));
    !disabled.contains("tomorrow_rain")
        && forecast_rain_fires(weighted, i.rain_skip_in, i.forecast_stale)
}
fn rain_3day_fires(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    !disabled.contains("rain_3day")
        && forecast_rain_fires(
            i.rain_3day_weighted_in,
            p.rain_3day_factor * i.rain_skip_in,
            i.forecast_stale,
        )
}
/// OBSERVED-recent-rain backstop. Fires when measured rain over the recent
/// window (today + the configured past days) reaches the user `rain_skip_in`
/// threshold. Unlike the three forward-looking rain gates this reads PAST
/// measured rain, so it is NOT gated on `forecast_stale` (an Open-Meteo outage
/// cannot fabricate observed rain) and it is a HARD skip in `pre_soil`: it binds
/// every zone and is evaluated before the soil_floor moat, so a dry zone cannot
/// run right after heavy observed rain even when its soil probe is bad/offline.
fn rain_observed_recent_fires(i: &Inputs, _p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    !disabled.contains("observed_rain") && i.rain_observed_recent_in >= i.rain_skip_in
}

/// Whether the current-rain reading is OBSERVATION-GRADE (a real measured or
/// radar-measured rain, not a model estimate). True for `Measured` (a LAN gauge
/// or NWS observation) and `RadarQpe` (NOAA MRMS radar QPE); false for `Model`
/// (a forecast fill). This is the single discriminator the "currently raining"
/// gate uses to decide HARD vs SOFT skip.
fn rain_now_is_observation_grade(i: &Inputs) -> bool {
    matches!(i.rain_nature, RainNature::Measured | RainNature::RadarQpe)
}

/// The "currently raining" rate is over the skip threshold (the raw rate
/// condition both the hard and soft tiers share). Honors the `rain_now` disable
/// id so the whole gate (either tier) goes inert when the operator turns it off.
fn rain_now_rate_fires(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    !disabled.contains("rain_now")
        && i.rain_intensity_now_in_hr
            .is_none_or(|rate| rate > p.rain_now_in_hr)
}

/// The "currently raining" gate as a HARD skip: the rate is over threshold AND
/// the rain is observation-grade (a real gauge / NWS observation / MRMS radar).
/// A hard skip binds every zone and beats the soil_floor moat: measured rain
/// falling right now is ground truth, not a forecast a dry zone may override.
fn rain_now_hard_fires(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    rain_now_rate_fires(i, p, disabled) && rain_now_is_observation_grade(i)
}

/// The "currently raining" gate as a SOFT (demotable) skip: the rate is over
/// threshold but the rain is only a MODEL estimate (`Model` nature, e.g.
/// Open-Meteo / Met.no / Pirate-rain current-hour precip). A model rain rate is
/// not ground truth, so a measured-dry zone (or the soil_floor moat) may demote
/// it to a run, exactly like the forward-looking forecast-rain gates. Shares the
/// `rain_now` id (no new catalog gate); the demotion infra routes it through the
/// moat instead of the hard pre-soil tier.
fn rain_now_model_fires(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    rain_now_rate_fires(i, p, disabled) && !rain_now_is_observation_grade(i)
}

/// Shared heat eligibility for the deciding ladder and its trace. Humidity
/// affects the heat-index multiplier, not whether a dry heat wave needs water.
fn heat_advisory_applies(i: &Inputs, p: &SkipRuleParams) -> bool {
    i.temp_max_3day_f >= p.heat_advisory_temp_f
        && i.days_since_significant_rain >= p.heat_advisory_dry_days
        && i.rain_3day_weighted_in
            .is_some_and(|rain| rain < 0.5 * i.rain_skip_in)
}

/// The moat's core per-zone test. A zone may demote a soft forecast-rain skip to
/// a RUN only when its probe is HEALTHY (present AND non-zero) AND measured soil
/// is strictly below the zone's dry floor (`target_min_pct`). Returns
/// `Some(measured_pct)` when the veto applies, else `None`.
///
/// Fail-safe by construction: a missing/unassigned probe is `None`; a flatlined
/// dead probe reads `0.0` (mapped to `None` upstream by `apply_soil_quality`, and
/// double-guarded here by `> 0.0`); a stale probe is nulled to `None` in the
/// refresher's `resolve_soil_zones`. All three unhealthy signals collapse to
/// `pct == None`, so the soft skip stands. A saturated zone is mechanically
/// `pct >= saturation >= target_min`, so it can never be healthy-dry. A
/// `target_min_pct` of 0 (no floor configured) also yields `None` (no veto).
fn zone_healthy_dry(z: &ZoneSoil) -> Option<f64> {
    match z.pct {
        Some(pct) if pct > 0.0 && pct < z.target_min_pct => Some(pct),
        _ => None,
    }
}

/// True when ANY configured zone is healthy-dry (the aggregate mirror of
/// `zone_healthy_dry`). Gated by the operator `soil_floor` disable id so turning
/// the rung off restores forecast-only behavior everywhere.
fn any_zone_healthy_dry(i: &Inputs, disabled: &HashSet<&str>) -> bool {
    !disabled.contains("soil_floor") && i.soil_zones.iter().any(|z| zone_healthy_dry(z).is_some())
}

/// If the yard-wide decision would be a soft forecast-rain skip a measured-dry
/// zone may demote, return its rule id; else `None`. Reachability-correct: only
/// `Some` when no hard skip (pre_soil) and no yard saturation fired first,
/// mirroring `post_soil`'s order. Single-sourced from the same rain predicates
/// the gates use, so it can never drift from the ladder.
fn demotable_soft_skip_id(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
) -> Option<&'static str> {
    if pre_soil(i, p, disabled).is_some() {
        return None;
    }
    if soil_saturation(i, disabled).is_some() {
        return None;
    }
    // First firing soft-rain gate, in post_soil order. The ids are sourced from
    // SOIL_FLOOR_DEMOTABLE (zipped with the predicates in the same order) so the
    // allow-list is the single source of the demotable scope. `rain_now` leads:
    // a model-grade "currently raining" estimate is demotable; an observation-
    // grade one is a hard pre_soil skip and short-circuits above.
    let fires = [
        rain_now_model_fires(i, p, disabled),
        rain_next_4h_fires(i, p, disabled),
        tomorrow_rain_fires(i, p, disabled),
        rain_3day_fires(i, p, disabled),
    ];
    SOIL_FLOOR_DEMOTABLE
        .iter()
        .zip(fires)
        .find_map(|(id, fired)| fired.then_some(*id))
}

/// True when the soil-floor rung demotes the yard's soft forecast-rain skip to a
/// run: some zone is healthy-dry, the would-be skip is a demotable soft rain
/// skip, AND removing that rain skip actually leaves a run (so `dry_run` or any
/// later post_soil skip still wins). Shared by `decide`, `decide_per_zone`, and
/// `decide_traced` so all three agree by construction.
fn soil_floor_demotes(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    any_zone_healthy_dry(i, disabled)
        && demotable_soft_skip_id(i, p, disabled).is_some()
        && post_soil(i, p, disabled, true).0 != "skip"
}

/// Human label for the demoted soft-rain rule, for per-zone provenance.
fn soft_rain_label(id: &str) -> &'static str {
    match id {
        "rain_now" => "current model-rain estimate",
        "rain_next_4h" => "4h forecast rain",
        "tomorrow_rain" => "tomorrow forecast rain",
        "rain_3day" => "3-day forecast rain",
        _ => "forecast rain",
    }
}

/// Gates that run after soil saturation: model-grade currently-raining, rain-
/// within-4h, tomorrow rain, 3-day rain, heat advisory, dry-run, default run. The
/// dry-run gate ignores `disabled` (PROTECTED_RULES, hard-enforced).
/// `floor_active` suppresses the soft rain SKIPs (the soil-floor demotion);
/// callers pass `false` for the normal verdict and `true` only to probe the
/// rain-removed hypothetical inside `soil_floor_demotes`.
fn post_soil(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
    floor_active: bool,
) -> (&'static str, String, &'static str) {
    // MODEL-grade "currently raining" (soft tier): the live rain reading is over
    // the threshold but is only a forecast estimate (Model nature), so it is
    // demotable here rather than a hard pre_soil skip. An OBSERVATION-grade
    // rain_now already returned a hard skip from pre_soil and never reaches here.
    // Suppressed by `floor_active` (the soil_floor moat) just like the forecast
    // gates. NOT gated on `forecast_stale`: it reads the merged live rate, not the
    // forward-looking forecast window. The top-level reason stays the stable
    // "Currently raining (...)" the unit-aware renderer mirrors; the honest soft
    // model-estimate phrasing rides the trace detail.
    if !floor_active && rain_now_model_fires(i, p, disabled) {
        return ("skip", rain_now_reason(i), "rain_now");
    }
    // The three forward-looking rain SKIPs below are suppressed when the
    // forecast is stale (`forecast_stale`): a frozen "rain coming" snapshot from
    // an Open-Meteo outage must not skip a real watering and starve the yard. The
    // measured gates (rain_now from the station, already_wet, soil) still apply,
    // and freeze / heat-advisory keep their own (safe-direction) behavior.
    if !floor_active && rain_next_4h_fires(i, p, disabled) {
        return ("skip", rain_next_4h_reason(i), "rain_next_4h");
    }
    if !floor_active && tomorrow_rain_fires(i, p, disabled) {
        return ("skip", tomorrow_rain_reason(i), "tomorrow_rain");
    }
    if !floor_active && rain_3day_fires(i, p, disabled) {
        return ("skip", rain_3day_reason(i), "rain_3day");
    }
    // A heat advisory no longer requires HUMID heat.
    //
    // This gate used to demand humidity at or above a threshold
    // defaulting to 60%, which meant an arid yard never got a heat
    // response at all: Phoenix at 110 F and 15% relative humidity failed
    // the test, while a muggy Gulf Coast afternoon passed it. That is
    // backwards. Dry heat evaporates more, not less, and the arid yard
    // is the one that needs the extension.
    //
    // The remaining conditions are the ones that were always doing the
    // work: a hot three-day peak, a dry stretch behind it, and no
    // meaningful rain in the forecast.
    // Above the heat advisory, and only just. The advisory returns
    // run_extended, so with the hold BELOW it a hot dry morning watered
    // the yard with "All watering is on hold" switched on. Everything
    // above this point is a weather SKIP, and a weather skip stays the
    // reported reason because it tells the owner more than the hold does
    // and both answers are "do not water".
    //
    // The rule id stays `dry_run` so the gate catalog and any stored
    // decision traces are unaffected.
    if i.is_dry_run {
        return ("skip", "All watering is on hold".to_string(), "dry_run");
    }

    if !disabled.contains("heat_advisory") && heat_advisory_applies(i, p) {
        return (
            "run_extended",
            format!(
                "Heat advisory: running planned + 15% (peak {:.0}°F)",
                i.temp_max_3day_f
            ),
            "heat_advisory",
        );
    }

    ("run", String::new(), "run")
}

// ─────────────────────────────────────────────────────────────────────
// Decision provenance (powers the Rule Lab UI).
//
// `decide_traced` mirrors `decide`'s ladder exactly but records EVERY
// rule it walks: whether the rule was applicable, whether it fired, the
// data values it saw, and the verdict it produced. The first rule to fire
// is the decision; later rules are recorded as `not_reached` (first-match
// wins, same as `decide`). The `decide_traced_matches_decide` test pins
// the two functions together so they can never silently drift.
// ─────────────────────────────────────────────────────────────────────

// `RuleEval` + `DecisionTrace` live in `crate::model` (the shared,
// both-features serde contract) so the hydrate-side Rule Lab UI can read
// them; `decide_traced` here (ssr-only) produces them.

#[allow(clippy::too_many_arguments)]
fn gate(
    rules: &mut Vec<RuleEval>,
    decided: &mut Option<(String, String)>,
    disabled: &HashSet<&str>,
    id: &str,
    label: &str,
    category: &str,
    applicable: bool,
    cond: bool,
    detail: String,
    verdict: &str,
    reason: String,
) {
    // Operator-disabled rules stay visible in the trace (transparency)
    // but never decide. Checked before not_reached so the trace always
    // explains WHY the rule is inert. Protected ids never reach here
    // (filtered out of the set by `disabled_set`).
    if disabled.contains(id) {
        rules.push(RuleEval {
            id: id.into(),
            label: label.into(),
            category: category.into(),
            detail: "disabled by operator".into(),
            outcome: "skipped".into(),
            over_line: false,
            verdict: None,
            margin_label: None,
            // P1 operands filled in by annotate_margins for evaluated threshold
            // gates; None here (disabled / not_reached / inapplicable rows).
            value: None,
            threshold: None,
            unit_kind: None,
            // Override provenance is stamped later, if a better-scoped
            // resolution sets this row aside; producers never pre-set it.
            overridden_by: None,
            overridden_detail: None,
        });
        return;
    }
    if decided.is_some() {
        rules.push(RuleEval {
            id: id.into(),
            label: label.into(),
            category: category.into(),
            detail: "not reached (an earlier rule decided)".into(),
            outcome: "not_reached".into(),
            over_line: false,
            verdict: None,
            margin_label: None,
            // P1 operands filled in by annotate_margins for evaluated threshold
            // gates; None here (disabled / not_reached / inapplicable rows).
            value: None,
            threshold: None,
            unit_kind: None,
            // Override provenance is stamped later, if a better-scoped
            // resolution sets this row aside; producers never pre-set it.
            overridden_by: None,
            overridden_detail: None,
        });
        return;
    }
    if !applicable {
        rules.push(RuleEval {
            id: id.into(),
            label: label.into(),
            category: category.into(),
            detail,
            outcome: "skipped".into(),
            over_line: false,
            verdict: None,
            margin_label: None,
            // P1 operands filled in by annotate_margins for evaluated threshold
            // gates; None here (disabled / not_reached / inapplicable rows).
            value: None,
            threshold: None,
            unit_kind: None,
            // Override provenance is stamped later, if a better-scoped
            // resolution sets this row aside; producers never pre-set it.
            overridden_by: None,
            overridden_detail: None,
        });
        return;
    }
    rules.push(RuleEval {
        id: id.into(),
        label: label.into(),
        category: category.into(),
        detail,
        outcome: if cond { "fired" } else { "passed" }.into(),
        over_line: false,
        verdict: if cond { Some(verdict.into()) } else { None },
        margin_label: None,
        // P1 operands written by annotate_margins after the ladder is built (it
        // needs the settled fired/passed outcome); seeded None here.
        value: None,
        threshold: None,
        unit_kind: None,
        // Override provenance is stamped later, if a better-scoped
        // resolution sets this row aside; producers never pre-set it.
        overridden_by: None,
        overridden_detail: None,
    });
    if cond {
        *decided = Some((verdict.into(), reason));
    }
}

/// Reconstruct engine `Inputs` from a snapshot's `SkipCheck` for the
/// Simulator's "what-if". The control gates (pause / restriction /
/// dry-run / tomorrow-override) are intentionally neutralized so the
/// hypothetical reflects pure weather + soil logic, otherwise a dry-run
/// or pause would mask every weather slider behind the same skip.
pub fn inputs_from_skipcheck(s: &SkipCheck) -> Inputs {
    Inputs {
        // The wire carries both now, so the hypothetical keeps whichever
        // number actually held the yard rather than silently dropping the
        // modelled one and changing the verdict it is explaining.
        rain_today_forecast_in: s.rain_today_forecast_in,
        // Unused: `when` below is Unknown, so no gate asks the calendar
        // anything. Named explicitly rather than defaulted so the next
        // reader does not have to work out whether it mattered.
        calendar: crate::engine::calendar::Calendar::utc(),
        // The wire does not round-trip the deployment offset; the
        // Simulator rebuilding from a snapshot evaluates in UTC, which is
        // deterministic and never a live decision.
        temp_now_f: s.temp_now_f,
        wind_now_mph: s.wind_now_mph,
        rain_today_in: s.rain_today_in,
        rain_intensity_now_in_hr: s.rain_intensity_now_in_hr,
        // SkipCheck doesn't round-trip the live rain NATURE, so the Simulator's
        // what-if treats the reconstructed rate as a model estimate (the safe
        // demotable default). The what-if explores forecast/soil sliders, not the
        // hard-vs-soft rain provenance, so this never masks a slider's effect.
        rain_nature: RainNature::default(),
        // The what-if reconstructs stored weather; forecast staleness is not part
        // of the SkipCheck, so the hypothetical treats the forecast as trusted.
        forecast_stale: false,
        humidity_now_pct: s.humidity_now_pct,
        forecast_in: s.forecast_in,
        rain_tomorrow_prob_pct: s.rain_tomorrow_prob_pct,
        rain_3day_weighted_in: s.rain_3day_weighted_in,
        rain_7day_weighted_in: s.rain_7day_weighted_in,
        rain_next_4h_in: s.rain_next_4h_in,
        rain_observed_recent_in: s.rain_observed_recent_in,
        wind_max_today_mph: s.wind_max_today_mph,
        wind_window_max_mph: s.wind_window_max_mph,
        watered_days: Vec::new(),
        run_window: s.run_window,
        window_min_temp_f: s.window_min_temp_f,
        temp_min_24h_f: if s.temp_min_24h_valid {
            Some(s.temp_min_24h_f)
        } else {
            None
        },
        temp_max_3day_f: s.temp_max_3day_f,
        // Forecast-derived per-day 3-day peak heat index, round-tripped so the
        // Simulator's what-if reuses the corrected value instead of recomputing
        // the impossible temp_max_3day × humidity_now pairing.
        heat_index_max_3day_f: s.heat_index_max_3day_f,
        days_since_significant_rain: s.days_since_significant_rain,
        max_wind_mph: s.max_wind_mph,
        min_temp_f: s.min_temp_f,
        rain_skip_in: s.rain_skip_in,
        // Rebuild the per-zone soil Vec from SkipCheck's flattened soil map so
        // the Simulator's what-if reflects every configured zone (any slug).
        soil_zones: rebuild_soil_zones(s),
        soil_temp_yard_min_f: s.soil_temp_yard_min_f,
        soil_temp_yard_max_f: s.soil_temp_yard_max_f,
        frost_skip_soil_f: s.frost_skip_soil_f,
        // SkipCheck doesn't carry live-readings provenance; the what-if
        // assumes healthy inputs (matches the other neutralized gates).
        live_readings: LiveReadings::Station,
        // Control gates neutralized for the what-if.
        restart_required: false,
        is_paused: false,
        is_dry_run: false,
        pause_until_epoch: 0,
        // No clock: the control gates are neutralized for the
        // what-if, and Unknown is what that actually means. The
        // old spelling was 0 + 0, which claims a real instant in
        // 1970 in a UTC deployment.
        when: crate::engine::clock::DecisionTime::Unknown,
        override_tomorrow: String::new(),
        is_tomorrow: false,
        global_override: "auto".to_string(),
        zone_overrides: std::collections::HashMap::new(),
        watering_restrictions: Vec::new(),
        address_parity: AddressParity::NotApplicable,
    }
}

/// Annotate each threshold gate with a plain-language "distance to flip"
/// so the Rule Lab shows how close tonight's call was, not just pass/fire.
/// Only gates with a numeric threshold get a margin; binary control/safety
/// gates (override, pause, restrictions, dry_run, live_data) and the run /
/// extend gates (soil_floor, heat_advisory) stay bare. The operands mirror each
/// gate's exact condition in `decide_traced`; the parity tests pin the ladder so
/// this can't silently drift from what actually decided.
///
/// P1 (units architecture): the SAME `(actual, threshold, unit)` that phrase the
/// `margin_label` are ALSO written into `RuleEval.value` / `.threshold` /
/// `.unit_kind` so a later client phase can re-render the margin unit-aware
/// without parsing the baked string. The display `unit` maps to a stable
/// `unit_kind` ("temp_f","wind_mph","rain_in","rain_rate_in_hr","pct",
/// "soil_temp_f","none"). ADDITIVE: the baked `margin_label` is byte-identical;
/// only the new operand fields are filled in.
fn annotate_margins(rules: &mut [RuleEval], i: &Inputs, p: &SkipRuleParams) {
    // "headroom": how far the driving input can move before this gate skips.
    let head = |dist: f64, unit: &str, prec: usize| -> String {
        format!(
            "{:.*}{} of headroom before this skips",
            prec,
            dist.max(0.0),
            unit
        )
    };
    // "past the line": for the gate that actually fired, by how much.
    let past = |dist: f64, unit: &str, prec: usize| -> String {
        format!("skipped, {:.*}{} past the line", prec, dist.max(0.0), unit)
    };
    for r in rules.iter_mut() {
        let fired = r.outcome == "fired";
        // Only applicable, evaluated gates (fired or passed) carry a margin.
        if !fired && r.outcome != "passed" {
            continue;
        }
        // `fires_raw` is the gate's EXACT raw threshold condition (same operator
        // it actually fires on -- strict `>`/`<` for rain_now/wind/freeze, `>=`
        // for already_wet/rain forecasts/soil_saturation). It lets us tell a
        // genuine "headroom" pass from a gate that is OVER its own line but was
        // held by a stronger rule (the dry-soil floor demoting a forecast-rain
        // skip). Using the exact operator (not a >= approximation) keeps the
        // boundary actual==threshold correct: a `>` gate at the line PASSES with
        // 0 headroom, it is NOT "overridden".
        //
        // P1: returns `(margin_label, value, threshold, unit_kind)` so the same
        // operands that phrase the label also populate the structured fields.
        let mk = |actual: f64,
                  threshold: f64,
                  fires_raw: bool,
                  unit: &str,
                  prec: usize|
         -> Option<(String, f64, f64, &'static str, bool)> {
            let dist = (actual - threshold).abs();
            let over_line = !fired && fires_raw;
            let label = if fired {
                past(dist, unit, prec)
            } else if fires_raw {
                // Passed while its own raw threshold IS met: a stronger rule
                // overrode the skip. Say so honestly; the overriding gate (e.g.
                // soil_floor) carries the full why. "Headroom" would be backwards.
                format!("{:.*}{} past the line, but overridden", prec, dist, unit)
            } else {
                head(dist, unit, prec)
            };
            Some((label, actual, threshold, unit_kind_for(unit), over_line))
        };
        let annotated = match r.id.as_str() {
            "rain_now" => i.rain_intensity_now_in_hr.and_then(|a| {
                let t = p.rain_now_in_hr;
                mk(a, t, a > t, " in/hr", 2)
            }),
            "already_wet" => {
                let (a, t) = (i.rain_today_in, p.already_wet_in);
                mk(a, t, a >= t, "\"", 2)
            }
            "observed_rain" => {
                let (a, t) = (i.rain_observed_recent_in, i.rain_skip_in);
                mk(a, t, a >= t, "\"", 2)
            }
            "rain_next_4h" => i.rain_next_4h_in.and_then(|a| {
                let t = p.rain_next_4h_skip_in;
                mk(a, t, a >= t, "\"", 2)
            }),
            "rain_3day" => i.rain_3day_weighted_in.and_then(|a| {
                let t = p.rain_3day_factor * i.rain_skip_in;
                mk(a, t, a >= t, "\"", 2)
            }),
            "tomorrow_rain" => i.forecast_in.and_then(|amount| {
                let a = amount * tomorrow_prob_weight(i);
                let t = i.rain_skip_in;
                mk(a, t, a >= t, "\"", 2)
            }),
            "wind_now" => {
                let (a, t) = (i.wind_now_mph, i.max_wind_mph);
                mk(a, t, a > t, " mph", 0)
            }
            "wind_forecast" => {
                let (a, _) = wind_on_trial(i);
                let t = i.max_wind_mph + p.wind_forecast_slack_mph;
                mk(a, t, a > t, " mph", 0)
            }
            "freeze_now" => {
                let (a, _) = freeze_on_trial(i);
                let t = i.min_temp_f;
                mk(a, t, a < t, "°F", 0)
            }
            "overnight_freeze" => i
                .temp_min_24h_f
                .filter(|_| overnight_freeze_applies(i))
                .and_then(|a| mk(a, i.min_temp_f, a < i.min_temp_f, "°F", 0)),
            "soil_frost" => i
                .soil_temp_yard_min_f
                .and_then(|a| mk(a, i.frost_skip_soil_f, a < i.frost_skip_soil_f, "°F", 0)),
            "soil_saturation" => i
                .soil_zones
                .iter()
                // The tightest (smallest signed pct - saturation) zone binds the
                // gate, matching decide_traced's `min_by`.
                .filter_map(|z| z.pct.map(|pct| (pct, z.saturation_pct)))
                .min_by(|a, b| {
                    (a.0 - a.1)
                        .partial_cmp(&(b.0 - b.1))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .and_then(|(pct, sat)| mk(pct, sat, pct >= sat, "%", 0)),
            _ => None,
        };
        // P1: split the single source `(label, value, threshold, unit_kind)` into
        // the existing `margin_label` (byte-identical) and the new structured
        // operands. A gate with no numeric threshold annotates nothing (all None),
        // which is correct for the binary control/safety gates.
        match annotated {
            Some((label, value, threshold, unit_kind, over_line)) => {
                r.margin_label = Some(label);
                r.value = Some(value);
                r.threshold = Some(threshold);
                // The same fact the label phrases, kept as data so a
                // renderer never has to read the phrasing back.
                r.over_line = over_line;
                // soil_frost shares the `°F` display unit with the air-temp gates
                // but measures SOIL temperature; remap it to the soil dimension so
                // a client can resolve the correct (and distinct) unit preference.
                r.unit_kind = Some(if r.id == "soil_frost" {
                    "soil_temp_f".to_string()
                } else {
                    unit_kind.to_string()
                });
            }
            None => {
                r.margin_label = None;
                r.value = None;
                r.threshold = None;
                r.unit_kind = None;
                r.over_line = false;
            }
        }
    }
}

/// P1 (units architecture): map a gate's display `unit` (as used to PHRASE the
/// `margin_label`) to a stable `unit_kind`, the dimension a unit-aware client
/// renderer keys on. The set is the canonical {temp_f, wind_mph, rain_in,
/// rain_rate_in_hr, pct, soil_temp_f, none}. soil_frost shares the `°F` display
/// unit with the air-temp gates but is a SOIL temperature, so it is mapped by the
/// caller (not here); every other `°F` gate is air temp.
fn unit_kind_for(unit: &str) -> &'static str {
    match unit {
        " in/hr" => "rain_rate_in_hr",
        "\"" => "rain_in",
        " mph" => "wind_mph",
        "°F" => "temp_f",
        "%" => "pct",
        _ => "none",
    }
}

/// Forced-run safety signal. When a sticky `global_override = "run"` waters
/// THROUGH a hard guard (freeze, restriction, currently-raining, dry-run, etc.),
/// returns the guard's reason string (e.g. "Freeze risk now (28°F < 35°F)") so
/// the UI can warn the operator they are running past a real protection. `None`
/// when there is no force-run, or when the force-run is not overriding anything
/// (the engine would have run anyway).
///
/// This does NOT change override-beats-all semantics: the override still wins.
/// It only surfaces WHAT the override is suppressing, by re-running the ladder
/// with the global override neutralized to "auto" and reporting the would-be
/// skip. Zone-level overrides are intentionally untouched here (this is the
/// yard-wide force-run warning); per-zone `decide_per_zone` keeps its own
/// override provenance.
pub fn force_overrode_guard(i: &Inputs, p: &SkipRuleParams) -> Option<String> {
    if i.global_override.as_str() != "run" || decide_with_code(i, p).0 == "skip" {
        return None;
    }
    // Neutralize ONLY the global override; everything else (pause, dry_run,
    // weather, soil) is left intact so the would-be verdict is exactly what the
    // engine would have decided without the force-run.
    let mut probe = i.clone();
    probe.global_override = "auto".to_string();
    // `decide` is test-only; `decide_with_code` is the production twin (same
    // verdict + reason, plus a reason code we don't need here).
    let (verdict, reason, _code) = decide_with_code(&probe, p);
    if verdict == "skip" && !reason.is_empty() {
        Some(reason)
    } else {
        None
    }
}

/// Traced twin of `decide`. Returns the same verdict + reason plus the
/// full per-rule provenance. Order and conditions mirror `decide`.
pub fn decide_traced(i: &Inputs, p: &SkipRuleParams) -> DecisionTrace {
    let force_requested = i.global_override == "run"
        || (i.global_override != "skip" && i.is_tomorrow && i.override_tomorrow == "run");
    if force_requested && force_run_block(&with_effective_soil(i, p), p, &disabled_set(p)).is_some()
    {
        // Trace the exact same scoped evaluation that refused the force request.
        // Keeping the request row visible explains why an armed force still holds.
        let mut scoped_params = p.clone();
        scoped_params
            .disabled_rules
            .extend(FORCE_RUN_BYPASSED_GATES.iter().map(|id| (*id).to_string()));
        let mut trace = decide_traced(&without_force_run(i), &scoped_params);
        if let Some(override_row) = trace.rules.iter_mut().find(|r| r.id == "override") {
            if override_row.outcome != "fired" {
                override_row.detail =
                    "force run requested; safety checks and operator holds still apply".into();
            }
        }
        return trace;
    }
    let mut rules: Vec<RuleEval> = Vec::with_capacity(18);
    let mut decided: Option<(String, String)> = None;
    // Operator-disabled built-in rules (protected ids already filtered
    // out by `disabled_set`). Threaded into every gate() so a disabled
    // rule still surfaces in the trace as "disabled by operator" but can
    // never decide. Mirrors the checks in pre_soil/soil_saturation/
    // post_soil exactly; the parity tests pin the two ladders together.
    let disabled = disabled_set(p);

    // Quarantine-filtered effective soil, shared with decide()/decide_per_zone so
    // every soil-gate path judges identical soil. ONLY soil_zones differs from
    // `i`; the soil_saturation gate, the soil_floor demotion state, and the
    // soil_saturation margin all read `eff`, while every other gate keeps reading
    // raw `i` (its scalars are unchanged here).
    let eff = with_effective_soil(i, p);

    // Soil-floor (moat) demotion state, shared with decide()/decide_per_zone so
    // all three ladders agree. When it holds, the three soft-rain gates below are
    // suppressed (reported as `passed`) and the soil_floor gate fires a run.
    let demotes = soil_floor_demotes(&eff, p, &disabled);
    let soil_floor_detail = if demotes {
        let sid = demotable_soft_skip_id(&eff, p, &disabled).unwrap_or("rain_next_4h");
        match eff
            .soil_zones
            .iter()
            .find_map(|z| zone_healthy_dry(z).map(|pct| (z, pct)))
        {
            Some((z, pct)) => format!(
                "{} {:.0}% < {:.0}% minimum; {} overridden",
                z.name,
                pct,
                z.target_min_pct,
                soft_rain_label(sid)
            ),
            None => format!("measured-dry zone overrides {}", soft_rain_label(sid)),
        }
    } else {
        "no measured-dry zone; soft forecast-rain skip applies".to_string()
    };

    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "restart_required",
        "Restart required",
        "control",
        true,
        i.restart_required,
        if i.restart_required {
            crate::gates_catalog::RESTART_REQUIRED_REASON.to_string()
        } else {
            "startup configuration is active".to_string()
        },
        "skip",
        crate::gates_catalog::RESTART_REQUIRED_REASON.to_string(),
    );

    // Manual override (sticky global override + the tomorrow cell), one row.
    //
    // #3 fix: the traced ladder previously honored ONLY the tomorrow-cell
    // override (`override_tomorrow`) and silently ignored the sticky
    // `global_override`, so a vacation-skip / force-run produced a hero verdict
    // that contradicted the plain-English explanation. The sticky global
    // override is the very first rung in the non-traced ladder (pre_soil:834-838
    // -> decide_with_code), so it must be the first deciding rung here too.
    // Folded into the SINGLE pre-existing "override" gate (rather than a second
    // row) so the trace ids stay 1:1 with builtin_rule_catalog (pinned by
    // catalog_covers_every_traced_gate). Precedence mirrors pre_soil exactly:
    // the global override is checked before the tomorrow override. Verdict +
    // reason strings match pre_soil's so the parity tests stay green.
    {
        // (applicable, fired-condition, detail, verdict, reason), matching
        // pre_soil's first-wins ladder: global override, then the tomorrow cell.
        let (applicable, cond, detail, verdict, reason) = match i.global_override.as_str() {
            "skip" => (
                true,
                true,
                "global override = skip".to_string(),
                "skip",
                "Manual override: skip".to_string(),
            ),
            "run" => (
                true,
                true,
                "global override = run".to_string(),
                "run",
                "Manual override: force run".to_string(),
            ),
            // No sticky global override: fall back to the tomorrow-cell override.
            _ if i.is_tomorrow => match i.override_tomorrow.as_str() {
                "skip" => (
                    true,
                    true,
                    "override = skip".to_string(),
                    "skip",
                    "Manual override (skip tomorrow)".to_string(),
                ),
                "run" => (
                    true,
                    true,
                    "override = run".to_string(),
                    "run",
                    String::new(),
                ),
                _ => (
                    true,
                    false,
                    "no override set".to_string(),
                    "skip",
                    String::new(),
                ),
            },
            _ => (
                false,
                false,
                "no global override; tomorrow override only applies to the tomorrow cell"
                    .to_string(),
                "skip",
                String::new(),
            ),
        };
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "override",
            "Manual override",
            "control",
            applicable,
            cond,
            detail,
            verdict,
            reason,
        );
    }

    // Vacation pause (until a date).
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "pause_until",
        "Vacation pause (timed)",
        "control",
        i.pause_until_epoch > 0 && i.now_epoch() > 0,
        i.now_epoch() < i.pause_until_epoch,
        // Stable, readable detail. The old "now {now_epoch} vs until {x}" baked
        // the live clock into the trace, so the decision_trace mutated every
        // ~10s tick and defeated the SSE change-gate (and read as noise).
        if i.pause_until_epoch > 0 {
            format!(
                "until {}",
                format_pause_until(i.pause_until_epoch, i.calendar)
            )
        } else {
            "no timed pause set".to_string()
        },
        "skip",
        format!(
            "Paused (vacation until {})",
            format_pause_until(i.pause_until_epoch, i.calendar)
        ),
    );

    // Vacation pause (toggle).
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "paused",
        "Vacation pause",
        "control",
        true,
        i.is_paused,
        format!("paused = {}", i.is_paused),
        "skip",
        "Paused (vacation mode)".to_string(),
    );

    // Jurisdictional / HOA watering restrictions.
    {
        let applicable = !i.watering_restrictions.is_empty() && i.when.is_known();
        let (cond, reason) = if applicable {
            let v = restrictions::evaluate_for(
                i.when,
                &i.watering_restrictions,
                i.address_parity,
                &i.watered_days,
                None,
            );
            (
                v.skip,
                v.reason
                    .unwrap_or_else(|| "Watering restriction".to_string()),
            )
        } else {
            (false, String::new())
        };
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "restrictions",
            "Watering restrictions",
            "safety",
            applicable,
            cond,
            format!("{} rule(s) configured", i.watering_restrictions.len()),
            "skip",
            reason,
        );
    }

    // Live-data integrity (mirrors pre_soil): with no station and no
    // forecast, fail safe instead of judging fabricated readings.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "live_data",
        "Live weather availability",
        "safety",
        true,
        i.live_readings == LiveReadings::Unavailable,
        match i.live_readings {
            LiveReadings::Station => "measured current readings".to_string(),
            LiveReadings::ForecastFallback => "estimated current weather (degraded)".to_string(),
            LiveReadings::Unavailable => "no station data and no forecast".to_string(),
        },
        "skip",
        "Live weather unavailable (no station data or forecast); failing safe".to_string(),
    );

    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "soil_probe",
        "Soil probe availability",
        "safety",
        true,
        soil_probe_unavailable(&eff),
        if soil_probe_unavailable(&eff) {
            SOIL_PROBE_HOLD_REASON.to_string()
        } else {
            "no probe fault holds every zone in this scope".to_string()
        },
        "skip",
        SOIL_PROBE_HOLD_REASON.to_string(),
    );

    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "planning_forecast",
        "Watering-plan rain availability",
        "soil",
        true,
        planning_forecast_unavailable(&eff),
        if planning_forecast_unavailable(&eff) {
            PLANNING_FORECAST_HOLD_REASON.into()
        } else {
            "no scoped watering plan is held for missing rain evidence".into()
        },
        "skip",
        PLANNING_FORECAST_HOLD_REASON.into(),
    );

    // Currently raining. Fires (skip) for an OBSERVATION-GRADE rate over the
    // threshold (a hard skip: measured / NWS / MRMS rain is ground truth) OR for a
    // MODEL-grade rate that is NOT being demoted by the soil_floor moat. When the
    // rate is a model estimate AND a measured-dry zone demotes it, this row PASSES
    // (the soil_floor gate fires the run downstream), mirroring how the three
    // forecast-rain gates report `passed` under demotion. The detail + top-level
    // reason keep the EXACT pre-existing operand format the unit-aware renderer
    // (`render_rule_detail` / `render_skip_reason`) mirrors byte-for-byte, so the
    // honesty of the rain NATURE is carried on the snapshot's `Forecast.rain_nature`
    // (the dashboard badge) and enforced by the hard-vs-soft routing above, rather
    // than baked into this gate's string (which would break that byte-identity
    // contract). The verdict still differs honestly: an observation-grade rain
    // FIRES here (hard), while a model estimate either fires (soft, no dry zone) or
    // PASSES here and lets soil_floor demote it.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "rain_now",
        "Currently raining",
        "safety",
        true,
        rain_now_hard_fires(i, p, &disabled) || (rain_now_model_fires(i, p, &disabled) && !demotes),
        format!(
            "{:.2} in/hr vs {:.2} threshold",
            rain_amount(i.rain_intensity_now_in_hr),
            p.rain_now_in_hr
        ),
        "skip",
        rain_now_reason(i),
    );

    // Freeze risk now.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "freeze_now",
        "Freeze risk now",
        "safety",
        true,
        freeze_on_trial(i).0 < i.min_temp_f,
        {
            // The pre-dawn detail keeps its exact historical shape; the
            // renderer reconstructs it byte for byte. A post-sunrise
            // window names its hours.
            let (t, when) = freeze_on_trial(i);
            if when == "now" {
                format!("{t:.0}°F vs {:.0}°F min", i.min_temp_f)
            } else {
                format!("{t:.0}°F {when} vs {:.0}°F min", i.min_temp_f)
            }
        },
        "skip",
        {
            let (t, when) = freeze_on_trial(i);
            format!("Freeze risk {when} ({t:.0}°F < {:.0}°F)", i.min_temp_f)
        },
    );

    // Overnight freeze look-ahead. Applicable only when a 24h forecast
    // low exists; a genuine 0 °F (or colder) low still fires the rule.
    {
        let t24 = i.temp_min_24h_f;
        let applies = overnight_freeze_applies(i);
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "overnight_freeze",
            "Overnight freeze",
            "safety",
            t24.is_some() && applies,
            applies && t24.map(|t| t < i.min_temp_f).unwrap_or(false),
            match (applies, t24) {
                (false, _) => "post-sunrise window; judged by its own hours".to_string(),
                (true, Some(t)) => format!("24h low {:.0}°F vs {:.0}°F min", t, i.min_temp_f),
                (true, None) => "no 24h forecast low".to_string(),
            },
            "skip",
            format!(
                "Overnight freeze ({:.0}°F low next 24h < {:.0}°F)",
                t24.unwrap_or(0.0),
                i.min_temp_f
            ),
        );
    }

    // Soil frost.
    {
        let t = i.soil_temp_yard_min_f;
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "soil_frost",
            "Soil frost",
            "safety",
            t.is_some(),
            t.map(|t| t < i.frost_skip_soil_f).unwrap_or(false),
            match t {
                Some(t) => format!("soil {:.1}°F vs {:.0}°F", t, i.frost_skip_soil_f),
                None => "no soil-temp sensor".into(),
            },
            "skip",
            format!(
                "Soil frost ({:.1}°F < {:.0}°F threshold)",
                t.unwrap_or(0.0),
                i.frost_skip_soil_f
            ),
        );
    }

    // Wind now.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "wind_now",
        "Wind too high now",
        "safety",
        true,
        i.wind_now_mph > i.max_wind_mph,
        format!("{:.1} mph vs {:.0} mph max", i.wind_now_mph, i.max_wind_mph),
        "skip",
        format!(
            "Wind too high now ({:.1} mph > {:.0} mph)",
            i.wind_now_mph, i.max_wind_mph
        ),
    );

    // Windy-day forecast.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "wind_forecast",
        "Windy day forecast",
        "weather",
        true,
        wind_on_trial(i).0 > i.max_wind_mph + p.wind_forecast_slack_mph,
        {
            let (peak, scope) = wind_on_trial(i);
            format!(
                "peak {:.0} mph {} vs {:.0}+{:.0} (day peak {:.0})",
                peak,
                scope.detail(),
                i.max_wind_mph,
                p.wind_forecast_slack_mph,
                i.wind_max_today_mph
            )
        },
        "skip",
        {
            let (peak, scope) = wind_on_trial(i);
            wind_forecast_reason(scope, peak, i.max_wind_mph, p.wind_forecast_slack_mph)
        },
    );

    // Already wet today.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "already_wet",
        "Already wet today",
        "weather",
        true,
        i.rain_today_in >= p.already_wet_in,
        format!(
            "{:.2}\" measured today vs {:.2}\" floor",
            i.rain_today_in, p.already_wet_in
        ),
        "skip",
        format!("Already wet ({:.2}\" measured today)", i.rain_today_in),
    );

    // The modelled twin, so the trace shows WHICH of the two rain numbers
    // held the yard. Without a row here the decision trace would show the
    // measured gate not firing and no reason for the skip.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "rain_today_forecast",
        "Rain forecast today",
        "weather",
        true,
        forecast_rain_fires(i.rain_today_forecast_in, p.already_wet_in, false),
        format!(
            "{:.2}\" expected today vs {:.2}\" floor",
            rain_amount(i.rain_today_forecast_in),
            p.already_wet_in
        ),
        "skip",
        rain_today_forecast_reason(i),
    );

    // Observed recent rain (sensor-independent backstop). Mirrors pre_soil's
    // hard skip ordered before soil saturation + the soil_floor moat: measured
    // rain over the recent window (today + window) binds every zone.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "observed_rain",
        "Observed recent rain",
        "weather",
        true,
        rain_observed_recent_fires(i, p, &disabled),
        format!(
            "{:.2}\" last {} day(s) vs {:.2}\" skip",
            i.rain_observed_recent_in,
            p.rain_observed_window_days + 1,
            i.rain_skip_in
        ),
        "skip",
        format!(
            "Already wet ({:.2}\" rain in the last {} day(s))",
            i.rain_observed_recent_in,
            p.rain_observed_window_days + 1
        ),
    );

    // Yard-wide soil saturation. Generalized to iterate the configured
    // zones; applicable only when at least one zone exists and every zone
    // reports a reading. Judged on the EFFECTIVE (quarantine-filtered) soil so
    // the trace's verdict matches decide()'s `soil_saturation(&eff, ...)`: an
    // offline/outlier probe inheriting its trustworthy siblings' median can now
    // make the gate applicable + fire.
    {
        let applicable =
            !eff.soil_zones.is_empty() && eff.soil_zones.iter().all(|z| z.pct.is_some());
        let cond = applicable
            && eff
                .soil_zones
                .iter()
                .all(|z| z.pct.unwrap() >= z.saturation_pct);
        let (detail, reason) = if applicable {
            let tightest = eff
                .soil_zones
                .iter()
                .min_by(|a, b| {
                    let am = a.pct.unwrap() - a.saturation_pct;
                    let bm = b.pct.unwrap() - b.saturation_pct;
                    am.partial_cmp(&bm).unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap();
            (
                format!(
                    "tightest {} {:.0}% vs {:.0}%",
                    tightest.name,
                    tightest.pct.unwrap(),
                    tightest.saturation_pct
                ),
                format!(
                    "All zones soil-saturated (tightest: {} {:.0}% ≥ {:.0}% threshold)",
                    tightest.name,
                    tightest.pct.unwrap(),
                    tightest.saturation_pct
                ),
            )
        } else if eff.soil_zones.is_empty() {
            ("no soil zones configured".to_string(), String::new())
        } else {
            // Name the zones holding the gate inapplicable: a flatlined
            // probe resolves to None upstream, and the old generic "not
            // all zones have soil sensors" hid which hardware was dead. An
            // offline probe that quarantine could infer from siblings is no
            // longer offline in `eff`, so it correctly drops off this list.
            let missing: Vec<&str> = eff
                .soil_zones
                .iter()
                .filter(|z| z.pct.is_none())
                .map(|z| z.slug.as_str())
                .collect();
            (
                format!("no soil reading: {}", missing.join(", ")),
                String::new(),
            )
        };
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "soil_saturation",
            "Yard-wide soil saturation",
            "soil",
            applicable,
            cond,
            detail,
            "skip",
            reason,
        );
    }

    // Rain within 4h.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "rain_next_4h",
        "Rain within 4 hours",
        "weather",
        true,
        !demotes && rain_next_4h_fires(i, p, &disabled),
        format!(
            "{:.2}\" next 4h vs {:.2}\" skip",
            rain_amount(i.rain_next_4h_in),
            p.rain_next_4h_skip_in
        ),
        "skip",
        rain_next_4h_reason(i),
    );

    // Tomorrow rain (confidence-weighted).
    {
        let detail = match i.forecast_in {
            None => "Tomorrow's rain amount is unavailable".into(),
            Some(amount) => match i.rain_tomorrow_prob_pct {
                Some(0) if amount > 0.0 => {
                    format!("{amount:.2}\" forecast at a reported 0% probability (does not skip)")
                }
                Some(prob) => format!(
                    "{amount:.2}\" × {prob}% = {:.2}\" vs {:.2}\"",
                    amount * tomorrow_prob_weight(i),
                    i.rain_skip_in
                ),
                None => format!(
                    "{amount:.2}\" at full weight (no probability reported) vs {:.2}\"",
                    i.rain_skip_in
                ),
            },
        };
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "tomorrow_rain",
            "Tomorrow rain",
            "weather",
            true,
            !demotes && tomorrow_rain_fires(i, p, &disabled),
            detail,
            "skip",
            tomorrow_rain_reason(i),
        );
    }

    // Heavy rain over 3 days.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "rain_3day",
        "Heavy rain (3 day)",
        "weather",
        true,
        !demotes && rain_3day_fires(i, p, &disabled),
        format!(
            "{:.2}\" weighted vs {:.2}\"",
            rain_amount(i.rain_3day_weighted_in),
            p.rain_3day_factor * i.rain_skip_in
        ),
        "skip",
        rain_3day_reason(i),
    );

    // Soil floor (the moat): a measured-dry zone demotes the soft forecast-rain
    // skip(s) above to a run. Fires only when soil_floor_demotes held (which
    // suppressed the three rain gates); decides ("run","") to match decide()'s
    // default-run so the parity tests stay green. The rich WHY lives in `detail`,
    // never in the top-level reason. Disableable via the "soil_floor" id.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "soil_floor",
        "Dry-soil floor",
        "soil",
        true,
        demotes,
        soil_floor_detail,
        "run",
        String::new(),
    );

    // Dry-run mode.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "dry_run",
        "Hold all watering",
        "control",
        true,
        i.is_dry_run,
        format!("dry_run = {}", i.is_dry_run),
        "skip",
        "All watering is on hold".to_string(),
    );

    // Heat advisory -> extend the run.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "heat_advisory",
        "Heat advisory",
        "heat",
        true,
        heat_advisory_applies(i, p),
        format!(
            "peak {:.0}°F, RH {:.0}%, {} dry days",
            i.temp_max_3day_f, i.humidity_now_pct, i.days_since_significant_rain
        ),
        "run_extended",
        format!(
            "Heat advisory: running planned + 15% (peak {:.0}°F)",
            i.temp_max_3day_f
        ),
    );

    // Fill in each threshold gate's distance-to-flip now that the full
    // ladder is built (so "fired vs passed" is settled before we phrase it).
    // `eff` differs from `i` only in soil_zones, so every non-soil margin is
    // identical; the soil_saturation margin reads the effective soil
    // to match the gate it annotates.
    annotate_margins(&mut rules, &eff, p);

    let (verdict, reason) = decided.unwrap_or_else(|| ("run".to_string(), String::new()));
    // P1 (units architecture): the trace's reason_code mirrors the DECIDING
    // RuleEval.id (the single "fired" rule, first-match-wins like decide()); when
    // nothing fired it is "run", matching SkipCheck/decide_with_code on a clean
    // run. ADDITIVE + invisible; the parity guard pins it equal to
    // decide_with_code's code so the two ladders can never disagree.
    let reason_code = rules
        .iter()
        .find(|r| r.decided())
        .map(|r| r.id.clone())
        .unwrap_or_else(|| "run".to_string());
    DecisionTrace {
        verdict,
        reason,
        // "Degraded" means the decision ran on genuinely poor inputs: either
        // the live conditions are UNAVAILABLE (no live station AND no forecast,
        // so the engine fell back to fabricated defaults) or the forecast itself
        // is STALE. A ForecastFallback (a headline field served by a valid
        // current-hour forecast, e.g. wind provided by a configured forecast
        // source in the per-field chain) is the NORMAL steady state, NOT a
        // degradation: it drives the "lower confidence" push prefix and the
        // localsky_refresh_degraded_total metric, both of which were pinned at
        // 100% on every healthy chain install when this also tripped on
        // ForecastFallback, making the metric useless and training users to
        // ignore the daily "backup data" qualifier on the day it is real.
        degraded: i.live_readings == LiveReadings::Unavailable
            || i.forecast_stale
            || i.rain_today_forecast_in.is_none()
            || i.forecast_in.is_none()
            || i.rain_next_4h_in.is_none()
            || i.rain_3day_weighted_in.is_none()
            || i.rain_intensity_now_in_hr.is_none(),
        reason_code,
        rules,
    }
}

#[cfg(test)]
mod tests {

    /// The probe card's band and the engine's gates have to agree at the
    /// boundaries, because they are the same claim shown two ways: a pill
    /// reading HEALTHY on a reading the saturation gate is skipping on
    /// would be the app contradicting itself. Both edges are exact: the
    /// gate skips at or above saturation, and the dry-floor veto needs
    /// STRICTLY below the floor.
    #[test]
    fn the_probe_band_matches_the_gate_operators_at_the_edges() {
        use crate::model::{SoilBand, SoilForecast};
        let fc = |pct: f64| SoilForecast {
            zone_slug: "z".into(),
            current_pct: Some(pct),
            target_min_pct: 30.0,
            target_max_pct: 70.0,
            ..Default::default()
        };
        // Saturation edge: exactly at the ceiling is saturated, and the
        // gate skips there too.
        assert_eq!(fc(70.0).current_band(), SoilBand::Saturated);
        assert_eq!(fc(69.9).current_band(), SoilBand::Healthy);
        // Floor edge: exactly at the floor is NOT dry, matching the
        // veto's strict <.
        assert_eq!(fc(30.0).current_band(), SoilBand::Healthy);
        assert_eq!(fc(29.9).current_band(), SoilBand::Dry);
        assert_eq!(
            SoilForecast {
                current_pct: None,
                ..fc(50.0)
            }
            .current_band(),
            SoilBand::Offline
        );
    }

    /// `over_line` and the margin label have to agree on every evaluated
    /// gate: the label is written for people, the flag is what renderers
    /// read, and a row where one says "overridden" and the other does not
    /// would render the opposite of what the engine decided.
    #[test]
    fn the_over_line_flag_and_its_label_never_disagree() {
        let mut i = base();
        // A yard whose soil floor demotes a forecast-rain skip: gates sit
        // over their own thresholds while a stronger rule holds the call.
        i.rain_next_4h_in = Some(1.0);
        i.rain_3day_weighted_in = Some(3.0);
        i.rain_today_in = 1.0;
        let trace = decide_traced(&i, &SkipRuleParams::default());
        let mut checked = 0;
        for r in &trace.rules {
            let Some(label) = r.margin_label.as_deref() else {
                assert!(!r.over_line, "{}: flag set with no margin label", r.id);
                continue;
            };
            assert_eq!(
                r.over_line,
                label.contains("overridden"),
                "{}: flag {} against label {label:?}",
                r.id,
                r.over_line
            );
            checked += 1;
        }
        assert!(checked > 0, "no evaluated gate carried a margin label");
    }

    /// The UI decides "this yard is paused" from the reason CODE, via
    /// `snapshot::is_pause_code`. Both pause gates have to be covered by
    /// it, and no other gate may be: a rename here with no matching
    /// change there would quietly stop the paused banner from showing, or
    /// show it for an unrelated skip.
    #[test]
    fn the_pause_codes_are_exactly_what_the_ui_treats_as_paused() {
        use crate::model::is_pause_code;
        let mut i = base();
        i.is_paused = true;
        let toggle = evaluate_with(&i, &SkipRuleParams::default());
        assert_eq!(toggle.reason_code, "paused");
        assert!(is_pause_code(&toggle.reason_code));

        let mut i = base();
        i.is_paused = false;
        i.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_700_000_000,
        );
        i.pause_until_epoch = i.now_epoch() + 86_400;
        let timed = evaluate_with(&i, &SkipRuleParams::default());
        assert_eq!(timed.reason_code, "pause_until");
        assert!(is_pause_code(&timed.reason_code));

        // Nothing else counts as paused.
        for other in [
            "rain_now",
            "wind_now",
            "heat_advisory",
            "tomorrow_rain",
            "run",
        ] {
            assert!(!is_pause_code(other), "{other} is not a pause");
        }
    }
    use super::*;
    use crate::engine::conditions::{
        CmpOp, ConditionExpr, ConditionRule, Metric, RuleAction, RuleScope,
    };

    #[test]
    fn soil_fields_generalize_to_any_zone_slug() {
        // A zone with a non-default slug must surface soil_<slug>_pct +
        // saturation_<slug>_pct (the manifest reads these), and round-trip back.
        // A non-default per-zone floor (42%, not the 30% default) must
        // survive the round-trip; the pre-fix rebuild hardcoded 30.0 and silently
        // dropped it in the simulator's what-if.
        let zones = vec![ZoneSoil {
            slug: "vegetable_garden".into(),
            name: "veg".into(),
            pct: Some(33.0),
            saturation_pct: 65.0,
            target_min_pct: 42.0,
            probe_configured: false,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        }];
        let m = build_soil_fields(&zones);
        assert_eq!(m.get("soil_vegetable_garden_pct"), Some(&Some(33.0)));
        assert_eq!(m.get("saturation_vegetable_garden_pct"), Some(&Some(65.0)));
        assert_eq!(m.get("target_vegetable_garden_pct"), Some(&Some(42.0)));
        let sc = crate::model::SkipCheck {
            soil_fields: m,
            ..Default::default()
        };
        let rebuilt = rebuild_soil_zones(&sc);
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].slug, "vegetable_garden");
        assert_eq!(rebuilt[0].pct, Some(33.0));
        assert_eq!(rebuilt[0].saturation_pct, 65.0);
        assert_eq!(
            rebuilt[0].target_min_pct, 42.0,
            "custom per-zone soil floor survives the simulator round-trip"
        );
    }

    #[test]
    fn rebuild_soil_zones_floor_defaults_to_30_when_absent() {
        // Backward compatibility: an older serialized SkipCheck (or a demo
        // fixture) written before target_* still rebuilds with the 30% default.
        let sc = crate::model::SkipCheck {
            soil_fields: std::collections::BTreeMap::from([
                ("soil_back_yard_pct".to_string(), Some(25.0)),
                ("saturation_back_yard_pct".to_string(), Some(70.0)),
            ]),
            ..Default::default()
        };
        let rebuilt = rebuild_soil_zones(&sc);
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].target_min_pct, 30.0);
    }

    fn base() -> Inputs {
        Inputs {
            rain_today_forecast_in: Some(0.0),
            // Fixtures evaluate in UTC so a restriction window means the
            // same thing on every machine.
            calendar: crate::engine::calendar::Calendar::utc(),
            temp_now_f: 70.0,
            wind_now_mph: 3.0,
            rain_today_in: 0.0,
            rain_intensity_now_in_hr: Some(0.0),
            // The historical fixture assumed a live LAN gauge drove the rain rate,
            // so default the nature to Measured (observation-grade). A test that
            // sets rain_intensity_now_in_hr thus exercises the HARD rain_now skip
            // by default; the model-rain (soft) tests set rain_nature = Model.
            rain_nature: RainNature::Measured,
            humidity_now_pct: 55.0,
            forecast_in: Some(0.0),
            rain_tomorrow_prob_pct: None,
            rain_3day_weighted_in: Some(0.0),
            rain_7day_weighted_in: Some(0.0),
            rain_next_4h_in: Some(0.0),
            rain_observed_recent_in: 0.0,
            wind_max_today_mph: 6.0,
            wind_window_max_mph: None,
            watered_days: Vec::new(),
            run_window: Default::default(),
            window_min_temp_f: None,
            temp_min_24h_f: Some(60.0),
            temp_max_3day_f: 80.0,
            heat_index_max_3day_f: 0.0,
            days_since_significant_rain: 1,
            max_wind_mph: 10.0,
            min_temp_f: 38.0,
            rain_skip_in: 0.25,
            soil_zones: Vec::new(),
            soil_temp_yard_min_f: None,
            soil_temp_yard_max_f: None,
            frost_skip_soil_f: 35.0,
            live_readings: LiveReadings::Station,
            forecast_stale: false,
            restart_required: false,
            is_paused: false,
            is_dry_run: false,
            pause_until_epoch: 0,
            when: crate::engine::clock::DecisionTime::at(
                crate::engine::calendar::Calendar::utc(),
                1_700_000_000,
            ),
            override_tomorrow: String::new(),
            is_tomorrow: false,
            global_override: "auto".to_string(),
            zone_overrides: std::collections::HashMap::new(),
            watering_restrictions: Vec::new(),
            address_parity: AddressParity::NotApplicable,
        }
    }

    // A stale forecast must not fabricate a forward-looking rain skip (it
    // would starve the yard during an outage), and it must mark the trace degraded
    // so the confidence is honest. Mirror with a fresh forecast that DOES skip.
    #[test]
    fn stale_forecast_suppresses_predicted_rain_skip_and_marks_degraded() {
        let p = SkipRuleParams::default();
        let mut i = base();
        // Heavy 3-day rain that, with a fresh forecast, fires the rain_3day skip.
        i.rain_3day_weighted_in = Some(5.0);

        i.forecast_stale = false;
        let fresh = decide_traced(&i, &p);
        assert_eq!(
            fresh.verdict, "skip",
            "fresh forecast skips for predicted rain"
        );
        assert!(
            !fresh.degraded,
            "fresh station + fresh forecast is not degraded"
        );

        i.forecast_stale = true;
        let stale = decide_traced(&i, &p);
        assert_ne!(
            stale.verdict, "skip",
            "a stale forecast must not skip on its own predicted rain"
        );
        assert!(
            stale.degraded,
            "a stale forecast must mark the decision trace degraded"
        );
    }

    #[test]
    fn forecast_fallback_is_not_degraded_but_unavailable_is() {
        // A headline field served by a valid current-hour forecast (the normal
        // steady state on a per-field-chain install, e.g. wind from a configured
        // forecast source) must NOT flag the decision degraded: it drives the
        // "backup data" push prefix and the degraded-rate metric, which were
        // pinned at 100% forever when ForecastFallback tripped this.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.forecast_stale = false;
        i.live_readings = LiveReadings::ForecastFallback;
        assert!(
            !decide_traced(&i, &p).degraded,
            "a forecast-filled headline field with a fresh forecast is not degraded"
        );
        // Genuinely flying blind (no live station AND no forecast, so inputs are
        // fabricated defaults) still flags degraded.
        i.live_readings = LiveReadings::Unavailable;
        assert!(
            decide_traced(&i, &p).degraded,
            "unavailable live inputs must mark the decision trace degraded"
        );
    }

    /// The four legacy soil zones with default thresholds (70/70/70/85),
    /// for porting the pre-generalization soil tests.
    fn soil4(b: Option<f64>, f: Option<f64>, s: Option<f64>, sh: Option<f64>) -> Vec<ZoneSoil> {
        vec![
            ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: b,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: f,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "side_yard".into(),
                name: "side yard".into(),
                pct: s,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "back_yard_shrubs".into(),
                name: "back yard shrubs".into(),
                pct: sh,
                saturation_pct: 85.0,
                target_min_pct: 25.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
        ]
    }

    /// Scenario battery shared by the decide vs decide_traced parity
    /// tests: one entry per rule in the ladder, so a drift anywhere in
    /// the ladder trips the parity assertions.
    fn parity_scenarios() -> Vec<Inputs> {
        let mut scenarios: Vec<Inputs> = vec![base()];
        let mut push = |f: fn(&mut Inputs)| {
            let mut i = base();
            f(&mut i);
            scenarios.push(i);
        };
        push(|i| i.rain_intensity_now_in_hr = Some(0.05));
        push(|i| i.temp_now_f = 30.0);
        push(|i| {
            i.temp_now_f = 50.0;
            i.temp_min_24h_f = Some(32.0);
        });
        push(|i| i.temp_min_24h_f = None);
        push(|i| i.live_readings = LiveReadings::ForecastFallback);
        push(|i| i.live_readings = LiveReadings::Unavailable);
        push(|i| i.restart_required = true);
        push(|i| i.rain_today_forecast_in = None);
        push(|i| i.rain_next_4h_in = None);
        push(|i| i.forecast_in = None);
        push(|i| i.rain_3day_weighted_in = None);
        push(|i| i.rain_intensity_now_in_hr = None);
        push(|i| {
            i.soil_zones = vec![ZoneSoil {
                planning_forecast_unavailable: true,
                ..Default::default()
            }]
        });
        push(|i| i.soil_temp_yard_min_f = Some(33.0));
        push(|i| i.wind_now_mph = 20.0);
        push(|i| i.wind_max_today_mph = 30.0);
        push(|i| i.rain_today_in = 0.10);
        push(|i| i.rain_observed_recent_in = 1.5);
        push(|i| {
            i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        });
        push(|i| i.rain_next_4h_in = Some(0.20));
        push(|i| {
            i.forecast_in = Some(0.40);
            i.rain_tomorrow_prob_pct = Some(90);
        });
        push(|i| i.rain_3day_weighted_in = Some(1.0));
        push(|i| {
            i.temp_max_3day_f = 98.0;
            i.humidity_now_pct = 70.0;
            i.days_since_significant_rain = 3;
            i.rain_3day_weighted_in = Some(0.0);
        });
        push(|i| i.is_dry_run = true);
        push(|i| i.is_paused = true);
        push(|i| {
            i.is_tomorrow = true;
            i.override_tomorrow = "skip".to_string();
        });
        push(|i| {
            i.is_tomorrow = true;
            i.override_tomorrow = "run".to_string();
            i.rain_today_in = 0.5;
        });
        // Soil-floor demotion: a soft 4h-rain skip + one measured healthy-dry
        // zone -> decide() and decide_traced must agree on ("run","") with only
        // the soil_floor gate firing.
        push(|i| {
            i.rain_next_4h_in = Some(0.50);
            i.soil_zones = soil4(Some(20.0), Some(45.0), Some(45.0), Some(45.0));
        });
        // Sticky global override (the #3 fix): the trace ladder must honor it as
        // the first rung exactly like pre_soil, or the hero verdict and the
        // explainer drift. "run" force-runs THROUGH a hard guard (here a freeze)
        // so the two paths can never diverge on either override direction.
        push(|i| i.global_override = "skip".into());
        push(|i| {
            i.global_override = "run".into();
            i.temp_now_f = 28.0;
            i.min_temp_f = 35.0;
        });
        scenarios
    }

    #[test]
    fn missing_rain_holds_its_enabled_rung_and_known_zero_does_not() {
        type Missing = fn(&mut Inputs);
        let cases: &[(&str, Missing)] = &[
            ("rain_today_forecast", |i| i.rain_today_forecast_in = None),
            ("rain_next_4h", |i| i.rain_next_4h_in = None),
            ("tomorrow_rain", |i| i.forecast_in = None),
            ("rain_3day", |i| i.rain_3day_weighted_in = None),
            ("rain_now", |i| i.rain_intensity_now_in_hr = None),
        ];
        for (id, missing) in cases {
            let mut input = base();
            assert_eq!(
                evaluate_with(&input, &SkipRuleParams::default()).verdict,
                "run"
            );
            missing(&mut input);
            let result = evaluate_with(&input, &SkipRuleParams::default());
            assert_eq!(result.reason_code, *id);
            assert!(result.will_skip && result.reason.contains("unavailable"));
            let trace = decide_traced(&input, &SkipRuleParams::default());
            assert_eq!(trace.reason_code, result.reason_code);
            assert_eq!(trace.reason, result.reason);
            assert!(trace.degraded);
            let row = trace.rules.iter().find(|row| row.id == *id).unwrap();
            assert!(
                row.margin_label.is_none(),
                "unknown rain has no numeric headroom"
            );
            let mut params = SkipRuleParams::default();
            params.disabled_rules.push((*id).into());
            assert_eq!(
                evaluate_with(&input, &params).verdict,
                "run",
                "{id} stays in its disable scope"
            );
        }
        let mut hot = base();
        hot.temp_max_3day_f = 99.0;
        hot.days_since_significant_rain = 10;
        hot.rain_3day_weighted_in = None;
        let mut params = SkipRuleParams::default();
        params.disabled_rules.push("rain_3day".into());
        assert_eq!(
            evaluate_with(&hot, &params).verdict,
            "run",
            "unknown cannot earn a heat extension"
        );
        hot.rain_3day_weighted_in = Some(0.0);
        assert_eq!(evaluate_with(&hot, &params).verdict, "run_extended");
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn planning_rain_hold_is_scoped_and_force_cannot_revive_an_unavailable_plan() {
        let mut input = base();
        input.soil_zones = vec![
            ZoneSoil {
                slug: "held".into(),
                name: "Held".into(),
                planning_forecast_unavailable: true,
                ..Default::default()
            },
            ZoneSoil {
                slug: "ready".into(),
                name: "Ready".into(),
                ..Default::default()
            },
        ];
        input.global_override = "run".into();
        let mut params = SkipRuleParams::default();
        params.disabled_rules.push("planning_forecast".into());
        for soil in [false, true] {
            input.soil_zones[0].governed_by_soil_model = soil;
            let answer = evaluate_decisions(&input, &params, &[], &CompiledScripts::compile(&[]));
            assert_eq!(zv(&answer.zones, "held").reason_code, "planning_forecast");
            assert_eq!(zv(&answer.zones, "held").verdict, "skip");
            assert_eq!(zv(&answer.zones, "ready").verdict, "run");
            assert!(!answer.skip_check.will_skip, "only the affected zone holds");
            let roundtrip = inputs_from_skipcheck(&answer.skip_check);
            assert!(
                roundtrip
                    .soil_zones
                    .iter()
                    .find(|zone| zone.slug == "held")
                    .unwrap()
                    .planning_forecast_unavailable
            );
        }
        input.soil_zones[1].planning_forecast_unavailable = true;
        let answer = evaluate_decisions(&input, &params, &[], &CompiledScripts::compile(&[]));
        assert_eq!(answer.skip_check.reason_code, "planning_forecast");
        assert_eq!(answer.trace.reason_code, "planning_forecast");
        assert!(answer.zones.iter().all(|zone| zone.verdict == "skip"));
    }

    #[test]
    fn soil_configuration_cannot_waive_missing_forecast_rain() {
        let mut input = base();
        input.forecast_in = None;
        input.rain_3day_weighted_in = None;
        input.soil_zones = vec![
            ZoneSoil {
                slug: "soil".into(),
                governed_by_soil_model: true,
                ..Default::default()
            },
            ZoneSoil {
                slug: "weekly".into(),
                ..Default::default()
            },
        ];
        let zones = decide_per_zone(&input, &SkipRuleParams::default(), &[]);
        assert_eq!(zv(&zones, "soil").reason_code, "tomorrow_rain");
        assert_eq!(zv(&zones, "weekly").reason_code, "tomorrow_rain");
        input.forecast_stale = true;
        input.forecast_in = Some(2.0);
        input.rain_3day_weighted_in = Some(5.0);
        assert!(
            decide_per_zone(&input, &SkipRuleParams::default(), &[])
                .iter()
                .all(|zone| zone.verdict == "run"),
            "known stale rain keeps the documented starvation escape"
        );
        input.forecast_in = None;
        assert_eq!(
            zv(
                &decide_per_zone(&input, &SkipRuleParams::default(), &[]),
                "weekly"
            )
            .reason_code,
            "tomorrow_rain"
        );
    }

    /// Parity assertions shared by the default-params and disabled-rules
    /// parity tests.
    fn assert_parity(p: &SkipRuleParams) {
        for (n, i) in parity_scenarios().iter().enumerate() {
            let (v, r) = decide(i, p);
            let t = decide_traced(i, p);
            assert_eq!(t.verdict, v, "verdict drift in scenario {n}");
            assert_eq!(t.reason, r, "reason drift in scenario {n}");
            // Exactly one fired rule (or zero when the default 'run' applies).
            let fired = t.rules.iter().filter(|e| e.outcome == "fired").count();
            assert!(fired <= 1, "more than one fired rule in scenario {n}");
        }
    }

    #[test]
    fn margin_labels_show_distance_to_flip() {
        let p = SkipRuleParams::default();

        // Dry, calm night: the already-wet gate passes with measurable headroom
        // (0.05" floor - 0.00" today). A binary control gate carries no margin.
        let t = decide_traced(&base(), &p);
        let aw = t.rules.iter().find(|r| r.id == "already_wet").unwrap();
        assert_eq!(aw.outcome, "passed");
        assert_eq!(
            aw.margin_label.as_deref(),
            Some("0.05\" of headroom before this skips")
        );
        let dr = t.rules.iter().find(|r| r.id == "dry_run").unwrap();
        assert_eq!(dr.margin_label, None);

        // Make it genuinely wet: already_wet fires 0.05" past its floor.
        let mut wet = base();
        wet.rain_today_in = 0.10;
        let tw = decide_traced(&wet, &p);
        let aw2 = tw.rules.iter().find(|r| r.id == "already_wet").unwrap();
        assert_eq!(aw2.outcome, "fired");
        assert_eq!(
            aw2.margin_label.as_deref(),
            Some("skipped, 0.05\" past the line")
        );
    }

    #[test]
    fn margin_demoted_gate_says_overridden_not_headroom() {
        // Soil-floor demotion: a measured-dry zone (20% < 30% target_min)
        // overrides a 0.50" forecast-rain skip. decide_traced records the
        // rain_next_4h gate as "passed", but its rain is 0.40" OVER the 0.10"
        // skip line. The margin must NOT claim comfortable headroom (the bug the
        // adversarial review caught); it must say the gate was overridden.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_next_4h_in = Some(0.50);
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0),
            saturation_pct: 70.0,
            target_min_pct: 30.0,
            probe_configured: false,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        }];
        let t = decide_traced(&i, &p);
        let row = t.rules.iter().find(|r| r.id == "rain_next_4h").unwrap();
        assert_eq!(row.outcome, "passed", "demotion records the gate as passed");
        let m = row.margin_label.as_deref().unwrap_or("");
        assert!(
            m.contains("past the line") && m.contains("overridden"),
            "demoted gate must read as over-the-line + overridden, got: {m:?}"
        );
        assert!(
            !m.contains("headroom"),
            "demoted gate must NOT claim headroom, got: {m:?}"
        );
    }

    #[test]
    fn margin_boundary_strict_gate_is_headroom_not_overridden() {
        // A strict-inequality gate (wind_now fires on `>`) sitting EXACTLY at its
        // threshold PASSES with zero headroom. It must read as "0 mph of headroom",
        // never "0 mph past the line, but overridden" (nothing overrode it). This
        // is the boundary the non-strict >= approximation got wrong.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.wind_now_mph = i.max_wind_mph; // exactly on the line
        let t = decide_traced(&i, &p);
        let row = t.rules.iter().find(|r| r.id == "wind_now").unwrap();
        assert_eq!(row.outcome, "passed");
        let m = row.margin_label.as_deref().unwrap_or("");
        assert!(
            m.contains("headroom"),
            "strict gate at the line = headroom, got: {m:?}"
        );
        assert!(
            !m.contains("overridden"),
            "nothing overrode a normal boundary pass, got: {m:?}"
        );
    }

    #[test]
    fn trace_is_stable_across_the_clock() {
        // The decision_trace must not bake the live clock into any rule detail:
        // it would mutate every ~10s refresh, defeating the SSE change-gate
        // and reading as noise. Two evaluations 11s apart with identical weather,
        // soil, and control state must produce byte-identical traces.
        let p = SkipRuleParams::default();
        let mut early = base();
        early.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_782_567_229,
        );
        let mut late = base();
        late.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_782_567_240,
        );
        assert_eq!(
            decide_traced(&early, &p),
            decide_traced(&late, &p),
            "decision_trace must not change with the wall clock alone"
        );

        // Also exercise the pause_until FIRING path -- the exact gate whose detail
        // string used to bake in now_epoch. With an active pause (same expiry,
        // different clock) the trace must STILL be byte-identical.
        let mut p_early = base();
        p_early.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_782_567_229,
        );
        p_early.pause_until_epoch = p_early.now_epoch() + 3600;
        let mut p_late = base();
        p_late.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_782_567_240,
        );
        p_late.pause_until_epoch = p_early.pause_until_epoch;
        let te = decide_traced(&p_early, &p);
        assert_eq!(te.verdict, "skip", "an active pause skips");
        assert_eq!(
            te,
            decide_traced(&p_late, &p),
            "active-pause trace must not change with the wall clock"
        );
    }

    /// P1 (units architecture) GUARD: the DECIDED (verdict, reason) tuple is
    /// byte-identical to the pre-P1 baseline for the whole parity battery. The
    /// expected table below was captured from the engine BEFORE the additive
    /// reason_code / operand fields existed; if adding those fields ever perturbs
    /// a verdict or a baked reason string, an entry here fails. This is the
    /// linchpin invariant for the whole units architecture: P1 must be invisible.
    /// The third column is the reason_code the firing rule now emits (additive,
    /// never decision-affecting); it is asserted equal to the FIRING rule's id, so
    /// it can't silently disagree with the ladder.
    const FROZEN_DECIDED: &[(&str, &str, &str)] = &[
        ("run", "", "run"),
        ("skip", "Currently raining (0.05 in/hr)", "rain_now"),
        ("skip", "Freeze risk now (30°F < 38°F)", "freeze_now"),
        (
            "skip",
            "Overnight freeze (32°F low next 24h < 38°F)",
            "overnight_freeze",
        ),
        ("run", "", "run"),
        ("run", "", "run"),
        (
            "skip",
            "Live weather unavailable (no station data or forecast); failing safe",
            "live_data",
        ),
        (
            "skip",
            crate::gates_catalog::RESTART_REQUIRED_REASON,
            "restart_required",
        ),
        (
            "skip",
            "Today's rain forecast unavailable; watering held",
            "rain_today_forecast",
        ),
        (
            "skip",
            "Rain forecast unavailable for the next 4 hours; watering held",
            "rain_next_4h",
        ),
        (
            "skip",
            "Tomorrow's rain forecast unavailable; watering held",
            "tomorrow_rain",
        ),
        (
            "skip",
            "Rain forecast unavailable for the next 3 days; watering held",
            "rain_3day",
        ),
        (
            "skip",
            "Current rain estimate unavailable; watering held",
            "rain_now",
        ),
        ("skip", PLANNING_FORECAST_HOLD_REASON, "planning_forecast"),
        ("skip", "Soil frost (33.0°F < 35°F threshold)", "soil_frost"),
        ("skip", "Wind too high now (20.0 mph > 10 mph)", "wind_now"),
        (
            "skip",
            "Windy day forecast (peak 30 mph > 10 + 5)",
            "wind_forecast",
        ),
        ("skip", "Already wet (0.10\" measured today)", "already_wet"),
        (
            "skip",
            "Already wet (1.50\" rain in the last 2 day(s))",
            "observed_rain",
        ),
        (
            "skip",
            "All zones soil-saturated (tightest: back yard shrubs 90% ≥ 85% threshold)",
            "soil_saturation",
        ),
        (
            "skip",
            "Rain expected within 4h (0.20\" forecast)",
            "rain_next_4h",
        ),
        (
            "skip",
            "Tomorrow rain (0.40\" × 90% confidence)",
            "tomorrow_rain",
        ),
        (
            "skip",
            "Heavy rain in next 3 days (1.00\" weighted)",
            "rain_3day",
        ),
        (
            "run_extended",
            "Heat advisory: running planned + 15% (peak 98°F)",
            "heat_advisory",
        ),
        ("skip", "All watering is on hold", "dry_run"),
        ("skip", "Paused (vacation mode)", "paused"),
        ("skip", "Manual override (skip tomorrow)", "override"),
        ("run", "", "override"),
        ("run", "", "soil_floor"),
        // Intent safety correction: a convenience force may skip rain/soil
        // recommendations, but the same freeze still binds both ladders.
        ("skip", "Manual override: skip", "override"),
        ("skip", "Freeze risk now (28°F < 35°F)", "freeze_now"),
    ];

    #[test]
    fn decided_tuple_unchanged_by_additive_p1_fields() {
        let p = SkipRuleParams::default();
        let scenarios = parity_scenarios();
        assert_eq!(
            scenarios.len(),
            FROZEN_DECIDED.len(),
            "parity battery changed size; refresh FROZEN_DECIDED deliberately"
        );
        for (n, i) in scenarios.iter().enumerate() {
            let s = evaluate_with(i, &p);
            let (ev, er, ec) = FROZEN_DECIDED[n];
            // The decision + baked string must be byte-identical to the baseline.
            assert_eq!(s.verdict, ev, "verdict drifted in scenario {n}");
            assert_eq!(s.reason, er, "baked reason drifted in scenario {n}");
            // And the additive code mirrors the firing rule (here, the frozen id).
            assert_eq!(s.reason_code, ec, "reason_code drifted in scenario {n}");
            // The trace must agree on verdict + reason + code (parity), so the
            // additive fields are consistent across both ladders.
            let t = decide_traced(i, &p);
            assert_eq!(t.verdict, ev, "trace verdict drift in scenario {n}");
            assert_eq!(t.reason, er, "trace reason drift in scenario {n}");
            assert_eq!(t.reason_code, ec, "trace reason_code drift in scenario {n}");
        }
    }

    #[test]
    fn reason_code_matches_firing_rule_for_representative_gates() {
        let p = SkipRuleParams::default();

        // Clean run: nothing fires -> "run".
        assert_eq!(evaluate_with(&base(), &p).reason_code, "run");

        // wind_now.
        let mut i = base();
        i.wind_now_mph = 20.0;
        assert_eq!(evaluate_with(&i, &p).reason_code, "wind_now");

        // freeze_now (temp gate).
        let mut i = base();
        i.temp_now_f = 30.0;
        assert_eq!(evaluate_with(&i, &p).reason_code, "freeze_now");

        // rain_now (rain-rate gate).
        let mut i = base();
        i.rain_intensity_now_in_hr = Some(0.05);
        assert_eq!(evaluate_with(&i, &p).reason_code, "rain_now");

        // observed_rain (the sensor-independent backstop).
        let mut i = base();
        i.rain_observed_recent_in = 1.5;
        assert_eq!(evaluate_with(&i, &p).reason_code, "observed_rain");

        // soil_saturation (all zones at/above threshold).
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        assert_eq!(evaluate_with(&i, &p).reason_code, "soil_saturation");

        // The trace's reason_code mirrors the deciding RuleEval.id.
        let t = decide_traced(&i, &p);
        assert_eq!(t.reason_code, "soil_saturation");
        let fired = t.rules.iter().find(|r| r.outcome == "fired").unwrap();
        assert_eq!(t.reason_code, fired.id);
    }

    #[test]
    fn reason_code_soil_probe_for_quarantined_zone() {
        // A wild-outlier probe (28% vs siblings ~73%) is quarantined; the zone's
        // data hold carries the diagnostic source without claiming saturation.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(28.0), Some(72.0), Some(74.0), Some(90.0));
        let zvs = decide_per_zone(&i, &p, &[]);
        let bad = zvs.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(
            bad.source, "soil_quarantine",
            "the outlier zone is quarantined"
        );
        assert_eq!(bad.reason_code, "soil_probe");
        assert!(
            bad.value.is_none() && bad.threshold.is_none(),
            "unknown moisture has no saturation operands"
        );
    }

    #[test]
    fn suspect_probes_flags_outlier_independent_of_verdict() {
        // A wild-outlier probe (28% vs siblings ~52%) is quarantined. Here a
        // GLOBAL operator pause decides every zone, so the
        // per-zone verdict.source is "global" and the old verdict-gated banner
        // would have hidden the bad probe. suspect_probes still flags it because
        // it reads the quarantine plan off the RAW readings, not the verdict.
        let p = SkipRuleParams::default();
        let mut i = base();
        // 28% vs a ~73% yard is a >35pp outlier -> quarantined (the 2026-06
        // incident's numbers). back_yard at 28% keeps the all-zones
        // soil_saturation gate from firing, so the deciding gate is global.
        i.soil_zones = soil4(Some(28.0), Some(72.0), Some(74.0), Some(76.0));
        // An operator pause precedes the protected probe-data gate and
        // therefore masks its source while the diagnostic remains visible.
        i.is_paused = true;

        let zvs = decide_per_zone(&i, &p, &[]);
        let bad = zvs.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(bad.verdict, "skip");
        assert_eq!(
            bad.source, "global",
            "a global gate masks the per-zone quarantine source"
        );

        // ...yet the verdict-independent surface still flags the bad probe.
        let suspects = suspect_probes(&i, &p);
        let back = suspects.first().unwrap().as_deref();
        assert!(
            back.is_some_and(|r| r.starts_with("Soil probe suspect (28% vs yard")),
            "back_yard probe flagged suspect regardless of verdict: {back:?}"
        );
        // Trustworthy siblings are NOT flagged.
        assert!(suspects[1].is_none());
        assert!(suspects[2].is_none());

        // Disabling quarantine suppresses the surface entirely (parity with the
        // engine's all-None plan).
        let mut p_off = p.clone();
        p_off.soil_quarantine_enabled = false;
        assert!(suspect_probes(&i, &p_off).iter().all(Option::is_none));
    }

    #[test]
    fn rule_eval_operands_populated_for_threshold_gate_none_for_binary() {
        let p = SkipRuleParams::default();
        // wind_now FIRED: value = wind_now_mph, threshold = max_wind_mph, unit_kind
        // = "wind_mph". (wind_now is the first gate to fire, so later threshold
        // gates are not_reached -- the clean-base trace below exercises a PASS.)
        let mut wind = base();
        wind.wind_now_mph = 20.0;
        let tw = decide_traced(&wind, &p);
        let w = tw.rules.iter().find(|r| r.id == "wind_now").unwrap();
        assert_eq!(w.outcome, "fired");
        assert_eq!(w.value, Some(20.0));
        assert_eq!(w.threshold, Some(wind.max_wind_mph));
        assert_eq!(w.unit_kind.as_deref(), Some("wind_mph"));

        // Clean dry/calm night: nothing fires, so every threshold gate is
        // evaluated and PASSES, carrying operands (so the client can show
        // headroom). already_wet passed -> value/threshold/unit_kind set.
        let t = decide_traced(&base(), &p);
        let aw = t.rules.iter().find(|r| r.id == "already_wet").unwrap();
        assert_eq!(aw.outcome, "passed");
        assert_eq!(aw.value, Some(base().rain_today_in));
        assert_eq!(aw.threshold, Some(p.already_wet_in));
        assert_eq!(aw.unit_kind.as_deref(), Some("rain_in"));

        // soil_frost remaps the shared °F display unit to the soil dimension.
        let mut i2 = base();
        i2.soil_temp_yard_min_f = Some(33.0);
        let t2 = decide_traced(&i2, &p);
        let sf = t2.rules.iter().find(|r| r.id == "soil_frost").unwrap();
        assert_eq!(sf.unit_kind.as_deref(), Some("soil_temp_f"));
        assert_eq!(sf.value, Some(33.0));
        assert_eq!(sf.threshold, Some(i2.frost_skip_soil_f));

        // A binary control gate (dry_run) carries NO operands even when evaluated.
        let dr = t.rules.iter().find(|r| r.id == "dry_run").unwrap();
        assert_eq!(dr.outcome, "passed");
        assert_eq!(dr.value, None);
        assert_eq!(dr.threshold, None);
        assert_eq!(dr.unit_kind, None);
        // ...and so does live_data (binary safety gate), also evaluated here.
        let ld = t.rules.iter().find(|r| r.id == "live_data").unwrap();
        assert_eq!(ld.value, None);
        assert_eq!(ld.threshold, None);
        assert_eq!(ld.unit_kind, None);
    }

    #[test]
    fn decide_traced_matches_decide() {
        // The trace's verdict + reason must always equal decide()'s, across
        // every rule. If this fails, the two ladders have drifted.
        assert_parity(&SkipRuleParams::default());
    }

    #[test]
    fn decide_traced_matches_decide_with_disabled_rules() {
        // Same battery, with a representative operator disable set: every
        // category of disableable gate, a protected id (must be ignored),
        // and an unknown id (must be harmless). Parity must still hold,
        // and no disabled rule may ever fire or decide in the trace.
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec![
            "rain_now".into(),
            "overnight_freeze".into(),
            "already_wet".into(),
            "soil_saturation".into(),
            "tomorrow_rain".into(),
            "heat_advisory".into(),
            "live_data".into(),
            "paused".into(),          // protected: ignored
            "not_a_real_rule".into(), // unknown: harmless
        ];
        assert_parity(&p);

        for (n, i) in parity_scenarios().iter().enumerate() {
            let t = decide_traced(i, &p);
            for e in &t.rules {
                if !p.disabled_rules.contains(&e.id) || PROTECTED_RULES.contains(&e.id.as_str()) {
                    continue;
                }
                // Disabled rules stay visible but never decide.
                assert_eq!(
                    e.outcome, "skipped",
                    "disabled rule {} not inert in scenario {n}",
                    e.id
                );
                assert_eq!(e.detail, "disabled by operator", "scenario {n}");
                assert!(e.verdict.is_none(), "scenario {n}");
            }
        }
    }

    #[test]
    fn defaults_match_v01_consts() {
        // Sanity that the default SkipRuleParams produces the same
        // verdicts as the old const-based ladder. This is the contract:
        // upgrading to v2 must not change any verdict for unchanged inputs.
        let p = SkipRuleParams::default();
        assert!((p.already_wet_in - 0.05).abs() < 1e-9);
        assert!((p.rain_now_in_hr - 0.01).abs() < 1e-9);
        assert!((p.rain_next_4h_skip_in - 0.10).abs() < 1e-9);
        assert!((p.rain_3day_factor - 1.5).abs() < 1e-9);
        assert!((p.heat_advisory_temp_f - 95.0).abs() < 1e-9);
        assert!((p.heat_advisory_humidity_pct - 60.0).abs() < 1e-9);
        assert_eq!(p.heat_advisory_dry_days, 2);
        assert!((p.wind_forecast_slack_mph - 5.0).abs() < 1e-9);
    }

    #[test]
    fn pause_until_short_circuits_with_human_reason() {
        let mut i = base();
        i.pause_until_epoch = i.now_epoch() + 3600;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.starts_with("Paused (vacation until"));
    }

    #[test]
    fn pause_until_expired_falls_through() {
        let mut i = base();
        i.pause_until_epoch = i.now_epoch() - 3600;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run");
    }

    #[test]
    fn override_skip_only_applies_to_tomorrow_cell() {
        let mut i = base();
        i.override_tomorrow = "skip".to_string();
        let today = evaluate(&i);
        assert_eq!(today.verdict, "run");
        i.is_tomorrow = true;
        let tomorrow = evaluate(&i);
        assert_eq!(tomorrow.verdict, "skip");
        assert!(tomorrow.reason.contains("Manual override"));
    }

    #[test]
    fn no_skip_when_clear() {
        let s = evaluate(&base());
        assert_eq!(s.verdict, "run");
        assert!(s.reason.is_empty());
    }

    #[test]
    fn currently_raining() {
        let mut i = base();
        i.rain_intensity_now_in_hr = Some(0.05);
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        // base() is Measured (observation-grade) -> a HARD rain_now skip. The
        // top-level reason stays the stable "Currently raining (...)" string (the
        // honest nature rides the trace detail); see the rain_nature tests below.
        assert!(s.reason.starts_with("Currently raining"));
    }

    #[test]
    fn rain_next_4h_skips() {
        let mut i = base();
        i.rain_next_4h_in = Some(0.15);
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("4h"));
    }

    #[test]
    fn tomorrow_high_confidence_skips() {
        let mut i = base();
        i.forecast_in = Some(0.30);
        i.rain_tomorrow_prob_pct = Some(90);
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
    }

    #[test]
    fn tomorrow_amount_with_no_probability_skips_at_full_weight() {
        // The deliberate honest-unknowns verdict flip, pinned: an amount with
        // NO reported probability (legacy HA REST sensor while the daily
        // window lacks tomorrow, or a probability-less provider) weights at
        // FULL value and can skip. The pre-1.18 engine multiplied by a
        // fabricated 0% and watered ahead of every such forecast storm.
        let mut i = base();
        i.forecast_in = Some(0.40); // >= rain_skip_in 0.25 at full weight
        i.rain_tomorrow_prob_pct = None;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(
            s.reason.starts_with("Tomorrow rain (0.40"),
            "reason claims the amount: {}",
            s.reason
        );
        assert!(
            !s.reason.contains("confidence"),
            "no fabricated confidence claim: {}",
            s.reason
        );

        // A REPORTED 0% is a real "model says dry" and still never fires.
        i.rain_tomorrow_prob_pct = Some(0);
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run");
    }

    #[test]
    fn already_wet_uses_default_floor() {
        let mut i = base();
        i.rain_today_in = 0.05;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.starts_with("Already wet"));
    }

    #[test]
    fn already_wet_threshold_is_configurable() {
        let mut i = base();
        i.rain_today_in = 0.05;
        // Operator wants stricter: only count >=0.10" as "wet".
        let mut params = SkipRuleParams::default();
        params.already_wet_in = 0.10;
        let s = evaluate_with(&i, &params);
        assert_eq!(
            s.verdict, "run",
            "0.05\" should not be wet under stricter threshold"
        );

        i.rain_today_in = 0.12;
        let s = evaluate_with(&i, &params);
        assert_eq!(s.verdict, "skip");
    }

    // ── Observed-recent-rain backstop (sensor-independent) ───────────────────

    #[test]
    fn observed_rain_yesterday_skips() {
        // 1.5" measured rain over the recent window (today + yesterday) with the
        // default 0.25" rain_skip threshold: a hard skip, sensor-independent.
        let mut i = base();
        i.rain_observed_recent_in = 1.5;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(
            s.reason.starts_with("Already wet") && s.reason.contains("in the last"),
            "reason should name observed recent rain, got {:?}",
            s.reason
        );
        // window default 1 -> "the last 2 day(s)" (window + 1, today included).
        assert!(
            s.reason.contains("2 day(s)"),
            "default window includes today + 1 past day, got {:?}",
            s.reason
        );
        assert!(s.reason.contains("1.50"));
    }

    #[test]
    fn observed_rain_light_does_not_skip() {
        // 0.10" observed is below the 0.25" rain_skip threshold: no skip.
        let mut i = base();
        i.rain_observed_recent_in = 0.10;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run");
    }

    #[test]
    fn observed_rain_window_zero_labels_today_only() {
        // window = 0 -> "the last 1 day(s)" (today only). The refresher would
        // then feed only today's observed rain; the gate label reflects that.
        let mut i = base();
        i.rain_observed_recent_in = 0.30;
        let mut p = SkipRuleParams::default();
        p.rain_observed_window_days = 0;
        let s = evaluate_with(&i, &p);
        assert_eq!(s.verdict, "skip");
        assert!(
            s.reason.contains("1 day(s)"),
            "window 0 means today only, got {:?}",
            s.reason
        );
    }

    #[test]
    fn observed_rain_beats_soil_floor() {
        // The load-bearing safety property: a measured-dry zone (20% < 30% min)
        // would normally demote a FORECAST-rain skip and run. But heavy OBSERVED
        // rain is a hard skip ordered before the soil_floor moat, so the dry zone
        // must NOT run right after a soaking even though its probe says it's dry.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_observed_recent_in = 1.5;
        i.rain_next_4h_in = Some(0.50); // also a (demotable) forecast-rain skip
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0),
            saturation_pct: 70.0,
            target_min_pct: 30.0,
            probe_configured: false,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        }];
        // Aggregate skips (the hard observed-rain gate wins).
        assert_eq!(decide(&i, &p).0, "skip");
        assert!(evaluate_with(&i, &p).will_skip);
        // The dry zone still skips and is bound by the global gate, not soil_floor.
        let v = decide_per_zone(&i, &p, &[]);
        let back = v.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(
            back.verdict, "skip",
            "dry zone must not run after a soaking"
        );
        assert_ne!(back.source, "soil_floor");
        assert!(back.reason.contains("in the last"));
    }

    #[test]
    fn observed_rain_disabled_by_operator() {
        // Disabling "observed_rain" turns the backstop off; with no other gate
        // tripping, the run proceeds.
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["observed_rain".into()];
        let mut i = base();
        i.rain_observed_recent_in = 1.5;
        assert_eq!(evaluate_with(&i, &p).verdict, "run");
    }

    #[test]
    fn heat_advisory_extends_run() {
        let mut i = base();
        i.temp_max_3day_f = 96.0;
        i.humidity_now_pct = 65.0;
        i.days_since_significant_rain = 3;
        i.rain_3day_weighted_in = Some(0.05);
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run_extended");
    }

    #[test]
    fn heat_advisory_temp_threshold_is_configurable() {
        let mut i = base();
        i.temp_max_3day_f = 92.0; // below default 95
        i.humidity_now_pct = 65.0;
        i.days_since_significant_rain = 3;
        i.rain_3day_weighted_in = Some(0.05);
        // Default config -> not hot enough -> plain run.
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run");
        // Operator drops the heat advisory floor.
        let mut params = SkipRuleParams::default();
        params.heat_advisory_temp_f = 90.0;
        let s = evaluate_with(&i, &params);
        assert_eq!(s.verdict, "run_extended");
    }

    #[test]
    fn soil_frost_skips_when_yard_min_below_threshold() {
        let mut i = base();
        i.soil_temp_yard_min_f = Some(33.0);
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.starts_with("Soil frost"));
    }

    #[test]
    fn yard_wide_saturation_skips_when_all_zones_at_or_above_threshold() {
        let mut i = base();
        i.soil_zones = soil4(Some(72.0), Some(80.0), Some(75.0), Some(90.0));
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.starts_with("All zones soil-saturated"));
        assert!(s.reason.contains("back yard"));
    }

    #[test]
    fn heat_index_below_80_unchanged() {
        assert!((heat_index_f(75.0, 90.0) - 75.0).abs() < 1e-9);
    }

    #[test]
    fn heat_index_at_95_60_in_range() {
        // Steadman 1979 full regression at 95°F, 60% RH yields ~113.1.
        // NOAA's published lookup table (rounded, slightly different
        // coefficient form) lists ~115 for the same inputs. The earlier
        // ha::skip_logic test asserted 100..110 which the formula has
        // never satisfied for these inputs; bound corrected to match
        // the actual Steadman output.
        let hi = heat_index_f(95.0, 60.0);
        assert!(hi > 110.0 && hi < 116.0, "heat index {hi}");
    }

    #[test]
    fn et_multiplier_clamps_low() {
        assert!((et_heat_multiplier(70.0) - 1.0).abs() < 1e-9);
        assert!((et_heat_multiplier(85.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn et_multiplier_clamps_high() {
        assert!((et_heat_multiplier(120.0) - 1.30).abs() < 1e-9);
    }

    #[test]
    fn et_multiplier_midrange() {
        // HI 95: bonus = (95 - 85)/20 * 0.30 = 0.15 -> 1.15
        assert!((et_heat_multiplier(95.0) - 1.15).abs() < 1e-9);
    }

    // ── 3-day peak heat index uses the forecast-derived per-day value ────────

    #[test]
    fn heat_index_3day_uses_forecast_derived_input_not_now_humidity_pairing() {
        // The incident: a post-rain morning. temp_now 72°F, humidity_now 97%
        // (a 3:40am saturated reading), forecast high 93.5°F at ~50% afternoon
        // RH. The OLD code computed heat_index_f(temp_max_3day_f=93.5,
        // humidity_now=97) = a bogus ~147°F. The fix takes the per-day
        // forecast-derived value instead, which is a realistic ~95-100°F.
        let mut i = base();
        i.temp_now_f = 72.0;
        i.humidity_now_pct = 97.0;
        i.temp_max_3day_f = 93.5;
        // What the refresher would set from fc.max_heat_index_n_day(3): the
        // 93.5°F high paired with THAT day's ~50% afternoon humidity.
        let realistic = heat_index_f(93.5, 50.0);
        i.heat_index_max_3day_f = realistic;

        let s = evaluate(&i);

        // The buggy now-humidity pairing the OLD code would have produced.
        let inflated = heat_index_f(93.5, 97.0);
        assert!(inflated > 140.0, "the old pairing overshoots: {inflated}");

        // The SkipCheck surfaces the forecast-derived value, NOT the inflated one.
        assert!(
            (s.heat_index_max_3day_f - realistic).abs() < 1e-9,
            "SkipCheck must carry the forecast-derived per-day value, got {}",
            s.heat_index_max_3day_f
        );
        assert!(
            (90.0..105.0).contains(&s.heat_index_max_3day_f),
            "feels-like is realistic, not ~147°F: {}",
            s.heat_index_max_3day_f
        );

        // The ET heat multiplier of the corrected value is strictly lower than
        // it would have been for the bogus 147°F (which clamps at the +30% cap).
        let corrected_mult = et_heat_multiplier(s.heat_index_max_3day_f);
        let inflated_mult = et_heat_multiplier(inflated);
        assert!(
            corrected_mult < inflated_mult,
            "corrected ET multiplier {corrected_mult} must be below the inflated {inflated_mult}"
        );

        // heat_index_now stays the valid same-time pairing (72°F < 80 -> 72).
        assert!(
            (s.heat_index_now_f - 72.0).abs() < 1e-9,
            "heat_index_now is the unchanged same-time pairing, got {}",
            s.heat_index_now_f
        );
    }

    #[test]
    fn heat_index_3day_high_for_genuinely_hot_humid_forecast() {
        // A genuinely hot AND humid forecast day must still produce a high
        // 3-day heat index so the advisory pre-water path stays meaningful.
        let mut i = base();
        i.temp_max_3day_f = 98.0;
        i.heat_index_max_3day_f = heat_index_f(98.0, 70.0);
        let s = evaluate(&i);
        assert!(
            s.heat_index_max_3day_f >= 110.0,
            "hot+humid forecast must read as a high feels-like, got {}",
            s.heat_index_max_3day_f
        );
        assert!(et_heat_multiplier(s.heat_index_max_3day_f) > 1.15);
    }

    #[test]
    fn soil_frost_no_data_does_not_skip() {
        let mut i = base();
        i.soil_temp_yard_min_f = None;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run");
    }

    #[test]
    fn yard_wide_saturation_does_not_skip_with_one_dry_zone() {
        // Tests the saturation gate's "all zones at/above threshold" logic in
        // isolation. The one below-threshold zone (55%) is within the outlier
        // band of its 72/75/90 siblings (|55-73.5| = 18.5 < 25), so quarantine
        // leaves it alone and the gate correctly stays open -> run.
        let mut i = base();
        i.soil_zones = soil4(Some(72.0), Some(55.0), Some(75.0), Some(90.0));
        assert_eq!(evaluate(&i).verdict, "run");
    }

    #[test]
    fn yard_wide_saturation_does_not_skip_with_partial_data_quarantine_off() {
        // With quarantine OFF, an offline probe keeps the yard-wide gate
        // inapplicable (the pre-quarantine behavior the gate guarantees on its
        // own). Configured offline probes have a separate protected data hold.
        let mut p = SkipRuleParams::default();
        p.soil_quarantine_enabled = false;
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), None, Some(75.0), Some(90.0));
        assert_eq!(evaluate_with(&i, &p).verdict, "run");
    }

    #[test]
    fn soil_frost_takes_priority_over_yard_saturation() {
        let mut i = base();
        i.soil_temp_yard_min_f = Some(30.0);
        i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.starts_with("Soil frost"));
    }

    #[test]
    fn weather_skip_wins_over_dry_run() {
        let mut i = base();
        i.is_dry_run = true;
        i.rain_today_in = 0.10;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.starts_with("Already wet"));
    }

    #[test]
    fn dry_run_skips_with_its_own_reason_when_weather_clear() {
        let mut i = base();
        i.is_dry_run = true;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert_eq!(s.reason, "All watering is on hold");
    }

    #[test]
    fn overnight_freeze_look_ahead() {
        let mut i = base();
        i.temp_now_f = 50.0;
        i.temp_min_24h_f = Some(32.0);
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("Overnight freeze"));
    }

    #[test]
    fn overnight_freeze_fires_on_subzero_low() {
        // Regression: 0.0 used to be the missing-data sentinel, so a real
        // forecast low at or below 0 °F silently disabled the rule.
        let mut i = base();
        i.temp_now_f = 45.0;
        i.temp_min_24h_f = Some(-5.0);
        i.min_temp_f = 38.0;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("Overnight freeze"));
        assert!(s.reason.contains("-5"));
    }

    #[test]
    fn overnight_freeze_missing_forecast_does_not_fire() {
        let mut i = base();
        i.temp_min_24h_f = None;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run");
        // Wire surface: legacy 0.0 placeholder + explicit validity flag.
        assert_eq!(s.temp_min_24h_f, 0.0);
        assert!(!s.temp_min_24h_valid);
        // Traced ladder marks the rule not-applicable, not passed.
        let t = decide_traced(&i, &SkipRuleParams::default());
        let r = t.rules.iter().find(|r| r.id == "overnight_freeze").unwrap();
        assert_eq!(r.outcome, "skipped");
    }

    #[test]
    fn skipcheck_surfaces_overnight_low_validity() {
        let mut i = base();
        i.temp_min_24h_f = Some(-5.0);
        let s = evaluate(&i);
        assert!(s.temp_min_24h_valid);
        assert_eq!(s.temp_min_24h_f, -5.0);
    }

    #[test]
    fn unavailable_live_readings_fail_safe_skip() {
        let mut i = base();
        i.live_readings = LiveReadings::Unavailable;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("Live weather unavailable"));
    }

    #[test]
    fn forecast_fallback_runs_and_is_not_degraded() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.live_readings = LiveReadings::ForecastFallback;
        let t = decide_traced(&i, &p);
        assert_eq!(t.verdict, "run");
        // A headline field served by a valid current-hour forecast (normal on a
        // per-field-chain install) is NOT a degradation: it must not pin the
        // degraded-rate metric or the daily "backup data" push at 100%. Only
        // Unavailable inputs (fabricated defaults) or a stale forecast degrade.
        assert!(!t.degraded);
        // Fresh station data is not degraded either.
        let t2 = decide_traced(&base(), &p);
        assert!(!t2.degraded);
    }

    #[test]
    fn override_run_forces_run_through_weather_skip() {
        let mut i = base();
        i.is_tomorrow = true;
        i.override_tomorrow = "run".to_string();
        i.rain_today_in = 0.5;
        assert_eq!(evaluate(&i).verdict, "run");
    }

    /// The wind gate judges the minutes the yard waters, not the day.
    ///
    /// The daily peak is the afternoon's figure. A yard that finishes
    /// before sunrise was being refused for weather it would never be
    /// out in: a 25 mph afternoon peak skipped a 5 mph dawn.
    #[test]
    fn a_gusty_afternoon_does_not_skip_a_calm_dawn() {
        let mut i = base();
        i.wind_max_today_mph = 25.0;
        i.wind_window_max_mph = Some(5.0);
        let v = evaluate(&i);
        assert_eq!(v.verdict, "run", "{}", v.reason);
    }

    /// And the converse: a windy dawn skips even when the day as a whole
    /// reads mild, because the day as a whole is not when the yard waters.
    #[test]
    fn a_windy_dawn_skips_even_under_a_mild_day_peak() {
        let mut i = base();
        i.wind_max_today_mph = 12.0;
        i.wind_window_max_mph = Some(20.0);
        let v = evaluate(&i);
        assert_eq!(v.verdict, "skip");
        assert_eq!(v.reason_code, "wind_forecast");
        assert!(
            v.reason.contains("run window"),
            "the reason says which minutes were judged: {}",
            v.reason
        );
    }

    /// With no run window to judge, the daily peak still governs. This
    /// is the install with no location, or a strip cell past the hourly
    /// series, and a calm zero would be the wrong thing to invent.
    #[test]
    fn without_a_window_the_day_peak_governs() {
        let mut i = base();
        i.wind_max_today_mph = 25.0;
        i.wind_window_max_mph = None;
        let v = evaluate(&i);
        assert_eq!(v.verdict, "skip");
        assert_eq!(v.reason_code, "wind_forecast");
        assert!(v.reason.starts_with("Windy day forecast"), "{}", v.reason);
    }

    /// A drip bed under a rule that exempts drip waters on a day the
    /// rule refuses the yard, and says why; the spray zone next to it
    /// does not.
    #[test]
    fn a_drip_zone_under_an_exempting_rule_runs_on_a_non_allowed_day() {
        use crate::config::schema::{SprinklerType, WateringRestriction};
        let mut i = base();
        i.watering_restrictions = vec![WateringRestriction {
            id: "schedule".into(),
            name: "Schedule".into(),
            allowed_weekdays: vec![3, 6],
            exempt_sprinklers: vec![SprinklerType::Drip],
            ..Default::default()
        }];
        // Monday 27 July 2026, 06:00 UTC.
        i.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_785_132_000,
        );
        i.soil_zones = vec![
            ZoneSoil {
                slug: "front".into(),
                name: "Front".into(),
                sprinkler_type: SprinklerType::Spray,
                ..Default::default()
            },
            ZoneSoil {
                slug: "beds".into(),
                name: "Beds".into(),
                sprinkler_type: SprinklerType::Drip,
                ..Default::default()
            },
        ];
        let yard = evaluate(&i);
        assert_eq!(yard.reason_code, "restrictions", "{}", yard.reason);
        let zones = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        let front = zones.iter().find(|z| z.zone_slug == "front").unwrap();
        let beds = zones.iter().find(|z| z.zone_slug == "beds").unwrap();
        assert_eq!(front.verdict, "skip");
        assert_eq!(front.reason_code, "restrictions");
        assert_eq!(beds.verdict, "run", "{}", beds.reason);
        assert_eq!(beds.source, "exempt");
        assert!(beds.reason.starts_with("Exempt"), "{}", beds.reason);
    }

    /// The restricted-day zone set the exempt branch is judged against, with
    /// explicit soil so the saturation gate has something to read.
    fn exempting_restriction() -> crate::config::schema::WateringRestriction {
        use crate::config::schema::{SprinklerType, WateringRestriction};
        WateringRestriction {
            id: "schedule".into(),
            name: "Schedule".into(),
            allowed_weekdays: vec![3, 6],
            exempt_sprinklers: vec![SprinklerType::Drip],
            ..Default::default()
        }
    }

    fn spray_and_drip(front_pct: f64, beds_pct: f64) -> Vec<ZoneSoil> {
        use crate::config::schema::SprinklerType;
        vec![
            ZoneSoil {
                slug: "front".into(),
                name: "Front".into(),
                pct: Some(front_pct),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: SprinklerType::Spray,
            },
            ZoneSoil {
                slug: "beds".into(),
                name: "Beds".into(),
                pct: Some(beds_pct),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: SprinklerType::Drip,
            },
        ]
    }

    /// The exemption lifts the ordinance, not the ground. A drip bed the
    /// restriction stands aside for, whose OWN probe reads saturated, skips
    /// for its own soil.
    ///
    /// The exempt branch returns before the per-zone soil gate (that gate is
    /// only reached when the yard verdict was a run), so this verdict used to
    /// be "run". While the dispatcher blanket-held the yard on any aggregate
    /// skip, that only painted a card. The moment the dispatcher started
    /// honoring exemptions it became a valve opening on saturated ground on a
    /// restricted day after rain.
    #[test]
    fn an_exempt_zone_whose_own_soil_is_saturated_still_skips() {
        let mut i = base();
        i.watering_restrictions = vec![exempting_restriction()];
        // Monday 27 July 2026, 06:00 UTC: not an allowed weekday.
        i.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_785_132_000,
        );
        i.soil_zones = spray_and_drip(75.0, 80.0);
        assert_eq!(evaluate(&i).reason_code, "restrictions");
        let zones = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        let beds = zones.iter().find(|z| z.zone_slug == "beds").unwrap();
        assert_eq!(beds.verdict, "skip", "{}", beds.reason);
        assert_eq!(beds.source, "soil_saturation");
        assert_eq!(beds.reason_code, "soil_saturation");
        assert!(beds.reason.starts_with("Soil saturated"), "{}", beds.reason);
        assert_eq!(beds.value, Some(80.0));
        assert_eq!(beds.threshold, Some(70.0));
    }

    /// The same zone with dry ground still runs, so the gate above is the
    /// soil and not the exemption having stopped working.
    #[test]
    fn an_exempt_zone_with_dry_soil_still_runs() {
        let mut i = base();
        i.watering_restrictions = vec![exempting_restriction()];
        i.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_785_132_000,
        );
        i.soil_zones = spray_and_drip(40.0, 45.0);
        let zones = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        let beds = zones.iter().find(|z| z.zone_slug == "beds").unwrap();
        assert_eq!(beds.verdict, "run", "{}", beds.reason);
        assert_eq!(beds.source, "exempt");
    }

    /// The owner's own condition rule holds an exempt zone too. Same reason
    /// as the soil gate: the exempt branch returns before the per-zone rule
    /// pass, and the dispatcher now acts on what it returns, so a rule the
    /// owner wrote would otherwise be silently deleted on restricted days.
    #[test]
    fn an_exempt_zone_under_a_user_condition_skip_still_skips() {
        let mut i = base();
        i.watering_restrictions = vec![exempting_restriction()];
        i.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_785_132_000,
        );
        i.soil_zones = spray_and_drip(40.0, 45.0);
        let rule = ConditionRule {
            id: "beds_wet_enough".into(),
            name: String::new(),
            enabled: true,
            scope: RuleScope::Zones(vec!["beds".into()]),
            condition: ConditionExpr::Compare {
                metric: Metric::ZoneSoilPct,
                op: CmpOp::Gt,
                value: 35.0,
            },
            action: RuleAction::Skip,
        };
        let zones = decide_per_zone(&i, &SkipRuleParams::default(), std::slice::from_ref(&rule));
        let beds = zones.iter().find(|z| z.zone_slug == "beds").unwrap();
        assert_eq!(beds.verdict, "skip", "{}", beds.reason);
        assert_eq!(beds.source, "condition");
        assert_eq!(beds.reason_code, "condition");
    }

    /// The exemption lifts the schedule, not the weather. The same drip
    /// bed on a freezing morning still skips, for the freeze.
    #[test]
    fn an_exempt_zone_is_still_judged_by_the_weather() {
        use crate::config::schema::{SprinklerType, WateringRestriction};
        let mut i = base();
        i.watering_restrictions = vec![WateringRestriction {
            id: "schedule".into(),
            name: "Schedule".into(),
            allowed_weekdays: vec![3, 6],
            exempt_sprinklers: vec![SprinklerType::Drip],
            ..Default::default()
        }];
        i.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_785_132_000,
        );
        i.temp_now_f = 28.0;
        i.soil_zones = vec![ZoneSoil {
            slug: "beds".into(),
            name: "Beds".into(),
            sprinkler_type: SprinklerType::Drip,
            ..Default::default()
        }];
        let zones = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        assert_eq!(zones[0].verdict, "skip");
        assert_eq!(zones[0].reason_code, "freeze_now", "{}", zones[0].reason);
    }

    /// The parity gate that did nothing: identical odd and even rows on
    /// a default install with no parity chosen now bind.
    #[test]
    fn identical_rows_skip_a_monday_without_a_parity() {
        use crate::config::schema::{AddressParity, WateringRestriction};
        let mut i = base();
        i.address_parity = AddressParity::NotApplicable;
        i.watering_restrictions = vec![WateringRestriction {
            id: "two_days".into(),
            name: "Two days a week".into(),
            allowed_weekdays_odd: vec![3, 6],
            allowed_weekdays_even: vec![3, 6],
            ..Default::default()
        }];
        i.when = crate::engine::clock::DecisionTime::at(
            crate::engine::calendar::Calendar::utc(),
            1_785_132_000,
        );
        let v = evaluate(&i);
        assert_eq!(v.verdict, "skip");
        assert_eq!(v.reason_code, "restrictions");
    }

    /// A post-sunrise window is judged on its own hours. The pre-dawn
    /// reading of 30 F is the hour the window was moved to avoid.
    #[test]
    fn a_post_sunrise_window_is_judged_on_its_own_hours() {
        use crate::engine::dispatch_window::WindowKind;
        let mut i = base();
        i.temp_now_f = 30.0;
        i.temp_min_24h_f = Some(28.0);
        i.min_temp_f = 38.0;
        let dawn = evaluate(&i);
        assert_eq!(dawn.reason_code, "freeze_now", "{}", dawn.reason);

        i.run_window = WindowKind::PostSunrise;
        i.window_min_temp_f = Some(42.0);
        let later = evaluate(&i);
        assert_eq!(later.verdict, "run", "{}", later.reason);

        // A post-sunrise window that does not clear the threshold after
        // all (the forecast moved) still skips, and says when.
        i.window_min_temp_f = Some(35.0);
        let cold = evaluate(&i);
        assert_eq!(cold.reason_code, "freeze_now");
        assert!(cold.reason.contains("during the run"), "{}", cold.reason);
    }

    #[test]
    fn wind_slack_is_configurable() {
        let mut i = base();
        i.wind_now_mph = 5.0;
        i.wind_max_today_mph = 13.0; // 13 > 10+0 but < 10+5
                                     // Default slack=5: 13 < 15, no skip.
        assert_eq!(evaluate(&i).verdict, "run");
        // Tighter slack=2: 13 > 12, skip.
        let mut params = SkipRuleParams::default();
        params.wind_forecast_slack_mph = 2.0;
        assert_eq!(evaluate_with(&i, &params).verdict, "skip");
    }

    // ── Per-zone decision (decide_per_zone) ──

    /// A soil-governed zone rides through the forward-rain gates, and
    /// the ENGINE says so.
    ///
    /// This rule existed, but it lived in the refresher, which rewrote
    /// verdicts the engine had already produced by string-matching reason
    /// codes. The decision trace was deliberately left on the raw ladder,
    /// so the trace described a morning that did not happen and the hero
    /// and its own explanation read different copies.
    /// Dry heat evaporates more, and the multiplier now knows it.
    ///
    /// The heat index rises with humidity while evapotranspiration falls
    /// with it. A dry 105 F afternoon in Phoenix scores a LOWER heat
    /// index than a muggy 95 F one on the Gulf Coast while evaporating
    /// considerably more water, so the old multiplier pushed hardest
    /// exactly where demand was lowest.
    #[test]
    fn the_demand_multiplier_follows_drying_power_not_comfort() {
        // Calm, humid air asks nothing extra.
        assert!((et_demand_multiplier(0.5) - 1.0).abs() < 1e-9);
        assert!((et_demand_multiplier(1.0) - 1.0).abs() < 1e-9);
        // Dry air asks more, up to the cap.
        assert!((et_demand_multiplier(2.0) - 1.15).abs() < 1e-9);
        assert!((et_demand_multiplier(3.0) - 1.30).abs() < 1e-9);
        assert!((et_demand_multiplier(6.0) - 1.30).abs() < 1e-9);
        // The old measure got Phoenix versus Jacksonville backwards:
        // 105 F at 15% RH has a lower heat index than 95 F at 70% RH.
        let phoenix = heat_index_f(105.0, 15.0);
        let jacksonville = heat_index_f(95.0, 70.0);
        assert!(
            phoenix < jacksonville,
            "heat index rises with humidity: {phoenix} vs {jacksonville}"
        );
        assert!(
            et_heat_multiplier(phoenix) < et_heat_multiplier(jacksonville),
            "so the old multiplier pushed the humid yard harder"
        );
    }

    /// An arid yard gets a heat response.
    ///
    /// The advisory used to require humidity at or above a threshold,
    /// defaulting to 60%, so Phoenix at 110 F and 15% RH never
    /// qualified while a muggy afternoon did. Dry heat is the case that
    /// most needs the extension.
    #[test]
    fn a_dry_heat_wave_earns_the_advisory() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.temp_max_3day_f = p.heat_advisory_temp_f + 5.0;
        i.humidity_now_pct = 12.0;
        i.days_since_significant_rain = p.heat_advisory_dry_days + 1;
        i.rain_3day_weighted_in = Some(0.0);
        i.rain_skip_in = 0.25;
        let s = evaluate_with(&i, &p);
        assert_eq!(
            s.verdict, "run_extended",
            "arid heat extends the run: {} / {}",
            s.verdict, s.reason
        );
        assert_eq!(s.reason_code, "heat_advisory");
    }

    #[test]
    fn dry_and_humid_heat_share_trace_and_hold_all_precedence() {
        let p = SkipRuleParams::default();
        for humidity in [12.0, 75.0] {
            for held in [false, true] {
                let mut i = base();
                i.temp_max_3day_f = 110.0;
                i.humidity_now_pct = humidity;
                i.days_since_significant_rain = 10;
                i.is_dry_run = held;
                let decided = evaluate_with(&i, &p);
                let traced = decide_traced(&i, &p);
                assert_eq!(decided.verdict, if held { "skip" } else { "run_extended" });
                assert_eq!(traced.verdict, decided.verdict);
                assert_eq!(traced.reason_code, decided.reason_code);
                assert_eq!(traced.reason, decided.reason);
                assert_eq!(
                    traced.rules.iter().filter(|r| r.outcome == "fired").count(),
                    1
                );
            }
        }
    }

    #[test]
    fn hold_all_prevents_soil_floor_demotion_and_preserves_trace_parity() {
        let p = SkipRuleParams::default();
        for held in [false, true] {
            let mut i = base();
            i.soil_zones = soil4(Some(20.0), Some(20.0), Some(20.0), Some(20.0));
            i.rain_next_4h_in = Some(1.0);
            i.temp_max_3day_f = 110.0;
            i.days_since_significant_rain = 10;
            i.is_dry_run = held;
            assert_eq!(soil_floor_demotes(&i, &p, &disabled_set(&p)), !held);
            let decided = evaluate_with(&i, &p);
            let traced = decide_traced(&i, &p);
            assert_eq!(decided.verdict, if held { "skip" } else { "run" });
            assert_eq!(traced.verdict, decided.verdict);
            assert_eq!(traced.reason_code, decided.reason_code);
            assert_eq!(traced.reason, decided.reason);
            assert_eq!(
                traced.rules.iter().filter(|r| r.outcome == "fired").count(),
                1
            );
            assert!(decide_per_zone(&i, &p, &[])
                .iter()
                .all(|z| z.verdict == decided.verdict));
        }
    }

    #[test]
    fn a_soil_governed_zone_rides_through_forecast_rain() {
        let p = SkipRuleParams::default();
        let mut i = base();
        // A yard-wide skip on rain three days out, which the soil model
        // has already credited against each zone's deficit.
        i.rain_3day_weighted_in = Some(5.0);
        i.rain_skip_in = 0.25;
        i.soil_zones = vec![
            ZoneSoil {
                slug: "soil_zone".into(),
                name: "Soil zone".into(),
                pct: Some(50.0),
                saturation_pct: 90.0,
                target_min_pct: 20.0,
                probe_configured: false,
                governed_by_soil_model: true,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "weekly_zone".into(),
                name: "Weekly zone".into(),
                pct: Some(50.0),
                saturation_pct: 90.0,
                target_min_pct: 20.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
        ];

        let verdicts = decide_per_zone(&i, &p, &[]);
        let soil = verdicts
            .iter()
            .find(|v| v.zone_slug == "soil_zone")
            .expect("soil zone verdict");
        let weekly = verdicts
            .iter()
            .find(|v| v.zone_slug == "weekly_zone")
            .expect("weekly zone verdict");

        assert_eq!(soil.verdict, "run", "the soil zone waters anyway");
        assert_eq!(soil.source, "soil_model", "and says who decided");
        assert!(
            soil.reason.contains("already count this forecast rain"),
            "the reason explains why: {}",
            soil.reason
        );
        assert_eq!(
            weekly.verdict, "skip",
            "the weekly zone still obeys the yard-wide gate"
        );
    }

    /// Inertness is only for gates the soil model has already accounted
    /// for. Safety, law and rain falling right now still bind every zone.
    #[test]
    fn a_soil_governed_zone_still_obeys_the_gates_that_are_not_inert() {
        let p = SkipRuleParams::default();
        let mut i = base();
        // Freeze is not a forward-rain gate.
        i.temp_now_f = 20.0;
        i.min_temp_f = 38.0;
        i.soil_zones = vec![ZoneSoil {
            slug: "soil_zone".into(),
            name: "Soil zone".into(),
            pct: Some(30.0),
            saturation_pct: 90.0,
            target_min_pct: 20.0,
            probe_configured: false,
            governed_by_soil_model: true,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        }];

        let v = decide_per_zone(&i, &p, &[]);
        assert_eq!(v[0].verdict, "skip", "freeze binds a soil zone too");
        assert_ne!(v[0].source, "soil_model");
    }

    #[test]
    fn decide_per_zone_matches_decide_when_uniform() {
        // With a UNIFORM soil state across zones, every per-zone verdict
        // must equal decide()'s aggregate verdict. (Reasons may differ:
        // the aggregate orders soil before rain-forecast, the per-zone
        // path orders global weather first, but the VERDICT agrees.)
        let p = SkipRuleParams::default();
        let mut scenarios = vec![];
        let mut push = |f: fn(&mut Inputs)| {
            let mut i = base();
            i.soil_zones = soil4(Some(20.0), Some(20.0), Some(20.0), Some(20.0));
            f(&mut i);
            scenarios.push(i);
        };
        push(|_| {}); // all dry, clear -> run
        push(|i| i.soil_zones = soil4(Some(90.0), Some(90.0), Some(90.0), Some(95.0))); // all sat -> skip
        push(|i| i.rain_today_in = 0.10); // weather skip binds all
        push(|i| {
            i.temp_max_3day_f = 98.0;
            i.humidity_now_pct = 70.0;
            i.days_since_significant_rain = 3;
        }); // heat -> run_extended
            // Uniform all-dry + soft forecast rain: the soil floor demotes yard-wide,
            // so decide() AND every per-zone verdict are "run" -> they still AGREE.
            // Only a MIXED yard diverges (soil_floor_demotes_soft_rain_per_zone).
        push(|i| i.rain_next_4h_in = Some(0.50));

        for (n, i) in scenarios.iter().enumerate() {
            let (agg, _) = decide(i, &p);
            let zv = decide_per_zone(i, &p, &[]);
            assert_eq!(zv.len(), 4, "scenario {n}");
            for z in &zv {
                assert_eq!(
                    z.verdict, agg,
                    "zone {} verdict drift vs aggregate in scenario {n}",
                    z.zone_slug
                );
            }
        }
    }

    // ── measured-dry-soil veto (the soil_floor moat) ───────────────────

    /// Find a zone's verdict by slug.
    fn zv<'a>(v: &'a [ZoneVerdict], slug: &str) -> &'a ZoneVerdict {
        v.iter()
            .find(|z| z.zone_slug == slug)
            .unwrap_or_else(|| panic!("no verdict for {slug}"))
    }

    #[test]
    fn soil_floor_demotable_is_post_soil_rain_and_not_protected() {
        // The moat's scope is the three soft forecast-rain gates PLUS the model-
        // grade currently-raining gate (rain_now, demotable ONLY when the rain is a
        // model estimate; an observation-grade rain_now is a hard pre_soil skip and
        // is never demoted). None is a protected/operator-control rule. Pins the
        // allow-list so it can never be silently widened to a hard skip.
        for id in SOIL_FLOOR_DEMOTABLE {
            assert!(
                ["rain_now", "rain_next_4h", "tomorrow_rain", "rain_3day"].contains(id),
                "{id} is not a soft rain gate"
            );
            assert!(!PROTECTED_RULES.contains(id), "{id} must not be protected");
        }
    }

    #[test]
    fn soil_floor_demotes_soft_rain_per_zone() {
        // The load-bearing MIXED-yard test: a soft 4h-rain skip, one healthy-dry
        // zone (20% < 30% min) and one wet zone (45%). The dry zone RUNS via the
        // floor; the wet zone SKIPS on the (demoted) global rain skip; the
        // aggregate runs (will_skip bypassed).
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_next_4h_in = Some(0.50);
        i.soil_zones = vec![
            ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: Some(20.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: Some(45.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
        ];
        let v = decide_per_zone(&i, &p, &[]);
        assert_eq!(zv(&v, "back_yard").verdict, "run");
        assert_eq!(zv(&v, "back_yard").source, "soil_floor");
        assert!(zv(&v, "back_yard").reason.contains("20% < 30%"));
        assert_eq!(zv(&v, "front_yard").verdict, "skip");
        assert_eq!(zv(&v, "front_yard").source, "global");
        assert_eq!(decide(&i, &p).0, "run");
        assert!(!evaluate_with(&i, &p).will_skip);
    }

    #[test]
    fn soil_floor_never_demotes_hard_skip() {
        // Every hard skip beats the dry-soil floor. Each case is a demotable-
        // LOOKING morning (rain_next_4h tripped) + a healthy-dry zone + one hard
        // condition; the zone must still SKIP and never carry source "soil_floor".
        let p = SkipRuleParams::default();
        let dry = || {
            vec![ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: Some(20.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            }]
        };
        type Mut = fn(&mut Inputs);
        let cases: &[(&str, Mut)] = &[
            ("rain_now", |i| i.rain_intensity_now_in_hr = Some(0.05)),
            ("freeze_now", |i| i.temp_now_f = 30.0),
            ("overnight_freeze", |i| {
                i.temp_now_f = 50.0;
                i.temp_min_24h_f = Some(32.0);
            }),
            ("soil_frost", |i| i.soil_temp_yard_min_f = Some(33.0)),
            ("wind_now", |i| i.wind_now_mph = 20.0),
            ("wind_forecast", |i| i.wind_max_today_mph = 30.0),
            ("already_wet", |i| i.rain_today_in = 0.10),
            ("paused", |i| i.is_paused = true),
            ("pause_until", |i| {
                i.pause_until_epoch = i.now_epoch() + 3600;
            }),
            ("global_override", |i| i.global_override = "skip".into()),
            ("live_data", |i| i.live_readings = LiveReadings::Unavailable),
            // RISK A: dry_run fires in post_soil AFTER the rain gates; the floor
            // suppresses rain, but dry_run must still win (it's not demotable).
            ("dry_run", |i| i.is_dry_run = true),
        ];
        for (name, mutate) in cases {
            let mut i = base();
            i.rain_next_4h_in = Some(0.50);
            i.soil_zones = dry();
            mutate(&mut i);
            let v = decide_per_zone(&i, &p, &[]);
            assert_eq!(
                zv(&v, "back_yard").verdict,
                "skip",
                "hard skip {name} must beat the soil floor"
            );
            assert_ne!(
                zv(&v, "back_yard").source,
                "soil_floor",
                "hard skip {name} must not demote"
            );
            assert_eq!(decide(&i, &p).0, "skip", "aggregate must skip for {name}");
        }
    }

    #[test]
    fn soil_floor_aggregate_skip_for_saturated_yard() {
        // A fully-saturated yard on a demotable-looking morning: no zone can be
        // healthy-dry (sat >= target), so the yard skips and nothing demotes.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_next_4h_in = Some(0.50);
        i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        assert_eq!(decide(&i, &p).0, "skip");
        for z in decide_per_zone(&i, &p, &[]) {
            assert_eq!(z.verdict, "skip");
            assert_ne!(z.source, "soil_floor");
        }
    }

    #[test]
    fn soil_floor_fail_safe_missing_flatline_and_zero_target() {
        // The probe-trust guards: a missing probe (None), a flatlined dead probe
        // (0.0), and an unconfigured floor (target_min 0) all fail safe to the
        // soft-rain SKIP, never the veto.
        let p = SkipRuleParams::default();
        let mk = |pct: Option<f64>, target: f64| {
            let mut i = base();
            i.rain_next_4h_in = Some(0.50);
            i.soil_zones = vec![ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct,
                saturation_pct: 70.0,
                target_min_pct: target,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            }];
            i
        };
        for (label, i) in [
            ("missing", mk(None, 30.0)),
            ("flatline_zero", mk(Some(0.0), 30.0)),
            ("zero_target", mk(Some(20.0), 0.0)),
        ] {
            let v = decide_per_zone(&i, &p, &[]);
            assert_eq!(zv(&v, "back_yard").verdict, "skip", "{label} must not veto");
            assert_eq!(decide(&i, &p).0, "skip", "{label} aggregate must skip");
        }
    }

    #[test]
    fn soil_floor_disabled_by_operator() {
        // With soil_floor disabled, a dry zone behaves exactly as before: the
        // soft rain skip binds it and the aggregate skips.
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["soil_floor".into()];
        let mut i = base();
        i.rain_next_4h_in = Some(0.50);
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0),
            saturation_pct: 70.0,
            target_min_pct: 30.0,
            probe_configured: false,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        }];
        assert_eq!(decide(&i, &p).0, "skip");
        assert_eq!(
            zv(&decide_per_zone(&i, &p, &[]), "back_yard").verdict,
            "skip"
        );
    }

    // ── Observation-grade-only HARD rain skip (rain_nature) ──────────────────

    /// A healthy-dry zone over a currently-raining reading. Shared by the three
    /// rain-nature cases below; only `rain_nature` differs between them.
    fn raining_with_dry_zone(nature: RainNature) -> Inputs {
        let mut i = base();
        i.rain_intensity_now_in_hr = Some(0.05); // over the default rain_now threshold
        i.rain_nature = nature;
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0), // measured-dry: below the 30% floor
            saturation_pct: 70.0,
            target_min_pct: 30.0,
            probe_configured: false,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        }];
        i
    }

    #[test]
    fn model_rain_does_not_hard_skip_and_is_demotable() {
        // A MODEL-grade "currently raining" estimate (Open-Meteo / Met.no current-
        // hour precip) over the threshold is only a SOFT skip: a measured-dry zone
        // demotes it to a run via the soil_floor moat. It must NOT hard-skip.
        let p = SkipRuleParams::default();
        let i = raining_with_dry_zone(RainNature::Model);

        // Aggregate: the dry-soil floor demotes the model rain to a run.
        let (verdict, _reason, code) = decide_with_code(&i, &p);
        assert_eq!(
            verdict, "run",
            "model rain must be demotable, not a hard skip"
        );
        assert_eq!(code, "soil_floor", "the moat demotes a model-rain estimate");

        // Per-zone: the measured-dry zone runs, sourced from the soil_floor rung.
        let pz = decide_per_zone(&i, &p, &[]);
        let z = zv(&pz, "back_yard");
        assert_eq!(z.verdict, "run");
        assert_eq!(z.source, "soil_floor");

        // Trace: the rain_now row PASSES (demoted, NOT a hard decider) and the
        // soil_floor gate fires the run. The honest soft-vs-hard distinction is
        // carried by this outcome (passed + soil_floor demotes), with the rain
        // NATURE itself surfaced on the snapshot's Forecast.rain_nature badge.
        let t = decide_traced(&i, &p);
        assert_eq!(t.verdict, "run");
        let rn = t.rules.iter().find(|r| r.id == "rain_now").unwrap();
        assert_eq!(
            rn.outcome, "passed",
            "model rain_now is demoted, not deciding"
        );
        let sf = t.rules.iter().find(|r| r.id == "soil_floor").unwrap();
        assert_eq!(sf.outcome, "fired");
    }

    #[test]
    fn measured_and_radar_rain_hard_skip_past_a_dry_zone() {
        // OBSERVATION-grade rain (a LAN gauge / NWS observation = Measured, or NOAA
        // MRMS radar = RadarQpe) is ground truth: it HARD-skips and binds even a
        // measured-dry zone (the soil_floor moat can never demote it). The top-level
        // reason stays the stable "Currently raining" string; the honest nature is
        // surfaced on the snapshot's Forecast.rain_nature badge.
        let p = SkipRuleParams::default();

        for nature in [RainNature::Measured, RainNature::RadarQpe] {
            let i = raining_with_dry_zone(nature);

            let (verdict, reason, code) = decide_with_code(&i, &p);
            assert_eq!(verdict, "skip", "{nature:?} must hard-skip");
            assert_eq!(code, "rain_now", "{nature:?} fires the rain_now gate");
            assert!(
                reason.starts_with("Currently raining"),
                "top-level reason stays the stable currently-raining string"
            );

            // The dry zone cannot demote an observation-grade rain skip.
            let pz = decide_per_zone(&i, &p, &[]);
            let z = zv(&pz, "back_yard");
            assert_eq!(z.verdict, "skip", "{nature:?} binds the dry zone");
            assert_ne!(z.source, "soil_floor", "{nature:?} is not demotable");

            // Trace: rain_now FIRES (it decides) and soil_floor never demotes.
            let t = decide_traced(&i, &p);
            assert_eq!(t.verdict, "skip");
            let rn = t.rules.iter().find(|r| r.id == "rain_now").unwrap();
            assert_eq!(rn.outcome, "fired", "{nature:?} rain_now must fire");
            let sf = t.rules.iter().find(|r| r.id == "soil_floor").unwrap();
            assert_ne!(sf.outcome, "fired", "{nature:?} must not be demoted");
        }
    }

    #[test]
    fn soil_floor_still_hard_skips_on_saturation_under_model_rain() {
        // The soil_floor design stays intact: a fully-saturated yard still SKIPS on
        // a model-rain morning (no zone can be healthy-dry, so nothing demotes),
        // exactly as for the forecast-rain gates. Pins that routing rain_now into
        // the demotable tier did not weaken the saturation hard skip.
        let p = SkipRuleParams::default();
        let mut i = raining_with_dry_zone(RainNature::Model);
        // Saturate every zone: sat >= target, so no zone is healthy-dry.
        i.soil_zones = soil4(Some(90.0), Some(90.0), Some(90.0), Some(90.0));
        assert_eq!(
            decide(&i, &p).0,
            "skip",
            "a saturated yard still skips under model rain"
        );
        for z in decide_per_zone(&i, &p, &[]) {
            assert_eq!(z.verdict, "skip");
            assert_ne!(z.source, "soil_floor");
        }
    }

    // ── end-to-end golden matrix ───────────────────────────────────────
    // Every gate firing in isolation (ladder order), the key head-to-head
    // precedence cases, the soil_floor demotion, heat run_extended, the stale-
    // forecast suppression, and the default run. Verdict + reason-substring are
    // pinned against the real gate format strings, so any drift is one readable
    // failure.
    /// A forecast is not rain that fell.
    ///
    /// `already_wet` is the reactive gate: its whole job is "did enough
    /// water already land here". It used to be fed `max(gauge, model)`,
    /// so on a July morning where the model expected an afternoon storm
    /// the yard hard-skipped and the dashboard reported "Already wet
    /// (0.34" today)" over dry ground. In a convective climate that is
    /// most mornings, and the observations ledger correctly refuses to
    /// record a model total as measurement, so the miss left no trace.
    ///
    /// Both halves are asserted here: the measured gate must ignore a
    /// forecast, and the modelled gate must never claim measurement.
    #[test]
    fn a_forecast_never_fires_the_measured_rain_gate() {
        let p = SkipRuleParams::default();

        // Model says a storm is coming; nothing has fallen.
        let mut i = Inputs::default();
        i.rain_today_forecast_in = Some(0.40);
        i.rain_today_in = 0.0;
        let (verdict, reason, code) = decide_with_code(&i, &p);
        assert_eq!(verdict, "skip", "an expected storm still holds the yard");
        assert_eq!(
            code, "rain_today_forecast",
            "but on the MODELLED rung, not the measured one"
        );
        assert!(
            reason.contains("expected") && !reason.starts_with("Already wet"),
            "the reason must not claim the rain fell: {reason}"
        );

        // Same number, actually caught by a gauge.
        let mut i = Inputs::default();
        i.rain_today_in = 0.40;
        let (verdict, reason, code) = decide_with_code(&i, &p);
        assert_eq!(verdict, "skip");
        assert_eq!(
            code, "already_wet",
            "a measured total fires the measured gate"
        );
        assert!(
            reason.contains("measured"),
            "and says so, so the owner can tell the two apart: {reason}"
        );

        // Case 1 above already proves the separation: the same 0.40 that
        // fires the modelled rung leaves `already_wet` unfired, because
        // the measured total is zero and the codes differ.
    }

    #[test]
    fn golden_verdict_matrix() {
        type Mut = fn(&mut Inputs);
        let rows: &[(&str, Mut, &str, &str)] = &[
            ("default_clear", |_| {}, "run", ""),
            // each gate in isolation, ladder order
            (
                "override_skip",
                |i| i.global_override = "skip".into(),
                "skip",
                "Manual override: skip",
            ),
            (
                "override_run",
                |i| i.global_override = "run".into(),
                "run",
                "Manual override: force run",
            ),
            (
                "pause_until",
                |i| i.pause_until_epoch = i.now_epoch() + 3600,
                "skip",
                "Paused (vacation until",
            ),
            (
                "paused",
                |i| i.is_paused = true,
                "skip",
                "Paused (vacation mode)",
            ),
            (
                "live_data",
                |i| i.live_readings = LiveReadings::Unavailable,
                "skip",
                "Live weather unavailable",
            ),
            (
                "rain_now",
                |i| i.rain_intensity_now_in_hr = Some(0.05),
                "skip",
                "Currently raining",
            ),
            (
                "freeze_now",
                |i| i.temp_now_f = 30.0,
                "skip",
                "Freeze risk now",
            ),
            (
                "overnight_freeze",
                |i| {
                    i.temp_now_f = 50.0;
                    i.temp_min_24h_f = Some(32.0);
                },
                "skip",
                "Overnight freeze",
            ),
            (
                "soil_frost",
                |i| i.soil_temp_yard_min_f = Some(33.0),
                "skip",
                "Soil frost",
            ),
            (
                "wind_now",
                |i| i.wind_now_mph = 20.0,
                "skip",
                "Wind too high now",
            ),
            (
                "wind_forecast",
                |i| i.wind_max_today_mph = 30.0,
                "skip",
                "Windy day forecast",
            ),
            (
                "already_wet",
                |i| i.rain_today_in = 0.10,
                "skip",
                "Already wet",
            ),
            // The modelled twin. Same threshold, different rung, and the
            // reason has to say "expected" so a forecast is never read as
            // rain that fell.
            (
                "rain_today_forecast",
                |i| i.rain_today_forecast_in = Some(0.10),
                "skip",
                "Rain forecast today",
            ),
            (
                "observed_rain",
                |i| i.rain_observed_recent_in = 1.5,
                "skip",
                "in the last",
            ),
            (
                "soil_saturation",
                |i| i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0)),
                "skip",
                "All zones soil-saturated",
            ),
            (
                "rain_next_4h",
                |i| i.rain_next_4h_in = Some(0.20),
                "skip",
                "Rain expected within 4h",
            ),
            (
                "tomorrow_rain",
                |i| {
                    i.forecast_in = Some(0.40);
                    i.rain_tomorrow_prob_pct = Some(90);
                },
                "skip",
                "Tomorrow rain",
            ),
            (
                "rain_3day",
                |i| i.rain_3day_weighted_in = Some(1.0),
                "skip",
                "Heavy rain in next 3 days",
            ),
            (
                "heat_advisory",
                |i| {
                    i.temp_max_3day_f = 98.0;
                    i.humidity_now_pct = 70.0;
                    i.days_since_significant_rain = 3;
                    i.rain_3day_weighted_in = Some(0.0);
                },
                "run_extended",
                "Heat advisory",
            ),
            (
                "dry_run",
                |i| i.is_dry_run = true,
                "skip",
                "All watering is on hold",
            ),
            // precedence: the earlier gate wins when two fire
            (
                "override_run_beats_rain",
                |i| {
                    i.rain_intensity_now_in_hr = Some(0.05);
                    i.global_override = "run".into();
                },
                "run",
                "Manual override: force run",
            ),
            (
                "pause_beats_weather",
                |i| {
                    i.is_paused = true;
                    i.rain_today_in = 0.10;
                },
                "skip",
                "Paused (vacation mode)",
            ),
            (
                "live_data_beats_weather",
                |i| {
                    i.live_readings = LiveReadings::Unavailable;
                    i.rain_today_in = 0.10;
                },
                "skip",
                "Live weather unavailable",
            ),
            (
                "rain_now_beats_freeze",
                |i| {
                    i.rain_intensity_now_in_hr = Some(0.05);
                    i.temp_now_f = 30.0;
                },
                "skip",
                "Currently raining",
            ),
            (
                "soil_frost_beats_saturation",
                |i| {
                    i.soil_temp_yard_min_f = Some(30.0);
                    i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
                },
                "skip",
                "Soil frost",
            ),
            (
                "saturation_beats_rain_4h",
                |i| {
                    i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
                    i.rain_next_4h_in = Some(0.20);
                },
                "skip",
                "All zones soil-saturated",
            ),
            (
                "already_wet_beats_dry_run",
                |i| {
                    i.is_dry_run = true;
                    i.rain_today_in = 0.10;
                },
                "skip",
                "Already wet",
            ),
            // soil_floor demotion (aggregate decide -> run) + stale suppression
            (
                "soil_floor_demotes_4h",
                |i| {
                    i.rain_next_4h_in = Some(0.50);
                    i.soil_zones = soil4(Some(20.0), Some(45.0), Some(45.0), Some(45.0));
                },
                "run",
                "",
            ),
            (
                "stale_forecast_no_skip",
                |i| {
                    i.rain_3day_weighted_in = Some(5.0);
                    i.forecast_stale = true;
                },
                "run",
                "",
            ),
        ];
        let p = SkipRuleParams::default();
        for (name, mutate, want_v, want_r) in rows {
            let mut i = base();
            mutate(&mut i);
            let s = evaluate_with(&i, &p);
            assert_eq!(
                &s.verdict, want_v,
                "verdict for {name}: reason={:?}",
                s.reason
            );
            assert!(
                s.reason.contains(want_r),
                "reason for {name}: got {:?}, want substr {want_r:?}",
                s.reason
            );
        }
    }

    #[test]
    fn per_zone_soil_diverges() {
        // One zone saturated, one dry, clear weather: the saturated zone
        // skips on its own while the dry zone runs.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), Some(25.0), None, None);
        let zv = decide_per_zone(&i, &p, &[]);
        let back = zv.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        let front = zv.iter().find(|z| z.zone_slug == "front_yard").unwrap();
        assert_eq!(back.verdict, "skip");
        assert_eq!(back.source, "soil_saturation");
        assert_eq!(front.verdict, "run");
    }

    #[test]
    fn soil_gate_detail_names_zones_missing_readings() {
        // Two probes offline (front_yard flatlined, shrubs unassigned):
        // the inapplicable gate's detail must name them, not the old
        // generic "not all zones have soil sensors". Missing readings remain
        // unknown regardless of whether outlier detection is enabled.
        let mut p = SkipRuleParams::default();
        p.soil_quarantine_enabled = false;
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), None, Some(80.0), None);
        let t = decide_traced(&i, &p);
        let g = t.rules.iter().find(|r| r.id == "soil_saturation").unwrap();
        assert_eq!(g.outcome, "skipped");
        assert_eq!(g.detail, "no soil reading: front_yard, back_yard_shrubs");
    }

    #[test]
    fn soil_gate_detail_distinguishes_unconfigured_from_dead_probes() {
        // No soil zones configured at all (weather-only deployment) is a
        // different inapplicability than a dead probe.
        let i = base();
        let t = decide_traced(&i, &SkipRuleParams::default());
        let g = t.rules.iter().find(|r| r.id == "soil_saturation").unwrap();
        assert_eq!(g.outcome, "skipped");
        assert_eq!(g.detail, "no soil zones configured");
    }

    #[test]
    fn global_gate_binds_all_zones() {
        // A global safety gate (freeze) forces EVERY zone to skip, even a
        // bone-dry one that would otherwise want water.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(20.0), Some(20.0), Some(20.0), Some(20.0));
        i.temp_now_f = 30.0;
        let zv = decide_per_zone(&i, &p, &[]);
        assert!(zv
            .iter()
            .all(|z| z.verdict == "skip" && z.source == "global"));
    }

    #[test]
    fn condition_rule_skips_scoped_zone_only() {
        // A user rule scoped to front_yard skips only that zone.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(40.0), Some(40.0), None, None);
        let rule = ConditionRule {
            id: "front_wet".into(),
            name: String::new(),
            enabled: true,
            scope: RuleScope::Zones(vec!["front_yard".into()]),
            condition: ConditionExpr::Compare {
                metric: Metric::ZoneSoilPct,
                op: CmpOp::Gt,
                value: 35.0,
            },
            action: RuleAction::Skip,
        };
        let zv = decide_per_zone(&i, &p, std::slice::from_ref(&rule));
        let back = zv.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        let front = zv.iter().find(|z| z.zone_slug == "front_yard").unwrap();
        assert_eq!(front.verdict, "skip");
        assert_eq!(front.source, "condition");
        assert_eq!(back.verdict, "run", "out-of-scope zone unaffected");
    }

    #[test]
    fn condition_cannot_clear_global_gate() {
        // The safety boundary: no condition action can un-skip a global
        // gate (there is no run-forcing action; multipliers don't apply to
        // a skipped zone).
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(20.0), None, None, None);
        i.temp_now_f = 30.0; // freeze -> global skip
        let rule = ConditionRule {
            id: "boost".into(),
            name: String::new(),
            enabled: true,
            scope: RuleScope::AllZones,
            condition: ConditionExpr::Compare {
                metric: Metric::TempNowF,
                op: CmpOp::Lt,
                value: 100.0,
            },
            action: RuleAction::AdjustMultiplier { factor: 1.5 },
        };
        let zv = decide_per_zone(&i, &p, std::slice::from_ref(&rule));
        assert!(zv
            .iter()
            .all(|z| z.verdict == "skip" && z.source == "global"));
    }

    // ── Operator-disabled built-in rules ──

    #[test]
    fn disabled_rain_now_allows_run_while_raining() {
        let mut i = base();
        i.rain_intensity_now_in_hr = Some(0.05);
        // Sanity: default params skip on active rain.
        assert_eq!(evaluate(&i).verdict, "skip");

        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["rain_now".into()];
        let s = evaluate_with(&i, &p);
        assert_eq!(s.verdict, "run", "disabled rain_now must allow the run");

        // Trace transparency: the disabled rule is still listed, marked
        // inert, and the verdict comes from the rest of the ladder.
        let t = decide_traced(&i, &p);
        assert_eq!(t.verdict, "run");
        let r = t.rules.iter().find(|r| r.id == "rain_now").unwrap();
        assert_eq!(r.outcome, "skipped");
        assert_eq!(r.detail, "disabled by operator");
        assert!(r.verdict.is_none());
    }

    #[test]
    fn disabled_rule_still_listed_in_trace_after_decision() {
        // Even when an earlier rule already decided, a disabled rule shows
        // "disabled by operator" (not "not_reached") so the operator can
        // always see which rules they have switched off.
        let mut i = base();
        i.is_paused = true; // decides early
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["already_wet".into()];
        let t = decide_traced(&i, &p);
        assert_eq!(t.verdict, "skip");
        let r = t.rules.iter().find(|r| r.id == "already_wet").unwrap();
        assert_eq!(r.detail, "disabled by operator");
        assert_eq!(r.outcome, "skipped");
    }

    #[test]
    fn protected_paused_cannot_be_disabled() {
        let mut i = base();
        i.is_paused = true;
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["paused".into()];
        let s = evaluate_with(&i, &p);
        assert_eq!(s.verdict, "skip");
        assert_eq!(s.reason, "Paused (vacation mode)");
        // The trace shows the protected gate firing normally.
        let t = decide_traced(&i, &p);
        let r = t.rules.iter().find(|r| r.id == "paused").unwrap();
        assert_eq!(r.outcome, "fired");
    }

    #[test]
    fn restart_hold_cannot_be_disabled_or_hidden_by_an_override() {
        let mut i = base();
        i.restart_required = true;
        i.soil_zones = soil4(Some(20.0), Some(40.0), Some(40.0), Some(40.0));
        let mut p = SkipRuleParams::default();
        p.disabled_rules = builtin_rule_catalog()
            .iter()
            .map(|g| g.0.to_string())
            .collect();
        for override_mode in ["auto", "run", "skip"] {
            i.global_override = override_mode.into();
            i.zone_overrides.insert("back_yard".into(), "run".into());
            let answer = evaluate_decisions(&i, &p, &[], &CompiledScripts::compile(&[]));
            assert_eq!(answer.skip_check.reason_code, "restart_required");
            assert_eq!(
                answer.skip_check.reason,
                crate::gates_catalog::RESTART_REQUIRED_REASON
            );
            assert_eq!(answer.trace.reason_code, "restart_required");
            assert_eq!(answer.trace.rules[0].outcome, "fired");
            assert!(answer
                .zones
                .iter()
                .all(|z| z.verdict == "skip" && z.reason_code == "restart_required"));
        }
    }

    #[test]
    fn protected_control_gates_cannot_be_disabled() {
        // Listing EVERY protected id changes nothing: dry-run, the timed
        // pause, and the tomorrow override all keep deciding.
        let mut p = SkipRuleParams::default();
        p.disabled_rules = PROTECTED_RULES.iter().map(|s| s.to_string()).collect();

        let mut i = base();
        i.is_dry_run = true;
        let s = evaluate_with(&i, &p);
        assert_eq!(s.verdict, "skip");
        assert_eq!(s.reason, "All watering is on hold");

        let mut i = base();
        i.pause_until_epoch = i.now_epoch() + 3600;
        assert_eq!(evaluate_with(&i, &p).verdict, "skip");

        let mut i = base();
        i.is_tomorrow = true;
        i.override_tomorrow = "skip".to_string();
        let s = evaluate_with(&i, &p);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("Manual override"));
    }

    #[test]
    fn protected_restrictions_cannot_be_disabled() {
        use crate::config::schema::EffectiveWindow;
        let mut i = base();
        // A restriction that forbids every hour of every day.
        i.watering_restrictions = vec![WateringRestriction {
            id: "test_total_ban".into(),
            name: "Total ban".into(),
            enabled: true,
            effective: EffectiveWindow::AllYear,
            allowed_weekdays_odd: Vec::new(),
            allowed_weekdays_even: Vec::new(),
            forbidden_hour_start: Some(0),
            forbidden_hour_end: Some(24),
            max_minutes_per_zone: None,
            ..Default::default()
        }];
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["restrictions".into()];
        let s = evaluate_with(&i, &p);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("Watering restriction"));
    }

    #[test]
    fn unknown_disabled_ids_are_harmless() {
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["definitely_not_a_rule".into()];
        let mut i = base();
        i.rain_today_in = 0.10;
        // Real gates keep working; the unknown id matches nothing.
        assert_eq!(evaluate_with(&i, &p).verdict, "skip");
        assert_eq!(evaluate_with(&base(), &p).verdict, "run");
    }

    #[test]
    fn disabled_soil_saturation_disables_per_zone_gate_too() {
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), Some(25.0), None, None);
        // Sanity: under defaults the saturated zone skips on soil.
        let zv = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        let back = zv.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(back.verdict, "skip");
        assert_eq!(back.source, "soil_saturation");
        // Disabling "soil_saturation" clears BOTH the yard-wide gate and
        // the per-zone gate: same operator id, one behavior everywhere.
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["soil_saturation".into()];
        let zv = decide_per_zone(&i, &p, &[]);
        assert!(zv.iter().all(|z| z.verdict == "run"), "{zv:?}");
        // The aggregate path agrees.
        let mut i2 = base();
        i2.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        assert_eq!(evaluate_with(&i2, &p).verdict, "run");
    }

    #[test]
    fn decide_per_zone_inherits_disabled_rules() {
        // A disabled GLOBAL gate (already_wet) no longer binds the zones:
        // the per-zone path flows through the same shared helpers.
        let mut i = base();
        i.soil_zones = soil4(Some(20.0), Some(20.0), Some(20.0), Some(20.0));
        i.rain_today_in = 0.10;
        // Default: global weather skip binds all zones.
        let zv = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        assert!(zv.iter().all(|z| z.verdict == "skip"));
        // Disabled: every zone runs, matching the aggregate verdict.
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["already_wet".into()];
        let zv = decide_per_zone(&i, &p, &[]);
        assert!(zv.iter().all(|z| z.verdict == "run"), "{zv:?}");
        assert_eq!(decide(&i, &p).0, "run");
    }

    #[test]
    fn catalog_covers_every_traced_gate() {
        // The catalog must list exactly the gates the traced ladder emits,
        // in evaluation order, with protected flags agreeing with
        // PROTECTED_RULES. Pins the UI catalog to the real ladder.
        let t = decide_traced(&base(), &SkipRuleParams::default());
        let trace_ids: Vec<&str> = t.rules.iter().map(|r| r.id.as_str()).collect();
        let catalog = builtin_rule_catalog();
        let cat_ids: Vec<&str> = catalog.iter().map(|(id, _, _, _)| *id).collect();
        assert_eq!(cat_ids, trace_ids, "catalog vs traced ladder drift");

        for (id, label, desc, protected) in catalog {
            assert_eq!(
                *protected,
                PROTECTED_RULES.contains(id),
                "protected flag mismatch for {id}"
            );
            assert!(!label.is_empty(), "{id} missing label");
            assert!(!desc.is_empty(), "{id} missing description");
            assert!(!desc.contains('\u{2014}'), "em dash in {id} description");
        }
        // Every protected id is a real catalog entry (no orphans).
        for id in PROTECTED_RULES {
            assert!(cat_ids.contains(id), "protected id {id} not in catalog");
        }
    }

    // ── Sticky override (global + per-zone) ──────────────────────────────

    #[test]
    fn global_override_skip_forces_skip() {
        let mut i = base();
        i.global_override = "skip".into();
        assert_eq!(decide(&i, &SkipRuleParams::default()).0, "skip");
    }

    #[test]
    fn global_override_run_forces_run_past_rain() {
        let mut i = base();
        // Heavy rain now normally skips (matches the rain-now parity scenario).
        i.rain_intensity_now_in_hr = Some(0.05);
        assert_eq!(
            decide(&i, &SkipRuleParams::default()).0,
            "skip",
            "sanity: rain-now skips"
        );
        i.global_override = "run".into();
        assert_eq!(
            decide(&i, &SkipRuleParams::default()).0,
            "run",
            "force run overrides the rain-now skip"
        );
    }

    #[test]
    fn force_run_preserves_safety_restrictions_and_operator_holds_in_every_scope() {
        type Change = fn(&mut Inputs);
        let cases: &[(&str, Change)] = &[
            ("restart_required", |i| i.restart_required = true),
            ("freeze_now", |i| i.temp_now_f = 28.0),
            ("overnight_freeze", |i| i.temp_min_24h_f = Some(28.0)),
            ("soil_frost", |i| i.soil_temp_yard_min_f = Some(28.0)),
            ("wind_now", |i| i.wind_now_mph = 50.0),
            ("wind_forecast", |i| i.wind_max_today_mph = 50.0),
            ("live_data", |i| i.live_readings = LiveReadings::Unavailable),
            ("paused", |i| i.is_paused = true),
            ("pause_until", |i| {
                i.pause_until_epoch = i.now_epoch() + 3600
            }),
            ("dry_run", |i| i.is_dry_run = true),
            ("restrictions", |i| {
                i.watering_restrictions = vec![WateringRestriction {
                    allowed_weekdays: vec![0],
                    ..Default::default()
                }];
                i.when = crate::engine::clock::DecisionTime::at(
                    crate::engine::calendar::Calendar::utc(),
                    1_700_000_000,
                );
            }),
        ];
        let p = SkipRuleParams::default();
        for (expected, change) in cases {
            for scope in ["global", "zone", "tomorrow"] {
                let mut inputs = base();
                inputs.soil_zones = soil4(Some(40.0), Some(40.0), Some(40.0), Some(40.0));
                // A force-able earlier rain gate must not hide the later safety
                // gate, which was the failure mode of blanket override precedence.
                inputs.rain_intensity_now_in_hr = Some(1.0);
                change(&mut inputs);
                match scope {
                    "global" => inputs.global_override = "run".into(),
                    "zone" => {
                        inputs
                            .zone_overrides
                            .insert("back_yard".into(), "run".into());
                    }
                    _ => {
                        inputs.is_tomorrow = true;
                        inputs.override_tomorrow = "run".into();
                    }
                }
                let zones = decide_per_zone(&inputs, &p, &[]);
                let back = zv(&zones, "back_yard");
                assert_eq!(back.verdict, "skip", "{scope}: {expected}");
                assert_eq!(back.reason_code, *expected, "{scope}");
                if scope != "zone" {
                    let aggregate = evaluate_with(&inputs, &p);
                    let trace = decide_traced(&inputs, &p);
                    assert_eq!(aggregate.reason_code, *expected, "{scope}");
                    assert_eq!(trace.verdict, aggregate.verdict, "{scope}");
                    assert_eq!(trace.reason_code, aggregate.reason_code, "{scope}");
                    assert_eq!(trace.reason, aggregate.reason, "{scope}");
                }
            }
        }
    }

    #[test]
    fn soil_model_reruns_the_ladder_after_removing_forecast_rain() {
        let p = SkipRuleParams::default();
        let mut inputs = base();
        inputs.rain_next_4h_in = Some(1.0);
        inputs.is_dry_run = true;
        inputs.soil_zones = soil4(None, None, None, None);
        for zone in &mut inputs.soil_zones {
            zone.governed_by_soil_model = true;
        }
        let zones = decide_per_zone(&inputs, &p, &[]);
        assert!(zones
            .iter()
            .all(|zone| zone.verdict == "skip" && zone.reason_code == "dry_run"));

        // A forecast-today gate occurs before observed recent rain. Removing
        // only the first must still expose the actual measured-rain backstop.
        inputs.is_dry_run = false;
        inputs.rain_today_forecast_in = Some(1.0);
        inputs.rain_observed_recent_in = 2.0;
        let zones = decide_per_zone(&inputs, &p, &[]);
        assert!(zones
            .iter()
            .all(|zone| zone.verdict == "skip" && zone.reason_code == "observed_rain"));
    }

    #[test]
    fn custom_conditions_reach_soil_floor_and_soil_model_runs() {
        use crate::engine::conditions::{ConditionExpr, RuleAction, RuleScope};
        let rule = ConditionRule {
            id: "owner_hold".into(),
            name: "Garden party".into(),
            enabled: true,
            scope: RuleScope::AllZones,
            condition: ConditionExpr::All(vec![]),
            action: RuleAction::Skip,
        };
        for governed in [false, true] {
            let mut inputs = base();
            inputs.rain_next_4h_in = Some(1.0);
            inputs.soil_zones = soil4(Some(20.0), Some(20.0), Some(20.0), Some(20.0));
            for zone in &mut inputs.soil_zones {
                zone.governed_by_soil_model = governed;
            }
            let zones = decide_per_zone(
                &inputs,
                &SkipRuleParams::default(),
                std::slice::from_ref(&rule),
            );
            assert!(zones
                .iter()
                .all(|zone| zone.verdict == "skip" && zone.source == "condition"));
        }
    }

    #[test]
    fn force_overrode_guard_names_only_a_gate_actually_overridden() {
        // #2: a force-run watering THROUGH a hard guard surfaces the guard it is
        // suppressing (so the UI can warn), without changing override-beats-all.
        let p = SkipRuleParams::default();

        // No override: no signal.
        let i = base();
        assert_eq!(force_overrode_guard(&i, &p), None);

        // Force run on a clean day (nothing to override): no signal.
        let mut i = base();
        i.global_override = "run".into();
        assert_eq!(
            force_overrode_guard(&i, &p),
            None,
            "force run over a would-be run names no guard"
        );

        // A convenience force cannot waive a freeze. No warning may claim it did.
        let mut i = base();
        i.temp_now_f = 28.0;
        i.min_temp_f = 35.0;
        assert_eq!(
            decide(&i, &p).0,
            "skip",
            "sanity: the freeze skips without an override"
        );
        i.global_override = "run".into();
        assert_eq!(decide(&i, &p).0, "skip", "freeze still holds");
        assert_eq!(force_overrode_guard(&i, &p), None);

        // Rain/soil recommendations remain overridable and visible afterward.
        i.temp_now_f = 70.0;
        i.rain_today_in = 0.5;
        assert_eq!(decide(&i, &p).0, "run");
        assert!(force_overrode_guard(&i, &p)
            .unwrap()
            .contains("Already wet"));

        // A force-SKIP is not a force-run: no overridden-guard signal.
        let mut i = base();
        i.temp_now_f = 28.0;
        i.min_temp_f = 35.0;
        i.global_override = "skip".into();
        assert_eq!(force_overrode_guard(&i, &p), None);
    }

    #[test]
    fn zone_override_run_cannot_clear_global_skip() {
        let mut i = base();
        i.soil_zones = soil4(Some(40.0), Some(40.0), Some(40.0), Some(40.0));
        i.global_override = "skip".into();
        i.zone_overrides.insert("front_yard".into(), "run".into());
        let zv = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        let front = zv.iter().find(|z| z.zone_slug == "front_yard").unwrap();
        let back = zv.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(front.verdict, "skip", "the current yard-wide hold wins");
        assert_eq!(back.verdict, "skip", "other zones follow the global skip");
    }

    #[test]
    fn zone_override_skip_beats_global_run() {
        let mut i = base();
        i.soil_zones = soil4(Some(40.0), Some(40.0), Some(40.0), Some(40.0));
        i.global_override = "run".into();
        i.zone_overrides.insert("side_yard".into(), "skip".into());
        let zv = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        let side = zv.iter().find(|z| z.zone_slug == "side_yard").unwrap();
        let back = zv.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(side.verdict, "skip", "zone override skip beats global run");
        assert_eq!(back.verdict, "run", "other zones follow the global run");
    }

    #[test]
    fn force_run_overrides_soil_saturation_per_zone() {
        let mut i = base();
        // back_yard saturated (90% >= 70 threshold) normally skips that zone. The
        // whole yard is wet (90/80/80/80) so 90 is not a quarantine outlier
        // (|90-80| = 10 < 25) and the skip is a genuine per-zone soil_saturation.
        i.soil_zones = soil4(Some(90.0), Some(80.0), Some(80.0), Some(80.0));
        let zv0 = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        assert_eq!(
            zv0.iter()
                .find(|z| z.zone_slug == "back_yard")
                .unwrap()
                .verdict,
            "skip",
            "sanity: saturated zone skips"
        );
        i.zone_overrides.insert("back_yard".into(), "run".into());
        let zv = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        assert_eq!(
            zv.iter()
                .find(|z| z.zone_slug == "back_yard")
                .unwrap()
                .verdict,
            "run",
            "force run overrides per-zone soil saturation"
        );
    }

    #[test]
    fn auto_override_is_noop() {
        let mut i = base();
        i.soil_zones = soil4(Some(40.0), Some(40.0), Some(40.0), Some(40.0));
        let baseline = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        i.global_override = "auto".into();
        let with_auto = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        for (a, b) in baseline.iter().zip(with_auto.iter()) {
            assert_eq!(a.verdict, b.verdict, "auto override must change nothing");
        }
    }

    // ── Soil-probe quarantine and data holds ─────────────────────────

    #[test]
    fn quarantine_config_defaults() {
        // Additive params with the documented defaults: on, 35pp threshold.
        let p = SkipRuleParams::default();
        assert!(p.soil_quarantine_enabled);
        assert!((p.soil_outlier_threshold_pct - 35.0).abs() < 1e-9);
    }

    #[test]
    fn quarantine_low_outlier_holds_without_claiming_measured_saturation() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(28.0), Some(76.0), Some(71.0), Some(73.0));
        i.soil_zones[3].saturation_pct = 70.0;
        let answer = evaluate_decisions(&i, &p, &[], &CompiledScripts::compile(&[]));
        assert_eq!(answer.skip_check.verdict, "skip", "all final zones hold");
        let back = zv(&answer.zones, "back_yard");
        assert_eq!(back.verdict, "skip");
        assert_eq!(back.reason_code, "soil_probe");
        assert_eq!(back.source, "soil_quarantine");
        assert!(back.reason.contains("28% vs yard 73%"));
        assert!(back.reason.contains("watering held"));
        assert!(!back.reason.contains("saturated"));
        assert_eq!(zv(&answer.zones, "front_yard").source, "soil_saturation");
        assert_eq!(
            answer.skip_check.soil_fields["soil_back_yard_pct"],
            Some(28.0),
            "the diagnostic reading stays raw"
        );
    }

    #[test]
    fn quarantine_offline_probe_holds_with_saturated_siblings() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(None, Some(80.0), Some(78.0), Some(82.0));
        i.soil_zones[0].probe_configured = true;
        i.soil_zones[3].saturation_pct = 70.0;
        let answer = evaluate_decisions(&i, &p, &[], &CompiledScripts::compile(&[]));
        assert_eq!(answer.skip_check.verdict, "skip");
        let back = zv(&answer.zones, "back_yard");
        assert_eq!(back.verdict, "skip");
        assert_eq!(back.reason_code, "soil_probe");
        assert_eq!(back.source, "soil_quarantine");
        assert!(back.reason.contains("offline") && back.reason.contains("watering held"));
    }

    #[test]
    fn quarantine_normal_zone_within_threshold_unchanged() {
        // (c) A zone within the outlier threshold is NOT quarantined: a uniform,
        // mildly-varying dry yard runs exactly as before, every source "global".
        let p = SkipRuleParams::default();
        let mut i = base();
        // 40/45/42/38: spread well under 25pp, none an outlier.
        i.soil_zones = soil4(Some(40.0), Some(45.0), Some(42.0), Some(38.0));
        assert_eq!(decide(&i, &p).0, "run");
        let v = decide_per_zone(&i, &p, &[]);
        for z in &v {
            assert_eq!(z.verdict, "run", "{} should run", z.zone_slug);
            assert_ne!(
                z.source, "soil_quarantine",
                "{} must not be quarantined",
                z.zone_slug
            );
        }
        // None quarantined: the plan is all-None.
        assert!(quarantine_plan(&i.soil_zones, &p)
            .iter()
            .all(Option::is_none));
    }

    #[test]
    fn quarantine_genuinely_dry_trustworthy_yard_still_runs_via_floor() {
        // (d) THE load-bearing safety case: a genuinely dry, TRUSTWORTHY yard (all
        // zones ~low, none an outlier) on a soft forecast-rain morning must STILL
        // run via the soil_floor moat. Quarantine must not falsely distrust any
        // probe (no outliers, soil_floor intact).
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_next_4h_in = Some(0.50); // demotable soft forecast-rain skip
                                        // All four genuinely dry, tight spread (20/22/19/21): no outlier.
        i.soil_zones = soil4(Some(20.0), Some(22.0), Some(19.0), Some(21.0));
        // No zone is quarantined.
        assert!(
            quarantine_plan(&i.soil_zones, &p)
                .iter()
                .all(Option::is_none),
            "a tight dry yard must not be quarantined"
        );
        // Aggregate demotes to run (the moat), and every dry zone runs via soil_floor.
        assert_eq!(decide(&i, &p).0, "run");
        let v = decide_per_zone(&i, &p, &[]);
        for z in &v {
            assert_eq!(
                z.verdict, "run",
                "{} dry zone must run via the floor",
                z.zone_slug
            );
            assert_eq!(z.source, "soil_floor", "{} ran via the moat", z.zone_slug);
        }
    }

    #[test]
    fn quarantine_offline_probe_cannot_borrow_dry_permission_from_siblings() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_next_4h_in = Some(0.50);
        i.soil_zones = soil4(None, Some(20.0), Some(22.0), Some(21.0));
        i.soil_zones[0].probe_configured = true;
        let v = decide_per_zone(&i, &p, &[]);
        let back = zv(&v, "back_yard");
        assert_eq!(back.verdict, "skip");
        assert_eq!(back.reason_code, "soil_probe");
        assert_eq!(zv(&v, "front_yard").verdict, "run");
        assert_eq!(zv(&v, "front_yard").source, "soil_floor");
    }

    #[test]
    fn configured_offline_probe_holds_even_without_sibling_evidence() {
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), Some(82.0), None, None);
        i.soil_zones
            .iter_mut()
            .for_each(|z| z.probe_configured = true);
        let p = SkipRuleParams::default();
        let plan = quarantine_plan(&i.soil_zones, &p);
        assert!(
            plan[0].is_none() && plan[1].is_none(),
            "two readings cannot establish outliers"
        );
        let v = decide_per_zone(&i, &p, &[]);
        assert_eq!(zv(&v, "side_yard").reason_code, "soil_probe");

        i.soil_zones.iter_mut().for_each(|z| z.pct = None);
        for quarantine_enabled in [true, false] {
            let mut p = p.clone();
            p.soil_quarantine_enabled = quarantine_enabled;
            p.disabled_rules.push("soil_probe".into());
            let answer = evaluate_decisions(&i, &p, &[], &CompiledScripts::compile(&[]));
            assert_eq!(answer.skip_check.reason_code, "soil_probe");
            assert_eq!(answer.trace.reason_code, "soil_probe");
            assert!(answer
                .zones
                .iter()
                .all(|z| z.verdict == "skip" && z.reason_code == "soil_probe"));
        }
    }

    #[test]
    fn quarantine_low_outlier_needs_three_present() {
        // The outlier rule needs >= 3 present readings. With exactly 3 present, a
        // 28% reading next to {76,71} (median 71, |28-71|=43 > 25) IS an outlier;
        // with the same two siblings but only 2 present it is not judged at all.
        let p = SkipRuleParams::default();
        // 3 present: back_yard 28 is an outlier.
        let mut i3 = base();
        i3.soil_zones = vec![
            ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: Some(28.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: Some(76.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "side_yard".into(),
                name: "side yard".into(),
                pct: Some(71.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
        ];
        let plan3 = quarantine_plan(&i3.soil_zones, &p);
        assert!(
            plan3[0].is_some(),
            "28 vs {{76,71}} is an outlier with 3 present"
        );

        // 2 present: not judged.
        let mut i2 = base();
        i2.soil_zones = vec![
            ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: Some(28.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: Some(76.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
                probe_configured: false,
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
                sprinkler_type: Default::default(),
            },
        ];
        assert!(
            quarantine_plan(&i2.soil_zones, &p)
                .iter()
                .all(Option::is_none),
            "outliers are not judged with fewer than 3 present readings"
        );
    }

    #[test]
    fn quarantine_disabled_is_exact_current_behavior() {
        // (f) With quarantine disabled, the incident reproduces the OLD bug: the
        // bad 28% probe is trusted, so back_yard runs while its saturated siblings
        // skip. This pins "disabled == pre-quarantine behavior".
        let mut p = SkipRuleParams::default();
        p.soil_quarantine_enabled = false;
        let mut i = base();
        i.soil_zones = soil4(Some(28.0), Some(76.0), Some(71.0), Some(73.0));
        i.soil_zones[3].saturation_pct = 70.0;
        // Plan is all-None when disabled.
        assert!(quarantine_plan(&i.soil_zones, &p)
            .iter()
            .all(Option::is_none));
        // Aggregate does NOT skip (one effective-dry zone keeps the yard gate open).
        assert_eq!(decide(&i, &p).0, "run");
        let v = decide_per_zone(&i, &p, &[]);
        assert_eq!(
            zv(&v, "back_yard").verdict,
            "run",
            "disabled -> trusts the bad probe"
        );
        assert_ne!(zv(&v, "back_yard").source, "soil_quarantine");
        assert_eq!(zv(&v, "front_yard").verdict, "skip");
        assert_eq!(zv(&v, "front_yard").source, "soil_saturation");
    }

    #[test]
    fn quarantine_high_outlier_never_becomes_inferred_dry_permission() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(95.0), Some(30.0), Some(28.0), Some(32.0));
        assert!(quarantine_plan(&i.soil_zones, &p)[0].is_some());
        for mode in ["auto", "global_force", "zone_force", "soil_model"] {
            let mut i = i.clone();
            match mode {
                "global_force" => i.global_override = "run".into(),
                "zone_force" => {
                    i.zone_overrides.insert("back_yard".into(), "run".into());
                }
                "soil_model" => i.soil_zones[0].governed_by_soil_model = true,
                _ => {}
            }
            let v = decide_per_zone(&i, &p, &[]);
            let back = zv(&v, "back_yard");
            assert_eq!(back.verdict, "skip", "{mode}");
            assert_eq!(back.reason_code, "soil_probe", "{mode}");
            assert_eq!(
                zv(&v, "front_yard").verdict,
                "run",
                "a trusted sibling remains eligible"
            );
        }
    }

    #[test]
    fn quarantine_keeps_decide_decide_traced_parity() {
        // decide / decide_traced must agree on quarantine mornings too (same eff
        // soil reaches both). Exercise the outlier-skip and offline-infer cases.
        let p = SkipRuleParams::default();
        let mut outlier = base();
        outlier.soil_zones = soil4(Some(28.0), Some(76.0), Some(71.0), Some(73.0));
        outlier.soil_zones[3].saturation_pct = 70.0;
        let mut offline = base();
        offline.soil_zones = soil4(None, Some(80.0), Some(78.0), Some(82.0));
        offline.soil_zones[0].probe_configured = true;
        offline.soil_zones[3].saturation_pct = 70.0;
        for i in [&outlier, &offline] {
            let (v, r) = decide(i, &p);
            let t = decide_traced(i, &p);
            assert_eq!(t.verdict, v, "quarantine verdict parity");
            assert_eq!(t.reason, r, "quarantine reason parity");
            let fired = t.rules.iter().filter(|e| e.outcome == "fired").count();
            assert!(fired <= 1, "at most one fired rule on a quarantine morning");
        }
    }
    #[test]
    fn unbound_zones_never_acquire_probe_faults_or_inferred_dry_readings() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(None, Some(20.0), Some(22.0), Some(21.0));
        assert!(quarantine_plan(&i.soil_zones, &p)[0].is_none());
        assert!(probe_data_holds(&i, &p).is_empty());
        assert_eq!(
            zv(&decide_per_zone(&i, &p, &[]), "back_yard").verdict,
            "run"
        );
        i.rain_next_4h_in = Some(0.5);
        let zones = decide_per_zone(&i, &p, &[]);
        assert_eq!(
            zv(&zones, "back_yard").reason_code,
            "rain_next_4h",
            "an unprobed zone cannot borrow a sibling's dry-soil floor"
        );
        i.soil_zones[0].governed_by_soil_model = true;
        let zones = decide_per_zone(&i, &p, &[]);
        assert_eq!(
            zv(&zones, "back_yard").verdict,
            "run",
            "an intentionally unprobed soil model still governs its zone"
        );
    }

    #[test]
    fn probe_fault_stays_binding_under_force_and_trace_matches() {
        let mut i = base();
        i.soil_zones = vec![ZoneSoil {
            slug: "front".into(),
            probe_configured: true,
            governed_by_soil_model: true,
            planning_forecast_unavailable: false,
            ..Default::default()
        }];
        let mut p = SkipRuleParams::default();
        p.disabled_rules = PROTECTED_RULES.iter().map(|id| (*id).to_string()).collect();
        for force in ["none", "global", "zone", "tomorrow"] {
            let mut i = i.clone();
            match force {
                "global" => i.global_override = "run".into(),
                "zone" => {
                    i.zone_overrides.insert("front".into(), "run".into());
                }
                "tomorrow" => {
                    i.is_tomorrow = true;
                    i.override_tomorrow = "run".into();
                }
                _ => {}
            }
            let answer = evaluate_decisions(&i, &p, &[], &CompiledScripts::compile(&[]));
            assert_eq!(answer.skip_check.reason_code, "soil_probe", "{force}");
            assert_eq!(answer.trace.reason_code, "soil_probe", "{force}");
            assert_eq!(answer.zones[0].reason_code, "soil_probe", "{force}");
            assert_eq!(answer.zones[0].verdict, "skip", "{force}");
            assert_eq!(
                answer
                    .trace
                    .rules
                    .iter()
                    .filter(|r| r.outcome == "fired")
                    .count(),
                1
            );
        }
    }

    #[test]
    fn probe_holds_round_trip_as_typed_metadata_even_when_weather_wins() {
        let mut i = base();
        i.temp_now_f = 25.0;
        i.soil_zones = soil4(None, Some(95.0), Some(28.0), Some(30.0));
        i.soil_zones[0].probe_configured = true;
        let p = SkipRuleParams::default();
        let sc = evaluate_with(&i, &p);
        assert_eq!(sc.reason_code, "freeze_now");
        assert_eq!(
            sc.soil_probe_holds.len(),
            2,
            "the earlier freeze cannot hide offline or outlier data"
        );
        let json = serde_json::to_value(&sc).unwrap();
        assert_eq!(json["soil_probe_configured"]["back_yard"], true);
        assert!(json["soil_back_yard_pct"].is_null());
        assert!(json["soil_probe_holds"]["front_yard"]
            .as_str()
            .unwrap()
            .contains("95%"));
        let recovered: SkipCheck = serde_json::from_value(json.clone()).unwrap();
        let inputs = inputs_from_skipcheck(&recovered);
        assert!(
            inputs
                .soil_zones
                .iter()
                .find(|z| z.slug == "back_yard")
                .unwrap()
                .probe_configured
        );
        assert_eq!(probe_data_holds(&inputs, &p), sc.soil_probe_holds);
        let mut legacy = json;
        legacy
            .as_object_mut()
            .unwrap()
            .remove("soil_probe_configured");
        legacy.as_object_mut().unwrap().remove("soil_probe_holds");
        let recovered: SkipCheck = serde_json::from_value(legacy).unwrap();
        assert!(recovered.soil_probe_configured.is_empty());
        assert!(recovered.soil_probe_holds.is_empty());
    }
    #[test]
    fn soil_model_cannot_claim_to_have_counted_missing_forecast_rain() {
        for gate in SOIL_MODEL_INERT_GATES {
            let mut input = base();
            for zone in &mut input.soil_zones {
                zone.governed_by_soil_model = true;
                zone.probe_configured = false;
                zone.pct = None;
            }
            match *gate {
                "rain_today_forecast" => input.rain_today_forecast_in = None,
                "rain_next_4h" => input.rain_next_4h_in = None,
                "tomorrow_rain" => input.forecast_in = None,
                "rain_3day" => input.rain_3day_weighted_in = None,
                _ => unreachable!(),
            }
            let answer = evaluate_decisions(
                &input,
                &SkipRuleParams::default(),
                &[],
                &CompiledScripts::compile(&[]),
            );
            assert!(
                answer
                    .zones
                    .iter()
                    .all(|zone| zone.verdict == "skip" && zone.reason_code == *gate),
                "{gate}: {:?}",
                answer.zones
            );
            let trace = answer
                .trace
                .rules
                .iter()
                .find(|rule| rule.id == *gate)
                .unwrap();
            assert_eq!(trace.outcome, "fired", "{gate}");
            assert!(trace.overridden_by.is_none(), "{gate}");
        }
    }
}
