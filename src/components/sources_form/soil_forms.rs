// Phase 2 sensor-UX: point-and-click forms for the three soil-sensor paths
// that previously REQUIRED hand-editing localsky.toml. A rookie should never
// open the JSON/TOML to add a soil sensor.
//
//   1. MqttSoilSubscriptions     - add/edit MqttSourceConfig.subscriptions
//                                  (topic, json_path, field, zone, scale/offset)
//   2. EcowittSoilCalibration    - per-channel SoilAdCalibration (ad_dry/ad_wet)
//
// Both components are embedded in sources_form::SourceEditorPanel and operate
// on the SAME `config_text` signal the raw editor uses, so the existing config
// PUT path persists them unchanged. The zone editor binds an HA / native soil
// entity through its own consolidated <select> over /sensors/soil; the pure HA
// soil-filter helpers (filter_ha_soil, SoilEntity, has_ha_passthrough) live
// here, unit-tested, for reuse by any soil-entity discovery surface.
//
// All structured<->JSON round-tripping is pure (testable) and keeps ids and
// field names byte-identical to what the engine resolves (ha:<entity>,
// MqttSubscription.field defaults to the soil "rh_pct" the existing template
// uses, SoilAdCalibration keyed by the channel string).

use leptos::prelude::*;

use crate::components::ui::{Button, FormField};

// ===================================================================
// Pure logic: MQTT soil subscription round-trip
// ===================================================================

/// The `field` value a soil-moisture-percent MQTT subscription publishes when
/// it is NOT bound to a zone. There is no dedicated `soil_moisture`
/// WeatherField variant; an unbound soil reading routes through `rh_pct` (the
/// existing default the MQTT template uses), so the form defaults to it to stay
/// byte-identical. NOTE: when a subscription IS bound to a zone, the adapter
/// ignores this field and instead records a per-zone soil channel
/// (`soilmoisture_<zone_slug>`, see bus_recorder::zone_soil_key), so a
/// zone-bound soil probe never clobbers the global humidity field.
pub const DEFAULT_SOIL_FIELD: &str = "rh_pct";

/// (WeatherField value, label) pairs offered in the subscription "Reading"
/// dropdown. Soil moisture is first + default. Every value parses via
/// mqtt_subscribe::parse_weather_field, so the adapter ingests the topic.
pub const SUBSCRIPTION_FIELD_OPTIONS: &[(&str, &str)] = &[
    (DEFAULT_SOIL_FIELD, "Soil moisture (%)"),
    ("air_temp_f", "Air temperature (°F)"),
    ("rain_today_in", "Rain today (in)"),
    ("rain_intensity_in_hr", "Rain rate (in/hr)"),
    ("wind_mph", "Wind (mph)"),
    ("flow_gpm", "Flow (gpm)"),
];

/// One MqttSubscription row, mirrored as plain Strings for two-way binding in
/// text inputs. Empty `json_path`/`zone_slug` serialize back to JSON null
/// (i.e. the `#[serde(default)]` Option<String> absent shape).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MqttSubRow {
    pub topic: String,
    pub field: String,
    pub json_path: String,
    pub zone_slug: String,
    pub scale: f64,
    pub offset: f64,
}

impl MqttSubRow {
    /// A blank soil-moisture subscription seed: soil field preselected,
    /// identity scale/offset, everything else for the user to fill.
    pub fn new_soil() -> Self {
        Self {
            topic: String::new(),
            field: DEFAULT_SOIL_FIELD.to_string(),
            json_path: String::new(),
            zone_slug: String::new(),
            scale: 1.0,
            offset: 0.0,
        }
    }
}

/// Parse an MqttSourceConfig JSON object's `subscriptions` array into rows.
/// Missing/empty array -> empty Vec. Unknown shapes are skipped, not errors,
/// so a partially hand-edited config never wedges the form.
pub fn parse_mqtt_subscriptions(config: &serde_json::Value) -> Vec<MqttSubRow> {
    config
        .get("subscriptions")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let topic = s.get("topic")?.as_str()?.to_string();
                    Some(MqttSubRow {
                        topic,
                        field: s
                            .get("field")
                            .and_then(|v| v.as_str())
                            .unwrap_or(DEFAULT_SOIL_FIELD)
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
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Serialize one row to the exact MqttSubscription JSON shape serde expects.
/// Blank `json_path`/`zone_slug` become JSON null (Option<String> = None);
/// `scale`/`offset` always present (they have serde defaults but emitting them
/// is harmless and keeps the round-trip stable).
fn mqtt_sub_to_json(row: &MqttSubRow) -> serde_json::Value {
    let opt = |s: &str| {
        let t = s.trim();
        if t.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(t.to_string())
        }
    };
    serde_json::json!({
        "topic": row.topic.trim(),
        "field": row.field.trim(),
        "json_path": opt(&row.json_path),
        "zone_slug": opt(&row.zone_slug),
        "scale": row.scale,
        "offset": row.offset,
    })
}

/// Write rows back into an MqttSourceConfig JSON object's `subscriptions` key,
/// preserving every other key (broker_host, port, auth, client_id). Rows with
/// a blank topic are dropped (the engine requires a topic, and validation
/// would reject them anyway).
pub fn apply_mqtt_subscriptions(config: &mut serde_json::Value, rows: &[MqttSubRow]) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let subs: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| !r.topic.trim().is_empty())
        .map(mqtt_sub_to_json)
        .collect();
    if let Some(obj) = config.as_object_mut() {
        obj.insert("subscriptions".into(), serde_json::Value::Array(subs));
    }
}

// ===================================================================
// Pure logic: Ecowitt per-channel soil AD calibration round-trip
// ===================================================================

/// One SoilAdCalibration row keyed by channel ("1".."N").
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CalRow {
    pub channel: String,
    pub ad_dry: f64,
    pub ad_wet: f64,
}

/// Inline validation for a calibration row's AD endpoints. The poller maps
/// raw AD to 0-100% by interpolating between dry and wet, so equal endpoints
/// (including the default 0/0) are a divide-by-zero / nonsense calibration.
/// Returns the user-facing message, or None when the pair is usable.
pub fn calibration_error(ad_dry: f64, ad_wet: f64) -> Option<String> {
    if (ad_dry - ad_wet).abs() < f64::EPSILON {
        return Some("Dry and Wet AD must differ".to_string());
    }
    None
}

/// Parse EcowittGwPollConfig.soil_calibration (a map keyed by channel) into a
/// stable channel-sorted Vec. Non-numeric channels sort last but are kept so
/// nothing the user typed silently disappears.
pub fn parse_soil_calibration(config: &serde_json::Value) -> Vec<CalRow> {
    let mut rows: Vec<CalRow> = config
        .get("soil_calibration")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .map(|(ch, v)| CalRow {
                    channel: ch.clone(),
                    ad_dry: v.get("ad_dry").and_then(|x| x.as_f64()).unwrap_or(0.0),
                    ad_wet: v.get("ad_wet").and_then(|x| x.as_f64()).unwrap_or(0.0),
                })
                .collect()
        })
        .unwrap_or_default();
    rows.sort_by(
        |a, b| match (a.channel.parse::<u32>(), b.channel.parse::<u32>()) {
            (Ok(x), Ok(y)) => x.cmp(&y),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => a.channel.cmp(&b.channel),
        },
    );
    rows
}

/// Write calibration rows back into an EcowittGwPollConfig JSON object's
/// `soil_calibration` map, preserving host + poll_interval_s. Rows with a
/// blank channel are dropped. Rows whose AD endpoints fail `calibration_error`
/// (equal/zero endpoints = divide-by-zero) are ALSO dropped, so an invalid
/// in-progress calibration can never reach the persisted config; the inline
/// error tells the user why it isn't saved yet. The key is the channel string
/// verbatim, so a channel "1" maps to SoilAdCalibration for soil channel 1
/// exactly as the poller reads it.
pub fn apply_soil_calibration(config: &mut serde_json::Value, rows: &[CalRow]) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let mut map = serde_json::Map::new();
    for r in rows {
        let ch = r.channel.trim();
        if ch.is_empty() {
            continue;
        }
        // Exclude bad calibrations (equal/zero endpoints) like blank rows: a
        // divide-by-zero pair must never be serialized into config.
        if calibration_error(r.ad_dry, r.ad_wet).is_some() {
            continue;
        }
        map.insert(
            ch.to_string(),
            serde_json::json!({ "ad_dry": r.ad_dry, "ad_wet": r.ad_wet }),
        );
    }
    if let Some(obj) = config.as_object_mut() {
        obj.insert("soil_calibration".into(), serde_json::Value::Object(map));
    }
}

// ===================================================================
// Pure logic: HA soil-entity filtering
// ===================================================================

/// A pickable soil-moisture entity discovered from Home Assistant.
#[derive(Clone, Debug, PartialEq)]
pub struct SoilEntity {
    /// Canonical engine address, byte-identical to what a zone binds:
    /// `ha:<entity_id>`.
    pub id: String,
    pub label: String,
    pub current_pct: Option<f64>,
}

/// Filter the /api/v1/sensors/discovered grouped map down to HA soil-moisture
/// entities. The discovery endpoint already classifies by device_class
/// (`moisture` -> "soil") with a soil+moist name fallback, so the `soil` group
/// is the authoritative set; we additionally require an `ha:` id so native
/// `source:` channels (which the picker is NOT for) are excluded.
pub fn filter_ha_soil(discovered: &serde_json::Value) -> Vec<SoilEntity> {
    discovered
        .get("soil")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let id = s.get("id")?.as_str()?.to_string();
                    if !id.starts_with("ha:") {
                        return None;
                    }
                    Some(SoilEntity {
                        label: s
                            .get("label")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&id)
                            .to_string(),
                        current_pct: s.get("current_pct").and_then(|v| v.as_f64()),
                        id,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether the current config has at least one ha_passthrough source. The HA
/// picker needs an HA bridge to exist; when absent we show a hint instead of
/// an empty dropdown. `sources` is the config's sources array.
pub fn has_ha_passthrough(config: &serde_json::Value) -> bool {
    config
        .get("sources")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .any(|s| s.get("kind").and_then(|v| v.as_str()) == Some("ha_passthrough"))
        })
        .unwrap_or(false)
}

// ===================================================================
// Components
// ===================================================================

/// MQTT SOIL SUBSCRIPTION FORM. Reads `subscriptions` out of the bound MQTT
/// `config_text`, renders one editable card per subscription, and writes every
/// change straight back into `config_text` so the surrounding
/// SourceEditorPanel save (config PUT) persists it. No TOML required to wire an
/// ESPHome / Zigbee2MQTT soil probe.
#[component]
pub fn MqttSoilSubscriptions(
    /// The MQTT source's raw config JSON text, shared with the raw editor.
    config_text: RwSignal<String>,
    /// Zone slugs offered in the per-subscription zone binding dropdown.
    zone_slugs: Memo<Vec<(String, String)>>,
) -> impl IntoView {
    // Local row state, seeded from config_text and flushed back on every edit.
    let rows = RwSignal::new(parse_mqtt_subscriptions(
        &serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::Value::Null),
    ));

    // Guard flag: set true by flush() right before it writes config_text, then
    // checked-and-cleared by the re-seed Effect. It lets the Effect tell apart
    // a config_text change WE caused (skip re-seed) from an EXTERNAL one (the
    // raw textarea, re-seed). A value-based guard is NOT enough: flush() drops
    // blank-topic rows (apply_mqtt_subscriptions skips them), so right after
    // "+ Add" the textarea legitimately differs from `rows` (which holds the
    // in-progress blank row) and a value guard would re-seed and delete it.
    let self_edit = RwSignal::new(false);

    // Flush rows -> config_text, preserving the broker/auth keys already there.
    let flush = move || {
        let mut cfg: serde_json::Value =
            serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::json!({}));
        apply_mqtt_subscriptions(&mut cfg, &rows.get_untracked());
        // Mark this write as ours BEFORE setting, so the Effect (which runs
        // after this reactive cycle) sees the flag and skips re-seeding.
        self_edit.set(true);
        config_text.set(serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".into()));
    };

    // Two-way sync: re-seed the structured rows when `subscriptions` is changed
    // from OUTSIDE this form (the raw "advanced" JSON textarea). We always read
    // config_text first to stay subscribed, then bail on our own flushes via
    // the guard flag. This keeps blank in-progress rows alive (flush drops
    // them from config_text, but we never re-seed off our own write) while an
    // external textarea edit still re-seeds the cards.
    Effect::new(move |_| {
        // Track config_text unconditionally so we re-run on the NEXT change.
        let text = config_text.get();
        if self_edit.get_untracked() {
            self_edit.set(false);
            return;
        }
        let parsed = parse_mqtt_subscriptions(
            &serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
        );
        if parsed != rows.get_untracked() {
            rows.set(parsed);
        }
    });

    let add_row = move |_| {
        rows.update(|r| r.push(MqttSubRow::new_soil()));
        flush();
    };

    view! {
        <div class="soil-subs">
            <p class="sensors-section__hint">
                "Each subscription maps an MQTT topic to a reading. For a soil probe, point it at the "
                "topic your probe publishes (e.g. "<code>"zigbee2mqtt/garden_soil"</code>" or "
                <code>"esp/soil/back"</code>"), set the JSON field if the payload is an object, and bind it "
                "to the zone it measures. Binding to a zone records this topic as that zone's own soil "
                "channel; then open that zone in the zone editor and pick this source's channel as its "
                "soil-moisture sensor. Field stays \"soil moisture\" unless you know otherwise."
            </p>
            {move || {
                let rs = rows.get();
                rs.into_iter().enumerate().map(|(i, row)| {
                    let topic = row.topic.clone();
                    let field = row.field.clone();
                    let json_path = row.json_path.clone();
                    let zone = row.zone_slug.clone();
                    let scale = row.scale;
                    let offset = row.offset;
                    let topic_for_err = topic.clone();

                    let set_topic = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.topic = v; } });
                        flush();
                    };
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
                                label="MQTT topic".to_string()
                                helptext="The topic your probe publishes to. Wildcards (+ / #) are allowed.".to_string()
                                error=Signal::derive(move || (topic_for_err.trim().is_empty()).then(|| "Topic is required".to_string()))
                            >
                                <input
                                    type="text"
                                    class="ui-input"
                                    placeholder="e.g. zigbee2mqtt/garden_soil"
                                    prop:value=topic
                                    on:input=set_topic
                                />
                            </FormField>
                            <FormField
                                label="Reading".to_string()
                                helptext="What this topic measures. Defaults to soil moisture; change only if this probe also reports temperature/humidity you want LocalSky to ingest.".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <select class="ui-input" on:change=set_field>
                                    {SUBSCRIPTION_FIELD_OPTIONS.iter().map(|(val, label)| {
                                        let val = val.to_string();
                                        let sel = field == val;
                                        view! { <option value=val.clone() selected=sel>{label.to_string()}</option> }
                                    }).collect_view()}
                                </select>
                            </FormField>
                            <FormField
                                label="JSON field (optional)".to_string()
                                helptext="If the payload is JSON, the field to read (e.g. soil_moisture, 0.value). Leave blank if the payload is a bare number.".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <input
                                    type="text"
                                    class="ui-input"
                                    placeholder="e.g. soil_moisture"
                                    prop:value=json_path
                                    on:input=set_path
                                />
                            </FormField>
                            <FormField
                                label="Bind to zone (optional)".to_string()
                                helptext="The zone this probe measures. When set, this topic is recorded as that zone's own soil channel (it won't be merged into global humidity). Finish wiring it in the zone editor: open the zone and pick this source's soil channel as its soil-moisture sensor.".to_string()
                                error=Signal::derive(|| None::<String>)
                            >
                                <select class="ui-input" on:change=set_zone>
                                    <option value="" selected=zone.is_empty()>
                                        "(no zone, general moisture)"
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
                                    helptext="value × scale + offset. 1 = as-published.".to_string()
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
                                "Remove subscription"
                            </Button>
                        </div>
                    }
                }).collect_view()
            }}
            <button type="button" class="setup-footer__btn setup-footer__btn--ghost" on:click=add_row>
                "+ Add soil subscription"
            </button>
        </div>
    }
}

/// ECOWITT PER-CHANNEL CALIBRATION FORM. Reads `soil_calibration` out of the
/// bound EcowittGwPollConfig `config_text`, edits each channel's raw-AD dry/wet
/// endpoints, and flushes back so the SourceEditorPanel save persists them. The
/// poller then computes moisture% from the raw AD instead of trusting the
/// gateway's own (often unset) %.
#[component]
pub fn EcowittSoilCalibration(config_text: RwSignal<String>) -> impl IntoView {
    let rows = RwSignal::new(parse_soil_calibration(
        &serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::Value::Null),
    ));

    // Guard flag (see MqttSoilSubscriptions for the full rationale): flush()
    // drops blank-channel AND invalid (equal/zero AD) rows from config_text, so
    // a value-based guard would re-seed and delete an in-progress "+ Add"
    // channel (which starts blank/invalid). This flag lets the Effect skip
    // re-seeding on config_text changes WE caused.
    let self_edit = RwSignal::new(false);

    let flush = move || {
        let mut cfg: serde_json::Value =
            serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::json!({}));
        apply_soil_calibration(&mut cfg, &rows.get_untracked());
        self_edit.set(true);
        config_text.set(serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "{}".into()));
    };

    // Two-way sync: re-seed the structured rows when `soil_calibration` is
    // edited from OUTSIDE this form (the raw JSON textarea). Read config_text
    // first to stay subscribed, then bail on our own flushes via the guard so
    // in-progress (blank/invalid) rows survive; external textarea edits still
    // re-seed the cards.
    Effect::new(move |_| {
        let text = config_text.get();
        if self_edit.get_untracked() {
            self_edit.set(false);
            return;
        }
        let parsed =
            parse_soil_calibration(&serde_json::from_str(&text).unwrap_or(serde_json::Value::Null));
        if parsed != rows.get_untracked() {
            rows.set(parsed);
        }
    });

    let add_row = move |_| {
        // Default the next channel number to len+1 for convenience.
        let next = (rows.get_untracked().len() + 1).to_string();
        rows.update(|r| {
            r.push(CalRow {
                channel: next,
                ad_dry: 0.0,
                ad_wet: 0.0,
            })
        });
        flush();
    };

    view! {
        <div class="soil-cal">
            <p class="sensors-section__hint">
                "How to measure: take the probe out of the soil and let it dry in air, read its raw AD "
                "value (shown as \"Soil raw chN\" on this source's live readings) and enter it as "
                <strong>"Dry AD"</strong>". Then saturate the probe in water and read it again for "
                <strong>"Wet AD"</strong>". LocalSky maps everything in between to 0-100% moisture."
            </p>
            {move || {
                let rs = rows.get();
                rs.into_iter().enumerate().map(|(i, row)| {
                    let channel = row.channel.clone();
                    let ad_dry = row.ad_dry;
                    let ad_wet = row.ad_wet;
                    let channel_for_err = channel.clone();

                    let set_channel = move |ev: leptos::ev::Event| {
                        let v = event_target_value(&ev);
                        rows.update(|r| { if let Some(row) = r.get_mut(i) { row.channel = v; } });
                        flush();
                    };
                    let set_dry = move |ev: leptos::ev::Event| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            rows.update(|r| { if let Some(row) = r.get_mut(i) { row.ad_dry = v; } });
                            flush();
                        }
                    };
                    let set_wet = move |ev: leptos::ev::Event| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            rows.update(|r| { if let Some(row) = r.get_mut(i) { row.ad_wet = v; } });
                            flush();
                        }
                    };
                    let remove = move |_| {
                        rows.update(|r| { if i < r.len() { r.remove(i); } });
                        flush();
                    };

                    view! {
                        <div class="soil-cal-card">
                            <FormField
                                label="Soil channel".to_string()
                                helptext="Gateway soil channel number (1, 2, 3...).".to_string()
                                error=Signal::derive(move || (channel_for_err.trim().is_empty()).then(|| "Channel is required".to_string()))
                            >
                                <input
                                    type="text"
                                    class="ui-input"
                                    placeholder="e.g. 1"
                                    prop:value=channel
                                    on:input=set_channel
                                />
                            </FormField>
                            <div class="soil-cal-card__row">
                                <FormField
                                    label="Dry AD".to_string()
                                    helptext="Raw AD with the probe dry (in air).".to_string()
                                    error=Signal::derive(move || calibration_error(ad_dry, ad_wet))
                                >
                                    <input type="number" class="ui-input" step="any"
                                        prop:value=ad_dry.to_string() on:input=set_dry/>
                                </FormField>
                                <FormField
                                    label="Wet AD".to_string()
                                    helptext="Raw AD with the probe saturated.".to_string()
                                    error=Signal::derive(move || calibration_error(ad_dry, ad_wet))
                                >
                                    <input type="number" class="ui-input" step="any"
                                        prop:value=ad_wet.to_string() on:input=set_wet/>
                                </FormField>
                            </div>
                            <Button variant="danger" on_click=Callback::new(remove)>
                                "Remove channel"
                            </Button>
                        </div>
                    }
                }).collect_view()
            }}
            <button type="button" class="setup-footer__btn setup-footer__btn--ghost" on:click=add_row>
                "+ Add a channel"
            </button>
        </div>
    }
}

// NOTE: the HaSoilEntityPicker Leptos component was removed once the zone
// editor consolidated onto a single soil-sensor <select> that lists BOTH HA
// entities (ha:*) and LocalSky native channels (source:*) from
// /sensors/soil, so a dedicated HA-only picker is no longer mounted anywhere.
// The pure helpers it used (filter_ha_soil, SoilEntity, has_ha_passthrough)
// are kept above: they remain unit-tested and are reusable by any future
// soil-entity UI / discovery surface.

// ===================================================================
// Tests (pure logic only)
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mqtt_subscriptions_round_trip_preserves_broker_keys() {
        let cfg: serde_json::Value = serde_json::from_str(
            r#"{
                "broker_host": "broker.local",
                "broker_port": 1883,
                "username": "u",
                "password": "p",
                "client_id": "cid",
                "subscriptions": [
                    {"topic": "z2m/soil", "field": "rh_pct", "json_path": "moisture",
                     "zone_slug": "back_yard", "scale": 1.0, "offset": 0.0}
                ]
            }"#,
        )
        .unwrap();

        let rows = parse_mqtt_subscriptions(&cfg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].topic, "z2m/soil");
        assert_eq!(rows[0].field, "rh_pct");
        assert_eq!(rows[0].json_path, "moisture");
        assert_eq!(rows[0].zone_slug, "back_yard");

        let mut out = cfg.clone();
        apply_mqtt_subscriptions(&mut out, &rows);
        // Broker/auth keys survive the round-trip untouched.
        assert_eq!(out["broker_host"], "broker.local");
        assert_eq!(out["broker_port"], 1883);
        assert_eq!(out["username"], "u");
        assert_eq!(out["client_id"], "cid");
        // The subscription survives with identical fields.
        let s = &out["subscriptions"][0];
        assert_eq!(s["topic"], "z2m/soil");
        assert_eq!(s["field"], "rh_pct");
        assert_eq!(s["json_path"], "moisture");
        assert_eq!(s["zone_slug"], "back_yard");
        assert_eq!(s["scale"], 1.0);
        assert_eq!(s["offset"], 0.0);
    }

    #[test]
    fn mqtt_blank_optional_fields_serialize_to_null() {
        let mut cfg = serde_json::json!({ "broker_host": "b" });
        let mut row = MqttSubRow::new_soil();
        row.topic = "esp/soil".into();
        // json_path + zone_slug left blank.
        apply_mqtt_subscriptions(&mut cfg, &[row]);
        let s = &cfg["subscriptions"][0];
        assert_eq!(s["topic"], "esp/soil");
        assert_eq!(s["field"], DEFAULT_SOIL_FIELD);
        assert!(s["json_path"].is_null());
        assert!(s["zone_slug"].is_null());
    }

    #[test]
    fn mqtt_blank_topic_rows_are_dropped() {
        let mut cfg = serde_json::json!({});
        let blank = MqttSubRow::new_soil(); // empty topic
        let mut real = MqttSubRow::new_soil();
        real.topic = "ok/topic".into();
        apply_mqtt_subscriptions(&mut cfg, &[blank, real]);
        let arr = cfg["subscriptions"].as_array().unwrap();
        assert_eq!(arr.len(), 1, "blank-topic row dropped");
        assert_eq!(arr[0]["topic"], "ok/topic");
    }

    #[test]
    fn new_soil_subscription_defaults_to_soil_field_and_identity_transform() {
        let r = MqttSubRow::new_soil();
        assert_eq!(r.field, "rh_pct");
        assert_eq!(r.scale, 1.0);
        assert_eq!(r.offset, 0.0);
    }

    // References crate::sources, which is cfg(feature = "ssr"). Gated so bare
    // `cargo test` (no features) compiles; the pure round-trip tests stay ungated.
    #[cfg(feature = "ssr")]
    #[test]
    fn every_subscription_field_option_is_a_real_weather_field() {
        // The Reading dropdown must only offer values the MQTT adapter can
        // resolve; otherwise the subscription is silently dropped at ingest.
        use crate::sources::mqtt_subscribe::parse_weather_field;
        for (val, _label) in SUBSCRIPTION_FIELD_OPTIONS {
            assert!(
                parse_weather_field(val).is_some(),
                "subscription field option {val} is not a parseable WeatherField"
            );
        }
        // And the soil default is the first option.
        assert_eq!(SUBSCRIPTION_FIELD_OPTIONS[0].0, DEFAULT_SOIL_FIELD);
    }

    #[test]
    fn soil_calibration_round_trip_and_channel_sort() {
        let cfg: serde_json::Value = serde_json::from_str(
            r#"{
                "host": "192.0.2.50",
                "poll_interval_s": 30,
                "soil_calibration": {
                    "2": {"ad_dry": 80.0, "ad_wet": 30.0},
                    "1": {"ad_dry": 100.0, "ad_wet": 40.0}
                }
            }"#,
        )
        .unwrap();

        let rows = parse_soil_calibration(&cfg);
        // Sorted by numeric channel: 1 then 2.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].channel, "1");
        assert_eq!(rows[0].ad_dry, 100.0);
        assert_eq!(rows[0].ad_wet, 40.0);
        assert_eq!(rows[1].channel, "2");

        let mut out = cfg.clone();
        apply_soil_calibration(&mut out, &rows);
        // host + poll survive.
        assert_eq!(out["host"], "192.0.2.50");
        assert_eq!(out["poll_interval_s"], 30);
        // calibration keyed by channel string, byte-identical to schema.
        assert_eq!(out["soil_calibration"]["1"]["ad_dry"], 100.0);
        assert_eq!(out["soil_calibration"]["1"]["ad_wet"], 40.0);
        assert_eq!(out["soil_calibration"]["2"]["ad_dry"], 80.0);
    }

    #[test]
    fn calibration_error_flags_equal_or_zero_endpoints() {
        // Default both-zero row is invalid (divide-by-zero).
        assert_eq!(
            calibration_error(0.0, 0.0).as_deref(),
            Some("Dry and Wet AD must differ")
        );
        // Equal nonzero endpoints are also invalid.
        assert!(calibration_error(50.0, 50.0).is_some());
        // A real dry/wet spread is valid.
        assert!(calibration_error(100.0, 40.0).is_none());
        assert!(calibration_error(40.0, 100.0).is_none());
    }

    #[test]
    fn soil_calibration_invalid_endpoints_excluded_from_output() {
        // A row whose AD endpoints are equal (divide-by-zero, incl. the 0/0
        // default) must never reach the persisted config, just like a blank
        // channel. The inline calibration_error still tells the user why.
        let mut cfg = serde_json::json!({ "host": "h" });
        let rows = vec![
            // Invalid: equal endpoints.
            CalRow {
                channel: "1".into(),
                ad_dry: 50.0,
                ad_wet: 50.0,
            },
            // Invalid: the 0/0 default (e.g. a freshly added blank channel).
            CalRow {
                channel: "2".into(),
                ad_dry: 0.0,
                ad_wet: 0.0,
            },
            // Valid: a real dry/wet spread.
            CalRow {
                channel: "3".into(),
                ad_dry: 100.0,
                ad_wet: 40.0,
            },
        ];
        apply_soil_calibration(&mut cfg, &rows);
        let map = cfg["soil_calibration"].as_object().unwrap();
        // Only the valid channel survives; bad calibrations are dropped.
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("3"));
        assert!(!map.contains_key("1"));
        assert!(!map.contains_key("2"));
        assert_eq!(cfg["soil_calibration"]["3"]["ad_dry"], 100.0);
    }

    #[test]
    fn soil_calibration_blank_channel_dropped() {
        let mut cfg = serde_json::json!({ "host": "h" });
        let rows = vec![
            CalRow {
                channel: "".into(),
                ad_dry: 1.0,
                ad_wet: 2.0,
            },
            CalRow {
                channel: "1".into(),
                ad_dry: 100.0,
                ad_wet: 40.0,
            },
        ];
        apply_soil_calibration(&mut cfg, &rows);
        let map = cfg["soil_calibration"].as_object().unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key("1"));
        assert_eq!(cfg["host"], "h");
    }

    #[test]
    fn filter_ha_soil_keeps_only_ha_soil_entities() {
        let discovered: serde_json::Value = serde_json::from_str(
            r#"{
                "soil": [
                    {"id": "ha:sensor.back_soil_moisture", "label": "Back soil (HA)", "current_pct": 42.0},
                    {"id": "source:ecowitt_gw:soilmoisture1", "label": "ecowitt · Soil ch1", "current_pct": 55.0}
                ],
                "temperature": [
                    {"id": "ha:sensor.outdoor_temp", "label": "Outdoor (HA)", "current_pct": 70.0}
                ]
            }"#,
        )
        .unwrap();

        let soil = filter_ha_soil(&discovered);
        // Only the ha: soil entity, not the native source: channel, not the temp.
        assert_eq!(soil.len(), 1);
        assert_eq!(soil[0].id, "ha:sensor.back_soil_moisture");
        assert_eq!(soil[0].current_pct, Some(42.0));
    }

    #[test]
    fn filter_ha_soil_empty_when_no_soil_group() {
        let discovered = serde_json::json!({ "temperature": [] });
        assert!(filter_ha_soil(&discovered).is_empty());
    }

    #[test]
    fn has_ha_passthrough_detects_bridge() {
        let with = serde_json::json!({
            "sources": [
                {"id": "x", "kind": "mqtt", "config": {}},
                {"id": "ha", "kind": "ha_passthrough", "config": {}}
            ]
        });
        let without = serde_json::json!({
            "sources": [ {"id": "x", "kind": "mqtt", "config": {}} ]
        });
        assert!(has_ha_passthrough(&with));
        assert!(!has_ha_passthrough(&without));
        assert!(!has_ha_passthrough(&serde_json::json!({})));
    }
}
