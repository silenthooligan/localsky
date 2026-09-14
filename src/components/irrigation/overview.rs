//! A glanceable projection of recorded outcomes and the shared engine outlook.
//! No watering decisions are recomputed here; detailed evidence has its own route.
use super::daily::{recorded_morning, MorningHistory};
use crate::history::types::HistoryWindow;
use crate::model::{IrrigationSnapshot, WaterPlanDay};
use crate::timefmt::{day_key_in_tz, format_hm};
use leptos::prelude::*;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Status {
    pub headline: String,
    pub detail: String,
    pub tone: &'static str,
    pub icon: &'static str,
}

fn status(
    headline: impl Into<String>,
    detail: impl Into<String>,
    tone: &'static str,
    icon: &'static str,
) -> Status {
    Status {
        headline: headline.into(),
        detail: detail.into(),
        tone,
        icon,
    }
}

/// Condense stable reason codes, retaining distinct zone reasons without exposing
/// the engine's multi-paragraph water-budget explanation on the dashboard.
pub(super) fn short_reasons<'a>(reasons: impl Iterator<Item = (&'a str, &'a str)>) -> String {
    let mut labels = Vec::new();
    for (code, reason) in reasons {
        let label = match code {
            _ if crate::gates_catalog::GateFamily::of(code, reason)
                == crate::gates_catalog::GateFamily::NoData =>
            {
                "Weather or sensor data unavailable"
            }
            "soil_not_due" => "Soil has enough water",
            "water_balance" => "Water balance calls for no run",
            "restrictions" => "Outside allowed watering days",
            "already_wet" | "rain_now" => "Recent rain",
            "tomorrow_rain" | "rain_today_forecast" | "rain_next_4h" | "rain_3day" => {
                "Rain forecasted"
            }
            _ => match crate::gates_catalog::skip_phrase(code, reason) {
                "recent rain" => "Recent rain",
                "rain forecast" => "Rain forecasted",
                "soil still moist" => "Soil has enough water",
                _ => crate::gates_catalog::skip_phrase(code, reason),
            },
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        return "See the recorded zone decisions.".into();
    }
    if labels.len() > 2 {
        return "Zones are holding for different reasons. View decisions.".into();
    }
    let text = labels.join(" · ");
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => text,
    }
}

pub(super) fn projected(day: Option<&WaterPlanDay>, tz: &str) -> Status {
    let Some(day) = day else {
        return status(
            "Awaiting forecast",
            "Projection is not available yet.",
            "off",
            "cloud-sun",
        );
    };
    // The scenario publishes executable seconds after soil, weather, restrictions
    // and safety. A weather exemption flag is not evidence of a watering run.
    let watering: Vec<_> = day.zones.iter().filter(|z| z.planned_seconds > 0).collect();
    if day.zones.is_empty() {
        return status(
            "Awaiting plan",
            "Zone decisions are not available yet.",
            "off",
            "cloud-sun",
        );
    }
    if watering.is_empty() {
        let uncertain = day.zones.iter().any(|z| {
            crate::gates_catalog::GateFamily::of(&z.reason_code, &z.reason)
                == crate::gates_catalog::GateFamily::NoData
        });
        return status(
            if uncertain {
                "Awaiting data"
            } else {
                "Not watering"
            },
            short_reasons(
                day.zones
                    .iter()
                    .map(|z| (z.reason_code.as_str(), z.reason.as_str())),
            ),
            if uncertain { "off" } else { "skip" },
            if uncertain {
                "alert-triangle"
            } else {
                "sprinkler-off"
            },
        );
    }
    let seconds: u64 = watering.iter().map(|z| u64::from(z.planned_seconds)).sum();
    let duration = if seconds < 60 {
        "<1 min".into()
    } else {
        format!("{} min", (seconds + 30) / 60)
    };
    let count = watering.len();
    let timing = day
        .start_epoch
        .map(|epoch| format!(" · starts {}", format_hm(epoch, tz)))
        .unwrap_or_default();
    status(
        "Watering expected",
        format!(
            "{count} {} · {duration}{timing}",
            if count == 1 { "zone" } else { "zones" }
        ),
        "run",
        "sprinkler",
    )
}

fn today(s: &IrrigationSnapshot, history: Option<&Result<HistoryWindow, String>>) -> Status {
    if matches!(
        super::hero::resolve_phase(s),
        super::hero::HeroPhase::Offline
    ) {
        return status(
            "Status unavailable",
            "Waiting for a fresh system update.",
            "off",
            "alert-triangle",
        );
    }
    if s.zones
        .iter()
        .any(|z| !z.running_known || z.ledger_running && !z.running)
    {
        return status(
            "Checking valves",
            "Waiting for controller confirmation.",
            "off",
            "alert-triangle",
        );
    }
    let running: Vec<_> = s.zones.iter().filter(|z| z.running).collect();
    if let Some(first) = running.first() {
        let names = if running.len() > 1 {
            format!("{} + {} more", first.name, running.len() - 1)
        } else {
            first.name.clone()
        };
        return status("Watering now", names, "run", "sprinkler");
    }
    let history = match history {
        Some(Ok(history)) => history,
        Some(Err(_)) => {
            return status(
                "History unavailable",
                "Today's outcome could not be loaded. Open the Daily log to retry.",
                "off",
                "alert-triangle",
            )
        }
        None => {
            return status(
                "Loading today",
                "Checking the morning record.",
                "off",
                "cloud-sun",
            )
        }
    };
    if let Some(record) = recorded_morning(&history.runs, s.last_refresh_epoch, &s.timezone) {
        let detail = if record.headline.contains("check run details")
            || record.headline.contains("awaiting")
        {
            "Open the Daily log to check the run outcome.".into()
        } else if record.headline.starts_with("Watered") {
            "Automatic morning run recorded.".into()
        } else {
            short_reasons(record.reasons.iter().map(|r| ("", r.as_str())))
        };
        let (tone, icon) = match record.state {
            super::daily::MorningState::Watered => ("run", "sprinkler"),
            super::daily::MorningState::Held => ("skip", "sprinkler-off"),
            super::daily::MorningState::Unconfirmed => ("off", "alert-triangle"),
        };
        return status(record.headline, detail, tone, icon);
    }
    let date = day_key_in_tz(s.last_refresh_epoch, &s.timezone);
    if let Some(day) = history.daily.iter().find(|d| d.date_local == date) {
        let detail = short_reasons(
            day.zones
                .iter()
                .map(|z| (z.reason_code.as_str(), z.reason.as_str())),
        );
        match day.kind.as_str() {
            "scheduled" | "scheduled_legacy"
                if !day.zones.is_empty()
                    && day.zones.iter().all(|z| {
                        if day.kind == "scheduled_legacy" {
                            z.reason_code == "soil_not_due"
                        } else {
                            z.planned_seconds == 0
                        }
                    }) =>
            {
                return status("Skipped", detail, "skip", "sprinkler-off")
            }
            "scheduled" | "scheduled_legacy" => {
                return status(
                    "Morning recorded",
                    "Open the Daily log to check watering delivery.",
                    "off",
                    "alert-triangle",
                )
            }
            "missed_window" => {
                return status(
                    "Morning missed",
                    "The automatic watering window was missed.",
                    "off",
                    "cloud-sun",
                )
            }
            _ => {} // A refresh-time hold is not a completed morning outcome.
        }
    }
    if let Some(day) = s.water_plan.iter().find(|d| d.day_offset == 0) {
        let mut result = projected(Some(day), &s.timezone);
        if day.zones.is_empty() {
            return result;
        }
        result.headline = if day.zones.iter().any(|z| z.planned_seconds > 0) {
            "Scheduled"
        } else {
            "Holding"
        }
        .into();
        return result;
    }
    status(
        "No morning record",
        "No automatic watering outcome recorded yet.",
        "off",
        "cloud-sun",
    )
}

#[component]
pub fn IrrigationOverview(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let history = expect_context::<MorningHistory>().0;
    let current = Memo::new(move |_| today(&snap.get(), history.get().as_ref()));
    let tomorrow = Memo::new(move |_| {
        let s = snap.get();
        projected(s.water_plan.iter().find(|d| d.day_offset == 1), &s.timezone)
    });
    view! {
        <section class="next-run-hero irrigation-overview"
            class:hero-run=move || current.get().tone == "run"
            class:hero-skip=move || current.get().tone == "skip"
            class:hero-off=move || current.get().tone == "off">
            <div class="irrigation-overview__today">
                <div class="next-run-glyph" aria-hidden="true">{move || view! { <crate::components::ui::Icon name=current.get().icon size=48/> }}</div>
                <div class="next-run-eyebrow">"TODAY"</div>
                <h1 class="next-run-headline">{move || current.get().headline}</h1>
                <p class="next-run-tag">{move || current.get().detail}</p>
            </div>
            <div class="irrigation-overview__tomorrow" data-watering=move || tomorrow.get().tone>
                <div class="irrigation-overview__label"><span>"TOMORROW"</span><span>"Projected"</span></div>
                <span class="irrigation-overview__outlook-icon">{move || view! { <crate::components::ui::Icon name=tomorrow.get().icon size=30/> }}</span>
                <strong>{move || tomorrow.get().headline}</strong>
                <p>{move || tomorrow.get().detail}</p>
            </div>
            {move || snap.get().force_overrode_guard.map(|guard| view! {
                <p class="hero-forced-warn" role="status">{format!("Force bypasses {guard}. Safety checks still apply.")}</p>
            })}
            <nav class="irrigation-overview__links" aria-label="Watering details">
                <a href=crate::base::url("/irrigation/decisions")>"Watering decisions →"</a>
                <a href=crate::base::url("/history?view=daily")>"Daily log →"</a>
            </nav>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WaterPlanZone;

    #[test]
    fn a_projection_counts_only_executable_zones_and_never_times_a_skip() {
        assert_eq!(
            short_reasons(
                [(
                    "rain_today_forecast",
                    "Rain forecast unavailable; watering held"
                )]
                .into_iter()
            ),
            "Weather or sensor data unavailable"
        );
        let mut day = WaterPlanDay {
            start_epoch: Some(1789286400),
            zones: vec![
                WaterPlanZone {
                    planned_seconds: 90,
                    ..Default::default()
                },
                WaterPlanZone {
                    planned_seconds: 0,
                    reason_code: "restrictions".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            projected(Some(&day), "UTC").detail,
            "1 zone · 2 min · starts 08:00"
        );
        day.zones[0].planned_seconds = 0;
        assert_eq!(projected(Some(&day), "UTC").headline, "Not watering");
        assert!(!projected(Some(&day), "UTC").detail.contains("08:00"));
    }

    #[test]
    fn refresh_holds_are_not_reported_as_a_completed_skip() {
        let s = IrrigationSnapshot {
            ha_reachable: true,
            last_refresh_epoch: 1789210317,
            timezone: "America/New_York".into(),
            water_plan: vec![WaterPlanDay {
                zones: vec![WaterPlanZone {
                    reason_code: "already_wet".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut h = HistoryWindow {
            daily: vec![crate::history::types::DailyDecision {
                date_local: "2026-09-12".into(),
                kind: "recorded_decision".into(),
                zones: vec![crate::history::types::DailyZoneDecision {
                    reason_code: "soil_not_due".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(today(&s, Some(&Ok(h.clone()))).headline, "Holding");
        h.daily[0].kind = "scheduled_legacy".into();
        assert_eq!(today(&s, Some(&Ok(h))).headline, "Skipped");
        assert_eq!(
            today(&s, Some(&Err("offline".into()))).headline,
            "History unavailable"
        );
    }
}
