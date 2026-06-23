// ControllerEditorPanel: a self-contained add/edit form for one irrigation
// controller, mirroring sources_form::SourceEditorPanel's contract so both the
// Settings -> Controllers page and the unified Devices hub share one form.
//
// Seeds from `existing` (None = add). On save it parses the config JSON,
// assembles the `{id, default, enabled, kind, config}` entry, and hands it to
// `on_commit`, the caller persists (and is responsible for the `default`
// mutual-exclusivity across controllers). `on_cancel` dismisses the form.
//
// Keeps the "Scan zones" probe (POST /api/v1/wizard/scan_zones) so the user can
// list a controller's stations from the draft config without saving first.

use leptos::prelude::*;

use crate::components::ui::{FormField, Panel, SegmentedControl};
use crate::docs::doc_url;

mod fields;
pub use fields::{controller_fields, ControllerConfigForm};

/// Controller kinds offered in the Kind picker. (value, label) pairs.
///
/// `esphome_native` is intentionally NOT offered: the backend adapter is
/// deferred (runtime::build_controllers warn-skips it), so a saved
/// esphome_native controller would silently water nothing. ESPHome users go
/// through `mqtt_command` (the path the bundled reference firmware implements)
/// or `http_generic`. Keep this list in lockstep with the ControllerKind enum
/// (schema.rs); the `kind_picker_covers_buildable_controllers` test guards it.
pub fn controller_kind_options() -> Vec<(String, String)> {
    vec![
        ("opensprinkler_direct".into(), "OpenSprinkler".into()),
        ("http_generic".into(), "DIY (HTTP)".into()),
        ("mqtt_command".into(), "MQTT sink".into()),
        ("ha_service_call".into(), "HA service call".into()),
        ("rachio".into(), "Rachio".into()),
        ("hydrawise".into(), "Hydrawise".into()),
        ("bhyve".into(), "B-hyve".into()),
        ("rainbird".into(), "Rain Bird".into()),
        ("dry_run".into(), "DryRun".into()),
    ]
}

/// Per-kind starter config JSON, auto-filled while adding.
pub fn default_config_text(kind: &str) -> String {
    match kind {
        "opensprinkler_direct" => "{\n  \"host\": \"192.0.2.10\",\n  \"port\": 80,\n  \"password_md5\": \"\",\n  \"poll_interval_s\": 10\n}".into(),
        "ha_service_call" => "{\n  \"base_url\": \"http://homeassistant.local:8123\",\n  \"bearer_token\": \"${HA_LONG_LIVED_TOKEN}\",\n  \"start_service\": \"script.os_zone_toggle\",\n  \"stop_service\": \"opensprinkler.stop\",\n  \"zone_entity_map\": {}\n}".into(),
        "http_generic" => "{\n  \"base_url\": \"http://192.0.2.50\",\n  \"bearer_token\": null,\n  \"poll_interval_s\": 10\n}".into(),
        "rachio" => "{\n  \"api_token\": \"\",\n  \"device_id\": \"\",\n  \"zone_uuid_map\": {}\n}".into(),
        "hydrawise" => "{\n  \"api_key\": \"\",\n  \"controller_id\": 0,\n  \"zone_relay_map\": {}\n}".into(),
        "bhyve" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"device_id\": \"\",\n  \"zone_station_map\": {}\n}".into(),
        "rainbird" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"controller_id\": \"\",\n  \"zone_station_map\": {},\n  \"base_url\": \"https://rdz-rest.rainbird.com\"\n}".into(),
        "mqtt_command" => "{\n  \"broker_host\": \"broker.local\",\n  \"broker_port\": 1883,\n  \"username\": null,\n  \"password\": null,\n  \"availability_topic\": \"localsky-irrig/status\",\n  \"flow_topic\": null,\n  \"zone_command_map\": {\n    \"back_yard\": {\n      \"topic\": \"localsky-irrig/switch/zone_1/command\",\n      \"on_payload\": \"ON\",\n      \"off_payload\": \"OFF\",\n      \"retain\": false,\n      \"state_topic\": \"localsky-irrig/switch/zone_1/state\",\n      \"state_on_payload\": \"ON\"\n    }\n  }\n}".into(),
        "dry_run" => "{\n  \"simulate_runs\": false\n}".into(),
        _ => "{}".into(),
    }
}

#[component]
pub fn ControllerEditorPanel(
    #[prop(default = None)] existing: Option<serde_json::Value>,
    on_commit: Callback<serde_json::Value>,
    on_cancel: Callback<()>,
) -> impl IntoView {
    let editing = existing.is_some();
    let seed_id = existing
        .as_ref()
        .and_then(|s| s.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let seed_kind = existing
        .as_ref()
        .and_then(|s| s.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("opensprinkler_direct")
        .to_string();
    // A saved controller whose kind is no longer offered (e.g. the deferred
    // esphome_native) can't show a valid selection in the picker and a blind
    // Save would re-commit a non-functional kind that waters nothing. Coerce to
    // a working default so editing forces a conscious re-selection.
    let seed_kind = if controller_kind_options().iter().any(|(v, _)| v == &seed_kind) {
        seed_kind
    } else {
        "mqtt_command".to_string()
    };
    let seed_default = existing
        .as_ref()
        .and_then(|s| s.get("default"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let seed_enabled = existing
        .as_ref()
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let seed_config = existing
        .as_ref()
        .and_then(|s| s.get("config"))
        .map(|c| serde_json::to_string_pretty(c).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| default_config_text(&seed_kind));

    let id = RwSignal::new(seed_id);
    let kind = RwSignal::new(seed_kind);
    let default_flag = RwSignal::new(seed_default);
    let enabled = RwSignal::new(seed_enabled);
    let config_text = RwSignal::new(seed_config);
    let error = RwSignal::new(String::new());
    let scan_msg = RwSignal::new(String::new());

    // While adding, swap the JSON template as the kind changes.
    #[cfg(feature = "hydrate")]
    if !editing {
        Effect::new(move |_| {
            let k = kind.get();
            config_text.set(default_config_text(&k));
        });
    }

    let on_save = move |_| {
        let id_v = id.get().trim().to_string();
        if id_v.is_empty() {
            error.set("Controller id is required".into());
            return;
        }
        let cfg_value: serde_json::Value = match serde_json::from_str(&config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                error.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        error.set(String::new());
        on_commit.run(serde_json::json!({
            "id": id_v,
            "default": default_flag.get(),
            "enabled": enabled.get(),
            "kind": kind.get(),
            "config": cfg_value,
        }));
    };

    let on_scan = move |_| {
        let cfg_value: serde_json::Value = match serde_json::from_str(&config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                scan_msg.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        // Consumed only by the hydrate-gated scan request below.
        #[allow(unused_variables)]
        let entry = serde_json::json!({
            "id": id.get(),
            "default": default_flag.get(),
            "enabled": enabled.get(),
            "kind": kind.get(),
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
                    prop:value=move || id.get()
                    prop:disabled=editing
                    on:input=move |ev| id.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Kind".to_string()
                helptext="Pick the controller backend that fires your valves.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl
                    value=kind
                    options=controller_kind_options()
                    aria_label="Controller kind".to_string()
                />
                <p class="ui-form-field__helptext" style="margin-top: 0.4rem">
                    "See the "
                    <a href=doc_url("controllers")
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">"controller docs"</a>
                    " for the capabilities of each."
                </p>
            </FormField>

            <FormField
                label="Make this the default?".to_string()
                helptext="If checked, other controllers lose default status. Zones without an explicit controller_id use the default.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || default_flag.get()
                        on:input=move |ev| default_flag.set(event_target_checked(&ev))
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
                        prop:checked=move || enabled.get()
                        on:input=move |ev| enabled.set(event_target_checked(&ev))
                    />
                    "Enable this controller"
                </label>
            </FormField>

            // PRIMARY editing surface: labeled, per-kind connection fields
            // (host/url/tokens/poll cadence), two-way-synced to config_text.
            // The JSON textarea below stays as the advanced escape hatch and
            // owns the nested zone maps (populated by "Scan zones").
            <Panel title="Connection".to_string()>
                <ControllerConfigForm config_text=config_text kind=Signal::derive(move || kind.get())/>
            </Panel>

            <FormField
                label="Config (JSON, advanced)".to_string()
                helptext="Escape hatch for keys not in the labeled Connection form above, mainly the per-zone maps (zone_command_map, zone_*_map). Stays in sync both ways; \"Scan zones\" populates the zone map here. Template auto-fills when Kind changes (only while adding).".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <textarea
                    class="ui-input"
                    style="min-height: 180px; font-family: var(--font-mono); font-size: 0.85rem;"
                    prop:value=move || config_text.get()
                    on:input=move |ev| config_text.set(event_target_value(&ev))
                ></textarea>
            </FormField>

            {move || {
                let m = scan_msg.get();
                (!m.is_empty()).then(|| view! { <p class="sensors-section__hint">{m}</p> })
            }}
            {move || {
                let e = error.get();
                (!e.is_empty()).then(|| view! { <p class="source-editor__error">{e}</p> })
            }}

            <div class="settings-form-actions">
                <button type="button" class="setup-footer__btn setup-footer__btn--ghost" on:click=move |_| on_cancel.run(())>
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
                <button type="button" class="setup-footer__btn setup-footer__btn--primary" on:click=on_save>
                    {if editing { "Save controller changes" } else { "Add controller" }}
                </button>
            </div>
        </Panel></div>
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::{controller_kind_options, default_config_text};

    // Root-cause guard for the class of bug where a ControllerKind variant is
    // added to the backend but never surfaced in the UI (exactly how
    // `http_generic` shipped invisible). The sources form has the analogous
    // `coverage_kind_list_matches_default_config_text`; controllers had none.
    #[test]
    fn kind_picker_covers_buildable_controllers() {
        use crate::config::schema::ControllerKind as K;

        // Compile-time guard: adding a ControllerKind variant forces a new arm
        // here, i.e. a conscious "is this in the picker, or intentionally
        // hidden?" decision. The returned tag must match the serde
        // (tag = "kind", rename_all = "snake_case") wire string. Some(tag) =>
        // offered in the picker; None => deliberately hidden.
        fn classify(k: &K) -> Option<&'static str> {
            match k {
                K::OpensprinklerDirect(_) => Some("opensprinkler_direct"),
                K::HaServiceCall(_) => Some("ha_service_call"),
                K::Rachio(_) => Some("rachio"),
                K::Hydrawise(_) => Some("hydrawise"),
                K::Bhyve(_) => Some("bhyve"),
                K::Rainbird(_) => Some("rainbird"),
                K::MqttCommand(_) => Some("mqtt_command"),
                K::HttpGeneric(_) => Some("http_generic"),
                K::DryRun(_) => Some("dry_run"),
                // Deferred backend: build_controllers warn-skips it, so a saved
                // esphome_native controller would silently water nothing. Hidden
                // from the picker on purpose until the adapter is built.
                K::EsphomeNative(_) => None,
            }
        }
        // Referenced so the exhaustive match above is compiled (and thus its
        // E0004 guard enforced) even though we can't enumerate variant instances
        // without samples. NOTE: the runtime assertions below check a fixed
        // allowlist (`expected`), not classify's outputs; classify's job is the
        // compile-time "every variant is consciously handled" guard.
        let _ = classify;

        let picker: std::collections::BTreeSet<String> =
            controller_kind_options().into_iter().map(|(v, _)| v).collect();

        // Every picker entry must have a real (non-empty) starter template.
        // This is the exact invariant http_generic violated: an entry with no
        // template falls through to "{}" and 422s on apply.
        for kind in &picker {
            let t = default_config_text(kind);
            assert!(
                t.starts_with('{') && t != "{}",
                "default_config_text(`{kind}`) must be a real template, got `{t}`"
            );
        }

        // The deferred backend must never be offered.
        assert!(
            !picker.contains("esphome_native"),
            "esphome_native is deferred and must stay out of the kind picker"
        );

        // The buildable kinds we expect to be selectable, and ONLY those.
        let expected = [
            "opensprinkler_direct",
            "http_generic",
            "mqtt_command",
            "ha_service_call",
            "rachio",
            "hydrawise",
            "bhyve",
            "rainbird",
            "dry_run",
        ];
        for kind in expected {
            assert!(
                picker.contains(kind),
                "kind picker is missing buildable controller `{kind}`"
            );
        }
        // Bidirectional: the picker has no surprise extras beyond `expected`
        // (so a typo'd or accidentally-added entry is caught too).
        assert_eq!(
            picker.len(),
            expected.len(),
            "kind picker has unexpected entries: {picker:?}"
        );
    }
}
