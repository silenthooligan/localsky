//! A recorded morning and a forecast are different evidence. Never reconstruct
//! a completed dispatch from the continuously changing skip-check verdict.
use crate::history::types::{HistoryWindow, RunRecord};
use crate::model::IrrigationSnapshot;
use crate::timefmt::day_key_in_tz;
use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct MorningHistory(pub RwSignal<Option<Result<HistoryWindow, String>>>);

/// One request stream owned by the page, shared across desktop/mobile renders.
pub fn provide_morning_history(snap: ReadSignal<IrrigationSnapshot>) {
    let history = RwSignal::new(None);
    provide_context(MorningHistory(history));
    #[cfg(feature = "hydrate")]
    {
        let refresh = Memo::new(move |_| {
            let s = snap.get();
            (
                s.last_refresh_epoch / 60,
                s.zones
                    .iter()
                    .map(|z| (z.slug.clone(), z.running, z.last_run_epoch))
                    .collect::<Vec<_>>(),
            )
        });
        let generation = RwSignal::new(0u64);
        Effect::new(move |_| {
            let _ = refresh.get();
            let request = generation.get_untracked() + 1;
            generation.set(request);
            leptos::task::spawn_local(async move {
                let result = async {
                    let response = gloo_net::http::Request::get("/api/irrigation/history?days=2")
                        .send()
                        .await
                        .map_err(|_| "Today's run records could not be loaded.".to_string())?;
                    if !response.ok() {
                        return Err("Today's run records could not be loaded.".to_string());
                    }
                    response
                        .json::<HistoryWindow>()
                        .await
                        .map_err(|_| "Today's run records could not be read.".to_string())
                }
                .await;
                if generation.try_get_untracked() == Some(request) {
                    history.set(Some(result));
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = snap;
}

#[derive(Debug, PartialEq)]
pub(super) struct MorningSummary {
    pub headline: String,
    pub reasons: Vec<String>,
    pub state: MorningState,
}

#[derive(Debug, PartialEq)]
pub(super) enum MorningState {
    Watered,
    Held,
    Unconfirmed,
}

pub(super) fn recorded_morning(runs: &[RunRecord], now: i64, tz: &str) -> Option<MorningSummary> {
    let today = day_key_in_tz(now, tz);
    let automatic: Vec<_> = crate::history::rollup::group_run_records(runs)
        .into_iter()
        .filter(|group| day_key_in_tz(group[0].start_epoch, tz) == today)
        .flatten()
        .filter(|r| r.source == "smart_morning")
        .collect();
    if automatic.is_empty() {
        return None;
    }
    let seconds: i64 = crate::history::rollup::watering_intervals_per_zone(&automatic)
        .values()
        .flatten()
        .map(|r| r.valve_open_s)
        .sum();
    let mut reasons: Vec<_> = automatic
        .iter()
        .filter_map(|r| r.skip_reason.clone().or_else(|| r.note.clone()))
        .filter(|r| !r.is_empty())
        .collect();
    reasons.sort();
    reasons.dedup();
    let unfinished = automatic
        .iter()
        .any(|r| !matches!(r.status.as_str(), "completed" | "skipped"));
    let headline = if seconds > 0 {
        let duration = if seconds < 60 {
            "under a minute".to_string()
        } else {
            format!("{:.0} min", seconds as f64 / 60.0)
        };
        if unfinished {
            format!("Watered {duration} · check run details")
        } else {
            format!("Watered {duration}")
        }
    } else if automatic.iter().all(|r| r.status == "skipped") {
        "Did not water".to_string()
    } else {
        "Run recorded · awaiting outcome".to_string()
    };
    if reasons.is_empty() {
        reasons.push(
            if seconds > 0 {
                "The automatic schedule ran. Recorded valve-open time excludes soak waits."
            } else {
                "A completed watering outcome has not been recorded yet."
            }
            .into(),
        );
    }
    let state = if unfinished {
        MorningState::Unconfirmed
    } else if seconds > 0 {
        MorningState::Watered
    } else if automatic.iter().all(|r| r.status == "skipped") {
        MorningState::Held
    } else {
        MorningState::Unconfirmed
    };
    Some(MorningSummary {
        headline,
        reasons,
        state,
    })
}

pub(super) fn morning(history: &HistoryWindow, now: i64, tz: &str) -> MorningSummary {
    if let Some(summary) = recorded_morning(&history.runs, now, tz) {
        return summary;
    }
    let today = day_key_in_tz(now, tz);
    match history.daily.iter().find(|d| d.date_local == today) {
        Some(day) => {
            let mut reasons: Vec<_> = day.zones.iter().map(|z| z.reason.clone()).collect();
            reasons.sort();
            reasons.dedup();
            let headline = match day.kind.as_str() {
                "scheduled" if day.zones.iter().all(|z| z.planned_seconds == 0) => {
                    "No automatic watering requested"
                }
                "scheduled_legacy" if day.zones.iter().all(|z| z.reason_code == "soil_not_due") => {
                    "No automatic watering requested"
                }
                "scheduled" => "Morning plan recorded · see run log for delivery",
                "missed_window" => "Morning window missed",
                _ => "Recorded hold decisions · no automatic run on record",
            };
            MorningSummary {
                state: if headline == "No automatic watering requested" {
                    MorningState::Held
                } else {
                    MorningState::Unconfirmed
                },
                headline: headline.into(),
                reasons,
            }
        }
        None => MorningSummary {
            state: MorningState::Unconfirmed,
            headline: "No morning outcome recorded yet".into(),
            reasons: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_manual_run_does_not_replace_the_recorded_automatic_morning() {
        let now = 1789210317;
        let h = HistoryWindow {
            runs: vec![RunRecord {
                start_epoch: now,
                source: "manual".into(),
                status: "completed".into(),
                duration_s: 60,
                ..Default::default()
            }],
            daily: vec![crate::history::types::DailyDecision {
                date_local: "2026-09-12".into(),
                epoch: now,
                kind: "scheduled_legacy".into(),
                zones: vec![crate::history::types::DailyZoneDecision {
                    reason_code: "soil_not_due".into(),
                    reason: "Roots have water".into(),
                    ..Default::default()
                }],
            }],
            ..Default::default()
        };
        let summary = morning(&h, now, "America/New_York");
        assert_eq!(summary.headline, "No automatic watering requested");
        assert_eq!(summary.reasons, vec!["Roots have water"]);
        assert_eq!(summary.state, MorningState::Held);
    }

    #[test]
    fn recorded_water_and_unconfirmed_runs_have_distinct_presentation_states() {
        let now = 1789210317;
        let mut runs = vec![RunRecord {
            start_epoch: now - 120,
            source: "smart_morning".into(),
            status: "completed".into(),
            duration_s: 120,
            ..Default::default()
        }];
        assert_eq!(
            recorded_morning(&runs, now, "UTC").unwrap().state,
            MorningState::Watered
        );
        runs[0].status = "skipped".into();
        runs[0].duration_s = 0;
        assert_eq!(
            recorded_morning(&runs, now, "UTC").unwrap().state,
            MorningState::Held
        );
        runs[0].status = "running".into();
        assert_eq!(
            recorded_morning(&runs, now, "UTC").unwrap().state,
            MorningState::Unconfirmed
        );
        runs[0].status = "completed".into();
        assert_eq!(
            recorded_morning(&runs, now, "UTC").unwrap().state,
            MorningState::Unconfirmed
        );
    }
}
