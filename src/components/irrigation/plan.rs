//! Read-only, progressively disclosed views of the engine's daily water plan.
//! Stable day/zone keys preserve open disclosures through snapshot refreshes.
use super::daily::{morning, MorningHistory};
use super::overview::{projected, short_reasons};
use crate::components::ui::Icon;
use crate::components::units_fmt::{fmt_rain_amount_mm, use_unit_prefs};
use crate::model::{IrrigationSnapshot, WaterPlanDay, WaterPlanZone};
use crate::timefmt::{format_hm, format_md, format_wday_full};
use leptos::prelude::*;

fn zone_reason(zone: &WaterPlanZone) -> String {
    if zone.planned_seconds > 0 {
        if zone.session_capped {
            "Water needed · limited by the run cap"
        } else {
            "Water needed to replenish the root zone"
        }
        .into()
    } else {
        short_reasons(std::iter::once((
            zone.reason_code.as_str(),
            zone.reason.as_str(),
        )))
    }
}

fn duration(seconds: u32) -> String {
    if seconds == 0 {
        "Not watering".into()
    } else if seconds < 60 {
        "Water · <1 min".into()
    } else {
        format!("Water · {} min", (seconds + 30) / 60)
    }
}

fn zone_tone(zone: &WaterPlanZone) -> &'static str {
    if zone.planned_seconds > 0 {
        "run"
    } else if crate::gates_catalog::GateFamily::of(&zone.reason_code, &zone.reason)
        == crate::gates_catalog::GateFamily::NoData
    {
        "off"
    } else {
        "skip"
    }
}

#[component]
fn PlanZone(zone: Memo<WaterPlanZone>) -> impl IntoView {
    let prefs = use_unit_prefs();
    view! {
        <article class="plan-zone" data-watering=move || zone_tone(&zone.get())>
            <div class="plan-zone__heading">
                <h3>{move || zone.get().name}</h3>
                <span class="watering-state">{move || view! { <Icon name=match zone_tone(&zone.get()) { "run" => "sprinkler", "off" => "alert-triangle", _ => "sprinkler-off" } size=20/> }}{move || if zone_tone(&zone.get()) == "off" { "Awaiting data".into() } else { duration(zone.get().planned_seconds) }}</span>
            </div>
            <p class="plan-zone__reason">{move || zone_reason(&zone.get())}</p>
            <details class="plan-zone__detail">
                <summary><span>"Water balance & evidence"</span><Icon name="chevron-down" size=18/></summary>
                <div class="plan-zone__evidence">
                    <dl>
                        <div><dt>"Water used from soil"</dt><dd>{move || zone.get().depletion_range_mm.map(|(lo, hi)| format!("{}–{}", fmt_rain_amount_mm(lo, prefs.get()), fmt_rain_amount_mm(hi, prefs.get()))).unwrap_or_else(|| "Unknown".into())}</dd></div>
                        <div><dt>"Watering trigger"</dt><dd>{move || zone.get().trigger_mm.map(|v| fmt_rain_amount_mm(v, prefs.get())).unwrap_or_else(|| "Unknown".into())}</dd></div>
                        <div><dt>"Plant water use · day"</dt><dd>{move || fmt_rain_amount_mm(zone.get().demand_mm, prefs.get())}</dd></div>
                    </dl>
                    <p>{move || zone.get().reason}</p>
                </div>
            </details>
        </article>
    }
}

#[component]
fn PlanDay(snap: ReadSignal<IrrigationSnapshot>, initial: WaterPlanDay) -> impl IntoView {
    let date = initial.date_local.clone();
    let fallback = initial.clone();
    let day = Memo::new(move |_| {
        snap.get()
            .water_plan
            .into_iter()
            .find(|d| d.date_local == date)
            .unwrap_or_else(|| fallback.clone())
    });
    let outlook = Memo::new(move |_| projected(Some(&day.get()), &snap.get().timezone));
    let prefs = use_unit_prefs();
    let tz = snap.get_untracked().timezone;
    let name = if initial.day_offset == 1 {
        "Tomorrow".into()
    } else {
        format_wday_full(initial.time_epoch, &tz)
    };
    view! {
        <details class="water-plan__day" open={initial.day_offset == 1} data-watering=move || outlook.get().tone>
            <summary>
                <span class="water-plan__day-icon">{move || view! { <Icon name=outlook.get().icon size=28/> }}</span>
                <span class="water-plan__date"><strong>{name}</strong><span>{format_md(initial.time_epoch, &tz)}</span></span>
                <span class="water-plan__day-status"><strong>{move || outlook.get().headline}</strong><span>{move || day.get().forecast_rain_mm.map(|r| format!("{} rain forecasted", fmt_rain_amount_mm(r, prefs.get()))).unwrap_or_else(|| "Rain forecast unavailable".into())}</span></span>
                <span class="water-plan__toggle"><span class="disclosure-closed">"View zones"</span><span class="disclosure-open">"Hide zones"</span><Icon name="chevron-down" size=20/></span>
            </summary>
            <div class="water-plan__day-body">
                {move || day.get().start_epoch.zip(day.get().finish_epoch).map(|(start, end)| view! {
                    <p class="water-plan__timing">{format!("Projected window {}–{} · includes cycle and soak time", format_hm(start, &snap.get().timezone), format_hm(end, &snap.get().timezone))}</p>
                })}
                <div class="water-plan__zones">
                    <For each=move || day.get().zones key=|z| z.zone.clone() children=move |initial_zone| {
                        let zone = Memo::new(move |_| day.get().zones.into_iter().find(|z| z.zone == initial_zone.zone).unwrap_or_else(|| initial_zone.clone()));
                        view! { <PlanZone zone/> }
                    }/>
                </div>
                {move || (!day.get().evidence_complete).then(|| view! { <p class="water-plan__note">"Forecast inputs are incomplete. Missing rain is not counted."</p> })}
            </div>
        </details>
    }
}

#[component]
pub fn WateringPlan(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let history = expect_context::<MorningHistory>().0;
    let prefs = use_unit_prefs();
    view! {
        <div class="water-plan">
            <section class="water-plan__overview">
                <header class="water-plan__heading">
                    <p class="eyebrow">"WATERING OUTLOOK"</p>
                    <h2>{move || {
                        let s = snap.get();
                        match s.water_plan.iter().find(|d| d.day_offset == 1) {
                            Some(day) if day.zones.is_empty() => "Awaiting tomorrow’s zone plan".into(),
                            Some(day) if day.zones.iter().all(|z| z.planned_seconds == 0) => crate::voice::idle::NO_WATER_PLANNED.to_string(),
                            Some(day) => format!("Tomorrow: {:.0} min projected", day.zones.iter().map(|z| f64::from(z.planned_seconds)).sum::<f64>() / 60.0),
                            None => "Building the watering outlook".to_string(),
                        }
                    }}</h2>
                    <p>"Recent rain, watering and plant demand shape the days ahead."</p>
                </header>
                <div class="water-plan__evidence">
                    <div><span>"Recent rain · 7 days"</span><strong>{move || snap.get().water_budgets.first().filter(|b| b.observed_rain_source != "none").map(|b| fmt_rain_amount_mm(b.observed_rain_mm, prefs.get())).unwrap_or_else(|| "Unknown".into())}</strong><small>{move || snap.get().water_budgets.first().map(|b| match b.observed_rain_source.as_str() { "gauge" => "Measured at the station", "radar" => "Radar estimate", "model_archive" => "Historical model estimate", _ => "No rain evidence" }).unwrap_or("No rain evidence")}</small></div>
                    <div><span>"Rain ahead"</span><strong>{move || {
                        let days: Vec<_> = snap.get().water_plan.into_iter().filter(|d| d.day_offset > 0).collect();
                        days.iter().map(|d| d.forecast_rain_mm).collect::<Option<Vec<_>>>().filter(|_| !days.is_empty()).map(|v| fmt_rain_amount_mm(v.iter().sum(), prefs.get())).unwrap_or_else(|| "Unknown".into())
                    }}</strong><small>{move || format!("Next {} days · forecast", snap.get().water_plan.iter().filter(|d| d.day_offset > 0).count())}</small></div>
                    <div><span>"Plant water use ahead"</span><strong>{move || {
                        let s = snap.get();
                        let sums: Vec<f64> = s.zones.iter().map(|zone| s.water_plan.iter().filter(|d| d.day_offset > 0).flat_map(|d| &d.zones).filter(|z| z.zone == zone.slug).map(|z| z.demand_mm).sum()).collect();
                        if sums.is_empty() || s.water_plan.is_empty() { return "Unknown".into(); }
                        format!("{}–{}", fmt_rain_amount_mm(sums.iter().copied().fold(f64::INFINITY, f64::min), prefs.get()), fmt_rain_amount_mm(sums.iter().copied().fold(0.0, f64::max), prefs.get()))
                    }}</strong><small>"Across the outlook · varies by planting"</small></div>
                </div>
                <div class="water-plan__today">
                    <span class="water-plan__day-label">"TODAY’S AUTOMATIC MORNING"</span>
                    {move || {
                        let s = snap.get();
                        match history.get() {
                            Some(Ok(h)) => {
                                let summary = morning(&h, s.last_refresh_epoch, &s.timezone);
                                let reason = short_reasons(summary.reasons.iter().map(|r| ("", r.as_str())));
                                view! { <strong>{summary.headline}</strong><p>{reason}</p> }.into_any()
                            },
                            Some(Err(message)) => view! { <p role="alert">{message}</p> }.into_any(),
                            None => view! { <p>"Loading the recorded morning…"</p> }.into_any(),
                        }
                    }}
                    <a href=crate::base::url("/history?view=daily")>"Daily log and run details →"</a>
                </div>
            </section>
            <section class="water-plan__forecast">
                <header class="decision-section-heading"><h2>"Days ahead"</h2><p>"Open a day for each zone’s plan. Forecasts can change."</p></header>
                <div class="water-plan__days">
                    <For each={move || snap.get().water_plan.into_iter().filter(|d| d.day_offset > 0).collect::<Vec<_>>()} key=|d| d.date_local.clone() children={move |initial| view! { <PlanDay snap initial/> }}/>
                </div>
            </section>
            <details class="water-plan__method">
                <summary><Icon name="rules" size=22/><span>"How timing and water need are calculated"</span><Icon name="chevron-down" size=20/></summary>
                <div>
                    <p>"Runs finish 15 minutes before sunrise. Start time subtracts watering, cycles, soak waits and transitions. A zero-minute plan has no watering start."</p>
                    <p>"Rain and irrigation refill the root zone; plant water use draws it down. Soil, roots, planting, season and local weather set its capacity and watering trigger."</p>
                    <p>"Watering waits while the soil can last until rain. A smaller run can bridge a dry gap. Expected rain accounts for probability, local forecast bias and soil storage. It is never measured rain."</p>
                    <p>"Live conditions refresh every 10 seconds. Restrictions, sensor readings and safety holds determine the final run."</p>
                </div>
            </details>
        </div>
    }
}
