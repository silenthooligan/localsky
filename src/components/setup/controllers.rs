// ControllersStep. Add irrigation controllers during first-run setup,
// reusing the same inline editor as Settings -> Controllers. Each added
// controller can be tested live (POST /api/v1/wizard/test_controller) and
// its zones scanned (POST /api/v1/wizard/scan_zones); scanned stations can
// be imported as zone stubs so the Zones step opens pre-populated.
//
// Controllers are written into the wizard draft, not the live config.
// Skipping is fine: DryRun lets you explore scheduling without hardware,
// and controllers can be added under /settings/controllers any time.

use leptos::prelude::*;

use crate::components::controllers_form::{controller_kind_options, ControllerEditorPanel};
use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};

#[cfg(feature = "hydrate")]
async fn fetch_draft() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

#[cfg(feature = "hydrate")]
async fn save_draft(draft: serde_json::Value) -> Result<(), String> {
    let resp = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&draft)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

/// Human label for a controller kind, from the shared kind picker options.
fn controller_kind_pretty(kind: &str) -> String {
    controller_kind_options()
        .into_iter()
        .find(|(value, _)| value == kind)
        .map(|(_, label)| label)
        .unwrap_or_else(|| kind.to_string())
}

/// slug for a zone imported from a controller scan: lowercase, runs of
/// non-alphanumerics collapse to single underscores.
fn zone_slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_us = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_us = false;
        } else if !last_us {
            out.push('_');
            last_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        "zone".into()
    } else {
        out
    }
}

/// Minimal valid ZoneConfig stub for an imported station. Species, soil and
/// sprinkler are deliberately generic; the Zones step is where they get real.
fn zone_stub(name: &str, controller_id: &str, station_id: &str) -> serde_json::Value {
    serde_json::json!({
        "display_name": name,
        "area_sqft": 1000.0,
        "species": "other",
        "soil_texture": "loam",
        "sprinkler_type": "rotor",
        "controller_id": controller_id,
        "controller_station": station_id,
    })
}

#[derive(Clone, PartialEq)]
struct ScannedZone {
    station_id: String,
    name: String,
    selected: bool,
    already_added: bool,
}

#[component]
pub fn ControllersStep() -> impl IntoView {
    let draft = RwSignal::new(serde_json::Value::Null);
    let adding = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                draft.set(d);
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = draft;

    // Merge a controller entry from the shared editor into the draft,
    // enforcing default-flag exclusivity across draft controllers.
    let persist = Callback::new(move |entry: serde_json::Value| {
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        draft.update(|d| {
            let Some(cfg) = d.get_mut("config") else {
                return;
            };
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
                    *slot = entry.clone();
                } else {
                    arr.push(entry.clone());
                }
            }
        });
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
        adding.set(false);
    });

    let added_view = move || {
        let controllers = draft
            .get()
            .get("config")
            .and_then(|c| c.get("controllers"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if controllers.is_empty() {
            return view! {
                <p class="setup-step__body" style="margin:0">
                    "No controllers added yet. Add your hardware below, or add a DryRun "
                    "controller to explore scheduling without firing a single valve."
                </p>
            }
            .into_any();
        }
        controllers
            .into_iter()
            .map(|entry| view! { <WizardControllerRow entry=entry draft=draft /> })
            .collect_view()
            .into_any()
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"What runs your sprinklers?"</h2>
            <p class="setup-step__body">
                "Which hardware fires your valves? OpenSprinkler talks directly on the LAN; "
                "Rachio, Hydrawise, B-hyve and Rain Bird connect through their cloud APIs; "
                "MQTT and Home Assistant cover everything else. Add one, test the connection, "
                "then scan it to pull in your zones automatically."
            </p>

            <crate::components::setup::discover::NetworkScan mode="controllers" draft=draft/>

            <ul class="cond-list">{added_view}</ul>

            {move || if adding.get() {
                view! {
                    <ControllerEditorPanel
                        on_commit=persist
                        on_cancel=Callback::new(move |()| adding.set(false))
                    />
                }.into_any()
            } else {
                view! {
                    <button type="button" class="setup-footer__btn setup-footer__btn--primary"
                        on:click=move |_| adding.set(true)>"+ Add a controller"</button>
                }.into_any()
            }}

            <p class="sensors-section__hint" style="margin-top: var(--space-3)">
                "Skipping is fine: if HA_URL and HA_LONG_LIVED_TOKEN env vars are set, LocalSky "
                "synthesizes a Home Assistant controller automatically; otherwise add one later "
                "under "<a href="/settings/controllers">"Settings"</a>". Zones imported from a scan "
                "land in the next step with sensible placeholders you can refine."
            </p>

            <SetupFooter
                prev=prev_step_href("controllers")
                next=next_step_href("controllers")
            />
        </div>
    }
}

/// One added controller in the wizard list: identity row plus Test and
/// Scan-zones actions. Scan results render as an import checklist that
/// writes ZoneConfig stubs into the draft.
#[component]
fn WizardControllerRow(
    entry: serde_json::Value,
    draft: RwSignal<serde_json::Value>,
) -> impl IntoView {
    let id = entry
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let kind = entry
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let is_default = entry.get("default").and_then(|v| v.as_bool()) == Some(true);
    let pretty = controller_kind_pretty(&kind);

    let testing = RwSignal::new(false);
    let test_msg = RwSignal::new(String::new());
    let test_ok = RwSignal::new(false);
    let scanning = RwSignal::new(false);
    let scan_msg = RwSignal::new(String::new());
    let scanned: RwSignal<Vec<ScannedZone>> = RwSignal::new(Vec::new());

    let entry_for_test = entry.clone();
    let on_test = move |_| {
        if testing.get_untracked() {
            return;
        }
        testing.set(true);
        test_msg.set(String::new());
        let body = serde_json::json!({ "controller": entry_for_test.clone() });
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let result = async {
                let resp = gloo_net::http::Request::post("/api/v1/wizard/test_controller")
                    .json(&body)
                    .map_err(|e| e.to_string())?
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let v = resp
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| e.to_string())?;
                if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
                    let zones = v.get("zone_count").and_then(|n| n.as_u64()).unwrap_or(0);
                    let fw = v
                        .get("firmware")
                        .and_then(|f| f.as_str())
                        .map(|f| format!(", firmware {f}"))
                        .unwrap_or_default();
                    Ok(format!("Connected: {zones} stations reported{fw}"))
                } else {
                    Err(v
                        .get("detail")
                        .and_then(|d| d.as_str())
                        .unwrap_or("controller unreachable")
                        .to_string())
                }
            }
            .await;
            match result {
                Ok(msg) => {
                    test_ok.set(true);
                    test_msg.set(msg);
                }
                Err(e) => {
                    test_ok.set(false);
                    test_msg.set(format!("Test failed: {e}"));
                }
            }
            testing.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = body;
    };

    let entry_for_scan = entry.clone();
    let id_for_scan = id.clone();
    let on_scan = move |_| {
        if scanning.get_untracked() {
            return;
        }
        scanning.set(true);
        scan_msg.set(String::new());
        scanned.set(Vec::new());
        let body = serde_json::json!({ "controller": entry_for_scan.clone() });
        let cid = id_for_scan.clone();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let result = async {
                let resp = gloo_net::http::Request::post("/api/v1/wizard/scan_zones")
                    .json(&body)
                    .map_err(|e| e.to_string())?
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let v = resp
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| e.to_string())?;
                match v.get("zones").and_then(|z| z.as_array()) {
                    Some(zones) => Ok(zones.clone()),
                    None => Err(v
                        .get("detail")
                        .and_then(|d| d.as_str())
                        .unwrap_or("controller unreachable or kind not scannable")
                        .to_string()),
                }
            }
            .await;
            match result {
                Ok(zones) => {
                    let existing: Vec<String> = draft
                        .get_untracked()
                        .get("config")
                        .and_then(|c| c.get("zones"))
                        .and_then(|z| z.as_object())
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    let list: Vec<ScannedZone> = zones
                        .iter()
                        .filter_map(|z| {
                            let station_id = z.get("station_id")?.as_str()?.to_string();
                            let name = z.get("name")?.as_str()?.to_string();
                            let already_added = existing.contains(&zone_slug(&name));
                            Some(ScannedZone {
                                station_id,
                                name,
                                selected: !already_added,
                                already_added,
                            })
                        })
                        .collect();
                    if list.is_empty() {
                        scan_msg.set("No zones reported by this controller.".into());
                    }
                    scanned.set(list);
                }
                Err(e) => scan_msg.set(format!("Scan failed: {e}")),
            }
            scanning.set(false);
            let _ = cid;
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = body;
            let _ = cid;
        }
    };

    let id_for_import = id.clone();
    let on_import = move |_| {
        let picks: Vec<ScannedZone> = scanned
            .get_untracked()
            .into_iter()
            .filter(|z| z.selected && !z.already_added)
            .collect();
        if picks.is_empty() {
            scan_msg.set("Nothing selected to import.".into());
            return;
        }
        let count = picks.len();
        let cid = id_for_import.clone();
        draft.update(|d| {
            let Some(zones) = d
                .get_mut("config")
                .and_then(|c| c.get_mut("zones"))
                .and_then(|z| z.as_object_mut())
            else {
                return;
            };
            for z in &picks {
                let mut slug = zone_slug(&z.name);
                while zones.contains_key(&slug) {
                    slug.push('_');
                }
                zones.insert(slug, zone_stub(&z.name, &cid, &z.station_id));
            }
        });
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
        scanned.set(Vec::new());
        scan_msg.set(format!(
            "Imported {count} zone{}. Refine species and sprinklers in the Zones step.",
            if count == 1 { "" } else { "s" }
        ));
    };

    let scan_list = move || {
        let zones = scanned.get();
        if zones.is_empty() {
            return ().into_any();
        }
        let rows = zones
            .iter()
            .enumerate()
            .map(|(i, z)| {
                let label = if z.already_added {
                    format!("{} (station {}, already added)", z.name, z.station_id)
                } else {
                    format!("{} (station {})", z.name, z.station_id)
                };
                let checked = z.selected;
                let disabled = z.already_added;
                view! {
                    <label class="setup-scan__row">
                        <input
                            type="checkbox"
                            prop:checked=checked
                            prop:disabled=disabled
                            on:input=move |ev| {
                                let on = event_target_checked(&ev);
                                scanned.update(|list| {
                                    if let Some(slot) = list.get_mut(i) {
                                        slot.selected = on;
                                    }
                                });
                            }
                        />
                        <span>{label}</span>
                    </label>
                }
            })
            .collect_view();
        let n_selected = move || {
            scanned
                .get()
                .iter()
                .filter(|z| z.selected && !z.already_added)
                .count()
        };
        view! {
            <div class="setup-scan">
                <p class="setup-scan__title">"Zones found on this controller"</p>
                {rows}
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    on:click=on_import.clone()
                >
                    {move || format!("Import {} zone{}", n_selected(), if n_selected() == 1 { "" } else { "s" })}
                </button>
            </div>
        }
        .into_any()
    };

    view! {
        <li class="cond-row cond-row--stack">
            <div class="cond-row__head">
                <span class="cond-row__dot"></span>
                <div class="cond-row__text">
                    <span class="cond-row__name">{id.clone()}</span>
                    <span class="cond-row__sum">
                        {pretty}
                        {if is_default { " · default" } else { "" }}
                    </span>
                </div>
                <div class="cond-row__actions">
                    <button
                        type="button"
                        class="setup-footer__btn setup-footer__btn--ghost"
                        prop:disabled=move || testing.get()
                        on:click=on_test
                    >
                        {move || if testing.get() { "Testing…" } else { "Test" }}
                    </button>
                    <button
                        type="button"
                        class="setup-footer__btn setup-footer__btn--ghost"
                        prop:disabled=move || scanning.get()
                        on:click=on_scan
                    >
                        {move || if scanning.get() { "Scanning…" } else { "Scan zones" }}
                    </button>
                </div>
            </div>
            {move || {
                let m = test_msg.get();
                (!m.is_empty()).then(|| {
                    let cls = if test_ok.get() { "setup-test-result is-ok" } else { "setup-test-result is-err" };
                    view! { <p class=cls>{m}</p> }
                })
            }}
            {move || {
                let m = scan_msg.get();
                (!m.is_empty()).then(|| view! { <p class="setup-test-result">{m}</p> })
            }}
            {scan_list}
        </li>
    }
}
