//! Run the existing decision and sizing passes over a progressing water balance.
//! A modeled shower refills only the root-zone capacity; a modeled run spends
//! water and legal watering days. Neither is written back to observed evidence.
use super::*;
use crate::engine::clock::CivilDay;
use crate::history::rollup::{applied_in_window, cluster_events, RunSegment};
use crate::model::{WaterPlanDay, WaterPlanZone};
use chrono::Datelike;

pub(super) fn soil_horizon(
    params: &crate::engine::soil_schedule::ZoneSoilParams,
    policy: &WateringPolicy,
    balance: Option<&BalanceTick>,
    forecast: &ForecastSnapshot,
    now: i64,
    as_of: Option<i64>,
) -> Vec<crate::engine::soil_outlook::OutlookDay> {
    let Some(today) = policy.calendar.date_of(now) else {
        return Vec::new();
    };
    (0..7)
        .filter_map(|offset| {
            let weather = forecast.aligned(policy.calendar, today).ahead(offset);
            let date = today.naive() + chrono::Duration::days(i64::from(offset));
            let day = CivilDay::from_naive(date);
            let (mut demand, _) = daily_demand(
                policy,
                &params.slug,
                date,
                weather.and_then(|w| w.reference_et0_mm()),
            );
            if offset == 0 {
                let spent = balance
                    .and_then(|b| b.soil.today_partial_et0_mm)
                    .unwrap_or(0.0)
                    * crate::engine::kc_at_doy_lat(
                        params.species,
                        date.ordinal() as u16,
                        policy.location.0,
                    );
                demand = (demand - spent).max(0.0);
            }
            let rain = weather.and_then(|w| w.precip_sum_in.map(|r| r * w.precip_weight()));
            let rain = if offset == 0 {
                let (_, end) = policy.calendar.day_bounds_utc(day)?;
                let hours = ((end - now).max(0) / 3600) as usize;
                if hours == 0 {
                    Some(0.0)
                } else if let Some(as_of) = as_of {
                    forecast.scenario_rain_in(now, as_of, policy.calendar)
                } else {
                    forecast.planning_precip_weighted_in(hours, now)
                }
            } else {
                rain
            };
            let multiplier = balance
                .map(|b| b.bias.multiplier_for(date.month()))
                .unwrap_or(1.0);
            let allowed = !crate::engine::restrictions::evaluate_for(
                crate::engine::clock::DecisionTime::Day(day),
                &policy.restrictions,
                policy.address_parity,
                &watered_days(balance),
                Some(crate::engine::restrictions::ZoneScope {
                    slug: &params.slug,
                    sprinkler: params.sprinkler_type,
                }),
            )
            .skip;
            Some(crate::engine::soil_outlook::OutlookDay {
                demand_mm: demand,
                rain_mm: rain.map(|r| crate::units::in_to_mm(r * multiplier)),
                can_water: allowed,
            })
        })
        .collect()
}

pub(super) fn project(
    live: &IrrigationSnapshot,
    inputs: &Inputs,
    policy: &WateringPolicy,
    scripts: &crate::engine::scripting::CompiledScripts,
    balance: Option<&BalanceTick>,
    forecast: &ForecastSnapshot,
    now: i64,
) -> Vec<WaterPlanDay> {
    let Some(today) = policy.calendar.date_of(now) else {
        return Vec::new();
    };
    let Some(mut evidence) = balance.cloned().filter(|b| !b.runs_degraded) else {
        return Vec::new();
    };
    if evidence.soil.dates.is_empty() {
        return Vec::new();
    }
    let mut result = Vec::new();
    let mut previous = live.clone();
    let mut complete =
        !crate::forecast::snapshot::forecast_is_stale(forecast.last_refresh_epoch, now);
    for offset in 0..7 {
        let date = today.naive() + chrono::Duration::days(i64::from(offset));
        let day = CivilDay::from_naive(date);
        let Some((start, end)) = policy.calendar.day_bounds_utc(day) else {
            break;
        };
        let Some(weather) = forecast
            .aligned(policy.calendar, today)
            .ahead(offset as u16)
        else {
            break;
        };
        let fc = forecast.for_day(policy.calendar, day);
        if offset > 0 {
            advance(
                &mut evidence,
                &previous,
                forecast,
                policy,
                date.pred_opt().unwrap(),
                now,
                offset == 1,
                &mut complete,
            );
        }
        let mut snap = if offset == 0 {
            live.clone()
        } else {
            previous.clone()
        };
        if offset > 0 {
            snap.water_plan.clear();
            snap.seven_day_verdicts.clear();
            // Iterating couples the forecast's dispatch-hour safety checks to
            // the same cycle/soak duration used by the actual scheduler.
            let mut wall = 0;
            for _ in 0..3 {
                let site = crate::engine::sunrise::Site::new(policy.location, wall);
                let mut context = inputs.clone();
                context.watered_days = watered_days(Some(&evidence));
                context.rain_today_in = 0.0;
                context.rain_observed_recent_in = crate::units::mm_to_in(
                    evidence
                        .soil
                        .rain_mm
                        .iter()
                        .rev()
                        .skip(1)
                        .take(policy.skip_rules.rain_observed_window_days as usize)
                        .sum(),
                );
                let Some(mut trial) = crate::engine::verdict_strip::inputs_for_days(
                    &fc,
                    &context,
                    &policy.skip_rules,
                    policy.calendar,
                    site,
                )
                .into_iter()
                .next() else {
                    break;
                };
                trial.is_tomorrow = offset == 1;
                trial.is_dry_run = inputs.is_dry_run;
                // A future day has no measured probe reading. The forecast
                // remains conditional on real sensor/safety checks at dispatch.
                trial.soil_zones = inputs
                    .soil_zones
                    .iter()
                    .map(|zone| {
                        let mut zone = zone.clone();
                        zone.pct = None;
                        zone.probe_configured = false;
                        zone.governed_by_soil_model = policy.resolve_scheduling_model(&zone.slug)
                            == crate::config::schema::SchedulingModel::Soil;
                        zone.planning_forecast_unavailable =
                            crate::forecast::snapshot::forecast_is_stale(
                                fc.last_refresh_epoch,
                                now,
                            ) || fc.scenario_rain_in(start, now, policy.calendar).is_none();
                        zone
                    })
                    .collect();
                let epoch = crate::engine::sunrise::smart_morning_target_start(
                    date,
                    policy.location.0,
                    policy.location.1,
                    wall,
                    policy.calendar,
                )
                .map(|t| t.timestamp())
                .unwrap_or(start);
                let restrictions = crate::engine::restrictions::evaluate_for(
                    trial.when,
                    &policy.restrictions,
                    policy.address_parity,
                    &context.watered_days,
                    None,
                );
                let cap = restrictions.max_minutes_cap.map(|m| m * 60);
                snap.water_budgets = compute_water_budgets_for_horizon(
                    &fc,
                    &policy.zone_runtime,
                    policy.defer_threshold_in(),
                    cap,
                    &policy.budget_zones,
                    Some(&evidence),
                    policy.calendar,
                    epoch,
                    Some(now),
                );
                let soil_plans = prepare_soil_schedule(
                    &mut snap,
                    policy,
                    Some(&evidence),
                    &fc,
                    cap,
                    crate::engine::Tick::at(policy.calendar, epoch),
                    epoch,
                    Some(now),
                );
                set_soil_governance(&mut trial, &soil_plans);
                apply_engine(
                    &mut snap,
                    &trial,
                    scripts,
                    &policy.condition_rules,
                    &policy.skip_rules,
                );
                apply_soil_plans(
                    &mut snap,
                    policy,
                    cap,
                    crate::engine::Tick::at(policy.calendar, epoch),
                    epoch,
                    soil_plans,
                );
                apply_budget_plan(
                    &mut snap,
                    policy,
                    date.weekday().num_days_from_sunday() as u8,
                );
                apply_verdict_multiplier(&mut snap);
                let next_wall = planned_wall(&snap, policy);
                if next_wall == wall {
                    break;
                }
                wall = next_wall;
            }
        } else {
            apply_verdict_multiplier(&mut snap);
        }
        let wall = planned_wall(&snap, policy);
        let window = (wall > 0)
            .then(|| {
                crate::engine::dispatch_window::choose(
                    day,
                    crate::engine::sunrise::Site::new(policy.location, wall),
                    policy.calendar,
                    &fc,
                    inputs.min_temp_f,
                    crate::engine::dispatch_window::Rules {
                        restrictions: &policy.restrictions,
                        parity: policy.address_parity,
                        watered: &watered_days(Some(&evidence)),
                    },
                )
            })
            .flatten();
        let expected = weather.precip_sum_in.map(|rain| {
            crate::units::in_to_mm(
                rain * weather.precip_weight() * evidence.bias.multiplier_for(date.month()),
            )
        });
        complete &= expected.is_some();
        let zones = snap
            .zones
            .iter()
            .map(|zone| {
                let budget = snap.water_budgets.iter().find(|b| b.zone_slug == zone.slug);
                let (demand, source) =
                    daily_demand(policy, &zone.slug, date, weather.reference_et0_mm());
                let seconds = executable_seconds(zone);
                let need = budget
                    .map(|b| b.today_reason.clone())
                    .unwrap_or_else(|| "Water need unavailable".into());
                let held = zone
                    .verdict
                    .as_ref()
                    .is_none_or(|v| !matches!(v.verdict.as_str(), "run" | "run_extended"));
                let reason = if held {
                    format!(
                        "{}; {}",
                        zone.verdict
                            .as_ref()
                            .map(|v| v.reason.as_str())
                            .unwrap_or("Decision unavailable"),
                        need
                    )
                } else if seconds == 0 {
                    need.clone()
                } else {
                    format!("{:.0} min planned: {need}", f64::from(seconds) / 60.0)
                };
                WaterPlanZone {
                    zone: zone.slug.clone(),
                    name: zone.name.clone(),
                    planned_seconds: seconds,
                    reason,
                    water_need: need,
                    reason_code: if held {
                        zone.verdict
                            .as_ref()
                            .map(|v| v.reason_code.clone())
                            .unwrap_or_else(|| "unknown".into())
                    } else {
                        "water_balance".into()
                    },
                    depletion_mm: budget.and_then(|b| b.soil_depletion_mm),
                    trigger_mm: budget.and_then(|b| b.soil_raw_mm),
                    depletion_range_mm: budget.and_then(|b| b.soil_depletion_range_mm),
                    capacity_mm: budget.and_then(|b| b.soil_taw_mm),
                    demand_mm: demand,
                    demand_source: source.into(),
                    model: budget
                        .map(|b| b.scheduling_model.clone())
                        .unwrap_or_default(),
                    session_capped: budget.is_some_and(|b| b.session_capped),
                }
            })
            .collect();
        result.push(WaterPlanDay {
            date_local: date.to_string(),
            day_offset: offset,
            time_epoch: start,
            start_epoch: window.map(|w| w.start),
            finish_epoch: window.map(|w| w.finish),
            forecast_rain_mm: weather.precip_sum_in.map(crate::units::in_to_mm),
            expected_rain_mm: expected,
            rain_probability_pct: weather.precip_probability_max,
            evidence_complete: complete,
            zones,
        });
        previous = snap;
        let _ = end;
    }
    result
}

pub(super) fn align_strip(snapshot: &mut IrrigationSnapshot) {
    for cell in snapshot
        .seven_day_verdicts
        .iter_mut()
        .filter(|c| c.day_offset > 0)
    {
        let Some(day) = snapshot
            .water_plan
            .iter()
            .find(|d| d.day_offset == cell.day_offset)
        else {
            continue;
        };
        let seconds: u32 = day.zones.iter().map(|z| z.planned_seconds).sum();
        cell.verdict = if seconds > 0 { "run" } else { "skip" }.into();
        cell.reason_code = "water_balance".into();
        cell.mixed_hold = seconds > 0 && day.zones.iter().any(|z| z.planned_seconds == 0);
        cell.reason = if seconds > 0 {
            format!(
                "{:.0} min projected from the rolling root-zone and weather balance",
                f64::from(seconds) / 60.0
            )
        } else {
            "No watering planned from the rolling water balance and current forecast; see the daily plan for zone reasons".into()
        };
    }
}

fn executable_seconds(zone: &ZoneState) -> u32 {
    if zone
        .verdict
        .as_ref()
        .is_some_and(|v| matches!(v.verdict.as_str(), "run" | "run_extended"))
    {
        zone.planned_run_seconds
    } else {
        0
    }
}

fn planned_wall(snap: &IrrigationSnapshot, policy: &WateringPolicy) -> u64 {
    let zones: Vec<_> = snap
        .zones
        .iter()
        .map(|z| {
            let mut z = z.clone();
            z.planned_run_seconds = executable_seconds(&z);
            z
        })
        .collect();
    crate::scheduler::smart_morning::sequence_wall_seconds(
        &policy.zone_agronomy,
        &zones,
        policy.soak_minutes,
        policy.interleave_cycles,
        policy.duration_quantum_s,
    )
}

fn daily_demand(
    policy: &WateringPolicy,
    slug: &str,
    date: chrono::NaiveDate,
    et0: Option<f64>,
) -> (f64, &'static str) {
    let agr = policy.zone_agronomy.get(slug);
    let Some(agr) = agr else {
        let weekly = policy
            .budget_zones
            .iter()
            .find(|b| b.slug == slug)
            .map(|b| b.weekly_budget_in.unwrap_or(b.default_budget_in))
            .unwrap_or(0.0);
        return (crate::units::in_to_mm(weekly) / 7.0, "weekly_target");
    };
    let species = agr.species;
    match et0 {
        Some(et0) => (
            et0 * crate::engine::kc_at_doy_lat(species, date.ordinal() as u16, policy.location.0),
            "forecast_et0",
        ),
        None => (
            crate::engine::soil_schedule::fallback_daily_etc_mm(
                policy
                    .budget_zones
                    .iter()
                    .find(|b| b.slug == slug)
                    .and_then(|b| b.weekly_budget_in),
                species,
            ),
            "seasonal_estimate",
        ),
    }
}

/// Settle the previous scenario day, retaining exact historical events and
/// appending simulated irrigation only when the decision actually permits it.
fn advance(
    evidence: &mut BalanceTick,
    plan: &IrrigationSnapshot,
    forecast: &ForecastSnapshot,
    policy: &WateringPolicy,
    previous: chrono::NaiveDate,
    now: i64,
    first: bool,
    complete: &mut bool,
) {
    let day = CivilDay::from_naive(previous);
    let Some((start, end)) = policy.calendar.day_bounds_utc(day) else {
        *complete = false;
        return;
    };
    let weather = forecast
        .daily
        .iter()
        .find(|d| policy.calendar.day_of(d.day_marker) == Some(day));
    let et0 = weather.and_then(|d| d.reference_et0_mm());
    if let Some(et0) = et0 {
        let et0 = if first {
            evidence
                .soil
                .et0_ledger
                .iter()
                .filter(|(date, _)| *date == previous)
                .map(|(_, value)| *value)
                .chain(evidence.soil.today_partial_et0_mm)
                .fold(et0, f64::max)
        } else {
            et0
        };
        evidence
            .soil
            .et0_ledger
            .retain(|(date, _)| *date != previous);
        evidence.soil.et0_ledger.push((previous, et0));
    }
    let rain = if first {
        // Credit only the still-future hours today. A provider's full-day
        // forecast must never be added on top of rain already measured today.
        let hours = ((end - now).max(0) / 3600) as usize;
        if hours == 0 {
            Some(0.0)
        } else {
            forecast.planning_precip_weighted_in(hours, now)
        }
    } else {
        weather.and_then(|d| d.precip_sum_in.map(|rain| rain * d.precip_weight()))
    }
    .map(|rain| crate::units::in_to_mm(rain * evidence.bias.multiplier_for(previous.month())));
    *complete &= rain.is_some();
    if let Some(last) = evidence.soil.rain_mm.last_mut() {
        *last += rain.unwrap_or(0.0);
    }
    // Today's automatic window may already have passed; never manufacture a
    // second run later today in the forward balance.
    let target = crate::engine::sunrise::smart_morning_target_start(
        previous,
        policy.location.0,
        policy.location.1,
        planned_wall(plan, policy),
        policy.calendar,
    )
    .map(|t| t.timestamp());
    if !first || target.is_some_and(|t| t > now) {
        for zone in &plan.zones {
            let seconds = executable_seconds(zone);
            if seconds == 0 {
                continue;
            }
            let stamp = target.unwrap_or(start);
            evidence
                .soil
                .run_segments
                .entry(zone.slug.clone())
                .or_default()
                .push(RunSegment {
                    session_id: Some(format!("projection:{previous}:{}", zone.slug)),
                    start_epoch: stamp,
                    end_epoch: stamp + i64::from(seconds),
                });
            let applied = evidence
                .soil
                .applied_valve_s
                .entry(zone.slug.clone())
                .or_insert_with(|| vec![0; evidence.soil.dates.len()]);
            if let Some(last) = applied.last_mut() {
                *last += i64::from(seconds);
            }
        }
    }
    let next = previous.succ_opt().unwrap();
    evidence.soil.dates.push(next);
    evidence.soil.rain_mm.push(0.0);
    evidence.soil.today_partial_et0_mm = Some(0.0);
    for zone in &plan.zones {
        evidence
            .soil
            .applied_valve_s
            .entry(zone.slug.clone())
            .or_insert_with(|| vec![0; evidence.soil.dates.len() - 1])
            .push(0);
    }
    let length = evidence.soil.dates.len();
    evidence.observed_rain_days_mm = evidence.soil.rain_mm[length.saturating_sub(7)..].to_vec();
    evidence.observed_rain_mm = evidence.observed_rain_days_mm.iter().sum();
    // Model credits are explicitly confined to this clone.
    evidence.observed_rain_source = "projection".into();
    for (slug, segments) in &evidence.soil.run_segments {
        let applied = applied_in_window(segments, end - 7 * 86_400, end);
        let last = cluster_events(segments)
            .into_iter()
            .max_by_key(|e| e.end_epoch);
        evidence.per_zone.insert(
            slug.clone(),
            crate::refresher::ZoneRunEvidence {
                applied_open_s: applied.valve_open_s,
                sessions_done: applied.events,
                last_run_epoch: last.as_ref().map(|e| e.end_epoch).unwrap_or(0),
                last_session_open_s: last.map(|e| e.valve_open_s),
            },
        );
    }
}
