// Typed irrigation snapshot. One immutable struct rebuilt every refresh
// cycle and atomically swapped into the store. Serialized to JSON for
// the /api/irrigation/snapshot endpoint and the SSE stream, mirrors the
// tempest::Snapshot pattern exactly.

use serde::{Deserialize, Serialize};

/// Per-zone state. Fields named so the JSON keys read naturally on the
/// browser side (`zone.running`, `zone.today_run_minutes`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneState {
    /// Friendly name as shown in HA's name_by_user override (e.g.
    /// "Garage Door" for the vehicle door, "Storage Door" for the other).
    /// Falls back to the slug when no override is set.
    pub name: String,
    /// Slug used in OpenSprinkler entity IDs:
    /// `back_yard`, `front_yard`, `back_yard_shrubs`, `side_yard`.
    pub slug: String,
    /// Last six hex chars of the OpenSprinkler MAC. Stations don't have
    /// their own MACs; this is the controller's MAC, identical across
    /// all four zones. Kept on the zone for symmetry with the binary
    /// sensor entity IDs (`garage_door_opener_<hex>_*`).
    pub hex: String,
    /// True when the matching `binary_sensor.aperture_sprinklers_<slug>_station_running`
    /// is `on`.
    pub running: bool,
    /// Today's accumulated run-minutes for this zone. Sums every
    /// recorded run that started since local midnight. Populated from
    /// the SQLite history layer once Phase 3 ingest lands; zero in
    /// Phase 2.
    pub today_run_minutes: f64,
    /// Smart Irrigation deficit (mm) for this zone. Read from
    /// `sensor.smart_irrigation_<slug>` attributes.
    pub bucket_mm: f64,
    /// Per-zone duration the next IU sequence will use (seconds). Read
    /// from `sensor.smart_irrigation_<slug>` state. SI's nightly sync
    /// pushes this into IU's next-run amounts.
    pub planned_run_seconds: u32,
    /// Last-run epoch in UTC, or 0 if unknown. Populated from history
    /// in Phase 3.
    pub last_run_epoch: i64,

    /// Per-zone flex-math breakdown (Phase E followup — math transparency).
    /// Surfaces SI's internal computation: bucket × Kc × heat_mult ÷
    /// throughput ÷ capture = need_seconds, then the maximum_duration
    /// safety ceiling. Renders the dashboard's "Why this duration?" tile.
    /// `None` when SI hasn't populated yet (first boot or sensor offline).
    #[serde(default)]
    pub math: Option<ZoneMath>,

    /// Optional zone photo URL. Sourced from `zones.<slug>.photo_url` in
    /// the config; copied here by the refresher so the dashboard can
    /// render it without a separate /api/config round-trip. Accepts any
    /// relative or absolute URL the browser can load (e.g. a local
    /// `/site/photos/back_yard.jpg` or an off-site `https://...` link).
    #[serde(default)]
    pub photo_url: Option<String>,
}

/// Per-zone flex-math breakdown for the math-transparency tile. All
/// numbers pulled from `sensor.smart_irrigation_<slug>` attributes,
/// with `heat_mult` carried from the snapshot's `forecast.heat_multiplier`
/// (it's a global, not per-zone, but applies to every zone's ET
/// calculation). `capture_efficiency` is the constant LocalSky uses in
/// the Phase E water-balance projection — surfaced here so the displayed
/// math matches the projection's math.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneMath {
    /// Soil-water deficit, mm. Negative = needs water. From SI's
    /// `bucket` attribute on `sensor.smart_irrigation_<slug>`.
    pub bucket_mm: f64,
    /// Crop coefficient. Drawn from SI's `multiplier` attribute (which
    /// in this codebase is set per zone: 1.08 for turf, 0.50 for shrubs).
    pub kc: f64,
    /// Sprinkler precipitation rate, mm/hr. From SI's `throughput`
    /// attribute. Low values (~2-3 mm/hr) suggest rotors or drip;
    /// fixed sprays land around 20-40 mm/hr.
    pub throughput_mm_hr: f64,
    /// ET heat multiplier, dimensionless. Drawn from
    /// `forecast.heat_multiplier`. 1.00 at HI ≤ 85 °F, scaling to 1.30
    /// at HI ≥ 105 °F.
    pub heat_mult: f64,
    /// Effective rain/applied-water capture efficiency, 0..1. Constant
    /// 0.70 to match the Phase E water-balance model.
    pub capture_eff: f64,
    /// SI's computed need, seconds. = (|bucket_mm| / throughput_mm_hr) × 3600 × kc.
    /// What SI would ship if maximum_duration didn't cap it.
    pub raw_seconds: u32,
    /// SI's `maximum_duration` ceiling, seconds. Hard safety stop —
    /// prevents a misconfigured throughput from running for hours.
    pub max_duration_seconds: u32,
    /// What SI actually emits as `sensor.smart_irrigation_<slug>` state.
    /// Equal to `min(raw_seconds, max_duration_seconds)`. This is what
    /// the SI -> IU sync at 23:30 pushes into IU's next-run amounts.
    pub scheduled_seconds: u32,
    /// True when `raw_seconds > max_duration_seconds` (the cap is
    /// binding and the zone is being shorted). Dashboard renders the
    /// max-duration row in a warning color when this is true.
    pub cap_binding: bool,
}

/// Inputs and decision of the morning skip-check, rendered as a UI
/// breakdown. Single source of truth for the evaluation lives in
/// `skip_logic::evaluate` — both this dashboard and (Phase B) the HA
/// automation read the same verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SkipCheck {
    // ── Live readings (Tempest + HA) ──
    pub temp_now_f: f64,
    pub wind_now_mph: f64,
    pub rain_today_in: f64,
    pub rain_intensity_now_in_hr: f64,
    pub humidity_now_pct: f64,

    // ── Open-Meteo forecast inputs ──
    /// Tomorrow's rain (Open-Meteo `precipitation_sum`, today+1).
    pub forecast_in: f64,
    /// Tomorrow's max precipitation probability (0-100).
    pub rain_tomorrow_prob_pct: u32,
    /// Σ daily[1..4] precip × prob/100 — probability-weighted 3-day rollup.
    pub rain_3day_weighted_in: f64,
    /// Σ daily[1..7] precip × prob/100 — probability-weighted 7-day rollup.
    pub rain_7day_weighted_in: f64,
    /// Σ hourly[0..4] precip — total expected rain in the next 4 hours.
    pub rain_next_4h_in: f64,
    /// Today's forecast peak wind (Open-Meteo daily[0]).
    pub wind_max_today_mph: f64,
    /// Min hourly forecast temperature for the next 24h (overnight low).
    pub temp_min_24h_f: f64,
    /// Max forecast daily-high temperature across today + next 2 days.
    pub temp_max_3day_f: f64,
    /// Days since the last day with ≥ 0.05" rain (today included).
    /// 0 = wet today; saturates at past_daily window + 1.
    pub days_since_significant_rain: u32,
    /// Heat index now (NOAA Steadman), used as input to the heat advisory.
    pub heat_index_now_f: f64,
    /// Heat index for the 3-day forecast peak — drives the advisory rule
    /// that pre-waters before a multi-day heat wave.
    pub heat_index_max_3day_f: f64,

    // ── User-tunable thresholds (HA input_number helpers) ──
    pub max_wind_mph: f64,
    pub min_temp_f: f64,
    pub rain_skip_in: f64,

    // ── Soil sensor inputs (Phase E) ──
    /// Per-zone calibrated soil moisture %, from HA template sensors
    /// (raw FDR AD via ecowitt2mqtt, mapped between operator-captured
    /// dry/wet endpoints). `None` when the probe is unavailable (radio
    /// dropout, gateway offline, calibration not yet performed).
    pub soil_back_yard_pct: Option<f64>,
    pub soil_front_yard_pct: Option<f64>,
    pub soil_side_yard_pct: Option<f64>,
    pub soil_back_yard_shrubs_pct: Option<f64>,
    /// Yard-wide soil temperature aggregates (min/max across the 4 zones).
    /// Min drives the soil-frost gate; max is informational.
    pub soil_temp_yard_min_f: Option<f64>,
    pub soil_temp_yard_max_f: Option<f64>,
    /// Soil-frost skip threshold (°F). Below this, suspend the morning run.
    /// Pulled from input_number.irrigation_frost_skip_f (default 35.0).
    pub frost_skip_soil_f: f64,
    /// Per-zone saturation thresholds (%). When ALL four zones are at or
    /// above their threshold, the engine returns a yard-wide skip. Per-zone
    /// gating (one wet zone, others dry) is handled by HA-side automations
    /// calling irrigation_unlimited.adjust_time directly — LocalSky's
    /// verdict is sequence-level all-or-nothing.
    pub saturation_back_yard_pct: f64,
    pub saturation_front_yard_pct: f64,
    pub saturation_side_yard_pct: f64,
    pub saturation_back_yard_shrubs_pct: f64,

    // ── Toggles ──
    pub is_paused: bool,
    pub is_dry_run: bool,

    // ── Decision ──
    /// `true` if any condition trips and the morning run will skip.
    pub will_skip: bool,
    /// Verdict tag: "skip" / "run" / "run_extended". The HA REST sensor
    /// surfaces this directly so the morning automation can branch.
    pub verdict: String,
    /// Human-readable reason. Empty when `verdict == "run"`.
    pub reason: String,
}

/// Live + forecast weather context. The dashboard surfaces both
/// sources so the user can see when Tempest's local gauge disagrees
/// with Open-Meteo's regional model — and the skip-check uses the
/// LARGER of the two for `rain_today_in` so a stuck Tempest gauge
/// can't mask actual rain.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    /// Tempest's local rain gauge accumulated total since midnight, in
    /// inches. Tempest reports in inches when HA is in imperial mode.
    pub rain_today_tempest_in: f64,
    /// Open-Meteo's regional model accumulated total today, in inches.
    /// Falls back here when the Tempest gauge stays at 0 during a
    /// real rain event (haptic-sensor misses can happen).
    pub rain_today_om_in: f64,
    /// Tempest live rain rate, in/hr. Drives the "RAINING NOW" badge.
    pub rain_intensity_in_hr: f64,
    /// Tempest precipitation type: "none" / "rain" / "hail".
    pub rain_type: String,
    /// Open-Meteo forecast for tomorrow, in inches.
    pub rain_tomorrow_in: f64,
    /// Open-Meteo 3-day rolling rain forecast (today + next 2), in.
    pub rain_3day_in: f64,
    /// Reference evapotranspiration for the day. mm (FAO-56 ET₀).
    pub eto_today_mm: f64,
    /// Same for tomorrow.
    pub eto_tomorrow_mm: f64,
    /// 3-day average ET₀ used by Smart Irrigation's Passthrough module.
    pub eto_3day_avg_mm: f64,
    pub temp_max_today_f: f64,
    pub temp_min_today_f: f64,
    pub wind_max_today_mph: f64,
    pub humidity_mean_today_pct: f64,

    // ── Forecast intelligence (Phase A) ──
    /// Probability-weighted 3-day rain (today + next 2), in.
    pub rain_3day_weighted_in: f64,
    /// Probability-weighted 7-day rain (today + next 6), in.
    pub rain_7day_weighted_in: f64,
    /// Sum of expected precipitation for the next 4 hours, in.
    pub rain_next_4h_in: f64,
    /// Tomorrow's max precipitation probability (0-100).
    pub rain_tomorrow_prob_pct: u32,
    /// Min temperature in the next 24 hourly forecast entries, °F.
    pub temp_min_24h_f: f64,
    /// Max daily-high across today + next 2 days, °F.
    pub temp_max_3day_f: f64,
    /// Live humidity (Tempest), used by the heat-index calc.
    pub humidity_now_pct: f64,
    /// Heat index now (Tempest temp + humidity, NOAA Steadman).
    pub heat_index_now_f: f64,
    /// Heat index at the 3-day forecast peak (max temp + current humidity
    /// as a stand-in for forecast humidity, which Open-Meteo's daily
    /// rollup doesn't expose directly).
    pub heat_index_max_3day_f: f64,
    /// ET multiplier derived from heat_index_max_3day_f. 1.00 when no
    /// heat stress, scaling up to 1.30 at HI 105+. Phase C feeds this
    /// into Smart Irrigation's per-zone Kc.
    pub heat_multiplier: f64,
    /// Days since last ≥ 0.05" rain day (heat-advisory input).
    pub days_since_significant_rain: u32,
}

/// One day in the 7-day forward verdict strip. Result of running the
/// skip-check engine against synthetic Inputs derived from each future
/// day's forecast — gives the user an at-a-glance preview of which
/// days are predicted to run, skip, or trigger heat-advisory pre-water.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayVerdict {
    /// 0 = today, 1 = tomorrow, ..., 6 = day +6.
    pub day_offset: u32,
    /// UTC epoch (00:00 local in the user's timezone).
    pub time_epoch: i64,
    /// WMO weather code for the day's dominant condition.
    pub weather_code: u32,
    pub temp_max_f: f64,
    pub temp_min_f: f64,
    /// Forecast daily precipitation total, in.
    pub precip_in: f64,
    /// Max precipitation probability for the day, 0-100.
    pub precip_probability_max: u32,
    /// "skip" / "run" / "run_extended".
    pub verdict: String,
    /// Human-readable reason; empty on plain "run".
    pub reason: String,
}

/// Per-zone 7-day soil-moisture projection (Phase E predictive). Built
/// from a simple FAO-56-flavored water-balance model: subtract daily
/// ET, add captured rain, no irrigation. The user reads this as "if I
/// did nothing all week, would each zone stay in its healthy band?"
///
/// Predicted % is informational only — no new skip rules fire on it.
/// The dashboard's job is to make the trajectory visible so the user
/// can tune thresholds or queue manual runs ahead of dry stretches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SoilForecast {
    /// Slug used in entity ids (`back_yard`, `front_yard`, etc.).
    pub zone_slug: String,
    /// Friendly name for the UI.
    pub zone_name: String,
    /// Current calibrated moisture %, today, from
    /// `sensor.<zone>_soil_moisture`. `None` when the probe is offline.
    pub current_pct: Option<f64>,
    /// Lower bound of the healthy band — at-or-below this prediction
    /// for any of the next 3 days, the dashboard surfaces a "dry" badge.
    /// Pulled from `input_number.irrigation_<zone>_target_min_pct`.
    pub target_min_pct: f64,
    /// Upper bound = the existing saturation threshold (we don't want
    /// the model to "aim" above this). Just for plotting the target band.
    pub target_max_pct: f64,
    /// 7-day predicted moisture %. Index 0 = today (the live reading),
    /// then `today + N` for N in 1..=6. Each step:
    /// `next = prev - (et_mm × Kc) / soil_depth_mm × 100 + (rain_mm × CAPTURE) / soil_depth_mm × 100`,
    /// clamped 0..100. Excludes any irrigation we might run — this is
    /// the "no-water baseline" projection.
    pub predicted_pct: Vec<f64>,
    /// Min predicted % across the 7-day window (for at-a-glance "will
    /// this zone go dry?" tile coloring).
    pub min_predicted_pct: f64,
    /// Max predicted %, same window.
    pub max_predicted_pct: f64,
    /// Days within the window predicted at or below `target_min_pct`.
    pub days_below_target: u32,
    /// Days within the window predicted above `target_max_pct` (over-
    /// saturation — usually triggered by heavy forecast rain).
    pub days_above_max: u32,
    /// At-a-glance status: "dry" | "ok" | "wet" | "no_data".
    /// - "dry": min_predicted_pct <= target_min_pct OR days_below_target >= 2
    /// - "wet": max_predicted_pct >= target_max_pct
    /// - "ok": in band for the full window
    /// - "no_data": current_pct is None (probe offline)
    pub status: String,
}

/// Phase H — weekly water-budget plan per zone. When the operator flips
/// `input_boolean.irrigation_<zone>_weekly_budget_mode` to `on`, the HA
/// budget-override automation at 23:30:25 reads `today_seconds` from
/// here and calls `irrigation_unlimited.adjust_time(actual=...)` to
/// replace SI's daily-bucket value. The model targets a healthy moisture
/// band for warm-season grass (St. Augustine FL) by allocating a weekly
/// water budget across `sessions_per_week` days, subtracting
/// probability-weighted forecast rain, and skipping today if rain is
/// imminent or the last run was recent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WaterBudget {
    pub zone_slug: String,
    pub zone_name: String,
    /// True when this zone is in budget mode (HA helper). False = SI's
    /// daily flex math owns the schedule and budget data here is
    /// informational only.
    pub mode_active: bool,
    /// Operator-tunable weekly water target in inches (HA input_number).
    pub weekly_budget_in: f64,
    /// Operator-tunable session count per week (HA input_number,
    /// typical 1-3). Determines per-session depth.
    pub sessions_per_week: u32,
    /// Probability-weighted forecast rain over the next 7 days, mm.
    /// Net irrigation need is reduced by this × capture efficiency.
    pub expected_rain_mm: f64,
    /// Net irrigation depth needed this week (mm), after rain credit.
    pub needed_mm: f64,
    /// Per-session depth (mm) and run-time (seconds at the zone's
    /// throughput). seconds_per_session = (mm / throughput) × 3600 ×
    /// heat_multiplier, scaled up by 1/capture_efficiency to deliver
    /// the target depth at the root after runoff.
    pub mm_per_session: f64,
    pub seconds_per_session: u32,
    /// True if seconds_per_session exceeded the zone's maximum_duration
    /// — single-session can't deliver the target depth. Operator can
    /// raise sessions_per_week, raise SI's max_duration, or accept it.
    pub session_capped: bool,
    /// Epoch of the most recent run for this zone (from SQLite history),
    /// or 0 if no run on record.
    pub last_run_epoch: i64,
    /// Today's recommendation. `seconds == 0` means "don't run today";
    /// reason explains why (rain forecast, last run recent, mode off).
    pub today_seconds: u32,
    pub today_reason: String,
}

/// Top-level snapshot for the irrigation page. Cheap to clone (`Arc`-
/// wrapped before any client touches it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IrrigationSnapshot {
    /// UTC epoch of the most recent successful HA poll. 0 if we've never
    /// successfully refreshed.
    pub last_refresh_epoch: i64,
    /// True when the most recent poll completed without error.
    pub ha_reachable: bool,

    /// IU sequence's next scheduled fire time. UTC epoch.
    pub next_run_epoch: i64,
    /// Total minutes the next sequence will run if not skipped.
    pub next_run_total_minutes: f64,
    /// Master controller enable (`switch.aperture_sprinklers_enabled`).
    pub master_enable: bool,
    /// IU sequence enabled in YAML.
    pub iu_enabled: bool,
    /// IU sequence currently suspended (skip-check fired or manual).
    pub iu_suspended: bool,
    /// OpenSprinkler controller `water_level` (0-250%, where 100% is
    /// the SI baseline).
    pub water_level_pct: f64,

    pub zones: Vec<ZoneState>,
    pub skip_check: SkipCheck,
    pub forecast: Forecast,

    /// 7-day forward verdict strip — predicted skip/run for today + 6
    /// future days. Computed server-side by running `skip_logic::evaluate`
    /// against synthetic Inputs from each daily forecast entry.
    pub seven_day_verdicts: Vec<DayVerdict>,

    /// Per-zone soil-moisture projections (Phase E predictive). One
    /// entry per WH52 zone, holding the 7-day "no irrigation" baseline
    /// + the target band so the dashboard can show whether each zone
    /// stays healthy on rain + ET alone.
    #[serde(default)]
    pub soil_forecasts: Vec<SoilForecast>,

    /// Per-zone weekly water budgets (Phase H). One entry per zone with
    /// the budget plan + today's recommendation. HA's
    /// localsky_weekly_budget_override automation reads `today_seconds`
    /// for zones with `mode_active == true` and overrides SI's value
    /// via `irrigation_unlimited.adjust_time` at 23:30:25.
    #[serde(default)]
    pub water_budgets: Vec<WaterBudget>,

    /// Vacation pause expiry, UTC epoch seconds. 0 means not set / not paused.
    /// Read from `input_datetime.irrigation_pause_until` (a manually-created
    /// HA helper). When `now < pause_until_epoch` skip_logic short-circuits
    /// to "skip" with reason "Vacation until ...". The helper attribute is
    /// the canonical source of truth; HA's midnight check is unnecessary
    /// because the comparison is direct against current time.
    #[serde(default)]
    pub pause_until_epoch: i64,
    /// One-day override for tomorrow's verdict. "none" | "skip" | "run".
    /// Read from `input_select.irrigation_override_tomorrow`. An HA midnight
    /// automation should reset this to "none" each day so the override is a
    /// one-day knob, not a permanent override. When the snapshot is missing
    /// this entity (helper not created), the field stays "none".
    #[serde(default)]
    pub override_tomorrow: String,
    /// True when the input_datetime + input_select helpers exist in HA. Lets
    /// the UI hide the controls with an "(HA helper not configured)" hint
    /// instead of failing silently when the user taps them.
    #[serde(default)]
    pub override_helpers_present: bool,
}
