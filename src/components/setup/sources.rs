// SourcesStep. Add weather + sensor sources during first-run setup,
// reusing the same inline editor as the Sensors hub. Sources are written
// into the wizard draft (not the live config, which doesn't exist yet);
// once you finish setup they go live and you validate them on the Sensors
// hub. Skipping is fine, sensors can be added there any time, no wizard
// required.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::sources_form::{kind_pretty, SourceEditorPanel};

#[cfg(feature = "hydrate")]
async fn fetch_draft() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

#[cfg(feature = "hydrate")]
async fn save_draft(draft: serde_json::Value) -> Result<(), String> {
    let resp = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&draft)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

#[component]
pub fn SourcesStep() -> impl IntoView {
    let draft = RwSignal::new(serde_json::Value::Null);
    let adding = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                draft.set(d);
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = draft;

    let persist = Callback::new(move |entry: serde_json::Value| {
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        draft.update(|d| {
            if let Some(arr) = d
                .get_mut("config")
                .and_then(|c| c.get_mut("sources"))
                .and_then(|v| v.as_array_mut())
            {
                if let Some(slot) = arr
                    .iter_mut()
                    .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                {
                    *slot = entry.clone();
                } else {
                    arr.push(entry.clone());
                }
            }
        });
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
        adding.set(false);
    });

    let added_view = move || {
        let sources = draft
            .get()
            .get("config")
            .and_then(|c| c.get("sources"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if sources.is_empty() {
            return view! {
                <p class="setup-step__body" style="margin:0">"No sources added yet."</p>
            }
            .into_any();
        }
        sources
            .into_iter()
            .map(|s| {
                let id = s
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let kind = s.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                let pretty = kind_pretty(kind).to_string();
                view! {
                    <li class="cond-row">
                        <span class="cond-row__dot"></span>
                        <div class="cond-row__text">
                            <span class="cond-row__name">{id}</span>
                            <span class="cond-row__sum">{pretty}</span>
                        </div>
                    </li>
                }
            })
            .collect_view()
            .into_any()
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Where should your weather come from?"</h2>
            <p class="setup-step__body">
                "LocalSky merges across any number of sources. A live LAN station (Tempest, "
                "Ecowitt) is the strongest signal; a forecast model (Open-Meteo, NWS) fills the "
                "horizon. Add what you have, the engine picks per-field winners by priority."
            </p>

            <crate::components::setup::discover::NetworkScan mode="sources" draft=draft/>

            <ul class="cond-list">{added_view}</ul>

            {move || if adding.get() {
                view! {
                    <SourceEditorPanel
                        on_commit=persist
                        on_cancel=Callback::new(move |()| adding.set(false))
                    />
                }.into_any()
            } else {
                view! {
                    <button type="button" class="setup-footer__btn setup-footer__btn--primary"
                        on:click=move |_| adding.set(true)>"+ Add a source"</button>
                }.into_any()
            }}

            <p class="sensors-section__hint" style="margin-top: var(--space-3)">
                "Sources you add here go live when you finish setup. To confirm one is actually "
                "ingesting (and see its live readings), open the "<a href="/sensors">"Sensors hub"</a>
                " afterward, that's also where you can add or edit sensors any time, no wizard required. "
                "Skipping this step is fine: LocalSky will synthesize a Tempest UDP + Open-Meteo default."
            </p>

            <SetupFooter
                prev=prev_step_href("sources")
                next=next_step_href("sources")
            />
        </div>
    }
}
