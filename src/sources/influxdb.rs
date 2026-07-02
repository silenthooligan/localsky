// InfluxDB source (InfluxQL via the JSON /query endpoint).
//
// For the self-hoster who already stores weather/soil in InfluxDB and wants
// LocalSky to read it. Each configured InfluxQL query runs once per poll against
// `GET {url}/query?db=<database>[&org=<org>]&q=<influxql>` and maps the most
// recent value to a WeatherField or a per-zone soil channel (scale/offset).
//
// Works against InfluxDB 1.x (db + optional basic-auth) and 2.x (org + token via
// the v1-compatible /query endpoint). JSON results only -- no Flux/CSV, so no
// new dependency. Rides the merge bus like every other source, and outbound
// requests go through net::safe_fetch (SSRF-hardened).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::InfluxDbConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::mqtt_subscribe::parse_weather_field;

const INFLUX_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_INTERVAL_S: u64 = 10;

pub struct InfluxDb {
    id: String,
    config: InfluxDbConfig,
}

/// InfluxQL JSON response (the bits we read):
/// {"results":[{"series":[{"columns":["time","last"],"values":[[ts, 72.4]]}]}]}
#[derive(Debug, Deserialize)]
struct InfluxResp {
    #[serde(default)]
    results: Vec<InfluxResult>,
}
#[derive(Debug, Deserialize)]
struct InfluxResult {
    #[serde(default)]
    series: Vec<InfluxSeries>,
}
#[derive(Debug, Deserialize)]
struct InfluxSeries {
    #[serde(default)]
    columns: Vec<String>,
    #[serde(default)]
    values: Vec<Vec<serde_json::Value>>,
}

/// Pull the latest numeric value from an InfluxQL response: the LAST row of the
/// first series, taking the first non-"time" column that is numeric (a number,
/// or a stringified number). Returns None for an empty result.
fn latest_value(resp: &InfluxResp) -> Option<f64> {
    let series = resp.results.first()?.series.first()?;
    let row = series.values.last()?;
    for (i, cell) in row.iter().enumerate() {
        // Skip the time column (by name when known, else assume index 0).
        let is_time = series
            .columns
            .get(i)
            .map(|c| c.eq_ignore_ascii_case("time"))
            .unwrap_or(i == 0);
        if is_time {
            continue;
        }
        if let Some(n) = cell.as_f64() {
            return Some(n);
        }
        if let Some(n) = cell.as_str().and_then(|s| s.parse::<f64>().ok()) {
            return Some(n);
        }
    }
    None
}

impl InfluxDb {
    pub fn new(id: impl Into<String>, config: InfluxDbConfig) -> Self {
        Self {
            id: id.into(),
            config,
        }
    }

    async fn query(&self, influxql: &str) -> anyhow::Result<f64> {
        let base = format!("{}/query", self.config.url.trim_end_matches('/'));
        let mut url = reqwest::Url::parse(&base)?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("db", &self.config.database);
            if let Some(org) = &self.config.org {
                q.append_pair("org", org);
            }
            q.append_pair("q", influxql);
        }
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(url.as_str(), INFLUX_TIMEOUT).await?;
        let mut req = client.get(safe_url);
        if let Some(token) = &self.config.token {
            req = req.header("Authorization", format!("Token {token}"));
        } else if let Some(user) = &self.config.username {
            req = req.basic_auth(user, self.config.password.as_deref());
        }
        let resp: InfluxResp = req.send().await?.error_for_status()?.json().await?;
        latest_value(&resp).ok_or_else(|| anyhow::anyhow!("query returned no value: {influxql}"))
    }
}

#[async_trait]
impl WeatherSource for InfluxDb {
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
            "InfluxDb source started"
        );
        if self.config.queries.is_empty() {
            warn!(source_id = %self.id, "InfluxDb has no queries; idle");
        }
        let mut tick = interval(Duration::from_secs(interval_s));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let now = chrono::Utc::now().timestamp();
                    let mut fields: Vec<(WeatherField, f64)> = Vec::new();
                    let mut any_ok = false;
                    let mut any_err = false;
                    for q in &self.config.queries {
                        match self.query(&q.query).await {
                            Ok(raw) => {
                                any_ok = true;
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
                                any_err = true;
                                warn!(source_id = %self.id, query = q.query, error = %e, "InfluxDb query failed");
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
                    let reachable = any_ok || !any_err;
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
                        info!(source_id = %self.id, "InfluxDb shutdown");
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

    fn resp(v: serde_json::Value) -> InfluxResp {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn latest_value_numeric_last_row() {
        let r = resp(json!({
            "results": [ { "series": [ {
                "name": "weather", "columns": ["time", "last"],
                "values": [[1715000000000i64, 70.1], [1715000060000i64, 72.4]]
            } ] } ]
        }));
        assert_eq!(latest_value(&r), Some(72.4));
    }

    #[test]
    fn latest_value_stringified_number() {
        let r = resp(json!({
            "results": [ { "series": [ {
                "columns": ["time", "mean"], "values": [[1715000000000i64, "29.95"]]
            } ] } ]
        }));
        assert_eq!(latest_value(&r), Some(29.95));
    }

    #[test]
    fn latest_value_skips_time_column() {
        // Even if a non-time column precedes the value, time at index 0 is skipped.
        let r = resp(json!({
            "results": [ { "series": [ {
                "columns": ["time", "soil"], "values": [[1715000000000i64, 42.0]]
            } ] } ]
        }));
        assert_eq!(latest_value(&r), Some(42.0));
    }

    #[test]
    fn empty_results_is_none() {
        assert_eq!(latest_value(&resp(json!({ "results": [] }))), None);
        assert_eq!(
            latest_value(&resp(json!({ "results": [ { "series": [] } ] }))),
            None
        );
    }

    #[test]
    fn caps_reflect_field_queries_only() {
        let cfg = InfluxDbConfig {
            url: "http://influx.invalid:8086".into(),
            database: "weather".into(),
            org: None,
            token: None,
            username: None,
            password: None,
            poll_interval_s: 60,
            queries: vec![
                crate::config::schema::InfluxQuery {
                    field: "air_temp_f".into(),
                    zone_slug: None,
                    query: "SELECT last(\"t\") FROM weather".into(),
                    scale: 1.0,
                    offset: 0.0,
                },
                crate::config::schema::InfluxQuery {
                    field: String::new(),
                    zone_slug: Some("garden".into()),
                    query: "SELECT last(\"soil\") FROM soil".into(),
                    scale: 1.0,
                    offset: 0.0,
                },
            ],
        };
        let s = InfluxDb::new("influx", cfg);
        let caps = s.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::AirTempF));
        assert_eq!(caps.fields.len(), 1);
    }
}
