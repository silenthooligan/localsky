// Weather + forecast panel. Surfaces every input the skip-check
// uses so the user can see the data, not just the verdict. Two
// sources for rain (Tempest local, Open-Meteo regional) shown
// side-by-side with the larger one, used by the skip-check
// highlighted. When the gap between them is large (Tempest gauge
// underreporting), a "Tempest sheltered" hint flags it.

use crate::components::units_fmt::{
    depth_unit, depth_value_in, depth_value_mm, fmt_rain_amount, fmt_rain_amount_mm, fmt_rain_rate,
    fmt_temp_short, fmt_wind, temp_unit, temp_value, use_unit_prefs, UnitPrefs,
};
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
            // Lawn-care-first order: the water BALANCE (does the lawn need water?)
            // headlines, then the RAIN outlook (will rain handle it?), then the
            // STRESS flags (heat / freeze / wind). The raw engine math
            // (forecast intelligence, water-budget figures, day detail) stays
            // below in nerd mode for auditing the decision.
            <div class="forecast-blocks">
                {view! { <BalanceHeadline snap/> }.into_any()}
                {view! { <RainBlock snap/> }.into_any()}
                {view! { <StressFlags snap/> }.into_any()}
                {view! { <MultiDayRainBlock snap/> }.into_any()}
                {view! { <BalanceBlock snap/> }.into_any()}
                {view! { <DayBlock snap/> }.into_any()}
            </div>
        </section>
    }
}

/// Water-balance headline -- the first thing a homeowner wants to know: did the
/// lawn gain water (rain) or lose it to evapotranspiration today, and what's the
/// net? Promotes the formerly nerd-only budget math to the top of the panel with
/// a proportional rain-vs-ET bar so the balance reads at a glance.
#[component]
fn BalanceHeadline(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // Rain source is INCHES; keep both: inches for the display figure
    // (so the unit toggle drives it), mm for the internal bar math.
    let rain_in = move || {
        let f = snap.get().forecast;
        f.rain_today_tempest_in.max(f.rain_today_om_in)
    };
    let rain_mm = move || rain_in() * 25.4;
    let et_mm = move || snap.get().forecast.eto_today_mm;
    let net = move || rain_mm() - et_mm();
    let drying = move || net() < -0.05;
    let rain_pct = move || {
        let total = (rain_mm() + et_mm()).max(0.01);
        (rain_mm() / total * 100.0).clamp(2.0, 98.0)
    };
    let status = move || {
        let n = net();
        if n >= 0.05 {
            "Rain more than covered today's loss".to_string()
        } else if n >= -0.05 {
            "Rain and evaporation roughly balanced".to_string()
        } else {
            "Drying out, losing more than it's getting".to_string()
        }
    };
    view! {
        <div class="forecast-block fc-balance">
            <div class="forecast-block-title">"Today's water balance"</div>
            <div class="balance-figures">
                <div class="balance-fig balance-fig--in">
                    <span class="balance-fig__k">"Rain in"</span>
                    <span class="balance-fig__v">
                        {move || format!("+{}", depth_value_in(rain_in(), prefs.get()))}
                        <span class="balance-fig__u">{move || depth_unit(prefs.get())}</span>
                    </span>
                </div>
                <div class="balance-fig balance-fig--out">
                    <span class="balance-fig__k">"Lost to sun & heat (ET)"</span>
                    <span class="balance-fig__v">
                        {move || format!("-{}", depth_value_mm(et_mm(), prefs.get()))}
                        <span class="balance-fig__u">{move || depth_unit(prefs.get())}</span>
                    </span>
                </div>
                <div class=move || {
                    if drying() {
                        "balance-fig balance-fig--net is-drying"
                    } else {
                        "balance-fig balance-fig--net is-wet"
                    }
                }>
                    <span class="balance-fig__k">"Net"</span>
                    <span class="balance-fig__v">
                        {move || {
                            let n = net();
                            let sign = if n >= 0.0 { "+" } else { "-" };
                            format!("{sign}{}", depth_value_mm(n.abs(), prefs.get()))
                        }}
                        <span class="balance-fig__u">{move || depth_unit(prefs.get())}</span>
                    </span>
                </div>
            </div>
            <div class="balance-bar" role="img" aria-label=status>
                <div class="balance-bar__rain" style=move || format!("width: {}%", rain_pct())></div>
                <div class="balance-bar__et"></div>
            </div>
            <div class=move || if drying() { "balance-status is-drying" } else { "balance-status" }>
                {status}
            </div>
        </div>
    }
}

/// At-a-glance stress flags: heat, freeze, and wind. Each tile stays quiet until
/// its rule actually trips, then goes loud so the homeowner sees the one thing
/// affecting tonight's run without reading the full skip-check breakdown.
#[component]
fn StressFlags(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let heat_flagged = move || {
        let f = snap.get().forecast;
        let s = snap.get().skip_check;
        f.temp_max_3day_f >= 95.0
            && f.humidity_now_pct >= 60.0
            && f.days_since_significant_rain >= 2
            && f.rain_3day_weighted_in < 0.5 * s.rain_skip_in
    };
    let freeze_flagged = move || {
        let s = snap.get();
        s.skip_check.temp_min_24h_valid && s.forecast.temp_min_24h_f < s.skip_check.min_temp_f
    };
    let wind_flagged = move || {
        let s = snap.get();
        s.forecast.wind_max_today_mph > s.skip_check.max_wind_mph
    };
    view! {
        <div class="forecast-block fc-stress">
            <div class="forecast-block-title">"Conditions & stress"</div>
            <div class="stress-tiles">
                {view! {
                    <StressTile
                        label="Heat index"
                        value=Signal::derive(move || format!("{}{}", temp_value(snap.get().forecast.heat_index_now_f, prefs.get()), temp_unit(prefs.get())))
                        sub=Signal::derive(move || format!("peak {}{}", temp_value(snap.get().forecast.heat_index_max_3day_f, prefs.get()), temp_unit(prefs.get())))
                        flagged=Signal::derive(heat_flagged)
                        flag_note="heat advisory"
                    />
                }.into_any()}
                {view! {
                    <StressTile
                        label="Overnight low"
                        value=Signal::derive(move || format!("{}{}", temp_value(snap.get().forecast.temp_min_24h_f, prefs.get()), temp_unit(prefs.get())))
                        sub=Signal::derive(move || format!("freeze < {}{}", temp_value(snap.get().skip_check.min_temp_f, prefs.get()), temp_unit(prefs.get())))
                        flagged=Signal::derive(freeze_flagged)
                        flag_note="freeze risk"
                    />
                }.into_any()}
                {view! {
                    <StressTile
                        label="Peak wind"
                        value=Signal::derive(move || fmt_wind(snap.get().forecast.wind_max_today_mph, prefs.get()))
                        sub=Signal::derive(move || format!("skip > {}", fmt_wind(snap.get().skip_check.max_wind_mph, prefs.get())))
                        flagged=Signal::derive(wind_flagged)
                        flag_note="too windy"
                    />
                }.into_any()}
            </div>
        </div>
    }
}

#[component]
fn StressTile(
    label: &'static str,
    value: Signal<String>,
    sub: Signal<String>,
    flagged: Signal<bool>,
    flag_note: &'static str,
) -> impl IntoView {
    view! {
        <div class=move || if flagged.get() { "stress-tile is-flagged" } else { "stress-tile" }>
            <span class="stress-tile__k">{label}</span>
            <span class="stress-tile__v">{move || value.get()}</span>
            <span class="stress-tile__sub">
                {move || if flagged.get() { flag_note.to_string() } else { sub.get() }}
            </span>
        </div>
    }
}

/// Live "raining now" badge that lights up green ONLY when a LIVE source is
/// actually OBSERVING active precipitation. Hides when calm. On a cloud-only or
/// station-stale install the intensity/type fields are an Open-Meteo current-hour
/// forecast FILL (a model prediction), not an observation, so the badge stays
/// calm there (T3): the green "RAINING NOW" must mean a station is measuring
/// rain right now, not that the model expects some. The forecast's rain
/// expectation is already represented in the rain-outlook block below.
#[component]
fn RainingNowBadge(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let raining = move || {
        let f = snap.get().forecast;
        // Gate on rain_is_live: only a live source that currently owns the rain
        // reading can light the OBSERVED badge. A forecast fill (rain_is_live ==
        // false) never does, even when it carries a non-zero intensity / "rain".
        f.rain_is_live && (f.rain_intensity_in_hr > 0.001 || f.rain_type != "none")
    };
    let label = move || {
        let f = snap.get().forecast;
        if f.rain_intensity_in_hr > 0.001 {
            // rain_intensity is IN/HR; route through the rate formatter.
            format!(
                "RAINING NOW · {}",
                fmt_rain_rate(f.rain_intensity_in_hr, prefs.get())
            )
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
    let prefs = use_unit_prefs();
    let tempest_in = move || snap.get().forecast.rain_today_tempest_in;
    let om_in = move || snap.get().forecast.rain_today_om_in;
    let used_in = move || tempest_in().max(om_in());
    let tempest_used = move || tempest_in() >= om_in();
    let om_used = move || !tempest_used();
    let sheltered = move || om_in() - tempest_in() > 0.05;
    // Real source provenance for the two cards (fallbacks keep the UI sane
    // before the engine has populated them).
    let station_label = move || {
        let l = snap.get().forecast.station_source_label;
        if l.is_empty() {
            "Station local".to_string()
        } else {
            format!("{l} local")
        }
    };
    let forecast_label = move || {
        let l = snap.get().forecast.forecast_source_label;
        if l.is_empty() {
            "Forecast regional".to_string()
        } else {
            format!("{l} regional")
        }
    };
    // Hover/aria-label text spelling out what "sheltered" means and
    // what the user can do about it. Surfacing this inline addresses
    // the feedback that the badge was opaque jargon with no path to
    // act on the insight.
    let sheltered_explain = move || {
        let s = snap.get();
        let fl = if s.forecast.forecast_source_label.is_empty() {
            "The forecast".to_string()
        } else {
            s.forecast.forecast_source_label.clone()
        };
        let sl = if s.forecast.station_source_label.is_empty() {
            "your station".to_string()
        } else {
            format!("your {}", s.forecast.station_source_label)
        };
        format!(
            "{fl} is reading {} more rain than {sl}. \
             Local gauge may be blocked by tree cover or roof overhang. \
             Skip-check uses the max of both, so you're still protected. \
             Tap to review weather sources.",
            fmt_rain_amount((om_in() - tempest_in()).max(0.0), prefs.get())
        )
    };

    view! {
        <div class="forecast-block">
            <div class="forecast-block-title">"Rain outlook"</div>
            <div class="rain-today">
                <div class=move || if tempest_used() { "rain-card is-used" } else { "rain-card" }>
                    <div class="rain-card-label">{station_label}</div>
                    <div class="rain-card-value">
                        {move || fmt_rain_amount(tempest_in(), prefs.get())}
                    </div>
                    <div class="rain-card-hint">
                        {move || if sheltered() {
                            view! {
                                <a class="rain-card-hint-link"
                                    href="/settings/sources"
                                    title=sheltered_explain
                                    aria-label=sheltered_explain>
                                    "sheltered · trust forecast "
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
                    <div class="rain-card-label">{forecast_label}</div>
                    <div class="rain-card-value">
                        {move || fmt_rain_amount(om_in(), prefs.get())}
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
                    prefs=prefs.get()
                />
                <RainBar
                    label="Tomorrow"
                    value=Signal::derive(move || snap.get().forecast.rain_tomorrow_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in)
                    threshold_label="Rain-skip threshold"
                    prefs=prefs.get()
                />
                <RainBar
                    label="Next 3 days"
                    value=Signal::derive(move || snap.get().forecast.rain_3day_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in * 3.0)
                    threshold_label="3× skip threshold"
                    prefs=prefs.get()
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
    // value/threshold are INCHES; the bar math stays in inches internally,
    // only the displayed figures convert via the unit prefs.
    prefs: UnitPrefs,
) -> impl IntoView {
    let pct = move || {
        let max = (threshold.get() * 1.2).max(0.01);
        ((value.get() / max).clamp(0.0, 1.0) * 100.0).round()
    };
    let threshold_pct = move || {
        let max = (threshold.get() * 1.2).max(0.01);
        ((threshold.get() / max).clamp(0.0, 1.0) * 100.0).round()
    };
    // Two-tone fill: BLUE (wet, still accumulating) up to where the threshold
    // sits WITHIN the fill, then TEAL (satisfied/saturated) for the portion
    // beyond it. The threshold's position relative to the fill is
    // threshold/value (not threshold/max) so it lands at the actual mark. When
    // value <= threshold the threshold is at/after the fill end, so thr_pct
    // clamps to 100 -> all blue; once value > threshold, thr_pct < 100 and the
    // teal segment appears past the mark.
    let thr_pct = move || {
        let v = value.get();
        if v > 0.0 {
            (threshold.get() / v * 100.0).clamp(0.0, 100.0)
        } else {
            100.0
        }
    };
    view! {
        <div class=move || {
            if value.get() >= threshold.get() { "rain-bar rain-bar-above" } else { "rain-bar" }
        }>
            <div class="rain-bar-head">
                <span class="rain-bar-label">{label}</span>
                <span class="rain-bar-value">{move || fmt_rain_amount(value.get(), prefs)}</span>
            </div>
            <div class="rain-bar-track">
                <div
                    class="rain-bar-fill"
                    style=move || format!("width:{}%; --thr-pct:{}%", pct(), thr_pct())
                ></div>
                <div
                    class="rain-bar-threshold"
                    style=move || format!("left: {}%", threshold_pct())
                    title=threshold_label
                ></div>
            </div>
            <div class="rain-bar-foot">
                {threshold_label} ": " {move || fmt_rain_amount(threshold.get(), prefs)}
            </div>
        </div>
    }
}

/// Multi-day forecast intelligence (Phase A). Shows the four rules
/// the engine added on top of the legacy 1-day check: next-4h hourly
/// rollup, probability-weighted tomorrow, 3-day weighted, 7-day
/// weighted. Each bar fills against its own threshold; bars that
/// have crossed brighten (the "outlook met" state) -- still blue, since
/// crossing means enough rain is expected, which is the water family.
#[component]
fn MultiDayRainBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    view! {
        <div class="forecast-block nerd-only">
            <div class="forecast-block-title">"Forecast intelligence"</div>
            <div class="rain-row">
                <RainBar
                    label="Next 4h (hourly)"
                    value=Signal::derive(move || snap.get().forecast.rain_next_4h_in)
                    threshold=Signal::derive(|| 0.10_f64)
                    threshold_label="Skip-if-≥"
                    prefs=prefs.get()
                />
                <RainBar
                    label="Tomorrow × confidence"
                    value=Signal::derive(move || {
                        let f = snap.get().forecast;
                        f.rain_tomorrow_in * (f.rain_tomorrow_prob_pct as f64) / 100.0
                    })
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in)
                    threshold_label="Rain-skip threshold"
                    prefs=prefs.get()
                />
                <RainBar
                    label="3-day weighted"
                    value=Signal::derive(move || snap.get().forecast.rain_3day_weighted_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in * 1.5)
                    threshold_label="1.5× skip threshold"
                    prefs=prefs.get()
                />
                <RainBar
                    label="7-day weighted"
                    value=Signal::derive(move || snap.get().forecast.rain_7day_weighted_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in * 3.0)
                    threshold_label="3× skip threshold (info)"
                    prefs=prefs.get()
                />
            </div>
            <div class="forecast-block-foot">
                {move || {
                    let f = snap.get().forecast;
                    // rain_tomorrow_in is INCHES; route through the formatter.
                    format!(
                        "Tomorrow: {} at {}% confidence · Days since significant rain: {}",
                        fmt_rain_amount(f.rain_tomorrow_in, prefs.get()), f.rain_tomorrow_prob_pct, f.days_since_significant_rain
                    )
                }}
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
    let prefs = use_unit_prefs();
    // All four figures here are MILLIMETERS.
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
                    <span class="v">{move || fmt_rain_amount_mm(eto_today(), prefs.get())}</span>
                </div>
                <div class="kv">
                    <span class="k">"Rain today"</span>
                    <span class="v">{move || fmt_rain_amount_mm(rain_today_mm(), prefs.get())}</span>
                </div>
                <div class="kv">
                    <span class="k">"Net (rain−ET₀)"</span>
                    <span class=move || {
                        if net() >= 0.0 { "v v-pos" } else { "v v-neg" }
                    }>
                        {move || {
                            let n = net();
                            let p = prefs.get();
                            let sign = if n >= 0.0 { "+" } else { "-" };
                            format!("{sign}{}{}", depth_value_mm(n.abs(), p), depth_unit(p))
                        }}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"ET₀ tomorrow"</span>
                    <span class="v">{move || fmt_rain_amount_mm(eto_tomorrow(), prefs.get())}</span>
                </div>
                <div class="kv">
                    <span class="k">"ET₀ 3-day avg"</span>
                    <span class="v">{move || fmt_rain_amount_mm(eto_3day(), prefs.get())}</span>
                </div>
                <div class="kv">
                    <span class="k">"Heat ET multiplier"</span>
                    <span class=move || {
                        if snap.get().forecast.heat_multiplier > 1.05 { "v v-warn" } else { "v" }
                    }>
                        {move || format!("{:.2}×", snap.get().forecast.heat_multiplier)}
                    </span>
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
    let prefs = use_unit_prefs();
    view! {
        <div class="forecast-block nerd-only">
            <div class="forecast-block-title">"Today"</div>
            <div class="kv-grid">
                <div class="kv">
                    <span class="k">"Temp range"</span>
                    <span class="v">
                        {move || {
                            let f = snap.get().forecast;
                            let p = prefs.get();
                            format!("{} / {}", fmt_temp_short(f.temp_min_today_f, p), fmt_temp_short(f.temp_max_today_f, p))
                        }}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Peak wind"</span>
                    <span class="v">
                        {move || fmt_wind(snap.get().forecast.wind_max_today_mph, prefs.get())}
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
