// Wind compass + speed gauges. The compass is an inline SVG with a
// rotating arrow driven by `rapid_wind_dir`; CSS handles the smooth
// transition so the needle visibly drifts when the 3-second sample
// updates. Beneath it sit lull / avg / gust as horizontal bars
// scaled to the larger of (gust, 30) so a calm day still has scale.

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn WindPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let dir = move || {
        let s = snap.get();
        // Prefer rapid_wind direction; fall back to obs_st avg when no rapid yet.
        if s.rapid_wind_mph > 0.0 || s.last_packet_epoch == 0 {
            s.rapid_wind_dir
        } else {
            s.wind_dir_deg
        }
    };
    let cardinal = move || cardinal_for(dir());
    let scale = move || {
        let s = snap.get();
        s.wind_gust_mph.max(30.0)
    };

    view! {
        <section class="panel wind">
            <h2 class="panel-title">"Wind"</h2>
            <div class="wind-row">
                <div class="compass" aria-label=move || format!("Wind from {}", cardinal())>
                    <svg viewBox="0 0 100 100" class="compass-svg">
                        <circle cx="50" cy="50" r="46" class="compass-ring"/>
                        <text x="50" y="11" class="compass-mark">"N"</text>
                        <text x="91" y="53" class="compass-mark">"E"</text>
                        <text x="50" y="95" class="compass-mark">"S"</text>
                        <text x="9"  y="53" class="compass-mark">"W"</text>
                        <g class="compass-needle"
                           style=move || format!("transform: rotate({}deg);", dir())>
                            <polygon points="50,8 56,52 50,46 44,52" class="needle-shaft"/>
                            <circle cx="50" cy="50" r="3" class="needle-hub"/>
                        </g>
                    </svg>
                    <div class="compass-readout">
                        <div class="compass-card">{cardinal}</div>
                        <div class="compass-deg">{move || format!("{:.0}°", dir())}</div>
                    </div>
                </div>
                <div class="wind-bars">
                    <WindBar label="lull" mph=move || snap.get().wind_lull_mph scale=scale color="cool"/>
                    <WindBar label="avg"  mph=move || snap.get().wind_avg_mph  scale=scale color="mid"/>
                    <WindBar label="gust" mph=move || snap.get().wind_gust_mph scale=scale color="hot"/>
                    <WindBar label="now"  mph=move || snap.get().rapid_wind_mph scale=scale color="live"/>
                </div>
            </div>
        </section>
    }
}

#[component]
fn WindBar<F, S>(label: &'static str, mph: F, scale: S, color: &'static str) -> impl IntoView
where
    F: Fn() -> f64 + Copy + Send + Sync + 'static,
    S: Fn() -> f64 + Copy + Send + Sync + 'static,
{
    let pct = move || (mph() / scale().max(0.1)) * 100.0;
    view! {
        <div class={format!("wind-bar wind-bar-{}", color)}>
            <span class="wind-bar-label">{label}</span>
            <div class="wind-bar-track">
                <div class="wind-bar-fill" style=move || format!("width: {:.1}%;", pct())></div>
            </div>
            <span class="wind-bar-value">{move || format!("{:.1} mph", mph())}</span>
        </div>
    }
}

fn cardinal_for(deg: f64) -> &'static str {
    let d = ((deg % 360.0) + 360.0) % 360.0;
    let idx = ((d + 11.25) / 22.5).floor() as usize % 16;
    [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE",
        "S", "SSW", "SW", "WSW", "W", "WNW", "NW", "NNW",
    ][idx]
}
