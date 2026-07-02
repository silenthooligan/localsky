// 7-day daily forecast strip. One card per day with weather glyph,
// high/low, rain mm + probability, peak wind, UV index. Today's card
// gets a highlight ring so it's the visual anchor.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::components::units_fmt::{
    fmt_rain_amount, fmt_temp_short, fmt_wind, use_unit_prefs, UnitPrefs,
};
use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot};
use crate::timefmt::{format_md, format_wday_short};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn DailyForecast(snap: ReadSignal<ForecastSnapshot>) -> impl IntoView {
    let unit_prefs = use_unit_prefs();
    view! {
        <section class="forecast-daily">
            <header class="forecast-section-head">
                <h2 class="forecast-section-title">"7-day forecast"</h2>
                <span class="forecast-section-meta">
                    {move || {
                        let s = snap.get();
                        let label = if s.source_label.is_empty() { "Forecast" } else { &s.source_label };
                        if !s.source_reachable { format!("{label} unreachable") }
                        else if s.daily.is_empty() { "Loading…".to_string() }
                        else { format!("{label} · {}", s.timezone) }
                    }}
                </span>
            </header>
            <div class="daily-row">
                {move || {
                    let s = snap.get();
                    let prefs = unit_prefs.get();
                    if s.daily.is_empty() {
                        (0..7).map(|_| {
                            view! { <crate::components::ui::Skeleton variant="block"/> }.into_any()
                        }).collect::<Vec<_>>().into_any()
                    } else {
                        let tz = s.timezone.clone();
                        s.daily.iter().enumerate().take(7).map(|(idx, d)| {
                            view! { <DailyCard entry=d.clone() is_today={idx == 0} prefs tz=tz.clone()/> }.into_any()
                        }).collect::<Vec<_>>().into_any()
                    }
                }}
            </div>
        </section>
    }
}

#[component]
fn DailyCard(entry: DailyEntry, is_today: bool, prefs: UnitPrefs, tz: String) -> impl IntoView {
    let (g, label) = weather_code_glyph(entry.weather_code, true);
    // Weekday + month/day in the DEPLOYMENT timezone, not the viewer's browser
    // TZ, so a traveling viewer sees the deployment's calendar day, not theirs.
    let day_label = if is_today {
        "Today".to_string()
    } else {
        let d = format_wday_short(entry.time_epoch, &tz);
        if d.is_empty() {
            "-".to_string()
        } else {
            d
        }
    };
    let date_label = format_md(entry.time_epoch, &tz);
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
            <div class="daily-card-glyph" title=label aria-label=label>
                <crate::components::ui::Icon name=g size=30/>
            </div>
            <div class="daily-card-temps">
                <span class="daily-card-temp-hi">{fmt_temp_short(entry.temp_max_f, prefs)}</span>
                <span class="daily-card-temp-sep">"/"</span>
                <span class="daily-card-temp-lo">{fmt_temp_short(entry.temp_min_f, prefs)}</span>
            </div>
            <div class="daily-card-rain">
                <span class="daily-card-rain-amt">{fmt_rain_amount(entry.precip_sum_in, prefs)}</span>
                <span class="daily-card-rain-pct">{format!("{}%", entry.precip_probability_max)}</span>
            </div>
            <dl class="daily-card-meta">
                <div class="kv">
                    <dt class="k">"wind"</dt>
                    <dd class="v">{fmt_wind(entry.wind_max_mph, prefs)}</dd>
                </div>
                <div class="kv">
                    <dt class="k">"uv"</dt>
                    <dd class="v">{format!("{:.0}", entry.uv_index_max)}</dd>
                </div>
            </dl>
        </article>
    }
}
