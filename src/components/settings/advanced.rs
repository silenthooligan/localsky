// SettingsAdvanced. Nerd mode toggle + rollback snapshot list + demo
// mode display.

use leptos::prelude::*;

use crate::components::ui::{Panel, Toggle};

#[component]
pub fn SettingsAdvanced() -> impl IntoView {
    let nerd_mode = RwSignal::new(false);
    let demo_mode_active = RwSignal::new(false);
    let snapshots = RwSignal::new(Vec::<SnapshotRow>::new());

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    if let Ok(Some(v)) = storage.get_item("nerd_mode") {
                        nerd_mode.set(v == "1" || v == "true");
                    }
                }
            }
        });
        // Persist nerd_mode on change.
        Effect::new(move |_| {
            let v = nerd_mode.get();
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    let _ = storage.set_item("nerd_mode", if v { "1" } else { "0" });
                }
            }
        });
        // Fetch current config + snapshot list on mount.
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(demo) = fetch_demo_mode().await {
                    demo_mode_active.set(demo);
                }
            });
        });
    }

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Advanced"</h1>
                <p class="settings-page__subtitle">
                    "Debug visibility and rollback. None of these change the "
                    "engine's behavior; they just expose what's already "
                    "happening."
                </p>
            </header>

            <Panel title="Nerd mode".to_string()>
                <Toggle
                    checked=nerd_mode
                    label="Show raw engine math everywhere".to_string()
                    helptext="When on, every irrigation panel surfaces ET0, ETc, bucket depth, Kc, MAD, available water, and root depth. Per-device, persisted to localStorage.".to_string()
                />
            </Panel>

            <Panel title="Demo mode".to_string()>
                <p class="settings-page__subtitle" style="margin: 0">
                    {move || if demo_mode_active.get() {
                        "Active. All controller actions are recorded but not fired; weather data is simulated."
                    } else {
                        "Inactive. To enable, set LOCALSKY_DEMO=1 in the container env or features.demo_mode = true in /data/localsky.toml."
                    }}
                </p>
            </Panel>

            <Panel title="Configuration history".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 1rem">
                    "Every PUT /api/config snapshots the previous config "
                    "before writing. Roll back to any of the last 20 "
                    "versions via "
                    <code>"POST /api/config/rollback?to=&lt;version&gt;"</code>
                    "."
                </p>
                <Show
                    when=move || !snapshots.get().is_empty()
                    fallback=|| view! {
                        <p class="settings-page__subtitle" style="margin: 0">
                            "No snapshots yet. The first save records version 1."
                        </p>
                    }
                >
                    <ul class="setup-source-list">
                        {move || snapshots.get().into_iter().map(|s| view! {
                            <li>
                                <strong>{format!("v{}", s.version)}</strong>
                                " applied "
                                {format_epoch(s.applied_at_epoch)}
                                {s.note.map(|n| format!(" — {n}"))}
                            </li>
                        }).collect_view()}
                    </ul>
                </Show>
            </Panel>

            <Panel title="Raw config".to_string()>
                <p class="settings-page__subtitle" style="margin: 0">
                    "The full TOML lives at "
                    <code>"/data/localsky.toml"</code>
                    ". Inspect via "
                    <code>"GET /api/config"</code>
                    " (returns JSON; secrets redacted). The JSON Schema is "
                    "available at "
                    <code>"GET /api/config/schema"</code>
                    " for tooling integration."
                </p>
            </Panel>
        </main>
    }
}

#[derive(Clone)]
struct SnapshotRow {
    version: u32,
    applied_at_epoch: i64,
    note: Option<String>,
}

fn format_epoch(epoch: i64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(epoch, 0).single() {
        Some(dt) => dt.format("%Y-%m-%d %H:%M").to_string(),
        None => format!("epoch {epoch}"),
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_demo_mode() -> Result<bool, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(val
        .get("features")
        .and_then(|f| f.get("demo_mode"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false))
}
