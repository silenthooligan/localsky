// <Icon/> — the single app-wide inline-SVG registry. Every glyph is a
// Lucide-style stroke icon drawn with `currentColor`, so it inherits
// the surrounding text color and themes for free across dark / light /
// high-contrast (emoji could not). This supersedes the old
// sidebar-only `SidebarIcon` and the emoji weather/nav glyphs.
//
// Adding an icon = one match arm: paste the path data from
// https://lucide.dev (or any stroke set drawn on a 24×24 viewBox) and
// add the name to `paths_for`.

use leptos::prelude::*;

#[component]
pub fn Icon(
    /// Registry name. Unknown names render a debug box.
    name: &'static str,
    /// Pixel size (width == height). Default 18.
    #[prop(optional)]
    size: Option<u32>,
    /// Stroke width override. Default 1.75.
    #[prop(optional)]
    stroke: Option<f32>,
    /// Extra class on the <svg> (e.g. for spin / color overrides).
    #[prop(into, optional)]
    class: String,
) -> impl IntoView {
    let dim = size.unwrap_or(18);
    let sw = stroke.unwrap_or(1.75);
    let style = format!("width:{dim}px;height:{dim}px;");
    let body = paths_for(name);
    view! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width=sw.to_string()
            stroke-linecap="round"
            stroke-linejoin="round"
            class=class
            style=style
            aria-hidden="true"
            inner_html=body
        />
    }
}

/// Resolve a weather condition glyph name from a free-text condition
/// string. Replaces the emoji ladder in hero.rs. Falls back to "cloud".
pub fn weather_glyph(condition: &str) -> &'static str {
    let c = condition.to_ascii_lowercase();
    if c.contains("thunder") || c.contains("lightning") {
        "cloud-lightning"
    } else if c.contains("snow") || c.contains("sleet") || c.contains("flurr") {
        "cloud-snow"
    } else if c.contains("rain") || c.contains("shower") || c.contains("drizzle") {
        "cloud-rain"
    } else if c.contains("fog") || c.contains("mist") || c.contains("haze") {
        "cloud-fog"
    } else if c.contains("partly") || c.contains("mostly sunny") || c.contains("few cloud") {
        "cloud-sun"
    } else if c.contains("clear") || c.contains("sunny") || c.contains("fair") {
        "sun"
    } else if c.contains("wind") {
        "wind"
    } else {
        "cloud"
    }
}

fn paths_for(name: &str) -> &'static str {
    match name {
        // ── Brand ────────────────────────────────────────────────────
        "brand" => {
            r#"<path d="M5.5 11.5 C 7.2 7 11.4 5.6 13.5 8 C 14.6 4.5 19.4 4.5 20.5 8 C 22.6 5.6 26.8 7 28.5 11.5 L 28.5 14 L 17 23.5 L 5.5 14 Z" transform="translate(-5 -2) scale(0.9)"/><circle cx="9.5" cy="11" r="2"/><circle cx="14.5" cy="11" r="2"/><path d="M9 7.5 a 3 3 0 0 1 6 0"/><path d="M12 13 c 1.5 2 1.5 4 0 5 c -1.5 -1 -1.5 -3 0 -5 Z"/>"#
        }

        // ── Primary nav ──────────────────────────────────────────────
        "weather" => r#"<path d="M17.5 19a4.5 4.5 0 1 0-1.7-8.66 7 7 0 1 0-11.6 6.66"/>"#,
        "droplet" => r#"<path d="M12 2.69 5.64 9.05a9 9 0 1 0 12.72 0Z"/>"#,
        "zones" => {
            r#"<path d="M12 21V8"/><path d="M7 21V11"/><path d="M17 21V11"/><path d="M12 8a4 4 0 0 0-4-4 4 4 0 0 0-4 4c0 2 1 4 4 4"/><path d="M12 8a4 4 0 0 1 4-4 4 4 0 0 1 4 4c0 2-1 4-4 4"/>"#
        }
        "budget" => r#"<path d="M3 3v18h18"/><path d="M7 14l4-4 4 4 5-5"/>"#,
        "history" => {
            r#"<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><polyline points="3 3 3 8 8 8"/><path d="M12 7v5l3 2"/>"#
        }

        // ── Analyze group (new marquee features) ─────────────────────
        // Simulator = sliders-horizontal.
        "simulator" => {
            r#"<line x1="21" y1="6" x2="3" y2="6"/><line x1="21" y1="12" x2="3" y2="12"/><line x1="21" y1="18" x2="3" y2="18"/><circle cx="8" cy="6" r="2"/><circle cx="16" cy="12" r="2"/><circle cx="10" cy="18" r="2"/>"#
        }
        // Rule Lab = flask-conical (the "lab" of the rule ladder).
        "rule-lab" => {
            r#"<path d="M10 2v7.31"/><path d="M14 9.3V2"/><path d="M8.5 2h7"/><path d="M14 9.3a6.5 6.5 0 1 1-4 0"/><path d="M5.58 16.5h12.85"/>"#
        }
        // Clipboard-list (used by skip-rule cards / settings)
        "rules" => {
            r#"<path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><rect x="8" y="2" width="8" height="4" rx="1"/><path d="M9 12h6"/><path d="M9 16h6"/>"#
        }
        "more" => {
            r#"<circle cx="5" cy="12" r="1.5"/><circle cx="12" cy="12" r="1.5"/><circle cx="19" cy="12" r="1.5"/>"#
        }

        // ── Config / settings ────────────────────────────────────────
        "sources" => {
            r#"<path d="M4.93 19.07A10 10 0 0 1 4.93 4.93"/><path d="M19.07 4.93a10 10 0 0 1 0 14.14"/><path d="M7.76 16.24a6 6 0 0 1 0-8.49"/><path d="M16.24 7.76a6 6 0 0 1 0 8.49"/><circle cx="12" cy="12" r="1.5"/>"#
        }
        "controllers" => {
            r#"<line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><line x1="1" y1="14" x2="7" y2="14"/><line x1="9" y1="8" x2="15" y2="8"/><line x1="17" y1="16" x2="23" y2="16"/>"#
        }
        "location" => {
            r#"<path d="M20 10c0 6-8 12-8 12s-8-6-8-12a8 8 0 0 1 16 0Z"/><circle cx="12" cy="10" r="3"/>"#
        }
        "ban" => {
            r#"<circle cx="12" cy="12" r="10"/><line x1="4.93" y1="4.93" x2="19.07" y2="19.07"/>"#
        }
        "calendar" => {
            r#"<rect x="3" y="4" width="18" height="18" rx="2"/><line x1="16" y1="2" x2="16" y2="6"/><line x1="8" y1="2" x2="8" y2="6"/><line x1="3" y1="10" x2="21" y2="10"/>"#
        }
        "llm" => {
            r#"<rect x="3" y="11" width="18" height="10" rx="2"/><circle cx="12" cy="5" r="2"/><path d="M12 7v4"/><line x1="8" y1="16" x2="8" y2="16.01"/><line x1="16" y1="16" x2="16" y2="16.01"/>"#
        }
        "bell" => {
            r#"<path d="M18 8a6 6 0 0 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.73 21a2 2 0 0 1-3.46 0"/>"#
        }
        "settings" => {
            r#"<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>"#
        }
        "units" => {
            r#"<path d="M21 3 3 21"/><path d="M3 9V3h6"/><path d="M21 15v6h-6"/><path d="M9 3h12v6"/><path d="M3 15v6h6"/>"#
        }
        "theme" => {
            r#"<circle cx="13.5" cy="6.5" r=".5"/><circle cx="17.5" cy="10.5" r=".5"/><circle cx="8.5" cy="7.5" r=".5"/><circle cx="6.5" cy="12.5" r=".5"/><path d="M12 2C6.5 2 2 6.5 2 12s4.5 10 10 10c.926 0 1.648-.746 1.648-1.688 0-.437-.18-.835-.437-1.125-.29-.289-.438-.652-.438-1.125a1.64 1.64 0 0 1 1.668-1.668h1.996c3.051 0 5.555-2.503 5.555-5.554C21.965 6.012 17.461 2 12 2z"/>"#
        }
        "advanced" => {
            r#"<path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/>"#
        }
        "wizard" => {
            r#"<path d="M12 3v3"/><path d="M18.5 5.5l-2.12 2.12"/><path d="M21 12h-3"/><path d="M18.5 18.5l-2.12-2.12"/><path d="M12 18v3"/><path d="M5.5 18.5l2.12-2.12"/><path d="M3 12h3"/><path d="M5.5 5.5l2.12 2.12"/>"#
        }
        "external" => {
            r#"<path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/><polyline points="15 3 21 3 21 9"/><line x1="10" y1="14" x2="21" y2="3"/>"#
        }
        "info" => {
            r#"<circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/>"#
        }

        // ── Controls / chrome ────────────────────────────────────────
        "play" => r#"<polygon points="6 3 20 12 6 21 6 3"/>"#,
        "stop" => r#"<rect x="5" y="5" width="14" height="14" rx="2"/>"#,
        "pause" => {
            r#"<rect x="6" y="4" width="4" height="16" rx="1"/><rect x="14" y="4" width="4" height="16" rx="1"/>"#
        }
        "plus" => r#"<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>"#,
        "minus" => r#"<line x1="5" y1="12" x2="19" y2="12"/>"#,
        "check" => r#"<polyline points="20 6 9 17 4 12"/>"#,
        "x" => r#"<line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/>"#,
        "chevron-right" => r#"<polyline points="9 18 15 12 9 6"/>"#,
        "chevron-down" => r#"<polyline points="6 9 12 15 18 9"/>"#,
        "chevron-up" => r#"<polyline points="18 15 12 9 6 15"/>"#,
        "arrow-up" => {
            r#"<line x1="12" y1="19" x2="12" y2="5"/><polyline points="5 12 12 5 19 12"/>"#
        }
        "arrow-down" => {
            r#"<line x1="12" y1="5" x2="12" y2="19"/><polyline points="19 12 12 19 5 12"/>"#
        }
        "refresh" => {
            r#"<path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-15 6.7L3 16"/><path d="M3 21v-5h5"/>"#
        }
        "download" => {
            r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/>"#
        }
        "print" => {
            r#"<polyline points="6 9 6 2 18 2 18 9"/><path d="M6 18H4a2 2 0 0 1-2-2v-5a2 2 0 0 1 2-2h16a2 2 0 0 1 2 2v5a2 2 0 0 1-2 2h-2"/><rect x="6" y="14" width="12" height="8"/>"#
        }

        // ── Weather glyphs (replace emoji) ───────────────────────────
        "sun" => {
            r#"<circle cx="12" cy="12" r="4"/><path d="M12 2v2"/><path d="M12 20v2"/><path d="m4.93 4.93 1.41 1.41"/><path d="m17.66 17.66 1.41 1.41"/><path d="M2 12h2"/><path d="M20 12h2"/><path d="m6.34 17.66-1.41 1.41"/><path d="m19.07 4.93-1.41 1.41"/>"#
        }
        "cloud" => {
            r#"<path d="M17.5 19a4.5 4.5 0 1 0 0-9 6 6 0 0 0-11.66 1.5A4 4 0 0 0 6.5 19Z"/>"#
        }
        "cloud-sun" => r#"<path d="M17.5 19a4.5 4.5 0 1 0-1.7-8.66 7 7 0 1 0-11.6 6.66"/>"#,
        "cloud-rain" => {
            r#"<path d="M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 0 9H7"/><line x1="8" y1="19" x2="8" y2="21"/><line x1="12" y1="19" x2="12" y2="22"/><line x1="16" y1="19" x2="16" y2="21"/>"#
        }
        "cloud-snow" => {
            r#"<path d="M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 0 9H7"/><line x1="8" y1="20" x2="8" y2="20.01"/><line x1="12" y1="20" x2="12" y2="20.01"/><line x1="16" y1="20" x2="16" y2="20.01"/>"#
        }
        "cloud-lightning" => {
            r#"<path d="M6 16.3A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 1.1 8.9"/><path d="m13 12-3 5h4l-3 5"/>"#
        }
        "cloud-fog" => {
            r#"<path d="M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 0 9"/><line x1="5" y1="20" x2="19" y2="20"/><line x1="7" y1="23" x2="17" y2="23"/>"#
        }
        "wind" => {
            r#"<path d="M12.8 19.6A2 2 0 1 0 14 16H2"/><path d="M17.5 8a2.5 2.5 0 1 1 2 4H2"/><path d="M9.8 4.4A2 2 0 1 1 11 8H2"/>"#
        }
        "thermometer" => r#"<path d="M14 4v10.54a4 4 0 1 1-4 0V4a2 2 0 0 1 4 0Z"/>"#,
        "gauge" => r#"<path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/>"#,
        "moon" => r#"<path d="M12 3a6 6 0 0 0 9 9 9 9 0 1 1-9-9Z"/>"#,
        "activity" => r#"<polyline points="22 12 18 12 15 21 9 3 6 12 2 12"/>"#,
        "hail" => {
            r#"<path d="M4 14.9A7 7 0 1 1 15.7 8h1.8a4.5 4.5 0 0 1 0 9H7"/><circle cx="8" cy="20" r="1"/><circle cx="12" cy="21" r="1"/><circle cx="16" cy="20" r="1"/>"#
        }

        _ => r#"<rect x="3" y="3" width="18" height="18" rx="2"/>"#,
    }
}
