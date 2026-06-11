// 48-hour rolling forecast as an SVG chart: temperature line on top,
// rain probability bars below, and a glyph-strip header so the eye
// can scan the next two days at a glance. Horizontal scroll on
// small screens so phone users get the whole 48h without zooming.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::components::units_fmt::{fmt_temp_short, use_unit_prefs, UnitPrefs};
use crate::forecast::snapshot::{ForecastSnapshot, HourlyEntry};
use chrono::{DateTime, Local, TimeZone, Timelike};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn HourlyForecast(snap: ReadSignal<ForecastSnapshot>) -> impl IntoView {
    let unit_prefs = use_unit_prefs();
    view! {
        <section class="forecast-hourly">
            <header class="forecast-section-head">
                <h2 class="forecast-section-title">"Next 48 hours"</h2>
                <span class="forecast-section-meta">
                    {move || {
                        let s = snap.get();
                        if s.hourly.is_empty() { "Loading…".to_string() }
                        else {
                            let last_idx = (s.hourly.len() - 1).min(47);
                            let end = Local.timestamp_opt(s.hourly[last_idx].time_epoch, 0)
                                .single()
                                .map(|d: DateTime<Local>| d.format("%a %-I%p").to_string())
                                .unwrap_or_default();
                            format!("through {end}")
                        }
                    }}
                </span>
            </header>
            <div class="hourly-scroll">
                {move || {
                    let entries: Vec<HourlyEntry> = snap.get().hourly.into_iter().take(48).collect();
                    let prefs = unit_prefs.get();
                    if entries.is_empty() {
                        view! { <crate::components::ui::Skeleton variant="chart"/> }.into_any()
                    } else {
                        view! { <HourlyChart entries prefs/> }.into_any()
                    }
                }}
            </div>
        </section>
    }
}

#[component]
fn HourlyChart(entries: Vec<HourlyEntry>, prefs: UnitPrefs) -> impl IntoView {
    let n = entries.len().max(1);
    let col_w: f64 = 56.0;
    let total_w = col_w * n as f64;
    let header_h: f64 = 70.0;
    let temp_h: f64 = 90.0;
    let rain_h: f64 = 50.0;
    let total_h = header_h + temp_h + rain_h + 10.0;

    let temps: Vec<f64> = entries.iter().map(|e| e.temp_f).collect();
    let temp_min = temps.iter().cloned().fold(f64::INFINITY, f64::min);
    let temp_max = temps.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let temp_span = (temp_max - temp_min).max(1.0);

    let temp_to_y = move |t: f64| {
        let pad = 12.0;
        let usable = temp_h - pad * 2.0;
        header_h + pad + usable * (1.0 - (t - temp_min) / temp_span)
    };

    // Header glyphs + temps per hour.
    let header_cells: Vec<_> = entries.iter().enumerate().map(|(i, e)| {
        let x = col_w * (i as f64) + col_w / 2.0;
        let label = Local.timestamp_opt(e.time_epoch, 0).single()
            .map(|d: DateTime<Local>| d.format("%-I%p").to_string())
            .unwrap_or_default();
        let local_hour = Local.timestamp_opt(e.time_epoch, 0).single()
            .map(|d: DateTime<Local>| d.hour())
            .unwrap_or(12);
        let is_day = (6..20).contains(&local_hour);
        let (g, _) = weather_code_glyph(e.weather_code, is_day);
        view! {
            <g>
                <text x={x.to_string()} y="14" text-anchor="middle" class="hourly-time">{label}</text>
                <svg
                    x={(x - 11.0).to_string()}
                    y="26"
                    width="22"
                    height="22"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.75"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    class="hourly-glyph"
                    inner_html=crate::components::ui::icon::paths_for(g)
                ></svg>
                <text x={x.to_string()} y="62" text-anchor="middle" class="hourly-temp">
                    {fmt_temp_short(e.temp_f, prefs)}
                </text>
            </g>
        }.into_any()
    }).collect();

    // Temperature line path.
    let temp_path = {
        let mut d = String::new();
        for (i, e) in entries.iter().enumerate() {
            let x = col_w * (i as f64) + col_w / 2.0;
            let y = temp_to_y(e.temp_f);
            if i == 0 {
                d.push_str(&format!("M {x:.2} {y:.2}"));
            } else {
                d.push_str(&format!(" L {x:.2} {y:.2}"));
            }
        }
        d
    };

    // Rain probability bars.
    let rain_baseline = header_h + temp_h;
    let rain_bars: Vec<_> = entries.iter().enumerate().map(|(i, e)| {
        let bar_w = col_w * 0.5;
        let x = col_w * (i as f64) + (col_w - bar_w) / 2.0;
        let h = rain_h * (e.precip_probability as f64 / 100.0);
        let y = rain_baseline + (rain_h - h);
        let opacity = 0.35 + 0.65 * (e.precip_probability as f64 / 100.0);
        view! {
            <rect
                x={x.to_string()}
                y={y.to_string()}
                width={bar_w.to_string()}
                height={h.to_string()}
                rx="2"
                class="hourly-rain-bar"
                opacity={opacity.to_string()}
            >
                <title>{format!("{}% rain at {}", e.precip_probability, format_local_hour(e.time_epoch))}</title>
            </rect>
        }.into_any()
    }).collect();

    // Now-line: vertical marker at the first hour (typically the
    // current hour since Open-Meteo aligns to top-of-hour).
    let now_line = view! {
        <line
            x1={(col_w / 2.0).to_string()} y1={header_h.to_string()}
            x2={(col_w / 2.0).to_string()} y2={(header_h + temp_h + rain_h).to_string()}
            class="hourly-now-line"
        />
    };

    view! {
        <svg
            class="hourly-svg"
            viewBox={format!("0 0 {total_w} {total_h}")}
            preserveAspectRatio="xMinYMid meet"
            style={format!("min-width: {}px", total_w)}
        >
            // Background bands so the eye can read at-a-glance which
            // band is temperature vs rain.
            <rect x="0" y={header_h.to_string()} width={total_w.to_string()} height={temp_h.to_string()} class="hourly-band-temp"/>
            <rect x="0" y={(header_h + temp_h).to_string()} width={total_w.to_string()} height={rain_h.to_string()} class="hourly-band-rain"/>

            {now_line}
            {header_cells}
            <path d={temp_path} class="hourly-temp-line"/>
            {rain_bars}

            <text x="6" y={(header_h + 14.0).to_string()} class="hourly-band-label">"TEMP"</text>
            <text x="6" y={(header_h + temp_h + 14.0).to_string()} class="hourly-band-label">"RAIN %"</text>
        </svg>
    }
}

fn format_local_hour(epoch: i64) -> String {
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|d: DateTime<Local>| d.format("%a %-I%p").to_string())
        .unwrap_or_default()
}
