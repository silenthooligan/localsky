// SettingsDevices. The Music-Assistant-style device view: every gateway,
// hub, controller, cloud account, and the HA bridge LocalSky knows about,
// each as an expandable card listing the sensors or zones it provides.
//
// Read-only in Phase D (the topology comes from GET /api/v1/devices, derived
// from the configured sources + controllers). Native discovery (E) and HA
// import (F) enrich the same shape; editing lands with the unified device UX
// in Phase G. The card kit + fetch pattern mirror SettingsControllers.

use leptos::prelude::*;
use serde::Deserialize;

use crate::components::settings_ui::{BadgeTone, SettingsBadge, SettingsCard};
use crate::components::ui::Panel;

/// Frontend mirror of `crate::devices::Device` (the SSR type isn't available
/// to the hydrate build, which is the established split). `kind` and `origin`
/// arrive as the snake_case strings the API serializes.
#[derive(Clone, Debug, Deserialize)]
struct Device {
    id: String,
    kind: String,
    name: String,
    #[serde(default)]
    model: Option<String>,
    origin: String,
    #[serde(default)]
    online: Option<bool>,
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

/// Frontend mirror of `crate::discovery::DiscoveredGateway`.
#[derive(Clone, Debug, Deserialize)]
struct DiscoveredGateway {
    vendor: String,
    mac: String,
    ip: String,
    model: String,
    suggested_host: String,
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

/// Glyph for a device kind. Plain strings so the card kit can render them
/// without an icon font dependency.
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "weather_gateway" => "📡",
        "weather_cloud" => "☁",
        "irrigation_controller" => "💧",
        "ha_bridge" => "🏠",
        _ => "⚙",
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

#[component]
pub fn SettingsDevices() -> impl IntoView {
    let devices: RwSignal<Vec<Device>> = RwSignal::new(Vec::new());
    let loaded = RwSignal::new(false);
    let error = RwSignal::new(String::new());

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_devices().await {
                    Ok(list) => devices.set(list),
                    Err(e) => error.set(e),
                }
                loaded.set(true);
            });
        });
    }

    let cards = move || {
        let list = devices.get();
        if list.is_empty() {
            let msg = if !loaded.get() {
                "Loading devices…"
            } else if !error.get().is_empty() {
                "Could not load devices."
            } else {
                "No devices configured yet. Add a weather source or controller in Settings."
            };
            return view! { <p class="settings-empty">{msg}</p> }.into_any();
        }
        let items: Vec<_> = list.into_iter().map(device_card).collect();
        view! { <ul class="settings-card-list">{items}</ul> }.into_any()
    };

    // Native LAN discovery (E2). Found gateways aren't configured sources
    // yet; the card shows the suggested host to add under Weather sources.
    let discovered: RwSignal<Vec<DiscoveredGateway>> = RwSignal::new(Vec::new());
    let discovering = RwSignal::new(false);
    let discovered_once = RwSignal::new(false);
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

    let discovery_results = move || {
        let list = discovered.get();
        if list.is_empty() {
            if discovered_once.get() && !discovering.get() {
                return view! { <p class="settings-empty">"No gateways found on the LAN."</p> }
                    .into_any();
            }
            return view! {}.into_any();
        }
        let rows: Vec<_> = list.into_iter().map(discovered_card).collect();
        view! { <ul class="settings-card-list">{rows}</ul> }.into_any()
    };

    view! {
        <Panel>
            <div class="settings-section-head">
                <h2 class="settings-section-head__title">"Devices"</h2>
                <p class="settings-section-head__sub">
                    "Every gateway, controller, and service LocalSky knows about, with the sensors or zones it provides. Home Assistant mirroring lands in an upcoming release."
                </p>
            </div>
            {cards}
            <div class="settings-section-head" style="margin-top:var(--space-5)">
                <h2 class="settings-section-head__title">"Discover on your network"</h2>
                <p class="settings-section-head__sub">
                    "Broadcast for Ecowitt gateways on every attached subnet. Found gateways can be added under Weather sources as an Ecowitt gateway (poll)."
                </p>
                <button
                    class="setup-footer__btn setup-footer__btn--primary"
                    type="button"
                    on:click=on_discover
                    disabled=move || discovering.get()
                >
                    {move || if discovering.get() { "Scanning…" } else { "Discover gateways" }}
                </button>
            </div>
            {discovery_results}
        </Panel>
    }
}

/// A discovered (not-yet-configured) gateway: model, IP, MAC, and the host
/// to add under Weather sources.
fn discovered_card(gw: DiscoveredGateway) -> impl IntoView {
    let title = if gw.model.is_empty() {
        format!("{} gateway", gw.vendor)
    } else {
        gw.model.clone()
    };
    let subtitle = format!("{} · {}", gw.ip, gw.mac);
    let host = gw.suggested_host.clone();
    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon="📡".to_string()
                title=title
                subtitle=subtitle
                badges=Box::new(move || {
                    view! { <SettingsBadge label="Discovered".into() tone=BadgeTone::Accent/> }
                        .into_any()
                })
                details=Box::new(move || {
                    view! {
                        <p class="device-child-empty">
                            "Add under Settings -> Weather sources as kind "
                            <code>"ecowitt_gw_poll"</code>" with host "<code>{host.clone()}</code>"."
                        </p>
                    }
                    .into_any()
                })
                actions=Box::new(move || view! {}.into_any())
            />
        </li>
    }
}

/// One device as an expandable card: kind glyph + name, origin/online/child
/// badges, and a child list (sensors with their role, or zones).
fn device_card(dev: Device) -> impl IntoView {
    let icon = kind_icon(&dev.kind).to_string();
    let title = dev.name.clone();
    let subtitle = match &dev.model {
        Some(m) => format!("{m} · {}", kind_label(&dev.kind)),
        None => kind_label(&dev.kind).to_string(),
    };

    let origin = dev.origin.clone();
    let online = dev.online;
    let child_count = dev.children.len();

    // Child rows: sensors show their role, zones are tagged. Built into a
    // flat Vec to keep the view! nesting shallow.
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
                        None => view! {}.into_any(),
                    };
                    view! {
                        {origin_badge}
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
                actions=Box::new(move || view! {}.into_any())
            />
        </li>
    }
}
