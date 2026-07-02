// G7 field-mapping UI: a labeled, point-and-click editor for the `fields`
// array of the HTTP-webhook RECEIVER and the generic REST POLLER (both use the
// identical HttpWebhookField shape). Each row maps a JSON path in the device's
// payload to a LocalSky WeatherField (or, when bound to a zone, to that zone's
// soil channel) with a linear scale/offset. This retires the raw-JSON box for
// the two most generic "any JSON API" sources, so wiring WeatherUnderground,
// Tomorrow.io, a DIY ESP, etc. never requires hand-editing JSON.
//
// Built on the same two-way `config_text` round-trip the soil forms use
// (parse -> rows -> edit -> flush back), with a self_edit guard so an external
// advanced-textarea edit re-seeds the cards but our own flush does not.

use leptos::prelude::*;

use crate::components::ui::{Button, FormField};

/// (WeatherField value, label) pairs for the "Reading" dropdown. Every value
/// parses via mqtt_subscribe::parse_weather_field, so the adapter ingests it
/// (enforced by a unit test). Zone-bound rows ignore this (soil channel).
pub const WEATHER_FIELD_OPTIONS: &[(&str, &str)] = &[
    ("air_temp_f", "Air temperature (°F)"),
    ("rh_pct", "Humidity (%)"),
    ("dew_point_f", "Dew point (°F)"),
    ("wind_mph", "Wind (mph)"),
    ("wind_gust_mph", "Wind gust (mph)"),
    ("wind_bearing_deg", "Wind direction (°)"),
    ("pressure_in_hg", "Pressure (inHg)"),
    ("solar_w_m2", "Solar (W/m²)"),
    ("uv_index", "UV index"),
    ("rain_today_in", "Rain today (in)"),
    ("rain_intensity_in_hr", "Rain rate (in/hr)"),
    ("et0_today", "ET₀ today (mm)"),
    ("flow_gpm", "Flow (gpm)"),
    ("flow_total_gal_today", "Flow total today (gal)"),
    ("leaf_wetness_pct", "Leaf wetness (%)"),
];

/// Default reading for a freshly-added mapping row.
pub const DEFAULT_FIELD: &str = "air_temp_f";

/// One HttpWebhookField row, mirrored as Strings for two-way text-input binding.
/// Empty `json_path`/`zone_slug` serialize back to JSON null (the
/// `Option<String>` absent shape).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FieldMapRow {
    pub field: String,
    pub json_path: String,
    pub zone_slug: String,
    pub scale: f64,
    pub offset: f64,
}

impl FieldMapRow {
    /// A blank mapping seed: air temperature preselected, identity transform.
    pub fn new_default() -> Self {
        Self {
            field: DEFAULT_FIELD.to_string(),
            json_path: String::new(),
            zone_slug: String::new(),
            scale: 1.0,
            offset: 0.0,
        }
    }
}

/// Parse an HttpWebhookConfig / RestPollConfig JSON object's `fields` array into
/// rows. Missing/empty array -> empty Vec; unknown shapes are skipped, not
/// errors, so a partially hand-edited config never wedges the form.
pub fn parse_field_map(config: &serde_json::Value) -> Vec<FieldMapRow> {
    config
        .get("fields")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| FieldMapRow {
                    field: s
                        .get("field")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    json_path: s
                        .get("json_path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    zone_slug: s
                        .get("zone_slug")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    scale: s.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0),
                    offset: s.get("offset").and_then(|v| v.as_f64()).unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Serialize one row to the exact HttpWebhookField JSON shape. Blank
/// `json_path`/`zone_slug` become JSON null (Option<String> = None).
fn field_map_to_json(row: &FieldMapRow) -> serde_json::Value {
    let opt = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(t.to_string())
        }
    };
    serde_json::json!({
        "field": row.field.trim(),
        "json_path": opt(&row.json_path),
        "zone_slug": opt(&row.zone_slug),
        "scale": row.scale,
        "offset": row.offset,
    })
}

/// Write rows back into the config's `fields` key, preserving every other key
/// (url, method, headers, body, ...). A row is dropped when it maps NOTHING:
/// no weather field AND no zone (the adapter would ignore it anyway).
pub fn apply_field_map(config: &mut serde_json::Value, rows: &[FieldMapRow]) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let fields: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| !r.field.trim().is_empty() || !r.zone_slug.trim().is_empty())
        .map(field_map_to_json)
        .collect();
    if let Some(obj) = config.as_object_mut() {
        obj.insert("fields".into(), serde_json::Value::Array(fields));
    }
}

/// FIELD-MAPPING FORM for http_webhook + rest_poll. Reads `fields` out of the
/// bound `config_text`, renders one editable card per mapping, and flushes every
/// change back so the surrounding save (config PUT) persists it. No JSON
/// required to map a generic weather API or DIY device.
#[component]
pub fn WeatherFieldMapEditor(
    /// The source's raw config JSON text, shared with the raw editor.
    config_text: RwSignal<String>,
    /// Zone slugs offered in the per-row zone binding dropdown (for soil).
    zone_slugs: Memo<Vec<(String, String)>>,
) -> impl IntoView {
    let rows = RwSignal::new(parse_field_map(
        &serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::Value::Null),
    ));

    // Guard flag (see soil_forms.rs for the full rationale): flush() drops
    // empty rows, so a value-based guard would re-seed and delete an in-progress
    // "+ Add" row. This flag lets the Effect skip config_text changes WE caused.
    let self_edit = RwSignal::new(false);

    let flush = move || {
        let mut cfg: serde_json::Value =
            serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::json!({}));
        apply_field_map(&mut cfg, &rows.get_untracked());
        self_edit.set(true);
        config_text.set(serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".into()));
    };

    Effect::new(move |_| {
        let text = config_text.get();
        if self_edit.get_untracked() {
            self_edit.set(false);
            return;
        }
        let parsed =
            parse_field_map(&serde_json::from_str(&text).unwrap_or(serde_json::Value::Null));
        if parsed != rows.get_untracked() {
            rows.set(parsed);
        }
    });

    let add_row = move |_| {
        rows.update(|r| r.push(FieldMapRow::new_default()));
        flush();
    };

    view! {
        <div class="soil-subs">
            <p class="sensors-section__hint">
                "Each mapping pulls one number out of the API's JSON response and feeds it to a "
                "LocalSky reading. Set the JSON path to the value (e.g. "<code>"current.temp_f"</code>
                " or "<code>"observations.0.imperial.temp"</code>"), pick what it measures, and add a "
                "scale/offset if the units differ (°C→°F is scale 1.8, offset 32). To feed a soil "
                "probe instead, bind the row to a zone; finish in the zone editor by picking this "
                "source's channel as that zone's soil sensor."
            </p>
            {move || {
                let rs = rows.get();
                rs.into_iter().enumerate().map(|(i, row)| {
                    let field = row.field.clone();
                    let json_path = row.json_path.clone();
                    let zone = row.zone_slug.clone();
                    let scale = row.scale;
                    let offset = row.offset;
                    let path_for_err = json_path.clone();
                    let is_soil = !zone.trim().is_empty();

                    let set_field = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.field = v; } });
                        flush();
                    };
                    let set_path = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.json_path = v; } });
                        flush();
                    };
                    let set_zone = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.zone_slug = v; } });
                        flush();
                    };
                    let set_scale = move |ev: leptos::ev::Event| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            rows.update(|r| { if let Some(row) = r.get_mut(i) { row.scale = v; } });
                            flush();
                        }
                    };
                    let set_offset = move |ev: leptos::ev::Event| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            rows.update(|r| { if let Some(row) = r.get_mut(i) { row.offset = v; } });
                            flush();
                        }
                    };
                    let remove = move |_| {
                        rows.update(|r| { if i < r.len() { r.remove(i); } });
                        flush();
                    };

                    view! {
                        <div class="soil-sub-card">
                            <FormField
                                label="JSON path".to_string()
                                helptext="Dot/index path to the value in the API response (e.g. current.temp_f, data.0.value). Leave blank if the body is a bare number.".to_string()
                                error=Signal::derive(move || (path_for_err.trim().is_empty()).then(|| "A JSON path is usually required".to_string()))
                            >
                                <input
                                    type="text"
                                    class="ui-input"
                                    placeholder="e.g. current.temp_f"
                                    prop:value=json_path
                                    on:input=set_path
                                />
                            </FormField>
                            <FormField
                                label="Reading".to_string()
                                helptext="Which LocalSky reading this value feeds. Ignored when a zone is bound below (then it's a soil channel).".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <select class="ui-input" prop:disabled=is_soil on:change=set_field>
                                    {WEATHER_FIELD_OPTIONS.iter().map(|(val, label)| {
                                        let val = val.to_string();
                                        let sel = field == val;
                                        view! { <option value=val.clone() selected=sel>{label.to_string()}</option> }
                                    }).collect_view()}
                                </select>
                            </FormField>
                            <FormField
                                label="Bind to zone (optional)".to_string()
                                helptext="Set this only for a soil probe. When bound, the value becomes that zone's own soil channel (not a global reading); pick it as the zone's soil sensor in the zone editor.".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <select class="ui-input" on:change=set_zone>
                                    <option value="" selected=zone.is_empty()>
                                        "(not soil, a weather reading)"
                                    </option>
                                    {zone_slugs.get().into_iter().map(|(slug, name)| {
                                        let sel = zone == slug;
                                        view! { <option value=slug.clone() selected=sel>{name}</option> }
                                    }).collect_view()}
                                </select>
                            </FormField>
                            <div class="soil-sub-card__row">
                                <FormField
                                    label="Scale".to_string()
                                    helptext="value × scale + offset. 1 = as-reported.".to_string()
                                    error=Signal::derive(|| None::<String>)
                                >
                                    <input type="number" class="ui-input" step="any"
                                        prop:value=scale.to_string() on:input=set_scale/>
                                </FormField>
                                <FormField
                                    label="Offset".to_string()
                                    helptext="Added after scaling. 0 = none.".to_string()
                                    error=Signal::derive(|| None::<String>)
                                >
                                    <input type="number" class="ui-input" step="any"
                                        prop:value=offset.to_string() on:input=set_offset/>
                                </FormField>
                            </div>
                            <Button variant="danger" on_click=Callback::new(remove)>
                                "Remove mapping"
                            </Button>
                        </div>
                    }
                }).collect_view()
            }}
            <Button variant="ghost" on_click=Callback::new(add_row)>
                "+ Add a field mapping"
            </Button>
        </div>
    }
}

// ===================================================================
// Group B: device-based maps (YoLink + Tuya), array `device_field_map`
// ===================================================================
//
// YoLink and Tuya share a per-device mapping shape: (WeatherField | zone) <-
// (device_id + a path into that device's state) + scale/offset. The path column
// differs by vendor -- YoLink has a device_type plus a state path; Tuya has a
// single status code (DP) -- so one row struct carries both and parse/apply
// branch on `kind`.

/// One device_field_map row for YoLink/Tuya, mirrored as Strings for binding.
/// `device_type` is YoLink-only; `path` is the YoLink state path or the Tuya
/// status code depending on kind.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeviceFieldRow {
    pub field: String,
    pub zone_slug: String,
    pub device_id: String,
    pub device_type: String,
    pub path: String,
    pub scale: f64,
    pub offset: f64,
}

impl DeviceFieldRow {
    pub fn new_default() -> Self {
        Self {
            field: DEFAULT_FIELD.to_string(),
            zone_slug: String::new(),
            device_id: String::new(),
            device_type: String::new(),
            path: String::new(),
            scale: 1.0,
            offset: 0.0,
        }
    }
}

/// Parse a YolinkConfig / TuyaCloudConfig JSON object's `device_field_map` array
/// into rows. `kind` is "yolink" or "tuya"; the path column reads from
/// `state_path` (YoLink) or `status_code` (Tuya).
pub fn parse_device_field_map(config: &serde_json::Value, kind: &str) -> Vec<DeviceFieldRow> {
    let path_key = if kind == "tuya_cloud" {
        "status_code"
    } else {
        "state_path"
    };
    config
        .get("device_field_map")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| DeviceFieldRow {
                    field: s
                        .get("field")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    zone_slug: s
                        .get("zone_slug")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    device_id: s
                        .get("device_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    device_type: s
                        .get("device_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    path: s
                        .get(path_key)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    scale: s.get("scale").and_then(|v| v.as_f64()).unwrap_or(1.0),
                    offset: s.get("offset").and_then(|v| v.as_f64()).unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Write rows back into `device_field_map`, preserving credentials/base_url. A
/// row is dropped unless it has a device_id, a path, and either a field or a
/// zone (the schema requires device_id + path, and a target-less row is inert).
pub fn apply_device_field_map(config: &mut serde_json::Value, rows: &[DeviceFieldRow], kind: &str) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let is_tuya = kind == "tuya_cloud";
    let opt = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(t.to_string())
        }
    };
    let maps: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| {
            !r.device_id.trim().is_empty()
                && !r.path.trim().is_empty()
                && (!r.field.trim().is_empty() || !r.zone_slug.trim().is_empty())
        })
        .map(|r| {
            let mut obj = serde_json::Map::new();
            obj.insert(
                "field".into(),
                serde_json::Value::String(r.field.trim().into()),
            );
            obj.insert("zone_slug".into(), opt(&r.zone_slug));
            obj.insert(
                "device_id".into(),
                serde_json::Value::String(r.device_id.trim().into()),
            );
            if is_tuya {
                obj.insert(
                    "status_code".into(),
                    serde_json::Value::String(r.path.trim().into()),
                );
            } else {
                obj.insert(
                    "device_type".into(),
                    serde_json::Value::String(r.device_type.trim().into()),
                );
                obj.insert(
                    "state_path".into(),
                    serde_json::Value::String(r.path.trim().into()),
                );
            }
            obj.insert("scale".into(), serde_json::json!(r.scale));
            obj.insert("offset".into(), serde_json::json!(r.offset));
            serde_json::Value::Object(obj)
        })
        .collect();
    if let Some(obj) = config.as_object_mut() {
        obj.insert("device_field_map".into(), serde_json::Value::Array(maps));
    }
}

/// DEVICE FIELD-MAP FORM for YoLink + Tuya. Reads `device_field_map` from the
/// bound `config_text` and flushes edits back, so the config PUT persists it.
#[component]
pub fn DeviceFieldMapEditor(
    config_text: RwSignal<String>,
    zone_slugs: Memo<Vec<(String, String)>>,
    /// "yolink" or "tuya_cloud" -- selects the path column + JSON key.
    #[prop(into)]
    kind: String,
) -> impl IntoView {
    // Capture a Copy bool (not the String kind) so `flush` stays Copy and can be
    // shared into every per-row setter closure. The JSON key is derived from it.
    let is_tuya = kind == "tuya_cloud";
    let kind_str = move || if is_tuya { "tuya_cloud" } else { "yolink" };
    let rows = RwSignal::new(parse_device_field_map(
        &serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::Value::Null),
        kind_str(),
    ));
    let self_edit = RwSignal::new(false);

    let flush = move || {
        let mut cfg: serde_json::Value =
            serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::json!({}));
        apply_device_field_map(&mut cfg, &rows.get_untracked(), kind_str());
        self_edit.set(true);
        config_text.set(serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".into()));
    };

    Effect::new(move |_| {
        let text = config_text.get();
        if self_edit.get_untracked() {
            self_edit.set(false);
            return;
        }
        let parsed = parse_device_field_map(
            &serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
            kind_str(),
        );
        if parsed != rows.get_untracked() {
            rows.set(parsed);
        }
    });

    let add_row = move |_| {
        rows.update(|r| r.push(DeviceFieldRow::new_default()));
        flush();
    };

    let path_label = if is_tuya {
        "Status code (DP)"
    } else {
        "State path"
    };
    let path_help = if is_tuya {
        "Tuya status code / DP for this reading (e.g. temp_current, humi_current, water_total)."
    } else {
        "Dot path into the device state, rooted at data.state (e.g. temperature, waterFlow)."
    };
    let path_ph = if is_tuya {
        "e.g. temp_current"
    } else {
        "e.g. temperature"
    };

    view! {
        <div class="soil-subs">
            <p class="sensors-section__hint">
                {if is_tuya {
                    "Each mapping reads one status code from a Tuya device. Get device ids from your \
                     Tuya IoT project's Devices tab; the status code (DP) is the value's key. Bind to a \
                     zone for a soil probe, or pick a weather reading."
                } else {
                    "Each mapping reads one value from a YoLink device's state. Get the device id from \
                     Home.getDeviceList; device type composes the {Type}.getState call (e.g. THSensor). \
                     Bind to a zone for a soil probe, or pick a weather reading."
                }}
            </p>
            {move || {
                let rs = rows.get();
                rs.into_iter().enumerate().map(|(i, row)| {
                    let field = row.field.clone();
                    let zone = row.zone_slug.clone();
                    let device_id = row.device_id.clone();
                    let device_type = row.device_type.clone();
                    let path = row.path.clone();
                    let scale = row.scale;
                    let offset = row.offset;
                    let id_for_err = device_id.clone();
                    let is_soil = !zone.trim().is_empty();

                    let set_field = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.field = v; } });
                        flush();
                    };
                    let set_zone = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.zone_slug = v; } });
                        flush();
                    };
                    let set_device = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.device_id = v; } });
                        flush();
                    };
                    let set_type = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.device_type = v; } });
                        flush();
                    };
                    let set_path = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.path = v; } });
                        flush();
                    };
                    let set_scale = move |ev: leptos::ev::Event| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            rows.update(|r| { if let Some(row) = r.get_mut(i) { row.scale = v; } });
                            flush();
                        }
                    };
                    let set_offset = move |ev: leptos::ev::Event| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            rows.update(|r| { if let Some(row) = r.get_mut(i) { row.offset = v; } });
                            flush();
                        }
                    };
                    let remove = move |_| {
                        rows.update(|r| { if i < r.len() { r.remove(i); } });
                        flush();
                    };

                    view! {
                        <div class="soil-sub-card">
                            <FormField
                                label="Device id".to_string()
                                helptext="The device's id from the vendor app / API.".to_string()
                                error=Signal::derive(move || (id_for_err.trim().is_empty()).then(|| "Device id is required".to_string()))
                            >
                                <input type="text" class="ui-input" placeholder="device id"
                                    prop:value=device_id on:input=set_device/>
                            </FormField>
                            {(!is_tuya).then(|| view! {
                                <FormField
                                    label="Device type".to_string()
                                    helptext="YoLink device type, used to compose {Type}.getState (e.g. THSensor, WaterMeterController).".to_string()
                                    error=Signal::derive(|| None::<String>)
                                >
                                    <input type="text" class="ui-input" placeholder="e.g. THSensor"
                                        prop:value=device_type on:input=set_type/>
                                </FormField>
                            })}
                            <FormField
                                label=path_label.to_string()
                                helptext=path_help.to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <input type="text" class="ui-input" placeholder=path_ph
                                    prop:value=path on:input=set_path/>
                            </FormField>
                            <FormField
                                label="Reading".to_string()
                                helptext="Which LocalSky reading this value feeds. Ignored when a zone is bound below.".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <select class="ui-input" prop:disabled=is_soil on:change=set_field>
                                    {WEATHER_FIELD_OPTIONS.iter().map(|(val, label)| {
                                        let val = val.to_string();
                                        let sel = field == val;
                                        view! { <option value=val.clone() selected=sel>{label.to_string()}</option> }
                                    }).collect_view()}
                                </select>
                            </FormField>
                            <FormField
                                label="Bind to zone (optional)".to_string()
                                helptext="Set this only for a soil probe; the value becomes that zone's soil channel. Finish in the zone editor.".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <select class="ui-input" on:change=set_zone>
                                    <option value="" selected=zone.is_empty()>
                                        "(not soil, a weather reading)"
                                    </option>
                                    {zone_slugs.get().into_iter().map(|(slug, name)| {
                                        let sel = zone == slug;
                                        view! { <option value=slug.clone() selected=sel>{name}</option> }
                                    }).collect_view()}
                                </select>
                            </FormField>
                            <div class="soil-sub-card__row">
                                <FormField label="Scale".to_string()
                                    helptext="value × scale + offset. 1 = as-reported.".to_string()
                                    error=Signal::derive(|| None::<String>)>
                                    <input type="number" class="ui-input" step="any"
                                        prop:value=scale.to_string() on:input=set_scale/>
                                </FormField>
                                <FormField label="Offset".to_string()
                                    helptext="Added after scaling. 0 = none.".to_string()
                                    error=Signal::derive(|| None::<String>)>
                                    <input type="number" class="ui-input" step="any"
                                        prop:value=offset.to_string() on:input=set_offset/>
                                </FormField>
                            </div>
                            <Button variant="danger" on_click=Callback::new(remove)>
                                "Remove mapping"
                            </Button>
                        </div>
                    }
                }).collect_view()
            }}
            <Button variant="ghost" on_click=Callback::new(add_row)>
                "+ Add a device mapping"
            </Button>
        </div>
    }
}

// ===================================================================
// Group C: HA passthrough field_map (map of WeatherField -> HA entity_id)
// ===================================================================
//
// HaPassthroughConfig.field_map is a BTreeMap<WeatherField name, entity_id>:
// each reading is fed by exactly one HA entity. (soil_zone_map -- per-zone soil
// entities -- is bound in the zone editor, not here.) The entity_id is a free
// text input rather than a dropdown so the editor works on every surface
// regardless of whether HA discovery has loaded.

/// One field_map entry as editable strings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HaFieldRow {
    pub field: String,
    pub entity_id: String,
}

/// Parse HaPassthroughConfig.field_map (an object) into stable field-sorted rows.
pub fn parse_ha_field_map(config: &serde_json::Value) -> Vec<HaFieldRow> {
    let mut rows: Vec<HaFieldRow> = config
        .get("field_map")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .map(|(field, ent)| HaFieldRow {
                    field: field.clone(),
                    entity_id: ent.as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(|a, b| a.field.cmp(&b.field));
    rows
}

/// Write rows back into `field_map`, preserving base_url, bearer_token, and
/// soil_zone_map. Rows with an empty field or entity are dropped; a duplicate
/// field keeps the last (BTreeMap semantics).
pub fn apply_ha_field_map(config: &mut serde_json::Value, rows: &[HaFieldRow]) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let mut map = serde_json::Map::new();
    for r in rows {
        let field = r.field.trim();
        let ent = r.entity_id.trim();
        if field.is_empty() || ent.is_empty() {
            continue;
        }
        map.insert(
            field.to_string(),
            serde_json::Value::String(ent.to_string()),
        );
    }
    if let Some(obj) = config.as_object_mut() {
        obj.insert("field_map".into(), serde_json::Value::Object(map));
    }
}

/// HA FIELD-MAP FORM. Maps LocalSky readings to Home Assistant entities, so an
/// HA temperature/wind/etc. entity feeds the engine without raw JSON.
#[component]
pub fn HaFieldMapEditor(config_text: RwSignal<String>) -> impl IntoView {
    let rows = RwSignal::new(parse_ha_field_map(
        &serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::Value::Null),
    ));
    let self_edit = RwSignal::new(false);

    let flush = move || {
        let mut cfg: serde_json::Value =
            serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::json!({}));
        apply_ha_field_map(&mut cfg, &rows.get_untracked());
        self_edit.set(true);
        config_text.set(serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".into()));
    };

    Effect::new(move |_| {
        let text = config_text.get();
        if self_edit.get_untracked() {
            self_edit.set(false);
            return;
        }
        let parsed =
            parse_ha_field_map(&serde_json::from_str(&text).unwrap_or(serde_json::Value::Null));
        if parsed != rows.get_untracked() {
            rows.set(parsed);
        }
    });

    let add_row = move |_| {
        rows.update(|r| {
            r.push(HaFieldRow {
                field: DEFAULT_FIELD.to_string(),
                entity_id: String::new(),
            })
        });
        flush();
    };

    view! {
        <div class="soil-subs">
            <p class="sensors-section__hint">
                "Map a LocalSky reading to the Home Assistant entity that provides it (e.g. "
                <code>"sensor.outdoor_temperature"</code>"). Entity ids are listed under "
                "\"Discovered from Home Assistant\" on the "<a href="/sensors">"Sensors hub"</a>". "
                "Soil probes are bound per-zone in the zone editor, not here."
            </p>
            {move || {
                let rs = rows.get();
                rs.into_iter().enumerate().map(|(i, row)| {
                    let field = row.field.clone();
                    let entity_id = row.entity_id.clone();
                    let ent_for_err = entity_id.clone();

                    let set_field = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.field = v; } });
                        flush();
                    };
                    let set_entity = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.entity_id = v; } });
                        flush();
                    };
                    let remove = move |_| {
                        rows.update(|r| { if i < r.len() { r.remove(i); } });
                        flush();
                    };

                    view! {
                        <div class="soil-sub-card">
                            <FormField
                                label="Reading".to_string()
                                helptext="Which LocalSky reading this HA entity feeds.".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <select class="ui-input" on:change=set_field>
                                    {WEATHER_FIELD_OPTIONS.iter().map(|(val, label)| {
                                        let val = val.to_string();
                                        let sel = field == val;
                                        view! { <option value=val.clone() selected=sel>{label.to_string()}</option> }
                                    }).collect_view()}
                                </select>
                            </FormField>
                            <FormField
                                label="HA entity id".to_string()
                                helptext="The Home Assistant entity that provides this reading.".to_string()
                                error=Signal::derive(move || (ent_for_err.trim().is_empty()).then(|| "Entity id is required".to_string()))
                            >
                                <input type="text" class="ui-input" placeholder="e.g. sensor.outdoor_temperature"
                                    prop:value=entity_id on:input=set_entity/>
                            </FormField>
                            <Button variant="danger" on_click=Callback::new(remove)>
                                "Remove mapping"
                            </Button>
                        </div>
                    }
                }).collect_view()
            }}
            <Button variant="ghost" on_click=Callback::new(add_row)>
                "+ Add a field mapping"
            </Button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ha_field_map_round_trip_preserves_other_keys() {
        let cfg: serde_json::Value = serde_json::from_str(
            r#"{"base_url":"http://ha:8123","bearer_token":"t",
                "field_map":{"air_temp_f":"sensor.temp","wind_mph":"sensor.wind"},
                "soil_zone_map":{"sensor.soil1":"back_yard"}}"#,
        )
        .unwrap();
        let rows = parse_ha_field_map(&cfg);
        // Field-sorted: air_temp_f before wind_mph.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].field, "air_temp_f");
        assert_eq!(rows[0].entity_id, "sensor.temp");
        let mut out = cfg.clone();
        apply_ha_field_map(&mut out, &rows);
        assert_eq!(out["base_url"], "http://ha:8123");
        assert_eq!(out["bearer_token"], "t");
        // soil_zone_map untouched.
        assert_eq!(out["soil_zone_map"]["sensor.soil1"], "back_yard");
        assert_eq!(out["field_map"]["air_temp_f"], "sensor.temp");
        assert_eq!(out["field_map"]["wind_mph"], "sensor.wind");
    }

    #[test]
    fn ha_rows_missing_field_or_entity_are_dropped() {
        let mut cfg = serde_json::json!({});
        let rows = vec![
            HaFieldRow {
                field: "air_temp_f".into(),
                entity_id: "".into(),
            },
            HaFieldRow {
                field: "".into(),
                entity_id: "sensor.x".into(),
            },
            HaFieldRow {
                field: "wind_mph".into(),
                entity_id: "sensor.wind".into(),
            },
        ];
        apply_ha_field_map(&mut cfg, &rows);
        let map = cfg["field_map"].as_object().unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("wind_mph"));
    }

    #[test]
    fn field_map_round_trip_preserves_connection_keys() {
        let cfg: serde_json::Value = serde_json::from_str(
            r#"{
                "url": "https://api.example.test/obs",
                "method": "GET",
                "poll_interval_s": 300,
                "fields": [
                    {"field": "air_temp_f", "json_path": "current.temp_f",
                     "zone_slug": null, "scale": 1.0, "offset": 0.0},
                    {"field": "", "json_path": "soil.0.pct",
                     "zone_slug": "back_yard", "scale": 1.0, "offset": 0.0}
                ]
            }"#,
        )
        .unwrap();

        let rows = parse_field_map(&cfg);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].field, "air_temp_f");
        assert_eq!(rows[0].json_path, "current.temp_f");
        assert_eq!(rows[1].zone_slug, "back_yard");

        let mut out = cfg.clone();
        apply_field_map(&mut out, &rows);
        // Connection keys survive untouched.
        assert_eq!(out["url"], "https://api.example.test/obs");
        assert_eq!(out["method"], "GET");
        assert_eq!(out["poll_interval_s"], 300);
        // Mappings survive with identical fields.
        assert_eq!(out["fields"][0]["field"], "air_temp_f");
        assert_eq!(out["fields"][0]["json_path"], "current.temp_f");
        assert_eq!(out["fields"][1]["zone_slug"], "back_yard");
    }

    #[test]
    fn blank_optional_fields_serialize_to_null() {
        let mut cfg = serde_json::json!({ "url": "u" });
        let mut row = FieldMapRow::new_default();
        row.json_path = "temp".into();
        // zone_slug left blank.
        apply_field_map(&mut cfg, &[row]);
        let f = &cfg["fields"][0];
        assert_eq!(f["field"], DEFAULT_FIELD);
        assert_eq!(f["json_path"], "temp");
        assert!(f["zone_slug"].is_null());
    }

    #[test]
    fn rows_with_no_field_and_no_zone_are_dropped() {
        let mut cfg = serde_json::json!({});
        let blank = FieldMapRow {
            field: String::new(),
            ..FieldMapRow::new_default()
        };
        let real = FieldMapRow::new_default();
        apply_field_map(&mut cfg, &[blank, real]);
        let arr = cfg["fields"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "the field-less, zone-less row is dropped");
        assert_eq!(arr[0]["field"], DEFAULT_FIELD);
    }

    #[test]
    fn new_default_row_is_identity_transform() {
        let r = FieldMapRow::new_default();
        assert_eq!(r.field, "air_temp_f");
        assert_eq!(r.scale, 1.0);
        assert_eq!(r.offset, 0.0);
    }

    #[test]
    fn yolink_device_map_round_trip() {
        let cfg: serde_json::Value = serde_json::from_str(
            r#"{"client_id":"u","client_secret":"s","device_field_map":[
              {"field":"air_temp_f","zone_slug":null,"device_id":"dev1",
               "device_type":"THSensor","state_path":"temperature","scale":1.0,"offset":0.0}
            ]}"#,
        )
        .unwrap();
        let rows = parse_device_field_map(&cfg, "yolink");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].device_type, "THSensor");
        assert_eq!(rows[0].path, "temperature");
        let mut out = cfg.clone();
        apply_device_field_map(&mut out, &rows, "yolink");
        assert_eq!(out["client_id"], "u");
        assert_eq!(out["device_field_map"][0]["state_path"], "temperature");
        assert_eq!(out["device_field_map"][0]["device_type"], "THSensor");
    }

    #[test]
    fn tuya_device_map_uses_status_code() {
        let cfg: serde_json::Value = serde_json::from_str(
            r#"{"client_id":"u","client_secret":"s","device_field_map":[
              {"field":"air_temp_f","device_id":"dev1","status_code":"temp_current","scale":0.1,"offset":0.0}
            ]}"#,
        )
        .unwrap();
        let rows = parse_device_field_map(&cfg, "tuya_cloud");
        assert_eq!(rows[0].path, "temp_current");
        assert_eq!(rows[0].device_type, "");
        let mut out = cfg.clone();
        apply_device_field_map(&mut out, &rows, "tuya_cloud");
        assert_eq!(out["device_field_map"][0]["status_code"], "temp_current");
        assert!(out["device_field_map"][0].get("state_path").is_none());
    }

    #[test]
    fn device_rows_missing_id_or_path_are_dropped() {
        let mut cfg = serde_json::json!({});
        let no_id = DeviceFieldRow {
            device_id: "".into(),
            path: "x".into(),
            ..DeviceFieldRow::new_default()
        };
        let no_path = DeviceFieldRow {
            device_id: "d".into(),
            path: "".into(),
            ..DeviceFieldRow::new_default()
        };
        let ok = DeviceFieldRow {
            device_id: "d".into(),
            path: "p".into(),
            ..DeviceFieldRow::new_default()
        };
        apply_device_field_map(&mut cfg, &[no_id, no_path, ok], "tuya_cloud");
        assert_eq!(cfg["device_field_map"].as_array().unwrap().len(), 1);
    }

    // References crate::sources (cfg ssr). Gated so bare `cargo test` compiles.
    #[cfg(feature = "ssr")]
    #[test]
    fn every_field_option_is_a_real_weather_field() {
        use crate::sources::mqtt_subscribe::parse_weather_field;
        for (val, _label) in WEATHER_FIELD_OPTIONS {
            assert!(
                parse_weather_field(val).is_some(),
                "field option {val} is not a parseable WeatherField"
            );
        }
        assert_eq!(WEATHER_FIELD_OPTIONS[0].0, DEFAULT_FIELD);
    }
}
