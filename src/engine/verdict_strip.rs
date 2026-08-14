// 7-day forward verdict projection. For each daily forecast entry
// (today + 6 future days), construct synthetic Inputs and run the same
// skip-rule ladder the morning skip-check uses. Same engine, same rules
// -- this is a preview of the actual decision, not a separate heuristic.
//
// Phase 3E extraction from src/refresher.rs::compute_seven_day_verdicts.
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

        // None (no window / provider gap) stays None: the engine weights the
        // amount at full value and the cell omits the confidence claim.
        let prob_tomorrow = next.and_then(|n| n.precip_probability_max);
        let precip_tomorrow = next.map(|n| n.precip_sum_in).unwrap_or(0.0);

        // Probability-less days weight at full value (DailyEntry::precip_weight).
        let rain_3day_weighted: f64 = fc
            .daily
            .iter()
            .skip(day_idx + 1)
            .take(3)
            .map(|x| x.precip_sum_in * x.precip_weight())
            .sum();
        let rain_7day_weighted: f64 = fc
            .daily
            .iter()
            .skip(day_idx + 1)
            .take(7)
            .map(|x| x.precip_sum_in * x.precip_weight())
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

        // Per-day 3-day peak heat index for this cell's window: each day's high
        // temp paired with THAT day's humidity (never the saturated "now"). 0
        // when the window has no derived daily humidity; the heat-advisory rule
        // keys on temp_max_3day_f, so the verdict is unaffected either way.
        let heat_index_max_3day = fc
            .daily
            .iter()
            .skip(day_idx)
            .take(3)
            .filter(|x| x.humidity_pct > 0)
            .map(|x| crate::engine::skip_rules::heat_index_f(x.temp_max_f, x.humidity_pct as f64))
            .fold(0.0_f64, f64::max);

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

        // Observed-recent rain for this projected day: the day's own precip plus
        // the prior `rain_observed_window_days` of already-known precip (earlier
        // forecast days, then past_daily). Mirrors the refresher's observed total
        // (today + window) so the strip previews the same observed-rain gate.
        let observed_recent = {
            let window = params.rain_observed_window_days as usize;
            // For TODAY's own cell (day_idx 0) the day-back loop below is empty
            // (1..=0), so the base must itself fold in the MEASURED rain to date:
            // mirror the refresher's rain_today_used = max(station, model) so a
            // real afternoon downpour today shows as a skip on today's cell, not
            // just tomorrow's. Future cells have no measurement, so they stay on
            // the forecast precip.
            let mut acc = if day_idx == 0 {
                d.precip_sum_in.max(today.rain_today_in)
            } else {
                d.precip_sum_in
            };
            let mut taken = 0usize;
            // Walk back through already-simulated forecast days first.
            for back in 1..=day_idx {
                if taken >= window {
                    break;
                }
                let di = day_idx - back;
                // For TODAY (the most recent ACTUAL day, index 0) use the MEASURED
                // rain to date, not the forecast. Rain that actually fell this
                // afternoon (rain_today_in) must carry into tomorrow morning's
                // observed-rain skip; the pure-forecast look-back missed it, so the
                // engine would water ground that just got real rain the day before.
                // Future forecast days stay forecast (we have no measurement yet).
                acc += if di == 0 {
                    today.rain_today_in
                } else {
                    fc.daily[di].precip_sum_in
                };
                taken += 1;
            }
            // Then spill into observed past_daily (latest→earliest).
            for past in fc.past_daily.iter().rev() {
                if taken >= window {
                    break;
                }
                acc += past.precip_sum_in;
                taken += 1;
            }
            // TODAY's cell: never claim LESS observed-recent rain than the live
            // engine's own gauge-informed value (which maxes the station gauge
            // history against the model's past_daily). The strip only sees the
            // model archive here, so after hyperlocal rain the model missed, the
            // day-0 cell would read RUN while the morning actually skipped on
            // observed rain, contradicting "same engine as the morning check"
            // right above it. Future cells have no live value to anchor to.
            if day_idx == 0 {
                acc = acc.max(today.rain_observed_recent_in);
            }
            acc
        };

        let inputs = Inputs {
            temp_now_f: d.temp_min_f,
            wind_now_mph: 0.0,
            rain_today_in: d.precip_sum_in,
            rain_intensity_now_in_hr: 0.0,
            // The 7-day strip projects forecast weather with no live current-rain
            // reading (rate 0), so the rain_now gate never fires here; the nature
            // is the honest Model default regardless.
            rain_nature: crate::ha::snapshot::RainNature::default(),
            // The 7-day strip is a forward projection on the current forecast;
            // live-staleness gating belongs to the refresher's today decision.
            forecast_stale: false,
            humidity_now_pct: today.humidity_now_pct,

            forecast_in: precip_tomorrow,
            rain_tomorrow_prob_pct: prob_tomorrow,
            rain_3day_weighted_in: rain_3day_weighted,
            rain_7day_weighted_in: rain_7day_weighted,
            rain_next_4h_in: 0.0,
            rain_observed_recent_in: observed_recent,
            wind_max_today_mph: d.wind_max_mph,
            temp_min_24h_f: Some(d.temp_min_f),
            temp_max_3day_f: temp_max_3day,
            heat_index_max_3day_f: heat_index_max_3day,
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
            // P1 (units architecture): copy the engine's per-day firing rule id
            // straight off the SkipCheck the strip just ran. Additive + invisible.
            reason_code: s.reason_code,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::SkipRuleParams;
    use crate::engine::skip_rules::Inputs;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot};

    /// A mild, dry forecast day no rule fires on by itself.
    fn mild_day() -> DailyEntry {
        DailyEntry {
            temp_max_f: 72.0,
            temp_min_f: 55.0,
            precip_sum_in: 0.0,
            precip_probability_max: Some(0),
            ..Default::default()
        }
    }

    /// Baseline `today` Inputs matching the existing carry-forward test:
    /// the user rain threshold + a benign overnight low, everything else
    /// default.
    fn base_inputs() -> Inputs {
        Inputs {
            rain_skip_in: 0.25,
            temp_min_24h_f: Some(55.0),
            ..Default::default()
        }
    }

    fn default_params() -> SkipRuleParams {
        serde_json::from_str("{}").expect("default skip params")
    }

    // Rain that ACTUALLY fell today (measured) must carry into TOMORROW's skip
    // decision. Before the fix, tomorrow's observed-rain look-back summed today's
    // FORECAST rain, not the measured value, so a real 0.36in afternoon downpour
    // never suppressed the next morning's run. Control: same mild, dry forecast;
    // only the measured rain_today_in differs, so it is provably the cause.
    #[test]
    fn measured_rain_today_carries_into_tomorrow_skip() {
        let mild = DailyEntry {
            temp_max_f: 72.0,
            temp_min_f: 55.0,
            precip_sum_in: 0.0,
            precip_probability_max: Some(0),
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            daily: vec![mild.clone(), mild.clone(), mild.clone()],
            ..Default::default()
        };
        let params: SkipRuleParams = serde_json::from_str("{}").expect("default skip params");
        let base = Inputs {
            rain_skip_in: 0.25,
            temp_min_24h_f: Some(55.0),
            ..Default::default()
        };

        // No measured rain -> tomorrow RUNS (nothing suppresses it).
        let v_dry = compute(
            &fc,
            &Inputs {
                rain_today_in: 0.0,
                ..base.clone()
            },
            &params,
        );
        let tomo_dry = v_dry.iter().find(|v| v.day_offset == 1).expect("tomorrow");

        // 0.36in measured today (> 0.25 threshold) -> tomorrow SKIPS.
        let v_wet = compute(
            &fc,
            &Inputs {
                rain_today_in: 0.36,
                ..base
            },
            &params,
        );
        let tomo_wet = v_wet.iter().find(|v| v.day_offset == 1).expect("tomorrow");

        assert_eq!(
            tomo_dry.verdict, "run",
            "control: tomorrow should run with no measured rain, got {} ({})",
            tomo_dry.verdict, tomo_dry.reason
        );
        assert_eq!(
            tomo_wet.verdict, "skip",
            "tomorrow should skip after 0.36in measured today, got {} ({})",
            tomo_wet.verdict, tomo_wet.reason
        );
    }

    /// A missing forecast produces an empty strip, never a panic or a
    /// fabricated cell.
    #[test]
    fn empty_forecast_yields_empty_strip() {
        let fc = ForecastSnapshot::default();
        assert!(fc.daily.is_empty(), "default snapshot has no daily entries");
        let v = compute(&fc, &base_inputs(), &default_params());
        assert!(v.is_empty(), "no forecast days -> no strip cells");
    }

    /// The strip length tracks the forecast: shorter than 7 days yields one
    /// cell per day (contiguous day_offset + per-day epochs), longer is
    /// capped at 7.
    #[test]
    fn strip_length_tracks_daily_len_capped_at_seven() {
        let params = default_params();

        // 3-day forecast -> exactly 3 cells, offsets 0..=2, epochs copied.
        let fc = ForecastSnapshot {
            daily: (0..3i64)
                .map(|i| DailyEntry {
                    time_epoch: 1_700_000_000 + i * 86_400,
                    ..mild_day()
                })
                .collect(),
            ..Default::default()
        };
        let v = compute(&fc, &base_inputs(), &params);
        assert_eq!(v.len(), 3, "3 forecast days -> 3 cells");
        assert_eq!(
            v.iter().map(|d| d.day_offset).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(v[2].time_epoch, 1_700_000_000 + 2 * 86_400);
        assert!(v.iter().all(|d| d.verdict == "run"), "every mild cell runs");

        // 10-day forecast -> capped at 7 cells.
        let fc = ForecastSnapshot {
            daily: vec![mild_day(); 10],
            ..Default::default()
        };
        let v = compute(&fc, &base_inputs(), &params);
        assert_eq!(v.len(), 7, "strip is capped at 7 cells");
    }

    /// window=2: measured rain today ACCUMULATES with yesterday's observed
    /// past_daily rain into tomorrow's cell (neither day alone clears the
    /// 0.25in threshold), and the carry EXPIRES once a later cell's
    /// look-back window slides past the measured days.
    #[test]
    fn multi_day_observed_accumulation_window_two_and_boundary() {
        let fc = ForecastSnapshot {
            daily: vec![mild_day(); 4],
            // Yesterday's OBSERVED rain (past_daily is stored earliest ->
            // latest; a single entry IS yesterday).
            past_daily: vec![DailyEntry {
                precip_sum_in: 0.15,
                ..mild_day()
            }],
            ..Default::default()
        };
        let params: SkipRuleParams =
            serde_json::from_str(r#"{ "rain_observed_window_days": 2 }"#).unwrap();
        let today = Inputs {
            rain_today_in: 0.15,
            ..base_inputs()
        };

        let v = compute(&fc, &today, &params);
        // Tomorrow looks back 2 days: measured today 0.15 + past_daily
        // yesterday 0.15 = 0.30 >= 0.25 -> observed-rain skip.
        let d1 = v.iter().find(|d| d.day_offset == 1).expect("day 1");
        assert_eq!(
            d1.verdict, "skip",
            "tomorrow must skip on ACCUMULATED observed rain, got {} ({})",
            d1.verdict, d1.reason
        );
        assert_eq!(d1.reason_code, "observed_rain");

        // Day 2's two back-days are forecast day 1 (0.0) + measured today
        // (0.15); yesterday's past_daily rain is past the window: 0.15 <
        // 0.25 -> run. The carry expires across the window boundary.
        let d2 = v.iter().find(|d| d.day_offset == 2).expect("day 2");
        assert_eq!(
            d2.verdict, "run",
            "day 2 is past the observed window, got {} ({})",
            d2.verdict, d2.reason
        );

        // Control: with the DEFAULT window=1 tomorrow sees only today's
        // 0.15 (no past_daily spill), so nothing fires: window=2 is
        // provably what accumulated above.
        let v1 = compute(&fc, &today, &default_params());
        let d1 = v1.iter().find(|d| d.day_offset == 1).expect("day 1");
        assert_eq!(
            d1.verdict, "run",
            "window=1 control must run, got {} ({})",
            d1.verdict, d1.reason
        );
    }

    /// The day-0 cell folds the MEASURED rain-to-date into its own observed
    /// total (max(forecast, measured), mirroring the refresher's
    /// rain_today_used), so after a real 0.36in downpour on a dry-forecast day
    /// today's strip cell skips exactly like the live engine, and tomorrow's
    /// cell carries the measurement too. (Previously the day-0 look-back was the
    /// empty range 1..=0, so today's cell used forecast precip only and diverged
    /// from the live verdict.)
    #[test]
    fn day_zero_cell_reflects_measured_rain_today() {
        let fc = ForecastSnapshot {
            daily: vec![mild_day(); 3],
            ..Default::default()
        };
        let params = default_params();
        let today = Inputs {
            rain_today_in: 0.36,
            ..base_inputs()
        };

        let v = compute(&fc, &today, &params);
        let d0 = v.iter().find(|d| d.day_offset == 0).expect("day 0");
        let d1 = v.iter().find(|d| d.day_offset == 1).expect("day 1");
        assert_eq!(
            d1.verdict, "skip",
            "tomorrow carries the measured rain (the carry-forward fix)"
        );

        // The live engine, fed the same measured rain, skips today.
        let live = evaluate_with(
            &Inputs {
                rain_today_in: 0.36,
                rain_observed_recent_in: 0.36,
                ..base_inputs()
            },
            &params,
        );
        assert_eq!(live.verdict, "skip", "live verdict skips on 0.36in today");

        // Today's strip cell now agrees with the live verdict.
        assert_eq!(
            d0.verdict, "skip",
            "today's strip cell reflects measured rain, got {} ({})",
            d0.verdict, d0.reason
        );
    }

    /// TODAY's cell anchors to the live engine's gauge-informed observed-recent
    /// value: hyperlocal rain YESTERDAY that the model's past_daily missed (the
    /// gauge recorded it; the model archive shows ~0) made the morning skip on
    /// observed rain, so the day-0 cell must not read RUN under the header
    /// "same engine as the morning check".
    #[test]
    fn day_zero_cell_anchors_to_gauge_informed_observed_recent() {
        // Dry forecast, empty model archive (a non-Open-Meteo provider, or the
        // model simply missed the pop-up storm).
        let fc = ForecastSnapshot {
            daily: vec![mild_day(); 3],
            ..Default::default()
        };
        let params = default_params();
        // The refresher's gauge-informed window value: 0.36in fell YESTERDAY on
        // the yard's own gauge (rain_today is 0.0, it is a new dry day).
        let today = Inputs {
            rain_today_in: 0.0,
            rain_observed_recent_in: 0.36,
            ..base_inputs()
        };

        let v = compute(&fc, &today, &params);
        let d0 = v.iter().find(|d| d.day_offset == 0).expect("day 0");
        assert_eq!(
            d0.verdict, "skip",
            "day-0 cell must match the gauge-informed live gate, got {} ({})",
            d0.verdict, d0.reason
        );
        // The anchor is day-0 only: future cells stay on the forecast (the
        // look-back carry uses the forecast/model chain as before, and this
        // 0.36 from yesterday is outside tomorrow's window=1 look-back, which
        // sees only today's 0.0).
        let d2 = v.iter().find(|d| d.day_offset == 2).expect("day 2");
        assert_eq!(d2.verdict, "run", "future dry cells unaffected");
    }

    /// days_since_significant_rain falls back through past_daily when no
    /// already-simulated forecast day was wet: a wet day 3 past-days back
    /// leaves a dry streak long enough for the heat-advisory rule
    /// (run_extended), while rain just yesterday resets the streak and the
    /// same hot cell is a plain run.
    #[test]
    fn days_since_rain_falls_back_through_past_daily_for_dry_streak_rule() {
        let hot = DailyEntry {
            temp_max_f: 98.0,
            temp_min_f: 74.0,
            ..mild_day()
        };
        let dry = mild_day();
        // Significant (>= 0.05) but under every observed-skip threshold, so
        // ONLY the streak arithmetic distinguishes the two arrangements.
        let wet = DailyEntry {
            precip_sum_in: 0.10,
            ..mild_day()
        };
        let params = default_params();
        let today = Inputs {
            humidity_now_pct: 70.0,
            temp_min_24h_f: Some(74.0),
            ..base_inputs()
        };

        // Wet 3 days back (earliest), then two dry days -> streak = 3 >= 2.
        let fc = ForecastSnapshot {
            daily: vec![hot.clone(), mild_day(), mild_day()],
            past_daily: vec![wet.clone(), dry.clone(), dry.clone()],
            ..Default::default()
        };
        let v = compute(&fc, &today, &params);
        let d0 = v.iter().find(|d| d.day_offset == 0).expect("day 0");
        assert_eq!(
            d0.verdict, "run_extended",
            "dry streak via past_daily + heat -> heat advisory, got {} ({})",
            d0.verdict, d0.reason
        );
        assert_eq!(d0.reason_code, "heat_advisory");

        // Control: wet YESTERDAY (latest past day) -> streak = 1 < 2; same
        // heat, same humidity, no advisory.
        let fc = ForecastSnapshot {
            daily: vec![hot, mild_day(), mild_day()],
            past_daily: vec![dry.clone(), dry, wet],
            ..Default::default()
        };
        let v = compute(&fc, &today, &params);
        let d0 = v.iter().find(|d| d.day_offset == 0).expect("day 0");
        assert_eq!(
            d0.verdict, "run",
            "wet yesterday resets the streak, got {} ({})",
            d0.verdict, d0.reason
        );
    }

    /// The strip is a WEATHER-ONLY projection: a healthy-dry soil zone
    /// demotes a soft forecast-rain skip in the LIVE engine (the soil_floor
    /// moat), but every strip cell is built with soil_zones = [] by design
    /// (there is no per-day soil forecast), so the forecast skip stands on
    /// the strip. Pins the containment so today's soil state can never
    /// silently leak into the 7-day cells (or the moat silently vanish from
    /// the live path).
    #[test]
    fn soil_floor_demotion_does_not_leak_into_future_strip_cells() {
        let params = default_params();
        let dry_zone = crate::engine::skip_rules::ZoneSoil {
            slug: "back_yard".into(),
            name: "Back yard".into(),
            pct: Some(12.0),
            saturation_pct: 60.0,
            target_min_pct: 25.0,
        };

        // LIVE engine control: a tomorrow-rain skip (0.6in x 100% >= 0.25)
        // WITH the dry zone present is demoted to a run by the moat.
        let live = evaluate_with(
            &Inputs {
                forecast_in: 0.6,
                rain_tomorrow_prob_pct: Some(100),
                soil_zones: vec![dry_zone.clone()],
                ..base_inputs()
            },
            &params,
        );
        assert_eq!(
            live.verdict, "run",
            "control: soil_floor demotes the live tomorrow-rain skip, got {} ({})",
            live.verdict, live.reason
        );
        assert_eq!(live.reason_code, "soil_floor");

        // Day 3 forecasts the same heavy, certain rain; cell day 2 sees it
        // as "tomorrow rain". Today's Inputs carry the SAME dry zone.
        let wet_day3 = DailyEntry {
            precip_sum_in: 0.6,
            precip_probability_max: Some(100),
            ..mild_day()
        };
        let fc = ForecastSnapshot {
            daily: vec![mild_day(), mild_day(), mild_day(), wet_day3],
            ..Default::default()
        };
        let today = Inputs {
            soil_zones: vec![dry_zone],
            ..base_inputs()
        };
        let v = compute(&fc, &today, &params);
        let d2 = v.iter().find(|d| d.day_offset == 2).expect("day 2");
        assert_eq!(
            d2.verdict, "skip",
            "day 2 keeps its forecast-rain skip (weather-only cells), got {} ({})",
            d2.verdict, d2.reason
        );
        assert_eq!(d2.reason_code, "tomorrow_rain");
    }
}
