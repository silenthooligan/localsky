// SettingsSources. List view of configured weather + sensor sources,
// with add/edit/delete + raw-JSON config editor for per-kind customization.
//
// The /settings/sources page is intentionally raw-JSON for the per-kind
// configs in v0.1; per-kind form widgets (e.g. an MQTT subscription
// editor with topic + field + json_path inputs) land in a follow-up
// once the catalog of supported kinds stabilizes. Today's UX:
//
//   List of configured sources (id, kind, priority, edit + delete buttons)
//   "Add source" button -> kind picker + JSON config textarea
//   "Edit" on any row -> pre-populates the form with the source's
//   current values; ID is locked while editing. Save replaces the
//   entry in place (matched by id) instead of appending.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::settings_ui::{
    config_kvs, BadgeTone, SettingsBadge, SettingsCard, SettingsResult,
};
use crate::components::sources_form::{default_config_text, kind_icon, kind_pretty};
use crate::components::ui::{FormField, Panel, SegmentedControl};

#[component]
pub fn SettingsSources() -> impl IntoView {
    // The raw config JSON we round-trip; mutable as the operator edits.
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let add_open = RwSignal::new(false);
    // None = adding a new source; Some(id) = editing the source with that id.
    let editing_id: RwSignal<Option<String>> = RwSignal::new(None);
    let new_id = RwSignal::new(String::new());
    let new_kind = RwSignal::new("mqtt".to_string());
    let new_priority = RwSignal::new(50i32);
    let new_config_text = RwSignal::new(default_config_text("mqtt"));
    let new_enabled = RwSignal::new(true);

    // Load on mount.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    config_json.set(cfg);
                }
            });
        });
        // When kind changes during an add (not edit), swap default JSON template.
        Effect::new(move |_| {
            let k = new_kind.get();
            if editing_id.get_untracked().is_none() {
                new_config_text.set(default_config_text(&k));
            }
        });
        Effect::new(move |_| {
            let open = add_open.get();
            let _ = editing_id.get();
            if !open {
                return;
            }
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                if let Some(elt) = doc.get_element_by_id("source-form-panel") {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                    opts.set_block(web_sys::ScrollLogicalPosition::Start);
                    elt.scroll_into_view_with_scroll_into_view_options(&opts);
                }
            }
        });
    }

    let reset_form = move || {
        reset_source_draft(
            editing_id,
            new_id,
            new_kind,
            new_priority,
            new_config_text,
            new_enabled,
        );
    };

    let sources_view = move || {
        let cfg = config_json.get();
        let arr = cfg
            .get("sources")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        arr.into_iter()
            .map(|src| {
                view! {
                    <SourceCard
                        src=src
                        config_json=config_json
                        new_id=new_id
                        new_kind=new_kind
                        new_priority=new_priority
                        new_enabled=new_enabled
                        new_config_text=new_config_text
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
                            "Saved. Source registry hot-reloads on next tick.",
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
                <h1 class="settings-page__title">"Weather sources"</h1>
                <p class="settings-page__subtitle">
                    "Each source contributes per-field observations the merge engine picks from by priority. "
                    "Add what you have; remove what you don't. Higher priority wins per field for the same observation type."
                </p>
                <p class="settings-page__subtitle">
                    "Tip: the "<a href="/sensors">"Sensors hub"</a>" lets you add, validate (see live readings), and "
                    "assign sensors to zones in one place. This page is the advanced raw-config editor."
                </p>
            </header>

            <Panel title="Configured sources".to_string()>
                <ul class="settings-card-list">
                    {sources_view}
                </ul>

                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    style="margin-top: 1rem"
                    on:click=move |_| {
                        let now_open = !add_open.get();
                        add_open.set(now_open);
                        // Closing the panel resets editing state so a subsequent
                        // "+ Add source" starts blank rather than re-opening
                        // the previous edit.
                        if !now_open {
                            reset_form();
                        }
                    }
                >
                    {move || {
                        if !add_open.get() {
                            "+ Add source"
                        } else if editing_id.get().is_some() {
                            "× Cancel edit"
                        } else {
                            "× Cancel add"
                        }
                    }}
                </button>
            </Panel>

            <Show when=move || add_open.get()>
                <SourceForm
                    config_json=config_json
                    new_id=new_id
                    new_kind=new_kind
                    new_priority=new_priority
                    new_enabled=new_enabled
                    new_config_text=new_config_text
                    editing_id=editing_id
                    add_open=add_open
                    result_msg=result_msg
                    result_ok=result_ok
                />
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

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
        </main>
    }
}

/// Add/edit form for a single source, extracted out of the page
/// component so the page is a thin shell (header + list + save bar) and
/// this whole `<Panel>` view tree compiles inside its own
/// monomorphization boundary instead of nesting into the page. Owns the
/// "add to in-memory config" handler; the page still owns the load
/// (Effect) and the persist (Save all changes -> PUT).
#[component]
fn SourceForm(
    config_json: RwSignal<serde_json::Value>,
    new_id: RwSignal<String>,
    new_kind: RwSignal<String>,
    new_priority: RwSignal<i32>,
    new_enabled: RwSignal<bool>,
    new_config_text: RwSignal<String>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    result_msg: RwSignal<String>,
    result_ok: RwSignal<bool>,
) -> impl IntoView {
    let on_add = move |_| {
        let id = new_id.get().trim().to_string();
        if id.is_empty() {
            result_ok.set(false);
            result_msg.set("Source id is required".into());
            return;
        }
        let cfg_value: serde_json::Value = match serde_json::from_str(&new_config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                result_ok.set(false);
                result_msg.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        let entry = serde_json::json!({
            "id": id,
            "priority": new_priority.get(),
            "enabled": new_enabled.get(),
            "kind": new_kind.get(),
            "config": cfg_value,
        });
        let editing = editing_id.get_untracked();
        config_json.update(|cfg| {
            // Ensure the sources array exists.
            let arr = cfg.as_object_mut().and_then(|o| {
                o.entry("sources")
                    .or_insert(serde_json::json!([]))
                    .as_array_mut()
            });
            if let Some(arr) = arr {
                if let Some(target_id) = editing.as_ref() {
                    // Replace in place, matching by id.
                    if let Some(slot) = arr
                        .iter_mut()
                        .find(|s| s.get("id").and_then(|v| v.as_str()) == Some(target_id.as_str()))
                    {
                        *slot = entry;
                    } else {
                        // Editing target vanished — append as a new entry rather than lose the change.
                        arr.push(entry);
                    }
                } else {
                    arr.push(entry);
                }
            }
        });
        let was_editing = editing.is_some();
        reset_source_draft(
            editing_id,
            new_id,
            new_kind,
            new_priority,
            new_config_text,
            new_enabled,
        );
        add_open.set(false);
        result_ok.set(true);
        result_msg.set(if was_editing {
            "Updated. Click Save to apply.".into()
        } else {
            "Added. Click Save to apply.".into()
        });
    };

    let on_cancel = move |_| {
        reset_source_draft(
            editing_id,
            new_id,
            new_kind,
            new_priority,
            new_config_text,
            new_enabled,
        );
        add_open.set(false);
    };

    view! {
        <div id="source-form-panel"><Panel title="Source details".to_string()>
            <FormField
                label="ID".to_string()
                helptext="snake_case, unique across sources (e.g. tempest_lan, mqtt_sensors). Locked while editing.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="e.g. mqtt_sensors"
                    prop:value=move || new_id.get()
                    prop:disabled=move || editing_id.get().is_some()
                    on:input=move |ev| new_id.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Kind".to_string()
                helptext="What protocol or service this source uses.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl
                    value=new_kind
                    options=vec![
                        ("tempest_udp".into(), "Tempest UDP".into()),
                        ("tempest_ws".into(), "Tempest cloud".into()),
                        ("davis_wll".into(), "Davis WLL".into()),
                        ("ecowitt_local".into(), "Ecowitt LAN".into()),
                        ("ambient_weather".into(), "AmbientWeather".into()),
                        ("netatmo".into(), "Netatmo".into()),
                        ("yolink".into(), "YoLink".into()),
                        ("lacrosse".into(), "LaCrosse View".into()),
                        ("tuya_cloud".into(), "Tuya / RainPoint".into()),
                        ("open_meteo".into(), "Open-Meteo".into()),
                        ("nws".into(), "NWS".into()),
                        ("met_norway".into(), "Met.no".into()),
                        ("openweather".into(), "OpenWeather".into()),
                        ("pirate_weather".into(), "PirateWeather".into()),
                        ("mqtt".into(), "MQTT".into()),
                        ("http_webhook".into(), "HTTP webhook".into()),
                        ("ha_passthrough".into(), "HA passthrough".into()),
                        ("demo_replay".into(), "Demo".into()),
                    ]
                    aria_label="Source kind".to_string()
                />
            </FormField>

            <FormField
                label="Priority".to_string()
                helptext="Higher wins per-field. Convention: 100=LAN station, 50=cloud forecast, 10=fallback.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    class="ui-input"
                    min="-100"
                    max="200"
                    prop:value=move || new_priority.get().to_string()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<i32>() {
                            new_priority.set(v);
                        }
                    }
                />
            </FormField>

            <FormField
                label="Enabled".to_string()
                helptext="Unchecked sources stay in the config but don't poll.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || new_enabled.get()
                        on:input=move |ev| new_enabled.set(event_target_checked(&ev))
                    />
                    "Enable this source"
                </label>
            </FormField>

            <FormField
                label="Config (JSON)".to_string()
                helptext="Kind-specific configuration. The template auto-fills when you change Kind above (only while adding).".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <textarea
                    class="ui-input"
                    style="min-height: 180px; font-family: var(--font-mono); font-size: 0.85rem;"
                    prop:value=move || new_config_text.get()
                    on:input=move |ev| new_config_text.set(event_target_value(&ev))
                ></textarea>
            </FormField>

            <div class="settings-form-actions">
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--ghost"
                    on:click=on_cancel
                >
                    "Cancel"
                </button>
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    on:click=on_add
                >
                    {move || if editing_id.get().is_some() { "Save source changes" } else { "Add to list" }}
                </button>
            </div>
        </Panel></div>
    }
}

/// Reset the source draft signals back to a blank "new source" state.
/// Shared by the page's Cancel toggle and the form's post-add cleanup
/// so the two stay in sync.
fn reset_source_draft(
    editing_id: RwSignal<Option<String>>,
    new_id: RwSignal<String>,
    new_kind: RwSignal<String>,
    new_priority: RwSignal<i32>,
    new_config_text: RwSignal<String>,
    new_enabled: RwSignal<bool>,
) {
    editing_id.set(None);
    new_id.set(String::new());
    new_kind.set("mqtt".into());
    new_priority.set(50);
    new_config_text.set(default_config_text("mqtt"));
    new_enabled.set(true);
}

#[cfg(feature = "hydrate")]
async fn fetch_config() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
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

/// Single source row. Extracted into a named component so the
/// monomorphized view-tree of badges + KVs + action buttons stays
/// inside one boundary instead of compounding through the page's
/// outer view tree.
#[component]
fn SourceCard(
    src: serde_json::Value,
    config_json: RwSignal<serde_json::Value>,
    new_id: RwSignal<String>,
    new_kind: RwSignal<String>,
    new_priority: RwSignal<i32>,
    new_enabled: RwSignal<bool>,
    new_config_text: RwSignal<String>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
) -> impl IntoView {
    let id = src
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = src
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let priority = src.get("priority").and_then(|v| v.as_i64()).unwrap_or(50);
    let enabled = src.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let icon = kind_icon(&kind).to_string();
    let subtitle = format!("{} \u{00b7} priority {}", kind_pretty(&kind), priority);
    let config_obj = src
        .get("config")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let kv_rows = config_kvs(&config_obj);
    let id_for_edit = id.clone();
    let id_for_delete = id.clone();
    let id_for_edit_label = id.clone();
    let id_for_delete_label = id.clone();
    let src_for_edit = src.clone();
    let title = id.clone();

    let on_edit = move |_| {
        let target = id_for_edit.clone();
        let src = src_for_edit.clone();
        new_id.set(target.clone());
        new_kind.set(
            src.get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("mqtt")
                .to_string(),
        );
        new_priority.set(src.get("priority").and_then(|v| v.as_i64()).unwrap_or(50) as i32);
        new_enabled.set(src.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
        let cfg_text = src
            .get("config")
            .map(|c| serde_json::to_string_pretty(c).unwrap_or_else(|_| "{}".into()))
            .unwrap_or_else(|| "{}".into());
        new_config_text.set(cfg_text);
        editing_id.set(Some(target));
        add_open.set(true);
    };
    let on_delete = move |_| {
        let target = id_for_delete.clone();
        config_json.update(|cfg| {
            if let Some(arr) = cfg.get_mut("sources").and_then(|v| v.as_array_mut()) {
                arr.retain(|s| s.get("id").and_then(|v| v.as_str()) != Some(&target));
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
                    {if enabled {
                        view! { <SettingsBadge label="Enabled".into() tone=BadgeTone::Good/> }.into_any()
                    } else {
                        view! { <SettingsBadge label="Disabled".into() tone=BadgeTone::Muted/> }.into_any()
                    }}
                }.into_any())
                details=Box::new(move || view! { {kv_rows} }.into_any())
                actions=Box::new(move || view! {
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        aria-label=format!("Edit source {id_for_edit_label}")
                        on:click=on_edit
                    >
                        "Edit"
                    </button>
                    <button
                        class="setup-footer__btn setup-footer__btn--danger"
                        type="button"
                        aria-label=format!("Delete source {id_for_delete_label}")
                        on:click=on_delete
                    >
                        "Delete"
                    </button>
                }.into_any())
            />
        </li>
    }
}
