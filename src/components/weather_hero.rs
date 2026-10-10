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

use crate::components::ui::{Icon, TemperatureValue};
use crate::components::units_fmt::{fmt_pressure, fmt_rain_rate, fmt_wind, use_unit_prefs};
use crate::engine::sky::{SkyBasis, SkyCondition, SkyNow, SunPhase};
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

const WARM: &str = "var(--accent-warm)";
const COOL: &str = "var(--accent-cool)";
const DIM: &str = "var(--text-dim)";
const RAIN: &str = "var(--accent-rain)";
const LIGHTNING: &str = "var(--accent-lightning)";

/// Glyph, label and accent for the server's judgement of the sky. The server
/// weighs every source the deployment has (`engine::sky`); the card only
/// draws it, so the card and Home Assistant always agree.
fn sky_presentation(sky: &SkyNow) -> (&'static str, String, &'static str) {
    use SkyCondition as C;
    let night = sky.is_day == Some(false);
    let (icon, label, accent) = match sky.condition {
        C::Thunderstorm => ("cloud-lightning", "Lightning nearby", LIGHTNING),
        C::Hail => ("hail", "Hail", COOL),
        C::Snow => ("cloud-snow", "Snow", COOL),
        C::WintryMix => ("cloud-snow", "Wintry mix", COOL),
        C::HeavyRain => ("cloud-rain", "Heavy rain", RAIN),
        C::Rain => ("cloud-rain", "Rain", RAIN),
        C::LightRain => ("cloud-drizzle", "Light rain", RAIN),
        C::Fog => ("cloud-fog", "Fog", DIM),
        C::LowVisibility => ("cloud-fog", "Low visibility", DIM),
        C::Clear if sky.is_day.is_none() => ("cloud", "Clear", DIM),
        C::Clear if night => ("moon", "Clear", COOL),
        C::Clear => ("sun", "Sunny", WARM),
        C::MostlyClear if night => ("cloud-moon", "Mostly clear", COOL),
        C::MostlyClear if sky.is_day.is_none() => ("cloud", "Mostly clear", DIM),
        C::MostlyClear => ("cloud-sun", "Mostly sunny", WARM),
        C::PartlyCloudy if night => ("cloud-moon", "Partly cloudy", COOL),
        C::PartlyCloudy if sky.is_day.is_none() => ("cloud", "Partly cloudy", DIM),
        C::PartlyCloudy => ("cloud-sun", "Partly cloudy", WARM),
        C::MostlyCloudy => ("cloud", "Mostly cloudy", DIM),
        C::Overcast => ("cloud", "Overcast", DIM),
        // No evidence about the sky: say where the sun is, never guess.
        C::Unknown => match sky.phase {
            Some(SunPhase::Dawn) => ("sunrise", "Sunrise", WARM),
            Some(SunPhase::Dusk) => ("sunset", "Sunset", WARM),
            Some(SunPhase::Night) => ("moon", "Night", COOL),
            Some(SunPhase::Day) => ("sun", "Daytime", WARM),
            None if sky.windy => return ("wind", "Windy".into(), DIM),
            None => ("cloud", "Sky not reported", DIM),
        },
    };
    let label = if sky.windy {
        format!("{label}, windy")
    } else {
        label.to_string()
    };
    (icon, label, accent)
}

/// Short provenance at the point of use, with limitations available on tap.
/// Never label a sunlight or forecast estimate as a measured cloud fraction.
fn sky_evidence(sky: &SkyNow) -> (&'static str, &'static str) {
    if sky.condition == SkyCondition::Thunderstorm {
        return ("Lightning detected", "A strike was detected within 10 miles (16 km) in the last 15 minutes. Rain may be elsewhere.");
    }
    if sky.precipitating {
        return ("Precipitation detected", "Based on a fresh gauge, radar or hail reading. A rain rate alone cannot identify snow or freezing rain.");
    }
    match sky.cover_basis {
        SkyBasis::Observation => ("Reported conditions", "From a fresh sensor or weather-station report. A nearby station can differ from your location; low visibility does not identify its cause."),
        SkyBasis::MeasuredSunlight => ("Estimated from sunlight", "Sunlight is compared with a clear-sky model. Shade, haze and sensor placement can affect this estimate; it is not a cloud-cover measurement."),
        SkyBasis::Model => ("Estimated conditions", "From your weather provider's current model. Conditions at your location may differ."),
        SkyBasis::Forecast => ("From the hourly forecast", "No current sky report is available. This estimate uses the current forecast hour; a rain or storm forecast is not an observation."),
        SkyBasis::None => ("Sky not reported", "No fresh source reports the sky. Day and night follow the sun at your configured location."),
    }
}

#[component]
pub fn Hero(snap: ReadSignal<Snapshot>) -> impl IntoView {
    view! {
        <section class="hero panel is-tier-1" aria-label="Current weather">
            {move || {
                // Boot/empty window: before ANY source has written a reading
                // the snapshot is all Snapshot::default() zeros, and
                // rendering them presented a confident 0-degree "Calm night"
                // dashboard for the whole restart window. Show the standard
                // skeleton treatment instead. SSR and hydrate's first frame
                // both take this branch (signals start at default), so
                // hydration stays sound; the SSE push swaps in the real
                // panel client-side.
                if !snap.get().has_any_reading() {
                    return view! {
                        <div class="hero-focal" aria-hidden="true">
                            <div class="hero-glyph">
                                <crate::components::ui::Skeleton variant="tile"/>
                            </div>
                            <div class="hero-headline">
                                <crate::components::ui::Skeleton variant="block" width="9rem"/>
                                <crate::components::ui::Skeleton variant="line" width="6rem"/>
                            </div>
                        </div>
                        <div class="hero-strip" role="status" aria-label="Waiting for the first reading">
                            <crate::components::ui::Skeleton variant="row"/>
                        </div>
                    }
                    .into_any();
                }
                view! { <HeroReadings snap/> }.into_any()
            }}
        </section>
    }
}

/// The populated hero content (focal row + telemetry strip), rendered only
/// once the snapshot carries at least one real reading (see the warming-up
/// guard in [`Hero`]).
#[component]
fn HeroReadings(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // The headline glyph + label + accent. Glyphs are themeable stroke icons
    // (currentColor) tinted by accent, so they read correctly in every theme.
    let condition = move || -> (&'static str, String, &'static str) {
        match snap.get().sky.as_ref() {
            Some(sky) => sky_presentation(sky),
            // Served snapshots always carry a sky; this is only a stream that
            // predates it.
            None => ("cloud", "Sky not reported".into(), DIM),
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
            ("—", "trend--pending")
        };
        view! {
            <crate::components::ui::StatTile layout="inline" role="listitem" label="PRESS"
                value=fmt_pressure(s.pressure_inhg, prefs.get()) detail=arrow detail_class=format!("trend {klass}")/>

        }
    };
    view! {
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
                        <TemperatureValue value=Signal::derive(move || snap.get().air_temp_f)/>
                    </div>
                    <div class="hero-condition">{move || condition().1}</div>
                    {move || snap.get().sky.map(|sky| {
                        let (label, explanation) = sky_evidence(&sky);
                        view! {
                            <details class="hero-sky-evidence">
                                <summary>{label}<span aria-hidden="true">" ⓘ"</span></summary>
                                <p>{explanation}</p>
                            </details>
                        }
                    })}
                    // Provenance: a subtle "via {source}" chip on the headline
                    // reading, deep-linking to the per-field data-sources page so
                    // provenance surfaces at the point of consumption. Renders
                    // only once a source has claimed the reading (Show on empty).
                    <Show when=move || !provenance().is_empty()>
                        <a
                            class="hero-source"
                            href="/settings?section=devices"
                            title="Temperature source"
                            style="display:inline-flex;align-items:center;gap:0.2em;font-size:0.7rem;\
                                   line-height:1;color:var(--text-dim);text-decoration:none;\
                                   opacity:0.85;margin-top:0.15rem;"
                        >
                            "Temperature via "{provenance}
                        </a>
                    </Show>
                </div>
                <div class="hero-callouts">
                    <span class="hero-callout">
                        <span class="hero-callout__k">"Feels like"</span>
                        <span class="hero-callout__v">
                            <TemperatureValue value=Signal::derive(move || snap.get().feels_like_f)/>
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
                <crate::components::ui::StatTile layout="inline" role="listitem" label="HUM"
                    value=Signal::derive(move || format!("{:.0}%", snap.get().rh_pct))/>
                <crate::components::ui::StatTile layout="inline" role="listitem" label="Dew point"
                    temperature_f=Signal::derive(move || snap.get().dew_point_f)/>
                <crate::components::ui::StatTile layout="inline" role="listitem" label="Wet bulb"
                    temperature_f=Signal::derive(move || snap.get().wet_bulb_f)/>
                <crate::components::ui::StatTile layout="inline" role="listitem" label="WIND"
                    value=Signal::derive(move || {
                        let s = snap.get();
                        format!("{} {}", fmt_wind(s.wind_avg_mph, prefs.get()), dir_card(s.wind_dir_deg))
                    })/>
                <crate::components::ui::StatTile layout="inline" role="listitem" label="RAIN"
                    value=Signal::derive(move || fmt_rain_rate(snap.get().rain_intensity_in_hr, prefs.get()))/>
                {pressure_chip}
            </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sky::SkyBasis;

    fn sky(condition: SkyCondition, phase: Option<SunPhase>, windy: bool) -> SkyNow {
        SkyNow {
            condition,
            phase,
            is_day: phase.map(|p| p != SunPhase::Night),
            cloud_cover_pct: None,
            cover_basis: SkyBasis::None,
            precipitating: false,
            windy,
            at_epoch: 0,
        }
    }

    #[test]
    fn night_and_day_draw_their_own_sky() {
        let night = Some(SunPhase::Night);
        let day = Some(SunPhase::Day);
        assert_eq!(
            sky_presentation(&sky(SkyCondition::Clear, night, false)).1,
            "Clear"
        );
        assert_eq!(
            sky_presentation(&sky(SkyCondition::Clear, night, false)).0,
            "moon"
        );
        assert_eq!(
            sky_presentation(&sky(SkyCondition::Clear, day, false)).1,
            "Sunny"
        );
        assert_eq!(
            sky_presentation(&sky(SkyCondition::MostlyClear, night, false)).0,
            "cloud-moon"
        );
        assert_eq!(
            sky_presentation(&sky(SkyCondition::Overcast, day, false)).1,
            "Overcast"
        );
    }

    #[test]
    fn an_unknown_sky_names_the_sun_instead_of_guessing() {
        let label = |phase| sky_presentation(&sky(SkyCondition::Unknown, phase, false)).1;
        assert_eq!(label(Some(SunPhase::Dawn)), "Sunrise");
        assert_eq!(label(Some(SunPhase::Dusk)), "Sunset");
        assert_eq!(label(Some(SunPhase::Night)), "Night");
        assert_eq!(label(None), "Sky not reported");
    }

    #[test]
    fn wind_qualifies_the_sky() {
        let night = Some(SunPhase::Night);
        assert_eq!(
            sky_presentation(&sky(SkyCondition::PartlyCloudy, night, true)).1,
            "Partly cloudy, windy"
        );
        assert_eq!(
            sky_presentation(&sky(SkyCondition::Unknown, None, true)).1,
            "Windy"
        );
    }
}
