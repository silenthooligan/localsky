// 7-day forward verdict projection. For each daily forecast entry
// (today + 6 future days), construct synthetic Inputs and run the same
// skip-rule ladder the morning skip-check uses. Same engine, same rules
// -- this is a preview of the actual decision, not a separate heuristic.
//
// Phase 3E extraction from src/ha/refresher.rs::compute_seven_day_verdicts.
// Pure function: takes the merged forecast + today's thresholds and
// returns Vec<DayVerdict>. HA-entity reading stays in refresher.rs.

use crate::config::schema::SkipRuleParams;
use crate::engine::skip_rules::{evaluate_with, Inputs};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::ha::snapshot::DayVerdict;

/// Compute the 7-day verdict strip. `today` carries forward the
/// threshold values + override + pause state; the synthetic per-day
/// Inputs reset live signals (wind_now, rain_intensity_now, etc.) since
/// the strip projects conditions rather than replaying live data.
pub fn compute(fc: &ForecastSnapshot, today: &Inputs, params: &SkipRuleParams) -> Vec<DayVerdict> {
    if fc.daily.is_empty() {
        return Vec::new();
    }
    let n_days = fc.daily.len().min(7);
    let mut out = Vec::with_capacity(n_days);

    for day_idx in 0..n_days {
        let d = &fc.daily[day_idx];
        let next = fc.daily.get(day_idx + 1);

        let prob_tomorrow = next.map(|n| n.precip_probability_max).unwrap_or(0);
        let precip_tomorrow = next.map(|n| n.precip_sum_in).unwrap_or(0.0);

        let rain_3day_weighted: f64 = fc
            .daily
            .iter()
            .skip(day_idx + 1)
            .take(3)
            .map(|x| x.precip_sum_in * (x.precip_probability_max as f64) / 100.0)
            .sum();
        let rain_7day_weighted: f64 = fc
            .daily
            .iter()
            .skip(day_idx + 1)
            .take(7)
            .map(|x| x.precip_sum_in * (x.precip_probability_max as f64) / 100.0)
            .sum();

        let temp_max_3day = fc
            .daily
            .iter()
            .skip(day_idx)
            .take(3)
            .map(|x| x.temp_max_f)
            .fold(f64::NEG_INFINITY, f64::max);
        let temp_max_3day = if temp_max_3day.is_finite() {
            temp_max_3day
        } else {
            d.temp_max_f
        };

        // days_since_significant_rain (forward): scan past days within
        // the window we've already simulated; fall back to past_daily.
        let mut days_since = 0u32;
        if d.precip_sum_in < 0.05 {
            let mut found = false;
            for back in 1..=day_idx {
                if fc.daily[day_idx - back].precip_sum_in >= 0.05 {
                    days_since = back as u32;
                    found = true;
                    break;
                }
            }
            if !found {
                let mut acc = day_idx as u32;
                for past in fc.past_daily.iter().rev() {
                    acc += 1;
                    if past.precip_sum_in >= 0.05 {
                        days_since = acc;
                        found = true;
                        break;
                    }
                }
                if !found {
                    days_since = (fc.past_daily.len() as u32 + day_idx as u32).saturating_add(1);
                }
            }
        }

        let inputs = Inputs {
            temp_now_f: d.temp_min_f,
            wind_now_mph: 0.0,
            rain_today_in: d.precip_sum_in,
            rain_intensity_now_in_hr: 0.0,
            humidity_now_pct: today.humidity_now_pct,

            forecast_in: precip_tomorrow,
            rain_tomorrow_prob_pct: prob_tomorrow,
            rain_3day_weighted_in: rain_3day_weighted,
            rain_7day_weighted_in: rain_7day_weighted,
            rain_next_4h_in: 0.0,
            wind_max_today_mph: d.wind_max_mph,
            temp_min_24h_f: Some(d.temp_min_f),
            temp_max_3day_f: temp_max_3day,
            days_since_significant_rain: days_since,

            max_wind_mph: today.max_wind_mph,
            min_temp_f: today.min_temp_f,
            rain_skip_in: today.rain_skip_in,

            // The 7-day forward strip models weather only, not per-zone
            // soil (we have no soil forecast per future day).
            soil_zones: Vec::new(),
            soil_temp_yard_min_f: None,
            soil_temp_yard_max_f: None,
            frost_skip_soil_f: today.frost_skip_soil_f,
            // The strip cells are forecast projections by construction;
            // the live-data integrity gate is a today-only concern.
            live_readings: Default::default(),
            is_paused: today.is_paused,
            is_dry_run: false,

            pause_until_epoch: today.pause_until_epoch,
            // Each day's synthetic Inputs has to carry that day's epoch so
            // the restriction evaluator (which converts now_epoch ->
            // DateTime<Local> -> .weekday() / .month()) gates the right
            // weekday for the right cell. Reusing today's epoch made the
            // 7-day strip evaluate every day as if it were today, so a
            // restriction that blocked Wed never showed up on Wed's cell
            // unless today was already Wed.
            now_epoch: d.time_epoch,
            override_tomorrow: today.override_tomorrow.clone(),
            is_tomorrow: day_idx == 1,
            // Sticky overrides are persistent, so every forward day inherits
            // them (the strip models weather-only, but the global override
            // still binds each cell's verdict via pre_soil).
            global_override: today.global_override.clone(),
            zone_overrides: today.zone_overrides.clone(),

            // Phase C: forward-project the restriction set; address parity
            // is a deployment property that doesn't change day-to-day.
            watering_restrictions: today.watering_restrictions.clone(),
            address_parity: today.address_parity,
        };
        let s = evaluate_with(&inputs, params);

        out.push(DayVerdict {
            day_offset: day_idx as u32,
            time_epoch: d.time_epoch,
            weather_code: d.weather_code,
            temp_max_f: d.temp_max_f,
            temp_min_f: d.temp_min_f,
            precip_in: d.precip_sum_in,
            precip_probability_max: d.precip_probability_max,
            verdict: s.verdict,
            reason: s.reason,
        });
    }
    out
}
