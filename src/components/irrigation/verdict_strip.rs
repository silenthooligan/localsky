// 7-day forward verdict strip. Each cell shows: weekday, weather glyph,
// temp range, precip × probability, and a colored border keyed to the
// predicted verdict (green=run, blue=skip-rain, amber=skip-freeze,
// red=skip-wind, orange=run-extended). Clicking/hovering a cell
// surfaces the reason. The data comes from the server-precomputed
// `seven_day_verdicts` field on IrrigationSnapshot, so the strip
// reflects exactly what the morning skip-check would decide if today's
// conditions matched that day's forecast.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::ha::snapshot::{DayVerdict, IrrigationSnapshot};
use chrono::{DateTime, Local, TimeZone};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn VerdictStrip(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <section class="verdict-strip">
            <header class="verdict-strip-head">
                <h3 class="verdict-strip-title">"7-Day Verdict"</h3>
                <span class="verdict-strip-subtitle">
                    "Predicted skip / run for today + 6 days, same engine as the morning check"
                </span>
            </header>
            <div class="verdict-strip-cells" role="region" aria-live="polite" aria-label="7-day irrigation verdict">
                {move || {
                    // Use a plain iter+collect rather than <For>. The strip
                    // is a fixed-shape 7-cell layout; the SSR-rendered DOM
                    // and the hydrate-initial Vec wouldn't reconcile cleanly
                    // through <For>'s keyed reconciler (SSR has 7 cells,
                    // hydrate starts with an empty Vec until the SSE
                    // snapshot arrives — that's a structural mismatch the
                    // <For> reconciler can't bridge without nuking the
                    // root subtree's other event handlers, which was
                    // killing the top-nav click handlers).
                    snap.get()
                        .seven_day_verdicts
                        .into_iter()
                        .map(|v| view! { <VerdictCell v=v/> }.into_any())
                        .collect::<Vec<_>>()
                }}
            </div>
        </section>
    }
}

#[component]
fn VerdictCell(v: DayVerdict) -> impl IntoView {
    let weekday = format_weekday(v.time_epoch, v.day_offset);
    let glyph = weather_code_glyph(v.weather_code, true).0;
    let kind_class = match v.verdict.as_str() {
        "skip" => verdict_skip_class(&v.reason),
        "run_extended" => "verdict-cell-extended",
        _ => "verdict-cell-run",
    };
    let cls = format!("verdict-cell {}", kind_class);
    let tooltip = if v.reason.is_empty() {
        format!("{weekday}: run")
    } else {
        format!("{weekday}: {} — {}", v.verdict, v.reason)
    };
    let temp_str = format!("{:.0}°/{:.0}°", v.temp_max_f, v.temp_min_f);
    let rain_str = format!("{:.2}″ · {}%", v.precip_in, v.precip_probability_max);
    let tag = verdict_short_label(&v);
    view! {
        <div class=cls title=tooltip>
            <div class="verdict-cell-day">{weekday}</div>
            <div class="verdict-cell-glyph">{glyph}</div>
            <div class="verdict-cell-temp">{temp_str}</div>
            <div class="verdict-cell-rain">{rain_str}</div>
            <div class="verdict-cell-tag">{tag}</div>
        </div>
    }
}

/// Map the day-offset + epoch to "TODAY" / "TOM" / "Wed" / etc. Always
/// renders something even if the epoch is 0 (forecast not yet loaded).
fn format_weekday(epoch: i64, offset: u32) -> String {
    if offset == 0 {
        return "TODAY".to_string();
    }
    if offset == 1 {
        return "TOM".to_string();
    }
    if epoch == 0 {
        return format!("+{offset}");
    }
    let dt: DateTime<Local> = match Local.timestamp_opt(epoch, 0) {
        chrono::LocalResult::Single(d) => d,
        _ => return format!("+{offset}"),
    };
    dt.format("%a").to_string().to_uppercase()
}

/// Drill the skip reason into a more specific class so the cell border
/// communicates the rule that fired without needing a tooltip.
fn verdict_skip_class(reason: &str) -> &'static str {
    let r = reason.to_lowercase();
    if r.contains("freeze") {
        "verdict-cell-skip-freeze"
    } else if r.contains("wind") {
        "verdict-cell-skip-wind"
    } else if r.contains("rain") || r.contains("wet") {
        "verdict-cell-skip-rain"
    } else if r.contains("paused") {
        "verdict-cell-skip-pause"
    } else {
        "verdict-cell-skip"
    }
}

/// One-word tag for the cell footer. Echoes the verdict but condensed.
fn verdict_short_label(v: &DayVerdict) -> &'static str {
    match v.verdict.as_str() {
        "run_extended" => "EXTEND",
        "skip" => {
            let r = v.reason.to_lowercase();
            if r.contains("freeze") {
                "FREEZE"
            } else if r.contains("wind") {
                "WIND"
            } else if r.contains("rain") || r.contains("wet") {
                "RAIN"
            } else if r.contains("paused") {
                "PAUSE"
            } else {
                "SKIP"
            }
        }
        _ => "RUN",
    }
}
