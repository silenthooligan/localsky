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
use leptos_router::hooks::{use_location, use_navigate};

use crate::components::controllers_form::ControllerEditorPanel;
use crate::components::settings::{form_state_url, parse_form_state, FormState};
use crate::components::settings_ui::{
    config_kvs, BadgeTone, EntityKind, SettingsBadge, SettingsCard, SettingsLoadError,
    SettingsResult,
};
use crate::components::ui::{Button, HelpHint, Panel};
use crate::docs::doc_url;

#[component]
pub fn SettingsControllers() -> impl IntoView {
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    // Persistent, dismissible restart-required banner. Populated only when a
    // save returns restart_required=true (a newly added controller that needs a
    // boot-wired connection); routine edits hot-reload and leave it empty.
    let restart_reasons: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    let restart_dismissed = RwSignal::new(false);
    // The add/edit form's open-state is URL state (?add=1 / ?edit=<id>), not a
    // bare signal: opening pushes a history entry, so the phone back gesture
    // closes the form instead of leaving settings. SSR + hydrate both derive it
    // from the same URL. `add_open` / `editing_id` are kept as derived views of
    // that state so the rest of the page is unchanged.
    let loc = use_location();
    let form_state = Signal::derive(move || parse_form_state(&loc.search.get()));
    let add_open = Signal::derive(move || form_state.get() != FormState::Closed);
    let editing_id = Signal::derive(move || match form_state.get() {
        FormState::Edit(id) => Some(id),
        _ => None,
    });
    let navigate = use_navigate();
    let nav_form: Callback<FormState> = Callback::new(move |next: FormState| {
        let url = form_state_url(
            &loc.pathname.get_untracked(),
            &loc.search.get_untracked(),
            &next,
        );
        navigate(&url, Default::default());
    });
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
        nav_form.run(FormState::Closed);
        result_ok.set(true);
        result_msg.set(if was_editing {
            "Updated. Click Save to apply.".into()
        } else {
            "Added. Click Save to apply.".into()
        });
    });

    let on_cancel_form = Callback::new(move |()| nav_form.run(FormState::Closed));

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
                        nav_form=nav_form
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
                    Ok(reasons) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Controller registry hot-reloads on next dispatch.",
                        );
                        // A newly added controller that needs a boot-wired
                        // connection raises the dismissible banner with the
                        // server's reasons; an empty list (hot-reload) clears it.
                        restart_dismissed.set(false);
                        restart_reasons.set(reasons);
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
        <div class="settings-page">
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

            <crate::components::settings::RestartBanner reasons=restart_reasons dismissed=restart_dismissed/>

            <Show
                when=move || load_error.get().is_none()
                fallback=move || view! { <SettingsLoadError error=load_error retry=load_retry/> }
            >
            <Panel title="Configured controllers".to_string()>
                <ul class="settings-card-list">
                    {controllers_view}
                </ul>

                <div class="settings-add-btn">
                    <Button
                        variant="primary"
                        on_click=Callback::new(move |_| {
                            // Toggle: open the add form, or close whatever's open.
                            let next = if add_open.get() {
                                FormState::Closed
                            } else {
                                FormState::Add
                            };
                            nav_form.run(next);
                        })
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
                    </Button>
                </div>
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
        </div>
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
        "http_generic" => "activity",
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
        "http_generic" => "DIY HTTP / REST board",
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

/// PUT the candidate config. Returns the restart_reasons the PUT response
/// carried (empty when the change hot-reloaded). A newly added controller that
/// needs a boot-wired connection (e.g. an OpenSprinkler poll loop) flags
/// restart_required=true with the reasons; the caller raises the shared
/// RestartBanner. A missing/old field reads as "no restart", the safe default.
#[cfg(feature = "hydrate")]
async fn save_config(cfg: serde_json::Value) -> Result<Vec<String>, String> {
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
    let reasons = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .filter(|v| {
            v.get("restart_required")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
        })
        .and_then(|v| {
            v.get("restart_reasons")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default();
    Ok(reasons)
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
    nav_form: Callback<FormState>,
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
    // by id (the page's <Show> looks the entry up), so we only set URL state.
    let on_edit = move |_| {
        nav_form.run(FormState::Edit(id_for_edit.clone()));
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
                entity=Some(EntityKind::Controller)
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
                        <Button
                            variant="ghost"
                            on_click=Callback::new(make_default)
                        >
                            "Make default"
                        </Button>
                    })}
                    <Button
                        variant="ghost"
                        aria_label=format!("Edit controller {id_for_edit_label}")
                        on_click=Callback::new(on_edit)
                    >
                        "Edit"
                    </Button>
                    <Button
                        variant="danger"
                        aria_label=format!("Delete controller {id_for_delete_label}")
                        on_click=Callback::new(on_delete)
                    >
                        "Delete"
                    </Button>
                }.into_any())
            />
        </li>
    }
}
