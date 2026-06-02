// Wind compass + speed gauges. The compass is an inline SVG with a
// rotating two-tone needle driven by `rapid_wind_dir`; CSS handles
// the smooth transition so the needle visibly drifts when the
// 3-second sample updates. Beneath it sit lull / avg / gust as
// horizontal bars scaled to the larger of (gust, 30) so a calm day
// still has scale.
//
// The compass is theme-aware: the disc gradient, ticks, cardinal
// labels, needle, and hub all read from CSS custom properties so the
// face inverts cleanly in light mode without losing depth.

use crate::tempest::state::Snapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

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

    // 16-tick rose: every 22.5° gets a tick mark; the four cardinal
    // ticks (N, E, S, W) are longer and read as accents.
    let ticks: Vec<_> = (0..16)
        .map(|i| {
            let is_cardinal = i % 4 == 0;
            let angle_deg = (i as f64) * 22.5;
            let angle_rad = (angle_deg - 90.0).to_radians();
            let (inner_r, outer_r) = if is_cardinal {
                (78.0, 92.0)
            } else {
                (84.0, 92.0)
            };
            let cos = angle_rad.cos();
            let sin = angle_rad.sin();
            let x1 = 100.0 + inner_r * cos;
            let y1 = 100.0 + inner_r * sin;
            let x2 = 100.0 + outer_r * cos;
            let y2 = 100.0 + outer_r * sin;
            let cls = if is_cardinal {
                "compass-tick compass-tick--cardinal"
            } else {
                "compass-tick"
            };
            view! {
                <line
                    x1={format!("{:.2}", x1)}
                    y1={format!("{:.2}", y1)}
                    x2={format!("{:.2}", x2)}
                    y2={format!("{:.2}", y2)}
                    class=cls
                />
            }
            .into_any()
        })
        .collect();

    view! {
        <section class="panel wind">
            <h2 class="panel-title">"Wind"</h2>
            <div class="wind-row">
                <div class="compass" aria-label=move || format!("Wind from {}", cardinal())>
                    <svg viewBox="0 0 200 200" class="compass-svg" aria-hidden="true">
                        <defs>
                            <radialGradient id="compass-disc" cx="50%" cy="35%" r="70%">
                                <stop offset="0%" stop-color="var(--compass-bg-top)"/>
                                <stop offset="100%" stop-color="var(--compass-bg-bot)"/>
                            </radialGradient>
                            <linearGradient id="compass-needle-n" x1="0" y1="0" x2="0" y2="1">
                                <stop offset="0%" stop-color="var(--accent-cool)"/>
                                <stop offset="100%" stop-color="var(--accent)"/>
                            </linearGradient>
                            <linearGradient id="compass-needle-s" x1="0" y1="0" x2="0" y2="1">
                                <stop offset="0%" stop-color="var(--compass-needle-s-top)"/>
                                <stop offset="100%" stop-color="var(--compass-needle-s-bot)"/>
                            </linearGradient>
                            <radialGradient id="compass-hub" cx="35%" cy="30%" r="80%">
                                <stop offset="0%" stop-color="var(--compass-hub-top)"/>
                                <stop offset="100%" stop-color="var(--compass-hub-bot)"/>
                            </radialGradient>
                        </defs>

                        // Disc backdrop with a soft top-down highlight gradient.
                        <circle cx="100" cy="100" r="94" fill="url(#compass-disc)" class="compass-disc"/>
                        <circle cx="100" cy="100" r="94" class="compass-disc-border" fill="none"/>

                        // 16 tick marks (cardinals longer).
                        <g class="compass-ticks">{ticks}</g>

                        // Inner ring for a hint of inset depth.
                        <circle cx="100" cy="100" r="68" class="compass-inner-ring" fill="none"/>

                        // Cardinal labels.
                        <text x="100" y="24" class="compass-mark compass-mark--n">"N"</text>
                        <text x="178" y="102" class="compass-mark">"E"</text>
                        <text x="100" y="180" class="compass-mark">"S"</text>
                        <text x="22"  y="102" class="compass-mark">"W"</text>

                        // Two-tone needle. North half is the brand accent
                        // (this is what the wind is "from"); south half is a
                        // muted ink so it reads as a tail, not an arrow.
                        <g class="compass-needle"
                           style=move || format!("transform: rotate({}deg);", dir())>
                            <polygon
                                class="compass-needle-n"
                                fill="url(#compass-needle-n)"
                                points="100,30 108,96 100,100 92,96"
                            />
                            <polygon
                                class="compass-needle-s"
                                fill="url(#compass-needle-s)"
                                points="100,100 105,158 100,164 95,158"
                            />
                            <circle cx="100" cy="100" r="10" class="compass-hub-rim"/>
                            <circle cx="100" cy="100" r="7" fill="url(#compass-hub)" class="compass-hub"/>
                            <circle cx="100" cy="100" r="2.5" class="compass-hub-jewel"/>
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
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ][idx]
}
