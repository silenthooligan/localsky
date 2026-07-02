// SettingsDevices, the Music-Assistant-style unified device hub. Every
// gateway, hub, controller, cloud account, and HA bridge LocalSky knows about,
// each an expandable card. Native devices (sources + controllers) are editable
// in place via the shared SourceEditorPanel / ControllerEditorPanel; HA-origin
// devices are read-only (managed in Home Assistant). Discovered LAN gateways
// can be adopted as a source with one click. The same hub is deep-linkable via
// the settings shell route (which keeps the left section rail):
//   /settings?section=devices&add=source        open the add-source form
//   /settings?section=devices&add=controller    open the add-controller form
//   /settings?section=devices&adopt=<host>       adopt a discovered gateway as a source
//   /settings?section=devices&discover=1         auto-run LAN discovery
// so contextual "+ Add" buttons elsewhere (zone editor, sensors page) can land
// the user straight in the right form.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
#[cfg(feature = "hydrate")]
use leptos_router::hooks::use_query_map;
use serde::Deserialize;

use crate::components::controllers_form::ControllerEditorPanel;
use crate::components::settings::cloud_weather::{cloud_status_word_for_entry, CloudCatalog};
use crate::components::settings_ui::{BadgeTone, EntityKind, SettingsBadge, SettingsCard};
use crate::components::sources_form::SourceEditorPanel;
use crate::components::ui::{Button, HelpHint, Panel, Toggle};

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
    /// The config-entry enabled flag: `Some(true)`/`Some(false)` for a native
    /// source/controller device (a real on/off toggle backs it), `None` for an
    /// HA-imported device that has no LocalSky config entry (no toggle shown).
    #[serde(default)]
    enabled: Option<bool>,
    /// The `SourceKind` serde tag slug (`open_meteo`, `tempest_ws`,
    /// `ecowitt_gw_poll`, ...) for a native SOURCE device, so the card can join
    /// the cloud catalog for a status word. `None` for controllers + HA devices.
    #[serde(default)]
    source_kind: Option<String>,
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

/// GET the cloud source catalog, so a configured cloud weather-source CARD can
/// reuse the panel's exact calm status word (looked up by `source_kind`). A miss
/// (older payload / network error) reads as an empty catalog: the card then falls
/// back to the plain enabled/off word rather than failing.
#[cfg(feature = "hydrate")]
async fn fetch_catalog() -> Result<CloudCatalog, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config/source_catalog")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<CloudCatalog>().await.map_err(|e| e.to_string())
}

/// PUT the merged config. Returns the restart_reasons the PUT response carried
/// (empty when the change hot-reloaded). Adding a brand-new source/controller
/// that needs a boot-wired connection (a Tempest listener, an OpenSprinkler
/// poll loop, an Open-Meteo refresher) flags restart_required=true with the
/// reasons; the caller raises the shared RestartBanner. A missing/old field
/// reads as "no restart", the safe default.
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

/// True when a device `kind` string is a LOCAL weather source: a real station or
/// hub on the user's network, not a cloud service. `classify_source`
/// (devices/builder.rs:86) folds EVERY LAN station family (Tempest udp/ws,
/// Ecowitt local + gateway-poll, Davis WLL, YoLink, LaCrosse) into the single
/// `DeviceKind::WeatherGateway` -> wire string "weather_gateway", so that one
/// string is the local-hardware signal. "weather_cloud" is deliberately EXCLUDED
/// (it is a cloud service, never local hardware), which is the whole fix: a
/// station owner must read as having local hardware so the panel leads local, not
/// cloud-first. Kept as its own predicate (not an inline `== "weather_gateway"`)
/// so the local-vs-cloud intent is explicit and any future local source kind has
/// one place to land.
fn is_local_weather_source(kind: &str) -> bool {
    kind == "weather_gateway"
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

/// The ids of every source EXCEPT `exclude_id`, so the editor can reject a
/// rename that collides with a sibling before it corrupts anything.
fn ids_except(config: &serde_json::Value, array: &str, exclude_id: &str) -> Vec<String> {
    config
        .get(array)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
                .filter(|id| *id != exclude_id)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[component]
pub fn SettingsDevices() -> impl IntoView {
    let devices: RwSignal<Vec<Device>> = RwSignal::new(Vec::new());
    let config: RwSignal<serde_json::Value> = RwSignal::new(serde_json::Value::Null);
    // The cloud source catalog, joined by `source_kind` so a configured cloud
    // weather-source card reuses the panel's exact calm status word. A native local
    // station carries no catalog entry (it reads the plain "On, measuring" word),
    // so an empty catalog is a safe default. Refetched alongside devices after every
    // toggle/remove so a removed/disabled source stops rendering everywhere.
    let catalog: RwSignal<CloudCatalog> = RwSignal::new(CloudCatalog::default());
    // Bumped after a device-card toggle/remove so the CloudWeatherServices panel
    // (which owns its OWN catalog signal for the matrix + discovery) refetches;
    // otherwise a removed/disabled source would stay stale in that panel.
    let panel_reload = RwSignal::new(0u32);
    let loaded = RwSignal::new(false);
    let error = RwSignal::new(String::new());
    let sel: RwSignal<Sel> = RwSignal::new(Sel::List);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    // Persistent, dismissible restart-required banner. Populated only when a
    // save returns restart_required=true (a newly added source/controller that
    // needs a boot-wired connection); routine edits hot-reload and leave it
    // empty. `restart_dismissed` hides it after the user acknowledges.
    let restart_reasons: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    let restart_dismissed = RwSignal::new(false);

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
            if let Ok(cat) = fetch_catalog().await {
                catalog.set(cat);
            }
            loaded.set(true);
        });
    };

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            refresh();
        });
        // Deep-link handling from query params: ?add=source|controller opens the
        // matching add form, ?adopt=<host> opens the adopt-gateway form, and
        // ?discover=1 kicks off the LAN scan on mount (so a "find my gateway"
        // deep-link lands the user with discovery already running, no extra
        // click). Each branch is exclusive; a malformed value is a no-op, never a
        // dead-end.
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
            } else if q.get("discover").is_some_and(|d| d == "1") {
                // Same scan as the "Scan the network" button (on_discover); run it
                // once on mount so the deep-link is live, not inert. Guarded so a
                // re-run while a scan is in flight does not double-fire.
                if !discovering.get_untracked() {
                    discovering.set(true);
                    wasm_bindgen_futures::spawn_local(async move {
                        if let Ok(list) = fetch_discover().await {
                            discovered.set(list);
                        }
                        discovering.set(false);
                        discovered_once.set(true);
                    });
                }
            }
        });
    }

    // Persist a committed source/controller entry: merge into the right config
    // array (controllers get default-exclusivity), PUT, then refresh devices.
    let persist_entry = Callback::new(move |mut entry: serde_json::Value| {
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
        // A rename carries the previous id (from the source editor) so we replace
        // the right slot and migrate references to it. Strip it before storing.
        let old_id = entry
            .as_object_mut()
            .and_then(|o| o.remove("old_id"))
            .and_then(|v| v.as_str().map(str::to_string))
            .filter(|s| !s.is_empty() && *s != id);
        // The slot to overwrite: the OLD id on a rename, otherwise the current id.
        let match_id = old_id.clone().unwrap_or_else(|| id.clone());
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
                .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(match_id.as_str()))
            {
                *slot = entry;
            } else {
                arr.push(entry);
            }
        }
        // Rename: repoint every reference to the old id at the new id so a rename
        // never orphans anything. A SOURCE rename repoints per-reading picks, the
        // forecast source pin, and zone soil bindings. A CONTROLLER rename repoints
        // every zone whose controller_id named the old id (the sole config
        // reference to a controller id), so no zone is left firing from a gone
        // controller (which would trip zone controller_id_invalid).
        if let Some(old) = &old_id {
            if is_controller {
                migrate_controller_id_refs(&mut cfg, old, &id);
            } else {
                migrate_source_id_refs(&mut cfg, old, &id);
            }
        }
        config.set(cfg.clone());
        result_msg.set(String::new());
        // The PUT body is the same config plus a TRANSIENT rename hint. On a
        // rename the entry now lives under the NEW id, but its redacted secrets
        // ("***redacted***" from the GET) are only restorable from the OLD stored
        // id, so the server needs the mapping to unredact them; without it the
        // sentinel survives and the PUT 400s. The hint is stripped server-side
        // before validate/persist and never touches the local config signal.
        let mut put_body = cfg;
        if let Some(old) = &old_id {
            let mut renames = serde_json::Map::new();
            renames.insert(id.clone(), serde_json::Value::String(old.clone()));
            put_body["__renames"] = serde_json::Value::Object(renames);
        }
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            match save_config(put_body).await {
                Ok(reasons) => {
                    crate::components::settings_ui::toast_saved(
                        result_msg,
                        result_ok,
                        "Saved. Registry hot-reloads shortly.",
                    );
                    // A newly added source/controller that needs a boot-wired
                    // connection raises the dismissible banner with the server's
                    // reasons; an empty list (the hot-reload path) clears it.
                    restart_dismissed.set(false);
                    restart_reasons.set(reasons);
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
                    // The optimistic config.set above may hold a rename's mutated
                    // state (new id + migrated refs) that the server rejected;
                    // re-fetch the authoritative config + device list so the local
                    // signal is not left dirty for the next save attempt (which
                    // would otherwise miss the old slot and pile on more entries).
                    if let Ok(cfg) = fetch_config().await {
                        config.set(cfg);
                    }
                    if let Ok(list) = fetch_devices().await {
                        devices.set(list);
                    }
                }
            }
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = put_body;
    });

    let on_cancel = Callback::new(move |()| sel.set(Sel::List));

    // Refetch BOTH the device list AND the cloud catalog after a mutation, so a
    // toggled/removed source stops rendering everywhere at once (the card status
    // word reads the catalog; a removed cloud source must also drop back into the
    // panel's discovery list, which the catalog's `already_configured` drives).
    // Also re-pulls the authoritative config so the local signal is never left
    // dirty. Only the hydrate build runs it.
    #[cfg(feature = "hydrate")]
    let refetch_after_mutation = move || {
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(cfg) = fetch_config().await {
                config.set(cfg);
            }
            match fetch_devices().await {
                Ok(list) => devices.set(list),
                Err(e) => error.set(e),
            }
            if let Ok(cat) = fetch_catalog().await {
                catalog.set(cat);
            }
            // Tell the CloudWeatherServices panel (its own catalog signal) to
            // refetch so its matrix + discovery are not left stale.
            panel_reload.update(|n| *n += 1);
        });
    };

    // TOGGLE a native source on/off from its device card. Flips that source
    // entry's `enabled` flag in config (OPTIMISTIC: set the local signal first),
    // PUTs, and on error re-pulls config+devices+catalog so the optimistic state is
    // not left dirty. `source_id` is the REAL config source id (never a kind-join),
    // taken from the device's `source_id` backlink stripped of its "source:" prefix.
    #[allow(unused_variables)]
    let on_toggle = Callback::new(move |(source_id, on): (String, bool)| {
        let mut cfg = config.get();
        // Optimistically flip the entry in the local config signal so the card
        // reflects the new state immediately.
        if let Some(arr) = cfg.get_mut("sources").and_then(|v| v.as_array_mut()) {
            if let Some(slot) = arr
                .iter_mut()
                .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(source_id.as_str()))
            {
                if let Some(obj) = slot.as_object_mut() {
                    obj.insert("enabled".into(), serde_json::Value::Bool(on));
                }
            }
        }
        config.set(cfg.clone());
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            match save_config(cfg).await {
                Ok(reasons) => {
                    crate::components::settings_ui::toast_saved(
                        result_msg,
                        result_ok,
                        if on {
                            "Source turned on. Registry hot-reloads shortly."
                        } else {
                            "Source turned off. Registry hot-reloads shortly."
                        },
                    );
                    restart_dismissed.set(false);
                    restart_reasons.set(reasons);
                    if let Ok(list) = fetch_devices().await {
                        devices.set(list);
                    }
                    if let Ok(cat) = fetch_catalog().await {
                        catalog.set(cat);
                    }
                    // Refresh the CloudWeatherServices panel (its own catalog) too.
                    panel_reload.update(|n| *n += 1);
                }
                Err(e) => {
                    result_ok.set(false);
                    result_msg.set(e);
                    refetch_after_mutation();
                }
            }
        });
    });

    // REMOVE a native source from its device card. REFERENCE-SAFE: before the PUT
    // it drops the source from config.sources AND clears every reference to that id
    // (per-reading picks, the forecast pin, zone soil bindings) via
    // `clear_source_id_refs`, so nothing dangles on a gone source. The confirm()
    // names the FRIENDLY source (the device name), never the raw id/slug. On success
    // it refetches devices + catalog (so the removed source stops rendering in the
    // card list AND reappears in the panel's discovery list).
    #[allow(unused_variables)]
    let on_remove = Callback::new(move |source_id: String| {
        // The friendly name for the confirm: the device whose backlink is this id.
        let friendly = devices
            .get()
            .into_iter()
            .find(|d| d.source_id.as_deref() == Some(source_id.as_str()))
            .map(|d| d.name)
            .unwrap_or_else(|| source_id.clone());
        #[cfg(feature = "hydrate")]
        {
            let confirmed = web_sys::window()
                .map(|w| {
                    w.confirm_with_message(&format!(
                        "Remove \u{201c}{friendly}\u{201d}? This deletes the source and \
                         clears every reading pick, forecast pin, and zone soil sensor \
                         that used it. This cannot be undone."
                    ))
                    .unwrap_or(false)
                })
                .unwrap_or(false);
            if !confirmed {
                return;
            }
        }
        let mut cfg = config.get();
        // Drop the source entry.
        if let Some(arr) = cfg.get_mut("sources").and_then(|v| v.as_array_mut()) {
            arr.retain(|e| e.get("id").and_then(|v| v.as_str()) != Some(source_id.as_str()));
        }
        // Clean every reference to the gone id (all three kinds) so nothing orphans.
        clear_source_id_refs(&mut cfg, &source_id);
        config.set(cfg.clone());
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            match save_config(cfg).await {
                Ok(reasons) => {
                    crate::components::settings_ui::toast_saved(
                        result_msg,
                        result_ok,
                        "Source removed. Registry hot-reloads shortly.",
                    );
                    restart_dismissed.set(false);
                    restart_reasons.set(reasons);
                    sel.set(Sel::List);
                    if let Ok(list) = fetch_devices().await {
                        devices.set(list);
                    }
                    if let Ok(cat) = fetch_catalog().await {
                        catalog.set(cat);
                    }
                    // Refresh the CloudWeatherServices panel: the removed source
                    // must reappear in its discovery list (and drop from the matrix).
                    panel_reload.update(|n| *n += 1);
                }
                Err(e) => {
                    result_ok.set(false);
                    result_msg.set(e);
                    refetch_after_mutation();
                }
            }
        });
    });

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

    // When an editor opens (sel leaves List), bring the top of the page into
    // view. Clicking Edit on a card far down the list otherwise leaves the
    // browser scrolled where the click happened, so the editor swaps in above
    // the fold and reads as "it shifted down". Defer past the DOM swap (the same
    // TimeoutFuture(0) trick used elsewhere), then scroll to the top.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let opening = !matches!(sel.get(), Sel::List);
        if opening {
            wasm_bindgen_futures::spawn_local(async move {
                gloo_timers::future::TimeoutFuture::new(0).await;
                if let Some(win) = web_sys::window() {
                    win.scroll_to_with_x_and_y(0.0, 0.0);
                }
            });
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

    // STRUCTURAL peer sections (D3): the device list splits into "Weather sources"
    // and "Controllers", each with its own eyebrow header + live count + whitespace,
    // so category is carried by GROUPING (Gestalt), not by a color tint wall and not
    // by a row of filter tabs. Sources = the local stations + gateways + cloud
    // services; Controllers = irrigation controllers. The HA bridge (and any other
    // kind) falls under sources as an ingest endpoint.
    let cards = move || {
        let all = devices.get();
        // Read the catalog here so the card status words re-resolve when a
        // toggle/remove refetches it (this closure re-runs on catalog change too).
        let cat = catalog.get();
        if all.is_empty() {
            let msg = if !loaded.get() {
                "Loading devices\u{2026}"
            } else if !error.get().is_empty() {
                "Could not load devices."
            } else {
                "No devices yet. Add a weather source or controller above, or scan your network."
            };
            return view! { <p class="settings-empty">{msg}</p> }.into_any();
        }
        let (controllers, sources): (Vec<_>, Vec<_>) = all
            .into_iter()
            .partition(|d| d.kind == "irrigation_controller");

        // The calm status word for a weather-source device, built from CLONEABLE
        // data (so the card's Fn children stay Fn). Resolution:
        //   * disabled native source (enabled == Some(false)) -> dim "Off",
        //   * a configured CLOUD kind present in the catalog -> the panel's exact
        //     word via `cloud_status_word_for_entry` (looked up by source_kind),
        //   * any other local weather_gateway station -> owner "On, measuring".
        // Returns None for a device that carries no status word (controllers, HA
        // bridges, non-weather kinds); those keep their existing badge-only header.
        let status_for = |d: &Device| -> Option<(&'static str, String)> {
            // Only config-backed native SOURCES carry a status word (a `source:`
            // device). Controllers + the HA bridge keep their badge-only header.
            if d.origin != "native" || !d.id.starts_with("source:") {
                return None;
            }
            if d.enabled == Some(false) {
                return Some(("dim", "Off".to_string()));
            }
            // A configured cloud source reuses the panel's exact word (matched on
            // the SourceKind slug, READ-ONLY). Two same-kind cloud sources sharing a
            // word is acceptable per the status contract.
            if let Some(kind) = d.source_kind.as_deref() {
                if let Some(entry) = cat
                    .cloud_sources
                    .iter()
                    .find(|e| e.kind == kind && e.already_configured)
                {
                    let (slug, word) = cloud_status_word_for_entry(entry);
                    return Some((slug, word));
                }
            }
            // A local station (weather_gateway) is always-on hardware, measuring
            // while enabled; any other enabled source (a cloud kind not in the
            // catalog, or a virtual mqtt/webhook/prometheus/influxdb ingest) is
            // simply on (it does not "measure" a yard like a station).
            if d.kind == "weather_gateway" {
                Some(("owner", "On, measuring".to_string()))
            } else {
                Some(("owner", "On".to_string()))
            }
        };

        // One peer section: an eyebrow header carrying the title + a live count,
        // then the card list. Rendered only when the section has members, so an
        // empty category never adds a bare header.
        let section = |title: &'static str, list: Vec<Device>| {
            (!list.is_empty()).then(|| {
                let count = list.len();
                let items: Vec<_> = list
                    .into_iter()
                    .map(|d| {
                        let status = status_for(&d);
                        device_card(d, on_edit, on_toggle, on_remove, status)
                    })
                    .collect();
                view! {
                    <section class="settings-section-head" style="margin-top:var(--space-5)">
                        <h2 class="settings-section__title" style="display:flex;align-items:baseline;gap:var(--space-2)">
                            {title}
                            <span class="settings-checklist__count">{count}</span>
                        </h2>
                        <ul class="settings-card-list">{items}</ul>
                    </section>
                }
            })
        };

        view! {
            {section("Weather sources", sources)}
            {section("Controllers", controllers)}
        }
        .into_any()
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
        Sel::EditSource(entry) => {
            let cur_id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let siblings = ids_except(&config.get(), "sources", &cur_id);
            view! {
                <SourceEditorPanel existing=Some(entry) sibling_ids=siblings on_commit=persist_entry on_cancel=on_cancel/>
            }
            .into_any()
        }
        Sel::AdoptGateway(prefill) => view! {
            <SourceEditorPanel existing=Some(prefill) on_commit=persist_entry on_cancel=on_cancel/>
        }
        .into_any(),
        Sel::AddController => view! {
            <ControllerEditorPanel on_commit=persist_entry on_cancel=on_cancel/>
        }
        .into_any(),
        Sel::EditController(entry) => {
            let cur_id = entry
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let siblings = ids_except(&config.get(), "controllers", &cur_id);
            view! {
                <ControllerEditorPanel existing=Some(entry) sibling_ids=siblings on_commit=persist_entry on_cancel=on_cancel/>
            }
            .into_any()
        }
        Sel::List => view! {
            <header class="settings-page__header">
                <h1 class="settings-page__title">"Devices"<HelpHint topic="devices"/></h1>
                <p class="settings-page__subtitle">
                    "Everything LocalSky talks to: controllers, weather sources, and the sensors they carry. Native devices are editable here; Home Assistant devices mirror in automatically. "
                    <a href=crate::docs::doc_url("devices")
                        target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent); white-space: nowrap">
                        "Guide \u{2192}"
                    </a>
                </p>
            </header>

            // Cloud weather, the PROMINENT top section (not a subtle fold):
            // first-class for every customer. A no-hardware customer sees it as
            // the primary path; a hardware customer sees the same list framed as
            // a complement/backup to their station (local sensors always
            // outrank cloud). A PUT in here refreshes this hub's device list via
            // on_changed.
            <crate::components::settings::CloudWeatherServices
                // The per-field ownership picker (the "Advanced" fold below) is the
                // ONE home for source-to-reading arbitration now, so the panel's
                // read-only backup-chain visualization is turned off here to avoid a
                // second representation of the same fact (persona E: four mappings
                // collapse to one).
                show_chain=false
                for_hardware=Signal::derive(move || {
                    // LOCAL-FIRST: true when the user has ANY local weather
                    // hardware (a LAN station OR a gateway), so the panel leads
                    // with the local band. A Tempest/Davis/Ecowitt LAN station all
                    // classify as the single "weather_gateway" device kind
                    // (devices/builder.rs:86), so is_local_weather_source catches
                    // them; a "weather_cloud" device is excluded (cloud is not
                    // hardware) so a cloud-only user still reads cloud-first.
                    devices
                        .get()
                        .iter()
                        .any(|d| is_local_weather_source(&d.kind))
                })
                on_changed=Callback::new(move |()| refresh())
                // Multi-field keyed sources (WeatherKit) cannot use the cloud
                // panel's single-secret one-click without writing a dead,
                // 401-ing entry, so they route here: open this hub's full
                // SourceEditorPanel as a prefilled ADD (id editable, kind seeded,
                // config = the kind's default template), which captures all four
                // WeatherKit fields together. AdoptGateway is the existing
                // "prefilled add" selection the editor already understands.
                on_edit_full=Callback::new(move |kind: String| {
                    sel.set(Sel::AdoptGateway(serde_json::json!({ "kind": kind })));
                })
                // A device-card toggle/remove bumps this so the panel's matrix +
                // discovery refetch (they own a separate catalog signal).
                reload_trigger=panel_reload
            />

            <Panel>
                // The mental model, taught in one glance with the same entity chips
                // used on the cards below. FIRST-RUN ONLY (persona A/D): a user with
                // devices already knows the model and does not need permanent chrome
                // promising Zones + a Controller they may not have built, so the
                // strip only shows while the list is empty. The Zone/Controller half
                // is dimmed (it is the part a brand-new user has not built yet).
                {move || devices.get().is_empty().then(|| view! {
                    <div class="entity-pipeline" aria-hidden="true">
                        <span class="entity-badge entity-badge--source">"Source"</span>
                        <span class="entity-pipeline__rel">"carries \u{2192}"</span>
                        <span class="entity-badge entity-badge--sensor">"Sensors"</span>
                        <span class="entity-pipeline__rel" style="opacity:0.5">"bind to \u{2192}"</span>
                        <span class="entity-badge entity-badge--zone" style="opacity:0.5">"Zones"</span>
                        <span class="entity-pipeline__rel" style="opacity:0.5">"\u{2190} run by"</span>
                        <span class="entity-badge entity-badge--controller" style="opacity:0.5">"Controller"</span>
                    </div>
                })}
                <div class="device-add-bar">
                    <div class="device-add-bar__group">
                        <span class="device-add-bar__label">"Add a device"</span>
                        <Button variant="primary" on_click=Callback::new(move |_| sel.set(Sel::AddSource))>"Weather source"</Button>
                        <Button variant="primary" on_click=Callback::new(move |_| sel.set(Sel::AddController))>"Controller"</Button>
                    </div>
                    <div class="device-add-bar__group device-add-bar__group--end">
                        <span class="device-add-bar__or">"or"</span>
                        // variant="secondary" (bordered surface at rest) + a radar
                        // icon so this reads as an actionable button, not helper
                        // text, alongside its primary "Add a device" siblings.
                        <Button variant="secondary" icon="search" on_click=Callback::new(on_discover) disabled=Signal::derive(move || discovering.get())>
                            {move || if discovering.get() { "Scanning network\u{2026}" } else { "Scan the network" }}
                        </Button>
                    </div>
                </div>
                // The category filter tabs are RETIRED: the list now separates
                // sources from controllers STRUCTURALLY (two peer sections with
                // their own header + count), so a tab row that re-filters the same
                // list is redundant chrome.
                {move || {
                    let m = result_msg.get();
                    (!m.is_empty()).then(|| {
                        let cls = if result_ok.get() { "setup-result setup-result--ok" } else { "setup-result setup-result--err" };
                        view! { <p class=cls>{m}</p> }
                    })
                }}
                {discovery_results}
                {cards}
            </Panel>

            // FIRST-CLASS control (not an "advanced" fold): deciding which source
            // provides each reading (your station for wind, radar for rain) is the
            // core of a self-hosted weather setup, not an afterthought. It sits BELOW
            // the device-card list (the one weather-source list) so the order reads
            // hardware inventory -> per-reading arbitration, one mental model. The
            // raw override map stays in its own advanced sub-fold INSIDE the
            // component. The standalone /settings/data-sources route redirects here.
            <section class="settings-section-head" style="margin-top:var(--space-6)">
                <h2 class="settings-section__title">"Which source provides each reading"</h2>
                <p class="settings-section-head__sub">
                    "You decide where each number comes from. Every reading has an ordered "
                    "chain of sources: the top one reporting now wins, and if it goes quiet "
                    "the next takes over. LocalSky sets a smart default order for your "
                    "region; drag to make your own (your station for wind, a cloud service "
                    "for rain, whatever fits). A source only leads while it is reporting, so "
                    "a reading is never lost."
                </p>
                <crate::components::settings::SettingsDataSources embedded=true/>
            </section>
        }
        .into_any(),
    }
    };

    // Constrain to the settings content width so the card list stays
    // mouse/eye-friendly on ultrawide displays (matches the other settings
    // sections, which wrap in .settings-page).
    view! {
        <div class="settings-page">
            <crate::components::settings::RestartBanner reasons=restart_reasons dismissed=restart_dismissed/>
            {detail}
        </div>
    }
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
                entity=Some(EntityKind::Source)
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
                        <Button variant="primary" on_click=Callback::new(move |_| on_adopt.run(h.clone()))>"Adopt as source"</Button>
                    }
                    .into_any()
                })
            />
        </li>
    }
}

/// One device as an expandable card. Native devices with a config backlink get
/// an Edit action; HA-origin and the HA bridge are read-only.
/// Repoint every config reference to a renamed source id from `old` to `new`:
/// the per-reading picks (`field_source_overrides` values), the forecast source
/// pin (`forecast_provider`), and each zone's soil binding
/// (`soil_sensor_id = "source:<id>:soilmoisture_<zone>"`). Called only on a
/// source rename so it never leaves an orphan, which would blank a pick or a
/// soil sensor and trip zone_soil_source_missing. Operates on the config JSON
/// (the same value persist_entry PUTs), so all three migrations land atomically
/// with the rename in one save.
fn migrate_source_id_refs(cfg: &mut serde_json::Value, old: &str, new: &str) {
    let Some(root) = cfg.as_object_mut() else {
        return;
    };
    // Per-reading picks: field -> source id, so the VALUES reference the source.
    if let Some(map) = root
        .get_mut("field_source_overrides")
        .and_then(|v| v.as_object_mut())
    {
        for v in map.values_mut() {
            if v.as_str() == Some(old) {
                *v = serde_json::Value::String(new.to_string());
            }
        }
    }
    // Forecast source pin: a bare source id.
    if root.get("forecast_provider").and_then(|v| v.as_str()) == Some(old) {
        root.insert(
            "forecast_provider".into(),
            serde_json::Value::String(new.to_string()),
        );
    }
    // Zone soil bindings: "source:<old>:soilmoisture_<zone>" -> repoint the id,
    // preserving the rest of the composite key.
    let old_prefix = format!("source:{old}:");
    if let Some(zones) = root.get_mut("zones").and_then(|v| v.as_object_mut()) {
        for z in zones.values_mut() {
            let Some(zobj) = z.as_object_mut() else {
                continue;
            };
            let repointed = zobj
                .get("soil_sensor_id")
                .and_then(|v| v.as_str())
                .and_then(|s| s.strip_prefix(&old_prefix))
                .map(|rest| format!("source:{new}:{rest}"));
            if let Some(newval) = repointed {
                zobj.insert("soil_sensor_id".into(), serde_json::Value::String(newval));
            }
        }
    }
}

/// Repoint every config reference to a renamed controller id from `old` to
/// `new`. The AUDITED reference set is a single kind: each zone's
/// `controller_id` (which zone fires from which controller). Called only on a
/// controller rename so it never leaves a zone pinned to a gone controller,
/// which would trip `controller_id_invalid` on save/load. Operates on the config
/// JSON (the same value `persist_entry` PUTs), so the migration lands atomically
/// with the rename in one save. Mirrors `migrate_source_id_refs`.
///
/// Deliberately NOT touched:
///   * `ControllerEntry.default` is a boolean intrinsic to the renamed entry
///     (not an id reference), so it rides along with the entry unchanged.
///   * The `controller_id` fields inside Hydrawise / Rain Bird configs are VENDOR
///     device serials living INSIDE the renamed entry's own config, not
///     references to a LocalSky controller id, so they must not be rewritten.
///   * Historical run records (runs / active_runs, keyed by controller_id) are
///     an immutable audit log, not config, and are left as-is exactly like a
///     source rename leaves history alone.
fn migrate_controller_id_refs(cfg: &mut serde_json::Value, old: &str, new: &str) {
    let Some(root) = cfg.as_object_mut() else {
        return;
    };
    // Zones: each zone's `controller_id` names the controller that fires it. This
    // is the only config reference to a controller id; repoint every zone that
    // named the old id, leaving zones on other controllers untouched.
    if let Some(zones) = root.get_mut("zones").and_then(|v| v.as_object_mut()) {
        for z in zones.values_mut() {
            let Some(zobj) = z.as_object_mut() else {
                continue;
            };
            if zobj.get("controller_id").and_then(|v| v.as_str()) == Some(old) {
                zobj.insert(
                    "controller_id".into(),
                    serde_json::Value::String(new.to_string()),
                );
            }
        }
    }
}

/// The REMOVE-variant of `migrate_source_id_refs`: when a source is deleted, clear
/// every config reference to its id so nothing points at a now-gone source (which
/// would blank a pick / a soil sensor and trip zone_soil_source_missing). It clears
/// the SAME three reference kinds the rename migrates, but by REMOVAL rather than
/// repoint:
///   * per-reading picks (`field_source_overrides` values == id) -> drop the entry,
///     so the reading falls back to Auto,
///   * the forecast source pin (`forecast_provider` == id) -> set to null,
///   * each zone soil binding (`soil_sensor_id` starting "source:<id>:") -> null.
/// Called by the host's on_remove BEFORE the PUT (on the same config value that is
/// PUT), so the delete + the reference cleanup land atomically in one save.
/// Unrelated references (a different source's picks / pin / soil) are untouched.
fn clear_source_id_refs(cfg: &mut serde_json::Value, id: &str) {
    let Some(root) = cfg.as_object_mut() else {
        return;
    };
    // Per-reading picks: drop every field whose VALUE pins the removed id, so the
    // reading returns to Auto rather than dangling on a gone source.
    if let Some(map) = root
        .get_mut("field_source_overrides")
        .and_then(|v| v.as_object_mut())
    {
        map.retain(|_, v| v.as_str() != Some(id));
    }
    // Forecast source pin: null it if it named the removed id.
    if root.get("forecast_provider").and_then(|v| v.as_str()) == Some(id) {
        root.insert("forecast_provider".into(), serde_json::Value::Null);
    }
    // Zone soil bindings: null any "source:<id>:soilmoisture_<zone>" so the zone
    // no longer references the gone source.
    let prefix = format!("source:{id}:");
    if let Some(zones) = root.get_mut("zones").and_then(|v| v.as_object_mut()) {
        for z in zones.values_mut() {
            let Some(zobj) = z.as_object_mut() else {
                continue;
            };
            let points_at_removed = zobj
                .get("soil_sensor_id")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.starts_with(&prefix));
            if points_at_removed {
                zobj.insert("soil_sensor_id".into(), serde_json::Value::Null);
            }
        }
    }
}

fn device_card(
    dev: Device,
    on_edit: Callback<String>,
    on_toggle: Callback<(String, bool)>,
    on_remove: Callback<String>,
    // The calm status word for a weather-source card: (semantic slug, word),
    // rendered in the header via the same `cloud-word--<slug>` classes the panel
    // rows use. None for controllers / HA bridges / non-weather kinds.
    status: Option<(&'static str, String)>,
) -> impl IntoView {
    let icon = kind_icon(&dev.kind).to_string();
    let title = dev.name.clone();
    let child_count = dev.children.len();
    // The item-count moves into the SUBTITLE (persona F: fewer badges): a card
    // that carries sensors/zones names them in its own subtitle line, so the
    // badge row is freed for status-by-exception only. Sensors read "sensors",
    // zones read "zones"; the count word tracks the child kind.
    let child_word = if dev.kind == "irrigation_controller" {
        "zone"
    } else {
        "sensor"
    };
    let count_suffix = if child_count == 0 {
        String::new()
    } else {
        format!(
            " \u{00b7} {child_count} {child_word}{}",
            if child_count == 1 { "" } else { "s" }
        )
    };
    let subtitle = match &dev.model {
        Some(m) => format!("{m} \u{00b7} {}{count_suffix}", kind_label(&dev.kind)),
        None => format!("{}{count_suffix}", kind_label(&dev.kind)),
    };

    let origin = dev.origin.clone();
    let online = dev.online;
    let also_in_ha = dev.also_in_ha;
    // Every native, config-backed device is EDITABLE: sources (`source:`, incl.
    // ha_passthrough + the virtual ingests) AND controllers (`controller:`). The
    // Devices hub is the sole nav path to edit a controller, so it must keep its
    // Edit action; only the read-only HA-imported connection (`ha:` + non-native)
    // is excluded. (Toggle + Remove stay `source:`-only via native_source below;
    // controllers have no on/off toggle here.)
    let editable = dev.origin == "native"
        && (dev.id.starts_with("source:") || dev.id.starts_with("controller:"));
    let edit_id = dev.id.clone();

    // The REAL config source id this card targets (never a kind-join): the
    // `source:<id>` backlink stripped of its prefix, so Edit/toggle/Remove all
    // address the exact config entry. None for a controller / HA device.
    let source_id = dev
        .source_id
        .clone()
        .or_else(|| dev.id.strip_prefix("source:").map(str::to_string));
    // Any native, config-backed SOURCE (not just weather_gateway/weather_cloud:
    // also the virtual mqtt/webhook/prometheus/influxdb/demo ingests and
    // ha_passthrough) gets the on/off toggle + reference-safe Remove, so the one
    // list can manage EVERY source kind. Controllers (controller:) + HA-imported
    // devices are excluded.
    let native_source =
        dev.origin == "native" && dev.id.starts_with("source:") && source_id.is_some();
    // A native weather source carries a config-backed enabled flag, so it gets a
    // WORKING on/off toggle (a cloud source MUST; a local station may too). The
    // toggle reflects `enabled != Some(false)`. HA-imported sources (enabled ==
    // None) get no toggle. Remove is offered for every native source.
    let toggleable = native_source && dev.enabled.is_some();
    let removable = native_source;
    let toggle_source_id = source_id.clone();
    let remove_source_id = source_id.clone();
    let enabled_now = dev.enabled != Some(false);

    // Map the device kind to its entity identity so the card carries the
    // canonical stripe + badge (Source = brings data in, Controller = sends
    // water out). The HA bridge isn't a source or controller, so it stays
    // neutral. This is the page meant to teach the model -- it must wear it.
    let entity_kind = match dev.kind.as_str() {
        "weather_gateway" | "weather_cloud" => Some(EntityKind::Source),
        "irrigation_controller" => Some(EntityKind::Controller),
        _ => None,
    };

    // Cap the child-chip block (persona D: a Tempest lists ~13 sensor chips) so
    // the expand stays scannable. Show at most CHILD_CAP chips, then a single
    // "+N more" row that states the remainder rather than the wall of chips.
    const CHILD_CAP: usize = 8;
    let overflow = child_count.saturating_sub(CHILD_CAP);
    let child_rows: Vec<_> = dev
        .children
        .iter()
        .take(CHILD_CAP)
        .map(|c| {
            // Child readings are entity chips (the quiet category cue), so the
            // source -> sensors and controller -> zones relationship is legible
            // at a glance.
            let (slug, meta) = if c.child_type == "zone" {
                ("zone", "zone".to_string())
            } else {
                (
                    "sensor",
                    c.role.clone().unwrap_or_else(|| "sensor".to_string()),
                )
            };
            view! {
                <li class="device-child">
                    <span class="device-child__label">{c.label.clone()}</span>
                    <span class=format!("entity-badge entity-badge--{slug}")>{meta}</span>
                </li>
            }
        })
        .collect();
    let overflow_row = (overflow > 0).then(|| {
        view! {
            <li class="device-child device-child-empty">{format!("+{overflow} more")}</li>
        }
    });

    let details_empty = child_rows.is_empty();
    let is_ha = origin == "home_assistant";

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon=icon
                title=title
                subtitle=subtitle
                entity=entity_kind
                editable=editable
                badges=Box::new(move || {
                    // STATUS BY EXCEPTION (persona F): the common, expected states
                    // are silent. A native, online device carries NO badge (that is
                    // the norm). We surface only the exceptions worth a glance:
                    //   Offline   -> a real fault (danger)
                    //   + HA      -> also mirrored into Home Assistant (accent)
                    //   read-only -> managed in HA, nothing to do here (muted)
                    // At most two pills ever show, down from up to five.
                    let offline_badge = matches!(online, Some(false)).then(|| {
                        view! { <SettingsBadge label="Offline".into() tone=BadgeTone::Danger/> }
                    });
                    let readonly_badge = (origin == "home_assistant").then(|| {
                        view! { <SettingsBadge label="Managed in HA".into() tone=BadgeTone::Muted/> }
                    });
                    // The mirror marker only makes sense for a NATIVE device that is
                    // ALSO in HA; an HA-origin device already reads "Managed in HA".
                    let mirror_badge = (also_in_ha && origin != "home_assistant").then(|| {
                        view! { <SettingsBadge label="+ HA".into() tone=BadgeTone::Accent/> }
                    });
                    // The calm status WORD for a weather source (the same one-word
                    // vocabulary the cloud panel rows use), in its single semantic
                    // color via `cloud-word--<slug>`. Only weather sources carry it.
                    let status_word = status.as_ref().map(|(slug, word)| {
                        view! {
                            <span class=format!("cloud-word cloud-word--{slug}")>{word.clone()}</span>
                        }
                    });
                    view! {
                        {status_word}
                        {offline_badge}
                        {readonly_badge}
                        {mirror_badge}
                    }
                    .into_any()
                })
                details=Box::new(move || {
                    if details_empty {
                        view! { <p class="device-child-empty">"No sensors or zones listed yet."</p> }
                            .into_any()
                    } else {
                        view! { <ul class="device-child-list">{child_rows}{overflow_row}</ul> }.into_any()
                    }
                })
                actions=Box::new(move || {
                    // The on/off TOGGLE for a native weather source (a cloud source
                    // MUST have one; a local station gets one too). Its `checked`
                    // seeds from the current enabled state; on flip it calls the host
                    // on_toggle with the REAL source id. An Effect fires the callback
                    // only when the value DIVERGES from the persisted state, so a
                    // refetch-driven re-seed does not re-PUT.
                    let toggle = toggleable.then(|| {
                        let checked = RwSignal::new(enabled_now);
                        let sid = toggle_source_id.clone().unwrap_or_default();
                        Effect::new(move |_| {
                            let on = checked.get();
                            if on != enabled_now {
                                on_toggle.run((sid.clone(), on));
                            }
                        });
                        view! {
                            <Toggle
                                checked=checked
                                label=if enabled_now { "On".to_string() } else { "Off".to_string() }
                            />
                        }
                    });
                    // Edit, for a native editable source/controller.
                    let edit_btn = editable.then(|| {
                        let id = edit_id.clone();
                        view! {
                            <Button variant="ghost" on_click=Callback::new(move |_| on_edit.run(id.clone()))>"Edit"</Button>
                        }
                    });
                    // Remove, reference-safe (the host clears every reference before
                    // the PUT). Offered for every native source.
                    let remove_btn = removable.then(|| {
                        let rid = remove_source_id.clone().unwrap_or_default();
                        view! {
                            <Button variant="ghost" on_click=Callback::new(move |_| on_remove.run(rid.clone()))>"Remove"</Button>
                        }
                    });
                    // An HA-origin device is read-only (nothing to toggle/edit/remove).
                    let ha_note = (is_ha && !editable && !removable).then(|| {
                        view! { <span class="device-child-empty">"Managed in Home Assistant"</span> }
                    });
                    view! {
                        {toggle}
                        {edit_btn}
                        {remove_btn}
                        {ha_note}
                    }
                    .into_any()
                })
            />
        </li>
    }
}

#[cfg(test)]
mod tests {
    use super::{clear_source_id_refs, migrate_controller_id_refs, migrate_source_id_refs};

    // Removing a source must CLEAR all three reference kinds so nothing dangles on
    // the gone id: per-reading picks whose VALUE is the id drop out (back to Auto),
    // the forecast pin nulls if it named the id, and each zone soil binding starting
    // "source:<id>:" nulls. Unrelated references (a different source's pick / pin /
    // soil) are untouched.
    #[test]
    fn remove_clears_every_reference() {
        let mut cfg = serde_json::json!({
            "field_source_overrides": {
                "wind_speed": "ecowitt_gw",   // -> dropped
                "rain_rate": "noaa_mrms"      // unrelated, untouched
            },
            "forecast_provider": "ecowitt_gw", // -> nulled
            "zones": {
                "front_lawn": {
                    "soil_sensor_id": "source:ecowitt_gw:soilmoisture_front_lawn" // -> nulled
                },
                "back_bed": {
                    "soil_sensor_id": "source:other_src:soilmoisture_back_bed" // untouched
                },
                "no_soil": { "soil_sensor_id": serde_json::Value::Null }
            }
        });

        clear_source_id_refs(&mut cfg, "ecowitt_gw");

        // Pick that pinned the removed source is gone; the unrelated pick survives.
        assert!(cfg["field_source_overrides"].get("wind_speed").is_none());
        assert_eq!(cfg["field_source_overrides"]["rain_rate"], "noaa_mrms");
        // Forecast pin nulled.
        assert!(cfg["forecast_provider"].is_null());
        // The removed source's zone soil binding nulled; the other zone untouched.
        assert!(cfg["zones"]["front_lawn"]["soil_sensor_id"].is_null());
        assert_eq!(
            cfg["zones"]["back_bed"]["soil_sensor_id"],
            "source:other_src:soilmoisture_back_bed"
        );
        assert!(cfg["zones"]["no_soil"]["soil_sensor_id"].is_null());
    }

    // Removing a source nothing references is a clean no-op (and must not panic on a
    // config missing the optional sections).
    #[test]
    fn remove_with_no_references_is_noop() {
        let mut cfg = serde_json::json!({ "sources": [] });
        clear_source_id_refs(&mut cfg, "gone");
        assert_eq!(cfg, serde_json::json!({ "sources": [] }));
    }

    // A source rename must repoint ALL three reference kinds so nothing orphans:
    // per-reading picks (field_source_overrides values), the forecast pin, and
    // zone soil bindings ("source:<id>:...") . Unrelated references stay put.
    #[test]
    fn rename_migrates_every_reference() {
        let mut cfg = serde_json::json!({
            "field_source_overrides": {
                "wind_speed": "ecowitt_gw",   // -> new id
                "rain_rate": "noaa_mrms"      // unrelated, untouched
            },
            "forecast_provider": "ecowitt_gw", // -> new id
            "zones": {
                "front_lawn": {
                    "soil_sensor_id": "source:ecowitt_gw:soilmoisture_front_lawn"
                },
                "back_bed": {
                    "soil_sensor_id": "source:other_src:soilmoisture_back_bed" // untouched
                },
                "no_soil": { "soil_sensor_id": serde_json::Value::Null }
            }
        });

        migrate_source_id_refs(&mut cfg, "ecowitt_gw", "yard_soil");

        assert_eq!(cfg["field_source_overrides"]["wind_speed"], "yard_soil");
        assert_eq!(cfg["field_source_overrides"]["rain_rate"], "noaa_mrms");
        assert_eq!(cfg["forecast_provider"], "yard_soil");
        assert_eq!(
            cfg["zones"]["front_lawn"]["soil_sensor_id"],
            "source:yard_soil:soilmoisture_front_lawn"
        );
        assert_eq!(
            cfg["zones"]["back_bed"]["soil_sensor_id"],
            "source:other_src:soilmoisture_back_bed"
        );
        assert!(cfg["zones"]["no_soil"]["soil_sensor_id"].is_null());
    }

    // Renaming a source that nothing references must be a clean no-op (and must
    // not panic on missing keys / a config without the optional sections).
    #[test]
    fn rename_with_no_references_is_noop() {
        let mut cfg = serde_json::json!({ "sources": [] });
        migrate_source_id_refs(&mut cfg, "a", "b");
        assert_eq!(cfg, serde_json::json!({ "sources": [] }));
    }

    // A controller rename must repoint the ONE audited reference kind (each zone's
    // controller_id) so no zone is left firing from a gone controller. Zones on a
    // different controller, and zones with an empty controller_id (default-target),
    // stay put. The VENDOR controller_id inside a Rain Bird / Hydrawise config is a
    // device serial, not a LocalSky controller reference, so it must NOT be
    // rewritten by the rename.
    #[test]
    fn controller_rename_migrates_every_reference() {
        let mut cfg = serde_json::json!({
            "controllers": [{
                "id": "rainbird_1",
                "kind": "rainbird",
                // The vendor device serial: same field NAME, must be left alone.
                "config": { "controller_id": "SN-99" }
            }],
            "zones": {
                "front_lawn": { "controller_id": "os_main" },   // -> new id
                "back_bed":   { "controller_id": "ha_backup" }, // unrelated, untouched
                "no_ctrl":    { "controller_id": "" }           // default target, untouched
            }
        });

        migrate_controller_id_refs(&mut cfg, "os_main", "sprinkler_box");

        // The zone that named the old id is repointed.
        assert_eq!(cfg["zones"]["front_lawn"]["controller_id"], "sprinkler_box");
        // A zone on a different controller is untouched.
        assert_eq!(cfg["zones"]["back_bed"]["controller_id"], "ha_backup");
        // A default-target zone (empty controller_id) is untouched.
        assert_eq!(cfg["zones"]["no_ctrl"]["controller_id"], "");
        // The VENDOR serial inside the controller's own config is NOT rewritten.
        assert_eq!(cfg["controllers"][0]["config"]["controller_id"], "SN-99");
    }

    // Renaming a controller that no zone references must be a clean no-op (and must
    // not panic on missing keys / a config without a zones section).
    #[test]
    fn controller_rename_with_no_references_is_noop() {
        let mut cfg = serde_json::json!({ "controllers": [] });
        migrate_controller_id_refs(&mut cfg, "a", "b");
        assert_eq!(cfg, serde_json::json!({ "controllers": [] }));
    }
}
