// SettingsDevices, the Music-Assistant-style unified device hub. Every
// gateway, hub, controller, cloud account, and HA bridge LocalSky knows about,
// each an expandable card. Native devices (sources + controllers) are editable
// in place via the shared SourceEditorPanel / ControllerEditorPanel; HA-origin
// devices are read-only (managed in Home Assistant). Discovered LAN gateways
// can be adopted as a source with one click. The same hub is deep-linkable:
//   /settings/devices?add=source            open the add-source form
//   /settings/devices?add=controller        open the add-controller form
//   /settings/devices?adopt=<host>          adopt a discovered gateway as a source
//   /settings/devices?discover=1            auto-run LAN discovery
// so contextual "+ Add" buttons elsewhere (zone editor, sensors page) can land
// the user straight in the right form.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
#[cfg(feature = "hydrate")]
use leptos_router::hooks::use_query_map;
use serde::Deserialize;

use crate::components::controllers_form::ControllerEditorPanel;
use crate::components::settings_ui::{BadgeTone, SettingsBadge, SettingsCard};
use crate::components::sources_form::SourceEditorPanel;
use crate::components::ui::{HelpHint, Panel};

/// Frontend mirror of `crate::devices::Device`. `kind` and `origin` arrive as
/// the snake_case strings the API serializes; `source_id` backlinks a native
/// device to its editable config entry.
#[derive(Clone, Debug, Deserialize)]
struct Device {
    id: String,
    kind: String,
    name: String,
    #[serde(default)]
    model: Option<String>,
    origin: String,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    online: Option<bool>,
    #[serde(default)]
    also_in_ha: bool,
    #[serde(default)]
    children: Vec<DeviceChild>,
}

/// Mirror of `crate::devices::DeviceChild` with the flattened child-kind tag.
#[derive(Clone, Debug, Deserialize)]
struct DeviceChild {
    label: String,
    /// "sensor" | "zone" (the DeviceChildKind tag).
    #[serde(rename = "type")]
    child_type: String,
    #[serde(default)]
    role: Option<String>,
}

/// Frontend mirror of `crate::discovery::DiscoveredGateway`.
#[derive(Clone, Debug, Deserialize)]
struct DiscoveredGateway {
    vendor: String,
    mac: String,
    ip: String,
    model: String,
    suggested_host: String,
}

/// What the detail pane is showing. List is the default browse view; the rest
/// are the in-place add/edit forms.
#[derive(Clone, PartialEq)]
enum Sel {
    List,
    AddSource,
    EditSource(serde_json::Value),
    AdoptGateway(serde_json::Value),
    AddController,
    EditController(serde_json::Value),
}

#[cfg(feature = "hydrate")]
async fn fetch_devices() -> Result<Vec<Device>, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/v1/devices")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Vec<Device>>().await.map_err(|e| e.to_string())
}

#[cfg(feature = "hydrate")]
async fn fetch_discover() -> Result<Vec<DiscoveredGateway>, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/v1/devices/discover")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Vec<DiscoveredGateway>>()
        .await
        .map_err(|e| e.to_string())
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

/// Glyph for a device kind.
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "weather_gateway" => "sources",
        "weather_cloud" => "cloud",
        "irrigation_controller" => "droplet",
        "ha_bridge" => "home",
        _ => "settings",
    }
}

/// Human label for a device kind (card subtitle).
fn kind_label(kind: &str) -> &'static str {
    match kind {
        "weather_gateway" => "LAN weather gateway",
        "weather_cloud" => "Cloud weather service",
        "irrigation_controller" => "Irrigation controller",
        "ha_bridge" => "Home Assistant bridge",
        _ => "Ingest endpoint",
    }
}

/// Look up a config array entry (`sources` or `controllers`) by id.
fn find_entry(config: &serde_json::Value, array: &str, id: &str) -> Option<serde_json::Value> {
    config
        .get(array)
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id))
                .cloned()
        })
}

#[component]
pub fn SettingsDevices() -> impl IntoView {
    let devices: RwSignal<Vec<Device>> = RwSignal::new(Vec::new());
    let config: RwSignal<serde_json::Value> = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);
    let error = RwSignal::new(String::new());
    let sel: RwSignal<Sel> = RwSignal::new(Sel::List);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    let discovered: RwSignal<Vec<DiscoveredGateway>> = RwSignal::new(Vec::new());
    let discovering = RwSignal::new(false);
    let discovered_once = RwSignal::new(false);

    // Called only from the hydrate-gated effects below; SSR builds never
    // invoke it.
    #[allow(unused_variables)]
    let refresh = move || {
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            match fetch_devices().await {
                Ok(list) => devices.set(list),
                Err(e) => error.set(e),
            }
            if let Ok(cfg) = fetch_config().await {
                config.set(cfg);
            }
            loaded.set(true);
        });
    };

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            refresh();
        });
        // Deep-link handling: open a form / run discovery from query params.
        let q = use_query_map();
        Effect::new(move |_| {
            let q = q.get();
            if let Some(add) = q.get("add") {
                match add.as_str() {
                    "source" => sel.set(Sel::AddSource),
                    "controller" => sel.set(Sel::AddController),
                    _ => {}
                }
            } else if let Some(host) = q.get("adopt") {
                sel.set(Sel::AdoptGateway(serde_json::json!({
                    "kind": "ecowitt_gw_poll",
                    "config": { "host": host.to_string(), "poll_interval_s": 30 }
                })));
            }
        });
    }

    // Persist a committed source/controller entry: merge into the right config
    // array (controllers get default-exclusivity), PUT, then refresh devices.
    let persist_entry = Callback::new(move |entry: serde_json::Value| {
        let is_controller = entry.get("default").is_some();
        let array = if is_controller {
            "controllers"
        } else {
            "sources"
        };
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let mut cfg = config.get();
        if !cfg.is_object() {
            cfg = serde_json::json!({});
        }
        if is_controller && entry.get("default").and_then(|v| v.as_bool()) == Some(true) {
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
            o.entry(array)
                .or_insert(serde_json::json!([]))
                .as_array_mut()
        }) {
            if let Some(slot) = arr
                .iter_mut()
                .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
            {
                *slot = entry;
            } else {
                arr.push(entry);
            }
        }
        config.set(cfg.clone());
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            match save_config(cfg).await {
                Ok(()) => {
                    crate::components::settings_ui::toast_saved(
                        result_msg,
                        result_ok,
                        "Saved. Registry hot-reloads shortly.",
                    );
                    sel.set(Sel::List);
                    // give the registry a beat to reload, then refresh the list
                    match fetch_devices().await {
                        Ok(list) => devices.set(list),
                        Err(e) => error.set(e),
                    }
                }
                Err(e) => {
                    result_ok.set(false);
                    result_msg.set(e);
                }
            }
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = cfg;
    });

    let on_cancel = Callback::new(move |()| sel.set(Sel::List));

    // Edit dispatch from a device card: resolve the device id to its config
    // entry and open the right editor. Card only calls this for editable cards.
    let on_edit = Callback::new(move |dev_id: String| {
        let cfg = config.get();
        if let Some(rest) = dev_id.strip_prefix("source:") {
            if let Some(entry) = find_entry(&cfg, "sources", rest) {
                sel.set(Sel::EditSource(entry));
            }
        } else if let Some(rest) = dev_id.strip_prefix("controller:") {
            if let Some(entry) = find_entry(&cfg, "controllers", rest) {
                sel.set(Sel::EditController(entry));
            }
        }
    });

    let on_adopt = Callback::new(move |host: String| {
        sel.set(Sel::AdoptGateway(serde_json::json!({
            "kind": "ecowitt_gw_poll",
            "config": { "host": host, "poll_interval_s": 30 }
        })));
    });

    let on_discover = move |_| {
        #[cfg(feature = "hydrate")]
        {
            discovering.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(list) = fetch_discover().await {
                    discovered.set(list);
                }
                discovering.set(false);
                discovered_once.set(true);
            });
        }
    };

    let cards = move || {
        let list = devices.get();
        if list.is_empty() {
            let msg = if !loaded.get() {
                "Loading devices…"
            } else if !error.get().is_empty() {
                "Could not load devices."
            } else {
                "No devices yet. Add a weather source or controller below, or scan your network."
            };
            return view! { <p class="settings-empty">{msg}</p> }.into_any();
        }
        let items: Vec<_> = list.into_iter().map(|d| device_card(d, on_edit)).collect();
        view! { <ul class="settings-card-list">{items}</ul> }.into_any()
    };

    let discovery_results = move || {
        let list = discovered.get();
        if list.is_empty() {
            if discovered_once.get() && !discovering.get() {
                return view! { <p class="settings-empty">"No gateways found on the LAN."</p> }
                    .into_any();
            }
            return {
                let _: () = view! {};
                ().into_any()
            };
        }
        let rows: Vec<_> = list
            .into_iter()
            .map(|gw| discovered_card(gw, on_adopt))
            .collect();
        view! { <ul class="settings-card-list">{rows}</ul> }.into_any()
    };

    // The editor pane (shown when sel != List), else the browse list.
    let detail = move || {
        match sel.get() {
        Sel::AddSource => view! {
            <SourceEditorPanel on_commit=persist_entry on_cancel=on_cancel/>
        }
        .into_any(),
        Sel::EditSource(entry) => view! {
            <SourceEditorPanel existing=Some(entry) on_commit=persist_entry on_cancel=on_cancel/>
        }
        .into_any(),
        Sel::AdoptGateway(prefill) => view! {
            <SourceEditorPanel existing=Some(prefill) on_commit=persist_entry on_cancel=on_cancel/>
        }
        .into_any(),
        Sel::AddController => view! {
            <ControllerEditorPanel on_commit=persist_entry on_cancel=on_cancel/>
        }
        .into_any(),
        Sel::EditController(entry) => view! {
            <ControllerEditorPanel existing=Some(entry) on_commit=persist_entry on_cancel=on_cancel/>
        }
        .into_any(),
        Sel::List => view! {
            <Panel>
                <div class="settings-section-head">
                    <h2 class="settings-section-head__title">"Devices"<HelpHint topic="devices"/></h2>
                    <p class="settings-section-head__sub">
                        "Every gateway, controller, sensor, and service LocalSky uses, from both its own sources and Home Assistant. Native devices are editable here; Home Assistant devices are managed in HA and mirror in automatically."
                    </p>
                    <p class="settings-section-head__sub">
                        "Three tiers: "<strong>"controllers"</strong>" open valves, "
                        <strong>"sources"</strong>" (a weather station, an Ecowitt gateway, a "
                        "forecast, an MQTT broker, or Home Assistant) bring data in, and "
                        <strong>"sensors"</strong>" are the probes and meters those carry. Add a "
                        "source here and its sensors appear under Settings, Sensors. "
                        <a href=crate::docs::doc_url("first-soil-sensor")
                            target="_blank" rel="noopener noreferrer"
                            style="color: var(--accent)">
                            "Add your first soil sensor →"
                        </a>
                    </p>
                </div>
                <div class="device-add-bar">
                    <span class="device-add-bar__label">"Add a device"</span>
                    <button class="setup-footer__btn setup-footer__btn--primary" type="button"
                        on:click=move |_| sel.set(Sel::AddSource)>"Weather source"</button>
                    <button class="setup-footer__btn setup-footer__btn--primary" type="button"
                        on:click=move |_| sel.set(Sel::AddController)>"Controller"</button>
                    <button class="setup-footer__btn setup-footer__btn--ghost" type="button"
                        on:click=on_discover disabled=move || discovering.get()>
                        {move || if discovering.get() { "Scanning network…" } else { "Scan network" }}
                    </button>
                </div>
                <p class="settings-section-head__sub" style="margin:-0.25rem 0 var(--space-4)">
                    "Add a weather station, gateway, or cloud service, wire up an irrigation controller, or scan the LAN for supported gateways to adopt."
                </p>
                {move || {
                    let m = result_msg.get();
                    (!m.is_empty()).then(|| {
                        let cls = if result_ok.get() { "settings-result settings-result--ok" } else { "settings-result settings-result--err" };
                        view! { <p class=cls>{m}</p> }
                    })
                }}
                {discovery_results}
                {cards}
            </Panel>
        }
        .into_any(),
    }
    };

    // Constrain to the settings content width so the card list stays
    // mouse/eye-friendly on ultrawide displays (matches the other settings
    // sections, which wrap in .settings-page).
    view! { <div class="settings-page">{detail}</div> }
}

/// A discovered (not-yet-configured) gateway: model, IP, MAC, and an Adopt
/// action that opens the add-source form prefilled.
fn discovered_card(gw: DiscoveredGateway, on_adopt: Callback<String>) -> impl IntoView {
    let title = if gw.model.is_empty() {
        format!("{} gateway", gw.vendor)
    } else {
        gw.model.clone()
    };
    let subtitle = format!("{} · {}", gw.ip, gw.mac);
    let host = gw.suggested_host.clone();
    let host_detail = host.clone();
    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon="sources".to_string()
                title=title
                subtitle=subtitle
                badges=Box::new(move || {
                    view! { <SettingsBadge label="Discovered".into() tone=BadgeTone::Accent/> }
                        .into_any()
                })
                details=Box::new(move || {
                    view! {
                        <p class="device-child-empty">
                            "Ecowitt gateway at "<code>{host_detail.clone()}</code>". Adopt it to poll soil + weather natively."
                        </p>
                    }
                    .into_any()
                })
                actions=Box::new(move || {
                    let h = host.clone();
                    view! {
                        <button class="setup-footer__btn setup-footer__btn--primary" type="button"
                            on:click=move |_| on_adopt.run(h.clone())>"Adopt as source"</button>
                    }
                    .into_any()
                })
            />
        </li>
    }
}

/// One device as an expandable card. Native devices with a config backlink get
/// an Edit action; HA-origin and the HA bridge are read-only.
fn device_card(dev: Device, on_edit: Callback<String>) -> impl IntoView {
    let icon = kind_icon(&dev.kind).to_string();
    let title = dev.name.clone();
    let subtitle = match &dev.model {
        Some(m) => format!("{m} · {}", kind_label(&dev.kind)),
        None => kind_label(&dev.kind).to_string(),
    };

    let origin = dev.origin.clone();
    let online = dev.online;
    let also_in_ha = dev.also_in_ha;
    let child_count = dev.children.len();
    let editable = dev.origin == "native" && dev.source_id.is_some() && dev.kind != "ha_bridge";
    let edit_id = dev.id.clone();

    let child_rows: Vec<_> = dev
        .children
        .iter()
        .map(|c| {
            let meta = if c.child_type == "zone" {
                "zone".to_string()
            } else {
                c.role.clone().unwrap_or_else(|| "sensor".to_string())
            };
            view! {
                <li class="device-child">
                    <span class="device-child__label">{c.label.clone()}</span>
                    <span class="device-child__meta">{meta}</span>
                </li>
            }
        })
        .collect();

    let details_empty = child_rows.is_empty();
    let is_ha = origin == "home_assistant";

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon=icon
                title=title
                subtitle=subtitle
                badges=Box::new(move || {
                    let origin_badge = if origin == "home_assistant" {
                        view! { <SettingsBadge label="Home Assistant".into() tone=BadgeTone::Accent/> }
                            .into_any()
                    } else {
                        view! { <SettingsBadge label="Native".into() tone=BadgeTone::Good/> }
                            .into_any()
                    };
                    let online_badge = match online {
                        Some(true) => {
                            view! { <SettingsBadge label="Online".into() tone=BadgeTone::Good/> }
                                .into_any()
                        }
                        Some(false) => {
                            view! { <SettingsBadge label="Offline".into() tone=BadgeTone::Danger/> }
                                .into_any()
                        }
                        None => {
                            let _: () = view! {};
                            ().into_any()
                        },
                    };
                    let mirror_badge = if also_in_ha {
                        view! { <SettingsBadge label="+ HA".into() tone=BadgeTone::Accent/> }
                            .into_any()
                    } else {
                        let _: () = view! {};
                        ().into_any()
                    };
                    view! {
                        {origin_badge}
                        {mirror_badge}
                        {online_badge}
                        <SettingsBadge
                            label=format!("{child_count} item{}", if child_count == 1 { "" } else { "s" })
                            tone=BadgeTone::Muted
                        />
                    }
                    .into_any()
                })
                details=Box::new(move || {
                    if details_empty {
                        view! { <p class="device-child-empty">"No sensors or zones listed yet."</p> }
                            .into_any()
                    } else {
                        view! { <ul class="device-child-list">{child_rows}</ul> }.into_any()
                    }
                })
                actions=Box::new(move || {
                    if editable {
                        let id = edit_id.clone();
                        view! {
                            <button class="setup-footer__btn setup-footer__btn--ghost" type="button"
                                on:click=move |_| on_edit.run(id.clone())>"Edit"</button>
                        }
                        .into_any()
                    } else if is_ha {
                        view! { <span class="device-child-empty">"Managed in Home Assistant"</span> }
                            .into_any()
                    } else {
                        let _: () = view! {};
                        ().into_any()
                    }
                })
            />
        </li>
    }
}
