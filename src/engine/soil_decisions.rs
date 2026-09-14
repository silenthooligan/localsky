//! Actual morning decisions used by the forecast-defer bound. A dry day is
//! water-balance evidence, not evidence that the forecast deferred a run.

use chrono::NaiveDate;

use crate::engine::soil_schedule::SoilDeferKind;
use crate::model::IrrigationSnapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MorningOutcome {
    ForecastRain,
    OtherHold,
    NotDue,
}

impl MorningOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ForecastRain => "forecast_rain",
            Self::OtherHold => "other_hold",
            Self::NotDue => "not_due",
        }
    }

    pub fn from_record(value: &str) -> Self {
        match value {
            "forecast_rain" => Self::ForecastRain,
            "not_due" => Self::NotDue,
            // An unknown/legacy reason never spends a forecast-defer allowance.
            _ => Self::OtherHold,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastMorning {
    pub date: NaiveDate,
    pub outcome: MorningOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneMorningDecision {
    pub zone_slug: String,
    pub outcome: MorningOutcome,
    pub reason_code: String,
}

/// Classify the decision at the actual scheduled morning. Callers must first
/// check timing and snapshot freshness; observing an arbitrary afternoon tick
/// would invent a morning that never happened. Final per-zone holds always win
/// over a soil plan's provisional forecast deferral.
pub fn from_snapshot(snapshot: &IrrigationSnapshot) -> Vec<ZoneMorningDecision> {
    snapshot
        .water_budgets
        .iter()
        .filter_map(|budget| {
            if budget.scheduling_model != "soil" || budget.soil_depletion_mm.is_none() {
                return None;
            }
            let zone = snapshot
                .zones
                .iter()
                .find(|zone| zone.slug == budget.zone_slug)?;
            let verdict = zone.verdict.as_ref().or_else(|| {
                snapshot
                    .zone_verdicts
                    .iter()
                    .find(|verdict| verdict.zone_slug == zone.slug)
            });
            let runnable = verdict
                .is_some_and(|verdict| matches!(verdict.verdict.as_str(), "run" | "run_extended"));
            let (outcome, reason_code) = if !budget.soil_due {
                (MorningOutcome::NotDue, "soil_not_due".into())
            } else if runnable
                && !zone
                    .smart_suppressed
                    .as_ref()
                    .is_some_and(|hold| hold.active_today)
                && zone.planned_run_seconds == 0
                && budget.soil_deferred_kind == Some(SoilDeferKind::ForecastRain)
            {
                (MorningOutcome::ForecastRain, "forecast_rain".into())
            } else {
                (
                    MorningOutcome::OtherHold,
                    verdict
                        .map(|v| v.reason_code.clone())
                        .unwrap_or_else(|| "unknown".into()),
                )
            };
            Some(ZoneMorningDecision {
                zone_slug: zone.slug.clone(),
                outcome,
                reason_code,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{WaterBudget, ZoneState, ZoneVerdict};

    fn deferred() -> IrrigationSnapshot {
        IrrigationSnapshot {
            water_budgets: vec![WaterBudget {
                zone_slug: "front".into(),
                scheduling_model: "soil".into(),
                soil_depletion_mm: Some(12.0),
                soil_due: true,
                soil_deferred_kind: Some(SoilDeferKind::ForecastRain),
                ..Default::default()
            }],
            zones: vec![ZoneState {
                slug: "front".into(),
                verdict: Some(ZoneVerdict {
                    zone_slug: "front".into(),
                    zone_name: "Front".into(),
                    verdict: "run".into(),
                    reason: "Soil plan counts forecast rain".into(),
                    source: "soil_model".into(),
                    reason_code: "rain_next_4h".into(),
                    multiplier: 1.0,
                    value: None,
                    threshold: None,
                }),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn only_a_forecast_that_actually_holds_an_eligible_zone_counts() {
        let mut snapshot = deferred();
        assert_eq!(
            from_snapshot(&snapshot)[0].outcome,
            MorningOutcome::ForecastRain
        );
        for reason in [
            "restrictions",
            "paused",
            "dry_run",
            "freeze_now",
            "owner_script",
        ] {
            let verdict = snapshot.zones[0].verdict.as_mut().unwrap();
            verdict.verdict = "skip".into();
            verdict.reason_code = reason.into();
            assert_eq!(
                from_snapshot(&snapshot)[0].outcome,
                MorningOutcome::OtherHold,
                "{reason}"
            );
        }
        snapshot.zones[0].verdict.as_mut().unwrap().verdict = "run".into();
        snapshot.zones[0].planned_run_seconds = 300;
        assert_eq!(
            from_snapshot(&snapshot)[0].outcome,
            MorningOutcome::OtherHold,
            "a force-run floor means the forecast did not withhold water"
        );
        snapshot.zones[0].planned_run_seconds = 0;
        snapshot.zones[0].smart_suppressed = Some(crate::model::SmartSuppression {
            weekdays: vec![2],
            schedules: vec!["Owner's schedule".into()],
            active_today: true,
        });
        assert_eq!(
            from_snapshot(&snapshot)[0].outcome,
            MorningOutcome::OtherHold
        );
        snapshot.water_budgets[0].soil_due = false;
        assert_eq!(from_snapshot(&snapshot)[0].outcome, MorningOutcome::NotDue);
    }
}
