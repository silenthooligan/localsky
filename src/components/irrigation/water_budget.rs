// Phase H — Weekly water-budget panel. Per-zone tile that shows the
// allocated weekly budget, expected rain credit, computed session
// depth + duration, and today's recommendation (run N min, or skip
// with a reason). The HA budget-override automation at 23:30:25 calls
// IU.adjust_time(actual=today_seconds) for zones where mode_active=true.
//
// All math is recomputed every snapshot tick (10s) so the dashboard
// reflects current rain forecast + last-run epoch instantly.

use crate::components::ui::HelpHint;
use crate::ha::snapshot::{IrrigationSnapshot, WaterBudget};
use chrono::{Local, TimeZone, Utc};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn WaterBudgetPanel(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let zone0 = Signal::derive(move || budget_for(&snap.get(), 0));
    let zone1 = Signal::derive(move || budget_for(&snap.get(), 1));
    let zone2 = Signal::derive(move || budget_for(&snap.get(), 2));
    let zone3 = Signal::derive(move || budget_for(&snap.get(), 3));

    view! {
        <section class="water-budget">
            <h2 class="water-budget-title">
                "Weekly water budget"
                <HelpHint topic="water-budget"/>
            </h2>
            <p class="water-budget-sub">
                "Deep + infrequent. Allocates the weekly water target across "
                "N sessions, subtracts forecast rain, and defers when rain is "
                "incoming or the zone ran recently."
            </p>
            <div class="water-budget-grid">
                {view! { <WaterBudgetTile zone=zone0/> }.into_any()}
                {view! { <WaterBudgetTile zone=zone1/> }.into_any()}
                {view! { <WaterBudgetTile zone=zone2/> }.into_any()}
                {view! { <WaterBudgetTile zone=zone3/> }.into_any()}
            </div>
        </section>
    }
}

fn budget_for(snap: &IrrigationSnapshot, idx: usize) -> WaterBudget {
    snap.water_budgets.get(idx).cloned().unwrap_or_default()
}

#[component]
fn WaterBudgetTile(zone: Signal<WaterBudget>) -> impl IntoView {
    let tile_class = move || {
        let z = zone.get();
        match (z.mode_active, z.today_seconds > 0) {
            (false, _) => "water-budget-tile water-budget-tile-off",
            (true, true) => "water-budget-tile water-budget-tile-run",
            (true, false) => "water-budget-tile water-budget-tile-skip",
        }
    };

    let badge_text = move || {
        let z = zone.get();
        match (z.mode_active, z.today_seconds > 0) {
            (false, _) => "MODE OFF",
            (true, true) => "RUN TODAY",
            (true, false) => "HOLD",
        }
    };

    let today_value = move || {
        let z = zone.get();
        if z.today_seconds > 0 {
            format!("{} min", ((z.today_seconds as f64) / 60.0).round() as u32)
        } else {
            "\u{2014}".to_string()
        }
    };

    let reason = move || zone.get().today_reason;
    let last_run = move || {
        let e = zone.get().last_run_epoch;
        if e == 0 {
            "no history".to_string()
        } else {
            let when = Utc.timestamp_opt(e, 0).single();
            match when {
                Some(dt) => {
                    let local = dt.with_timezone(&Local);
                    let age = Utc::now().signed_duration_since(dt);
                    let stamp = if age.num_days() < 7 {
                        local.format("%a %-I:%M %p").to_string()
                    } else {
                        local.format("%b %-d").to_string()
                    };
                    format!("{stamp}  ({}d ago)", age.num_days())
                }
                None => "—".to_string(),
            }
        }
    };

    let budget_summary = move || {
        let z = zone.get();
        format!(
            "{:.2}\" / week  ÷ {} session(s)  → {:.1} mm depth each",
            z.weekly_budget_in, z.sessions_per_week, z.mm_per_session
        )
    };

    let rain_credit = move || {
        let z = zone.get();
        format!(
            "rain credit: {:.1} mm (forecast 7d)  →  net need: {:.1} mm",
            z.expected_rain_mm, z.needed_mm
        )
    };

    let session_duration = move || {
        let z = zone.get();
        let m = (z.seconds_per_session as f64) / 60.0;
        if z.session_capped {
            format!(
                "session run-time: {:.0} min, capped (would need longer to deliver full depth)",
                m
            )
        } else {
            format!("session run-time: {:.0} min", m)
        }
    };

    view! {
        <article class=tile_class>
            <header class="water-budget-head">
                <h3 class="water-budget-name">{move || zone.get().zone_name}</h3>
                <span class="water-budget-badge">{badge_text}</span>
            </header>
            <div class="water-budget-today">
                <span class="water-budget-today-label">"today"</span>
                <span class="water-budget-today-value">{today_value}</span>
            </div>
            <p class="water-budget-reason">{reason}</p>
            <dl class="water-budget-rows">
                <div class="water-budget-row"><dt>"budget"</dt><dd>{budget_summary}</dd></div>
                <div class="water-budget-row"><dt>"rain"</dt><dd>{rain_credit}</dd></div>
                <div class="water-budget-row"><dt>"session"</dt><dd>{session_duration}</dd></div>
                <div class="water-budget-row"><dt>"last run"</dt><dd>{last_run}</dd></div>
            </dl>
        </article>
    }
}
