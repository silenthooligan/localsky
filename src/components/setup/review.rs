// ReviewStep. Final page of the wizard. Renders a per-section summary of
// the draft (with Edit links back into each step) and a primary "Save and
// finish" button that POSTs /api/wizard/apply. On success, redirects to /.
// On failure, surfaces the validation error.

use leptos::prelude::*;

use crate::components::setup::shell::{prev_step_href, SetupFooter};
use crate::components::sources_form::kind_pretty;

#[cfg(feature = "hydrate")]
async fn fetch_draft() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

/// One summary row: section label, computed value text, Edit link target.
fn summary_rows(draft: &serde_json::Value) -> Vec<(&'static str, String, &'static str)> {
    let cfg = draft.get("config").cloned().unwrap_or_default();

    let loc = cfg
        .get("deployment")
        .and_then(|d| d.get("location"))
        .cloned()
        .unwrap_or_default();
    let lat = loc.get("lat").and_then(|v| v.as_f64());
    let lon = loc.get("lon").and_then(|v| v.as_f64());
    let tz = cfg
        .get("deployment")
        .and_then(|d| d.get("timezone"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let location_text = match (lat, lon) {
        (Some(lat), Some(lon)) if lat != 0.0 || lon != 0.0 => {
            let tz_note = tz
                .map(|t| format!(", {t}"))
                .unwrap_or_else(|| ", timezone inferred at boot".into());
            format!("{lat:.4}, {lon:.4}{tz_note}")
        }
        _ => "Not set (engine needs this for sunrise and forecasts)".into(),
    };

    let sources = cfg
        .get("sources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let sources_text = if sources.is_empty() {
        "None added (defaults synthesize Tempest UDP + Open-Meteo)".into()
    } else {
        let kinds: Vec<String> = sources
            .iter()
            .filter_map(|s| s.get("kind").and_then(|k| k.as_str()))
            .map(|k| kind_pretty(k).to_string())
            .collect();
        format!("{} ({})", sources.len(), kinds.join(", "))
    };

    let controllers = cfg
        .get("controllers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let controllers_text = if controllers.is_empty() {
        "None added (HA env synthesizes one, or add later in Settings)".into()
    } else {
        let names: Vec<String> = controllers
            .iter()
            .filter_map(|c| {
                let id = c.get("id")?.as_str()?;
                let default = c.get("default").and_then(|d| d.as_bool()) == Some(true);
                Some(if default {
                    format!("{id} (default)")
                } else {
                    id.to_string()
                })
            })
            .collect();
        names.join(", ")
    };

    let zones = cfg
        .get("zones")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let zones_text = if zones.is_empty() {
        "None yet (add in Settings -> Zones any time)".into()
    } else {
        let names: Vec<String> = zones
            .values()
            .filter_map(|z| z.get("display_name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        format!("{}: {}", zones.len(), names.join(", "))
    };

    let llm = cfg.get("llm").cloned().unwrap_or(serde_json::Value::Null);
    let llm_text = if llm.is_null() {
        "Disabled".into()
    } else {
        llm.get("provider")
            .and_then(|p| p.as_str())
            .map(|p| match p {
                "auto" => "Auto-detect on boot".to_string(),
                "ollama" => "Ollama".to_string(),
                "llamacpp" => "llama.cpp".to_string(),
                "openai_compat" => "OpenAI-compatible".to_string(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "Configured".into())
    };

    let notif = cfg
        .get("notifications")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let channels: Vec<&str> = [
        ("web_push", "Web Push"),
        ("mqtt", "MQTT"),
        ("ntfy", "ntfy"),
        ("slack", "Slack"),
        ("email", "Email"),
    ]
    .iter()
    .filter(|(key, _)| notif.get(*key).map(|v| !v.is_null()).unwrap_or(false))
    .map(|(_, label)| *label)
    .collect();
    let notif_text = if channels.is_empty() {
        "None (dashboard only)".into()
    } else {
        channels.join(", ")
    };

    vec![
        ("Location", location_text, "/setup/location"),
        ("Weather sources", sources_text, "/setup/sources"),
        ("Controllers", controllers_text, "/setup/controllers"),
        ("Zones", zones_text, "/setup/zones"),
        ("LLM advisor", llm_text, "/setup/llm"),
        ("Notifications", notif_text, "/setup/notifications"),
    ]
}

#[component]
pub fn ReviewStep() -> impl IntoView {
    let applying = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let draft = RwSignal::new(serde_json::Value::Null);

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

    let on_apply = move |_| {
        applying.set(true);
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match call_apply().await {
                    Ok(()) => {
                        result_ok.set(true);
                        result_msg
                            .set("Configuration saved. Redirecting to dashboard…".to_string());
                        if let Some(win) = web_sys::window() {
                            // Arm the one-time "start here" card on the
                            // Weather home (welcome_card.rs reads this).
                            if let Ok(Some(storage)) = win.local_storage() {
                                let _ = storage.set_item("first_run_done", "0");
                            }
                            let _ = win.location().set_href(&crate::base::url("/"));
                        }
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(e);
                    }
                }
                applying.set(false);
            });
        }
        // SSR path: no-op; the button is only meaningfully interactive
        // after hydrate.
        #[cfg(not(feature = "hydrate"))]
        {
            applying.set(false);
        }
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Everything look right?"</h2>
            <p class="setup-step__body">
                "When you click apply, your settings save to "
                <code>"/data/localsky.toml"</code>" and the dashboard mounts. "
                "If validation fails, you'll get a specific error here and "
                "nothing changes on disk."
            </p>

            {move || {
                let d = draft.get();
                if d.is_null() {
                    return ().into_any();
                }
                let rows = summary_rows(&d)
                    .into_iter()
                    .map(|(label, value, href)| view! {
                        <div class="review-row">
                            <span class="review-row__label">{label}</span>
                            <span class="review-row__value">{value}</span>
                            <a class="review-row__edit" href=href>"Edit"</a>
                        </div>
                    })
                    .collect_view();
                view! { <div class="review-table">{rows}</div> }.into_any()
            }}

            <div class="review-summary">
                <p class="review-summary__line">
                    "Settings will be saved to "
                    <code>"/data/localsky.toml"</code>
                    " and a snapshot recorded in the config history table "
                    "(retained for 20 versions; rollback via /api/config/rollback)."
                </p>
                <p class="review-summary__line">
                    "Once applied, day-to-day edits live in /settings. If you "
                    "open the wizard again it offers a choice: modify the "
                    "current setup or start fresh."
                </p>
            </div>

            <button
                type="button"
                class="setup-apply-btn"
                disabled=move || applying.get()
                on:click=on_apply
            >
                {move || if applying.get() { "Saving…" } else { "Save and finish" }}
            </button>

            <Show when=move || !result_msg.get().is_empty()>
                <p
                    class="setup-result"
                    class:setup-result--ok=move || result_ok.get()
                    class:setup-result--err=move || !result_ok.get()
                    role="status"
                >
                    {move || result_msg.get()}
                </p>
            </Show>

            <SetupFooter prev=prev_step_href("review") next={None::<String>}/>
        </div>
    }
}

#[cfg(feature = "hydrate")]
async fn call_apply() -> Result<(), String> {
    use gloo_net::http::Request;
    match Request::post("/api/wizard/apply").send().await {
        Ok(r) if r.ok() => Ok(()),
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            Err(format!("Apply failed (HTTP {status}): {body}"))
        }
        Err(e) => Err(format!("Network error: {e}")),
    }
}
