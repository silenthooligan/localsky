// Weather hero, dense data strip, not a giant temperature billboard.
//
// Design: a single tier-1 panel that fits in ~140px vertical at desktop
// widths. Two rows:
//
//   row 1 (focal)
//     ┌──────┬──────────────────────────┬──────────────────────┐
//     │ ⛅   │ 72°  PARTLY SUNNY        │ feels 70°  · UV 4    │
//     └──────┴──────────────────────────┴──────────────────────┘
//
//   row 2 (telemetry strip, single horizontal line of stats)
//     ────────────────────────────────────────────────────────────
//     HUM 58%   DEW 56°   WET 64°   WIND 8mph NE   RAIN 0.0in/hr
//     PRESS 29.92↗
//
// The strip wraps to multiple rows on narrow viewports, but never adds
// vertical breathing room inside the card, every pixel of card height
// is used by data. Brand gradient accent stripe at the top.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::components::ui::Icon;
use crate::components::units_fmt::{
    fmt_pressure, fmt_rain_rate, fmt_temp_short, fmt_wind, use_unit_prefs,
};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

/// True when no physical weather station owns current conditions, i.e. a
/// cloud-only (Open-Meteo) deployment. Keys on the canonical `has_live_station`
/// signal (true the moment ANY live_current source, Tempest / Ecowitt / Davis /
/// MQTT / ..., claims a current-conditions field), NOT the old Tempest-only
/// serial + battery heuristic that misclassified a live Ecowitt/Davis/MQTT
/// station (no Tempest serial, no battery voltage) as cloud-only.
fn is_cloud_only(s: &Snapshot) -> bool {
    !s.has_live_station
}

/// Day/night decision for the forecast-driven glyph (clear vs moon, etc.).
/// Prefers the forecast's sunrise/sunset window for today; when those aren't
/// available yet, defaults to daytime so an empty/loading forecast renders a
/// daytime glyph rather than a moon at noon.
fn is_day_now(f: &ForecastSnapshot, s: &Snapshot) -> bool {
    let now = if s.last_packet_epoch > 0 {
        s.last_packet_epoch
    } else {
        f.last_refresh_epoch
    };
    match f.daily.first() {
        Some(today) if today.sunrise_epoch > 0 && today.sunset_epoch > 0 && now > 0 => {
            now >= today.sunrise_epoch && now < today.sunset_epoch
        }
        _ => true,
    }
}

/// Accent colour for a WMO weather code so the forecast-driven condition keeps
/// the same colour grammar the station-driven cascade uses (rain blue, sun
/// warm, storm lightning, fog/cloud dim, snow cool).
fn accent_for_code(code: u32) -> &'static str {
    match code {
        95 | 96 | 99 => "var(--accent-lightning)",
        51..=67 | 80..=82 => "var(--accent-rain)",
        71..=77 | 85 | 86 => "var(--accent-cool)",
        3 | 45 | 48 => "var(--text-dim)",
        // 0 / 1 (clear, mostly clear) and any unmapped code: warm/sun accent.
        _ => "var(--accent-warm)",
    }
}

#[component]
pub fn Hero(
    snap: ReadSignal<Snapshot>,
    /// Live forecast snapshot. When the deployment has no physical station
    /// (cloud-only), the headline condition is driven by the forecast's
    /// current `weather_code` (correct rain/snow/fog/storm) instead of the
    /// station-only solar-irradiance heuristic, which would read "Calm night"
    /// at noon for a cloud-only user. Optional so an older call site that
    /// only wires `snap` still compiles; absent => the solar cascade is used.
    /// CONTRACT: app.rs `render_hero` should pass `forecast=Some(forecast)`.
    #[prop(optional)]
    forecast: Option<ReadSignal<ForecastSnapshot>>,
) -> impl IntoView {
    let prefs = use_unit_prefs();
    // Choose the headline glyph + label + accent from the live state.
    // Order matters: lightning > rain > hail > then either the forecast
    // weather_code (cloud-only) or the station's solar irradiance.
    // Glyphs are themeable stroke icons (currentColor) tinted by accent,
    // not multicolor emoji, they read correctly in dark/light/hc.
    let condition = move || -> (&'static str, &'static str, &'static str) {
        let s = snap.get();
        // Live station / community sensors always win when they actually
        // observe weather, regardless of source: a real strike or measured
        // rain rate is ground truth over any model.
        if s.lightning_count_last_min > 0 || s.lightning_strikes_last_hour > 0 {
            return ("cloud-lightning", "Thunderstorm", "var(--accent-lightning)");
        } else if s.has_live_station && (s.precip_type == 1 || s.rain_intensity_in_hr > 0.0) {
            // Only a LIVE station observing precip earns the "Raining" headline.
            // On a cloud-only deploy (PirateWeather etc.) the bus path can write a
            // model-derived rain_intensity_in_hr; treating that as an OBSERVATION
            // would claim "Raining" off a forecast fill, so we fall through to the
            // forecast weather_code glyph below. Mirrors the irrigation RainingNow
            // badge's rain_is_live discipline (rain counts as "now" only when a
            // station measured it).
            return ("cloud-rain", "Raining", "var(--accent-rain)");
        } else if s.has_live_station && s.precip_type == 2 {
            return ("hail", "Hail", "var(--accent-cool)");
        }
        // Cloud-only (no station): drive the headline from the forecast's
        // current weather_code so a foggy / rainy / snowy / overcast day reads
        // correctly. The solar-irradiance heuristic below is station-shaped
        // and would mislabel a cloud-only deployment ("Calm night" at noon).
        if is_cloud_only(&s) {
            if let Some(fc) = forecast {
                let f = fc.get();
                if let Some(cur) = f.hourly.first() {
                    let is_day = is_day_now(&f, &s);
                    let (g, label) = weather_code_glyph(cur.weather_code, is_day);
                    return (g, label, accent_for_code(cur.weather_code));
                }
            }
        }
        // Station path (or cloud-only with no forecast yet): solar irradiance.
        if s.solar_w_m2 > 600.0 {
            ("sun", "Sunny", "var(--accent-warm)")
        } else if s.solar_w_m2 > 150.0 {
            ("cloud-sun", "Partly sunny", "var(--accent-warm)")
        } else if s.solar_w_m2 > 30.0 {
            ("cloud", "Cloudy", "var(--text-dim)")
        } else {
            ("moon", "Calm night", "var(--accent-cool)")
        }
    };

    // Provenance for the headline reading. The Snapshot's merged `source_label`
    // is the live owner of the headline air-temp reading (the per-field arbiter
    // sets it to whichever source owns air temp), so it is the right "via" for
    // the focal stats. The richer per-field source map rides the irrigation
    // snapshot, which Hero doesn't receive; the chip links to the data-sources
    // page where the full per-field breakdown lives. Empty => render no chip.
    let provenance = move || -> String { snap.get().source_label };

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
                <span class="hero-stat__v">{fmt_pressure(s.pressure_inhg, prefs.get())}</span>
                <span class=format!("trend {klass}")>{arrow}</span>
            </span>
        }
    };

    view! {
        <section class="hero panel is-tier-1" aria-label="Current weather">
            // Row 1: the focal info, glyph + temp + condition + the
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
                    // Provenance: a subtle "via {source}" chip on the headline
                    // reading, deep-linking to the per-field data-sources page so
                    // provenance surfaces at the point of consumption. Renders
                    // only once a source has claimed the reading (Show on empty).
                    <Show when=move || !provenance().is_empty()>
                        <a
                            class="hero-source"
                            href="/settings/data-sources"
                            title="Where this reading comes from"
                            style="display:inline-flex;align-items:center;gap:0.2em;font-size:0.7rem;\
                                   line-height:1;color:var(--text-dim);text-decoration:none;\
                                   opacity:0.85;margin-top:0.15rem;"
                        >
                            "via "{provenance}
                        </a>
                    </Show>
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

            // Row 2: the telemetry strip. Six high-density stats
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
                            format!("{} {}", fmt_wind(s.wind_avg_mph, prefs.get()), dir_card(s.wind_dir_deg))
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
