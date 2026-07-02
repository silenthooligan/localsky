// Zones, master-detail (marquee feature 3), mirroring the Sensors page:
// a top KPI strip, then a column of rich zone cards on the left and a
// slide-out detail pane on the right that updates as you click a card (no
// navigation). `/zones/:slug` still deep-links to the standalone detail.

pub mod card;
pub mod detail;

use leptos::prelude::*;

use crate::components::irrigation::anomaly_banner::AnomalyBanner;
use crate::components::ui::StatTile;
use crate::ha::snapshot::IrrigationSnapshot;
use card::ZoneCard;
pub use detail::{ZoneDetailPage, ZoneDetailView};

#[component]
pub fn ZonesPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let selected: RwSignal<Option<String>> = RwSignal::new(None);

    // Auto-select the first zone once the snapshot loads, so the detail
    // pane shows real data immediately (doesn't override a user pick).
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if selected.get_untracked().is_none() {
                if let Some(first) = snap.get().zones.first() {
                    selected.set(Some(first.slug.clone()));
                }
            }
        });
    }

    let detail_slug = Signal::derive(move || selected.get().unwrap_or_default());

    // When the user picks a different zone the detail pane swaps in place
    // (no navigation), so move focus into the pane and let SR/keyboard users
    // follow the selection. Skip the first run so SSR/initial auto-select
    // doesn't steal focus on load.
    //
    // The `.focus()` is DEFERRED to a microtask rather than called inline:
    // setting `selected` from a card click re-renders `ZoneDetailView`'s whole
    // subtree (via `detail_slug`) in the SAME reactive batch this effect runs
    // in. Focusing the container synchronously then races that DOM swap, and on
    // the first selection (the auto-select skeleton -> real-content transition)
    // the focus/scroll lands mid-swap, so the click appeared to need a second
    // tap to "take". Deferring lets the pane finish rendering first, so a single
    // click reliably switches the zone while focus-on-change is preserved.
    let detail_pane: NodeRef<leptos::html::Div> = NodeRef::new();
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |prev: Option<Option<String>>| {
            let cur = selected.get();
            // First run (prev is None): just record, don't focus.
            if let Some(prev_sel) = prev {
                if prev_sel != cur && cur.is_some() {
                    leptos::task::spawn_local(async move {
                        // Yield one microtask so the detail-pane DOM swap that
                        // this same selection triggered has been applied before
                        // we move focus into the (now-current) pane.
                        gloo_timers::future::TimeoutFuture::new(0).await;
                        if let Some(el) = detail_pane.get_untracked() {
                            let _ = el.focus();
                        }
                    });
                }
            }
            cur
        });
    }

    view! {
        <div class="zones-page">
            // Soil-anomaly surface, same component the irrigation page uses.
            // Quiet when there are no anomalies; the single owner of soil
            // offline/suspect warnings (never shown on the weather tab).
            <AnomalyBanner snap/>
            <header class="zones-page__header">
                <p class="zones-page__eyebrow">"Irrigation"</p>
                <h1 class="zones-page__title">"Zones"</h1>
                <p class="zones-page__sub">"Every zone at a glance, click one for full detail and control."</p>
            </header>

            // KPI summary strip.
            {move || {
                let s = snap.get();
                let total = s.zones.len();
                let running = s.zones.iter().filter(|z| z.running).count();
                let due = s.zones.iter().filter(|z| !z.running && z.planned_run_seconds > 0).count();
                let planned_min: u32 = s.zones.iter().map(|z| (z.planned_run_seconds + 30) / 60).sum();
                let skipping = s
                    .zones
                    .iter()
                    .filter(|z| z.verdict.as_ref().map(|v| v.verdict == "skip").unwrap_or(false))
                    .count();
                view! {
                    <div class="zones-kpis">
                        <StatTile label="Zones" value=total.to_string() icon="zones"/>
                        <StatTile label="Running" value=running.to_string() icon="play" accent="var(--verdict-run)".to_string()/>
                        <StatTile label="Due tonight" value=due.to_string() icon="droplet" accent="var(--accent)".to_string()/>
                        <StatTile label="Skipping" value=skipping.to_string() icon="ban" accent="var(--verdict-skip)".to_string()/>
                        <StatTile label="Planned" value=planned_min.to_string() unit="min" icon="gauge" accent="var(--accent-warm)".to_string()/>
                    </div>
                }
            }}

            // Master-detail: cards left, slide-out detail right.
            <div class="zones-shell">
                <div class="zones-cards">
                    {move || {
                        let s = snap.get();
                        if s.last_refresh_epoch == 0 {
                            // First snapshot hasn't streamed in yet.
                            return view! { <crate::components::ui::SkeletonRows count=4/> }.into_any();
                        }
                        if s.zones.is_empty() {
                            return view! {
                                <crate::components::ui::EmptyState
                                    title="No zones yet"
                                    body="Add a controller, scan it for stations, and your zones show up here with live status."
                                    cta_label="Set up zones"
                                    cta_href="/settings/zones"
                                    icon="zones"
                                />
                            }.into_any();
                        }
                        let soil: std::collections::HashMap<String, f64> = s
                            .soil_forecasts
                            .iter()
                            .filter_map(|f| f.current_pct.map(|p| (f.zone_slug.clone(), p)))
                            .collect();
                        s.zones
                            .into_iter()
                            .map(|z| {
                                let soil_pct = soil.get(&z.slug).copied();
                                view! { <ZoneCard zone=z selected soil_pct=soil_pct/> }
                            })
                            .collect_view()
                            .into_any()
                    }}
                </div>
                <div
                    class="zones-detail"
                    node_ref=detail_pane
                    tabindex="-1"
                    aria-live="polite"
                >
                    <ZoneDetailView snap slug=detail_slug/>
                </div>
            </div>
        </div>
    }
}
