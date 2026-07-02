// P3-8: Watering Week. A read-only 7-day plan: for today + 6 days, what LocalSky
// will do (water / skip / blocked) and why, color-coded by category, from the
// same engine that runs the morning check (the server-precomputed
// `seven_day_verdicts`). Richer than the compact verdict strip -- full reasons +
// weather context per day -- so a beginner can read their watering week at a
// glance. Read-only: a preview, not a commitment; the live call is made each
// morning against that day's actual conditions.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::components::units_fmt::{
    depth_value_in, fmt_rain_amount, fmt_temp_short, temp_unit, temp_value, use_unit_prefs,
    UnitPrefs,
};
use crate::ha::snapshot::{DayVerdict, IrrigationSnapshot};
use crate::timefmt::{format_md, format_wday_short};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

/// (human label, css-accent modifier) for a day's plan. The modifier keys the
/// row's accent color; the palette mirrors the verdict strip for consistency.
/// Categories follow the P3-8 brief: watering (scheduled/smart), rain skip,
/// blocked-by-law (jurisdictional restriction), freeze, plus wind/pause/other.
fn category(v: &DayVerdict) -> (&'static str, &'static str) {
    match v.verdict.as_str() {
        "run_extended" => ("Watering, extended for heat", "extended"),
        // Key on the structured reason_code (P2 units architecture) so the
        // category is unit-independent; legacy cells with an empty code fall back
        // to the baked-reason substring match (the original behavior).
        "skip" => match v.reason_code.as_str() {
            "restrictions" => ("Blocked by watering rules", "law"),
            "freeze_now" | "overnight_freeze" | "soil_frost" => ("Skipped, freeze risk", "freeze"),
            "wind_now" | "wind_forecast" => ("Skipped, too windy", "wind"),
            "rain_now" | "already_wet" | "observed_rain" | "rain_next_4h" | "tomorrow_rain"
            | "rain_3day" => ("Skipped, rain expected", "rain"),
            "paused" | "pause_until" => ("Paused", "pause"),
            "" => {
                let r = v.reason.to_lowercase();
                if r.contains("restrict")
                    || r.contains("allowed")
                    || r.contains("watering day")
                    || r.contains("forbidden")
                {
                    ("Blocked by watering rules", "law")
                } else if r.contains("freeze") || r.contains("frost") || r.contains("cold") {
                    ("Skipped, freeze risk", "freeze")
                } else if r.contains("wind") {
                    ("Skipped, too windy", "wind")
                } else if r.contains("rain") || r.contains("wet") || r.contains("saturat") {
                    // Wet / saturated soil is the water family (blue), not a
                    // generic gray skip: the lawn is skipping BECAUSE it has water.
                    ("Skipped, soil already wet", "rain")
                } else if r.contains("pause") {
                    ("Paused", "pause")
                } else {
                    ("Skipped", "skip")
                }
            }
            _ => ("Skipped", "skip"),
        },
        // "run" and any unknown verdict read as watering.
        _ => ("Watering", "run"),
    }
}

/// (primary, secondary) day label, e.g. ("Today", "Jun 27") / ("Wed",
/// "Jul 2"). Rendered in the deployment's IANA `tz` (not the viewer's browser
/// zone) via `crate::timefmt`, so a traveling viewer sees the deployment's
/// calendar week. Empty `tz` falls back to browser-local (hydrate) / UTC (ssr).
/// Falls back gracefully when the epoch hasn't loaded.
fn day_label(epoch: i64, offset: u32, tz: &str) -> (String, String) {
    let date = if epoch != 0 {
        format_md(epoch, tz)
    } else {
        String::new()
    };
    let primary = match offset {
        0 => "Today".to_string(),
        1 => "Tomorrow".to_string(),
        // timefmt exposes the short weekday ("Wed"); the full weekday isn't
        // available in the WASM (Intl) path, and the short form matches the
        // daily-forecast cards. Empty (epoch not loaded / bad tz) -> Day +N.
        _ => {
            let wd = if epoch != 0 {
                format_wday_short(epoch, tz)
            } else {
                String::new()
            };
            if wd.is_empty() {
                format!("Day +{offset}")
            } else {
                wd
            }
        }
    };
    (primary, date)
}

#[component]
pub fn WateringWeekPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    view! {
        <div class="wk-page">
            <header class="wk-page__header">
                <p class="wk-page__eyebrow">"Plan"</p>
                <h1 class="wk-page__title">"Watering Week"</h1>
                <p class="wk-page__sub">
                    "Your next seven days at a glance: what LocalSky plans for each day, and why. "
                    "The same engine as the morning check, applied to each day's forecast. "
                    "Read-only -- a preview, not a commitment; the live call is made each morning "
                    "against that day's actual conditions."
                </p>
            </header>
            <WeekLegend/>
            <div class="wk-list" role="list" aria-label="7-day watering plan">
                {move || {
                    let days = snap.get().seven_day_verdicts;
                    if days.is_empty() {
                        return view! {
                            <p class="wk-empty">
                                "The 7-day plan fills in once the forecast loads."
                            </p>
                        }
                        .into_any();
                    }
                    let p = prefs.get();
                    // Deployment IANA tz for the day labels (24h local, not the
                    // viewer's browser zone). Empty -> browser-local / UTC.
                    let tz = snap.get().timezone;
                    days.into_iter()
                        .map(|v| view! { <WeekRow v=v prefs=p tz=tz.clone()/> }.into_any())
                        .collect::<Vec<_>>()
                        .into_any()
                }}
            </div>
        </div>
    }
}

#[component]
fn WeekRow(v: DayVerdict, prefs: UnitPrefs, tz: String) -> impl IntoView {
    let (primary, date) = day_label(v.time_epoch, v.day_offset, &tz);
    let (label, modifier) = category(&v);
    let glyph = weather_code_glyph(v.weather_code, true).0;
    let is_today = v.day_offset == 0;
    let row_cls = format!("wk-row wk-row--{modifier}");
    let temp = format!(
        "{} / {}",
        fmt_temp_short(v.temp_max_f, prefs),
        fmt_temp_short(v.temp_min_f, prefs)
    );
    let rain = format!(
        "{} \u{b7} {}%",
        fmt_rain_amount(v.precip_in, prefs),
        v.precip_probability_max
    );
    let reason = if v.reason.is_empty() {
        "No skip conditions in the forecast.".to_string()
    } else {
        v.reason.clone()
    };
    // Single clean narration on the row; the visual children are aria-hidden so a
    // screen reader hears the full day once (incl. temp + rain), not the labels
    // twice. Mirrors verdict_strip's aria approach. Spoken units follow the
    // display preference so the narration matches the visible row.
    let rain_word = if prefs.rain_mm { "millimeter" } else { "inch" };
    let aria = format!(
        "{primary} {date}: {label}. {reason} High {} {unit}, low {} {unit}, {} {rain_word} rain at {} percent.",
        temp_value(v.temp_max_f, prefs),
        temp_value(v.temp_min_f, prefs),
        depth_value_in(v.precip_in, prefs),
        v.precip_probability_max,
        unit = temp_unit(prefs),
    );
    view! {
        <div class=row_cls class:wk-row--today=is_today role="listitem" aria-label=aria>
            <div class="wk-row__day" aria-hidden="true">
                <span class="wk-row__day-primary">{primary}</span>
                <span class="wk-row__day-date">{date}</span>
            </div>
            <div class="wk-row__weather" aria-hidden="true">
                <crate::components::ui::Icon name=glyph size=24/>
                <span class="wk-row__temp">{temp}</span>
                <span class="wk-row__rain">{rain}</span>
            </div>
            <div class="wk-row__plan" aria-hidden="true">
                <span class="wk-row__badge">{label}</span>
                <span class="wk-row__reason">{reason}</span>
            </div>
        </div>
    }
}

#[component]
fn WeekLegend() -> impl IntoView {
    let items = [
        ("run", "Watering"),
        ("rain", "Rain skip"),
        ("law", "Watering rules"),
        ("freeze", "Freeze"),
        ("extended", "Heat extend"),
        ("skip", "Other skip"),
    ];
    view! {
        <div class="wk-legend" aria-hidden="true">
            {items
                .into_iter()
                .map(|(m, l)| {
                    view! {
                        <span class=format!("wk-legend__item wk-legend__item--{m}")>
                            <span class="wk-legend__dot"></span>
                            {l}
                        </span>
                    }
                })
                .collect::<Vec<_>>()}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dv(verdict: &str, reason: &str) -> DayVerdict {
        DayVerdict {
            day_offset: 1,
            time_epoch: 0,
            weather_code: 0,
            temp_max_f: 80.0,
            temp_min_f: 60.0,
            precip_in: 0.0,
            precip_probability_max: 0,
            verdict: verdict.to_string(),
            reason: reason.to_string(),
            // P1 additive reason_code defaults to "" for this UI test fixture.
            ..Default::default()
        }
    }

    #[test]
    fn category_maps_verdict_and_reason() {
        assert_eq!(category(&dv("run", "")).1, "run");
        assert_eq!(category(&dv("run_extended", "heat")).1, "extended");
        assert_eq!(category(&dv("skip", "Rain expected within 4h")).1, "rain");
        // Jurisdictional restriction => blocked-by-law, not a generic skip.
        assert_eq!(
            category(&dv(
                "skip",
                "Watering restriction (St. Johns RWMD): today is not an allowed watering day"
            ))
            .1,
            "law"
        );
        assert_eq!(category(&dv("skip", "Freeze risk overnight")).1, "freeze");
        // Saturated/wet soil is the water family (blue rain accent), not a
        // generic gray skip -- the lawn is skipping because it already has water.
        assert_eq!(category(&dv("skip", "Soil already saturated")).1, "rain");
    }

    #[test]
    fn day_label_handles_today_tomorrow_and_unloaded() {
        assert_eq!(day_label(0, 0, "America/New_York").0, "Today");
        assert_eq!(day_label(0, 1, "America/New_York").0, "Tomorrow");
        // Unloaded epoch on a far day still labels something.
        assert_eq!(day_label(0, 4, "America/New_York").0, "Day +4");
    }
}
