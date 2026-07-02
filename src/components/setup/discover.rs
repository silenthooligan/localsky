// NetworkScan. The wizard's "find my hardware" panel, embedded in the
// Sources step (Tempest + Ecowitt) and the Controllers step
// (OpenSprinkler). One button fires GET /api/wizard/discover (passive
// Tempest + Ecowitt UDP broadcast + OpenSprinkler /24 sweep); each find
// renders with a one-click Add that writes a prefilled entry into the
// shared wizard draft and persists it.

use leptos::prelude::*;

use crate::components::ui::{Button, Icon};

#[cfg(feature = "hydrate")]
async fn save_draft(draft: serde_json::Value) {
    let _ = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&draft)
        .map(|r| r.send())
        .ok();
}

/// Insert a source entry into the draft (id-keyed upsert).
fn add_source(draft: RwSignal<serde_json::Value>, entry: serde_json::Value) {
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
    #[cfg(feature = "hydrate")]
    {
        let candidate = draft.get_untracked();
        leptos::task::spawn_local(async move {
            save_draft(candidate).await;
        });
    }
}

/// Insert a controller entry into the draft (id-keyed upsert).
fn add_controller(draft: RwSignal<serde_json::Value>, entry: serde_json::Value) {
    let id = entry
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    draft.update(|d| {
        if let Some(arr) = d
            .get_mut("config")
            .and_then(|c| c.get_mut("controllers"))
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
    #[cfg(feature = "hydrate")]
    {
        let candidate = draft.get_untracked();
        leptos::task::spawn_local(async move {
            save_draft(candidate).await;
        });
    }
}

/// mode: "sources" (Tempest + Ecowitt) | "controllers" (OpenSprinkler).
#[component]
pub fn NetworkScan(mode: &'static str, draft: RwSignal<serde_json::Value>) -> impl IntoView {
    let scanning = RwSignal::new(false);
    let scanned = RwSignal::new(false);
    let result: RwSignal<serde_json::Value> = RwSignal::new(serde_json::Value::Null);

    let on_scan = move |_| {
        if scanning.get_untracked() {
            return;
        }
        scanning.set(true);
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/wizard/discover")
                .send()
                .await
            {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    result.set(v);
                }
            }
            scanning.set(false);
            scanned.set(true);
        });
        #[cfg(not(feature = "hydrate"))]
        scanning.set(false);
    };

    let findings = move || {
        let v = result.get();
        if v.is_null() {
            return ().into_any();
        }
        let mut rows: Vec<leptos::prelude::AnyView> = Vec::new();

        if mode == "sources" {
            // Tempest: passive (already broadcasting = already found).
            let tempest_detected = v
                .pointer("/tempest/detected")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            if tempest_detected {
                let hub = v
                    .pointer("/tempest/hub_serial")
                    .and_then(|h| h.as_str())
                    .unwrap_or("hub")
                    .to_string();
                rows.push(view! {
                    <div class="scan-row">
                        <span class="scan-row__icon"><Icon name="wind" size=18/></span>
                        <span class="scan-row__text">
                            <strong>{format!("Tempest station ({hub})")}</strong>
                            <span>"broadcasting on UDP 50222 right now"</span>
                        </span>
                        <Button variant="primary"
                            on_click=Callback::new(move |_| add_source(draft, serde_json::json!({
                                "id": "tempest_lan",
                                "priority": 100,
                                "enabled": true,
                                "kind": "tempest_udp",
                                "config": {},
                            })))
                        >"Add"</Button>
                    </div>
                }.into_any());
            }
            // Ecowitt gateways.
            if let Some(gws) = v.get("ecowitt").and_then(|e| e.as_array()) {
                for gw in gws {
                    let model = gw
                        .get("model")
                        .and_then(|m| m.as_str())
                        .unwrap_or("Ecowitt")
                        .to_string();
                    let ip = gw
                        .get("ip")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let host = gw
                        .get("suggested_host")
                        .and_then(|h| h.as_str())
                        .unwrap_or(&ip)
                        .to_string();
                    let label = format!("{model} at {ip}");
                    // soil_channels (#[serde(default)] u32) is the live soil-probe
                    // channel count the backend detected via get_livedata_info at
                    // scan time; 0 = weather-only, N>0 = a soil-bearing gateway.
                    let soil_channels = gw
                        .get("soil_channels")
                        .and_then(|s| s.as_u64())
                        .unwrap_or(0);
                    let has_soil = soil_channels > 0;
                    let icon_name = if has_soil { "droplet" } else { "sources" };
                    let subtitle = if has_soil {
                        format!("Weather station + {soil_channels} soil probes")
                    } else {
                        "local gateway poll (no cloud)".to_string()
                    };
                    rows.push(view! {
                        <div class="scan-row">
                            <span class="scan-row__icon"><Icon name=icon_name size=18/></span>
                            <span class="scan-row__text">
                                <strong>{label}</strong>
                                {has_soil.then(|| view! {
                                    <span class="source-caps__badge">"Weather + soil"</span>
                                })}
                                <span>{subtitle}</span>
                                {has_soil.then(|| view! {
                                    <span>"soil probes appear in the Sensors step"</span>
                                })}
                            </span>
                            <Button variant="primary"
                                on_click=Callback::new(move |_| add_source(draft, serde_json::json!({
                                    "id": "ecowitt_gw",
                                    "priority": 90,
                                    "enabled": true,
                                    "kind": "ecowitt_gw_poll",
                                    "config": { "host": host.clone() },
                                })))
                            >"Add"</Button>
                        </div>
                    }.into_any());
                }
            }
        }

        if mode == "controllers" {
            if let Some(oss) = v.get("opensprinkler").and_then(|o| o.as_array()) {
                for os in oss {
                    let ip = os
                        .get("ip")
                        .and_then(|i| i.as_str())
                        .unwrap_or("")
                        .to_string();
                    let port = os.get("port").and_then(|p| p.as_u64()).unwrap_or(8080);
                    let fw = os
                        .get("firmware")
                        .and_then(|f| f.as_str())
                        .map(|f| format!("firmware {f}"))
                        .unwrap_or_else(|| "password required".to_string());
                    let label = format!("OpenSprinkler at {ip}:{port}");
                    let ip2 = ip.clone();
                    rows.push(view! {
                        <div class="scan-row">
                            <span class="scan-row__icon"><Icon name="droplet" size=18/></span>
                            <span class="scan-row__text">
                                <strong>{label}</strong>
                                <span>{fw}{" · set the device password, then Test + Scan zones"}</span>
                            </span>
                            <Button variant="primary"
                                on_click=Callback::new(move |_| add_controller(draft, serde_json::json!({
                                    "id": "opensprinkler",
                                    "default": true,
                                    "enabled": true,
                                    "kind": "opensprinkler_direct",
                                    "config": {
                                        "host": ip2.clone(),
                                        "port": port,
                                        "password_md5": "",
                                        "poll_interval_s": 10,
                                    },
                                })))
                            >"Add"</Button>
                        </div>
                    }.into_any());
                }
            }
        }

        if rows.is_empty() && scanned.get() {
            rows.push(view! {
                <p class="sensors-section__hint" style="margin:0">
                    {if mode == "sources" {
                        "Nothing answered on this network segment. Gateways on another subnet need their IP entered manually below."
                    } else {
                        "No OpenSprinkler answered on this network segment. Cloud controllers (Rachio, B-hyve, Hydrawise, Rain Bird) are added manually below with their API credentials."
                    }}
                </p>
            }.into_any());
        }

        view! { <div class="scan-results">{rows}</div> }.into_any()
    };

    view! {
        <div class="scan-panel">
            // variant="secondary" gives the scan a bordered surface at rest so it
            // reads as a button (its ghost form was transparent and looked like
            // helper text); the search icon already signals "find hardware".
            <Button
                variant="secondary"
                class="scan-panel__btn"
                icon="search"
                disabled=Signal::derive(move || scanning.get())
                on_click=Callback::new(on_scan)
            >
                {move || if scanning.get() { "Scanning the network…" } else { "Scan my network" }}
            </Button>
            {findings}
        </div>
    }
}
