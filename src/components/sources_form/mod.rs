// Reusable source add/edit form, shared by the Sensors hub (inline,
// no-navigation add/edit), the Settings raw editor, and the setup wizard.
// The panel owns only the draft signals + the assembled entry; the caller
// decides how to persist (config PUT for the hub/settings, wizard-draft
// PUT for setup), so the same widget serves every surface.

use leptos::prelude::*;

use crate::components::ui::{FormField, Panel, SegmentedControl};
use crate::docs::doc_url;

pub mod field_schema;
pub mod soil_forms;
pub use field_schema::{source_fields, SourceConfigForm};
pub use soil_forms::{EcowittSoilCalibration, MqttSoilSubscriptions};

/// Short, vendor-specific soil setup hint for the source kinds that carry soil
/// data. Returns nothing for kinds without a soil path, so the form stays lean.
/// Each hint names the exact LocalSky steps for that path and links to the
/// getting-started doc, so a rookie never has to guess at the wiring. Kept in
/// lockstep with `first-soil-sensor.md`.
fn soil_path_hint(kind: &str) -> impl IntoView {
    let text: Option<&'static str> = match kind {
        "ecowitt_gw_poll" => Some(
            "Ecowitt soil path: LocalSky polls your gateway over the LAN (no cloud). \
             Find the gateway's IP in the Ecowitt WS View app or your router, and pair \
             your soil probes to the gateway there. Set that IP as the host below; the \
             probes then appear as channels under Settings, Sensors, ready to bind to a zone.",
        ),
        "ecowitt_local" => Some(
            "Ecowitt push path: this is a push source, so set the listen path in the Ingest path \
             field above (and a Shared secret if you want one), then point the gateway's \
             Customized server (Ecowitt protocol) at that path. Probes paired to the gateway then \
             arrive as soil channels under Settings, Sensors, ready to bind to a zone.",
        ),
        "mqtt" => Some(
            "MQTT soil path: have your probe publish soil moisture to a topic, then add a \
             soil subscription below with that topic, the JSON field (if the payload is an \
             object), and the zone it measures. Finish by picking this source's channel as \
             the zone's soil sensor in the zone editor.",
        ),
        "ha_passthrough" => Some(
            "Home Assistant soil path: fill the Home Assistant URL and Long-lived token fields \
             above to bridge HA. Soil probes HA already owns are then bound straight from the \
             zone editor (Settings, Zones, pick the zone, Soil moisture sensor): they list as \
             ha:sensor.<entity>. The field_map (in the advanced JSON box) is for weather fields, \
             not soil.",
        ),
        _ => None,
    };
    text.map(|t| {
        view! {
            <p class="sensors-section__hint" style="margin-bottom: var(--space-3)">
                {t}
                " "
                <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                    style="color: var(--accent)">
                    "Add your first soil sensor →"
                </a>
            </p>
        }
    })
}

/// The source kinds the form offers, as (value, label) pairs.
pub fn kind_options() -> Vec<(String, String)> {
    [
        ("tempest_udp", "Tempest UDP"),
        ("tempest_ws", "Tempest cloud"),
        ("davis_wll", "Davis WLL"),
        ("ecowitt_local", "Ecowitt LAN (push)"),
        ("ecowitt_gw_poll", "Ecowitt gateway (poll)"),
        ("ambient_weather", "AmbientWeather"),
        ("netatmo", "Netatmo"),
        ("yolink", "YoLink"),
        ("lacrosse", "LaCrosse View"),
        ("tuya_cloud", "Tuya / RainPoint"),
        ("open_meteo", "Open-Meteo"),
        ("nws", "NWS (US)"),
        ("met_norway", "Met.no"),
        ("openweather", "OpenWeather"),
        ("pirate_weather", "PirateWeather"),
        ("mqtt", "MQTT"),
        ("http_webhook", "HTTP webhook"),
        ("ha_passthrough", "HA passthrough"),
        ("blitzortung", "Blitzortung lightning"),
        ("demo_replay", "Demo"),
    ]
    .into_iter()
    .map(|(v, l)| (v.to_string(), l.to_string()))
    .collect()
}

/// Icon registry name (ui::Icon) for a source kind.
pub fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "tempest_udp" | "tempest_ws" => "wind",
        "davis_wll" => "thermometer",
        "open_meteo" | "nws" | "openweather" | "pirate_weather" | "met_norway" => "cloud",
        "ecowitt_local" | "ecowitt_gw_poll" => "sources",
        "mqtt" => "download",
        "http_webhook" => "download",
        "ha_passthrough" => "home",
        "ambient_weather" => "cloud-sun",
        "netatmo" => "cloud-drizzle",
        "yolink" => "sources",
        "lacrosse" => "cloud-sun",
        "tuya_cloud" => "zap",
        "blitzortung" => "zap",
        "demo_replay" => "play",
        _ => "sources",
    }
}

pub fn kind_pretty(kind: &str) -> &'static str {
    match kind {
        "tempest_udp" => "Tempest UDP (LAN)",
        "tempest_ws" => "Tempest WebSocket (cloud)",
        "davis_wll" => "Davis WeatherLink Live",
        "open_meteo" => "Open-Meteo forecast",
        "nws" => "NWS forecast (US only)",
        "openweather" => "OpenWeather forecast",
        "pirate_weather" => "Pirate Weather forecast",
        "met_norway" => "MET Norway forecast",
        "ecowitt_local" => "Ecowitt local POST (push)",
        "ecowitt_gw_poll" => "Ecowitt gateway local-API poll",
        "mqtt" => "MQTT subscribe",
        "http_webhook" => "HTTP webhook receiver",
        "ha_passthrough" => "Home Assistant passthrough",
        "ambient_weather" => "Ambient Weather cloud",
        "netatmo" => "Netatmo cloud",
        "yolink" => "YoLink cloud",
        "lacrosse" => "La Crosse cloud",
        "tuya_cloud" => "Tuya / Smart Life cloud",
        "blitzortung" => "Blitzortung community lightning",
        "demo_replay" => "Demo replay (synthetic)",
        _ => "Unknown",
    }
}

pub fn default_config_text(kind: &str) -> String {
    match kind {
        "tempest_udp" => "{\n  \"bind_addr\": \"0.0.0.0:50222\"\n}".into(),
        "tempest_ws" => "{\n  \"access_token\": \"YOUR_TEMPEST_TOKEN\"\n}".into(),
        "davis_wll" => "{\n  \"host\": \"weatherlinklive.local\",\n  \"txid\": 1\n}".into(),
        "open_meteo" => "{\n  \"forecast_days\": 7,\n  \"forecast_hours\": 48,\n  \"past_days\": 1,\n  \"include_radar\": false\n}".into(),
        "nws" => "{\n  \"user_agent\": \"localsky/0.2 (you@example.com)\"\n}".into(),
        "met_norway" => "{\n  \"user_agent\": \"localsky/0.2 (you@example.com)\"\n}".into(),
        "openweather" => "{\n  \"api_key\": \"YOUR_OWM_KEY\"\n}".into(),
        "pirate_weather" => "{\n  \"api_key\": \"YOUR_PIRATE_KEY\"\n}".into(),
        "ambient_weather" => "{\n  \"app_key\": \"YOUR_APP_KEY\",\n  \"api_key\": \"YOUR_API_KEY\",\n  \"mac_address\": \"AA:BB:CC:DD:EE:FF\"\n}".into(),
        "netatmo" => "{\n  \"client_id\": \"YOUR_CLIENT_ID\",\n  \"client_secret\": \"YOUR_CLIENT_SECRET\",\n  \"refresh_token\": \"YOUR_REFRESH_TOKEN\",\n  \"device_id\": \"70:ee:50:00:11:22\"\n}".into(),
        "yolink" => "{\n  \"client_id\": \"YOUR_UAID\",\n  \"client_secret\": \"YOUR_SECRET\",\n  \"base_url\": \"https://api.yosmart.com\",\n  \"device_field_map\": [\n    {\n      \"field\": \"AirTempF\",\n      \"device_id\": \"<deviceId from Home.getDeviceList>\",\n      \"device_type\": \"THSensor\",\n      \"state_path\": \"temperature\",\n      \"scale\": 1.0,\n      \"offset\": 0.0\n    }\n  ]\n}".into(),
        "lacrosse" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"device_id\": null\n}".into(),
        "tuya_cloud" => "{\n  \"client_id\": \"YOUR_TUYA_ACCESS_ID\",\n  \"client_secret\": \"YOUR_TUYA_ACCESS_SECRET\",\n  \"base_url\": \"https://openapi.tuyaus.com\",\n  \"device_field_map\": [\n    {\n      \"field\": \"AirTempF\",\n      \"device_id\": \"<deviceId from tuya iot.tuya.com Devices tab>\",\n      \"status_code\": \"temp_current\",\n      \"scale\": 0.18,\n      \"offset\": 32.0\n    }\n  ]\n}".into(),
        "ecowitt_local" => "{\n  \"path\": \"/ingest/ecowitt\",\n  \"shared_secret\": null\n}".into(),
        "ecowitt_gw_poll" => "{\n  \"host\": \"192.0.2.50\",\n  \"poll_interval_s\": 30\n}".into(),
        "mqtt" => "{\n  \"broker_host\": \"broker.local\",\n  \"broker_port\": 1883,\n  \"username\": null,\n  \"password\": null,\n  \"subscriptions\": [\n    {\n      \"topic\": \"sensors/+/soil\",\n      \"field\": \"rh_pct\",\n      \"json_path\": \"moisture\",\n      \"scale\": 1.0,\n      \"offset\": 0.0\n    }\n  ]\n}".into(),
        "http_webhook" => "{\n  \"path\": \"/ingest/webhook/myhook\",\n  \"fields\": [\n    {\"field\": \"air_temp_f\", \"json_path\": \"temperature\", \"scale\": 1.0, \"offset\": 0.0}\n  ]\n}".into(),
        "ha_passthrough" => "{\n  \"base_url\": \"http://homeassistant.local:8123\",\n  \"bearer_token\": \"${HA_LONG_LIVED_TOKEN}\",\n  \"field_map\": {}\n}".into(),
        // enabled defaults false on purpose: Blitzortung.org community
        // data is CC BY-SA 4.0, private/non-commercial, display-only.
        // The operator flips it consciously; validation explains terms.
        "blitzortung" => "{\n  \"enabled\": false,\n  \"radius_mi\": 100.0\n}".into(),
        "demo_replay" => "{\n  \"rate\": 10.0\n}".into(),
        _ => "{}".into(),
    }
}

/// A self-contained add/edit form for one source. Seeds from `existing`
/// (None = add a new source). On save it parses the config JSON, assembles
/// the `{id, priority, enabled, kind, config}` entry, and hands it to
/// `on_commit`, the caller persists. `on_cancel` dismisses the form.
#[component]
pub fn SourceEditorPanel(
    #[prop(default = None)] existing: Option<serde_json::Value>,
    on_commit: Callback<serde_json::Value>,
    on_cancel: Callback<()>,
    /// Zone slugs (slug, display_name) offered in the MQTT soil-subscription
    /// per-zone binding dropdown. Empty by default (the dropdown then offers
    /// only "no zone"); the Sensors hub passes the live zone list.
    #[prop(optional, into)]
    zone_slugs: Option<Memo<Vec<(String, String)>>>,
) -> impl IntoView {
    // "edit" = the seed carries a real id (lock the id field). A seed with no
    // id but a kind/config (e.g. "adopt this discovered gateway") is a
    // prefilled ADD: the id stays editable and we keep the seeded config.
    let editing = existing
        .as_ref()
        .and_then(|s| s.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    // Read only by the hydrate-gated template-swap effect below.
    #[allow(unused_variables)]
    let has_seed_config = existing.as_ref().and_then(|s| s.get("config")).is_some();
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
        .unwrap_or("ecowitt_local")
        .to_string();
    let seed_priority = existing
        .as_ref()
        .and_then(|s| s.get("priority"))
        .and_then(|v| v.as_i64())
        .unwrap_or(50) as i32;
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
    let priority = RwSignal::new(seed_priority);
    let enabled = RwSignal::new(seed_enabled);
    let config_text = RwSignal::new(seed_config);
    let error = RwSignal::new(String::new());
    // Zone slugs for the MQTT soil-subscription per-zone dropdown. None = no
    // zone list supplied; default to empty so the dropdown still renders.
    let zone_slugs = zone_slugs.unwrap_or_else(|| Memo::new(|_| Vec::new()));

    // When composing a fresh source (not editing, no seeded config), swap the
    // JSON template as the kind changes. Skip when a config was seeded (adopt)
    // so the prefilled host isn't clobbered.
    #[cfg(feature = "hydrate")]
    if !editing && !has_seed_config {
        Effect::new(move |_| {
            let k = kind.get();
            config_text.set(default_config_text(&k));
        });
    }

    let on_save = move |_| {
        let id_v = id.get().trim().to_string();
        if id_v.is_empty() {
            error.set("Source id is required".into());
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
            "priority": priority.get(),
            "enabled": enabled.get(),
            "kind": kind.get(),
            "config": cfg_value,
        }));
    };

    view! {
        <div class="source-editor">
            <h3 class="source-editor__title">
                {if editing { "Edit sensor" } else { "Add a sensor" }}
            </h3>
            <FormField
                label="ID".to_string()
                helptext="snake_case, unique (e.g. ecowitt_gw, tempest_lan). Locked while editing.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="e.g. ecowitt_gw"
                    prop:value=move || id.get()
                    prop:disabled=editing
                    on:input=move |ev| id.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Kind".to_string()
                helptext="What protocol or service this sensor uses.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl value=kind options=kind_options() aria_label="Source kind".to_string()/>
            </FormField>

            <FormField
                label="Priority".to_string()
                helptext="Higher wins per-field. 100=LAN station, 50=cloud, 10=fallback.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    class="ui-input"
                    min="-100"
                    max="200"
                    prop:value=move || priority.get().to_string()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<i32>() {
                            priority.set(v);
                        }
                    }
                />
            </FormField>

            <FormField
                label="Enabled".to_string()
                helptext="Unchecked sensors stay configured but don't poll/receive.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || enabled.get()
                        on:input=move |ev| enabled.set(event_target_checked(&ev))
                    />
                    "Enable this sensor"
                </label>
            </FormField>

            // Structured soil forms for the kinds that previously required
            // hand-edited TOML. Each operates on the same `config_text` signal
            // the raw editor below uses, so the existing config PUT persists
            // them. The sync is two-way for the section each form owns
            // (subscriptions / soil_calibration): editing a card rewrites that
            // key in the textarea, and editing that key in the textarea
            // re-seeds the cards. The forms never touch the other keys
            // (broker/auth, host/poll), so the raw editor stays authoritative
            // for everything else.
            // PRIMARY editing surface: labeled, per-kind base-config fields
            // (host/port/base_url/tokens/api keys/poll cadence/model, ...),
            // rendered from the declarative field_schema and two-way-synced to
            // `config_text`. This is what an operator touches for every kind;
            // the JSON-advanced textarea below is the escape hatch for keys not
            // in the schema (and re-seeds these inputs when edited directly).
            <Panel title="Connection".to_string()>
                <SourceConfigForm config_text=config_text kind=Signal::derive(move || kind.get())/>
            </Panel>

            // Per-vendor soil setup hint, shown for the kinds that carry soil
            // data. Short, accurate, and specific to the path the user picked,
            // so a newcomer knows the exact steps before touching the form.
            {move || soil_path_hint(&kind.get())}

            {move || (kind.get() == "mqtt").then(|| view! {
                <Panel title="Soil subscriptions".to_string()>
                    <MqttSoilSubscriptions config_text=config_text zone_slugs=zone_slugs/>
                </Panel>
            })}
            {move || (kind.get() == "ecowitt_gw_poll").then(|| view! {
                <Panel title="Soil channel calibration".to_string()>
                    <EcowittSoilCalibration config_text=config_text/>
                </Panel>
            })}

            <FormField
                label="Config (JSON, advanced — optional)".to_string()
                helptext="Escape hatch for advanced keys not in the labeled Connection form above (and the nested soil forms). The labeled fields are the primary editor; this textarea stays in sync both ways, so you rarely need it. Use it for per-device maps (YoLink/Tuya), HA field_map, or hand-tuning. The template auto-fills as you change Kind (when adding).".to_string()
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
                let e = error.get();
                (!e.is_empty()).then(|| view! { <p class="source-editor__error">{e}</p> })
            }}

            <div class="settings-form-actions">
                <button type="button" class="setup-footer__btn setup-footer__btn--ghost" on:click=move |_| on_cancel.run(())>
                    "Cancel"
                </button>
                <button type="button" class="setup-footer__btn setup-footer__btn--primary" on:click=on_save>
                    {if editing { "Save changes" } else { "Add sensor" }}
                </button>
            </div>
        </div>
    }
}
