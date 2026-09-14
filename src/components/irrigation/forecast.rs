// Weather + forecast panel. Measured rain and expected rain remain separate:
// a larger forecast is neither fallen rain nor evidence of a faulty gauge.
// The water balance compares measured rain with the full-day ET budget.

use crate::components::units_fmt::{
    depth_unit, depth_value_in, depth_value_mm, fmt_optional_rain_amount, fmt_optional_wind,
    fmt_rain_amount, fmt_rain_amount_mm, fmt_rain_rate, fmt_temp_short, fmt_wind, temp_unit,
    temp_value, use_unit_prefs, UnitPrefs,
};
use crate::model::{Forecast, IrrigationSnapshot};
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
                {view! { <ModelSoilBlock snap/> }.into_any()}
                {view! { <DayBlock snap/> }.into_any()}
            </div>
        </section>
    }
}

/// Compare measured rain with the full-day ET budget. This is a budget outlook,
/// not a claim that the full day's evaporation has already happened.
#[component]
fn BalanceHeadline(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // Rain source is INCHES; keep both: inches for the display figure
    // (so the unit toggle drives it), mm for the internal bar math.
    let rain_in = move || snap.get().forecast.rain_today_tempest_in;
    let rain_mm = move || crate::units::in_to_mm(rain_in());
    // None when nothing real resolved today's ET0 (forecast outage / cold
    // start): the figures show a dash and the status says the balance is
    // unknown, never a fabricated daily loss presented as measured.
    let et_mm_opt = move || {
        snap.get()
            .forecast
            .eto_today_mm
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    let et_known = move || et_mm_opt().is_some();
    let net = move || rain_budget_balance_mm(&snap.get().forecast);
    let drying = move || net().is_some_and(|n| n < -0.05);
    let rain_pct = move || {
        et_mm_opt().and_then(|et| {
            let total = rain_mm() + et;
            (total > 0.0).then(|| (rain_mm() / total * 100.0).clamp(0.0, 100.0))
        })
    };
    let status = move || {
        let Some(n) = net() else {
            return "Full-day ET budget unknown, balance not computed".to_string();
        };
        if n >= 0.05 {
            "Measured rain exceeds today's ET budget".to_string()
        } else if n >= -0.05 {
            "Measured rain roughly matches today's ET budget".to_string()
        } else {
            "Today's ET budget exceeds measured rain".to_string()
        }
    };
    view! {
        <div class="forecast-block fc-balance">
            <div class="forecast-block-title">"Rain & today's ET budget"</div>
            <div class="balance-figures">
                <div class="balance-fig balance-fig--in">
                    <span class="balance-fig__k">"Measured rain today"</span>
                    <span class="balance-fig__v">
                        {move || format!("+{}", depth_value_in(rain_in(), prefs.get()))}
                        <span class="balance-fig__u">{move || depth_unit(prefs.get())}</span>
                    </span>
                </div>
                <div class="balance-fig balance-fig--out">
                    <span class="balance-fig__k">"Full-day ET budget"</span>
                    <span class="balance-fig__v">
                        {move || match et_mm_opt() {
                            Some(v) => format!("-{}", depth_value_mm(v, prefs.get())),
                            None => "-".to_string(),
                        }}
                        <span class="balance-fig__u">{move || if et_known() { depth_unit(prefs.get()) } else { "" }}</span>
                    </span>
                    // Intraday honesty: the figure above is the FULL day's
                    // budget; this notes how much the model says is already
                    // spent, so a morning reader isn't shown a whole day of
                    // loss charged up front. Hidden when the provider has no
                    // hourly ET curve.
                    {move || {
                        let spent = snap.get().forecast.eto_spent_today_mm;
                        let p = prefs.get();
                        // Unit glued to the number ("1.9mm spent so far"), not in a
                        // trailing span (which laid out as "1.9 spent so far mm").
                        (spent > 0.05).then(|| view! {
                            <span class="balance-fig__note">
                                {format!("{}{} estimated spent so far", depth_value_mm(spent, p), depth_unit(p))}
                            </span>
                        })
                    }}
                </div>
                <div class=move || {
                    if drying() {
                        "balance-fig balance-fig--net is-drying"
                    } else {
                        "balance-fig balance-fig--net is-wet"
                    }
                }>
                    <span class="balance-fig__k">"Rain minus budget"</span>
                    <span class="balance-fig__v">
                        {move || {
                            let Some(n) = net() else {
                                return "-".to_string();
                            };
                            let sign = if n >= 0.0 { "+" } else { "-" };
                            format!("{sign}{}", depth_value_mm(n.abs(), prefs.get()))
                        }}
                        <span class="balance-fig__u">{move || if et_known() { depth_unit(prefs.get()) } else { "" }}</span>
                    </span>
                </div>
            </div>
            {move || rain_pct().map(|pct| view! {
                <div class="balance-bar" role="img" aria-label=status>
                    <div class="balance-bar__rain" style=format!("width: {pct}%")></div>
                    <div class="balance-bar__et"></div>
                </div>
            })}
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
    // Each tile asks the ENGINE whether its gate tripped, by the gate's
    // own id in the decision trace, instead of re-testing the readings
    // against thresholds of its own. Re-testing is how these three drifted
    // in the first place: the wind tile compared against the bare wind
    // limit while the engine's forecast gate adds a slack margin, so the
    // tile went loud on nights the engine never intended to skip, and the
    // heat tile hardcoded three thresholds the operator can change and
    // ignored the switch that disables the gate outright.
    let gate_fired = move |id: &'static str| {
        snap.with(|s| {
            s.decision_trace.as_ref().is_some_and(|t| {
                // "fired" is the only outcome that means the gate tripped.
                // "skipped" is the engine's word for a gate the operator
                // DISABLED, so treating it as tripped would light the tile
                // for a rule that is switched off.
                t.rules.iter().any(|r| r.id == id && r.outcome == "fired")
            })
        })
    };
    let heat_flagged = move || gate_fired("heat_advisory");
    let freeze_flagged = move || gate_fired("freeze_now");
    let wind_flagged = move || gate_fired("wind_forecast") || gate_fired("wind_now");
    view! {
        <div class="forecast-block fc-stress">
            <div class="forecast-block-title">"Conditions & stress"</div>
            <div class="stress-tiles">
                {view! {
                    <StressTile
                        label="Heat index"
                        value=Signal::derive(move || forecast_temperature(snap.get().forecast.heat_index_now_f, prefs.get()))
                        sub=Signal::derive(move || format!("peak {}", forecast_temperature(snap.get().forecast.heat_index_max_3day_f, prefs.get())))
                        flagged=Signal::derive(heat_flagged)
                        flag_note="heat advisory"
                    />
                }.into_any()}
                {view! {
                    <StressTile
                        label="Overnight low"
                        value=Signal::derive(move || forecast_temperature(snap.get().forecast.temp_min_24h_f, prefs.get()))
                        sub=Signal::derive(move || format!("freeze < {}{}", temp_value(snap.get().skip_check.min_temp_f, prefs.get()), temp_unit(prefs.get())))
                        flagged=Signal::derive(freeze_flagged)
                        flag_note="freeze risk"
                    />
                }.into_any()}
                {view! {
                    <StressTile
                        label="Peak wind"
                        value=Signal::derive(move || fmt_optional_wind(snap.get().forecast.wind_max_today_mph, prefs.get()))
                        sub=Signal::derive(move || format!("skip > {}", fmt_wind(snap.get().skip_check.max_wind_mph, prefs.get())))
                        flagged=Signal::derive(wind_flagged)
                        flag_note="too windy"
                    />
                }.into_any()}
                // Transpiration stress (VPD): model data, advisory only.
                // Sustained VPD above ~1.6 kPa means plants lose water
                // faster than the standard Kc assumes; the tile flags the
                // condition without ever changing a skip decision. Hidden
                // entirely when the provider sends no VPD series.
                {move || {
                    let f = snap.get().forecast;
                    (f.vpd_now_kpa > 0.0 || f.vpd_max_today_kpa > 0.0).then(|| view! {
                        <StressTile
                            label="Transpiration (VPD)"
                            value=Signal::derive(move || format!("{:.2} kPa", snap.get().forecast.vpd_now_kpa))
                            sub=Signal::derive(move || format!("peak {:.2} kPa today", snap.get().forecast.vpd_max_today_kpa))
                            flagged=Signal::derive(move || snap.get().forecast.vpd_max_today_kpa >= 1.6)
                            flag_note="high water demand"
                        />
                    }.into_any())
                }}
            </div>
        </div>
    }
}

/// Forecast presence comes from the snapshot. The display must not substitute
/// current weather for a missing forecast extreme or attach units to unknown.
fn forecast_temperature(value: Option<f64>, prefs: UnitPrefs) -> String {
    value.filter(|v| v.is_finite()).map_or_else(
        || "unknown".to_string(),
        |v| format!("{}{}", temp_value(v, prefs), temp_unit(prefs)),
    )
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
        f.rain_is_live
            && (f
                .rain_intensity_in_hr
                .is_some_and(|rate| rate.is_finite() && rate > 0.001)
                || matches!(f.rain_type.as_str(), "rain" | "hail" | "snow" | "sleet"))
    };
    let label = move || {
        let f = snap.get().forecast;
        if let Some(rate) = f
            .rain_intensity_in_hr
            .filter(|rate| rate.is_finite() && *rate > 0.001)
        {
            // rain_intensity is IN/HR; route through the rate formatter.
            format!("RAINING NOW · {}", fmt_rain_rate(rate, prefs.get()))
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

/// Observed and forecast rain have different time scopes and cannot be ranked
/// as competing readings. Keep their provenance explicit beside each amount.
#[component]
fn RainBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let tempest_in = move || snap.get().forecast.rain_today_tempest_in;
    let om_in = move || snap.get().forecast.rain_today_om_in;
    // Real source provenance for the two cards (fallbacks keep the UI sane
    // before the engine has populated them).
    let station_label = move || {
        let l = snap.get().forecast.station_source_label;
        if l.is_empty() {
            "Measured rain".to_string()
        } else {
            l
        }
    };
    let forecast_label = move || {
        let l = snap.get().forecast.forecast_source_label;
        if l.is_empty() {
            "Forecast rain".to_string()
        } else {
            l
        }
    };

    view! {
        <div class="forecast-block">
            <div class="forecast-block-title">"Rain outlook"</div>
            <div class="rain-today">
                <div class="rain-card">
                    <div class="rain-card-label">{station_label}</div>
                    <div class="rain-card-value">
                        {move || fmt_rain_amount(tempest_in(), prefs.get())}
                    </div>
                    <div class="rain-card-hint">"Measured today"</div>
                </div>
                <div class="rain-card">
                    <div class="rain-card-label">{forecast_label}</div>
                    <div class="rain-card-value">
                        {move || fmt_optional_rain_amount(om_in(), prefs.get())}
                    </div>
                    <div class="rain-card-hint">"Expected today (full day)"</div>
                </div>
            </div>
            <a class="rain-block-config-link" href="/settings?section=devices">
                "Configure rain sources \u{2192}"
            </a>
            <div class="rain-row">
                <RainBar
                    label="Measured today"
                    value=Signal::derive(move || Some(tempest_in()))
                    threshold=Signal::derive(move || snap.get().skip_check.already_wet_in)
                    threshold_label="Already-wet floor"
                    prefs=prefs.get()
                />
                <RainBar
                    label="Tomorrow"
                    value=Signal::derive(move || snap.get().forecast.rain_tomorrow_in)
                    threshold=Signal::derive(move || snap.get().skip_check.rain_skip_in)
                    threshold_label="Rain-skip threshold"
                    prefs=prefs.get()
                    // Recency carry: rain measured TODAY still counts toward
                    // tomorrow's observed-rain skip (same carry the verdict
                    // strip and morning check apply), so the bar shows the
                    // decision's real input instead of a bare forecast 0
                    // that contradicts a "skipping on recent rain" cell.
                    carry=Signal::derive(move || snap.get().forecast.rain_today_tempest_in)
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

/// Shared displayed budget difference. Expected rain is not an observation
/// and cannot increase today's measured credit. This is not a gate decision.
fn rain_budget_balance_mm(forecast: &Forecast) -> Option<f64> {
    forecast
        .eto_today_mm
        .filter(|et| et.is_finite() && *et >= 0.0)
        .map(|et| crate::units::in_to_mm(forecast.rain_today_tempest_in) - et)
}

/// Percentages for a known forecast plus its measured carry. Missing amount
/// means no bar geometry, even when a measured carry is available separately.
fn rain_bar_geometry(value: Option<f64>, carry: f64, threshold: f64) -> Option<(f64, f64, f64)> {
    let amount = value.filter(|v| v.is_finite() && *v >= 0.0)?;
    let total = amount + carry;
    let max = (threshold * 1.2).max(0.01);
    let pct = ((total / max).clamp(0.0, 1.0) * 100.0).round();
    let denom = total.min(max);
    let threshold_share = if denom > 0.0 {
        (threshold / denom * 100.0).clamp(0.0, 100.0)
    } else {
        100.0
    };
    let carry_share = if total > 0.0 {
        (carry / total * 100.0).clamp(0.0, 100.0).round()
    } else {
        0.0
    };
    Some((pct, threshold_share, carry_share))
}

#[component]
fn RainBar(
    label: &'static str,
    value: Signal<Option<f64>>,
    threshold: Signal<f64>,
    threshold_label: &'static str,
    prefs: UnitPrefs,
    /// Measured rain that this day's decision also counts. The absence of a
    /// carry prop means this bar has no carry; it is not a missing forecast.
    #[prop(optional, into)]
    carry: Option<Signal<f64>>,
) -> impl IntoView {
    let carry_in = move || carry.map(|c| c.get()).unwrap_or(0.0).max(0.0);
    let total = move || {
        value
            .get()
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v + carry_in())
    };
    let geometry = move || rain_bar_geometry(value.get(), carry_in(), threshold.get());
    let threshold_pct = move || {
        let max = (threshold.get() * 1.2).max(0.01);
        ((threshold.get() / max).clamp(0.0, 1.0) * 100.0).round()
    };
    let has_carry = move || carry_in() > 0.005;
    let carry_title = move || {
        format!(
            "{} already fell (measured) and still counts toward this day's skip decision",
            fmt_rain_amount(carry_in(), prefs)
        )
    };
    view! {
        <div class=move || {
            if total().is_some_and(|v| v >= threshold.get()) { "rain-bar rain-bar-above" } else { "rain-bar" }
        }>
            <div class="rain-bar-head">
                <span class="rain-bar-label">{label}</span>
                <span class="rain-bar-value">
                    {move || fmt_optional_rain_amount(value.get(), prefs)}
                    {move || has_carry().then(|| view! {
                        <span class="rain-bar-carry-note" title=carry_title>
                            {format!(" +{} carried", fmt_rain_amount(carry_in(), prefs))}
                        </span>
                    })}
                </span>
            </div>
            <div class="rain-bar-track" role="img" aria-label=move || {
                if total().is_some() { format!("{label} rain amount") } else { format!("{label} rain amount unknown") }
            }>
                {move || geometry().map(|(pct, threshold_share, carry_share)| view! {
                    <div class="rain-bar-fill" style=format!(
                        "width:{pct}%; --thr-pct:{threshold_share}%; --carry-pct:{carry_share}%"
                    )></div>
                })}
                <div class="rain-bar-threshold"
                    style=move || format!("left: {}%", threshold_pct())
                    title=threshold_label></div>
            </div>
            <div class="rain-bar-foot">
                {threshold_label} ": " {move || fmt_rain_amount(threshold.get(), prefs)}
                {move || total().filter(|_| has_carry()).map(|known| view! {
                    <span class="rain-bar-foot-carry" title=carry_title>
                        {format!(" · counts {} total with today's rain", fmt_rain_amount(known, prefs))}
                    </span>
                })}
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
                    threshold=Signal::derive(move || snap.get().skip_check.rain_next_4h_skip_in)
                    threshold_label="Skip-if-≥"
                    prefs=prefs.get()
                />
                <RainBar
                    label="Tomorrow × confidence"
                    value=Signal::derive(move || {
                        let f = snap.get().forecast;
                        // Full weight when no probability was reported,
                        // matching the engine's tomorrow_prob_weight.
                        let weight = f
                            .rain_tomorrow_prob_pct
                            .map(|prob| prob as f64 / 100.0)
                            .unwrap_or(1.0);
                        f.rain_tomorrow_in.map(|rain| rain * weight)
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
                    // No probability reported: no confidence claim.
                    let confidence = match f.rain_tomorrow_prob_pct {
                        Some(prob) => format!(" at {prob}% confidence"),
                        None => String::new(),
                    };
                    format!(
                        "Tomorrow: {}{confidence} · Days since significant rain: {}",
                        fmt_optional_rain_amount(f.rain_tomorrow_in, prefs.get()), f.days_since_significant_rain
                    )
                }}
            </div>
        </div>
    }
}

/// Full-day ET₀ budget versus rain measured so far, with forecast ET context.
/// The displayed difference is not an already-completed daily bucket update.
#[component]
fn BalanceBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // All four figures here are MILLIMETERS. ET0 today is None when nothing
    // real resolved it; the tile and the Net row render a dash for that.
    let eto_today = move || snap.get().forecast.eto_today_mm;
    let eto_tomorrow = move || snap.get().forecast.eto_tomorrow_mm;
    let eto_3day = move || snap.get().forecast.eto_3day_avg_mm;
    let rain_today_mm = move || crate::units::in_to_mm(snap.get().forecast.rain_today_tempest_in);
    let net = move || rain_budget_balance_mm(&snap.get().forecast);
    view! {
        <div class="forecast-block nerd-only">
            <div class="forecast-block-title">"Water budget"</div>
            <div class="kv-grid">
                <div class="kv">
                    <span class="k">"Full-day ET₀ budget"</span>
                    <span class="v">
                        {move || match eto_today() {
                            Some(v) => fmt_rain_amount_mm(v, prefs.get()),
                            None => "-".to_string(),
                        }}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Measured rain today"</span>
                    <span class="v">{move || fmt_rain_amount_mm(rain_today_mm(), prefs.get())}</span>
                </div>
                <div class="kv">
                    <span class="k">"Rain minus ET₀ budget"</span>
                    <span class=move || {
                        match net() {
                            Some(n) if n < 0.0 => "v v-neg",
                            Some(_) => "v v-pos",
                            None => "v",
                        }
                    }>
                        {move || {
                            let p = prefs.get();
                            match net() {
                                Some(n) => {
                                    let sign = if n >= 0.0 { "+" } else { "-" };
                                    format!("{sign}{}{}", depth_value_mm(n.abs(), p), depth_unit(p))
                                }
                                None => "-".to_string(),
                            }
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
                    <span class="k">"Additional ET adjustment"</span>
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

/// Model soil (advisory, nerd mode): the forecast model's own root-zone
/// soil state. Model data, NEVER a probe substitute: zones with real
/// probes keep their measured readings everywhere decisions are made.
/// What this adds is the model's forward view, the 48h dry-down trend a
/// probe can't see. Hidden entirely when the provider has no soil series.
#[component]
fn ModelSoilBlock(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let has_data = move || snap.get().forecast.soil_moisture_3_9_now_vwc > 0.0;
    let now_pct = move || snap.get().forecast.soil_moisture_3_9_now_vwc * 100.0;
    let later_pct = move || snap.get().forecast.soil_moisture_3_9_in48h_vwc * 100.0;
    let trend = move || {
        let delta = later_pct() - now_pct();
        if delta <= -1.0 {
            format!("drying to {:.0}% in 48h", later_pct())
        } else if delta >= 1.0 {
            format!("wetting to {:.0}% in 48h", later_pct())
        } else {
            "steady over 48h".to_string()
        }
    };
    move || {
        has_data().then(|| view! {
            <div class="forecast-block nerd-only">
                <div class="forecast-block-title">"Model soil (advisory)"</div>
                <div class="kv-grid">
                    <div class="kv">
                        <span class="k">"Root zone moisture (3-9cm)"</span>
                        <span class="v">{move || format!("{:.0}%", now_pct())}</span>
                    </div>
                    <div class="kv">
                        <span class="k">"48h trend"</span>
                        <span class=move || {
                            if later_pct() < now_pct() - 1.0 { "v v-warn" } else { "v" }
                        }>{trend}</span>
                    </div>
                    <div class="kv">
                        <span class="k">"Soil temp (6cm)"</span>
                        <span class="v">
                            {move || fmt_temp_short(snap.get().forecast.soil_temp_6cm_now_f, prefs.get())}
                        </span>
                    </div>
                </div>
                <div class="forecast-block-foot">
                    "Weather-model estimate for context; zone probes stay authoritative."
                </div>
            </div>
        })
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
                            // None = no forecast temps resolved: dash, never
                            // a fabricated 0/0 range.
                            match (f.temp_min_today_f, f.temp_max_today_f) {
                                (Some(lo), Some(hi)) => {
                                    format!("{} / {}", fmt_temp_short(lo, p), fmt_temp_short(hi, p))
                                }
                                _ => "-".to_string(),
                            }
                        }}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Peak wind"</span>
                    <span class="v">
                        {move || fmt_optional_wind(snap.get().forecast.wind_max_today_mph, prefs.get())}
                    </span>
                </div>
                <div class="kv">
                    <span class="k">"Mean humidity"</span>
                    <span class="v">
                        {move || match snap.get().forecast.humidity_mean_today_pct {
                            Some(h) => format!("{h:.0}%"),
                            None => "-".to_string(),
                        }}
                    </span>
                </div>
            </div>
        </div>
    }
}

#[cfg(all(test, feature = "ssr"))]
mod stress_flag_tests {
    #[test]
    fn unknown_rain_has_no_bar_or_invented_total_even_with_measured_carry() {
        assert_eq!(super::rain_bar_geometry(None, 0.3, 0.5), None);
        assert_eq!(super::rain_bar_geometry(Some(f64::NAN), 0.3, 0.5), None);
        assert_eq!(
            super::rain_bar_geometry(Some(0.0), 0.0, 0.5),
            Some((0.0, 100.0, 0.0))
        );
        assert_eq!(
            super::rain_bar_geometry(Some(0.0), 0.3, 0.5),
            Some((50.0, 100.0, 100.0))
        );
    }

    #[test]
    fn expected_rain_never_increases_the_displayed_measured_budget_credit() {
        let mut forecast = crate::model::Forecast {
            rain_today_tempest_in: 0.0,
            rain_today_om_in: Some(2.0),
            eto_today_mm: Some(4.0),
            ..Default::default()
        };
        assert_eq!(super::rain_budget_balance_mm(&forecast), Some(-4.0));
        forecast.rain_today_om_in = Some(0.0);
        assert_eq!(super::rain_budget_balance_mm(&forecast), Some(-4.0));
        forecast.rain_today_om_in = None;
        assert_eq!(super::rain_budget_balance_mm(&forecast), Some(-4.0));

        forecast.rain_today_tempest_in = 0.5;
        assert!((super::rain_budget_balance_mm(&forecast).unwrap() - 8.7).abs() < 1e-9);
        forecast.eto_today_mm = None;
        assert_eq!(super::rain_budget_balance_mm(&forecast), None);
    }

    #[test]
    fn forecast_temperature_presence_is_visible_without_inventing_a_reading() {
        use crate::components::units_fmt::{IMPERIAL, METRIC};
        assert_eq!(super::forecast_temperature(None, IMPERIAL), "unknown");
        assert_eq!(
            super::forecast_temperature(Some(f64::NAN), METRIC),
            "unknown"
        );
        assert_eq!(super::forecast_temperature(Some(0.0), IMPERIAL), "0°F");
        assert_eq!(super::forecast_temperature(Some(32.0), METRIC), "0°C");
    }

    /// The three stress tiles read the engine's own gate outcomes by id,
    /// so those ids have to be ids the engine actually emits. They used to
    /// re-test the readings against thresholds of their own and drifted:
    /// the wind tile ignored the forecast gate's slack margin and the heat
    /// tile hardcoded three operator-tunable numbers. If a gate is ever
    /// renamed, this fails instead of the tile going quiet forever.
    #[test]
    fn the_tiles_name_gates_the_engine_emits() {
        let catalog = crate::gates_catalog::builtin_rule_catalog();
        for id in ["heat_advisory", "freeze_now", "wind_forecast", "wind_now"] {
            assert!(
                catalog.iter().any(|(gate_id, _, _, _)| *gate_id == id),
                "stress tile reads gate {id}, which is no longer produced"
            );
        }
    }
}
