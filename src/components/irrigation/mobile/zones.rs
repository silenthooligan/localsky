// Mobile "Zones" tab — vertical list of zones, one row each. Tap a row to
// drill into MobileZoneDetail at /irrigation/zone/:slug. Each row also has
// an inline "10m" quick-run chip on the right for the fastest-possible path
// to start a zone (one tap, no detail view).

use crate::components::irrigation::controls::post_action;
use crate::ha::snapshot::{IrrigationSnapshot, ZoneState};
use crate::nav_log::log_nav;
use leptos::prelude::*;
use leptos_router::hooks::use_navigate;
use leptos_router::NavigateOptions;
use serde_json::json;

#[component]
pub fn MobileZones(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let zones = move || snap.get().zones.clone();

    view! {
        <div class="mobile-stack">
            <h2 class="mobile-section-title">"Zones"</h2>
            <div class="mobile-zone-list">
                {move || zones().into_iter().map(|z| view! {
                    <MobileZoneRow zone=z/>
                }.into_any()).collect::<Vec<_>>()}
            </div>
        </div>
    }
}

#[component]
fn MobileZoneRow(zone: ZoneState) -> impl IntoView {
    let slug_for_nav = zone.slug.clone();
    let slug_for_run = zone.slug.clone();
    let slug_for_stop = zone.slug.clone();
    let zone_running = zone.running;
    let zone_planned_min = (zone.planned_run_seconds + 30) / 60;
    let zone_today_min = zone.today_run_minutes;
    let bucket_mm = zone.bucket_mm;
    let zone_name = zone.name.clone();

    let navigate = use_navigate();
    let on_row = move |ev: leptos::ev::MouseEvent| {
        // Don't navigate if the user tapped the inline action button — those
        // call stopPropagation themselves but be defensive.
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 { return; }
        ev.prevent_default();
        log_nav(format!("zone-row tap: {}", &slug_for_nav));
        navigate(
            &format!("/irrigation/zone/{}", &slug_for_nav),
            NavigateOptions::default(),
        );
    };

    let on_quick_run = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        ev.prevent_default();
        let slug = slug_for_run.clone();
        post_action(json!({"kind": "run", "zone": slug, "seconds": 600}));
    };

    let on_stop = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        ev.prevent_default();
        let slug = slug_for_stop.clone();
        post_action(json!({"kind": "stop", "zone": slug}));
    };

    let badge_class = if zone_running { "zone-row-badge zone-row-badge-running" } else { "zone-row-badge" };
    let badge_text = if zone_running { "RUNNING" } else { "IDLE" };

    view! {
        <a class="mobile-zone-row" href=format!("/irrigation/zone/{}", &zone.slug) on:click=on_row>
            <div class="mobile-zone-row-main">
                <div class="mobile-zone-row-name">{zone_name}</div>
                <div class="mobile-zone-row-meta">
                    <span class=badge_class>{badge_text}</span>
                    <span class="zone-row-stat">{zone_planned_min}" min planned"</span>
                    {move || if zone_today_min > 0.0 {
                        view! { <span class="zone-row-stat">{format!("{:.0} min today", zone_today_min)}</span> }.into_any()
                    } else if bucket_mm > 0.0 {
                        view! { <span class="zone-row-stat">{format!("{:.1} mm deficit", bucket_mm)}</span> }.into_any()
                    } else {
                        ().into_any()
                    }}
                </div>
            </div>
            <div class="mobile-zone-row-actions">
                {if zone_running {
                    view! {
                        <button class="btn-clay btn-clay-hot zone-row-action" on:click=on_stop>"STOP"</button>
                    }.into_any()
                } else {
                    view! {
                        <button class="btn-clay zone-row-action" on:click=on_quick_run>"10m"</button>
                    }.into_any()
                }}
                <span class="mobile-zone-row-chevron" aria-hidden="true">"›"</span>
            </div>
        </a>
    }
}
