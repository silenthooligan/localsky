// SettingsSensors. The "Sensors lens" over the existing sources +
// controllers: it does not introduce any new schema, it just re-projects
// what LocalSky already ingests as a sensor-first view. Soil probes are
// grouped under the gateway/source they arrive through; flow meters under
// the controller that reports them. Each probe carries a bind-to-zone
// control that writes the chosen zone's `soil_sensor_id` and PUTs the
// full Config exactly the way the zone editor does (no new write path).
//
// Data: GET /api/v1/sensors/inventory  -> { gateways, soil, flow }
//       GET /api/config                -> for the zone list + the save
//       PUT /api/config                -> persist a binding
//
// Like every other settings component this is HTTP-only (gloo_net) so it
// compiles for both ssr and hydrate without touching any ssr-gated module.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::settings_ui::{SettingsLoadError, SettingsResult};
use crate::components::ui::{HelpHint, Icon, Panel};
use crate::components::units_fmt::{temp_unit, temp_value, use_unit_prefs, UnitPrefs};
use crate::docs::doc_url;

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Frontend mirrors of the /api/v1/sensors/inventory payload. Every optional
// field is `#[serde(default)]` so a null/missing key parses to None instead
// of failing the whole decode.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Deserialize)]
struct Inventory {
    #[serde(default)]
    gateways: Vec<Gateway>,
    #[serde(default)]
    soil: Vec<SoilProbe>,
    #[serde(default)]
    flow: Vec<FlowMeter>,
}

#[derive(Clone, Debug, Deserialize)]
struct Gateway {
    source_id: String,
    label: String,
    kind: String,
    #[serde(default)]
    online: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
struct SoilProbe {
    id: String,
    #[serde(default)]
    channel_label: Option<String>,
    source_id: String,
    #[serde(default)]
    source_label: Option<String>,
    #[serde(default)]
    source_kind: Option<String>,
    #[serde(default)]
    moisture_pct: Option<f64>,
    #[serde(default)]
    age_s: Option<f64>,
    #[serde(default)]
    battery_pct: Option<f64>,
    #[serde(default)]
    temp_f: Option<f64>,
    #[serde(default)]
    ec: Option<f64>,
    #[serde(default)]
    bound_zone_slug: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct FlowMeter {
    #[serde(default)]
    controller_label: Option<String>,
    #[serde(default)]
    controller_kind: Option<String>,
    /// CAPABLE: the controller type supports flow metering.
    #[serde(default)]
    supported: Option<bool>,
    /// CONNECTED: a flow sensor is actually wired to this device.
    #[serde(default)]
    connected: Option<bool>,
    /// LIVE: latest measured flow.
    #[serde(default)]
    gpm: Option<f64>,
    #[serde(default)]
    age_s: Option<f64>,
}

// A zone option for the bind dropdown: (config key / slug, display name).
type ZoneOpt = (String, String);

// ---------------------------------------------------------------------------
// HTTP (hydrate-only; SSR never runs these).
// ---------------------------------------------------------------------------

#[cfg(feature = "hydrate")]
async fn fetch_inventory() -> Result<Inventory, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/v1/sensors/inventory")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<Inventory>().await.map_err(|e| e.to_string())
}

#[cfg(feature = "hydrate")]
async fn fetch_config() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
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

// ---------------------------------------------------------------------------
// Small formatting helpers.
// ---------------------------------------------------------------------------

/// Human "moments ago" style age from a second count.
fn fmt_age(age_s: Option<f64>) -> String {
    match age_s {
        None => "no reading".to_string(),
        Some(s) if s < 90.0 => "just now".to_string(),
        Some(s) if s < 5400.0 => format!("{} min ago", (s / 60.0).round() as i64),
        Some(s) if s < 172_800.0 => format!("{} hr ago", (s / 3600.0).round() as i64),
        Some(s) => format!("{} days ago", (s / 86400.0).round() as i64),
    }
}

/// Friendly label for a source/gateway kind (snake_case from the API).
fn kind_label(kind: &str) -> String {
    match kind {
        "ecowitt_gw_poll" | "ecowitt" => "Ecowitt gateway".to_string(),
        "ecowitt_push" => "Ecowitt push".to_string(),
        "mqtt" => "MQTT".to_string(),
        "home_assistant" | "ha" => "Home Assistant".to_string(),
        "opensprinkler" => "OpenSprinkler".to_string(),
        "esphome" => "ESPHome".to_string(),
        "" => "Source".to_string(),
        other => other.replace('_', " "),
    }
}

// ---------------------------------------------------------------------------
// Component.
// ---------------------------------------------------------------------------

#[component]
pub fn SettingsSensors() -> impl IntoView {
    // Live inventory from the sensors API.
    let inventory: RwSignal<Inventory> = RwSignal::new(Inventory::default());
    // Full config, kept in sync so a bind mutation + PUT round-trips the
    // same way the zone editor does.
    let config_json: RwSignal<serde_json::Value> = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);
    // Some(err) when the initial inventory/config GET failed; replaces the
    // body with a Retry banner.
    let load_error: RwSignal<Option<String>> = RwSignal::new(None);
    let load_retry = RwSignal::new(0u32);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    // Per-device display-unit prefs (temp scale for the soil-probe temp chip).
    // Read inside the soil/orphan closures and threaded into the non-reactive
    // card/row fns as a prop, the way VerdictCell takes prefs.
    let prefs = use_unit_prefs();

    // Load on mount and on every Retry bump.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let _ = load_retry.get();
            wasm_bindgen_futures::spawn_local(async move {
                // Config first: without it we can't render the bind dropdown
                // or save, so a config failure is the hard "load failed".
                match fetch_config().await {
                    Ok(cfg) => {
                        config_json.set(cfg);
                        load_error.set(None);
                    }
                    Err(e) => {
                        load_error.set(Some(e));
                        loaded.set(true);
                        return;
                    }
                }
                // Inventory is best-effort: an empty/failed inventory still
                // renders the page (empty state), it just has no probes.
                if let Ok(inv) = fetch_inventory().await {
                    inventory.set(inv);
                }
                loaded.set(true);
            });
        });
    }

    // Zone options for the bind dropdown, read straight from the config the
    // same way the zone editor reads them (object keyed by slug, value has
    // display_name). Sorted for stable order.
    let zone_opts = move || -> Vec<ZoneOpt> {
        let cfg = config_json.get();
        let zones = cfg
            .get("zones")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        let mut opts: Vec<ZoneOpt> = zones
            .iter()
            .map(|(slug, z)| {
                let name = z
                    .get("display_name")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| slug.replace(['-', '_'], " "));
                (slug.clone(), name)
            })
            .collect();
        opts.sort_by_key(|o| o.1.to_lowercase());
        opts
    };

    // Bind a probe to a zone. `zone_key` empty -> unbind (null out whichever
    // zone currently holds this probe). Otherwise set that zone's
    // `soil_sensor_id` to the probe id (overwriting whatever it had), and
    // clear the probe off any OTHER zone so a probe maps to one zone. Then
    // PUT the full Config, exactly the zone-editor save path.
    let bind = Callback::new(move |(probe_id, zone_key): (String, String)| {
        if saving.get() {
            return;
        }
        config_json.update(|cfg| {
            if let Some(zones) = cfg.get_mut("zones").and_then(|z| z.as_object_mut()) {
                for (slug, z) in zones.iter_mut() {
                    let Some(obj) = z.as_object_mut() else {
                        continue;
                    };
                    let holds_this = obj
                        .get("soil_sensor_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s == probe_id)
                        .unwrap_or(false);
                    if slug == &zone_key {
                        obj.insert(
                            "soil_sensor_id".into(),
                            serde_json::Value::String(probe_id.clone()),
                        );
                    } else if holds_this {
                        // Another zone had this probe; a probe binds to one
                        // zone, so release it there.
                        obj.insert("soil_sensor_id".into(), serde_json::Value::Null);
                    }
                }
                // Unbind: the loop above already nulled the holder.
            }
        });
        let candidate = config_json.get();
        saving.set(true);
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        wasm_bindgen_futures::spawn_local(async move {
            match save_config(candidate).await {
                Ok(()) => {
                    crate::components::settings_ui::toast_saved(
                        result_msg,
                        result_ok,
                        "Binding saved. Engine uses it on the next tick.",
                    );
                    // Refresh inventory so bound_zone_name reflects the change.
                    if let Ok(inv) = fetch_inventory().await {
                        inventory.set(inv);
                    }
                }
                Err(e) => {
                    result_ok.set(false);
                    result_msg.set(e);
                }
            }
            saving.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            saving.set(false);
            let _ = candidate;
        }
    });

    // SOIL: one card per gateway/source, its probes nested inside.
    let soil_view = move || {
        let inv = inventory.get();
        let opts = zone_opts();
        let p = prefs.get();
        let cards: Vec<_> = inv
            .gateways
            .iter()
            .map(|gw| {
                let children: Vec<SoilProbe> = inv
                    .soil
                    .iter()
                    .filter(|p| p.source_id == gw.source_id)
                    .cloned()
                    .collect();
                gateway_card(gw.clone(), children, opts.clone(), bind, p)
            })
            .collect();
        view! { <div class="soil-grid soil-grid--gateways">{cards}</div> }.into_any()
    };

    // Probes whose source_id isn't in gateways[] still deserve a home, so
    // they don't silently vanish. Group them under a synthetic "Other
    // sources" card keyed by their own source label.
    let orphan_view = move || {
        let inv = inventory.get();
        let gw_ids: Vec<String> = inv.gateways.iter().map(|g| g.source_id.clone()).collect();
        let orphans: Vec<SoilProbe> = inv
            .soil
            .iter()
            .filter(|p| !gw_ids.contains(&p.source_id))
            .cloned()
            .collect();
        if orphans.is_empty() {
            return ().into_any();
        }
        let opts = zone_opts();
        let p = prefs.get();
        // Synthesize a gateway header from the first orphan's source meta.
        let first = &orphans[0];
        let synth = Gateway {
            source_id: first.source_id.clone(),
            label: first
                .source_label
                .clone()
                .unwrap_or_else(|| "Other sources".to_string()),
            kind: first.source_kind.clone().unwrap_or_default(),
            online: None,
        };
        view! {
            <div class="soil-grid soil-grid--gateways">
                {gateway_card(synth, orphans, opts, bind, p)}
            </div>
        }
        .into_any()
    };

    // FLOW: one card per controller that SUPPORTS flow (omitting controllers
    // with no flow input at all), or a help line when there are no
    // flow-capable controllers. A supported card narrates its own state:
    // live gpm, connected-idle, or supported-but-none-connected with how-to.
    let flow_view = move || {
        let inv = inventory.get();
        let supported: Vec<FlowMeter> = inv
            .flow
            .iter()
            .filter(|f| f.supported.unwrap_or(false))
            .cloned()
            .collect();
        if supported.is_empty() {
            return view! {
                <p class="sensors-section__hint">
                    "No flow-capable controller. Flow metering needs a controller with a flow "
                    "input, such as OpenSprinkler. Wire a pulse flow sensor to its FLOW input "
                    "and set the K-factor on the device; LocalSky reads it automatically. "
                    <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">
                        "Flow meters: capable vs connected →"
                    </a>
                </p>
            }
            .into_any();
        }
        let cards: Vec<_> = supported.into_iter().map(flow_card).collect();
        view! { <div class="soil-grid soil-grid--gateways">{cards}</div> }.into_any()
    };

    // True when there is genuinely nothing to show in the soil lens.
    let soil_empty = move || {
        let inv = inventory.get();
        inv.gateways.is_empty() && inv.soil.is_empty()
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Sensors"<HelpHint topic="soil-sensors"/></h1>
                <p class="settings-page__subtitle">
                    "Soil probes and flow meters LocalSky reads, grouped by the gateway or controller they come through."
                </p>
            </header>

            {model_explainer()}

            <Show
                when=move || load_error.get().is_none()
                fallback=move || view! { <SettingsLoadError error=load_error retry=load_retry/> }
            >
                <Show
                    when=move || loaded.get()
                    fallback=|| view! { <p class="settings-empty">"Loading sensors…"</p> }
                >
                    {move || {
                        if soil_empty() {
                            // EMPTY STATE: warm explainer + the three ways in.
                            empty_state().into_any()
                        } else {
                            view! {
                                <Panel title="Soil probes".to_string()>
                                    <p class="sensors-section__hint" style="margin-bottom: var(--space-4)">
                                        "Grouped by the gateway or source each probe reports through. "
                                        "Bind a probe to a zone to let it drive that zone's skip decision."
                                    </p>
                                    {soil_view}
                                    {orphan_view}
                                </Panel>
                            }
                            .into_any()
                        }
                    }}

                    <Panel title="Flow meters".to_string()>
                        {flow_view}
                    </Panel>

                    <SettingsResult result_msg=result_msg result_ok=result_ok/>
                </Show>
            </Show>
        </div>
    }
}

// ---------------------------------------------------------------------------
// Pieces (plain fns, kept out of the page component to keep each view tree
// inside its own monomorphization boundary, per the no-deep-nesting rule).
// ---------------------------------------------------------------------------

/// One gateway/source card with its soil probes nested inside.
fn gateway_card(
    gw: Gateway,
    probes: Vec<SoilProbe>,
    zone_opts: Vec<ZoneOpt>,
    bind: Callback<(String, String)>,
    prefs: UnitPrefs,
) -> impl IntoView {
    let count = probes.len();
    let count_label = format!("{count} channel{}", if count == 1 { "" } else { "s" });
    let kind = kind_label(&gw.kind);
    let online = gw.online;
    let dot_class = match online {
        Some(true) => "source-health__dot",
        Some(false) => "source-health__dot",
        None => "source-health__dot",
    };
    let dot_style = match online {
        Some(true) => "background: var(--verdict-run)",
        Some(false) => "background: var(--danger, #e5484d)",
        None => "background: var(--text-faint)",
    };
    let online_text = match online {
        Some(true) => "Online",
        Some(false) => "Offline",
        None => "Unknown",
    };

    let rows: Vec<_> = probes
        .into_iter()
        .map(|p| soil_probe_row(p, zone_opts.clone(), bind, prefs))
        .collect();

    view! {
        <section class="sensor-detail-card">
            <div class="soil-card__head">
                <span class="soil-card__name">{gw.label}</span>
                <span class="source-health__kind">{kind}</span>
            </div>
            <div class="source-health__status" style="display:flex; align-items:center; gap:0.5rem">
                <span class=dot_class style=dot_style></span>
                <span>{online_text}</span>
                <span style="color: var(--text-faint)">"·"</span>
                <span>{count_label}</span>
            </div>
            <div class="soil-grid">
                {rows}
            </div>
        </section>
    }
}

/// One soil probe: live reading, optional chips, and the bind-to-zone select.
fn soil_probe_row(
    p: SoilProbe,
    zone_opts: Vec<ZoneOpt>,
    bind: Callback<(String, String)>,
    prefs: UnitPrefs,
) -> impl IntoView {
    let probe_id = p.id.clone();
    let label = p
        .channel_label
        .clone()
        .unwrap_or_else(|| p.id.replace(['-', '_'], " "));
    let reading = p
        .moisture_pct
        .map(|m| format!("{m:.1}%"))
        .unwrap_or_else(|| "-".to_string());
    let age = fmt_age(p.age_s);
    let bound_slug = p.bound_zone_slug.clone().unwrap_or_default();

    // Small chips: battery, temp, EC, shown only when present.
    let mut chips: Vec<_> = Vec::new();
    if let Some(b) = p.battery_pct {
        chips.push(view! {
            <span class="soil-card__pill" style="--sc: var(--verdict-run)">
                {format!("Battery {b:.0}%")}
            </span>
        });
    }
    if let Some(t) = p.temp_f {
        // Source value is Fahrenheit; route through the display-unit formatter
        // so it honors the temp-scale pref (value + °F/°C unit split).
        let temp_chip = format!("{}{}", temp_value(t, prefs), temp_unit(prefs));
        chips.push(view! {
            <span class="soil-card__pill" style="--sc: var(--accent)">
                {temp_chip}
            </span>
        });
    }
    if let Some(e) = p.ec {
        chips.push(view! {
            <span class="soil-card__pill" style="--sc: var(--accent)">
                {format!("EC {e:.0}")}
            </span>
        });
    }

    // Bind dropdown: a "Not bound" sentinel plus every zone. The currently
    // bound zone is pre-selected. on:change writes through the bind callback.
    let bind_id = probe_id.clone();
    let opts_for_select = zone_opts.clone();
    let selected_slug = bound_slug.clone();

    view! {
        <div class="soil-card" style="--sc: var(--accent)">
            <div class="soil-card__head">
                <span class="soil-card__name">{label}</span>
                <span class="soil-card__pill">{age}</span>
            </div>
            <div class="soil-card__value">{reading}</div>
            <div class="zone-soil-live" style="margin-top:0; flex-wrap:wrap">
                {chips}
            </div>
            <label class="sensor-readout__k" style="margin-top:0.4rem">"Bound zone"</label>
            <select
                class="ui-input"
                on:change=move |ev| {
                    bind.run((bind_id.clone(), event_target_value(&ev)));
                }
            >
                <option value="" selected=selected_slug.is_empty()>
                    "Not bound"
                </option>
                {opts_for_select
                    .into_iter()
                    .map(|(slug, name)| {
                        let sel = slug == selected_slug;
                        view! { <option value=slug selected=sel>{name}</option> }
                    })
                    .collect_view()}
            </select>
        </div>
    }
}

/// One flow meter card, distinguishing CAPABLE / CONNECTED / LIVE:
///   supported && connected && gpm>0  -> "Flow meter: X.X gpm"
///   supported && connected && idle   -> "Flow meter connected (idle)"
///   supported && !connected          -> "Flow meter supported. None connected." + how-to-wire
///   !supported                       -> muted "No flow input" (normally filtered upstream)
fn flow_card(f: FlowMeter) -> impl IntoView {
    let label = f
        .controller_label
        .clone()
        .unwrap_or_else(|| "Controller".to_string());
    let raw_kind = f.controller_kind.clone().unwrap_or_default();
    let kind = kind_label(&raw_kind);
    let supported = f.supported.unwrap_or(false);
    let connected = f.connected.unwrap_or(false);
    let flowing = f.gpm.map(|g| g > 0.0).unwrap_or(false);
    let age = fmt_age(f.age_s);

    // Classify into the four states; pick the readout, status line, and
    // whether to attach the wire-up help. Only connected cards show an age.
    let (state_label, value, show_age, show_help) = match (supported, connected, flowing) {
        (true, true, true) => {
            let g = f.gpm.unwrap_or(0.0);
            ("Flow meter".to_string(), format!("{g:.1} gpm"), true, false)
        }
        (true, true, false) => (
            "Flow meter connected (idle)".to_string(),
            "0.0 gpm".to_string(),
            true,
            false,
        ),
        // Supported but not connected. Defensive: if a gpm is still reported
        // (shouldn't happen once an adapter sets connected from a live reading,
        // but don't silently drop a real number), surface it instead of a dash.
        (true, false, _) => match f.gpm {
            Some(g) => (
                "Flow meter supported. None connected.".to_string(),
                format!("{g:.1} gpm"),
                false,
                true,
            ),
            None => (
                "Flow meter supported. None connected.".to_string(),
                "-".to_string(),
                false,
                true,
            ),
        },
        (false, _, _) => ("No flow input".to_string(), "-".to_string(), false, false),
    };

    let age_pill = show_age.then(|| view! { <span class="soil-card__pill">{age}</span> });
    // Wire-up help is controller-kind-aware: OpenSprinkler gets its specific
    // FLOW-input + K-factor steps; other controllers get a generic line since
    // their flow wiring differs (and we don't want to send them to the wrong
    // terminal).
    let help = show_help.then(|| {
        let help_text = if raw_kind == "opensprinkler" {
            "Wire a pulse flow sensor to your OpenSprinkler FLOW input and set the \
             K-factor on the device; LocalSky reads it automatically."
        } else {
            "Connect a flow sensor supported by this controller; LocalSky reads it \
             automatically once the controller reports it."
        };
        view! {
            <p class="sensors-section__hint" style="margin-top:0.4rem">
                {help_text}
                " "
                <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                    style="color: var(--accent)">
                    "How to connect a flow meter →"
                </a>
            </p>
        }
    });

    view! {
        <section class="sensor-detail-card">
            <div class="soil-card__head">
                <span class="soil-card__name">{label}</span>
                <span class="source-health__kind">{kind}</span>
            </div>
            <div class="soil-card__head">
                <span class="source-health__status">{state_label}</span>
                {age_pill}
            </div>
            <div class="soil-card__value" style="display:flex; align-items:center; gap:0.5rem">
                <Icon name="gauge".to_string() size=22/>
                {value}
            </div>
            {help}
        </section>
    }
}

/// The three-tier model, taught right where a newcomer first meets it.
/// Controllers open valves; sources/gateways bring data in; sensors are the
/// probes/meters those carry. Kept to a few sentences so it informs without
/// crowding the page, and links to the getting-started doc.
fn model_explainer() -> impl IntoView {
    view! {
        <p class="sensors-section__hint" style="margin: 0 0 var(--space-4)">
            "How the pieces fit: "<strong>"controllers"</strong>" open valves, "
            <strong>"sources"</strong>" (a weather station, an Ecowitt gateway, a forecast, "
            "an MQTT broker, or Home Assistant) bring data in, and "<strong>"sensors"</strong>
            " are the probes and meters those carry, such as a soil probe on a gateway or a "
            "flow meter on a controller. Add a source under Devices and its sensors show up here. "
            <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                style="color: var(--accent)">
                "Add your first soil sensor →"
            </a>
        </p>
    }
}

/// Warm, brief explainer shown when there are no soil sensors at all.
fn empty_state() -> impl IntoView {
    view! {
        <Panel title="Soil probes".to_string()>
            <div class="sensors-empty">
                <p style="margin:0 0 var(--space-3); color: var(--text-bright); font-weight: 600">
                    "No soil sensors yet."
                </p>
                <p style="margin:0 0 var(--space-3)">
                    "LocalSky can read soil moisture from a few places. Add one and it shows up here, "
                    "ready to bind to a zone:"
                </p>
                <ul class="sensors-empty__ways">
                    <li>
                        <strong>"Ecowitt gateway"</strong>
                        " on the LAN (recommended, cheapest). Adopt it under Devices and "
                        "LocalSky polls every soil channel natively."
                    </li>
                    <li>
                        <strong>"Any MQTT-published probe"</strong>
                        ". Add an MQTT source, point a soil subscription at the topic, and "
                        "bind it to a zone."
                    </li>
                    <li>
                        <strong>"A Home Assistant soil entity"</strong>
                        ". Bridge HA, then pick the probe from the zone editor; it mirrors "
                        "in automatically."
                    </li>
                </ul>
                <p style="margin: var(--space-3) 0 0">
                    <a href="/settings?section=devices&add=source" style="color: var(--accent)">
                        "Add a sensor in Devices →"
                    </a>
                    "  ·  "
                    <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                        style="color: var(--accent)">
                        "Add your first soil sensor"
                    </a>
                </p>
            </div>
        </Panel>
    }
}
