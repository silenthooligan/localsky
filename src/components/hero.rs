// Weather hero — dense data strip, not a giant temperature billboard.
//
// Design: a single tier-1 panel that fits in ~140px vertical at desktop
// widths. Two rows:
//
//   row 1 (focal)
//     ┌──────┬──────────────────────────┬──────────────────────┐
//     │ ⛅   │ 72°  PARTLY SUNNY        │ feels 70°  · UV 4    │
//     └──────┴──────────────────────────┴──────────────────────┘
//
//   row 2 (telemetry strip — single horizontal line of stats)
//     ────────────────────────────────────────────────────────────
//     HUM 58%   DEW 56°   WET 64°   WIND 8mph NE   RAIN 0.0in/hr
//     PRESS 29.92↗
//
// The strip wraps to multiple rows on narrow viewports, but never adds
// vertical breathing room inside the card — every pixel of card height
// is used by data. Brand gradient accent stripe at the top.

use crate::components::ui::Icon;
use crate::components::units_fmt::{fmt_rain_rate, fmt_temp_short, use_unit_prefs};
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn Hero(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // Choose the headline glyph + label + accent from the live state.
    // Order matters: lightning > rain > hail > sun-by-irradiance > night.
    // Glyphs are themeable stroke icons (currentColor) tinted by accent,
    // not multicolor emoji — they read correctly in dark/light/hc.
    let condition = move || -> (&'static str, &'static str, &'static str) {
        let s = snap.get();
        if s.lightning_count_last_min > 0 || s.lightning_strikes_last_hour > 0 {
            ("cloud-lightning", "Thunderstorm", "var(--accent-lightning)")
        } else if s.precip_type == 1 || s.rain_intensity_in_hr > 0.0 {
            ("cloud-rain", "Raining", "var(--accent-rain)")
        } else if s.precip_type == 2 {
            ("hail", "Hail", "var(--accent-cool)")
        } else if s.solar_w_m2 > 600.0 {
            ("sun", "Sunny", "var(--accent-warm)")
        } else if s.solar_w_m2 > 150.0 {
            ("cloud-sun", "Partly sunny", "var(--accent-warm)")
        } else if s.solar_w_m2 > 30.0 {
            ("cloud", "Cloudy", "var(--text-dim)")
        } else {
            ("moon", "Calm night", "var(--accent-cool)")
        }
    };

    // Wind direction in 16-point cardinal form.
    fn dir_card(deg: f64) -> &'static str {
        const POINTS: [&str; 16] = [
            "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
            "NW", "NNW",
        ];
        let normalized = (deg.rem_euclid(360.0) + 11.25) / 22.5;
        POINTS[(normalized as usize) % 16]
    }

    // Pressure trend from the last 3 hours of samples (>0.05 rising,
    // <-0.05 falling, else steady).
    let pressure_chip = move || {
        let s = snap.get();
        let trend = &s.pressure_trend_inhg;
        let (arrow, klass) = if trend.len() >= 2 {
            let first = trend.first().map(|(_, v)| *v).unwrap_or(s.pressure_inhg);
            let last = trend.last().map(|(_, v)| *v).unwrap_or(s.pressure_inhg);
            let delta = last - first;
            if delta > 0.05 {
                ("↗", "trend--up")
            } else if delta < -0.05 {
                ("↘", "trend--down")
            } else {
                ("→", "trend--flat")
            }
        } else {
            ("→", "trend--flat")
        };
        view! {
            <span class="hero-stat">
                <span class="hero-stat__k">"PRESS"</span>
                <span class="hero-stat__v">{format!("{:.2}", s.pressure_inhg)}</span>
                <span class=format!("trend {klass}")>{arrow}</span>
            </span>
        }
    };

    view! {
        <section class="hero panel is-tier-1" aria-label="Current weather">
            // Row 1: the focal info — glyph + temp + condition + the
            // two most-asked secondary stats inline.
            <div class="hero-focal">
                {move || {
                    let (icon, _, color) = condition();
                    view! {
                        <div class="hero-glyph" aria-hidden="true" style=format!("color:{color}")>
                            <Icon name=icon size=46 stroke=1.6/>
                        </div>
                    }
                }}
                <div class="hero-headline">
                    <div class="hero-temp">
                        {move || fmt_temp_short(snap.get().air_temp_f, prefs.get())}
                    </div>
                    <div class="hero-condition">{move || condition().1}</div>
                </div>
                <div class="hero-callouts">
                    <span class="hero-callout">
                        <span class="hero-callout__k">"feels"</span>
                        <span class="hero-callout__v">
                            {move || fmt_temp_short(snap.get().feels_like_f, prefs.get())}
                        </span>
                    </span>
                    <span class="hero-callout">
                        <span class="hero-callout__k">"UV"</span>
                        <span class="hero-callout__v">
                            {move || format!("{:.0}", snap.get().uv_index)}
                        </span>
                    </span>
                </div>
            </div>

            // Row 2: the telemetry strip. Six high-density stats —
            // monospace nums + uppercase meta labels. Single horizontal
            // line on wide screens, wraps to 2-3 rows on phones.
            <div class="hero-strip" role="list" aria-label="Current readings">
                <span class="hero-stat" role="listitem">
                    <span class="hero-stat__k">"HUM"</span>
                    <span class="hero-stat__v">{move || format!("{:.0}%", snap.get().rh_pct)}</span>
                </span>
                <span class="hero-stat" role="listitem">
                    <span class="hero-stat__k">"DEW"</span>
                    <span class="hero-stat__v">{move || fmt_temp_short(snap.get().dew_point_f, prefs.get())}</span>
                </span>
                <span class="hero-stat" role="listitem">
                    <span class="hero-stat__k">"WET"</span>
                    <span class="hero-stat__v">{move || fmt_temp_short(snap.get().wet_bulb_f, prefs.get())}</span>
                </span>
                <span class="hero-stat" role="listitem">
                    <span class="hero-stat__k">"WIND"</span>
                    <span class="hero-stat__v">
                        {move || {
                            let s = snap.get();
                            format!("{:.0}mph {}", s.wind_avg_mph, dir_card(s.wind_dir_deg))
                        }}
                    </span>
                </span>
                <span class="hero-stat" role="listitem">
                    <span class="hero-stat__k">"RAIN"</span>
                    <span class="hero-stat__v">
                        {move || fmt_rain_rate(snap.get().rain_intensity_in_hr, prefs.get())}
                    </span>
                </span>
                {pressure_chip}
            </div>
        </section>
    }
}
