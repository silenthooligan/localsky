// Watering Week. A read-only 7-day plan: for today + 6 days, what LocalSky
// will do (water / skip / blocked) and why, color-coded by category, from the
// same engine that runs the morning check (the server-precomputed
// `seven_day_verdicts`). Richer than the compact verdict strip -- full reasons +
// weather context per day -- so a beginner can read their watering week at a
// glance. Read-only: a preview, not a commitment; the live call is made each
// morning against that day's actual conditions.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::components::units_fmt::{
    fmt_optional_rain_amount, fmt_optional_temp_short, optional_depth_value_in,
    optional_temp_value, temp_unit, use_unit_prefs, UnitPrefs,
};
use crate::model::{DayVerdict, IrrigationSnapshot};
use crate::timefmt::{format_md, format_wday_short};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

/// (human label, css-accent modifier) for a day's plan. The modifier keys the
/// row's accent color; the palette mirrors the verdict strip for consistency.
/// Categories: watering (scheduled/smart), rain skip,
/// blocked-by-law (jurisdictional restriction), freeze, plus wind/pause/other.
fn category(v: &DayVerdict) -> (&'static str, &'static str) {
    match v.verdict.as_str() {
        "run_extended" => ("Watering, extended for heat", "extended"),
        // Key on the structured reason_code (P2 units architecture) so the
        // category is unit-independent; legacy cells with an empty code fall back
        // to the baked-reason substring match (the original behavior).
        "skip" => match crate::gates_catalog::GateFamily::of(&v.reason_code, &v.reason) {
            crate::gates_catalog::GateFamily::Restriction => ("Blocked by watering rules", "law"),
            crate::gates_catalog::GateFamily::Freeze => ("Skipped, freeze risk", "freeze"),
            crate::gates_catalog::GateFamily::Wind => ("Skipped, too windy", "wind"),
            // Saturated soil lands here too: the lawn is skipping BECAUSE
            // it has water, so it reads in the water family rather than as
            // a generic grey skip.
            crate::gates_catalog::GateFamily::Water => {
                match crate::gates_catalog::water_kind(&v.reason_code, &v.reason) {
                    crate::gates_catalog::WaterKind::Soil => ("Skipped, soil already wet", "rain"),
                    crate::gates_catalog::WaterKind::Recent => ("Skipped, recent rain", "rain"),
                    crate::gates_catalog::WaterKind::Forecast => ("Skipped, rain expected", "rain"),
                }
            }
            crate::gates_catalog::GateFamily::Pause => ("Paused", "pause"),
            crate::gates_catalog::GateFamily::SoilModel => ("Watering", "run"),
            crate::gates_catalog::GateFamily::NoData => {
                ("Skipped, required data unavailable", "skip")
            }
            crate::gates_catalog::GateFamily::Other if v.reason_code.is_empty() => {
                // A legacy row with no code: the shared prose ladder.
                match crate::gates_catalog::GateFamily::from_prose(&v.reason) {
                    crate::gates_catalog::GateFamily::Restriction => {
                        ("Blocked by watering rules", "law")
                    }
                    crate::gates_catalog::GateFamily::Freeze => ("Skipped, freeze risk", "freeze"),
                    crate::gates_catalog::GateFamily::Wind => ("Skipped, too windy", "wind"),
                    crate::gates_catalog::GateFamily::Water => {
                        match crate::gates_catalog::water_kind("", &v.reason) {
                            crate::gates_catalog::WaterKind::Forecast => {
                                ("Skipped, rain expected", "rain")
                            }
                            _ => ("Skipped, soil already wet", "rain"),
                        }
                    }
                    crate::gates_catalog::GateFamily::Pause => ("Paused", "pause"),
                    crate::gates_catalog::GateFamily::NoData => {
                        ("Skipped, required data unavailable", "skip")
                    }
                    _ => ("Skipped", "skip"),
                }
            }
            crate::gates_catalog::GateFamily::Other => ("Skipped", "skip"),
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
            <header class="page-head">
                <p class="page-eyebrow">"Plan"</p>
                <h1 class="page-title">"Watering Week"</h1>
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
                    let allowed = snap.get().allowed_days_phrase();
                    days.into_iter()
                        .map(|v| view! { <WeekRow v=v prefs=p tz=tz.clone() allowed_days=allowed.clone().unwrap_or_default()/> }.into_any())
                        .collect::<Vec<_>>()
                        .into_any()
                }}
            </div>
        </div>
    }
}

#[component]
fn WeekRow(
    v: DayVerdict,
    prefs: UnitPrefs,
    tz: String,
    /// "Thu and Sun": the days the rules allow, when a rule is configured.
    #[prop(optional, into)]
    allowed_days: Option<String>,
) -> impl IntoView {
    let (primary, date) = day_label(v.time_epoch, v.day_offset, &tz);
    let (label, modifier) = category(&v);
    let glyph = weather_code_glyph(v.weather_code, true).0;
    let is_today = v.day_offset == 0;
    let row_cls = format!("wk-row wk-row--{modifier}");
    let temp = format!(
        "{} / {}",
        fmt_optional_temp_short(v.temp_max_f, prefs),
        fmt_optional_temp_short(v.temp_min_f, prefs)
    );
    // Omit the percent when the provider reported no probability; the old
    // bare 0 read as a confident "0% chance".
    let rain = match v.precip_probability_max {
        Some(prob) => format!(
            "{} \u{b7} {prob}%",
            fmt_optional_rain_amount(v.precip_in, prefs)
        ),
        None => fmt_optional_rain_amount(v.precip_in, prefs),
    };
    let reason = if v.reason.is_empty() {
        "No skip conditions in the forecast.".to_string()
    } else if v.reason_code == "restrictions" {
        // The engine's sentence says "today"; on a Thursday row that reads
        // as a lie. The rule is composed from the allowed days instead.
        restriction_row_reason(
            v.day_offset,
            allowed_days.as_deref().filter(|d| !d.is_empty()),
        )
    } else {
        v.reason.clone()
    };
    // Single clean narration on the row; the visual children are aria-hidden so a
    // screen reader hears the full day once (incl. temp + rain), not the labels
    // twice. Mirrors verdict_strip's aria approach. Spoken units follow the
    // display preference so the narration matches the visible row.
    let rain_word = if prefs.rain_mm { "millimeter" } else { "inch" };
    let prob_phrase = match v.precip_probability_max {
        Some(prob) => format!(" at {prob} percent"),
        None => String::new(),
    };
    let aria = format!(
        "{primary} {date}: {label}. {reason} High {} {unit}, low {} {unit}, {} {rain_word} rain{prob_phrase}.",
        optional_temp_value(v.temp_max_f, prefs),
        optional_temp_value(v.temp_min_f, prefs),
        optional_depth_value_in(v.precip_in, prefs),
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

/// The sentence a restricted day gets on the Week page: names the day's
/// standing and the allowed days, never "today" for a day that is not.
pub fn restriction_row_reason(day_offset: u32, allowed_days: Option<&str>) -> String {
    let standing = match day_offset {
        0 => "Not a watering day under your rules.".to_string(),
        1 => "Tomorrow is not a watering day under your rules.".to_string(),
        _ => "Not a watering day under your rules.".to_string(),
    };
    match allowed_days {
        Some(days) => format!("{standing} They allow {days}."),
        None => standing,
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
            temp_max_f: Some(80.0),
            temp_min_f: Some(60.0),
            precip_in: Some(0.0),
            precip_probability_max: Some(0),
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
        assert_eq!(
            category(&dv("skip", "Soil already saturated")).0,
            "Skipped, soil already wet"
        );
        let dry_day = dv("run", "");
        assert_eq!(dry_day.precip_in, Some(0.0));
        assert_eq!(category(&dry_day), ("Watering", "run"));
        for code in [
            "rain_now",
            "rain_today_forecast",
            "rain_next_4h",
            "tomorrow_rain",
            "rain_3day",
            "planning_forecast",
        ] {
            let mut unknown = dv("skip", "Rain forecast unavailable; watering held");
            unknown.reason_code = code.into();
            unknown.precip_in = None;
            assert_eq!(
                category(&unknown),
                ("Skipped, required data unavailable", "skip"),
                "{code}"
            );
            // A known dry current day does not fill a missing future interval.
            unknown.precip_in = Some(0.0);
            assert_eq!(
                category(&unknown),
                ("Skipped, required data unavailable", "skip"),
                "{code}"
            );
        }
    }

    #[test]
    fn day_label_handles_today_tomorrow_and_unloaded() {
        assert_eq!(day_label(0, 0, "America/New_York").0, "Today");
        assert_eq!(day_label(0, 1, "America/New_York").0, "Tomorrow");
        // Unloaded epoch on a far day still labels something.
        assert_eq!(day_label(0, 4, "America/New_York").0, "Day +4");
    }
}

#[cfg(test)]
mod restriction_row_tests {
    use super::restriction_row_reason;

    /// A row four days out under a district rule names the rule and the
    /// allowed days; it contains no "today".
    #[test]
    fn a_thursday_row_never_says_today() {
        let r = restriction_row_reason(4, Some("Thu and Sun"));
        assert!(!r.contains("today"), "{r}");
        assert!(r.contains("Thu and Sun"), "{r}");
        assert_eq!(
            restriction_row_reason(1, None),
            "Tomorrow is not a watering day under your rules."
        );
    }
}
