//! FAO-56 root-zone scheduling with a forecast horizon. Rain is credited after
//! the day's demand (conservative about arrival time), capped by storage. Wait
//! only while depletion stays below RAW; otherwise deliver a bridge to useful
//! rain, or a refill when rain cannot help. Daily demand and legal opportunities
//! are inputs, so climate, season, roots and location determine the result.
use super::soil_schedule::{rain_effectiveness, size_refill, SoilZonePlan, ZoneSoilParams};

#[derive(Debug, Clone)]
pub struct OutlookDay {
    pub demand_mm: f64,
    pub rain_mm: Option<f64>,
    pub can_water: bool,
}

pub fn apply(
    plan: &mut SoilZonePlan,
    params: &ZoneSoilParams,
    days: &[OutlookDay],
    delivered_mm: f64,
) {
    if days.is_empty()
        || plan.evidence_days < super::soil_schedule::MIN_EVIDENCE_DAYS
        || params.dormancy().is_some()
        || days[0].rain_mm.is_none()
    {
        return;
    }
    if plan.initial_uncertainty_mm > super::soil_schedule::RESOLVED_UNCERTAINTY_MM {
        let uncertainty = plan.initial_uncertainty_mm;
        let lower = plan.depletion_mm;
        let mut wet = plan.clone();
        wet.initial_uncertainty_mm = 0.0;
        let mut dry = wet.clone();
        dry.depletion_mm += uncertainty;
        apply(&mut wet, params, days, delivered_mm);
        apply(&mut dry, params, days, delivered_mm);
        if wet.planned_seconds == 0 && dry.planned_seconds > 0 {
            wet.planned_seconds = 0;
            wet.due = true;
            wet.planning_reason = Some("The soil estimate spans the watering threshold; automatic watering is held until rain, water-use history or a calibrated reading resolves the need".into());
        }
        wet.initial_uncertainty_mm = uncertainty;
        wet.planning_reason = Some(format!("{} Estimated depletion is {:.1}–{:.1} mm; the starting soil state is not yet fully resolved.",
            wet.planning_reason.as_deref().unwrap_or("Water need estimated from the root-zone balance."), lower, lower + uncertainty));
        *plan = wet;
        return;
    }
    // Today's decision must protect the planting until the next legal morning.
    // We do not pretend a restricted day is another chance to irrigate.
    let opportunity = days
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, d)| d.can_water)
        .map(|(i, _)| i)
        .unwrap_or(days.len());
    // A missing day before the next legal morning can change both whether
    // water is needed and the bridge volume. It is not a forecast of no rain.
    // Beyond that opportunity, missing days earn no speculative rain credit.
    if days
        .iter()
        .take(opportunity)
        .any(|day| day.rain_mm.is_none())
    {
        let reason = "Rain coverage is incomplete before the next legal watering morning; automatic watering is held".to_string();
        plan.planned_seconds = 0;
        plan.deferred_kind = Some(super::soil_schedule::SoilDeferKind::ForecastUnavailable);
        plan.deferred_reason = Some(reason.clone());
        plan.planning_reason = Some(reason);
        plan.hold_is_forecast_rain = false;
        return;
    }
    let peak = peak_after(plan, params, days, opportunity, 0.0);
    if peak <= plan.raw_mm + 1e-9 {
        plan.due = false;
        plan.planned_seconds = 0;
        plan.deferred_reason = None;
        plan.deferred_kind = None;
        // Was the forecast rain what held this zone, or did it simply need
        // nothing? Replay the same horizon with the rain taken out: if
        // demand alone would have crossed the trigger, this hold IS the
        // soil model counting forecast rain against the deficit -- exactly
        // what the inert forward-rain gates defer to. Recorded, not acted
        // on; see SoilZonePlan::hold_is_forecast_rain.
        let dry: Vec<OutlookDay> = days
            .iter()
            .map(|d| OutlookDay {
                demand_mm: d.demand_mm,
                rain_mm: d.rain_mm.map(|_| 0.0),
                can_water: d.can_water,
            })
            .collect();
        plan.hold_is_forecast_rain =
            peak_after(plan, params, &dry, opportunity, 0.0) > plan.raw_mm + 1e-9;
        plan.planning_reason = Some(format!(
            "No watering needed: root-zone depletion is {:.1} mm; projected demand and rain keep it below the {:.1} mm trigger until the next legal morning",
            plan.depletion_mm, plan.raw_mm,
        ));
        return;
    }
    // The next legal morning decides whether water is needed now. Useful rain
    // can arrive AFTER that morning: size a bridge through its arrival instead
    // of needlessly filling the soil just because tomorrow is also legal.
    let mut depletion = plan.depletion_mm;
    let mut covered = true;
    let mut useful_rain = None;
    for (index, day) in days.iter().enumerate() {
        covered &= day.rain_mm.is_some();
        let before = depletion + day.demand_mm.max(0.0);
        let rain = captured_rain(params, day);
        depletion = (before - rain).clamp(0.0, plan.taw_mm);
        if covered && rain > 0.0 && before >= plan.raw_mm && depletion < plan.raw_mm {
            useful_rain = Some(index);
            break;
        }
    }
    let best_peak = peak_after(plan, params, days, opportunity, plan.depletion_mm);
    let mut bridge = None;
    if let Some(index) = useful_rain {
        bridge = minimum_refill(plan, params, days, opportunity.max(index + 1));
        // A filling storm can wash out today's credit before a long legal gap.
        // If no refill can protect that later gap, do not waste water before
        // the storm. Bridge the avoidable early stress only when doing so is
        // no worse for the later gap than a full refill.
        if bridge.is_none() && index < opportunity {
            bridge = minimum_refill(plan, params, days, index + 1).filter(|amount| {
                peak_after(plan, params, days, opportunity, *amount) <= best_peak + 1e-6
            });
        }
    }
    plan.due = true;
    plan.deferred_reason = None;
    plan.deferred_kind = None;
    plan.defer_bound_reached = false;
    let refill = bridge.unwrap_or(plan.depletion_mm);
    let sized = size_refill(refill, params, delivered_mm);
    plan.planned_seconds = sized.planned_seconds;
    plan.session_capped = sized.session_capped;
    plan.ceiling_binding = sized.ceiling_binding;
    plan.ceiling_reason = sized.ceiling_reason;
    plan.planning_reason = Some(if bridge.is_some() {
        format!("Rain is forecast, but waiting without water would cross the {:.1} mm stress trigger; {:.1} mm net bridges the gap while leaving storage for rain", plan.raw_mm, refill)
    } else {
        format!("Projected demand would cross the {:.1} mm trigger before the next legal morning; refill {:.1} mm of root-zone depletion", plan.raw_mm, refill)
    });
    if best_peak > plan.raw_mm + 1e-6 {
        plan.planning_reason = Some(format!(
            "{} Even a full refill leaves projected depletion at {:.1} mm before the next legal morning, above the {:.1} mm trigger. Available root-zone storage and permitted watering frequency cannot cover the demand; more water now would drain below the roots.",
            plan.planning_reason.as_deref().unwrap_or_default(), best_peak, plan.raw_mm,
        ));
    }
}

fn captured_rain(params: &ZoneSoilParams, day: &OutlookDay) -> f64 {
    let rain = day.rain_mm.unwrap_or(0.0).max(0.0);
    params
        .explicit_rain_cap_mm
        .filter(|cap| *cap > 0.0)
        .map(|cap| rain.min(cap))
        .unwrap_or(rain)
        * rain_effectiveness(params)
}

/// Replaying each candidate preserves drainage: water applied before a filling
/// storm cannot remain as credit after that storm. Missing rain earns no credit.
fn peak_after(
    plan: &SoilZonePlan,
    params: &ZoneSoilParams,
    days: &[OutlookDay],
    length: usize,
    refill: f64,
) -> f64 {
    let mut depletion = (plan.depletion_mm - refill).max(0.0);
    let mut peak = depletion;
    for day in days.iter().take(length) {
        depletion += day.demand_mm.max(0.0);
        peak = peak.max(depletion);
        depletion = (depletion - captured_rain(params, day)).clamp(0.0, plan.taw_mm);
    }
    peak
}

/// Monotonic bounded solve, in net millimetres. None means even filling the
/// current root-zone deficit cannot protect the entire requested interval.
fn minimum_refill(
    plan: &SoilZonePlan,
    params: &ZoneSoilParams,
    days: &[OutlookDay],
    length: usize,
) -> Option<f64> {
    let safe = |amount| peak_after(plan, params, days, length, amount) <= plan.raw_mm + 1e-9;
    if safe(0.0) {
        return Some(0.0);
    }
    let (mut low, mut high) = (0.0, plan.depletion_mm);
    if !safe(high) {
        return None;
    }
    for _ in 0..40 {
        let middle = (low + high) / 2.0;
        if safe(middle) {
            high = middle;
        } else {
            low = middle;
        }
    }
    Some(high)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{GrassSpecies, SoilTexture, SprinklerType};
    fn params() -> ZoneSoilParams {
        ZoneSoilParams {
            slug: "garden".into(),
            species: GrassSpecies::StAugustine,
            texture: SoilTexture::SandyLoam,
            root_depth_mm: Some(150.0),
            mad_pct: Some(0.5),
            latitude_deg: 30.0,
            capture_efficiency: 0.7,
            sprinkler_type: SprinklerType::Spray,
            soil_temp_f: None,
            throughput_mm_hr: 38.0,
            max_dur_s: 3600,
            explicit_rain_cap_mm: None,
            explicit_weekly_budget_in: None,
        }
    }
    fn plan(depletion: f64) -> SoilZonePlan {
        SoilZonePlan {
            depletion_mm: depletion,
            raw_mm: 10.0,
            taw_mm: 20.0,
            evidence_days: 14,
            ..Default::default()
        }
    }
    fn day(demand: f64, rain: f64, can_water: bool) -> OutlookDay {
        OutlookDay {
            demand_mm: demand,
            rain_mm: Some(rain),
            can_water,
        }
    }
    /// A hold that only exists BECAUSE rain is coming is the soil model
    /// doing the very thing the inert forward-rain gates defer to. It has
    /// to be distinguishable afterwards from a zone that simply needed
    /// nothing, or `soil_morning_decisions` records both as "not due" --
    /// which is why the 2026-09-20 morning could not explain itself.
    #[test]
    fn a_hold_that_depends_on_forecast_rain_says_so() {
        // Depletion 5.0 against a 10.0 trigger, 3 mm/day of demand, and
        // tomorrow is NOT a legal morning -- so the decision has to protect
        // two days, and day 1's demand would take it to 11.0. Day 0's rain
        // is credited after day 0's own demand (the module is deliberately
        // conservative about arrival time) but lands before day 1, which is
        // what keeps the peak under the trigger.
        let mut p = plan(5.0);
        apply(
            &mut p,
            &params(),
            &[
                day(3.0, 12.0, true),
                day(3.0, 0.0, false),
                day(3.0, 0.0, true),
            ],
            0.0,
        );
        assert!(!p.due, "rain keeps it under the trigger");
        assert_eq!(p.planned_seconds, 0);
        assert!(
            p.hold_is_forecast_rain,
            "demand alone would have crossed the trigger; the rain is what held it"
        );
        // It must NOT reach for the defer machinery: deferred_kind feeds
        // consecutive_defers and MAX_CONSECUTIVE_DEFERS, and this flag is
        // observational.
        assert_eq!(p.deferred_kind, None);
        assert_eq!(p.deferred_reason, None);
    }

    /// The opposite case: a zone comfortably under its trigger needs no
    /// rain to stay there, so the hold is an ordinary not-due morning.
    #[test]
    fn a_hold_that_needs_no_rain_is_not_a_rain_hold() {
        let mut p = plan(0.5);
        apply(
            &mut p,
            &params(),
            &[
                day(1.0, 6.0, true),
                day(1.0, 0.0, true),
                day(1.0, 0.0, true),
            ],
            0.0,
        );
        assert!(!p.due);
        assert!(
            !p.hold_is_forecast_rain,
            "demand alone never crosses the trigger; rain was not what held it"
        );
    }

    /// A zone that waters is not a hold at all, whatever the forecast.
    #[test]
    fn a_watering_morning_is_never_flagged_as_a_rain_hold() {
        let mut p = plan(12.0);
        apply(
            &mut p,
            &params(),
            &[day(4.0, 0.0, true), day(4.0, 0.0, true)],
            0.0,
        );
        assert!(p.due);
        assert!(p.planned_seconds > 0);
        assert!(!p.hold_is_forecast_rain);
    }

    #[test]
    fn recent_storm_and_rain_ahead_do_not_request_water() {
        let mut p = plan(0.0);
        apply(
            &mut p,
            &params(),
            &[
                day(3.0, 8.0, true),
                day(2.0, 12.0, false),
                day(3.0, 0.0, true),
            ],
            0.0,
        );
        assert_eq!(p.planned_seconds, 0);
        assert!(!p.due);
    }
    #[test]
    fn a_hot_day_requires_water_before_the_next_legal_opportunity() {
        let mut p = plan(6.0);
        apply(
            &mut p,
            &params(),
            &[
                day(3.0, 0.0, true),
                day(3.0, 0.0, false),
                day(3.0, 0.0, true),
            ],
            0.0,
        );
        assert!(p.planned_seconds > 0);
    }
    #[test]
    fn rain_coming_later_requires_only_a_bridge_when_waiting_would_stress_roots() {
        let mut wet = plan(9.0);
        let mut dry = wet.clone();
        apply(
            &mut wet,
            &params(),
            &[day(3.0, 15.0, true), day(3.0, 0.0, true)],
            0.0,
        );
        apply(
            &mut dry,
            &params(),
            &[day(3.0, 0.0, true), day(3.0, 0.0, true)],
            0.0,
        );
        assert!(wet.planned_seconds > 0);
        assert!(wet.planned_seconds < dry.planned_seconds);
    }
    #[test]
    fn useful_rain_tomorrow_sizes_a_bridge_even_when_tomorrow_is_legal() {
        let mut wet = plan(9.0);
        let mut dry = wet.clone();
        let days = [day(3.0, 0.0, true), day(3.0, 15.0, true)];
        let amount = minimum_refill(&wet, &params(), &days, 2).unwrap();
        assert!((amount - 5.0).abs() < 1e-6);
        apply(&mut wet, &params(), &days, 0.0);
        apply(
            &mut dry,
            &params(),
            &[day(3.0, 0.0, true), day(3.0, 0.0, true)],
            0.0,
        );
        assert!(wet.planned_seconds > 0 && wet.planned_seconds < dry.planned_seconds);
        assert!(wet.planning_reason.unwrap().contains("5.0 mm net bridges"));
    }
    #[test]
    fn a_filling_storm_erases_irrigation_credit_before_a_later_legal_gap() {
        let mut p = plan(9.0);
        let days = [
            day(3.0, 40.0, true),
            day(7.0, 0.0, false),
            day(7.0, 0.0, false),
            day(3.0, 0.0, true),
        ];
        assert!(minimum_refill(&p, &params(), &days, 3).is_none());
        assert!((peak_after(&p, &params(), &days, 3, 9.0) - 14.0).abs() < 1e-6);
        apply(&mut p, &params(), &days, 0.0);
        let reason = p.planning_reason.unwrap();
        assert!(reason.contains("2.0 mm net bridges"));
        assert!(reason.contains("storage and permitted watering frequency cannot cover"));
    }
    #[test]
    fn a_missing_forecast_day_cannot_earn_a_bridge_to_later_rain() {
        let mut p = plan(9.0);
        let mut dry = p.clone();
        let days = [
            day(3.0, 0.0, true),
            OutlookDay {
                demand_mm: 3.0,
                rain_mm: None,
                can_water: true,
            },
            day(3.0, 40.0, true),
        ];
        apply(&mut p, &params(), &days, 0.0);
        apply(&mut dry, &params(), &[day(3.0, 0.0, true)], 0.0);
        assert_eq!(p.planned_seconds, dry.planned_seconds);
    }
    #[test]
    fn failed_rain_increases_needed_water_without_a_three_day_stress_allowance() {
        let mut first = plan(10.0);
        let mut missed = plan(17.0);
        let days = [day(4.0, 20.0, true), day(4.0, 0.0, true)];
        apply(&mut first, &params(), &days, 0.0);
        apply(&mut missed, &params(), &days, 0.0);
        assert!(first.planned_seconds > 0);
        assert!(missed.planned_seconds > first.planned_seconds);
    }
    #[test]
    fn next_weeks_storm_does_not_cancel_a_dry_hot_day_now() {
        let mut p = plan(9.0);
        apply(
            &mut p,
            &params(),
            &[
                day(4.0, 0.0, true),
                day(4.0, 0.0, true),
                day(4.0, 50.0, true),
            ],
            0.0,
        );
        assert!(p.planned_seconds > 0);
    }
    #[test]
    fn missing_rain_before_a_restricted_next_opportunity_holds_the_plan() {
        let mut plan = plan(12.0);
        let days = vec![
            day(2.0, 0.0, true),
            OutlookDay {
                demand_mm: 2.0,
                rain_mm: None,
                can_water: false,
            },
            day(2.0, 0.0, true),
        ];
        apply(&mut plan, &params(), &days, 0.0);
        assert_eq!(plan.planned_seconds, 0);
        assert_eq!(
            plan.deferred_kind,
            Some(super::super::soil_schedule::SoilDeferKind::ForecastUnavailable)
        );
    }

    #[test]
    fn missing_rain_after_the_next_opportunity_does_not_block_a_known_need() {
        let mut plan = plan(12.0);
        let days = vec![
            day(2.0, 0.0, true),
            day(2.0, 0.0, true),
            OutlookDay {
                demand_mm: 2.0,
                rain_mm: None,
                can_water: true,
            },
        ];
        apply(&mut plan, &params(), &days, 0.0);
        assert!(plan.planned_seconds > 0);
        assert!(plan.deferred_kind.is_none());
    }
}
