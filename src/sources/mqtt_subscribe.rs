// MQTT subscribe source. Connects to any MQTT broker and turns
// subscribed topics into LocalSky Observation events. The
// standalone-friendly sensor ingestion path: pair with a Mosquitto
// container + ESPHome / Tasmota / Zigbee2MQTT publishers and you
// get full sensor coverage without Home Assistant.
//
// Each subscription maps:
//   topic -> (WeatherField, optional zone_slug, optional json_path,
//             linear scale + offset)
//
// Payload parsing:
//   - If json_path is set, drill into the payload as JSON and extract
//     a numeric value at that path. Dot-separated keys; ".0" indexes
//     into an array. e.g. "soil.0.moisture" reads obj["soil"][0]["moisture"].
//   - If json_path is unset, parse the entire payload as a number
//     (handles Tasmota-style "27.4" payloads).
//
// Reconnect strategy: rumqttc EventLoop handles reconnects automatically
// with backoff; the source's run() loop just consumes events until
// shutdown.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::collections::HashSet;
use tracing::{debug, info, warn};

use crate::config::schema::MqttSourceConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

pub struct MqttSubscribe {
    id: String,
    config: MqttSourceConfig,
}

impl MqttSubscribe {
    pub fn new(id: impl Into<String>, config: MqttSourceConfig) -> Self {
        Self {
            id: id.into(),
            config,
        }
    }

    fn fields_advertised(&self) -> HashSet<WeatherField> {
        let mut set = HashSet::new();
        for sub in &self.config.subscriptions {
            // A zone-bound subscription emits a per-zone soil CHANNEL
            // (KeyedReading), not a typed WeatherField Observation, so it
            // must NOT advertise a merge field (it would falsely claim to
            // produce e.g. RhPct).
            if sub
                .zone_slug
                .as_deref()
                .map(str::trim)
                .is_some_and(|z| !z.is_empty())
            {
                continue;
            }
            if let Some(field) = parse_weather_field(&sub.field) {
                set.insert(field);
            }
        }
        set
    }
}

#[async_trait]
impl WeatherSource for MqttSubscribe {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        SourceCaps {
            live_current: true,
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields: self.fields_advertised(),
        }
    }

    fn priority(&self, _field: WeatherField) -> i32 {
        // Mid priority: LAN MQTT broker likely faster than cloud
        // forecast but slower-truth than direct LAN UDP from a station.
        75
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        let client_id = self
            .config
            .client_id
            .clone()
            .unwrap_or_else(|| format!("localsky-source-{}", self.id));
        let mut opts = MqttOptions::new(
            &client_id,
            &self.config.broker_host,
            self.config.broker_port,
        );
        opts.set_keep_alive(Duration::from_secs(30));
        if let (Some(u), Some(p)) = (&self.config.username, &self.config.password) {
            opts.set_credentials(u, p);
        }
        let (client, mut eventloop) = AsyncClient::new(opts, 32);

        // Subscriptions are issued on every ConnAck (initial connect AND
        // each automatic reconnect). rumqttc reconnects with a clean
        // session and does NOT replay subscriptions, so subscribing only
        // once up front meant a broker restart left this source
        // connected but subscribed to nothing: permanent silent data
        // loss until the container restarted.
        info!(
            source = self.id,
            broker = self.config.broker_host,
            count = self.config.subscriptions.len(),
            "mqtt source starting; subscriptions issued on each connect"
        );

        loop {
            tokio::select! {
                ev = eventloop.poll() => {
                    match ev {
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            self.handle_publish(&bus, &p.topic, &p.payload);
                        }
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            let mut ok = 0usize;
                            for sub in &self.config.subscriptions {
                                match client.subscribe(&sub.topic, QoS::AtMostOnce).await {
                                    Ok(()) => ok += 1,
                                    Err(e) => warn!(
                                        source = self.id,
                                        topic = sub.topic,
                                        error = %e,
                                        "mqtt subscribe failed"
                                    ),
                                }
                            }
                            info!(
                                source = self.id,
                                subscribed = ok,
                                total = self.config.subscriptions.len(),
                                "mqtt source connected; subscriptions issued"
                            );
                        }
                        Ok(_) => {} // PingResp, SubAck, etc.
                        Err(e) => {
                            warn!(source = self.id, error = %e, "mqtt eventloop error; reconnecting");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source = self.id, "mqtt source shutting down");
                        let _ = client.disconnect().await;
                        return Ok(());
                    }
                }
            }
        }
    }
}

impl MqttSubscribe {
    fn handle_publish(&self, bus: &SourceBus, topic: &str, payload: &[u8]) {
        for sub in &self.config.subscriptions {
            if !topic_matches(&sub.topic, topic) {
                continue;
            }
            let Some(raw) = extract_numeric(payload, sub.json_path.as_deref()) else {
                debug!(
                    source = self.id,
                    topic = topic,
                    "could not extract numeric value from payload"
                );
                continue;
            };
            let value = raw * sub.scale + sub.offset;

            // Zone-bound subscription: emit a per-zone soil CHANNEL rather
            // than a global typed Observation. The merge bus is keyed by
            // WeatherField, which cannot tell "back yard soil" from "front
            // yard soil" (both would be RhPct) and would clobber the global
            // humidity field besides. Routing it as a KeyedReading writes a
            // discoverable `soilmoisture_<zone_slug>` channel to
            // sensor_history that the zone binds via
            // `source:<id>:soilmoisture_<zone_slug>` exactly like a native
            // Ecowitt channel. resolve_soil_pct + the soil discovery + the
            // Sensors inventory then all work through the normal binding.
            if let Some(zone) = sub
                .zone_slug
                .as_deref()
                .map(str::trim)
                .filter(|z| !z.is_empty())
            {
                let _ = bus.send(SourceEvent::KeyedReading {
                    source_id: self.id.clone(),
                    key: crate::sources::bus_recorder::zone_soil_key(zone),
                    value,
                    at_epoch: now_epoch(),
                });
                continue;
            }

            let Some(field) = parse_weather_field(&sub.field) else {
                debug!(
                    source = self.id,
                    field = sub.field,
                    "unknown weather field; dropping observation"
                );
                continue;
            };
            let _ = bus.send(SourceEvent::Observation {
                source_id: self.id.clone(),
                fields: vec![(field, value)],
                at_epoch: now_epoch(),
            });
        }
    }
}

/// MQTT wildcard match. `+` matches one segment; `#` matches any number
/// of trailing segments.
fn topic_matches(pattern: &str, topic: &str) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    let t: Vec<&str> = topic.split('/').collect();
    let mut i = 0;
    while i < p.len() {
        if p[i] == "#" {
            return true;
        }
        if i >= t.len() {
            return false;
        }
        if p[i] != "+" && p[i] != t[i] {
            return false;
        }
        i += 1;
    }
    i == t.len()
}

/// Extract a numeric value from `payload`. If `json_path` is set, drill
/// in dot-separated; ".0" indexes arrays. Otherwise parse the whole
/// payload as a number.
///
/// Pub so the http_webhook adapter can reuse the parsing logic without
/// duplicating it.
pub fn extract_numeric(payload: &[u8], json_path: Option<&str>) -> Option<f64> {
    let text = std::str::from_utf8(payload).ok()?.trim();
    if let Some(path) = json_path {
        let v: serde_json::Value = serde_json::from_str(text).ok()?;
        let leaf = walk_json(&v, path)?;
        return leaf.as_f64().or_else(|| {
            // Tasmota sometimes publishes numbers as strings.
            leaf.as_str().and_then(|s| s.parse::<f64>().ok())
        });
    }
    text.parse::<f64>().ok()
}

fn walk_json<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        if let Ok(idx) = seg.parse::<usize>() {
            cur = cur.get(idx)?;
        } else {
            cur = cur.get(seg)?;
        }
    }
    Some(cur)
}

/// Parse the snake-case form of a WeatherField variant. Mirrors the
/// SourceKind enum's #[serde(rename_all = "snake_case")] convention.
///
/// Pub so the http_webhook adapter can reuse it.
pub fn parse_weather_field(name: &str) -> Option<WeatherField> {
    use WeatherField::*;
    Some(match name {
        "air_temp_f" => AirTempF,
        "dew_point_f" => DewPointF,
        "rh_pct" => RhPct,
        "wind_mph" => WindMph,
        "wind_gust_mph" => WindGustMph,
        "wind_bearing_deg" => WindBearingDeg,
        "solar_w_m2" => SolarWm2,
        "uv_index" => UvIndex,
        "illuminance" => Illuminance,
        "pressure_in_hg" => PressureInHg,
        "rain_today_in" => RainTodayIn,
        "rain_intensity_in_hr" => RainIntensityInHr,
        "rain_type_str" => RainTypeStr,
        "lightning_count" => LightningCount,
        "lightning_distance_mi" => LightningDistanceMi,
        "et0_today" => Et0Today,
        "flow_gpm" => FlowGpm,
        "flow_total_gal_today" => FlowTotalGalToday,
        _ => return None,
    })
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_match_exact() {
        assert!(topic_matches("home/sensor", "home/sensor"));
        assert!(!topic_matches("home/sensor", "home/light"));
    }

    #[test]
    fn topic_match_plus_wildcard() {
        assert!(topic_matches("home/+/temp", "home/back_yard/temp"));
        assert!(topic_matches("home/+/temp", "home/front_yard/temp"));
        assert!(!topic_matches("home/+/temp", "home/back_yard/rh"));
        assert!(!topic_matches("home/+/temp", "home/back_yard/sub/temp"));
    }

    #[test]
    fn topic_match_hash_wildcard() {
        assert!(topic_matches("tasmota/#", "tasmota/foo"));
        assert!(topic_matches("tasmota/#", "tasmota/foo/bar/baz"));
        assert!(!topic_matches("tasmota/#", "other/foo"));
    }

    #[test]
    fn extract_plain_number() {
        let v = extract_numeric(b"27.4", None);
        assert_eq!(v, Some(27.4));
    }

    #[test]
    fn extract_plain_number_with_whitespace() {
        let v = extract_numeric(b" 42.0 \n", None);
        assert_eq!(v, Some(42.0));
    }

    #[test]
    fn extract_json_path_simple() {
        let payload = br#"{"soil_moisture": 38.5}"#;
        let v = extract_numeric(payload, Some("soil_moisture"));
        assert_eq!(v, Some(38.5));
    }

    #[test]
    fn extract_json_path_nested_with_array() {
        let payload = br#"{"sensors": [{"value": 12.3}, {"value": 99.9}]}"#;
        let v = extract_numeric(payload, Some("sensors.0.value"));
        assert_eq!(v, Some(12.3));
        let v2 = extract_numeric(payload, Some("sensors.1.value"));
        assert_eq!(v2, Some(99.9));
    }

    #[test]
    fn extract_tasmota_string_number() {
        let payload = br#"{"reading": "27.4"}"#;
        let v = extract_numeric(payload, Some("reading"));
        assert_eq!(v, Some(27.4));
    }

    #[test]
    fn extract_returns_none_for_invalid() {
        assert_eq!(extract_numeric(b"not a number", None), None);
        assert_eq!(extract_numeric(b"{}", Some("missing")), None);
    }

    #[test]
    fn parse_field_known_variants() {
        assert!(matches!(
            parse_weather_field("air_temp_f"),
            Some(WeatherField::AirTempF)
        ));
        assert!(matches!(
            parse_weather_field("rh_pct"),
            Some(WeatherField::RhPct)
        ));
        assert!(parse_weather_field("not_a_field").is_none());
    }

    #[test]
    fn parse_flow_fields() {
        assert!(matches!(
            parse_weather_field("flow_gpm"),
            Some(WeatherField::FlowGpm)
        ));
        assert!(matches!(
            parse_weather_field("flow_total_gal_today"),
            Some(WeatherField::FlowTotalGalToday)
        ));
    }

    use crate::config::schema::{MqttSourceConfig, MqttSubscription};

    fn sub(
        topic: &str,
        field: &str,
        json_path: Option<&str>,
        zone: Option<&str>,
    ) -> MqttSubscription {
        MqttSubscription {
            topic: topic.to_string(),
            field: field.to_string(),
            json_path: json_path.map(str::to_string),
            zone_slug: zone.map(str::to_string),
            scale: 1.0,
            offset: 0.0,
        }
    }

    fn source_with(subs: Vec<MqttSubscription>) -> MqttSubscribe {
        MqttSubscribe::new(
            "garden_mqtt",
            MqttSourceConfig {
                broker_host: "broker.local".into(),
                broker_port: 1883,
                username: None,
                password: None,
                client_id: None,
                subscriptions: subs,
            },
        )
    }

    // A zone-bound soil subscription must NOT emit a global typed Observation
    // (which would clobber humidity and lose the zone). It emits a per-zone
    // soil CHANNEL keyed `soilmoisture_<zone_slug>` so the zone can bind it
    // via `source:<id>:soilmoisture_<zone_slug>` like a native channel.
    #[test]
    fn zone_bound_soil_emits_per_zone_channel() {
        let src = source_with(vec![sub(
            "esp/soil/back",
            super::super::bus_recorder::zone_soil_key("back_yard").as_str(), // field ignored when zone-bound
            None,
            Some("back_yard"),
        )]);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<SourceEvent>(8);
        src.handle_publish(&tx, "esp/soil/back", b"38.5");

        match rx.try_recv().expect("an event was emitted") {
            SourceEvent::KeyedReading {
                source_id,
                key,
                value,
                ..
            } => {
                assert_eq!(source_id, "garden_mqtt");
                assert_eq!(key, "soilmoisture_back_yard");
                assert_eq!(value, 38.5);
            }
            other => panic!("expected KeyedReading, got {other:?}"),
        }
        // And nothing else (no stray global Observation).
        assert!(rx.try_recv().is_err(), "no second event");
    }

    // An UNbound soil subscription keeps the legacy behavior: a global typed
    // Observation routed through rh_pct.
    #[test]
    fn unbound_soil_emits_global_observation() {
        let src = source_with(vec![sub("esp/soil/loose", "rh_pct", None, None)]);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<SourceEvent>(8);
        src.handle_publish(&tx, "esp/soil/loose", b"41.0");

        match rx.try_recv().expect("an event was emitted") {
            SourceEvent::Observation { fields, .. } => {
                assert_eq!(fields, vec![(WeatherField::RhPct, 41.0)]);
            }
            other => panic!("expected Observation, got {other:?}"),
        }
    }

    // A blank/whitespace zone_slug is treated as unbound (matches the form's
    // serialize-blank-to-null and the schema #[serde(default)] Option).
    #[test]
    fn blank_zone_slug_falls_back_to_global_observation() {
        let src = source_with(vec![sub("esp/soil/x", "rh_pct", None, Some("   "))]);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<SourceEvent>(8);
        src.handle_publish(&tx, "esp/soil/x", b"50.0");
        assert!(matches!(
            rx.try_recv().unwrap(),
            SourceEvent::Observation { .. }
        ));
    }

    // The scale/offset transform applies to the zone-bound channel value too.
    #[test]
    fn zone_bound_soil_applies_scale_offset() {
        let mut s = sub("esp/raw", "rh_pct", None, Some("front_yard"));
        s.scale = 0.1;
        s.offset = 5.0;
        let src = source_with(vec![s]);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<SourceEvent>(8);
        src.handle_publish(&tx, "esp/raw", b"300"); // 300*0.1 + 5 = 35
        match rx.try_recv().unwrap() {
            SourceEvent::KeyedReading { key, value, .. } => {
                assert_eq!(key, "soilmoisture_front_yard");
                assert!((value - 35.0).abs() < 1e-9);
            }
            other => panic!("expected KeyedReading, got {other:?}"),
        }
    }
}
