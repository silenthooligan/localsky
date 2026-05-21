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

use crate::config::schema::SkipRuleParams;
use crate::ha::snapshot::SkipCheck;

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
    pub temp_min_24h_f: f64,
    pub temp_max_3day_f: f64,
    pub days_since_significant_rain: u32,

    // ── User-tunable thresholds (HA input_number / config.engine.skip_rules) ──
    pub max_wind_mph: f64,
    pub min_temp_f: f64,
    pub rain_skip_in: f64,

    // ── Soil sensor inputs (Phase E) ──
    pub soil_back_yard_pct: Option<f64>,
    pub soil_front_yard_pct: Option<f64>,
    pub soil_side_yard_pct: Option<f64>,
    pub soil_back_yard_shrubs_pct: Option<f64>,
    pub soil_temp_yard_min_f: Option<f64>,
    pub soil_temp_yard_max_f: Option<f64>,
    pub frost_skip_soil_f: f64,
    pub saturation_back_yard_pct: f64,
    pub saturation_front_yard_pct: f64,
    pub saturation_side_yard_pct: f64,
    pub saturation_back_yard_shrubs_pct: f64,

    // ── Toggles ──
    pub is_paused: bool,
    pub is_dry_run: bool,

    // ── Phase 4 control surfaces ──
    pub pause_until_epoch: i64,
    pub now_epoch: i64,
    pub override_tomorrow: String,
    pub is_tomorrow: bool,
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
        temp_min_24h_f: i.temp_min_24h_f,
        temp_max_3day_f: i.temp_max_3day_f,
        days_since_significant_rain: i.days_since_significant_rain,
        heat_index_now_f: heat_index_now,
        heat_index_max_3day_f: heat_index_3day,

        max_wind_mph: i.max_wind_mph,
        min_temp_f: i.min_temp_f,
        rain_skip_in: i.rain_skip_in,

        soil_back_yard_pct: i.soil_back_yard_pct,
        soil_front_yard_pct: i.soil_front_yard_pct,
        soil_side_yard_pct: i.soil_side_yard_pct,
        soil_back_yard_shrubs_pct: i.soil_back_yard_shrubs_pct,
        soil_temp_yard_min_f: i.soil_temp_yard_min_f,
        soil_temp_yard_max_f: i.soil_temp_yard_max_f,
        frost_skip_soil_f: i.frost_skip_soil_f,
        saturation_back_yard_pct: i.saturation_back_yard_pct,
        saturation_front_yard_pct: i.saturation_front_yard_pct,
        saturation_side_yard_pct: i.saturation_side_yard_pct,
        saturation_back_yard_shrubs_pct: i.saturation_back_yard_shrubs_pct,

        is_paused: i.is_paused,
        is_dry_run: i.is_dry_run,

        will_skip: verdict == "skip",
        verdict: verdict.to_string(),
        reason,
    }
}

/// Rule ladder. Order matters: first matching rule wins. Order is
/// override > paused > weather > heat-advisory > dry-run > run.
fn decide(i: &Inputs, p: &SkipRuleParams) -> (&'static str, String) {
    if i.is_tomorrow {
        match i.override_tomorrow.as_str() {
            "skip" => return ("skip", "Manual override (skip tomorrow)".to_string()),
            "run" => return ("run", String::new()),
            _ => {}
        }
    }
    if i.pause_until_epoch > 0 && i.now_epoch > 0 && i.now_epoch < i.pause_until_epoch {
        let until = format_pause_until(i.pause_until_epoch);
        return ("skip", format!("Paused (vacation until {until})"));
    }
    if i.is_paused {
        return ("skip", "Paused (vacation mode)".to_string());
    }
    if i.rain_intensity_now_in_hr > p.rain_now_in_hr {
        return (
            "skip",
            format!(
                "Currently raining ({:.2} in/hr)",
                i.rain_intensity_now_in_hr
            ),
        );
    }
    if i.temp_now_f < i.min_temp_f {
        return (
            "skip",
            format!(
                "Freeze risk now ({:.0}°F < {:.0}°F)",
                i.temp_now_f, i.min_temp_f
            ),
        );
    }
    if i.temp_min_24h_f > 0.0 && i.temp_min_24h_f < i.min_temp_f {
        return (
            "skip",
            format!(
                "Overnight freeze ({:.0}°F low next 24h < {:.0}°F)",
                i.temp_min_24h_f, i.min_temp_f
            ),
        );
    }
    if let Some(t) = i.soil_temp_yard_min_f {
        if t < i.frost_skip_soil_f {
            return (
                "skip",
                format!(
                    "Soil frost ({:.1}°F < {:.0}°F threshold)",
                    t, i.frost_skip_soil_f
                ),
            );
        }
    }
    if i.wind_now_mph > i.max_wind_mph {
        return (
            "skip",
            format!(
                "Wind too high now ({:.1} mph > {:.0} mph)",
                i.wind_now_mph, i.max_wind_mph
            ),
        );
    }
    if i.wind_max_today_mph > i.max_wind_mph + p.wind_forecast_slack_mph {
        return (
            "skip",
            format!(
                "Windy day forecast (peak {:.0} mph > {:.0} + {:.0})",
                i.wind_max_today_mph, i.max_wind_mph, p.wind_forecast_slack_mph
            ),
        );
    }
    if i.rain_today_in >= p.already_wet_in {
        return (
            "skip",
            format!("Already wet ({:.2}\" today)", i.rain_today_in),
        );
    }
    let zones_sat = [
        ("back yard",        i.soil_back_yard_pct,        i.saturation_back_yard_pct),
        ("front yard",       i.soil_front_yard_pct,       i.saturation_front_yard_pct),
        ("side yard",        i.soil_side_yard_pct,        i.saturation_side_yard_pct),
        ("back yard shrubs", i.soil_back_yard_shrubs_pct, i.saturation_back_yard_shrubs_pct),
    ];
    if zones_sat.iter().all(|(_, p, _)| p.is_some())
        && zones_sat.iter().all(|(_, p, t)| p.unwrap() >= *t)
    {
        let tightest = zones_sat
            .iter()
            .min_by(|a, b| {
                let am = a.1.unwrap() - a.2;
                let bm = b.1.unwrap() - b.2;
                am.partial_cmp(&bm).unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap();
        return (
            "skip",
            format!(
                "All zones soil-saturated (tightest: {} {:.0}% ≥ {:.0}% threshold)",
                tightest.0,
                tightest.1.unwrap(),
                tightest.2
            ),
        );
    }
    if i.rain_next_4h_in >= p.rain_next_4h_skip_in {
        return (
            "skip",
            format!(
                "Rain expected within 4h ({:.2}\" forecast)",
                i.rain_next_4h_in
            ),
        );
    }
    let tomorrow_weighted = i.forecast_in * (i.rain_tomorrow_prob_pct as f64) / 100.0;
    if tomorrow_weighted >= i.rain_skip_in {
        return (
            "skip",
            format!(
                "Tomorrow rain ({:.2}\" × {}% confidence)",
                i.forecast_in, i.rain_tomorrow_prob_pct
            ),
        );
    }
    if i.rain_3day_weighted_in >= p.rain_3day_factor * i.rain_skip_in {
        return (
            "skip",
            format!(
                "Heavy rain in next 3 days ({:.2}\" weighted)",
                i.rain_3day_weighted_in
            ),
        );
    }
    if i.temp_max_3day_f >= p.heat_advisory_temp_f
        && i.humidity_now_pct >= p.heat_advisory_humidity_pct
        && i.days_since_significant_rain >= p.heat_advisory_dry_days
        && i.rain_3day_weighted_in < 0.5 * i.rain_skip_in
    {
        return (
            "run_extended",
            format!(
                "Heat advisory — running planned + 15% (peak {:.0}°F)",
                i.temp_max_3day_f
            ),
        );
    }

    if i.is_dry_run {
        return ("skip", "Dry-run mode".to_string());
    }

    ("run", String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            temp_min_24h_f: 60.0,
            temp_max_3day_f: 80.0,
            days_since_significant_rain: 1,
            max_wind_mph: 10.0,
            min_temp_f: 38.0,
            rain_skip_in: 0.25,
            soil_back_yard_pct: None,
            soil_front_yard_pct: None,
            soil_side_yard_pct: None,
            soil_back_yard_shrubs_pct: None,
            soil_temp_yard_min_f: None,
            soil_temp_yard_max_f: None,
            frost_skip_soil_f: 35.0,
            saturation_back_yard_pct: 70.0,
            saturation_front_yard_pct: 70.0,
            saturation_side_yard_pct: 70.0,
            saturation_back_yard_shrubs_pct: 85.0,
            is_paused: false,
            is_dry_run: false,
            pause_until_epoch: 0,
            now_epoch: 1_700_000_000,
            override_tomorrow: String::new(),
            is_tomorrow: false,
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
        assert_eq!(s.verdict, "run", "0.05\" should not be wet under stricter threshold");

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
        i.soil_back_yard_pct = Some(72.0);
        i.soil_front_yard_pct = Some(80.0);
        i.soil_side_yard_pct = Some(75.0);
        i.soil_back_yard_shrubs_pct = Some(90.0);
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
        i.soil_back_yard_pct = Some(72.0);
        i.soil_front_yard_pct = Some(25.0);
        i.soil_side_yard_pct = Some(75.0);
        i.soil_back_yard_shrubs_pct = Some(90.0);
        assert_eq!(evaluate(&i).verdict, "run");
    }

    #[test]
    fn yard_wide_saturation_does_not_skip_with_partial_data() {
        let mut i = base();
        i.soil_back_yard_pct = Some(80.0);
        i.soil_front_yard_pct = None;
        i.soil_side_yard_pct = Some(75.0);
        i.soil_back_yard_shrubs_pct = Some(90.0);
        assert_eq!(evaluate(&i).verdict, "run");
    }

    #[test]
    fn soil_frost_takes_priority_over_yard_saturation() {
        let mut i = base();
        i.soil_temp_yard_min_f = Some(30.0);
        i.soil_back_yard_pct = Some(80.0);
        i.soil_front_yard_pct = Some(80.0);
        i.soil_side_yard_pct = Some(80.0);
        i.soil_back_yard_shrubs_pct = Some(90.0);
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
        i.temp_min_24h_f = 32.0;
        let s = evaluate(&i);
        assert_eq!(s.verdict, "skip");
        assert!(s.reason.contains("Overnight freeze"));
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
}
