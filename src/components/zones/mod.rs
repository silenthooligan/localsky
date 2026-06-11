// Zones, master-detail (marquee feature 3), mirroring the Sensors page:
// a top KPI strip, then a column of rich zone cards on the left and a
// slide-out detail pane on the right that updates as you click a card (no
// navigation). `/zones/:slug` still deep-links to the standalone detail.

pub mod card;
pub mod detail;

use leptos::prelude::*;

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

    view! {
        <div class="zones-page">
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
                <div class="zones-detail">
                    <ZoneDetailView snap slug=detail_slug/>
                </div>
            </div>
        </div>
    }
}
