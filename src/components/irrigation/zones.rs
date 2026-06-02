// Zone tile grid. Phase 3a renders read-only tiles: name, state badge,
// today's run-minutes, today's bucket. Phase 3b adds the Claymorphism
// run buttons + stop control. Phase 3c adds the per-zone sparkline of
// last-14-day run-minutes.
//
// Four zones are static (back_yard, front_yard, side_yard, shrubs) so
// we unroll directly rather than using <For>. Each ZoneCard is type-
// erased via .into_any() so rustc's query depth doesn't blow up on
// the fully-monomorphized 4-card sibling tuple.

use crate::ha::snapshot::{IrrigationSnapshot, ZoneState};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
use serde_json::json;
use std::collections::HashMap;

#[component]
pub fn ZoneGrid(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // One-time fetch of zone photo URLs from /api/config so the cards
    // can render any operator-supplied images alongside the runtime
    // state. Keyed by zone slug. Empty during SSR + first hydrate frame,
    // then populated; cards re-render naturally via the signal.
    let (photos, set_photos) = signal::<HashMap<String, String>>(HashMap::new());
    #[cfg(not(feature = "hydrate"))]
    let _ = set_photos;
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            leptos::task::spawn_local(async move {
                if let Ok(m) = fetch_zone_photos().await {
                    if !m.is_empty() {
                        set_photos.set(m);
                    }
                }
            });
        });
    }

    let enrich = move |idx: usize| -> Signal<ZoneState> {
        Signal::derive(move || {
            let mut z = snap.get().zones.get(idx).cloned().unwrap_or_default();
            if z.photo_url.is_none() {
                if let Some(u) = photos.get().get(&z.slug).cloned() {
                    z.photo_url = Some(u);
                }
            }
            z
        })
    };
    let zone0 = enrich(0);
    let zone1 = enrich(1);
    let zone2 = enrich(2);
    let zone3 = enrich(3);

    view! {
        <section class="zone-grid">
            {view! { <ZoneCard zone=zone0/> }.into_any()}
            {view! { <ZoneCard zone=zone1/> }.into_any()}
            {view! { <ZoneCard zone=zone2/> }.into_any()}
            {view! { <ZoneCard zone=zone3/> }.into_any()}
        </section>
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_zone_photos() -> Result<HashMap<String, String>, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut out = HashMap::new();
    if let Some(zones) = val.get("zones").and_then(|z| z.as_object()) {
        for (slug, body) in zones {
            if let Some(url) = body
                .get("photo_url")
                .and_then(|u| u.as_str())
                .filter(|u| !u.is_empty())
            {
                out.insert(slug.clone(), url.to_string());
            }
        }
    }
    Ok(out)
}

#[component]
fn ZoneCard(zone: Signal<ZoneState>) -> impl IntoView {
    let state_class = move || {
        let z = zone.get();
        if z.running {
            "zone-card zone-card-running"
        } else if z.planned_run_seconds == 0 {
            "zone-card zone-card-disabled"
        } else {
            "zone-card"
        }
    };
    let badge = move || {
        let z = zone.get();
        if z.running {
            "RUNNING"
        } else if z.planned_run_seconds == 0 {
            // ZoneState exposes no config-disabled flag, so this branch
            // covers every "no run planned tonight" reason: engine
            // chose to skip (watering restriction, rain, wind, frost),
            // or the schedule simply doesn't fire on this day. Label
            // as SKIP so it matches the page-level "SKIPPING:" verdict
            // and stops implying the operator turned the zone off.
            "SKIP"
        } else {
            "IDLE"
        }
    };
    let running = Signal::derive(move || zone.get().running);

    view! {
        <article class=state_class>
            {move || {
                let url = zone.get().photo_url.clone();
                match url.filter(|u| !u.is_empty()) {
                    Some(u) => view! {
                        <img class="zone-card-photo" src=u alt="" loading="lazy"/>
                    }.into_any(),
                    None => view! { <></> }.into_any(),
                }
            }}
            <header class="zone-card-head">
                <h3 class="zone-card-name">{move || zone.get().name}</h3>
                <span class="zone-card-badge">{badge}</span>
            </header>
            <dl class="zone-card-stats">
                <div class="kv">
                    <dt class="k">"planned"</dt>
                    <dd class="v">
                        {move || format_minutes(zone.get().planned_run_seconds)}
                    </dd>
                </div>
                <div class="kv">
                    <dt class="k">"today"</dt>
                    <dd class="v">
                        {move || format!("{:.0} min", zone.get().today_run_minutes)}
                    </dd>
                </div>
                <div class="kv">
                    <dt class="k">"bucket"</dt>
                    <dd class="v">
                        {move || format!("{:+.1} mm", zone.get().bucket_mm)}
                    </dd>
                </div>
            </dl>
            {view! { <ZoneActions zone=zone running=running/> }.into_any()}
        </article>
    }
}

#[component]
fn ZoneActions(zone: Signal<ZoneState>, running: Signal<bool>) -> impl IntoView {
    // While idle: 3 quick-run buttons (10/30/60 min). While running:
    // single Stop button. Either dispatches to /api/irrigation/action
    // and the next poll updates the running flag, swapping the row.
    // Each button gets its own on:click closure so we don't have to
    // make a higher-order helper Clone-able through the move chain.
    view! {
        <div class="zone-actions" class:zone-actions-running=move || running.get()>
            {move || if running.get() {
                view! {
                    <button
                        class="btn-clay btn-clay-hot zone-stop-btn"
                        on:click=move |_| {
                            let slug = zone.get().slug;
                            super::controls::post_action(json!({"kind":"stop","zone":slug}));
                        }
                    >
                        "STOP"
                    </button>
                }.into_any()
            } else {
                view! {
                    <button
                        class="btn-clay"
                        on:click=move |_| {
                            let slug = zone.get().slug;
                            super::controls::post_action(json!({"kind":"run","zone":slug,"seconds":600}));
                        }
                    >"10m"</button>
                    <button
                        class="btn-clay"
                        on:click=move |_| {
                            let slug = zone.get().slug;
                            super::controls::post_action(json!({"kind":"run","zone":slug,"seconds":1800}));
                        }
                    >"30m"</button>
                    <button
                        class="btn-clay"
                        on:click=move |_| {
                            let slug = zone.get().slug;
                            super::controls::post_action(json!({"kind":"run","zone":slug,"seconds":3600}));
                        }
                    >"60m"</button>
                }.into_any()
            }}
        </div>
    }
}

fn format_minutes(seconds: u32) -> String {
    let m = seconds / 60;
    let s = seconds % 60;
    if s == 0 {
        format!("{m} min")
    } else {
        format!("{m}:{s:02}")
    }
}
