// SettingsControllers. List + add/edit/remove + raw-JSON config for
// irrigation controllers. Mirrors the Sources page pattern, with the
// added wrinkle that the `default` flag is mutually exclusive across
// controllers: toggling it on for one clears it on every other entry.
//
// The list view uses the SettingsCard UI kit, each controller is an
// expandable card with status badges and a read-only details panel.
// The user can browse what's configured without clicking "Edit"; the
// edit form only opens when they actually want to change something.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::controllers_form::ControllerEditorPanel;
use crate::components::settings_ui::{
    config_kvs, BadgeTone, SettingsBadge, SettingsCard, SettingsLoadError, SettingsResult,
};
use crate::components::ui::{HelpHint, Panel};
use crate::docs::doc_url;

#[component]
pub fn SettingsControllers() -> impl IntoView {
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let add_open = RwSignal::new(false);
    let editing_id: RwSignal<Option<String>> = RwSignal::new(None);
    // Initial-load state: Some(err) when the config GET failed. The
    // editor body is replaced by a Retry banner in that case.
    let load_error: RwSignal<Option<String>> = RwSignal::new(None);
    let load_retry = RwSignal::new(0u32);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let _ = load_retry.get();
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_config().await {
                    Ok(cfg) => {
                        config_json.set(cfg);
                        load_error.set(None);
                    }
                    Err(e) => load_error.set(Some(e)),
                }
            });
        });
        Effect::new(move |_| {
            let open = add_open.get();
            let _ = editing_id.get();
            if !open {
                return;
            }
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                if let Some(elt) = doc.get_element_by_id("controller-form-panel") {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                    opts.set_block(web_sys::ScrollLogicalPosition::Start);
                    elt.scroll_into_view_with_scroll_into_view_options(&opts);
                }
            }
        });
    }

    // Commit a controller entry from the shared editor into the in-memory
    // config (the page stays two-step: merge here, persist on "Save all
    // changes"). Enforces the default-flag exclusivity across controllers.
    let persist_entry = Callback::new(move |entry: serde_json::Value| {
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let was_editing = editing_id.get_untracked().is_some();
        config_json.update(|cfg| {
            if entry.get("default").and_then(|v| v.as_bool()) == Some(true) {
                if let Some(arr) = cfg.get_mut("controllers").and_then(|v| v.as_array_mut()) {
                    for c in arr.iter_mut() {
                        if c.get("id").and_then(|v| v.as_str()) != Some(id.as_str()) {
                            if let Some(obj) = c.as_object_mut() {
                                obj.insert("default".into(), serde_json::Value::Bool(false));
                            }
                        }
                    }
                }
            }
            if let Some(arr) = cfg.as_object_mut().and_then(|o| {
                o.entry("controllers")
                    .or_insert(serde_json::json!([]))
                    .as_array_mut()
            }) {
                if let Some(slot) = arr
                    .iter_mut()
                    .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                {
                    *slot = entry;
                } else {
                    arr.push(entry);
                }
            }
        });
        editing_id.set(None);
        add_open.set(false);
        result_ok.set(true);
        result_msg.set(if was_editing {
            "Updated. Click Save to apply.".into()
        } else {
            "Added. Click Save to apply.".into()
        });
    });

    let on_cancel_form = Callback::new(move |()| {
        editing_id.set(None);
        add_open.set(false);
    });

    let controllers_view = move || {
        let cfg = config_json.get();
        let arr = cfg
            .get("controllers")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        arr.into_iter()
            .map(|c| {
                view! {
                    <ControllerCard
                        controller=c
                        config_json=config_json
                        editing_id=editing_id
                        add_open=add_open
                    />
                }
            })
            .collect_view()
    };

    let on_save = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let candidate = config_json.get();
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match save_config(candidate).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Controller registry hot-reloads on next dispatch.",
                        );
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(e);
                    }
                }
                saving.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        {
            saving.set(false);
            let _ = candidate;
        }
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Irrigation controllers"<HelpHint topic="controllers"/></h1>
                <p class="settings-page__subtitle">
                    "Which hardware fires your valves. Exactly one must be default; new zones inherit that. "
                    "See "
                    <a href=doc_url("controllers")
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"the controllers doc"</a>
                    " for per-kind configuration details."
                </p>
            </header>

            <Show
                when=move || load_error.get().is_none()
                fallback=move || view! { <SettingsLoadError error=load_error retry=load_retry/> }
            >
            <Panel title="Configured controllers".to_string()>
                <ul class="settings-card-list">
                    {controllers_view}
                </ul>

                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    style="margin-top: 1rem"
                    on:click=move |_| {
                        let now_open = !add_open.get();
                        add_open.set(now_open);
                        if !now_open {
                            editing_id.set(None);
                        }
                    }
                >
                    {move || {
                        if !add_open.get() {
                            "+ Add controller"
                        } else if editing_id.get().is_some() {
                            "× Cancel edit"
                        } else {
                            "× Cancel add"
                        }
                    }}
                </button>
            </Panel>

            <Show when=move || add_open.get()>
                {move || {
                    // Re-mount with fresh seed when the edit target changes;
                    // look up the entry untracked so a config save doesn't
                    // re-create the form mid-commit.
                    let existing = editing_id.get().and_then(|eid| {
                        config_json
                            .get_untracked()
                            .get("controllers")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| {
                                arr.iter()
                                    .find(|c| {
                                        c.get("id").and_then(|v| v.as_str()) == Some(eid.as_str())
                                    })
                                    .cloned()
                            })
                    });
                    view! {
                        <ControllerEditorPanel
                            existing=existing
                            on_commit=persist_entry
                            on_cancel=on_cancel_form
                        />
                    }
                }}
            </Show>

            <button
                type="button"
                class="setup-apply-btn"
                style="margin-top: 1.5rem"
                disabled=move || saving.get()
                on:click=on_save
            >
                {move || if saving.get() { "Saving…" } else { "Save all changes" }}
            </button>
            </Show>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
        </main>
    }
}

/// Icon registry name (ui::Icon) for a controller kind.
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "opensprinkler_direct" => "droplet",
        "ha_service_call" => "home",
        "esphome_native" => "zap",
        "rachio" => "cloud",
        "hydrawise" => "cloud-rain",
        "bhyve" => "refresh",
        "rainbird" => "cloud-drizzle",
        "mqtt_command" => "download",
        "dry_run" => "play",
        _ => "droplet",
    }
}

fn kind_pretty(kind: &str) -> &'static str {
    match kind {
        "opensprinkler_direct" => "OpenSprinkler (direct)",
        "ha_service_call" => "Home Assistant service call",
        "esphome_native" => "ESPHome native API",
        "rachio" => "Rachio cloud",
        "hydrawise" => "Hunter Hydrawise cloud",
        "bhyve" => "Orbit B-hyve cloud",
        "rainbird" => "Rain Bird cloud",
        "mqtt_command" => "MQTT command publish",
        "dry_run" => "Dry-run (logs, no dispatch)",
        _ => "Unknown",
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_config() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    // A JSON error body must not be mistaken for the config.
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
}

#[cfg(feature = "hydrate")]
async fn save_config(cfg: serde_json::Value) -> Result<(), String> {
    use gloo_net::http::Request;
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {body}", resp.status()));
    }
    Ok(())
}

/// Single controller row, extracted into its own component so its
/// view-tree complexity (badges, KV grid, action buttons, click
/// handlers) is contained inside one monomorphization boundary
/// instead of compounding through the page's outer view tree. The
/// page renders a flat `Vec<ControllerCard view>` rather than a
/// deeply-typed tuple of card internals.
#[component]
fn ControllerCard(
    controller: serde_json::Value,
    config_json: RwSignal<serde_json::Value>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
) -> impl IntoView {
    let id = controller
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = controller
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let default = controller
        .get("default")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let enabled = controller
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let icon = kind_icon(&kind).to_string();
    let subtitle = kind_pretty(&kind).to_string();
    let title = id.clone();
    let config_obj = controller
        .get("config")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let kv_rows = config_kvs(&config_obj);
    let id_for_default = id.clone();
    let id_for_delete = id.clone();
    let id_for_edit = id.clone();
    let id_for_edit_label = id.clone();
    let id_for_delete_label = id.clone();

    let make_default = move |_| {
        let target = id_for_default.clone();
        config_json.update(|cfg| {
            if let Some(arr) = cfg.get_mut("controllers").and_then(|v| v.as_array_mut()) {
                for c in arr.iter_mut() {
                    let is_target = c.get("id").and_then(|v| v.as_str()) == Some(&target);
                    if let Some(obj) = c.as_object_mut() {
                        obj.insert("default".into(), serde_json::Value::Bool(is_target));
                    }
                }
            }
        });
    };
    // Open the shared editor on this controller; it seeds itself from config
    // by id (the page's <Show> looks the entry up), so we only flip state.
    let on_edit = move |_| {
        editing_id.set(Some(id_for_edit.clone()));
        add_open.set(true);
    };
    let on_delete = move |_| {
        let target = id_for_delete.clone();
        config_json.update(|cfg| {
            if let Some(arr) = cfg.get_mut("controllers").and_then(|v| v.as_array_mut()) {
                arr.retain(|c| c.get("id").and_then(|v| v.as_str()) != Some(&target));
            }
        });
    };

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon=icon
                title=title
                subtitle=subtitle
                badges=Box::new(move || view! {
                    {default.then(|| view! {
                        <SettingsBadge label="Default".into() tone=BadgeTone::Accent/>
                    })}
                    {if enabled {
                        view! { <SettingsBadge label="Enabled".into() tone=BadgeTone::Good/> }.into_any()
                    } else {
                        view! { <SettingsBadge label="Disabled".into() tone=BadgeTone::Muted/> }.into_any()
                    }}
                }.into_any())
                details=Box::new(move || view! { {kv_rows} }.into_any())
                actions=Box::new(move || view! {
                    {(!default).then(|| view! {
                        <button
                            class="setup-footer__btn setup-footer__btn--ghost"
                            type="button"
                            on:click=make_default
                        >
                            "Make default"
                        </button>
                    })}
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        aria-label=format!("Edit controller {id_for_edit_label}")
                        on:click=on_edit
                    >
                        "Edit"
                    </button>
                    <button
                        class="setup-footer__btn setup-footer__btn--danger"
                        type="button"
                        aria-label=format!("Delete controller {id_for_delete_label}")
                        on:click=on_delete
                    >
                        "Delete"
                    </button>
                }.into_any())
            />
        </li>
    }
}
