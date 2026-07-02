// Generic outbound REST poller.
//
// The counterpart to the HTTP-webhook RECEIVER (http_webhook.rs): instead of a
// device POSTing to LocalSky, this PULLS a JSON HTTP API on an interval and maps
// JSON paths -> WeatherFields / per-zone soil. One adapter covers any JSON
// weather API: WeatherUnderground PWS, Tomorrow.io, Aeris/Xweather, AcuRite
// cloud, Davis WeatherLink cloud, RainMachine-as-source, and the long tail of
// niche/regional services. Because it rides the merge bus like every other
// source, it inherits the snapshot, sensor_history, HA entities, source-health,
// and current-conditions arbitration for free.
//
// Field mapping reuses the HTTP-webhook shape (field, json_path, scale, offset,
// zone_slug). Units are handled by the user's scale/offset (e.g. C->F is
// scale=1.8, offset=32), matching the webhook receiver. Outbound requests go
// through net::safe_fetch (SSRF-hardened: forbidden-target filter, resolved-IP
// pin, no redirects), so a misconfigured URL can't reach loopback/metadata.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use std::collections::HashSet;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::RestPollConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::mqtt_subscribe::{extract_numeric, parse_weather_field};

const REST_TIMEOUT: Duration = Duration::from_secs(10);
/// Floor on the poll interval, to stay friendly to rate-limited APIs.
const MIN_INTERVAL_S: u64 = 10;

pub struct RestPoll {
    id: String,
    config: RestPollConfig,
}

impl RestPoll {
    pub fn new(id: impl Into<String>, config: RestPollConfig) -> Self {
        Self {
            id: id.into(),
            config,
        }
    }

    /// Fetch the configured URL through an SSRF-hardened client, applying the
    /// configured method, headers, and optional body. Returns the raw body.
    async fn fetch(&self) -> anyhow::Result<Vec<u8>> {
        let (client, safe_url) =
            crate::net::safe_fetch::build_safe_client(&self.config.url, REST_TIMEOUT).await?;
        let mut req = if self.config.method.eq_ignore_ascii_case("POST") {
            let r = client.post(safe_url);
            match &self.config.body {
                Some(b) => r.body(b.clone()),
                None => r,
            }
        } else {
            client.get(safe_url)
        };
        for (k, v) in &self.config.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let resp = req.send().await?.error_for_status()?;
        Ok(resp.bytes().await?.to_vec())
    }
}

#[async_trait]
impl WeatherSource for RestPoll {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        for f in &self.config.fields {
            if f.zone_slug.is_none() {
                if let Some(wf) = parse_weather_field(&f.field) {
                    fields.insert(wf);
                }
            }
        }
        SourceCaps {
            // A live poll of current conditions, unless it only carries soil.
            live_current: self
                .config
                .fields
                .iter()
                .any(|f| f.zone_slug.is_none() && parse_weather_field(&f.field).is_some()),
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        // Live-path arbitration uses the config SourceEntry.priority; this
        // adapter-level priority is only a fallback for the (dead) merge layer.
        if self
            .config
            .fields
            .iter()
            .any(|f| f.zone_slug.is_none() && parse_weather_field(&f.field) == Some(field))
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
            fields = self.config.fields.len(),
            "RestPoll source started"
        );
        if self.config.fields.is_empty() {
            warn!(source_id = %self.id, "RestPoll has no field mappings; idle");
        }
        let mut tick = interval(Duration::from_secs(interval_s));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch().await {
                        Ok(body) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            let now = chrono::Utc::now().timestamp();
                            let mut fields: Vec<(WeatherField, f64)> = Vec::new();
                            for m in &self.config.fields {
                                let Some(raw) = extract_numeric(&body, m.json_path.as_deref())
                                else {
                                    debug!(source_id = %self.id, field = m.field, "no numeric value at configured path");
                                    continue;
                                };
                                let value = raw * m.scale + m.offset;
                                // Per-zone soil channel -> KeyedReading.
                                if let Some(zone) = m
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
                                    continue;
                                }
                                let Some(wf) = parse_weather_field(&m.field) else {
                                    debug!(source_id = %self.id, field = m.field, "unknown field name");
                                    continue;
                                };
                                fields.push((wf, value));
                            }
                            if !fields.is_empty() {
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: self.id.clone(),
                                    fields,
                                    at_epoch: now,
                                });
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "RestPoll fetch failed");
                            if last_reachable != Some(false) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: false,
                                });
                                last_reachable = Some(false);
                            }
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source_id = %self.id, "RestPoll shutdown");
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
    use crate::config::schema::HttpWebhookField;

    fn field(name: &str, path: &str, scale: f64, zone: Option<&str>) -> HttpWebhookField {
        HttpWebhookField {
            field: name.to_string(),
            zone_slug: zone.map(|z| z.to_string()),
            json_path: Some(path.to_string()),
            scale,
            offset: 0.0,
        }
    }

    fn src(fields: Vec<HttpWebhookField>) -> RestPoll {
        RestPoll::new(
            "rest_test",
            RestPollConfig {
                url: "https://api.example.invalid/obs".into(),
                method: "GET".into(),
                poll_interval_s: 300,
                headers: Default::default(),
                body: None,
                fields,
            },
        )
    }

    #[test]
    fn capabilities_reflect_weather_fields_only() {
        let s = src(vec![
            field("air_temp_f", "temp", 1.0, None),
            field("", "soil", 1.0, Some("garden")), // soil-only, not a weather field
        ]);
        let caps = s.capabilities();
        assert!(caps.live_current, "has a weather field -> live");
        assert!(caps.fields.contains(&WeatherField::AirTempF));
        assert_eq!(caps.fields.len(), 1, "soil mapping is not a weather field");
    }

    #[test]
    fn soil_only_source_is_not_live_current() {
        let s = src(vec![field("", "soil", 1.0, Some("garden"))]);
        assert!(!s.capabilities().live_current);
    }

    #[test]
    fn priority_only_for_mapped_weather_fields() {
        let s = src(vec![field("air_temp_f", "temp", 1.0, None)]);
        assert_eq!(s.priority(WeatherField::AirTempF), 50);
        assert_eq!(s.priority(WeatherField::WindMph), i32::MIN);
    }
}
