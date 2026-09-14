// 48-hour rolling forecast as an SVG chart: temperature line on top,
// rain probability bars below, and a glyph-strip header so the eye
// can scan the next two days at a glance. Horizontal scroll on
// small screens so phone users get the whole 48h without zooming.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::components::units_fmt::{fmt_optional_temp_short, use_unit_prefs, UnitPrefs};
use crate::forecast::snapshot::{ForecastSnapshot, HourlyEntry};
use crate::timefmt::{format_hm, format_wday_short};
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
                        // Empty: say nothing. The body panel explains the wait
                        // honestly; a perpetual "Loading…" here contradicted it.
                        if s.hourly.is_empty() { String::new() }
                        else {
                            let last_idx = (s.hourly.len() - 1).min(47);
                            let epoch = s.hourly[last_idx].time_epoch;
                            // 24-hour, deployment-local (e.g. "Sun 14:00"), not browser-TZ.
                            let end = format!(
                                "{} {}",
                                format_wday_short(epoch, &s.timezone),
                                format_hm(epoch, &s.timezone),
                            );
                            format!("through {end}")
                        }
                    }}
                </span>
            </header>
            // Horizontally scrollable, so it is focusable for a keyboard.
            <div class="hourly-scroll" tabindex="0" role="region" aria-label="Hourly forecast">
                {move || {
                    let s = snap.get();
                    let tz = s.timezone.clone();
                    let entries: Vec<HourlyEntry> = s.hourly.into_iter().take(48).collect();
                    let prefs = unit_prefs.get();
                    if entries.is_empty() {
                        view! { <super::ForecastPending variant="chart" what="hourly forecast"/> }
                            .into_any()
                    } else {
                        view! { <HourlyChart entries prefs tz/> }.into_any()
                    }
                }}
            </div>
        </section>
    }
}

#[component]
fn HourlyChart(entries: Vec<HourlyEntry>, prefs: UnitPrefs, tz: String) -> impl IntoView {
    let n = entries.len().max(1);
    let col_w: f64 = 56.0;
    let total_w = col_w * n as f64;
    let header_h: f64 = 70.0;
    let temp_h: f64 = 90.0;
    let rain_h: f64 = 50.0;
    let total_h = header_h + temp_h + rain_h + 10.0;

    let temps: Vec<Option<f64>> = entries.iter().map(|e| e.temp_f).collect();
    let (temp_path, temp_area) = temperature_paths(&temps, col_w, header_h, temp_h);

    // Header glyphs + temps per hour. 24-hour, deployment-local (e.g. "14:00").
    let header_cells: Vec<_> = entries.iter().enumerate().map(|(i, e)| {
        let x = col_w * (i as f64) + col_w / 2.0;
        let label = format_hm(e.time_epoch, &tz);
        // Day/night for the glyph: the deployment-local hour, read off the
        // 24-hour "HH:MM" string (always 2-digit hour); fall back to noon.
        let local_hour = label
            .get(0..2)
            .and_then(|h| h.parse::<u32>().ok())
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
                    {fmt_optional_temp_short(e.temp_f, prefs)}
                </text>
            </g>
        }.into_any()
    }).collect();

    // Rain probability bars.
    let rain_baseline = header_h + temp_h;
    let rain_bars: Vec<_> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let bar_w = col_w * 0.5;
            let x = col_w * (i as f64) + (col_w - bar_w) / 2.0;
            // Missing probability is a visible gap, distinct from reported 0%.
            // A known probability can still be shown when its separate QPF
            // amount is unknown; the chart measures chance, not accumulation.
            let Some(prob) = e.precip_probability else {
                return view! {
                    <text x={(x + bar_w / 2.0).to_string()}
                        y={(rain_baseline + rain_h / 2.0).to_string()}
                        text-anchor="middle" class="hourly-time">
                        <title>{format!("Rain chance unknown at {}", format_local_hour(e.time_epoch, &tz))}</title>
                        "—"
                    </text>
                }.into_any();
            };
            let frac = prob as f64 / 100.0;
            let h = rain_h * frac;
            let y = rain_baseline + (rain_h - h);
            let opacity = 0.35 + 0.65 * frac;
            let title = format!("{prob}% rain at {}", format_local_hour(e.time_epoch, &tz));
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
                    <title>{title}</title>
                </rect>
            }
            .into_any()
        })
        .collect();

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

            <defs>
                <linearGradient id="hourly-temp-grad" x1="0" y1="0" x2="0" y2="1">
                    <stop offset="0%" stop-color="var(--accent-warm)" stop-opacity="0.20"/>
                    <stop offset="100%" stop-color="var(--accent-warm)" stop-opacity="0"/>
                </linearGradient>
            </defs>
            {now_line}
            {header_cells}
            <path d={temp_area} fill="url(#hourly-temp-grad)" class="hourly-temp-area"/>
            <path d={temp_path} class="hourly-temp-line"/>
            {rain_bars}

            <text x="6" y={(header_h + 14.0).to_string()} class="hourly-band-label">"TEMP"</text>
            <text x="6" y={(header_h + temp_h + 14.0).to_string()} class="hourly-band-label">"RAIN %"</text>
        </svg>
    }
}

/// Each contiguous set of known temperatures gets its own line and closed
/// area. Connecting across a missing hour would invent a temperature trend;
/// an entirely unknown series has neither a range nor any path to draw.
fn temperature_paths(
    temps: &[Option<f64>],
    col_w: f64,
    header_h: f64,
    temp_h: f64,
) -> (String, String) {
    let known: Vec<f64> = temps
        .iter()
        .flatten()
        .copied()
        .filter(|t| t.is_finite())
        .collect();
    let Some(first) = known.first().copied() else {
        return (String::new(), String::new());
    };
    let temp_min = known.iter().copied().fold(first, f64::min);
    let temp_max = known.iter().copied().fold(first, f64::max);
    // Halve before subtraction so even finite extreme inputs cannot overflow
    // the range into infinity and then generate NaN SVG coordinates.
    let half_span = (temp_max / 2.0 - temp_min / 2.0).max(0.5);
    let baseline = header_h + temp_h;
    let mut lines = Vec::new();
    let mut areas = Vec::new();
    let mut segment: Vec<(f64, f64)> = Vec::new();
    let finish =
        |points: &mut Vec<(f64, f64)>, lines: &mut Vec<String>, areas: &mut Vec<String>| {
            let (Some((first_x, _)), Some((last_x, _))) = (points.first(), points.last()) else {
                return;
            };
            let line = points
                .iter()
                .enumerate()
                .map(|(i, (x, y))| {
                    let verb = if i == 0 { "M" } else { "L" };
                    format!("{verb} {x:.2} {y:.2}")
                })
                .collect::<Vec<_>>()
                .join(" ");
            areas.push(format!(
                "{line} L {last_x:.2} {baseline:.2} L {first_x:.2} {baseline:.2} Z"
            ));
            lines.push(line);
            points.clear();
        };
    for (i, temp) in temps.iter().enumerate() {
        if let Some(t) = temp.filter(|t| t.is_finite()) {
            let x = col_w * i as f64 + col_w / 2.0;
            let frac = (t / 2.0 - temp_min / 2.0) / half_span;
            let y = header_h + 12.0 + (temp_h - 24.0) * (1.0 - frac);
            segment.push((x, y));
        } else {
            finish(&mut segment, &mut lines, &mut areas);
        }
    }
    finish(&mut segment, &mut lines, &mut areas);
    (lines.join(" "), areas.join(" "))
}

/// Weekday + 24-hour clock in the deployment timezone (e.g. "Sun 14:00"),
/// for the rain-bar tooltip. Empty / invalid tz -> browser-local (hydrate).
fn format_local_hour(epoch: i64, tz: &str) -> String {
    format!("{} {}", format_wday_short(epoch, tz), format_hm(epoch, tz))
}

#[cfg(test)]
mod tests {
    use super::temperature_paths;

    #[test]
    fn missing_hour_breaks_both_temperature_line_and_area_without_erasing_zero() {
        let (line, area) = temperature_paths(
            &[Some(0.0), Some(10.0), None, Some(20.0), Some(30.0)],
            56.0,
            70.0,
            90.0,
        );
        assert_eq!(
            line,
            "M 28.00 148.00 L 84.00 126.00 M 196.00 104.00 L 252.00 82.00"
        );
        assert_eq!(area.matches('M').count(), 2);
        assert_eq!(area.matches('Z').count(), 2);
        assert!(area.contains("L 84.00 160.00 L 28.00 160.00 Z M 196.00"));
        assert!(
            !area.contains("140.00"),
            "the missing hour has no invented point"
        );
    }

    #[test]
    fn all_missing_or_nonfinite_temperatures_draw_no_paths() {
        for values in [
            vec![],
            vec![
                None,
                Some(f64::NAN),
                Some(f64::INFINITY),
                Some(f64::NEG_INFINITY),
            ],
        ] {
            assert_eq!(
                temperature_paths(&values, 56.0, 70.0, 90.0),
                (String::new(), String::new())
            );
        }
    }

    #[test]
    fn flat_and_extreme_finite_temperatures_produce_finite_coordinates() {
        for values in [
            vec![Some(0.0), Some(0.0)],
            vec![Some(-f64::MAX), Some(f64::MAX)],
        ] {
            let (line, area) = temperature_paths(&values, 56.0, 70.0, 90.0);
            assert_eq!(line.matches('M').count(), 1);
            assert_eq!(area.matches('Z').count(), 1);
            for path in [line, area] {
                for coordinate in path
                    .split_whitespace()
                    .filter_map(|word| word.parse::<f64>().ok())
                {
                    assert!(coordinate.is_finite());
                }
            }
        }
    }
}
