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
use crate::model::DayVerdict;

/// Compute the 7-day verdict strip. `today` carries forward the
/// threshold values + override + pause state; the synthetic per-day
/// Inputs reset live signals (wind_now, rain_intensity_now, etc.) since
/// the strip projects conditions rather than replaying live data.
pub fn compute(
    fc: &ForecastSnapshot,
    today: &Inputs,
    params: &SkipRuleParams,
    cal: crate::engine::calendar::Calendar,
    site: crate::engine::sunrise::Site,
) -> Vec<DayVerdict> {
    inputs_for_days(fc, today, params, cal, site)
        .into_iter()
        .enumerate()
        .map(|(day_idx, inputs)| {
            let d = &fc.daily[day_idx];
            let s = evaluate_with(&inputs, params);
            DayVerdict {
                day_offset: day_idx as u32,
                time_epoch: cal
                    .day_of(d.day_marker)
                    .and_then(|day| cal.day_bounds_utc(day))
                    .map(|(s, _)| s)
                    .unwrap_or(0),
                weather_code: d.weather_code,
                temp_max_f: d.temp_max_f,
                temp_min_f: d.temp_min_f,
                precip_in: d.precip_sum_in,
                rain_evidence_incomplete: inputs.rain_today_forecast_in.is_none()
                    || inputs.forecast_in.is_none()
                    || inputs.rain_3day_weighted_in.is_none(),
                precip_probability_max: d.precip_probability_max,
                verdict: s.verdict,
                reason: s.reason,
                reason_code: s.reason_code,
                mixed_hold: false,
            }
        })
        .collect()
}

/// Shared forecast operands for both weather diagnostics and progressive water planning.
pub fn inputs_for_days(
    fc: &ForecastSnapshot,
    today: &Inputs,
    params: &SkipRuleParams,
    cal: crate::engine::calendar::Calendar,
    site: crate::engine::sunrise::Site,
) -> Vec<Inputs> {
    if fc.daily.is_empty() {
        return Vec::new();
    }
    let n_days = fc.daily.len().min(7);
    let mut out = Vec::with_capacity(n_days);

    for day_idx in 0..n_days {
        let d = &fc.daily[day_idx];
        // The cell's own civil day, and the instant this cell is judged at.
        //
        // The provider's day marker is a LABEL, and its hour is whatever
        // that provider happened to choose: midnight for Open-Meteo, the
        // 06:00 period start for NWS, noon for met.no. Judging a legal
        // gate at that hour asks the wrong question, and judging it in
        // UTC, which is what a zeroed offset meant, asked it in the wrong
        // frame as well. Resolve the DAY, then take an instant that
        // actually belongs to the deployment.
        let cell_day = cal.day_of(d.day_marker);
        let next = cell_day.and_then(|day| fc.aligned(cal, day).ahead(1));

        // None (no window / provider gap) stays None: the engine weights the
        // amount at full value and the cell omits the confidence claim.
        let prob_tomorrow = next.and_then(|n| n.precip_probability_max);
        let precip_tomorrow = next.and_then(|day| day.precip_sum_in);

        // Probability-less days weight at full value (DailyEntry::precip_weight).
        let rain_3day_weighted =
            cell_day.and_then(|day| fc.future_n_day_weighted_precip_in(3, cal, day));
        let rain_7day_weighted =
            cell_day.and_then(|day| fc.future_n_day_weighted_precip_in(7, cal, day));

        let temp_max_3day = fc
            .daily
            .iter()
            .skip(day_idx)
            .take(3)
            .filter_map(|x| x.temp_max_f.filter(|v| v.is_finite()))
            .fold(f64::NEG_INFINITY, f64::max);
        let temp_max_3day = if temp_max_3day.is_finite() {
            temp_max_3day
        } else {
            // Internal neutral heat operand, never published as a daily high.
            0.0
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
            .filter_map(|x| {
                x.temp_max_f
                    .filter(|v| v.is_finite())
                    .zip(x.humidity_pct.filter(|v| *v <= 100))
                    .map(|(temp, rh)| crate::engine::skip_rules::heat_index_f(temp, rh as f64))
            })
            .fold(0.0_f64, f64::max);

        // days_since_significant_rain (forward): scan past days within
        // the window we've already simulated; fall back to past_daily.
        // What counts as a wet day is the operator's own already-wet
        // threshold, not a literal. Three bare 0.05s here meant an
        // operator who raised that knob got a strip disagreeing with the
        // gate that actually skipped their morning.
        let wet_in = if params.already_wet_in > 0.0 {
            params.already_wet_in
        } else {
            crate::engine::WET_DAY_IN
        };
        // A missing day terminates dry-history evidence; absence is not a
        // drought and can never earn a heat extension.
        let mut days_since = 0u32;
        if d.precip_sum_in.is_some_and(|amount| amount < wet_in) {
            for previous in fc.daily[..day_idx]
                .iter()
                .rev()
                .chain(fc.past_daily.iter().rev())
            {
                match previous.precip_sum_in {
                    Some(amount) if amount < wet_in => days_since = days_since.saturating_add(1),
                    Some(_) => {
                        days_since = days_since.saturating_add(1);
                        break;
                    }
                    None => break,
                }
            }
        }
        // A projection never promotes a forecast into measured history.
        // Today's real total can still carry into a future observed window.
        let observed_recent = if day_idx == 0 {
            today.rain_observed_recent_in.max(today.rain_today_in)
        } else if day_idx <= params.rain_observed_window_days as usize {
            today.rain_today_in
        } else {
            0.0
        };

        // The window this cell's yard plans to water in, when knowable:
        // pre-dawn, or the first post-sunrise hour that clears a freezing
        // morning. The hour gates, the wind gate and the freeze gates all
        // read it, and the dispatcher asks the same function, so the cell
        // and the morning it previews are judged in the same window.
        let chosen = cell_day.and_then(|day| {
            crate::engine::dispatch_window::choose(
                day,
                site,
                cal,
                fc,
                today.min_temp_f,
                crate::engine::dispatch_window::Rules {
                    restrictions: &today.watering_restrictions,
                    parity: today.address_parity,
                    watered: &today.watered_days,
                },
            )
        });
        let window = chosen.map(|w| w.span());

        let trial_temp = chosen
            .and_then(|w| fc.temp_at(w.start))
            .or(d.temp_min_f)
            .filter(|v| v.is_finite());
        let trial_wind = window
            .and_then(|(s, e)| fc.wind_max_over_window_mph(s, e))
            .or(d.wind_max_mph)
            .filter(|v| v.is_finite() && *v >= 0.0);
        let inputs = Inputs {
            restart_required: today.restart_required,
            calendar: cal,
            // The forecast temperature at the planned start, when the
            // hourly series reaches it; the day's low otherwise. The low
            // is the pre-dawn figure on a continental morning, and the
            // cell used to be refused for it even when the window had
            // moved to a warmer hour.
            // Missing forecast temperatures must hit the integrity gate,
            // never a fabricated zero-degree freeze comparison.
            temp_now_f: trial_temp.unwrap_or(0.0),
            run_window: chosen.map(|w| w.kind).unwrap_or_default(),
            window_min_temp_f: chosen.and_then(|w| w.min_temp_f),
            wind_now_mph: 0.0,
            // A projected day has no gauge reading by definition, so the
            // measured total is zero and the model's total travels in the
            // modelled field. The cell then reads "Rain forecast today"
            // rather than "Already wet", which is what a future day is.
            rain_today_in: 0.0,
            rain_today_forecast_in: d.precip_sum_in,
            rain_intensity_now_in_hr: Some(0.0),
            // The 7-day strip projects forecast weather with no live current-rain
            // reading (rate 0), so the rain_now gate never fires here; the nature
            // is the honest Model default regardless.
            rain_nature: crate::model::RainNature::default(),
            // The 7-day strip is a forward projection on the current forecast;
            // live-staleness gating belongs to the refresher's today decision.
            forecast_stale: false,
            humidity_now_pct: today.humidity_now_pct,

            forecast_in: precip_tomorrow,
            rain_tomorrow_prob_pct: prob_tomorrow,
            rain_3day_weighted_in: rain_3day_weighted,
            rain_7day_weighted_in: rain_7day_weighted,
            rain_next_4h_in: Some(0.0),
            rain_observed_recent_in: observed_recent,
            wind_max_today_mph: d.wind_max_mph.unwrap_or(0.0),
            // The hourly series runs two days; cells past it carry None
            // and the gate judges the day's peak, which is honest.
            wind_window_max_mph: window.and_then(|(s, e)| fc.wind_max_over_window_mph(s, e)),
            // The week's spent days, so a days-per-week allowance reads
            // the same in the strip as on the hero. The strip does not
            // count its own future cells as spent: it is a projection of
            // the rules, not a simulation of itself.
            watered_days: today.watered_days.clone(),
            temp_min_24h_f: d.temp_min_f.filter(|v| v.is_finite()),
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
            // A forecast cell still needs a real temperature for its trial.
            live_readings: if trial_temp.is_some() && trial_wind.is_some() {
                crate::engine::skip_rules::LiveReadings::ForecastFallback
            } else {
                crate::engine::skip_rules::LiveReadings::Unavailable
            },
            is_paused: today.is_paused,
            is_dry_run: false,

            pause_until_epoch: today.pause_until_epoch,
            // The cell's own local midnight, resolved through the
            // deployment calendar, which mints the offset along with it.
            // Previously this was the provider's raw day marker paired
            // with an offset hardcoded to zero, so the strip judged a US
            // Eastern morning as if it were 10:00 UTC.
            // Judged at the instant the yard PLANS to water that day.
            //
            // Not the provider's day marker, and not local midnight
            // either: a district banning 22:00 to 06:00 would refuse
            // every day of the week if midnight were on trial. When no
            // instant is knowable, which is a real state for a polar
            // latitude or an install with no location, the cell carries
            // the DAY. Weekday and season still bind; the hour abstains.
            when: match (cell_day, window) {
                (None, _) => crate::engine::clock::DecisionTime::Unknown,
                (Some(_), Some((start, _))) => crate::engine::clock::DecisionTime::at(cal, start),
                (Some(day), None) => crate::engine::clock::DecisionTime::Day(day),
            },
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
        out.push(inputs);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::SkipRuleParams;
    use crate::engine::skip_rules::Inputs;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot};

    /// A yard with a location and a sequence length, so every cell has a
    /// planned start to be judged at. A strip fixture without one would
    /// abstain on every hour gate and could not test them at all.
    fn test_site() -> crate::engine::sunrise::Site {
        // Jacksonville, Florida: the deployment that reported the defect.
        crate::engine::sunrise::Site::new((30.33, -81.66), 30 * 60)
    }

    /// A mild, dry forecast day no rule fires on by itself.
    fn mild_day() -> DailyEntry {
        DailyEntry {
            temp_max_f: Some(72.0),
            temp_min_f: Some(55.0),
            wind_max_mph: Some(0.0),
            precip_sum_in: Some(0.0),
            precip_probability_max: Some(0),
            ..Default::default()
        }
    }

    /// Give synthetic weather rows real, distinct civil-day labels.
    fn dated(mut days: Vec<DailyEntry>) -> Vec<DailyEntry> {
        for (offset, day) in days.iter_mut().enumerate() {
            day.day_marker = crate::engine::clock::DayMarker::inside_local_day(
                1_699_920_000 + offset as i64 * 86400,
            );
        }
        days
    }

    #[test]
    fn missing_forecast_temperature_holds_with_integrity_reason_not_false_freeze() {
        let mut fc = ForecastSnapshot {
            daily: vec![mild_day()],
            ..Default::default()
        };
        fc.daily[0].temp_min_f = None;
        let cal = crate::engine::calendar::Calendar::utc();
        let inputs = Inputs {
            min_temp_f: 38.0,
            ..base_inputs()
        };
        let result = compute(&fc, &inputs, &SkipRuleParams::default(), cal, test_site());
        assert_eq!(result[0].verdict, "skip");
        assert_eq!(result[0].reason_code, "live_data");
        assert_eq!(result[0].temp_min_f, None);
        // A real zero is preserved and reaches the real freeze gate.
        fc.daily[0].temp_min_f = Some(0.0);
        let result = compute(&fc, &inputs, &SkipRuleParams::default(), cal, test_site());
        assert_eq!(result[0].reason_code, "freeze_now");
        assert_eq!(result[0].temp_min_f, Some(0.0));
    }

    /// The two St. Johns River Water Management District rules as a real
    /// operator has them configured: during daylight saving an even street
    /// address may water Thursday and Sunday, never between 10:00 and
    /// 16:00, capped at 60 minutes a zone. Outside daylight saving it is
    /// Sunday only.
    fn sjrwmd() -> Vec<crate::config::schema::WateringRestriction> {
        use crate::config::schema::{EffectiveWindow, WateringRestriction};
        vec![
            WateringRestriction {
                id: "sjrwmd_dst".into(),
                name: "St. Johns RWMD - Daylight saving".into(),
                enabled: true,
                effective: EffectiveWindow::DstOnly,
                allowed_weekdays_odd: vec![3, 6],
                allowed_weekdays_even: vec![4, 0],
                forbidden_hour_start: Some(10),
                forbidden_hour_end: Some(16),
                max_minutes_per_zone: Some(60),
                ..Default::default()
            },
            WateringRestriction {
                id: "sjrwmd_est".into(),
                name: "St. Johns RWMD - Standard time".into(),
                enabled: true,
                effective: EffectiveWindow::StandardOnly,
                allowed_weekdays_odd: vec![6],
                allowed_weekdays_even: vec![0],
                forbidden_hour_start: Some(10),
                forbidden_hour_end: Some(16),
                max_minutes_per_zone: Some(60),
                ..Default::default()
            },
        ]
    }

    /// The reported defect, end to end.
    ///
    /// A US Eastern yard, even address, NWS as the forecast owner. NWS
    /// stamps each daily row at the 06:00 local daytime period start,
    /// which is 10:00 UTC in EDT. The strip built its per-day inputs with
    /// the offset hardcoded to zero, so it read that stamp as hour 10,
    /// the inclusive lower edge of the 10:00 to 16:00 ban, on EVERY day.
    ///
    /// Five days were blocked correctly, for being the wrong weekday. The
    /// two days the operator was actually allowed to water were blocked
    /// for being inside the forbidden window, at a slot the yard waters at
    /// 06:00. The sum on screen was a week with no legal day in it.
    #[test]
    fn the_strip_allows_the_two_legal_days_under_sjrwmd() {
        use crate::config::schema::AddressParity;

        // Saturday 5 September 2026, 00:00 EDT.
        const SAT_MIDNIGHT_EDT: i64 = 1_788_580_800;
        let cal = crate::engine::calendar::Calendar::fixed_offset(-4 * 3600).expect("EDT");

        let fc = ForecastSnapshot {
            daily: (0..8i64)
                .map(|i| DailyEntry {
                    // The NWS anchor, not midnight.
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(
                        SAT_MIDNIGHT_EDT + i * 86_400 + 6 * 3600,
                    ),
                    ..mild_day()
                })
                .collect(),
            ..Default::default()
        };
        let today = Inputs {
            watering_restrictions: sjrwmd(),
            address_parity: AddressParity::Even,
            ..base_inputs()
        };

        let v = compute(&fc, &today, &default_params(), cal, test_site());
        assert_eq!(v.len(), 7);

        // Sat, Sun, Mon, Tue, Wed, Thu, Fri. Even parity waters Thu and Sun.
        let legal = [1usize, 5];
        for (i, cell) in v.iter().enumerate() {
            if legal.contains(&i) {
                assert_ne!(
                    cell.reason_code, "restrictions",
                    "day {i} is a legal watering day, got: {}",
                    cell.reason
                );
            } else {
                assert_eq!(
                    cell.reason_code, "restrictions",
                    "day {i} is not an allowed weekday"
                );
            }
        }
        // The specific wrong answer that shipped: a legal day refused on
        // the clock, for a morning slot hours outside the ban.
        assert!(
            !v.iter().any(|c| c.reason.contains("forbidden window")),
            "no cell may be refused on the hour: {:?}",
            v.iter().map(|c| &c.reason).collect::<Vec<_>>()
        );
    }

    /// The strip and the morning agree about wind, because they judge
    /// the same minutes.
    ///
    /// The cell used to carry the daily peak, an afternoon figure, into
    /// a gate judging a pre-dawn run. Now it reads the hourly series over
    /// the planned window and the afternoon stays where it belongs.
    #[test]
    fn a_gusty_afternoon_does_not_darken_a_calm_dawn_cell() {
        use crate::forecast::snapshot::HourlyEntry;

        const SAT_MIDNIGHT_EDT: i64 = 1_788_580_800;
        let cal = crate::engine::calendar::Calendar::fixed_offset(-4 * 3600).expect("EDT");

        let mut fc = ForecastSnapshot {
            daily: (0..8i64)
                .map(|i| DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(
                        SAT_MIDNIGHT_EDT + i * 86_400 + 6 * 3600,
                    ),
                    // Every day reads a 25 mph peak, well over 10 + 5.
                    wind_max_mph: Some(25.0),
                    ..mild_day()
                })
                .collect(),
            ..Default::default()
        };
        // Saturday's hours: a calm dawn and a gusty afternoon. Jacksonville
        // sunrise on 5 September is a little after 07:00 EDT, so a 30
        // minute sequence is planned inside the 06:00 hour.
        fc.hourly = (0..24i64)
            .map(|h| HourlyEntry {
                time_epoch: SAT_MIDNIGHT_EDT + h * 3600,
                wind_mph: Some(if (12..=18).contains(&h) { 25.0 } else { 5.0 }),
                ..Default::default()
            })
            .collect();
        let today = Inputs {
            max_wind_mph: 10.0,
            ..base_inputs()
        };

        let v = compute(&fc, &today, &default_params(), cal, test_site());
        assert_eq!(v.len(), 7);
        // Saturday has hourly coverage: the dawn is calm, the cell runs.
        assert_ne!(
            v[0].reason_code, "wind_forecast",
            "the afternoon must not refuse the dawn: {}",
            v[0].reason
        );
        // Monday is past the hourly series: no window is knowable, so
        // the day's 25 mph peak governs and the cell says so.
        assert_eq!(v[2].reason_code, "wind_forecast", "{}", v[2].reason);
        assert!(
            v[2].reason.starts_with("Windy day forecast"),
            "{}",
            v[2].reason
        );
    }

    /// A ban that wraps midnight is why local midnight is not a good
    /// enough answer either.
    ///
    /// Plenty of ordinances forbid watering overnight rather than at
    /// midday. Judging a whole-day cell at 00:00 sits squarely inside a
    /// 22:00 to 06:00 window, so every day of the week would be refused
    /// again, for a yard that plans to start after six in the morning.
    /// The cell has to be judged at the instant the yard actually plans
    /// to water.
    #[test]
    fn an_overnight_ban_does_not_refuse_a_morning_start() {
        use crate::config::schema::{AddressParity, EffectiveWindow, WateringRestriction};

        const SAT_MIDNIGHT_EDT: i64 = 1_788_580_800;
        let cal = crate::engine::calendar::Calendar::fixed_offset(-4 * 3600).expect("EDT");

        let overnight = vec![WateringRestriction {
            id: "overnight".into(),
            name: "Overnight ban".into(),
            enabled: true,
            effective: EffectiveWindow::AllYear,
            // Every weekday allowed: the hour window is the only gate.
            allowed_weekdays_odd: vec![],
            allowed_weekdays_even: vec![],
            forbidden_hour_start: Some(22),
            forbidden_hour_end: Some(6),
            max_minutes_per_zone: None,
            ..Default::default()
        }];

        let fc = ForecastSnapshot {
            daily: (0..8i64)
                .map(|i| DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(
                        SAT_MIDNIGHT_EDT + i * 86_400 + 6 * 3600,
                    ),
                    ..mild_day()
                })
                .collect(),
            ..Default::default()
        };
        let today = Inputs {
            watering_restrictions: overnight,
            address_parity: AddressParity::NotApplicable,
            ..base_inputs()
        };

        let v = compute(&fc, &today, &default_params(), cal, test_site());
        assert_eq!(v.len(), 7);
        for (i, cell) in v.iter().enumerate() {
            assert_ne!(
                cell.reason_code, "restrictions",
                "day {i} plans a post-dawn start, outside a 22:00 to 06:00 ban: {}",
                cell.reason
            );
        }
    }

    /// The property that makes the fix hold when the forecast owner
    /// changes underneath the operator.
    ///
    /// Six providers stamp their daily rows at four different hours. The
    /// same seven civil days, expressed in every one of those
    /// conventions, must produce identical verdicts. An offset-only fix
    /// cannot pass this: met.no stamps local noon, which is inside the
    /// 10:00 to 16:00 ban, so every day would block again after a
    /// failover with no configuration change at all.
    #[test]
    fn verdicts_are_invariant_across_provider_day_anchors() {
        use crate::config::schema::AddressParity;

        const SAT_MIDNIGHT_EDT: i64 = 1_788_580_800;
        let cal = crate::engine::calendar::Calendar::fixed_offset(-4 * 3600).expect("EDT");
        let today = Inputs {
            watering_restrictions: sjrwmd(),
            address_parity: AddressParity::Even,
            ..base_inputs()
        };

        let mut baseline: Option<Vec<DayVerdict>> = None;
        for (label, offset_s) in [
            ("Open-Meteo / Pirate 00:00 local", 0),
            ("NWS daytime 06:00 local", 6 * 3600),
            ("met.no local noon", 12 * 3600),
            ("NWS lone night 18:00 local", 18 * 3600),
            ("last second of the day", 86_399),
        ] {
            let fc = ForecastSnapshot {
                daily: (0..8i64)
                    .map(|i| DailyEntry {
                        day_marker: crate::engine::clock::DayMarker::inside_local_day(
                            SAT_MIDNIGHT_EDT + i * 86_400 + offset_s,
                        ),
                        ..mild_day()
                    })
                    .collect(),
                ..Default::default()
            };
            let v = compute(&fc, &today, &default_params(), cal, test_site());
            match &baseline {
                None => baseline = Some(v),
                Some(b) => assert_eq!(
                    &v, b,
                    "{label} disagreed with the first convention; the day anchor is leaking into the verdict"
                ),
            }
        }
    }

    /// Baseline `today` Inputs matching the existing carry-forward test:
    /// the user rain threshold + a benign overnight low, everything else
    /// default.
    fn base_inputs() -> Inputs {
        Inputs {
            forecast_in: Some(0.0),
            rain_today_forecast_in: Some(0.0),
            rain_intensity_now_in_hr: Some(0.0),
            rain_next_4h_in: Some(0.0),
            rain_3day_weighted_in: Some(0.0),
            rain_7day_weighted_in: Some(0.0),
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
            temp_max_f: Some(72.0),
            temp_min_f: Some(55.0),
            wind_max_mph: Some(0.0),
            precip_sum_in: Some(0.0),
            precip_probability_max: Some(0),
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            daily: dated(vec![mild.clone(), mild.clone(), mild.clone()]),
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
            crate::engine::calendar::Calendar::utc(),
            test_site(),
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
            crate::engine::calendar::Calendar::utc(),
            test_site(),
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
        let v = compute(
            &fc,
            &base_inputs(),
            &default_params(),
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
        assert!(v.is_empty(), "no forecast days -> no strip cells");
    }

    /// The strip length tracks the forecast: shorter than 7 days yields one
    /// cell per day (contiguous day_offset + per-day epochs), longer is
    /// capped at 7.
    #[test]
    fn strip_length_tracks_daily_len_capped_at_seven() {
        let params = default_params();

        // 3-day forecast -> exactly 3 cells, offsets 0..=2.
        //
        // The markers are stamped at 06:00, the NWS daytime period start,
        // rather than at midnight. That is the point: a cell's reported
        // epoch is the day's true local midnight resolved through the
        // calendar, not the provider's anchor copied through. The old
        // fixture spaced markers exactly 86400s from an arbitrary epoch,
        // which quietly assumed Open-Meteo's convention and could not have
        // expressed an NWS-shaped day at all.
        const MIDNIGHT: i64 = 1_699_920_000;
        let fc = ForecastSnapshot {
            daily: (0..3i64)
                .map(|i| DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(
                        MIDNIGHT + i * 86_400 + 6 * 3600,
                    ),
                    ..mild_day()
                })
                .collect(),
            ..Default::default()
        };
        let v = compute(
            &fc,
            &base_inputs(),
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
        assert_eq!(v.len(), 3, "3 forecast days -> 3 cells");
        assert_eq!(
            v.iter().map(|d| d.day_offset).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        assert_eq!(
            v[2].time_epoch,
            MIDNIGHT + 2 * 86_400,
            "the cell reports its day's midnight, not the provider's 06:00 anchor"
        );
        assert!(
            v[..2].iter().all(|d| d.verdict == "run"),
            "covered mild cells run"
        );
        assert_eq!(
            v[2].reason_code, "tomorrow_rain",
            "the horizon end cannot invent tomorrow dry"
        );
        assert!(v[2].rain_evidence_incomplete);

        // 10-day forecast -> capped at 7 cells.
        let fc = ForecastSnapshot {
            daily: dated(vec![mild_day(); 10]),
            ..Default::default()
        };
        let v = compute(
            &fc,
            &base_inputs(),
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
        assert_eq!(v.len(), 7, "strip is capped at 7 cells");
    }

    #[test]
    fn archive_forecast_rain_is_not_promoted_to_future_observed_rain() {
        let fc = ForecastSnapshot {
            daily: dated(vec![mild_day(); 5]),
            past_daily: vec![DailyEntry {
                precip_sum_in: Some(2.0),
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
        let verdicts = compute(
            &fc,
            &today,
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
        assert_eq!(
            verdicts[1].verdict, "run",
            "an archive estimate cannot inflate measured 0.15 inches"
        );
        let wet = Inputs {
            rain_today_in: 0.30,
            ..base_inputs()
        };
        let verdicts = compute(
            &fc,
            &wet,
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
        assert_eq!(verdicts[1].reason_code, "observed_rain");
        assert_eq!(verdicts[2].reason_code, "observed_rain");
        assert_eq!(
            verdicts[3].verdict, "run",
            "the actual measurement expires at the configured boundary"
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
            daily: dated(vec![mild_day(); 3]),
            ..Default::default()
        };
        let params = default_params();
        let today = Inputs {
            rain_today_in: 0.36,
            ..base_inputs()
        };

        let v = compute(
            &fc,
            &today,
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
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
            daily: dated(vec![mild_day(); 3]),
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

        let v = compute(
            &fc,
            &today,
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
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
        assert_eq!(
            d2.reason_code, "tomorrow_rain",
            "the final cell has no next-day rain evidence"
        );
        assert!(d2.rain_evidence_incomplete);
    }

    /// days_since_significant_rain falls back through past_daily when no
    /// already-simulated forecast day was wet: a wet day 3 past-days back
    /// leaves a dry streak long enough for the heat-advisory rule
    /// (run_extended), while rain just yesterday resets the streak and the
    /// same hot cell is a plain run.
    #[test]
    fn days_since_rain_falls_back_through_past_daily_for_dry_streak_rule() {
        let hot = DailyEntry {
            temp_max_f: Some(98.0),
            temp_min_f: Some(74.0),
            ..mild_day()
        };
        let dry = mild_day();
        // Significant (>= 0.05) but under every observed-skip threshold, so
        // ONLY the streak arithmetic distinguishes the two arrangements.
        let wet = DailyEntry {
            precip_sum_in: Some(0.10),
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
            daily: dated(vec![hot.clone(), mild_day(), mild_day()]),
            past_daily: vec![wet.clone(), dry.clone(), dry.clone()],
            ..Default::default()
        };
        let v = compute(
            &fc,
            &today,
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
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
            daily: dated(vec![hot, mild_day(), mild_day()]),
            past_daily: vec![dry.clone(), dry, wet],
            ..Default::default()
        };
        let v = compute(
            &fc,
            &today,
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
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
            probe_configured: false,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        };

        // LIVE engine control: a tomorrow-rain skip (0.6in x 100% >= 0.25)
        // WITH the dry zone present is demoted to a run by the moat.
        let live = evaluate_with(
            &Inputs {
                forecast_in: Some(0.6),
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
            precip_sum_in: Some(0.6),
            precip_probability_max: Some(100),
            ..mild_day()
        };
        let fc = ForecastSnapshot {
            daily: dated(vec![mild_day(), mild_day(), mild_day(), wet_day3]),
            ..Default::default()
        };
        let today = Inputs {
            soil_zones: vec![dry_zone],
            ..base_inputs()
        };
        let v = compute(
            &fc,
            &today,
            &params,
            crate::engine::calendar::Calendar::utc(),
            test_site(),
        );
        let d2 = v.iter().find(|d| d.day_offset == 2).expect("day 2");
        assert_eq!(
            d2.verdict, "skip",
            "day 2 keeps its forecast-rain skip (weather-only cells), got {} ({})",
            d2.verdict, d2.reason
        );
        assert_eq!(d2.reason_code, "tomorrow_rain");
    }
}
