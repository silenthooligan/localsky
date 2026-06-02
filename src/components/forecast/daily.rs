// 7-day daily forecast strip. One card per day with weather glyph,
// high/low, rain mm + probability, peak wind, UV index. Today's card
// gets a highlight ring so it's the visual anchor.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot};
use chrono::{DateTime, Local, TimeZone};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn DailyForecast(snap: ReadSignal<ForecastSnapshot>) -> impl IntoView {
    view! {
        <section class="forecast-daily">
            <header class="forecast-section-head">
                <h2 class="forecast-section-title">"7-day forecast"</h2>
                <span class="forecast-section-meta">
                    {move || {
                        let s = snap.get();
                        if !s.source_reachable { "Open-Meteo unreachable".to_string() }
                        else if s.daily.is_empty() { "Loading…".to_string() }
                        else { format!("Open-Meteo · {}", s.timezone) }
                    }}
                </span>
            </header>
            <div class="daily-row">
                {move || {
                    let s = snap.get();
                    if s.daily.is_empty() {
                        view! { <div class="daily-loading">"Loading forecast…"</div> }.into_any()
                    } else {
                        s.daily.iter().enumerate().take(7).map(|(idx, d)| {
                            view! { <DailyCard entry=d.clone() is_today={idx == 0}/> }.into_any()
                        }).collect::<Vec<_>>().into_any()
                    }
                }}
            </div>
        </section>
    }
}

#[component]
fn DailyCard(entry: DailyEntry, is_today: bool) -> impl IntoView {
    let (g, label) = weather_code_glyph(entry.weather_code, true);
    let day_label = if is_today {
        "Today".to_string()
    } else {
        Local
            .timestamp_opt(entry.time_epoch, 0)
            .single()
            .map(|d: DateTime<Local>| d.format("%a").to_string())
            .unwrap_or_else(|| "—".to_string())
    };
    let date_label = Local
        .timestamp_opt(entry.time_epoch, 0)
        .single()
        .map(|d: DateTime<Local>| d.format("%-m/%-d").to_string())
        .unwrap_or_default();
    let class = if is_today {
        "daily-card daily-card-today"
    } else {
        "daily-card"
    };

    view! {
        <article class=class>
            <header class="daily-card-head">
                <span class="daily-card-day">{day_label}</span>
                <span class="daily-card-date">{date_label}</span>
            </header>
            <div class="daily-card-glyph" title=label aria-label=label>{g}</div>
            <div class="daily-card-temps">
                <span class="daily-card-temp-hi">{format!("{:.0}°", entry.temp_max_f)}</span>
                <span class="daily-card-temp-sep">"/"</span>
                <span class="daily-card-temp-lo">{format!("{:.0}°", entry.temp_min_f)}</span>
            </div>
            <div class="daily-card-rain">
                <span class="daily-card-rain-amt">{format!("{:.2}\"", entry.precip_sum_in)}</span>
                <span class="daily-card-rain-pct">{format!("{}%", entry.precip_probability_max)}</span>
            </div>
            <dl class="daily-card-meta">
                <div class="kv">
                    <dt class="k">"wind"</dt>
                    <dd class="v">{format!("{:.0} mph", entry.wind_max_mph)}</dd>
                </div>
                <div class="kv">
                    <dt class="k">"uv"</dt>
                    <dd class="v">{format!("{:.0}", entry.uv_index_max)}</dd>
                </div>
            </dl>
        </article>
    }
}
