// Hero card for the irrigation page. Mirrors the Tempest hero in
// visual weight: huge mono headline, status tag, glyph. Says one of:
//
//   - "TOMORROW · 06:06" / scheduled total / each zone duration
//   - "SKIPPED" with the reason that tripped the morning skip-check
//   - "RUNNING NOW" if a zone is currently active
//   - "HA UNREACHABLE" if the refresher hasn't connected yet
//
// The detail row below the headline always carries the live skip-check
// inputs so the user can see why the system is making the call it is,
// not just the verdict.

use crate::components::irrigation::advisor::AdvisorExplanation;
use crate::ha::snapshot::IrrigationSnapshot;
use chrono::{DateTime, Local, TimeZone};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn NextRunHero(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let glyph = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "x"
        } else if s.zones.iter().any(|z| z.running) {
            "droplet"
        } else if s.skip_check.will_skip {
            // Pick a glyph that matches the skip reason at a glance.
            let r = s.skip_check.reason.as_str();
            if r.starts_with("Rain") || r.starts_with("Already wet") {
                "cloud-rain"
            } else if r.starts_with("Wind") {
                "wind"
            } else if r.starts_with("Freeze") {
                "snowflake"
            } else if r.starts_with("Paused") {
                "pause"
            } else {
                "cloud-sun"
            }
        } else {
            "droplet"
        }
    };

    let headline = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "HA UNREACHABLE".to_string()
        } else if s.zones.iter().any(|z| z.running) {
            "RUNNING NOW".to_string()
        } else if s.next_run_epoch == 0 {
            "—".to_string()
        } else {
            format_relative_time(s.next_run_epoch)
        }
    };

    let tag = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "Refresher offline".to_string()
        } else if let Some(z) = s.zones.iter().find(|z| z.running) {
            let running_count = s.zones.iter().filter(|z| z.running).count();
            if running_count > 1 {
                format!(
                    "{} running, {} of {} zones active",
                    z.name,
                    running_count,
                    s.zones.len()
                )
            } else {
                format!("{} running", z.name)
            }
        } else if s.skip_check.will_skip {
            // Plain-English: drop the SHOUTY case and lead with "Skipping".
            // The reason string itself is already a complete short sentence.
            format!("Skipping: {}", s.skip_check.reason)
        } else {
            // The "next morning run" headline reads tomorrow's epoch from
            // IU's next_start. If the engine's per-day verdict strip says
            // tomorrow is a skip (Phase C restriction blocks the weekday
            // or the seasonal window), surface that here so the user
            // doesn't see a confident "tomorrow 4:14 AM" while a
            // regulatory rule actually forbids it.
            let tomorrow = s.seven_day_verdicts.iter().find(|v| v.day_offset == 1);
            if let Some(t) = tomorrow {
                if t.verdict == "skip" {
                    return format!("Tomorrow skipped: {}", t.reason);
                }
            }
            format!(
                "Watering {} zones for {:.0} min total",
                s.zones.len(),
                s.next_run_total_minutes
            )
        }
    };

    view! {
        <section class="next-run-hero">
            <div class="next-run-glyph" aria-hidden="true">
                {move || view! { <crate::components::ui::Icon name=glyph() size=44/> }}
            </div>
            <div class="next-run-body">
                <div class="next-run-eyebrow">"Next morning run"</div>
                <h1 class="next-run-headline">{headline}</h1>
                <div class="next-run-tag" class:next-run-tag-skip=move || snap.get().skip_check.will_skip>
                    {tag}
                </div>
                {view! {
                    <AdvisorExplanation
                        verdict=Signal::derive(move || snap.get().skip_check.verdict.clone())
                    />
                }.into_any()}
                {view! { <SkipBreakdown snap/> }.into_any()}
            </div>
        </section>
    }
}

#[component]
fn SkipBreakdown(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // Each row pairs an input with its threshold and a tripped-or-not
    // marker. Always rendered so the user can see what was checked and
    // why it passed (or didn't), not just the final verdict.
    // Each row is type-erased so the breakdown's monomorphized type
    // stays flat (5 AnyViews under one div, vs. 5 deeply-nested
    // 4-span tuples that explode rustc's query-depth budget).
    view! {
        <div class="skip-breakdown" role="list">
            {view! {
                <SkipRow
                    label="Paused"
                    value=Signal::derive(move || if snap.get().skip_check.is_paused { "on".into() } else { "off".into() })
                    threshold=Signal::derive(|| "off".to_string())
                    tripped=Signal::derive(move || snap.get().skip_check.is_paused)
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Freeze"
                    value=Signal::derive(move || format!("{:.0}°F", snap.get().skip_check.temp_now_f))
                    threshold=Signal::derive(move || format!("≥ {:.0}°F", snap.get().skip_check.min_temp_f))
                    tripped=Signal::derive(move || {
                        let s = snap.get();
                        s.skip_check.temp_now_f < s.skip_check.min_temp_f
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Wind"
                    value=Signal::derive(move || format!("{:.1} mph", snap.get().skip_check.wind_now_mph))
                    threshold=Signal::derive(move || format!("≤ {:.0} mph", snap.get().skip_check.max_wind_mph))
                    tripped=Signal::derive(move || {
                        let s = snap.get();
                        s.skip_check.wind_now_mph > s.skip_check.max_wind_mph
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Wet today"
                    value=Signal::derive(move || format!("{:.2}″", snap.get().skip_check.rain_today_in))
                    threshold=Signal::derive(|| "< 0.05″".to_string())
                    tripped=Signal::derive(move || snap.get().skip_check.rain_today_in >= 0.05)
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Forecast (×prob)"
                    value=Signal::derive(move || {
                        let s = snap.get().skip_check;
                        format!(
                            "{:.2}″ × {}%",
                            s.forecast_in, s.rain_tomorrow_prob_pct
                        )
                    })
                    threshold=Signal::derive(move || format!("< {:.2}″", snap.get().skip_check.rain_skip_in))
                    tripped=Signal::derive(move || {
                        let s = snap.get().skip_check;
                        s.forecast_in * (s.rain_tomorrow_prob_pct as f64) / 100.0
                            >= s.rain_skip_in
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Next 4h"
                    value=Signal::derive(move || format!("{:.2}″", snap.get().skip_check.rain_next_4h_in))
                    threshold=Signal::derive(|| "< 0.10″".to_string())
                    tripped=Signal::derive(move || snap.get().skip_check.rain_next_4h_in >= 0.10)
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="3-day weighted"
                    value=Signal::derive(move || format!("{:.2}″", snap.get().skip_check.rain_3day_weighted_in))
                    threshold=Signal::derive(move || format!("< {:.2}″", snap.get().skip_check.rain_skip_in * 1.5))
                    tripped=Signal::derive(move || {
                        let s = snap.get().skip_check;
                        s.rain_3day_weighted_in >= 1.5 * s.rain_skip_in
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Overnight low"
                    value=Signal::derive(move || format!("{:.0}°F", snap.get().skip_check.temp_min_24h_f))
                    threshold=Signal::derive(move || format!("≥ {:.0}°F", snap.get().skip_check.min_temp_f))
                    tripped=Signal::derive(move || {
                        // Validity flag (not the old 0.0 sentinel) so a real
                        // sub-zero forecast low still trips the row.
                        let s = snap.get().skip_check;
                        s.temp_min_24h_valid && s.temp_min_24h_f < s.min_temp_f
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Heat index 3d"
                    value=Signal::derive(move || format!("{:.0}°F", snap.get().skip_check.heat_index_max_3day_f))
                    threshold=Signal::derive(|| "< 95°F".to_string())
                    tripped=Signal::derive(move || snap.get().skip_check.heat_index_max_3day_f >= 95.0)
                />
            }.into_any()}
        </div>
    }
}

#[component]
fn SkipRow(
    label: &'static str,
    value: Signal<String>,
    threshold: Signal<String>,
    tripped: Signal<bool>,
) -> impl IntoView {
    view! {
        <div class="sk-row" class:sk-row-tripped=move || tripped.get()>
            <span class="sk-mark" aria-hidden="true">
                {move || {
                    let name = if tripped.get() { "x" } else { "check" };
                    view! { <crate::components::ui::Icon name=name size=13 stroke=2.5/> }
                }}
            </span>
            <span class="sk-label">{label}</span>
            <span class="sk-value">{move || value.get()}</span>
            <span class="sk-threshold">{move || threshold.get()}</span>
        </div>
    }
}

/// Format an epoch as either "TODAY · HH:MM" or "TOMORROW · HH:MM" or
/// the date if further out. Uses the local TZ via chrono so the display
/// matches what the user sees on their HA mobile notification.
fn format_relative_time(epoch: i64) -> String {
    let dt: DateTime<Local> = match Local.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt,
        None => return "—".to_string(),
    };
    let now = Local::now();
    let today = now.date_naive();
    let target = dt.date_naive();
    let day_diff = (target - today).num_days();
    let hhmm = dt.format("%-I:%M %p").to_string();
    match day_diff {
        0 => format!("TODAY · {hhmm}"),
        1 => format!("TOMORROW · {hhmm}"),
        n if n < 7 => dt.format("%a · %-I:%M %p").to_string().to_uppercase(),
        _ => dt.format("%b %-d · %-I:%M %p").to_string().to_uppercase(),
    }
}
