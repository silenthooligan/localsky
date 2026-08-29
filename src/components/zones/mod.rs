// Zones, master-detail (marquee feature 3), mirroring the Sensors page:
// a top KPI strip, then a column of rich zone cards on the left and a
// slide-out detail pane on the right that updates as you click a card (no
// navigation). `/zones/:slug` still deep-links to the standalone detail.

pub mod card;
pub mod detail;
pub mod tuning;

use leptos::prelude::*;

use crate::components::irrigation::anomaly_banner::AnomalyBanner;
use crate::components::ui::StatTile;
use crate::ha::snapshot::IrrigationSnapshot;
use card::ZoneCard;
pub use detail::{ZoneDetailPage, ZoneDetailView};

#[component]
pub fn ZonesPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let selected: RwSignal<Option<String>> = RwSignal::new(None);
    // The page CONSUMES the app-level tuning report (provided by App()
    // via provide_tuning_summary): the Suggestions KPI, the per-card
    // attention pills, and the recommendation-aware auto-select below all
    // join against that one shared signal, never a second page fetch. The
    // detail pane's Tuning panel keeps its own fetch for its
    // Apply-refetch cycle; its Apply also bumps the app-wide TuningEpoch,
    // which re-fetches the shared report so every surface in the viewport
    // (the nav badge included) reflects the apply without a reload.
    let tuning_report = tuning::use_tuning_report();
    // What auto-select last picked, so a report arriving after the
    // snapshot can upgrade the initial zones[0] pick to the first zone
    // with a recommendation WITHOUT ever overriding a user's click.
    // Consumed only by the hydrate-only effects below (SSR renders with
    // no selection); the let binding keeps the ssr build warning-free.
    let auto_pick: RwSignal<Option<String>> = RwSignal::new(None);
    #[cfg(not(feature = "hydrate"))]
    let _ = auto_pick;

    // Auto-select once the snapshot loads, so the detail pane shows real
    // data immediately. When the report is loaded and a recommendation
    // exists, the first zone carrying one wins over zones[0]; a pick the
    // user made is never overridden (it differs from auto_pick).
    #[cfg(feature = "hydrate")]
    {
        // Entering the page re-fetches the shared report (the per-mount
        // freshness the page-local fetch had before the app context), so
        // the weekly push's "N zones" lands on numbers read NOW, not at
        // app hydrate. On-mount only: the closure reads no signals.
        Effect::new(move |_| {
            tuning::refresh_tuning_report();
        });
        Effect::new(move |_| {
            let s = snap.get();
            if s.zones.is_empty() {
                return;
            }
            let rep = tuning_report.get();
            let cur = selected.get_untracked();
            if cur.is_some() && cur != auto_pick.get_untracked() {
                // The user picked: selection is theirs from here on, and
                // clearing auto_pick lets the focus effect treat every
                // later pick (including re-picking the auto zone) as a
                // real user action.
                auto_pick.set(None);
                return;
            }
            let target = rep
                .as_ref()
                .map(tuning::recommended_slugs)
                .filter(|set| !set.is_empty())
                .and_then(|set| {
                    s.zones
                        .iter()
                        .find(|z| set.contains(&z.slug.replace('-', "_")))
                        .map(|z| z.slug.clone())
                })
                .or_else(|| s.zones.first().map(|z| z.slug.clone()));
            if let Some(t) = target {
                if cur.as_deref() != Some(t.as_str()) {
                    selected.set(Some(t.clone()));
                }
                auto_pick.set(Some(t));
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
            // Auto picks (initial selection + the recommendation upgrade)
            // must not steal focus; only a user's own selection moves it.
            if cur.is_some() && cur == auto_pick.get_untracked() {
                return cur;
            }
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

            // KPI summary strip. The Suggestions tile joins the page-level
            // tuning report, so the weekly push's "N zones" lands on a page
            // whose first visible number matches; a dash until the report
            // loads (unknown is never a fabricated zero).
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
                let suggestions = tuning_report
                    .get()
                    .map(|r| tuning::recommendation_count(&r).to_string())
                    .unwrap_or_else(|| "-".to_string());
                view! {
                    <div class="zones-kpis">
                        <StatTile label="Zones" value=total.to_string() icon="zones"/>
                        <StatTile label="Running" value=running.to_string() icon="play" accent="var(--verdict-run)".to_string()/>
                        <StatTile label="Due tonight" value=due.to_string() icon="droplet" accent="var(--accent)".to_string()/>
                        <StatTile label="Skipping" value=skipping.to_string() icon="ban" accent="var(--verdict-skip)".to_string()/>
                        <StatTile label="Planned" value=planned_min.to_string() unit="min" icon="gauge" accent="var(--accent-warm)".to_string()/>
                        <StatTile label="Suggestions" value=suggestions icon="zap" accent="var(--attention)".to_string()/>
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
                        // Per-card attention pills join the ONE page-level
                        // report (underscore-normalized), never a per-card
                        // fetch.
                        let recs = tuning_report
                            .get()
                            .map(|r| tuning::recommended_slugs(&r))
                            .unwrap_or_default();
                        s.zones
                            .into_iter()
                            .map(|z| {
                                let soil_pct = soil.get(&z.slug).copied();
                                let has_suggestion = recs.contains(&z.slug.replace('-', "_"));
                                view! { <ZoneCard zone=z selected soil_pct=soil_pct has_suggestion/> }
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
