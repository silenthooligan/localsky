// Prometheus instant-query source.
//
// For the self-hoster who already scrapes weather/soil metrics into Prometheus
// (a PWS exporter, an ESPHome/MQTT->prometheus bridge, a node_exporter textfile
// collector reading a sensor) and wants LocalSky to consume them. Each
// configured PromQL query is evaluated once per poll via the instant-query API
// (`GET {url}/api/v1/query?query=...`) and mapped to a WeatherField or a
// per-zone soil channel, with a scale/offset for units.
//
// Like every bus source it rides the merge bus, so it inherits the snapshot,
// sensor_history, HA entities, source-health, and current-conditions
// arbitration for free. Outbound requests go through net::safe_fetch
// (SSRF-hardened), and optional HTTP basic-auth covers a reverse-proxied
// Prometheus.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::PrometheusConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::mqtt_subscribe::parse_weather_field;

const PROM_TIMEOUT: Duration = Duration::from_secs(10);
/// Floor on the poll interval, to stay friendly to the Prometheus server.
const MIN_INTERVAL_S: u64 = 10;

pub struct Prometheus {
    id: String,
    config: PrometheusConfig,
}

/// Prometheus instant-query response (the bits we read).
#[derive(Debug, Deserialize)]
struct PromResp {
    status: String,
    data: PromData,
}

#[derive(Debug, Deserialize)]
struct PromData {
    #[serde(rename = "resultType")]
    result_type: String,
    result: serde_json::Value,
}

/// Pull the scalar sample value out of a Prometheus instant-query response.
/// Handles `resultType: "vector"` (take the first series' `value[1]`) and
/// `resultType: "scalar"` (`result[1]`). The value is a stringified float per
/// the Prometheus HTTP API. Returns None for an empty/unrecognized result.
fn scalar_from_response(resp: &PromResp) -> Option<f64> {
    if resp.status != "success" {
        return None;
    }
    let raw = match resp.data.result_type.as_str() {
        "vector" => resp
            .data
            .result
            .as_array()
            .and_then(|a| a.first())
            .and_then(|s| s.get("value"))
            .and_then(|v| v.as_array())
            .and_then(|v| v.get(1))
            .and_then(|v| v.as_str()),
        "scalar" => resp
            .data
            .result
            .as_array()
            .and_then(|v| v.get(1))
            .and_then(|v| v.as_str()),
        _ => None,
    }?;
    raw.parse::<f64>().ok()
}

impl Prometheus {
    pub fn new(id: impl Into<String>, config: PrometheusConfig) -> Self {
        Self {
            id: id.into(),
            config,
        }
    }

    /// Run one PromQL instant query through the SSRF-hardened client. The query
    /// is encoded into the URL (so build_safe_client pins the resolved host).
    async fn query(&self, promql: &str) -> anyhow::Result<f64> {
        let base = format!("{}/api/v1/query", self.config.url.trim_end_matches('/'));
        let mut url = reqwest::Url::parse(&base)?;
        url.query_pairs_mut().append_pair("query", promql);
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(url.as_str(), PROM_TIMEOUT).await?;
        let mut req = client.get(safe_url);
        if let Some(user) = &self.config.username {
            req = req.basic_auth(user, self.config.password.as_deref());
        }
        let resp: PromResp = req.send().await?.error_for_status()?.json().await?;
        scalar_from_response(&resp)
            .ok_or_else(|| anyhow::anyhow!("query returned no scalar sample: {promql}"))
    }
}

#[async_trait]
impl WeatherSource for Prometheus {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        for q in &self.config.queries {
            if q.zone_slug.is_none() {
                if let Some(wf) = parse_weather_field(&q.field) {
                    fields.insert(wf);
                }
            }
        }
        SourceCaps {
            live_current: !fields.is_empty(),
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        // Adapter-level priority for the legacy merge layer only; live
        // arbitration uses the config SourceEntry.priority.
        if self
            .config
            .queries
            .iter()
            .any(|q| q.zone_slug.is_none() && parse_weather_field(&q.field) == Some(field))
        {
            50
        } else {
            i32::MIN
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        let interval_s = self.config.poll_interval_s.max(MIN_INTERVAL_S);
        info!(
            source_id = %self.id,
            url = %self.config.url,
            interval_s,
            queries = self.config.queries.len(),
            "Prometheus source started"
        );
        if self.config.queries.is_empty() {
            warn!(source_id = %self.id, "Prometheus has no queries; idle");
        }
        let mut tick = interval(Duration::from_secs(interval_s));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let now = chrono::Utc::now().timestamp();
                    let mut fields: Vec<(WeatherField, f64)> = Vec::new();
                    // One transport failure marks the source unreachable; a parse
                    // miss on a single query is logged but not a reachability flip.
                    let mut any_transport_ok = false;
                    let mut any_transport_err = false;
                    for q in &self.config.queries {
                        match self.query(&q.query).await {
                            Ok(raw) => {
                                any_transport_ok = true;
                                let value = raw * q.scale + q.offset;
                                if let Some(zone) = q
                                    .zone_slug
                                    .as_deref()
                                    .map(str::trim)
                                    .filter(|z| !z.is_empty())
                                {
                                    let _ = bus.send(SourceEvent::KeyedReading {
                                        source_id: self.id.clone(),
                                        key: crate::sources::bus_recorder::zone_soil_key(zone),
                                        value,
                                        at_epoch: now,
                                    });
                                } else if let Some(wf) = parse_weather_field(&q.field) {
                                    fields.push((wf, value));
                                } else {
                                    debug!(source_id = %self.id, field = q.field, "unknown field name");
                                }
                            }
                            Err(e) => {
                                any_transport_err = true;
                                warn!(source_id = %self.id, query = q.query, error = %e, "Prometheus query failed");
                            }
                        }
                    }
                    if !fields.is_empty() {
                        let _ = bus.send(SourceEvent::Observation {
                            source_id: self.id.clone(),
                            fields,
                            at_epoch: now,
                        });
                    }
                    // Reachable when at least one query's transport succeeded;
                    // unreachable only when every query failed at transport.
                    let reachable = any_transport_ok || !any_transport_err;
                    if last_reachable != Some(reachable) {
                        let _ = bus.send(SourceEvent::Reachability {
                            source_id: self.id.clone(),
                            reachable,
                        });
                        last_reachable = Some(reachable);
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source_id = %self.id, "Prometheus shutdown");
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resp(v: serde_json::Value) -> PromResp {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn scalar_from_vector_takes_first_sample() {
        let r = resp(json!({
            "status": "success",
            "data": {
                "resultType": "vector",
                "result": [
                    { "metric": {"__name__": "weather_temp_f"}, "value": [1715000000.0, "72.4"] }
                ]
            }
        }));
        assert_eq!(scalar_from_response(&r), Some(72.4));
    }

    #[test]
    fn scalar_result_type() {
        let r = resp(json!({
            "status": "success",
            "data": { "resultType": "scalar", "result": [1715000000.0, "29.95"] }
        }));
        assert_eq!(scalar_from_response(&r), Some(29.95));
    }

    #[test]
    fn empty_vector_is_none() {
        let r = resp(json!({
            "status": "success",
            "data": { "resultType": "vector", "result": [] }
        }));
        assert_eq!(scalar_from_response(&r), None);
    }

    #[test]
    fn error_status_is_none() {
        let r = resp(json!({
            "status": "error",
            "data": { "resultType": "vector", "result": [] }
        }));
        assert_eq!(scalar_from_response(&r), None);
    }

    #[test]
    fn caps_reflect_field_queries_only() {
        let cfg = PrometheusConfig {
            url: "http://prom.invalid:9090".into(),
            poll_interval_s: 60,
            username: None,
            password: None,
            queries: vec![
                crate::config::schema::PrometheusQuery {
                    field: "air_temp_f".into(),
                    zone_slug: None,
                    query: "weather_temp_f".into(),
                    scale: 1.0,
                    offset: 0.0,
                },
                crate::config::schema::PrometheusQuery {
                    field: String::new(),
                    zone_slug: Some("garden".into()),
                    query: "soil_pct{zone=\"garden\"}".into(),
                    scale: 1.0,
                    offset: 0.0,
                },
            ],
        };
        let s = Prometheus::new("prom", cfg);
        let caps = s.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::AirTempF));
        assert_eq!(caps.fields.len(), 1, "soil query is not a global field");
    }
}
