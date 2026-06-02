// SettingsControllers. List + add/edit/remove + raw-JSON config for
// irrigation controllers. Mirrors the Sources page pattern, with the
// added wrinkle that the `default` flag is mutually exclusive across
// controllers: toggling it on for one clears it on every other entry.
//
// The list view uses the SettingsCard UI kit — each controller is an
// expandable card with status badges and a read-only details panel.
// The user can browse what's configured without clicking "Edit"; the
// edit form only opens when they actually want to change something.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::settings_ui::{
    config_kvs, BadgeTone, SettingsBadge, SettingsCard, SettingsResult,
};
use crate::components::ui::{FormField, Panel, SegmentedControl};
use crate::docs::doc_url;

#[component]
pub fn SettingsControllers() -> impl IntoView {
    let config_json = RwSignal::new(serde_json::Value::Null);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let add_open = RwSignal::new(false);
    let editing_id: RwSignal<Option<String>> = RwSignal::new(None);
    let new_id = RwSignal::new(String::new());
    let new_kind = RwSignal::new("opensprinkler_direct".to_string());
    let new_default = RwSignal::new(false);
    let new_enabled = RwSignal::new(true);
    let new_config_text = RwSignal::new(default_config_text("opensprinkler_direct"));

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    config_json.set(cfg);
                }
            });
        });
        // Only auto-swap the config template when the user is composing a
        // fresh controller. During an edit we keep their JSON intact.
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
                if let Some(elt) = doc.get_element_by_id("controller-form-panel") {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                    opts.set_block(web_sys::ScrollLogicalPosition::Start);
                    elt.scroll_into_view_with_scroll_into_view_options(&opts);
                }
            }
        });
    }

    let reset_form = move || {
        reset_controller_draft(
            editing_id,
            new_id,
            new_kind,
            new_default,
            new_enabled,
            new_config_text,
        );
    };

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
                        new_id=new_id
                        new_kind=new_kind
                        new_default=new_default
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
                        result_ok.set(true);
                        result_msg
                            .set("Saved. Controller registry hot-reloads on next dispatch.".into());
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
                <h1 class="settings-page__title">"Irrigation controllers"</h1>
                <p class="settings-page__subtitle">
                    "Which hardware fires your valves. Exactly one must be default; new zones inherit that. "
                    "See "
                    <a href=doc_url("controllers")
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"the controllers doc"</a>
                    " for per-kind configuration details."
                </p>
            </header>

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
                            reset_form();
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
                <ControllerForm
                    config_json=config_json
                    new_id=new_id
                    new_kind=new_kind
                    new_default=new_default
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

/// Add/edit form for a single controller, extracted out of the page
/// component so the page is a thin shell (header + list + save bar) and
/// this whole `<Panel>` view tree compiles inside its own
/// monomorphization boundary instead of nesting into the page. Owns the
/// "add to in-memory config" handler; the page still owns the load
/// (Effect) and the persist (Save all changes -> PUT).
#[component]
fn ControllerForm(
    config_json: RwSignal<serde_json::Value>,
    new_id: RwSignal<String>,
    new_kind: RwSignal<String>,
    new_default: RwSignal<bool>,
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
            result_msg.set("Controller id is required".into());
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
        let default_flag = new_default.get();
        let enabled_flag = new_enabled.get();
        let entry = serde_json::json!({
            "id": id,
            "default": default_flag,
            "enabled": enabled_flag,
            "kind": new_kind.get(),
            "config": cfg_value,
        });
        let editing = editing_id.get_untracked();
        config_json.update(|cfg| {
            // Default-flag exclusivity: if this entry is being marked
            // default, clear default on every other controller first.
            if default_flag {
                if let Some(arr) = cfg.get_mut("controllers").and_then(|v| v.as_array_mut()) {
                    for c in arr.iter_mut() {
                        let is_self = c.get("id").and_then(|v| v.as_str()) == Some(id.as_str());
                        if !is_self {
                            if let Some(obj) = c.as_object_mut() {
                                obj.insert("default".into(), serde_json::Value::Bool(false));
                            }
                        }
                    }
                }
            }
            let arr = cfg.as_object_mut().and_then(|o| {
                o.entry("controllers")
                    .or_insert(serde_json::json!([]))
                    .as_array_mut()
            });
            if let Some(arr) = arr {
                if let Some(target_id) = editing.as_ref() {
                    if let Some(slot) = arr
                        .iter_mut()
                        .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(target_id.as_str()))
                    {
                        *slot = entry;
                    } else {
                        arr.push(entry);
                    }
                } else {
                    arr.push(entry);
                }
            }
        });
        let was_editing = editing.is_some();
        reset_controller_draft(
            editing_id,
            new_id,
            new_kind,
            new_default,
            new_enabled,
            new_config_text,
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
        reset_controller_draft(
            editing_id,
            new_id,
            new_kind,
            new_default,
            new_enabled,
            new_config_text,
        );
        add_open.set(false);
    };

    // Scan zones: probe the controller (from the current draft config, no
    // save needed) and list its stations so the user knows which station
    // numbers to assign when adding zones. HA-free.
    let scan_msg = RwSignal::new(String::new());
    let on_scan = move |_| {
        let cfg_value: serde_json::Value = match serde_json::from_str(&new_config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                scan_msg.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        let entry = serde_json::json!({
            "id": new_id.get(),
            "default": new_default.get(),
            "enabled": new_enabled.get(),
            "kind": new_kind.get(),
            "config": cfg_value,
        });
        scan_msg.set("Scanning…".into());
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            let body = serde_json::json!({ "controller": entry });
            let req = match gloo_net::http::Request::post("/api/v1/wizard/scan_zones").json(&body) {
                Ok(r) => r,
                Err(e) => {
                    scan_msg.set(format!("encode failed: {e}"));
                    return;
                }
            };
            match req.send().await {
                Ok(resp) => {
                    let v = resp
                        .json::<serde_json::Value>()
                        .await
                        .unwrap_or(serde_json::Value::Null);
                    if let Some(zones) = v.get("zones").and_then(|z| z.as_array()) {
                        let list: Vec<String> = zones
                            .iter()
                            .filter_map(|z| {
                                Some(format!(
                                    "{} → station {}",
                                    z.get("name")?.as_str()?,
                                    z.get("station_id")?.as_str()?
                                ))
                            })
                            .collect();
                        scan_msg.set(if list.is_empty() {
                            "No zones found on this controller.".into()
                        } else {
                            format!("Found {}: {}", list.len(), list.join(" · "))
                        });
                    } else {
                        let detail = v
                            .get("detail")
                            .and_then(|d| d.as_str())
                            .unwrap_or("controller unreachable or kind not probeable");
                        scan_msg.set(format!("Scan failed: {detail}"));
                    }
                }
                Err(e) => scan_msg.set(format!("request failed: {e}")),
            }
        });
    };

    view! {
        <div id="controller-form-panel"><Panel title="Controller details".to_string()>
            <FormField
                label="ID".to_string()
                helptext="snake_case (e.g. os_main, ha_backup). Used by zones to reference this controller. Locked while editing.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="e.g. os_main"
                    prop:value=move || new_id.get()
                    prop:disabled=move || editing_id.get().is_some()
                    on:input=move |ev| new_id.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Kind".to_string()
                helptext="See controllers.md for capabilities of each.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl
                    value=new_kind
                    options=vec![
                        ("opensprinkler_direct".into(), "OpenSprinkler".into()),
                        ("ha_service_call".into(), "HA service call".into()),
                        ("esphome_native".into(), "ESPHome".into()),
                        ("rachio".into(), "Rachio".into()),
                        ("hydrawise".into(), "Hydrawise".into()),
                        ("bhyve".into(), "B-hyve".into()),
                        ("rainbird".into(), "Rain Bird".into()),
                        ("mqtt_command".into(), "MQTT sink".into()),
                        ("dry_run".into(), "DryRun".into()),
                    ]
                    aria_label="Controller kind".to_string()
                />
            </FormField>

            <FormField
                label="Make this the default?".to_string()
                helptext="If checked, other controllers lose default status. Zones without an explicit controller_id use the default.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || new_default.get()
                        on:input=move |ev| new_default.set(event_target_checked(&ev))
                    />
                    "Set as default controller"
                </label>
            </FormField>

            <FormField
                label="Enabled".to_string()
                helptext="Unchecked controllers stay in the config but don't dispatch.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || new_enabled.get()
                        on:input=move |ev| new_enabled.set(event_target_checked(&ev))
                    />
                    "Enable this controller"
                </label>
            </FormField>

            <FormField
                label="Config (JSON)".to_string()
                helptext="Kind-specific configuration. Template auto-fills when Kind changes (only while adding).".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <textarea
                    class="ui-input"
                    style="min-height: 180px; font-family: var(--font-mono); font-size: 0.85rem;"
                    prop:value=move || new_config_text.get()
                    on:input=move |ev| new_config_text.set(event_target_value(&ev))
                ></textarea>
            </FormField>

            {move || {
                let m = scan_msg.get();
                (!m.is_empty()).then(|| view! { <p class="sensors-section__hint">{m}</p> })
            }}

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
                    class="setup-footer__btn setup-footer__btn--ghost"
                    on:click=on_scan
                    title="Probe the controller and list its zones/stations (no save needed)"
                >
                    "Scan zones"
                </button>
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    on:click=on_add
                >
                    {move || if editing_id.get().is_some() { "Save controller changes" } else { "Add to list" }}
                </button>
            </div>
        </Panel></div>
    }
}

/// Reset the controller draft signals back to a blank "new controller"
/// state. Shared by the page's Cancel toggle and the form's post-add
/// cleanup so the two stay in sync.
fn reset_controller_draft(
    editing_id: RwSignal<Option<String>>,
    new_id: RwSignal<String>,
    new_kind: RwSignal<String>,
    new_default: RwSignal<bool>,
    new_enabled: RwSignal<bool>,
    new_config_text: RwSignal<String>,
) {
    editing_id.set(None);
    new_id.set(String::new());
    new_kind.set("opensprinkler_direct".into());
    new_default.set(false);
    new_enabled.set(true);
    new_config_text.set(default_config_text("opensprinkler_direct"));
}

fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "opensprinkler_direct" => "🚿",
        "ha_service_call" => "🏠",
        "esphome_native" => "⚡",
        "rachio" => "☁",
        "hydrawise" => "🌧",
        "bhyve" => "🌀",
        "rainbird" => "🐦",
        "mqtt_command" => "📬",
        "dry_run" => "🧪",
        _ => "💧",
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

fn default_config_text(kind: &str) -> String {
    match kind {
        "opensprinkler_direct" => "{\n  \"host\": \"192.0.2.10\",\n  \"port\": 80,\n  \"password_md5\": \"\",\n  \"poll_interval_s\": 10\n}".into(),
        "ha_service_call" => "{\n  \"base_url\": \"http://homeassistant.local:8123\",\n  \"bearer_token\": \"${HA_LONG_LIVED_TOKEN}\",\n  \"start_service\": \"script.os_zone_toggle\",\n  \"stop_service\": \"opensprinkler.stop\",\n  \"zone_entity_map\": {}\n}".into(),
        "esphome_native" => "{\n  \"host\": \"esp-sprinkler.local\",\n  \"port\": 6053,\n  \"password\": null,\n  \"zone_entity_map\": {}\n}".into(),
        "rachio" => "{\n  \"api_token\": \"\",\n  \"device_id\": \"\",\n  \"zone_uuid_map\": {}\n}".into(),
        "hydrawise" => "{\n  \"api_key\": \"\",\n  \"controller_id\": 0,\n  \"zone_relay_map\": {}\n}".into(),
        "bhyve" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"device_id\": \"\",\n  \"zone_station_map\": {}\n}".into(),
        "rainbird" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"controller_id\": \"\",\n  \"zone_station_map\": {},\n  \"base_url\": \"https://rdz-rest.rainbird.com\"\n}".into(),
        "mqtt_command" => "{\n  \"broker_host\": \"broker.local\",\n  \"broker_port\": 1883,\n  \"username\": null,\n  \"password\": null,\n  \"zone_command_map\": {\n    \"back_yard\": {\n      \"topic\": \"homeassistant/switch/back_yard/set\",\n      \"on_payload\": \"ON\",\n      \"off_payload\": \"OFF\",\n      \"retain\": false\n    }\n  }\n}".into(),
        "dry_run" => "{\n  \"simulate_runs\": false\n}".into(),
        _ => "{}".into(),
    }
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
    new_id: RwSignal<String>,
    new_kind: RwSignal<String>,
    new_default: RwSignal<bool>,
    new_enabled: RwSignal<bool>,
    new_config_text: RwSignal<String>,
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
    let c_for_edit = controller.clone();

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
    let on_edit = move |_| {
        let target = id_for_edit.clone();
        let c = c_for_edit.clone();
        new_id.set(target.clone());
        new_kind.set(
            c.get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("opensprinkler_direct")
                .to_string(),
        );
        new_default.set(c.get("default").and_then(|v| v.as_bool()).unwrap_or(false));
        new_enabled.set(c.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
        let cfg_text = c
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
