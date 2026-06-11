// Weather + forecast panel. Surfaces every input the skip-check
// uses so the user can see the data, not just the verdict. Two
// sources for rain (Tempest local, Open-Meteo regional) shown
// side-by-side with the larger one — used by the skip-check —
// highlighted. When the gap between them is large (Tempest gauge
// underreporting), a "Tempest sheltered" hint flags it.

use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn ForecastPanel(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <section class="forecast-panel">
            <header class="forecast-head">
                <h3 class="forecast-title">"Forecast & Weather"</h3>
                {view! { <RainingNowBadge snap/> }.into_any()}
            </header>
            {view! { <RainBlock snap/> }.into_any()}
            {view! { <MultiDayRainBlock snap/> }.into_any()}
            {view! { <HeatStressBlock snap/> }.into_any()}
            {view! { <BalanceBlock snap/> }.into_any()}
            {view! { <DayBlock snap/> }.into_any()}
        </section>
    }
}

/// Live "raining now" badge that lights up green when the Tempest is
/// reporting active precipitation. Hides when calm.
#[component]
fn RainingNowBadge(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let raining = move || {
        let f = snap.get().forecast;
        f.rain_intensity_in_hr > 0.001 || f.rain_type != "none"
    };
    let label = move || {
        let f = snap.get().forecast;
        if f.rain_intensity_in_hr > 0.001 {
            format!("RAINING NOW · {:.2} in/hr", f.rain_intensity_in_hr)
        } else {
            "RAINING NOW".to_string()
        }
    };
    view! {
        <span class=move || if raining() { "raining-badge is-on" } else { "raining-badge" }>
            <span class="raining-dot"></span>
            {label}
        </span>
    }
}

/// Rain block: today (with both sources side-by-side), tomorrow,
/// 3-day. The "used" pill on whichever source is currently driving
/// the skip-check makes the data lineage obvious.
#[component]
fn RainBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let tempest_in = move || snap.get().forecast.rain_today_tempest_in;
    let om_in = move || snap.get().forecast.rain_today_om_in;
    let used_in = move || tempest_in().max(om_in());
    let tempest_used = move || tempest_in() >= om_in();
    let om_used = move || !tempest_used();
    let sheltered = move || om_in() - tempest_in() > 0.05;
    // Hover/aria-label text spelling out what "sheltered" means and
    // what the user can do about it. Surfacing this inline addresses
    // the feedback that the badge was opaque jargon with no path to
    // act on the insight.
    let sheltered_explain = move || {
        format!(
            "Open-Meteo is reading {:.2}\" more rain than your Tempest. \
             Local gauge may be blocked by tree cover or roof overhang. \
             Skip-check uses the max of both, so you're still protected. \
             Tap to review weather sources.",
            (om_in() - tempest_in()).max(0.0)
        )
    };

    view! {
        <div class="forecast-block">
            <div class="forecast-block-title">"Rain"</div>
            <div class="rain-today">
                <div class=move || if tempest_used() { "rain-card is-used" } else { "rain-card" }>
                    <div class="rain-card-label">"Tempest local"</div>
                    <div class="rain-card-value">
                        {move || format!("{:.2}\"", tempest_in())}
                    </div>
                    <div class="rain-card-hint">
                        {move || if sheltered() {
                            view! {
                                <a class="rain-card-hint-link"
                                    href="/settings/sources"
                                    title=sheltered_explain
                                    aria-label=sheltered_explain>
                                    "sheltered · trust OM "
                                    <span class="rain-card-hint-glyph" aria-hidden="true">"?"</span>
                                </a>
                            }.into_any()
                        } else if tempest_used() {
                            view! { <span>"used by skip-check"</span> }.into_any()
                        } else {
                            view! { <span></span> }.into_any()
                        }}
                    </div>
                </div>
                <div class=move || if om_used() { "rain-card is-used" } else { "rain-card" }>
                    <div class="rain-card-label">"Open-Meteo regional"</div>
                    <div class="rain-card-value">
                        {move || format!("{:.2}\"", om_in())}
                    </div>
                    <div class="rain-card-hint">
                        {move || if om_used() {
                            view! { <span>"used by skip-check"</span> }.into_any()
                        } else {
                            view! { <span></span> }.into_any()
                        }}
                    </div>
                </div>
            </div>
            <a class="rain-block-config-link" href="/settings/sources">
                "Configure rain sources \u{2192}"
            </a>
            <div class="rain-row">
                <RainBar
                    label="Today (used)"
                    value=Signal::derive(used_in)
                    threshold=Signal::derive(|| 0.05_f64)
                    threshold_label="Already-wet floor"
                />
                <RainBar
                    label="Tomorrow"
                    value=Signal::derive(move || snap.get().forecast.rain_tomorrow_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in)
                    threshold_label="Rain-skip threshold"
                />
                <RainBar
                    label="Next 3 days"
                    value=Signal::derive(move || snap.get().forecast.rain_3day_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in * 3.0)
                    threshold_label="3× skip threshold"
                />
            </div>
        </div>
    }
}

#[component]
fn RainBar(
    label: &'static str,
    value: Signal<f64>,
    threshold: Signal<f64>,
    threshold_label: &'static str,
) -> impl IntoView {
    let pct = move || {
        let max = (threshold.get() * 1.2).max(0.01);
        ((value.get() / max).clamp(0.0, 1.0) * 100.0).round()
    };
    let threshold_pct = move || {
        let max = (threshold.get() * 1.2).max(0.01);
        ((threshold.get() / max).clamp(0.0, 1.0) * 100.0).round()
    };
    view! {
        <div class=move || {
            if value.get() >= threshold.get() { "rain-bar rain-bar-above" } else { "rain-bar" }
        }>
            <div class="rain-bar-head">
                <span class="rain-bar-label">{label}</span>
                <span class="rain-bar-value">{move || format!("{:.2}\"", value.get())}</span>
            </div>
            <div class="rain-bar-track">
                <div class="rain-bar-fill" style=move || format!("width: {}%", pct())></div>
                <div
                    class="rain-bar-threshold"
                    style=move || format!("left: {}%", threshold_pct())
                    title=threshold_label
                ></div>
            </div>
            <div class="rain-bar-foot">
                {threshold_label} ": " {move || format!("{:.2}\"", threshold.get())}
            </div>
        </div>
    }
}

/// Multi-day forecast intelligence (Phase A). Shows the four rules
/// the engine added on top of the legacy 1-day check: next-4h hourly
/// rollup, probability-weighted tomorrow, 3-day weighted, 7-day
/// weighted. Each bar fills against its own threshold; bars that
/// have crossed go "above" red.
#[component]
fn MultiDayRainBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <div class="forecast-block nerd-only">
            <div class="forecast-block-title">"Forecast intelligence"</div>
            <div class="rain-row">
                <RainBar
                    label="Next 4h (hourly)"
                    value=Signal::derive(move || snap.get().forecast.rain_next_4h_in)
                    threshold=Signal::derive(|| 0.10_f64)
                    threshold_label="Skip-if-≥"
                />
                <RainBar
                    label="Tomorrow × confidence"
                    value=Signal::derive(move || {
                        let f = snap.get().forecast;
                        f.rain_tomorrow_in * (f.rain_tomorrow_prob_pct as f64) / 100.0
                    })
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in)
                    threshold_label="Rain-skip threshold"
                />
                <RainBar
                    label="3-day weighted"
                    value=Signal::derive(move || snap.get().forecast.rain_3day_weighted_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in * 1.5)
                    threshold_label="1.5× skip threshold"
                />
                <RainBar
                    label="7-day weighted"
                    value=Signal::derive(move || snap.get().forecast.rain_7day_weighted_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in * 3.0)
                    threshold_label="3× skip threshold (info)"
                />
            </div>
            <div class="forecast-block-foot">
                {move || {
                    let f = snap.get().forecast;
                    format!(
                        "Tomorrow: {:.2}″ at {}% confidence · Days since significant rain: {}",
                        f.rain_tomorrow_in, f.rain_tomorrow_prob_pct, f.days_since_significant_rain
                    )
                }}
            </div>
        </div>
    }
}

/// Heat stress tile. Shows the live heat index, the 3-day forecast peak,
/// the ET multiplier that Phase C will inject into Smart Irrigation's
/// Kc, and an "advisory" badge when the rule conditions are met.
#[component]
fn HeatStressBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let advisory_active = move || {
        let f = snap.get().forecast;
        let s = snap.get().skip_check;
        f.temp_max_3day_f >= 95.0
            && f.humidity_now_pct >= 60.0
            && f.days_since_significant_rain >= 2
            && f.rain_3day_weighted_in < 0.5 * s.rain_skip_in
    };
    view! {
        <div class="forecast-block">
            <div class="forecast-block-title">
                "Heat stress"
                <span
                    class=move || if advisory_active() {
                        "heat-advisory-badge is-on"
                    } else {
                        "heat-advisory-badge"
                    }
                >
                    "ADVISORY"
                </span>
            </div>
            <div class="kv-grid">
                <div class="kv">
                    <span class="k">"Heat index now"</span>
                    <span class="v">
                        {move || format!("{:.0}°F", snap.get().forecast.heat_index_now_f)}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Peak (next 3 days)"</span>
                    <span class="v">
                        {move || format!("{:.0}°F", snap.get().forecast.heat_index_max_3day_f)}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Today's high"</span>
                    <span class="v">
                        {move || format!("{:.0}°F", snap.get().forecast.temp_max_today_f)}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"3-day high"</span>
                    <span class="v">
                        {move || format!("{:.0}°F", snap.get().forecast.temp_max_3day_f)}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"ET multiplier (Phase C)"</span>
                    <span class=move || {
                        if snap.get().forecast.heat_multiplier > 1.05 { "v v-warn" } else { "v" }
                    }>
                        {move || format!("{:.2}×", snap.get().forecast.heat_multiplier)}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Overnight low (24h)"</span>
                    <span class=move || {
                        // skip_check.temp_min_24h_valid carries the "is this
                        // a real forecast low?" bit (same refresh fills both
                        // structs), so a genuine <= 0 °F low still warns.
                        let s = snap.get();
                        if s.skip_check.temp_min_24h_valid
                           && s.forecast.temp_min_24h_f < 38.0 {
                            "v v-warn"
                        } else { "v" }
                    }>
                        {move || format!("{:.0}°F", snap.get().forecast.temp_min_24h_f)}
                    </span>
                </div>
            </div>
        </div>
    }
}

/// Water balance: today's ET₀ vs today's rain. The sprinkler system's
/// nightly Smart Irrigation calc uses (ET₀ − rain) to update zone
/// buckets; surfacing it here so the user can correlate the bucket
/// mm in each zone tile with the underlying weather.
#[component]
fn BalanceBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let eto_today = move || snap.get().forecast.eto_today_mm;
    let eto_tomorrow = move || snap.get().forecast.eto_tomorrow_mm;
    let eto_3day = move || snap.get().forecast.eto_3day_avg_mm;
    let rain_today_mm = move || {
        let f = snap.get().forecast;
        f.rain_today_tempest_in.max(f.rain_today_om_in) * 25.4
    };
    let net = move || rain_today_mm() - eto_today();
    view! {
        <div class="forecast-block nerd-only">
            <div class="forecast-block-title">"Water budget"</div>
            <div class="kv-grid">
                <div class="kv">
                    <span class="k">"ET₀ today"</span>
                    <span class="v">{move || format!("{:.2} mm", eto_today())}</span>
                </div>
                <div class="kv">
                    <span class="k">"Rain today"</span>
                    <span class="v">{move || format!("{:.2} mm", rain_today_mm())}</span>
                </div>
                <div class="kv">
                    <span class="k">"Net (rain−ET₀)"</span>
                    <span class=move || {
                        if net() >= 0.0 { "v v-pos" } else { "v v-neg" }
                    }>
                        {move || format!("{:+.2} mm", net())}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"ET₀ tomorrow"</span>
                    <span class="v">{move || format!("{:.2} mm", eto_tomorrow())}</span>
                </div>
                <div class="kv">
                    <span class="k">"ET₀ 3-day avg"</span>
                    <span class="v">{move || format!("{:.2} mm", eto_3day())}</span>
                </div>
            </div>
        </div>
    }
}

/// Day block: temp range, peak wind, mean humidity. These feed the
/// freeze and wind skip rules; surfacing them here lets the user
/// see at a glance whether the weather day looks tame or sketchy.
#[component]
fn DayBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <div class="forecast-block nerd-only">
            <div class="forecast-block-title">"Today"</div>
            <div class="kv-grid">
                <div class="kv">
                    <span class="k">"Temp range"</span>
                    <span class="v">
                        {move || {
                            let f = snap.get().forecast;
                            format!("{:.0}° / {:.0}°", f.temp_min_today_f, f.temp_max_today_f)
                        }}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Peak wind"</span>
                    <span class="v">
                        {move || format!("{:.1} mph", snap.get().forecast.wind_max_today_mph)}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Mean humidity"</span>
                    <span class="v">
                        {move || format!("{:.0}%", snap.get().forecast.humidity_mean_today_pct)}
                    </span>
                </div>
            </div>
        </div>
    }
}
