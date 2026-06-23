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
use crate::ha::snapshot::{DecisionTrace, RuleEval, SkipCheck, ZoneVerdict};

/// Inputs the engine needs. Caller fills these from HA states +
/// ForecastSnapshot helpers + TempestStore.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    // ── Live readings ──
    pub temp_now_f: f64,
    pub wind_now_mph: f64,
    pub rain_today_in: f64,
    pub rain_intensity_now_in_hr: f64,
    pub humidity_now_pct: f64,

    // ── Open-Meteo forecast ──
    pub forecast_in: f64,
    pub rain_tomorrow_prob_pct: u32,
    pub rain_3day_weighted_in: f64,
    pub rain_7day_weighted_in: f64,
    pub rain_next_4h_in: f64,
    pub wind_max_today_mph: f64,
    /// Forecast overnight low for the next 24h. `None` when the hourly
    /// forecast window is unavailable, so the overnight-freeze gate can
    /// distinguish "no data" from a genuine 0 °F (or colder) low. The
    /// old representation used 0.0 as a missing-data sentinel, which
    /// silently disabled the rule in real sub-zero cold snaps.
    pub temp_min_24h_f: Option<f64>,
    pub temp_max_3day_f: f64,
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

// Helpers to bridge the generalized `soil_zones` Vec back onto the eight
// fixed `SkipCheck` fields (the /api/v1 shape contract).
fn legacy_soil_pct(zones: &[ZoneSoil], slug: &str) -> Option<f64> {
    zones.iter().find(|z| z.slug == slug).and_then(|z| z.pct)
}

fn legacy_soil_sat(zones: &[ZoneSoil], slug: &str, default: f64) -> f64 {
    zones
        .iter()
        .find(|z| z.slug == slug)
        .map(|z| z.saturation_pct)
        .unwrap_or(default)
}

fn format_pause_until(epoch: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.format("%a %b %-d, %-I %p").to_string(),
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
    let heat_index_3day = heat_index_f(i.temp_max_3day_f, i.humidity_now_pct);

    let (verdict, reason) = decide(i, params);

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

        // Bridge the generalized per-zone Vec back onto the fixed eight
        // SkipCheck fields (the /api/v1 shape contract). Absent zones
        // serialize as None / their legacy default threshold.
        soil_back_yard_pct: legacy_soil_pct(&i.soil_zones, "back_yard"),
        soil_front_yard_pct: legacy_soil_pct(&i.soil_zones, "front_yard"),
        soil_side_yard_pct: legacy_soil_pct(&i.soil_zones, "side_yard"),
        soil_back_yard_shrubs_pct: legacy_soil_pct(&i.soil_zones, "back_yard_shrubs"),
        soil_temp_yard_min_f: i.soil_temp_yard_min_f,
        soil_temp_yard_max_f: i.soil_temp_yard_max_f,
        frost_skip_soil_f: i.frost_skip_soil_f,
        saturation_back_yard_pct: legacy_soil_sat(&i.soil_zones, "back_yard", 70.0),
        saturation_front_yard_pct: legacy_soil_sat(&i.soil_zones, "front_yard", 70.0),
        saturation_side_yard_pct: legacy_soil_sat(&i.soil_zones, "side_yard", 70.0),
        saturation_back_yard_shrubs_pct: legacy_soil_sat(&i.soil_zones, "back_yard_shrubs", 85.0),

        is_paused: i.is_paused,
        is_dry_run: i.is_dry_run,

        will_skip: verdict == "skip",
        verdict: verdict.to_string(),
        reason,
    }
}

/// Aggregate rule ladder. Order matters: first matching rule wins. Order
/// is override > paused > restriction > weather-safety > soil-saturation >
/// rain-forecast > heat-advisory > dry-run > run. Composed from three
/// pieces so the per-zone path (`decide_per_zone`) can reuse the global
/// gates while substituting its own per-zone soil logic.
fn decide(i: &Inputs, p: &SkipRuleParams) -> (&'static str, String) {
    let disabled = disabled_set(p);
    if let Some(v) = pre_soil(i, p, &disabled) {
        return v;
    }
    if let Some(v) = soil_saturation(i, &disabled) {
        return v;
    }
    post_soil(i, p, &disabled)
}

/// The global verdict EXCLUDING the per-zone soil-saturation gate. Used by
/// `decide_per_zone` as the yard-wide baseline that binds every zone;
/// each zone then layers its own soil + custom-condition gates on top.
fn global_verdict(
    i: &Inputs,
    p: &SkipRuleParams,
    disabled: &HashSet<&str>,
) -> (&'static str, String) {
    pre_soil(i, p, disabled).unwrap_or_else(|| post_soil(i, p, disabled))
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
    let (gverdict, greason) = global_verdict(i, p, &disabled);
    i.soil_zones
        .iter()
        .map(|z| {
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
                    }
                }
                _ => {}
            }
            // Global safety/weather gate binds every zone.
            if gverdict == "skip" {
                return ZoneVerdict {
                    zone_slug: z.slug.clone(),
                    zone_name: z.name.clone(),
                    verdict: "skip".into(),
                    reason: greason.clone(),
                    source: "global".into(),
                    multiplier: 1.0,
                };
            }
            // Global verdict is run / run_extended. Per-zone soil saturation
            // can still skip this individual zone. Honors the same operator
            // disable id as the yard-wide gate: disabling "soil_saturation"
            // disables soil-saturation skips everywhere.
            if let Some(pct) = z.pct.filter(|_| !disabled.contains("soil_saturation")) {
                if pct >= z.saturation_pct {
                    return ZoneVerdict {
                        zone_slug: z.slug.clone(),
                        zone_name: z.name.clone(),
                        verdict: "skip".into(),
                        reason: format!(
                            "Soil saturated ({:.0}% ≥ {:.0}% threshold)",
                            pct, z.saturation_pct
                        ),
                        source: "soil_saturation".into(),
                        multiplier: 1.0,
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
                };
            }
            let verdict = if gverdict == "run_extended" || outcome.extend {
                "run_extended"
            } else {
                "run"
            };
            let touched = outcome.extend || (outcome.multiplier - 1.0).abs() > 1e-9;
            ZoneVerdict {
                zone_slug: z.slug.clone(),
                zone_name: z.name.clone(),
                verdict: verdict.into(),
                reason: greason.clone(),
                source: if touched { "condition" } else { "global" }.into(),
                multiplier: outcome.multiplier,
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
) -> Option<(&'static str, String)> {
    // Sticky global override (highest precedence — beats the one-day override,
    // pause, restrictions, and every weather/soil gate). "run" force-runs past
    // all skip conditions; "skip" force-skips. "auto"/"" falls through.
    match i.global_override.as_str() {
        "skip" => return Some(("skip", "Manual override: skip".to_string())),
        "run" => return Some(("run", "Manual override: force run".to_string())),
        _ => {}
    }
    if i.is_tomorrow {
        match i.override_tomorrow.as_str() {
            "skip" => return Some(("skip", "Manual override (skip tomorrow)".to_string())),
            "run" => return Some(("run", String::new())),
            _ => {}
        }
    }
    if i.pause_until_epoch > 0 && i.now_epoch > 0 && i.now_epoch < i.pause_until_epoch {
        let until = format_pause_until(i.pause_until_epoch);
        return Some(("skip", format!("Paused (vacation until {until})")));
    }
    if i.is_paused {
        return Some(("skip", "Paused (vacation mode)".to_string()));
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
        ));
    }
    if !disabled.contains("rain_now") && i.rain_intensity_now_in_hr > p.rain_now_in_hr {
        return Some((
            "skip",
            format!(
                "Currently raining ({:.2} in/hr)",
                i.rain_intensity_now_in_hr
            ),
        ));
    }
    if !disabled.contains("freeze_now") && i.temp_now_f < i.min_temp_f {
        return Some((
            "skip",
            format!(
                "Freeze risk now ({:.0}°F < {:.0}°F)",
                i.temp_now_f, i.min_temp_f
            ),
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
        ));
    }
    if !disabled.contains("already_wet") && i.rain_today_in >= p.already_wet_in {
        return Some((
            "skip",
            format!("Already wet ({:.2}\" today)", i.rain_today_in),
        ));
    }
    None
}

/// The yard-wide soil-saturation gate (aggregate view): skip only when
/// EVERY configured zone has a soil reading AND all are at/above their
/// saturation threshold. Generalized from the former hardcoded 4-zone
/// array to iterate `i.soil_zones`. `None` when not all zones report or
/// any zone is below threshold.
fn soil_saturation(i: &Inputs, disabled: &HashSet<&str>) -> Option<(&'static str, String)> {
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
        ));
    }
    None
}

/// Gates that run after soil saturation: rain-within-4h, tomorrow rain,
/// 3-day rain, heat advisory, dry-run, default run. The dry-run gate
/// ignores `disabled` (PROTECTED_RULES, hard-enforced).
fn post_soil(i: &Inputs, p: &SkipRuleParams, disabled: &HashSet<&str>) -> (&'static str, String) {
    if !disabled.contains("rain_next_4h") && i.rain_next_4h_in >= p.rain_next_4h_skip_in {
        return (
            "skip",
            format!(
                "Rain expected within 4h ({:.2}\" forecast)",
                i.rain_next_4h_in
            ),
        );
    }
    let tomorrow_weighted = i.forecast_in * (i.rain_tomorrow_prob_pct as f64) / 100.0;
    if !disabled.contains("tomorrow_rain") && tomorrow_weighted >= i.rain_skip_in {
        return (
            "skip",
            format!(
                "Tomorrow rain ({:.2}\" × {}% confidence)",
                i.forecast_in, i.rain_tomorrow_prob_pct
            ),
        );
    }
    if !disabled.contains("rain_3day")
        && i.rain_3day_weighted_in >= p.rain_3day_factor * i.rain_skip_in
    {
        return (
            "skip",
            format!(
                "Heavy rain in next 3 days ({:.2}\" weighted)",
                i.rain_3day_weighted_in
            ),
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
        );
    }

    if i.is_dry_run {
        return ("skip", "Dry-run mode".to_string());
    }

    ("run", String::new())
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
        humidity_now_pct: s.humidity_now_pct,
        forecast_in: s.forecast_in,
        rain_tomorrow_prob_pct: s.rain_tomorrow_prob_pct,
        rain_3day_weighted_in: s.rain_3day_weighted_in,
        rain_7day_weighted_in: s.rain_7day_weighted_in,
        rain_next_4h_in: s.rain_next_4h_in,
        wind_max_today_mph: s.wind_max_today_mph,
        temp_min_24h_f: if s.temp_min_24h_valid {
            Some(s.temp_min_24h_f)
        } else {
            None
        },
        temp_max_3day_f: s.temp_max_3day_f,
        days_since_significant_rain: s.days_since_significant_rain,
        max_wind_mph: s.max_wind_mph,
        min_temp_f: s.min_temp_f,
        rain_skip_in: s.rain_skip_in,
        // Rebuild the per-zone soil Vec from SkipCheck's fixed legacy
        // fields so the Simulator's what-if reflects the same four zones.
        soil_zones: vec![
            ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: s.soil_back_yard_pct,
                saturation_pct: s.saturation_back_yard_pct,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: s.soil_front_yard_pct,
                saturation_pct: s.saturation_front_yard_pct,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "side_yard".into(),
                name: "side yard".into(),
                pct: s.soil_side_yard_pct,
                saturation_pct: s.saturation_side_yard_pct,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "back_yard_shrubs".into(),
                name: "back yard shrubs".into(),
                pct: s.soil_back_yard_shrubs_pct,
                saturation_pct: s.saturation_back_yard_shrubs_pct,
                target_min_pct: 25.0,
            },
        ],
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

    // Manual override (tomorrow cell only).
    if i.is_tomorrow {
        match i.override_tomorrow.as_str() {
            "skip" => gate(
                &mut rules,
                &mut decided,
                &disabled,
                "override",
                "Manual override",
                "control",
                true,
                true,
                "override = skip".into(),
                "skip",
                "Manual override (skip tomorrow)".to_string(),
            ),
            "run" => gate(
                &mut rules,
                &mut decided,
                &disabled,
                "override",
                "Manual override",
                "control",
                true,
                true,
                "override = run".into(),
                "run",
                String::new(),
            ),
            _ => gate(
                &mut rules,
                &mut decided,
                &disabled,
                "override",
                "Manual override",
                "control",
                true,
                false,
                "no override set".into(),
                "skip",
                String::new(),
            ),
        }
    } else {
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "override",
            "Manual override",
            "control",
            false,
            false,
            "only applies to the tomorrow cell".into(),
            "skip",
            String::new(),
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
        format!("now {} vs until {}", i.now_epoch, i.pause_until_epoch),
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

    // Currently raining.
    gate(
        &mut rules,
        &mut decided,
        &disabled,
        "rain_now",
        "Currently raining",
        "safety",
        true,
        i.rain_intensity_now_in_hr > p.rain_now_in_hr,
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

    // Yard-wide soil saturation. Generalized to iterate the configured
    // zones; applicable only when at least one zone exists and every zone
    // reports a reading.
    {
        let applicable = !i.soil_zones.is_empty() && i.soil_zones.iter().all(|z| z.pct.is_some());
        let cond = applicable
            && i.soil_zones
                .iter()
                .all(|z| z.pct.unwrap() >= z.saturation_pct);
        let (detail, reason) = if applicable {
            let tightest = i
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
        } else if i.soil_zones.is_empty() {
            ("no soil zones configured".to_string(), String::new())
        } else {
            // Name the zones holding the gate inapplicable: a flatlined
            // probe resolves to None upstream, and the old generic "not
            // all zones have soil sensors" hid which hardware was dead.
            let missing: Vec<&str> = i
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
        i.rain_next_4h_in >= p.rain_next_4h_skip_in,
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
        gate(
            &mut rules,
            &mut decided,
            &disabled,
            "tomorrow_rain",
            "Tomorrow rain",
            "weather",
            true,
            weighted >= i.rain_skip_in,
            format!(
                "{:.2}\" × {}% = {:.2}\" vs {:.2}\"",
                i.forecast_in, i.rain_tomorrow_prob_pct, weighted, i.rain_skip_in
            ),
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
        i.rain_3day_weighted_in >= p.rain_3day_factor * i.rain_skip_in,
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

    let (verdict, reason) = decided.unwrap_or_else(|| ("run".to_string(), String::new()));
    DecisionTrace {
        verdict,
        reason,
        // Anything short of fresh station data marks the trace degraded so
        // the Rule Lab / API can flag that live inputs were substituted.
        degraded: i.live_readings != LiveReadings::Station,
        rules,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::conditions::{
        CmpOp, ConditionExpr, ConditionRule, Metric, RuleAction, RuleScope,
    };

    fn base() -> Inputs {
        Inputs {
            temp_now_f: 70.0,
            wind_now_mph: 3.0,
            rain_today_in: 0.0,
            rain_intensity_now_in_hr: 0.0,
            humidity_now_pct: 55.0,
            forecast_in: 0.0,
            rain_tomorrow_prob_pct: 0,
            rain_3day_weighted_in: 0.0,
            rain_7day_weighted_in: 0.0,
            rain_next_4h_in: 0.0,
            wind_max_today_mph: 6.0,
            temp_min_24h_f: Some(60.0),
            temp_max_3day_f: 80.0,
            days_since_significant_rain: 1,
            max_wind_mph: 10.0,
            min_temp_f: 38.0,
            rain_skip_in: 0.25,
            soil_zones: Vec::new(),
            soil_temp_yard_min_f: None,
            soil_temp_yard_max_f: None,
            frost_skip_soil_f: 35.0,
            live_readings: LiveReadings::Station,
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

    #[test]
    fn soil_frost_no_data_does_not_skip() {
        let mut i = base();
        i.soil_temp_yard_min_f = None;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "run");
    }

    #[test]
    fn yard_wide_saturation_does_not_skip_with_one_dry_zone() {
        let mut i = base();
        i.soil_zones = soil4(Some(72.0), Some(25.0), Some(75.0), Some(90.0));
        assert_eq!(evaluate(&i).verdict, "run");
    }

    #[test]
    fn yard_wide_saturation_does_not_skip_with_partial_data() {
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), None, Some(75.0), Some(90.0));
        assert_eq!(evaluate(&i).verdict, "run");
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
        // generic "not all zones have soil sensors".
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), None, Some(80.0), None);
        let t = decide_traced(&i, &SkipRuleParams::default());
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
        // back_yard saturated (90% >= 70 threshold) normally skips that zone.
        i.soil_zones = soil4(Some(90.0), Some(40.0), Some(40.0), Some(40.0));
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
}
