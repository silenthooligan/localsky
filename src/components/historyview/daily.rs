use crate::history::types::{DailyDecision, HistoryWindow, RunRecord};
use crate::timefmt::day_key_in_tz;
use leptos::prelude::*;

#[derive(Default)]
struct DayLog {
    date: String,
    runs: Vec<RunRecord>,
    decisions: Vec<DailyDecision>,
}

fn days(window: &HistoryWindow, tz: &str, month: Option<(i32, u32)>) -> Vec<DayLog> {
    let bounds = month.map(|(y, m)| super::month_bounds(y, m, tz));
    let included = |epoch| bounds.is_none_or(|(lo, hi)| epoch >= lo && epoch < hi);
    let mut days = std::collections::BTreeMap::<String, DayLog>::new();
    for run in &window.runs {
        if included(run.start_epoch) {
            let date = day_key_in_tz(run.start_epoch, tz);
            days.entry(date.clone())
                .or_insert_with(|| DayLog {
                    date,
                    ..Default::default()
                })
                .runs
                .push(run.clone());
        }
    }
    for decision in &window.daily {
        if included(decision.epoch) {
            days.entry(decision.date_local.clone())
                .or_insert_with(|| DayLog {
                    date: decision.date_local.clone(),
                    ..Default::default()
                })
                .decisions
                .push(decision.clone());
        }
    }
    days.into_values().rev().collect()
}

#[component]
pub(super) fn DailyLog(
    window: RwSignal<HistoryWindow>,
    loaded: RwSignal<bool>,
    error: RwSignal<bool>,
    tz: RwSignal<String>,
    month: Signal<Option<(i32, u32)>>,
    query: RwSignal<String>,
) -> impl IntoView {
    let visible = RwSignal::new(14usize);
    view! {
        <div class="daily-log">
            {move || {
                if !loaded.get() { return view! { <crate::components::ui::SkeletonRows count=3/> }.into_any(); }
                if error.get() { return view! { <p role="alert">"Daily records could not be loaded."</p> }.into_any(); }
                let mut logs = days(&window.get(), &tz.get(), month.get());
                let query = query.get().trim().to_lowercase().replace('_', " ");
                if !query.is_empty() {
                    logs.retain(|d| d.date.contains(&query) || d.runs.iter().any(|r| r.zone.replace('_', " ").to_lowercase().contains(&query)
                        || r.skip_reason.as_deref().is_some_and(|reason| reason.to_lowercase().contains(&query)))
                        || d.decisions.iter().flat_map(|d| &d.zones).any(|z| z.name.to_lowercase().contains(&query) || z.reason.to_lowercase().contains(&query)));
                }
                if logs.is_empty() { return view! { <p>"No daily decisions or watering events recorded in this range."</p> }.into_any(); }
                let more = logs.len() > visible.get();
                view! {
                    <div class="daily-log__list">{logs.into_iter().take(visible.get()).map(|day| {
                        let seconds: i64 = crate::history::rollup::watering_intervals_per_zone(&day.runs).values().flatten().map(|r| r.valve_open_s).sum();
                        let automatic = day.runs.iter().any(|r| r.source == "smart_morning" && crate::history::rollup::is_watering_record(r));
                        let legacy = day.decisions.iter().any(|d| d.kind == "recorded_decision");
                        let mut reasons: Vec<String> = day.decisions.iter().flat_map(|d| &d.zones).map(|z| z.reason.clone())
                            .chain(day.runs.iter().filter_map(|r| r.skip_reason.clone())).collect();
                        reasons.sort(); reasons.dedup();
                        let headline = if seconds > 0 {
                            format!("{:.1} min watered{}", seconds as f64 / 60.0, if automatic { " · automatic run recorded" } else { " · manual or controller watering" })
                        } else if day.decisions.is_empty() { "No watering recorded".into() }
                        else { "No watering recorded · daily decisions available".into() };
                        view! {
                            <details class="daily-log__day">
                                <summary><time>{day.date}</time><strong>{headline}</strong><span>{format!("{} reason{}", reasons.len(), if reasons.len() == 1 { "" } else { "s" })}</span></summary>
                                <div class="daily-log__body">
                                    {reasons.into_iter().map(|reason| view! { <p>{reason}</p> }).collect_view()}
                                    {day.decisions.iter().find(|d| matches!(d.kind.as_str(), "scheduled" | "scheduled_legacy")).map(|d| view! { <small>{format!("Morning decision recorded at {}", crate::timefmt::format_hm(d.epoch, &tz.get()))}</small> })}
                                    {legacy.then(|| view! { <small>"Older engine hold records are retained here. They show the decision at the time recorded; they do not prove that a scheduled valve run was skipped."</small> })}
                                    {(!day.runs.is_empty()).then(|| view! { <small>"The Run log view contains the original sessions, cycles and delivery records."</small> })}
                                </div>
                            </details>
                        }
                    }).collect_view()}</div>
                    {more.then(|| view! { <crate::components::ui::Button variant="secondary" size="sm" on_click=Callback::new(move |_| visible.update(|n| *n += 30))>"Show 30 older days"</crate::components::ui::Button> })}
                }.into_any()
            }}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn waterless_decisions_and_older_months_are_not_dropped() {
        let window = HistoryWindow {
            runs: vec![RunRecord {
                start_epoch: 1778407557,
                duration_s: 60,
                ..Default::default()
            }],
            daily: vec![DailyDecision {
                date_local: "2026-09-12".into(),
                epoch: 1789210317,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(days(&window, "America/New_York", None).len(), 2);
        assert_eq!(days(&window, "America/New_York", Some((2026, 9))).len(), 1);
    }
}
