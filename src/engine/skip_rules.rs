// LocalSky's irrigation recommendation engine. Single source of truth
// for the morning skip decision: the dashboard renders the verdict from
// here, and HA's automation reads the same verdict via REST sensor and
// acts on it.
//
// Phase 3D extraction: this is the former src/ha/skip_logic.rs moved
// under engine/ with hardcoded constants pulled out into SkipRuleParams
// (sourced from config.engine.skip_rules at runtime). Defaults match
// the previous const values so existing call sites pass without changes.
// src/ha/skip_logic.rs is now a thin re-export shim for back-compat.

use std::collections::HashSet;

use chrono::{Local, TimeZone};

use crate::config::schema::{AddressParity, SkipRuleParams, WateringRestriction};
use crate::engine::conditions::{apply_zone_rules, ConditionCtx, ConditionRule};
use crate::engine::restrictions;
use crate::ha::snapshot::{DecisionTrace, RainNature, RuleEval, SkipCheck, ZoneVerdict};

/// Inputs the engine needs. Caller fills these from HA states +
/// ForecastSnapshot helpers + TempestStore.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    // ── Live readings ──
    pub temp_now_f: f64,
    pub wind_now_mph: f64,
    pub rain_today_in: f64,
    pub rain_intensity_now_in_hr: f64,
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
    pub forecast_in: f64,
    pub rain_tomorrow_prob_pct: u32,
    pub rain_3day_weighted_in: f64,
    pub rain_7day_weighted_in: f64,
    pub rain_next_4h_in: f64,
    /// OBSERVED rain over the recent window: today's measured total plus the
    /// last `rain_observed_window_days` of past observed daily rain. Drives the
    /// sensor-independent observed-rain SKIP backstop (a hard skip that binds
    /// every zone and beats the per-zone soil_floor override, so heavy past rain
    /// is honored even when a soil probe is bad/offline). Computed in the
    /// refresher from the live `rain_today_in` + `past_n_day_precip_in(window)`.
    pub rain_observed_recent_in: f64,
    pub wind_max_today_mph: f64,
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
    pub is_paused: bool,
    pub is_dry_run: bool,

    // ── Phase 4 control surfaces ──
    pub pause_until_epoch: i64,
    pub now_epoch: i64,
    pub override_tomorrow: String,
    pub is_tomorrow: bool,
    /// Sticky global override: "auto" | "skip" | "run". Beats the engine
    /// verdict (a per-zone override in turn beats this). "run" forces watering
    /// past the skip conditions. "" / "auto" = follow the engine.
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
}

/// Health/provenance of the live "now" readings feeding the engine.
/// Default `Station` preserves the historical behavior for every caller
/// that doesn't track provenance (simulator, verdict strip, tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LiveReadings {
    /// Fresh local station data (Tempest packet within the staleness window).
    #[default]
    Station,
    /// Station stale or absent; current-hour forecast values standing in.
    ForecastFallback,
    /// No station data and no forecast. Fail safe: skip, don't fabricate.
    Unavailable,
}

/// One zone's live soil reading + its per-zone thresholds, sourced from
/// `ZoneConfig` (saturation/target) and the assigned sensor. `pct: None`
/// means the probe is offline or no sensor is assigned.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneSoil {
    pub slug: String,
    pub name: String,
    pub pct: Option<f64>,
    pub saturation_pct: f64,
    pub target_min_pct: f64,
}

// ─────────────────────────────────────────────────────────────────────
// Soil-probe QUARANTINE + infer-from-siblings (2026-06 incident: a probe
// physically in a bad spot read 28% while siblings read 71-76% after the
// same rain, so the per-zone saturation gate trusted 28% and the zone ran
// while saturated). A zone whose probe is OFFLINE (None) or a WILD OUTLIER
// versus its siblings is DISTRUSTED; its effective soil for the soil gates
// (saturation + soil_floor) is INFERRED from the trustworthy sibling median,
// so a quarantined zone with saturated neighbors reads saturated and skips.
//
// Scope boundary: ONLY the soil gates ever see the inferred value. Global /
// weather / safety gates, the observed-recent-rain backstop, the per-zone
// condition rules, and the serialized raw `soil_<slug>_pct` fields all keep
// the raw reading. Quarantine never forces a run: a TRUSTWORTHY genuinely-dry
// zone is left untouched (the soil_floor moat stays intact). Disabled
// (`soil_quarantine_enabled = false`) restores the exact pre-quarantine path.
// ─────────────────────────────────────────────────────────────────────

/// One zone's quarantine outcome, produced by `quarantine_plan` and parallel
/// to `Inputs::soil_zones`. `Some` means the zone's probe was distrusted AND a
/// trustworthy sibling median existed, so its soil was inferred; `None` means
/// the zone is trusted as-is (or there was no trustworthy median to infer from,
/// in which case the raw reading stands and nothing is surfaced).
#[derive(Debug, Clone, Copy, PartialEq)]
struct ZoneQuarantine {
    /// The raw reading the probe reported. `None` when the probe was offline.
    raw_pct: Option<f64>,
    /// The substituted effective soil (the trustworthy sibling median).
    inferred_pct: f64,
    /// Median of the trustworthy siblings, for the surfaced reason. Equal to
    /// `inferred_pct`; kept separate for readability at the call sites.
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
///   * OFFLINE (None) probes are always UNTRUSTWORTHY.
///   * A PRESENT reading is UNTRUSTWORTHY only when >= 3 zones report AND it
///     deviates from the median of all present readings by more than
///     `soil_outlier_threshold_pct` (catches a wildly-low bad-spot probe and a
///     wildly-high one alike).
/// The TRUSTWORTHY MEDIAN is the median of readings that are present AND not
/// outliers. Every untrustworthy zone gets that median substituted as its
/// effective soil. When no trustworthy median exists (all offline/outliers, or
/// fewer than 3 present so outliers can't be judged and only offline zones are
/// distrusted with no present non-outlier siblings), the entry is `None` and the
/// raw reading stands (fallback to current behavior).
///
/// `enabled = false` returns an all-`None` plan (exact pre-quarantine behavior).
fn quarantine_plan(zones: &[ZoneSoil], p: &SkipRuleParams) -> Vec<Option<ZoneQuarantine>> {
    let none_plan = || vec![None; zones.len()];
    if !p.soil_quarantine_enabled || zones.is_empty() {
        return none_plan();
    }
    let present: Vec<f64> = zones.iter().filter_map(|z| z.pct).collect();
    // Outliers can only be judged with >= 3 present readings; with fewer, no
    // PRESENT reading is ever distrusted (offline zones still may be inferred).
    let can_judge_outliers = present.len() >= 3;
    let present_median = if present.is_empty() {
        0.0
    } else {
        median(&present)
    };
    let is_outlier = |pct: f64| {
        can_judge_outliers && (pct - present_median).abs() > p.soil_outlier_threshold_pct
    };
    // Trustworthy = present AND not an outlier. Its median is the inferred value.
    let trustworthy: Vec<f64> = zones
        .iter()
        .filter_map(|z| z.pct)
        .filter(|&pct| !is_outlier(pct))
        .collect();
    if trustworthy.is_empty() {
        // No trustworthy median to infer from: fall back to raw readings.
        return none_plan();
    }
    let trust_median = median(&trustworthy);
    zones
        .iter()
        .map(|z| {
            let untrustworthy = match z.pct {
                None => true,                 // offline
                Some(pct) => is_outlier(pct), // wild outlier vs siblings
            };
            untrustworthy.then_some(ZoneQuarantine {
                raw_pct: z.pct,
                inferred_pct: trust_median,
                sibling_median: trust_median,
            })
        })
        .collect()
}

/// The EFFECTIVE soil zones for the soil gates: each quarantined zone's `pct`
/// replaced by its inferred sibling median, every other zone unchanged. Shared
/// by `decide`, `global_verdict` (and thus `decide_per_zone`), and
/// `decide_traced` so all soil-gate paths judge identical effective soil.
fn effective_soil_zones(zones: &[ZoneSoil], plan: &[Option<ZoneQuarantine>]) -> Vec<ZoneSoil> {
    zones
        .iter()
        .zip(plan)
        .map(|(z, q)| match q {
            Some(qi) => ZoneSoil {
                pct: Some(qi.inferred_pct),
                ..z.clone()
            },
            None => z.clone(),
        })
        .collect()
}

/// An `Inputs` clone whose `soil_zones` carry the quarantine-inferred effective
/// soil. ONLY the soil gates read `soil_zones.pct`, so substituting here scopes
/// the inference to saturation + soil_floor without touching any other gate.
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

/// Surfaced reason for a quarantined zone whose soil verdict was decided on the
/// inferred value, e.g. "Soil probe suspect (28% vs yard 73%); inferred from
/// neighbors". The offline case names "(offline; ...)".
fn quarantine_reason(q: &ZoneQuarantine, decided: &str) -> String {
    let probe = match q.raw_pct {
        Some(raw) => format!("{raw:.0}%"),
        None => "offline".to_string(),
    };
    format!(
        "Soil probe suspect ({} vs yard {:.0}%); inferred from neighbors -> {}",
        probe, q.sibling_median, decided
    )
}

// Bridge the generalized per-zone `soil_zones` Vec to/from the flattened
// `SkipCheck.soil_fields` map ("soil_<slug>_pct" + "saturation_<slug>_pct" +
// "target_<slug>_pct" for EVERY zone), which the manifest's per-zone soil
// descriptor reads. Generalizes the old fixed four-yard-slug fields to any
// number of zones with any slug. P1-2: target_min_pct (the per-zone soil floor)
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
fn rebuild_soil_zones(s: &crate::ha::snapshot::SkipCheck) -> Vec<ZoneSoil> {
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
                .unwrap_or(70.0);
            // P1-2: recover the per-zone floor; 30.0 only when absent (an older
            // serialized SkipCheck or demo fixture written before target_*).
            let target_min_pct = s
                .soil_fields
                .get(&format!("target_{slug}_pct"))
                .copied()
                .flatten()
                .unwrap_or(30.0);
            ZoneSoil {
                name: slug.replace('_', " "),
                slug: slug.to_string(),
                pct,
                saturation_pct,
                target_min_pct,
            }
        })
        .collect()
}

fn format_pause_until(epoch: i64) -> String {
    // #3 (TZ correctness) + #8 (24h rule): render the vacation-pause "until"
    // timestamp in the deployment's CONFIGURED timezone (not chrono::Local, the
    // container TZ) and in 24-hour local time. The configured-TZ offset comes
    // from timeutil (process-wide, set at boot from cfg.deployment.timezone);
    // applied to the epoch it yields the operator's wall clock. %H:%M is 24-hour
    // (was %-I %p, 12-hour).
    let tz_offset = *crate::timeutil::now_local().offset();
    match chrono::DateTime::from_timestamp(epoch, 0).map(|dt| dt.with_timezone(&tz_offset)) {
        Some(dt) => dt.format("%a %b %-d, %H:%M").to_string(),
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

/// ET multiplier from heat index. 1.00 at HI ≤ 85, scaling linearly to
/// 1.30 at HI 105 °F. Capped at +30%.
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
    "override",
    "pause_until",
    "paused",
    "restrictions",
    "dry_run",
    // P1-8a: the live-data fail-safe (skip when no station AND no forecast) must
    // not be operator-disableable, or disabling it reintroduces deciding on
    // fabricated values. Hard-enforced like dry_run.
    "live_data",
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
        rain_intensity_now_in_hr: i.rain_intensity_now_in_hr,
        humidity_now_pct: i.humidity_now_pct,

        forecast_in: i.forecast_in,
        rain_tomorrow_prob_pct: i.rain_tomorrow_prob_pct,
        rain_3day_weighted_in: i.rain_3day_weighted_in,
        rain_7day_weighted_in: i.rain_7day_weighted_in,
        rain_next_4h_in: i.rain_next_4h_in,
        rain_observed_recent_in: i.rain_observed_recent_in,
        wind_max_today_mph: i.wind_max_today_mph,
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

        // Generalized per-zone soil: one "soil_<slug>_pct" + "saturation_
        // <slug>_pct" entry per configured zone (any slug, any count). The
        // manifest's per-zone soil descriptor reads these.
        soil_fields: build_soil_fields(&i.soil_zones),
        soil_temp_yard_min_f: i.soil_temp_yard_min_f,
        soil_temp_yard_max_f: i.soil_temp_yard_max_f,
        frost_skip_soil_f: i.frost_skip_soil_f,

        is_paused: i.is_paused,
        is_dry_run: i.is_dry_run,

        will_skip: verdict == "skip",
        verdict: verdict.to_string(),
        reason,
        // P1 (units architecture): the firing rule's stable id, additive +
        // invisible. "run" on a clean run; mirrors the verdict/reason above.
        reason_code: reason_code.to_string(),
    }
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
    // pre_soil never reads soil_zones.pct, so the raw inputs are correct here.
    if let Some(v) = pre_soil(i, p, &disabled) {
        return v;
    }
    // From the soil gates onward, judge the quarantine-inferred effective soil so
    // a bad/offline probe inherits its trustworthy siblings' reading.
    let eff = with_effective_soil(i, p);
    if let Some(v) = soil_saturation(&eff, &disabled) {
        return v;
    }
    // Soil-floor (the moat): a soft forecast-rain skip is demoted to a run when a
    // measured-dry zone needs water. `will_skip` then becomes false, bypassing the
    // dispatcher's blanket-skip early-return; the per-zone layer (decide_per_zone)
    // runs the dry zones and skips the wet ones. dry_run / hard skips are never
    // demotable (soil_floor_demotes is false for them). Code is "soil_floor" (the
    // moat rung), matching the soil_floor gate that fires in decide_traced.
    if soil_floor_demotes(&eff, p, &disabled) {
        return ("run", String::new(), "soil_floor");
    }
    post_soil(&eff, p, &disabled, false)
}

/// The global verdict EXCLUDING the per-zone soil-saturation gate. Used by
/// `decide_per_zone` as the yard-wide baseline that binds every zone;
/// each zone then layers its own soil + custom-condition gates on top.
fn global_verdict(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
) -> (&'static str, String, &'static str) {
    // floor_active = false: decide_per_zone needs the RAW soft-rain verdict so it
    // can tell a healthy-dry zone (which it RUNS) from a wet sibling (which still
    // SKIPs) on a demotion morning. The aggregate decide() handles the floor. The
    // third element is the firing global rule id (P1), which decide_per_zone
    // carries into each zone's reason_code when the global gate binds it.
    pre_soil(i, p, disabled).unwrap_or_else(|| post_soil(i, p, disabled, false))
}

/// Per-zone verdicts. The global gates (safety + weather) bind every zone
/// identically; then each zone layers its own soil-saturation gate and the
/// user's custom condition rules (augment-only). Safety boundary: this can
/// only ADD a skip, extend, or shrink a zone's run, never clear a global
/// gate or force a run. Returns one verdict per entry in `i.soil_zones`.
///
/// Note vs the aggregate `decide()`: there, yard-wide soil saturation is
/// ordered before the rain-forecast gates; here the global (weather)
/// verdict is computed first and binds all zones, then per-zone soil runs.
/// So a uniform setup yields the same per-zone VERDICT as `decide()`'s
/// aggregate (pinned by `decide_per_zone_matches_decide_when_uniform`),
/// though the skip REASON may name weather where the aggregate named soil.
pub fn decide_per_zone(
    i: &Inputs,
    p: &SkipRuleParams,
    rules: &[ConditionRule],
) -> Vec<ZoneVerdict> {
    let disabled = disabled_set(p);
    let (gverdict, greason, gcode) = global_verdict(i, p, &disabled);
    // Soil-probe quarantine plan (parallel to i.soil_zones) + the effective-soil
    // inputs the soil gates judge. A distrusted (offline / outlier) probe inherits
    // its trustworthy siblings' median for the saturation + soil_floor gates only.
    let plan = quarantine_plan(&i.soil_zones, p);
    let eff_i = with_effective_soil(i, p);
    // Soil-floor demotion baseline (the moat), computed on the EFFECTIVE soil so a
    // quarantined zone inferred-saturated cannot demote a rain skip. `demotes` is
    // yard-wide (some zone healthy-dry + a demotable soft-rain skip + rain-removed
    // leaves a run); `soft_id` names the overridden rule for provenance. Each zone
    // then applies its OWN `zone_healthy_dry` test, so on the same morning a dry
    // zone runs and a wet sibling skips.
    let demotes = soil_floor_demotes(&eff_i, p, &disabled);
    let soft_id = if demotes {
        demotable_soft_skip_id(&eff_i, p, &disabled)
    } else {
        None
    };
    i.soil_zones
        .iter()
        .zip(eff_i.soil_zones.iter())
        .zip(plan.iter())
        .map(|((z, eff_z), quarantine)| {
            // Sticky override beats the engine entirely for this zone. A
            // zone-specific override wins over the global one; "run" forces
            // the zone past every skip gate (incl. its own soil saturation),
            // "skip" force-skips it. (The global override also already shaped
            // gverdict via pre_soil; this per-zone pass is what lets a single
            // zone diverge from the global decision + override soil.)
            let (eff, scope) = match i.zone_overrides.get(&z.slug).map(String::as_str) {
                Some("skip") => ("skip", "this zone"),
                Some("run") => ("run", "this zone"),
                _ => match i.global_override.as_str() {
                    "skip" => ("skip", "global"),
                    "run" => ("run", "global"),
                    _ => ("auto", ""),
                },
            };
            match eff {
                "skip" => {
                    return ZoneVerdict {
                        zone_slug: z.slug.clone(),
                        zone_name: z.name.clone(),
                        verdict: "skip".into(),
                        reason: format!("Override: skip ({scope})"),
                        source: "override".into(),
                        multiplier: 1.0,
                        reason_code: "override".into(),
                        value: None,
                        threshold: None,
                    }
                }
                "run" => {
                    return ZoneVerdict {
                        zone_slug: z.slug.clone(),
                        zone_name: z.name.clone(),
                        verdict: "run".into(),
                        reason: format!("Override: force run ({scope})"),
                        source: "override".into(),
                        multiplier: 1.0,
                        reason_code: "override".into(),
                        value: None,
                        threshold: None,
                    }
                }
                _ => {}
            }
            // Global safety/weather gate binds every zone, UNLESS the skip is a
            // soft forecast-rain skip AND this zone is measured healthy-dry: then
            // the zone RUNS (the moat). Hard / dry_run / saturation skips are not
            // demotable (`demotes` is false), so they bind unchanged.
            if gverdict == "skip" {
                if demotes {
                    // The dry-floor veto judges the EFFECTIVE soil: a quarantined
                    // zone inferred-saturated is mechanically not healthy-dry, so it
                    // cannot run on a stale low reading; a trusted genuinely-dry zone
                    // still runs (moat intact).
                    if let Some(pct) = zone_healthy_dry(eff_z) {
                        let sid = soft_id.unwrap_or("rain_next_4h");
                        return ZoneVerdict {
                            zone_slug: z.slug.clone(),
                            zone_name: z.name.clone(),
                            verdict: "run".into(),
                            reason: format!(
                                "Soil {:.0}% < {:.0}% minimum; {} skip overridden",
                                pct,
                                z.target_min_pct,
                                soft_rain_label(sid)
                            ),
                            source: "soil_floor".into(),
                            multiplier: 1.0,
                            // P1: the dry-floor moat decided this zone. Soil
                            // operands: measured % vs the zone's dry floor.
                            reason_code: "soil_floor".into(),
                            value: Some(pct),
                            threshold: Some(z.target_min_pct),
                        };
                    }
                }
                return ZoneVerdict {
                    zone_slug: z.slug.clone(),
                    zone_name: z.name.clone(),
                    verdict: "skip".into(),
                    reason: greason.clone(),
                    source: "global".into(),
                    multiplier: 1.0,
                    // P1: a global gate bound this zone; carry its firing id so a
                    // later client renders the same reason the yard-wide decision
                    // used. No soil operands (the deciding gate is non-soil).
                    reason_code: gcode.into(),
                    value: None,
                    threshold: None,
                };
            }
            // Global verdict is run / run_extended. Per-zone soil saturation
            // can still skip this individual zone, judged on the EFFECTIVE soil so
            // a quarantined probe inherits its trustworthy siblings' reading.
            // Honors the same operator disable id as the yard-wide gate: disabling
            // "soil_saturation" disables soil-saturation skips everywhere.
            if let Some(pct) = eff_z.pct.filter(|_| !disabled.contains("soil_saturation")) {
                if pct >= eff_z.saturation_pct {
                    // Quarantined-and-inferred zones surface the suspect-probe
                    // provenance + source so the UI and an alerting layer can see
                    // the verdict rode the neighbors, not this zone's own probe.
                    // reason_code mirrors `source` here: a quarantined-and-inferred
                    // zone's saturation decision rode the neighbors, so its code is
                    // "soil_quarantine"; an own-probe saturation skip is
                    // "soil_saturation". Soil operands: effective % vs saturation %.
                    let (reason, source) = match quarantine {
                        Some(q) => (
                            quarantine_reason(
                                q,
                                &format!(
                                    "saturated ({:.0}% \u{2265} {:.0}% threshold)",
                                    pct, eff_z.saturation_pct
                                ),
                            ),
                            "soil_quarantine",
                        ),
                        None => (
                            format!(
                                "Soil saturated ({:.0}% \u{2265} {:.0}% threshold)",
                                pct, eff_z.saturation_pct
                            ),
                            "soil_saturation",
                        ),
                    };
                    return ZoneVerdict {
                        zone_slug: z.slug.clone(),
                        zone_name: z.name.clone(),
                        verdict: "skip".into(),
                        reason,
                        source: source.into(),
                        multiplier: 1.0,
                        reason_code: source.into(),
                        value: Some(pct),
                        threshold: Some(eff_z.saturation_pct),
                    };
                }
            }
            // User condition rules (augment-only).
            let ctx = ConditionCtx { i, zone: z };
            let outcome = apply_zone_rules(rules, &ctx);
            if let Some((_, reason)) = outcome.skip {
                return ZoneVerdict {
                    zone_slug: z.slug.clone(),
                    zone_name: z.name.clone(),
                    verdict: "skip".into(),
                    reason,
                    source: "condition".into(),
                    multiplier: 1.0,
                    // P1: a user custom-condition rule skipped this zone. No
                    // canonical engine operands (the rule's metric is user-defined).
                    reason_code: "condition".into(),
                    value: None,
                    threshold: None,
                };
            }
            let verdict = if gverdict == "run_extended" || outcome.extend {
                "run_extended"
            } else {
                "run"
            };
            let touched = outcome.extend || (outcome.multiplier - 1.0).abs() > 1e-9;
            // P1: a clean run carries the global firing id ("run" when nothing
            // fired, "heat_advisory" on a run_extended); a custom rule that only
            // extended/adjusted (no skip) is "condition". Mirrors `source`/verdict.
            let reason_code = if touched { "condition" } else { gcode };
            ZoneVerdict {
                zone_slug: z.slug.clone(),
                zone_name: z.name.clone(),
                verdict: verdict.into(),
                reason: greason.clone(),
                source: if touched { "condition" } else { "global" }.into(),
                multiplier: outcome.multiplier,
                reason_code: reason_code.into(),
                value: None,
                threshold: None,
            }
        })
        .collect()
}

/// Gates that run before the soil-saturation block: override, pause,
/// restriction, rain-now, freeze, soil-frost, wind, already-wet. `Some`
/// = a gate fired (first wins); `None` = fall through to soil/weather.
/// The control + restriction gates ignore `disabled` (PROTECTED_RULES,
/// hard-enforced); every weather/safety gate consults it.
fn pre_soil(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
) -> Option<(&'static str, String, &'static str)> {
    // Sticky global override (highest precedence: beats the one-day override,
    // pause, restrictions, and every weather/soil gate). "run" force-runs past
    // all skip conditions; "skip" force-skips. "auto"/"" falls through.
    match i.global_override.as_str() {
        "skip" => return Some(("skip", "Manual override: skip".to_string(), "override")),
        "run" => return Some(("run", "Manual override: force run".to_string(), "override")),
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
            "run" => return Some(("run", String::new(), "override")),
            _ => {}
        }
    }
    if i.pause_until_epoch > 0 && i.now_epoch > 0 && i.now_epoch < i.pause_until_epoch {
        let until = format_pause_until(i.pause_until_epoch);
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
    if !i.watering_restrictions.is_empty() && i.now_epoch > 0 {
        if let Some(now_local) = Local.timestamp_opt(i.now_epoch, 0).single() {
            let v = restrictions::evaluate(now_local, &i.watering_restrictions, i.address_parity);
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
        return Some((
            "skip",
            format!(
                "Currently raining ({:.2} in/hr)",
                i.rain_intensity_now_in_hr
            ),
            "rain_now",
        ));
    }
    if !disabled.contains("freeze_now") && i.temp_now_f < i.min_temp_f {
        return Some((
            "skip",
            format!(
                "Freeze risk now ({:.0}°F < {:.0}°F)",
                i.temp_now_f, i.min_temp_f
            ),
            "freeze_now",
        ));
    }
    // Applicability is "do we have a forecast low at all" (Option), not a
    // numeric sentinel: a genuine low of 0 °F or colder must still skip.
    if let Some(t24) = i
        .temp_min_24h_f
        .filter(|_| !disabled.contains("overnight_freeze"))
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
    if !disabled.contains("wind_forecast")
        && i.wind_max_today_mph > i.max_wind_mph + p.wind_forecast_slack_mph
    {
        return Some((
            "skip",
            format!(
                "Windy day forecast (peak {:.0} mph > {:.0} + {:.0})",
                i.wind_max_today_mph, i.max_wind_mph, p.wind_forecast_slack_mph
            ),
            "wind_forecast",
        ));
    }
    if !disabled.contains("already_wet") && i.rain_today_in >= p.already_wet_in {
        return Some((
            "skip",
            format!("Already wet ({:.2}\" today)", i.rain_today_in),
            "already_wet",
        ));
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

/// The three forward-looking rain SKIP conditions, factored out so `post_soil`,
/// `decide_traced`, and the soil-floor classifier all read ONE source of truth
/// (no drift between the aggregate ladder and the per-zone veto). Each is the
/// exact condition the matching gate fires on, `disabled` membership included so
/// the predicates are self-contained.
fn rain_next_4h_fires(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    !disabled.contains("rain_next_4h")
        && !i.forecast_stale
        && i.rain_next_4h_in >= p.rain_next_4h_skip_in
}
fn tomorrow_rain_fires(i: &Inputs, _p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    let weighted = i.forecast_in * (i.rain_tomorrow_prob_pct as f64) / 100.0;
    !disabled.contains("tomorrow_rain") && !i.forecast_stale && weighted >= i.rain_skip_in
}
fn rain_3day_fires(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> bool {
    !disabled.contains("rain_3day")
        && !i.forecast_stale
        && i.rain_3day_weighted_in >= p.rain_3day_factor * i.rain_skip_in
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
    !disabled.contains("rain_now") && i.rain_intensity_now_in_hr > p.rain_now_in_hr
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
        return (
            "skip",
            format!(
                "Currently raining ({:.2} in/hr)",
                i.rain_intensity_now_in_hr
            ),
            "rain_now",
        );
    }
    // The three forward-looking rain SKIPs below are suppressed when the
    // forecast is stale (`forecast_stale`): a frozen "rain coming" snapshot from
    // an Open-Meteo outage must not skip a real watering and starve the yard. The
    // measured gates (rain_now from the station, already_wet, soil) still apply,
    // and freeze / heat-advisory keep their own (safe-direction) behavior.
    if !floor_active && rain_next_4h_fires(i, p, disabled) {
        return (
            "skip",
            format!(
                "Rain expected within 4h ({:.2}\" forecast)",
                i.rain_next_4h_in
            ),
            "rain_next_4h",
        );
    }
    if !floor_active && tomorrow_rain_fires(i, p, disabled) {
        return (
            "skip",
            format!(
                "Tomorrow rain ({:.2}\" × {}% confidence)",
                i.forecast_in, i.rain_tomorrow_prob_pct
            ),
            "tomorrow_rain",
        );
    }
    if !floor_active && rain_3day_fires(i, p, disabled) {
        return (
            "skip",
            format!(
                "Heavy rain in next 3 days ({:.2}\" weighted)",
                i.rain_3day_weighted_in
            ),
            "rain_3day",
        );
    }
    if !disabled.contains("heat_advisory")
        && i.temp_max_3day_f >= p.heat_advisory_temp_f
        && i.humidity_now_pct >= p.heat_advisory_humidity_pct
        && i.days_since_significant_rain >= p.heat_advisory_dry_days
        && i.rain_3day_weighted_in < 0.5 * i.rain_skip_in
    {
        return (
            "run_extended",
            format!(
                "Heat advisory: running planned + 15% (peak {:.0}°F)",
                i.temp_max_3day_f
            ),
            "heat_advisory",
        );
    }

    if i.is_dry_run {
        return ("skip", "Dry-run mode".to_string(), "dry_run");
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

// `RuleEval` + `DecisionTrace` live in `crate::ha::snapshot` (the shared,
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
            verdict: None,
            margin_label: None,
            // P1 operands filled in by annotate_margins for evaluated threshold
            // gates; None here (disabled / not_reached / inapplicable rows).
            value: None,
            threshold: None,
            unit_kind: None,
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
            verdict: None,
            margin_label: None,
            // P1 operands filled in by annotate_margins for evaluated threshold
            // gates; None here (disabled / not_reached / inapplicable rows).
            value: None,
            threshold: None,
            unit_kind: None,
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
            verdict: None,
            margin_label: None,
            // P1 operands filled in by annotate_margins for evaluated threshold
            // gates; None here (disabled / not_reached / inapplicable rows).
            value: None,
            threshold: None,
            unit_kind: None,
        });
        return;
    }
    rules.push(RuleEval {
        id: id.into(),
        label: label.into(),
        category: category.into(),
        detail,
        outcome: if cond { "fired" } else { "passed" }.into(),
        verdict: if cond { Some(verdict.into()) } else { None },
        margin_label: None,
        // P1 operands written by annotate_margins after the ladder is built (it
        // needs the settled fired/passed outcome); seeded None here.
        value: None,
        threshold: None,
        unit_kind: None,
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
        is_paused: false,
        is_dry_run: false,
        pause_until_epoch: 0,
        now_epoch: 0,
        override_tomorrow: String::new(),
        is_tomorrow: false,
        global_override: "auto".to_string(),
        zone_overrides: std::collections::HashMap::new(),
        watering_restrictions: Vec::new(),
        address_parity: AddressParity::NotApplicable,
    }
}

/// P3-9: annotate each threshold gate with a plain-language "distance to flip"
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
         -> Option<(String, f64, f64, &'static str)> {
            let dist = (actual - threshold).abs();
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
            Some((label, actual, threshold, unit_kind_for(unit)))
        };
        let annotated = match r.id.as_str() {
            "rain_now" => {
                let (a, t) = (i.rain_intensity_now_in_hr, p.rain_now_in_hr);
                mk(a, t, a > t, " in/hr", 2)
            }
            "already_wet" => {
                let (a, t) = (i.rain_today_in, p.already_wet_in);
                mk(a, t, a >= t, "\"", 2)
            }
            "observed_rain" => {
                let (a, t) = (i.rain_observed_recent_in, i.rain_skip_in);
                mk(a, t, a >= t, "\"", 2)
            }
            "rain_next_4h" => {
                let (a, t) = (i.rain_next_4h_in, p.rain_next_4h_skip_in);
                mk(a, t, a >= t, "\"", 2)
            }
            "rain_3day" => {
                let (a, t) = (i.rain_3day_weighted_in, p.rain_3day_factor * i.rain_skip_in);
                mk(a, t, a >= t, "\"", 2)
            }
            "tomorrow_rain" => {
                let a = i.forecast_in * (i.rain_tomorrow_prob_pct as f64) / 100.0;
                let t = i.rain_skip_in;
                mk(a, t, a >= t, "\"", 2)
            }
            "wind_now" => {
                let (a, t) = (i.wind_now_mph, i.max_wind_mph);
                mk(a, t, a > t, " mph", 0)
            }
            "wind_forecast" => {
                let (a, t) = (
                    i.wind_max_today_mph,
                    i.max_wind_mph + p.wind_forecast_slack_mph,
                );
                mk(a, t, a > t, " mph", 0)
            }
            "freeze_now" => {
                let (a, t) = (i.temp_now_f, i.min_temp_f);
                mk(a, t, a < t, "°F", 0)
            }
            "overnight_freeze" => i
                .temp_min_24h_f
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
            Some((label, value, threshold, unit_kind)) => {
                r.margin_label = Some(label);
                r.value = Some(value);
                r.threshold = Some(threshold);
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
    if i.global_override.as_str() != "run" {
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
    let mut rules: Vec<RuleEval> = Vec::with_capacity(18);
    let mut decided: Option<(String, String)> = None;
    // Operator-disabled built-in rules (protected ids already filtered
    // out by `disabled_set`). Threaded into every gate() so a disabled
    // rule still surfaces in the trace as "disabled by operator" but can
    // never decide. Mirrors the checks in pre_soil/soil_saturation/
    // post_soil exactly; the parity tests pin the two ladders together.
    let disabled = disabled_set(p);

    // Quarantine-inferred effective soil, shared with decide()/decide_per_zone so
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
        i.pause_until_epoch > 0 && i.now_epoch > 0,
        i.now_epoch < i.pause_until_epoch,
        // Stable, readable detail. The old "now {now_epoch} vs until {x}" baked
        // the live clock into the trace, so the decision_trace mutated every
        // ~10s tick and defeated the P3-2 SSE change-gate (and read as noise).
        if i.pause_until_epoch > 0 {
            format!("until {}", format_pause_until(i.pause_until_epoch))
        } else {
            "no timed pause set".to_string()
        },
        "skip",
        format!(
            "Paused (vacation until {})",
            format_pause_until(i.pause_until_epoch)
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
        let applicable = !i.watering_restrictions.is_empty() && i.now_epoch > 0;
        let (cond, reason) = if applicable {
            match Local.timestamp_opt(i.now_epoch, 0).single() {
                Some(now_local) => {
                    let v = restrictions::evaluate(
                        now_local,
                        &i.watering_restrictions,
                        i.address_parity,
                    );
                    (
                        v.skip,
                        v.reason
                            .unwrap_or_else(|| "Watering restriction".to_string()),
                    )
                }
                None => (false, String::new()),
            }
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
            LiveReadings::Station => "live station readings".to_string(),
            LiveReadings::ForecastFallback => {
                "station stale/absent; using forecast current-hour values (degraded)".to_string()
            }
            LiveReadings::Unavailable => "no station data and no forecast".to_string(),
        },
        "skip",
        "Live weather unavailable (no station data or forecast); failing safe".to_string(),
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
            i.rain_intensity_now_in_hr, p.rain_now_in_hr
        ),
        "skip",
        format!(
            "Currently raining ({:.2} in/hr)",
            i.rain_intensity_now_in_hr
        ),
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
        i.temp_now_f < i.min_temp_f,
        format!("{:.0}°F vs {:.0}°F min", i.temp_now_f, i.min_temp_f),
        "skip",
        format!(
            "Freeze risk now ({:.0}°F < {:.0}°F)",
            i.temp_now_f, i.min_temp_f
        ),
    );

    // Overnight freeze look-ahead. Applicable only when a 24h forecast
    // low exists; a genuine 0 °F (or colder) low still fires the rule.
    {
        let t24 = i.temp_min_24h_f;
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "overnight_freeze",
            "Overnight freeze",
            "safety",
            t24.is_some(),
            t24.map(|t| t < i.min_temp_f).unwrap_or(false),
            match t24 {
                Some(t) => format!("24h low {:.0}°F vs {:.0}°F min", t, i.min_temp_f),
                None => "no 24h forecast low".to_string(),
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
        i.wind_max_today_mph > i.max_wind_mph + p.wind_forecast_slack_mph,
        format!(
            "peak {:.0} mph vs {:.0}+{:.0}",
            i.wind_max_today_mph, i.max_wind_mph, p.wind_forecast_slack_mph
        ),
        "skip",
        format!(
            "Windy day forecast (peak {:.0} mph > {:.0} + {:.0})",
            i.wind_max_today_mph, i.max_wind_mph, p.wind_forecast_slack_mph
        ),
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
            "{:.2}\" today vs {:.2}\" floor",
            i.rain_today_in, p.already_wet_in
        ),
        "skip",
        format!("Already wet ({:.2}\" today)", i.rain_today_in),
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
    // reports a reading. Judged on the EFFECTIVE (quarantine-inferred) soil so
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
            i.rain_next_4h_in, p.rain_next_4h_skip_in
        ),
        "skip",
        format!(
            "Rain expected within 4h ({:.2}\" forecast)",
            i.rain_next_4h_in
        ),
    );

    // Tomorrow rain (confidence-weighted).
    {
        let weighted = i.forecast_in * (i.rain_tomorrow_prob_pct as f64) / 100.0;
        // #4a: when the probability is 0 (Open-Meteo daily lacked tomorrow's PoP,
        // but an HA forecast sensor still supplied an amount), the old detail read
        // "0.40" × 0% = 0.00"", a confusing "0% confidence" row that looks like the
        // forecast is empty. The gate can never fire at 0% (weighted is 0), so
        // relabel the passed row to state the missing-probability case plainly
        // instead of multiplying by a phantom 0%.
        let detail = if i.rain_tomorrow_prob_pct == 0 && i.forecast_in > 0.0 {
            format!(
                "{:.2}\" forecast, no probability data (does not skip)",
                i.forecast_in
            )
        } else {
            format!(
                "{:.2}\" × {}% = {:.2}\" vs {:.2}\"",
                i.forecast_in, i.rain_tomorrow_prob_pct, weighted, i.rain_skip_in
            )
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
            format!(
                "Tomorrow rain ({:.2}\" × {}% confidence)",
                i.forecast_in, i.rain_tomorrow_prob_pct
            ),
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
            i.rain_3day_weighted_in,
            p.rain_3day_factor * i.rain_skip_in
        ),
        "skip",
        format!(
            "Heavy rain in next 3 days ({:.2}\" weighted)",
            i.rain_3day_weighted_in
        ),
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

    // Heat advisory -> extend the run.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "heat_advisory",
        "Heat advisory",
        "heat",
        true,
        i.temp_max_3day_f >= p.heat_advisory_temp_f
            && i.humidity_now_pct >= p.heat_advisory_humidity_pct
            && i.days_since_significant_rain >= p.heat_advisory_dry_days
            && i.rain_3day_weighted_in < 0.5 * i.rain_skip_in,
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

    // Dry-run mode.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "dry_run",
        "Dry-run mode",
        "control",
        true,
        i.is_dry_run,
        format!("dry_run = {}", i.is_dry_run),
        "skip",
        "Dry-run mode".to_string(),
    );

    // P3-9: fill in each threshold gate's distance-to-flip now that the full
    // ladder is built (so "fired vs passed" is settled before we phrase it).
    // `eff` differs from `i` only in soil_zones, so every non-soil margin is
    // identical; the soil_saturation margin reads the effective (inferred) soil
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
        .find(|r| r.outcome == "fired")
        .map(|r| r.id.clone())
        .unwrap_or_else(|| "run".to_string());
    DecisionTrace {
        verdict,
        reason,
        // Anything short of fresh station data OR a stale forecast marks the
        // trace degraded so the Rule Lab / API can flag that the decision ran on
        // substituted or aged inputs. (Previously only station staleness set this,
        // so a multi-hour-old forecast produced a confident, non-degraded verdict.)
        degraded: i.live_readings != LiveReadings::Station || i.forecast_stale,
        reason_code,
        rules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::conditions::{
        CmpOp, ConditionExpr, ConditionRule, Metric, RuleAction, RuleScope,
    };

    #[test]
    fn soil_fields_generalize_to_any_zone_slug() {
        // A zone with a non-default slug must surface soil_<slug>_pct +
        // saturation_<slug>_pct (the manifest reads these), and round-trip back.
        // P1-2: a non-default per-zone floor (42%, not the 30% default) must
        // survive the round-trip; the pre-fix rebuild hardcoded 30.0 and silently
        // dropped it in the simulator's what-if.
        let zones = vec![ZoneSoil {
            slug: "vegetable_garden".into(),
            name: "veg".into(),
            pct: Some(33.0),
            saturation_pct: 65.0,
            target_min_pct: 42.0,
        }];
        let m = build_soil_fields(&zones);
        assert_eq!(m.get("soil_vegetable_garden_pct"), Some(&Some(33.0)));
        assert_eq!(m.get("saturation_vegetable_garden_pct"), Some(&Some(65.0)));
        assert_eq!(m.get("target_vegetable_garden_pct"), Some(&Some(42.0)));
        let sc = crate::ha::snapshot::SkipCheck {
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
        let sc = crate::ha::snapshot::SkipCheck {
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
            temp_now_f: 70.0,
            wind_now_mph: 3.0,
            rain_today_in: 0.0,
            rain_intensity_now_in_hr: 0.0,
            // The historical fixture assumed a live LAN gauge drove the rain rate,
            // so default the nature to Measured (observation-grade). A test that
            // sets rain_intensity_now_in_hr thus exercises the HARD rain_now skip
            // by default; the model-rain (soft) tests set rain_nature = Model.
            rain_nature: RainNature::Measured,
            humidity_now_pct: 55.0,
            forecast_in: 0.0,
            rain_tomorrow_prob_pct: 0,
            rain_3day_weighted_in: 0.0,
            rain_7day_weighted_in: 0.0,
            rain_next_4h_in: 0.0,
            rain_observed_recent_in: 0.0,
            wind_max_today_mph: 6.0,
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
            is_paused: false,
            is_dry_run: false,
            pause_until_epoch: 0,
            now_epoch: 1_700_000_000,
            override_tomorrow: String::new(),
            is_tomorrow: false,
            global_override: "auto".to_string(),
            zone_overrides: std::collections::HashMap::new(),
            watering_restrictions: Vec::new(),
            address_parity: AddressParity::NotApplicable,
        }
    }

    // P0-2: a stale forecast must not fabricate a forward-looking rain skip (it
    // would starve the yard during an outage), and it must mark the trace degraded
    // so the confidence is honest. Mirror with a fresh forecast that DOES skip.
    #[test]
    fn stale_forecast_suppresses_predicted_rain_skip_and_marks_degraded() {
        let p = SkipRuleParams::default();
        let mut i = base();
        // Heavy 3-day rain that, with a fresh forecast, fires the rain_3day skip.
        i.rain_3day_weighted_in = 5.0;

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
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: f,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "side_yard".into(),
                name: "side yard".into(),
                pct: s,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "back_yard_shrubs".into(),
                name: "back yard shrubs".into(),
                pct: sh,
                saturation_pct: 85.0,
                target_min_pct: 25.0,
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
        push(|i| i.rain_intensity_now_in_hr = 0.05);
        push(|i| i.temp_now_f = 30.0);
        push(|i| {
            i.temp_now_f = 50.0;
            i.temp_min_24h_f = Some(32.0);
        });
        push(|i| i.temp_min_24h_f = None);
        push(|i| i.live_readings = LiveReadings::ForecastFallback);
        push(|i| i.live_readings = LiveReadings::Unavailable);
        push(|i| i.soil_temp_yard_min_f = Some(33.0));
        push(|i| i.wind_now_mph = 20.0);
        push(|i| i.wind_max_today_mph = 30.0);
        push(|i| i.rain_today_in = 0.10);
        push(|i| i.rain_observed_recent_in = 1.5);
        push(|i| {
            i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        });
        push(|i| i.rain_next_4h_in = 0.20);
        push(|i| {
            i.forecast_in = 0.40;
            i.rain_tomorrow_prob_pct = 90;
        });
        push(|i| i.rain_3day_weighted_in = 1.0);
        push(|i| {
            i.temp_max_3day_f = 98.0;
            i.humidity_now_pct = 70.0;
            i.days_since_significant_rain = 3;
            i.rain_3day_weighted_in = 0.0;
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
            i.rain_next_4h_in = 0.50;
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
        i.rain_next_4h_in = 0.50;
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0),
            saturation_pct: 70.0,
            target_min_pct: 30.0,
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
        // it would mutate every ~10s refresh, defeating the P3-2 SSE change-gate
        // and reading as noise. Two evaluations 11s apart with identical weather,
        // soil, and control state must produce byte-identical traces.
        let p = SkipRuleParams::default();
        let mut early = base();
        early.now_epoch = 1_782_567_229;
        let mut late = base();
        late.now_epoch = 1_782_567_240;
        assert_eq!(
            decide_traced(&early, &p),
            decide_traced(&late, &p),
            "decision_trace must not change with the wall clock alone"
        );

        // Also exercise the pause_until FIRING path -- the exact gate whose detail
        // string used to bake in now_epoch. With an active pause (same expiry,
        // different clock) the trace must STILL be byte-identical.
        let mut p_early = base();
        p_early.now_epoch = 1_782_567_229;
        p_early.pause_until_epoch = p_early.now_epoch + 3600;
        let mut p_late = base();
        p_late.now_epoch = 1_782_567_240;
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
        ("skip", "Soil frost (33.0°F < 35°F threshold)", "soil_frost"),
        ("skip", "Wind too high now (20.0 mph > 10 mph)", "wind_now"),
        (
            "skip",
            "Windy day forecast (peak 30 mph > 10 + 5)",
            "wind_forecast",
        ),
        ("skip", "Already wet (0.10\" today)", "already_wet"),
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
        ("skip", "Dry-run mode", "dry_run"),
        ("skip", "Paused (vacation mode)", "paused"),
        ("skip", "Manual override (skip tomorrow)", "override"),
        ("run", "", "override"),
        ("run", "", "soil_floor"),
        // Sticky global override (the #3 fix): both ladders honor it as the
        // first rung, so the decided tuple is the override verdict regardless of
        // the weather/soil state behind it (the "run" case force-runs a freeze).
        ("skip", "Manual override: skip", "override"),
        ("run", "Manual override: force run", "override"),
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
        i.rain_intensity_now_in_hr = 0.05;
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
    fn reason_code_soil_quarantine_for_inferred_zone() {
        // A wild-outlier probe (28% vs siblings ~73%) is quarantined; the zone's
        // saturation decision rides the inferred sibling median, so the per-zone
        // verdict's reason_code is "soil_quarantine", not "soil_saturation".
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(28.0), Some(72.0), Some(74.0), Some(90.0));
        let zvs = decide_per_zone(&i, &p, &[]);
        let bad = zvs.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(
            bad.source, "soil_quarantine",
            "the outlier zone is quarantined"
        );
        assert_eq!(bad.reason_code, "soil_quarantine");
        // Soil operands carried: effective % vs saturation %.
        assert!(bad.value.is_some() && bad.threshold == Some(70.0));
    }

    #[test]
    fn suspect_probes_flags_outlier_independent_of_verdict() {
        // A wild-outlier probe (28% vs siblings ~52%) is quarantined. Here a
        // GLOBAL gate (forecast rain within 4h) decides every zone, so the
        // per-zone verdict.source is "global" and the old verdict-gated banner
        // would have hidden the bad probe. suspect_probes still flags it because
        // it reads the quarantine plan off the RAW readings, not the verdict.
        let p = SkipRuleParams::default();
        let mut i = base();
        // 28% vs a ~73% yard is a >35pp outlier -> quarantined (the 2026-06
        // incident's numbers). back_yard at 28% keeps the all-zones
        // soil_saturation gate from firing, so the deciding gate is global.
        i.soil_zones = soil4(Some(28.0), Some(72.0), Some(74.0), Some(76.0));
        // Force a global forecast-rain skip that binds every zone (checked
        // before the per-zone soil-saturation rung, so it masks the source).
        i.rain_next_4h_in = 1.0;

        // The deciding gate is global, NOT soil_quarantine.
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
        i.pause_until_epoch = i.now_epoch + 3600;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.starts_with("Paused (vacation until"));
    }

    #[test]
    fn pause_until_expired_falls_through() {
        let mut i = base();
        i.pause_until_epoch = i.now_epoch - 3600;
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
        i.rain_intensity_now_in_hr = 0.05;
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
        i.rain_next_4h_in = 0.15;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("4h"));
    }

    #[test]
    fn tomorrow_high_confidence_skips() {
        let mut i = base();
        i.forecast_in = 0.30;
        i.rain_tomorrow_prob_pct = 90;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
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
        i.rain_next_4h_in = 0.50; // also a (demotable) forecast-rain skip
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0),
            saturation_pct: 70.0,
            target_min_pct: 30.0,
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
        i.rain_3day_weighted_in = 0.05;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run_extended");
    }

    #[test]
    fn heat_advisory_temp_threshold_is_configurable() {
        let mut i = base();
        i.temp_max_3day_f = 92.0; // below default 95
        i.humidity_now_pct = 65.0;
        i.days_since_significant_rain = 3;
        i.rain_3day_weighted_in = 0.05;
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
        // own). The quarantine-on counterpart (offline inferred from saturated
        // siblings -> skip) is `quarantine_offline_probe_infers_from_saturated_siblings`.
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
        assert_eq!(s.reason, "Dry-run mode");
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
    fn forecast_fallback_runs_but_marks_trace_degraded() {
        let p = SkipRuleParams::default();
        let mut i = base();
        i.live_readings = LiveReadings::ForecastFallback;
        let t = decide_traced(&i, &p);
        assert_eq!(t.verdict, "run");
        assert!(t.degraded);
        // Fresh station data is not degraded.
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
        push(|i| i.rain_next_4h_in = 0.50);

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

    // ── P1-2: measured-dry-soil veto (the soil_floor moat) ───────────────────

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
        i.rain_next_4h_in = 0.50;
        i.soil_zones = vec![
            ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: Some(20.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: Some(45.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
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
            }]
        };
        type Mut = fn(&mut Inputs);
        let cases: &[(&str, Mut)] = &[
            ("rain_now", |i| i.rain_intensity_now_in_hr = 0.05),
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
                i.pause_until_epoch = i.now_epoch + 3600;
            }),
            ("global_override", |i| i.global_override = "skip".into()),
            ("live_data", |i| i.live_readings = LiveReadings::Unavailable),
            // RISK A: dry_run fires in post_soil AFTER the rain gates; the floor
            // suppresses rain, but dry_run must still win (it's not demotable).
            ("dry_run", |i| i.is_dry_run = true),
        ];
        for (name, mutate) in cases {
            let mut i = base();
            i.rain_next_4h_in = 0.50;
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
        i.rain_next_4h_in = 0.50;
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
            i.rain_next_4h_in = 0.50;
            i.soil_zones = vec![ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct,
                saturation_pct: 70.0,
                target_min_pct: target,
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
        i.rain_next_4h_in = 0.50;
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0),
            saturation_pct: 70.0,
            target_min_pct: 30.0,
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
        i.rain_intensity_now_in_hr = 0.05; // over the default rain_now threshold
        i.rain_nature = nature;
        i.soil_zones = vec![ZoneSoil {
            slug: "back_yard".into(),
            name: "back yard".into(),
            pct: Some(20.0), // measured-dry: below the 30% floor
            saturation_pct: 70.0,
            target_min_pct: 30.0,
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

    // ── P1-6: end-to-end golden matrix ───────────────────────────────────────
    // Every gate firing in isolation (ladder order), the key head-to-head
    // precedence cases, the soil_floor demotion, heat run_extended, the stale-
    // forecast suppression, and the default run. Verdict + reason-substring are
    // pinned against the real gate format strings, so any drift is one readable
    // failure.
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
                |i| i.pause_until_epoch = i.now_epoch + 3600,
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
                |i| i.rain_intensity_now_in_hr = 0.05,
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
                |i| i.rain_next_4h_in = 0.20,
                "skip",
                "Rain expected within 4h",
            ),
            (
                "tomorrow_rain",
                |i| {
                    i.forecast_in = 0.40;
                    i.rain_tomorrow_prob_pct = 90;
                },
                "skip",
                "Tomorrow rain",
            ),
            (
                "rain_3day",
                |i| i.rain_3day_weighted_in = 1.0,
                "skip",
                "Heavy rain in next 3 days",
            ),
            (
                "heat_advisory",
                |i| {
                    i.temp_max_3day_f = 98.0;
                    i.humidity_now_pct = 70.0;
                    i.days_since_significant_rain = 3;
                    i.rain_3day_weighted_in = 0.0;
                },
                "run_extended",
                "Heat advisory",
            ),
            ("dry_run", |i| i.is_dry_run = true, "skip", "Dry-run mode"),
            // precedence: the earlier gate wins when two fire
            (
                "override_run_beats_rain",
                |i| {
                    i.rain_intensity_now_in_hr = 0.05;
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
                    i.rain_intensity_now_in_hr = 0.05;
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
                    i.rain_next_4h_in = 0.20;
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
                    i.rain_next_4h_in = 0.50;
                    i.soil_zones = soil4(Some(20.0), Some(45.0), Some(45.0), Some(45.0));
                },
                "run",
                "",
            ),
            (
                "stale_forecast_no_skip",
                |i| {
                    i.rain_3day_weighted_in = 5.0;
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
        // generic "not all zones have soil sensors". Quarantine OFF so the
        // offline probes stay offline (with it on they'd be inferred from the
        // two present readings and the gate would become applicable).
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
        i.rain_intensity_now_in_hr = 0.05;
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
    fn protected_control_gates_cannot_be_disabled() {
        // Listing EVERY protected id changes nothing: dry-run, the timed
        // pause, and the tomorrow override all keep deciding.
        let mut p = SkipRuleParams::default();
        p.disabled_rules = PROTECTED_RULES.iter().map(|s| s.to_string()).collect();

        let mut i = base();
        i.is_dry_run = true;
        let s = evaluate_with(&i, &p);
        assert_eq!(s.verdict, "skip");
        assert_eq!(s.reason, "Dry-run mode");

        let mut i = base();
        i.pause_until_epoch = i.now_epoch + 3600;
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
        i.rain_intensity_now_in_hr = 0.05;
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
    fn force_overrode_guard_names_the_overridden_hard_guard() {
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

        // Force run through a freeze: the freeze reason is surfaced, verdict unchanged.
        let mut i = base();
        i.temp_now_f = 28.0;
        i.min_temp_f = 35.0;
        assert_eq!(
            decide(&i, &p).0,
            "skip",
            "sanity: the freeze skips without an override"
        );
        i.global_override = "run".into();
        assert_eq!(decide(&i, &p).0, "run", "force run still wins");
        let guard = force_overrode_guard(&i, &p).expect("freeze guard surfaced");
        assert!(
            guard.contains("Freeze"),
            "guard names the freeze: {guard:?}"
        );

        // A force-SKIP is not a force-run: no overridden-guard signal.
        let mut i = base();
        i.temp_now_f = 28.0;
        i.min_temp_f = 35.0;
        i.global_override = "skip".into();
        assert_eq!(force_overrode_guard(&i, &p), None);
    }

    #[test]
    fn zone_override_run_beats_global_skip() {
        let mut i = base();
        i.soil_zones = soil4(Some(40.0), Some(40.0), Some(40.0), Some(40.0));
        i.global_override = "skip".into();
        i.zone_overrides.insert("front_yard".into(), "run".into());
        let zv = decide_per_zone(&i, &SkipRuleParams::default(), &[]);
        let front = zv.iter().find(|z| z.zone_slug == "front_yard").unwrap();
        let back = zv.iter().find(|z| z.zone_slug == "back_yard").unwrap();
        assert_eq!(front.verdict, "run", "zone override run beats global skip");
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

    // ── Soil-probe QUARANTINE + infer-from-siblings ─────────────────────────

    #[test]
    fn quarantine_config_defaults() {
        // Additive params with the documented defaults: on, 35pp threshold.
        let p = SkipRuleParams::default();
        assert!(p.soil_quarantine_enabled);
        assert!((p.soil_outlier_threshold_pct - 35.0).abs() < 1e-9);
    }

    #[test]
    fn quarantine_outlier_low_probe_infers_saturated_and_skips() {
        // (a) The real incident: back_yard's bad-spot probe reads 28% while its
        // three siblings read 76/71/73 after the same rain. With the saturation
        // threshold at 70, the three siblings are saturated; back_yard is a wild
        // outlier (28 vs median 72 -> 44pp > 35), so it is quarantined and its
        // soil inferred from the trustworthy median (~73). The zone then reads
        // saturated and SKIPS, sourced "soil_quarantine".
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(28.0), Some(76.0), Some(71.0), Some(73.0));
        // shrubs has an 85% saturation threshold; lift it so the bug isolates to
        // the saturation inference, not the shrubs' own dryness.
        i.soil_zones[3].saturation_pct = 70.0;

        // Aggregate: all four effective zones saturated -> yard-wide skip.
        assert_eq!(decide(&i, &p).0, "skip");
        assert!(evaluate_with(&i, &p).will_skip);

        // Per-zone: back_yard's verdict is decided on the inferred value.
        let v = decide_per_zone(&i, &p, &[]);
        let back = zv(&v, "back_yard");
        assert_eq!(
            back.verdict, "skip",
            "quarantined zone must not run while saturated"
        );
        assert_eq!(back.source, "soil_quarantine");
        assert!(
            back.reason.contains("28%") && back.reason.contains("inferred from neighbors"),
            "reason must name the suspect reading + inference, got {:?}",
            back.reason
        );
        // The trustworthy siblings keep their normal soil_saturation source.
        assert_eq!(zv(&v, "front_yard").source, "soil_saturation");
    }

    #[test]
    fn quarantine_offline_probe_infers_from_saturated_siblings() {
        // (b) An OFFLINE (None) probe with saturated siblings: quarantine infers
        // the trustworthy median, so the offline zone reads saturated and skips.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(None, Some(80.0), Some(78.0), Some(82.0));
        i.soil_zones[3].saturation_pct = 70.0;

        assert_eq!(
            decide(&i, &p).0,
            "skip",
            "yard-wide saturation via inference"
        );
        let v = decide_per_zone(&i, &p, &[]);
        let back = zv(&v, "back_yard");
        assert_eq!(back.verdict, "skip");
        assert_eq!(back.source, "soil_quarantine");
        assert!(
            back.reason.contains("offline") && back.reason.contains("inferred from neighbors"),
            "offline reason must say offline, got {:?}",
            back.reason
        );
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
        i.rain_next_4h_in = 0.50; // demotable soft forecast-rain skip
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
    fn quarantine_offline_runs_via_floor_when_dry_siblings_infer_dry() {
        // (d-bis) An OFFLINE zone whose trustworthy siblings are genuinely dry
        // infers a DRY value, so on a soft-rain morning it too runs via the floor.
        // Quarantine never forces a run nor a skip on its own; it only swaps the
        // effective soil and lets the normal gates decide.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_next_4h_in = 0.50;
        i.soil_zones = soil4(None, Some(20.0), Some(22.0), Some(21.0));
        let v = decide_per_zone(&i, &p, &[]);
        let back = zv(&v, "back_yard");
        assert_eq!(
            back.verdict, "run",
            "offline zone inferred dry runs via the floor"
        );
        assert_eq!(back.source, "soil_floor");
    }

    #[test]
    fn quarantine_under_three_present_no_outlier_offline_falls_back() {
        // (e) Fewer than 3 PRESENT readings: no present reading is ever flagged an
        // outlier. With only 2 present trustworthy readings an OFFLINE zone is
        // still inferred (a trustworthy median exists); with 0/1 present and the
        // rest offline, there is no trustworthy median, so quarantine is inert and
        // the raw (None) reading stands (current behavior).
        let p = SkipRuleParams::default();

        // Two present saturated + two offline. Outliers can't be judged (only 2
        // present), but a trustworthy median (median of the two present) exists,
        // so BOTH offline zones are inferred saturated. A would-be low outlier
        // among the two present is NOT flagged (under 3 present).
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), Some(82.0), None, None);
        i.soil_zones[0].saturation_pct = 70.0;
        i.soil_zones[1].saturation_pct = 70.0;
        i.soil_zones[2].saturation_pct = 70.0;
        i.soil_zones[3].saturation_pct = 70.0;
        let plan = quarantine_plan(&i.soil_zones, &p);
        assert!(
            plan[0].is_none() && plan[1].is_none(),
            "present zones not flagged with <3 present"
        );
        assert!(
            plan[2].is_some() && plan[3].is_some(),
            "offline zones inferred from the trustworthy median"
        );
        let v = decide_per_zone(&i, &p, &[]);
        assert_eq!(zv(&v, "side_yard").source, "soil_quarantine");

        // No trustworthy median at all (all offline): quarantine inert, fall back.
        let mut j = base();
        j.soil_zones = soil4(None, None, None, None);
        assert!(quarantine_plan(&j.soil_zones, &p)
            .iter()
            .all(Option::is_none));
        // With no soil data the saturation gate is inapplicable -> a clear day runs.
        assert_eq!(decide(&j, &p).0, "run");
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
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: Some(76.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "side_yard".into(),
                name: "side yard".into(),
                pct: Some(71.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
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
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: Some(76.0),
                saturation_pct: 70.0,
                target_min_pct: 30.0,
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
    fn quarantine_high_outlier_distrusted_too() {
        // The outlier test is two-sided: a wildly-HIGH probe (one stuck at 95
        // amid {30,28,32}) is distrusted and inferred down to the dry median, so
        // it does NOT spuriously skip on a saturated-looking false reading.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_zones = soil4(Some(95.0), Some(30.0), Some(28.0), Some(32.0));
        let plan = quarantine_plan(&i.soil_zones, &p);
        assert!(plan[0].is_some(), "95 vs ~30 median is a high outlier");
        // back_yard inferred to the dry median (~30) -> runs, not a false skip.
        let v = decide_per_zone(&i, &p, &[]);
        assert_eq!(zv(&v, "back_yard").verdict, "run");
        assert_ne!(zv(&v, "back_yard").source, "soil_saturation");
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
}
