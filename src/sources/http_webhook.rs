// Generic HTTP webhook receiver. Accepts arbitrary JSON POSTs at a
// configurable path and maps them to WeatherFields per the same
// (field, json_path, scale, offset) schema the MQTT subscribe adapter
// uses. Lets any Arduino / Pi script / commercial weather station with
// HTTP push capability feed LocalSky without needing MQTT in the middle.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use std::collections::HashSet;
use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::config::schema::HttpWebhookConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::mqtt_subscribe::{extract_numeric, parse_weather_field};

pub struct HttpWebhook {
    id: String,
    config: HttpWebhookConfig,
    bus: broadcast::Sender<SourceEvent>,
}

impl HttpWebhook {
    pub fn new(
        id: impl Into<String>,
        config: HttpWebhookConfig,
        bus: broadcast::Sender<SourceEvent>,
    ) -> Self {
        Self {
            id: id.into(),
            config,
            bus,
        }
    }

    pub fn path(&self) -> &str {
        &self.config.path
    }

    pub fn token(&self) -> Option<&str> {
        self.config.token.as_deref()
    }

    /// Process one webhook POST body (raw bytes). Verifies the optional
    /// token via either query string (?token=...) or header
    /// (X-LocalSky-Token), then walks each configured field mapping
    /// against the payload.
    ///
    /// Returns true if any observation was emitted.
    pub fn handle_post(&self, payload: &[u8], provided_token: Option<&str>) -> bool {
        if let Some(expected) = &self.config.token {
            if provided_token != Some(expected.as_str()) {
                debug!(source = self.id, "webhook post rejected: token mismatch");
                return false;
            }
        }

        let mut fields: Vec<(WeatherField, f64)> = Vec::new();
        for mapping in &self.config.fields {
            let Some(raw) = extract_numeric(payload, mapping.json_path.as_deref()) else {
                debug!(
                    source = self.id,
                    field = mapping.field,
                    "no numeric value at configured path"
                );
                continue;
            };
            let Some(field) = parse_weather_field(&mapping.field) else {
                debug!(source = self.id, field = mapping.field, "unknown field name");
                continue;
            };
            let value = raw * mapping.scale + mapping.offset;
            fields.push((field, value));
        }

        if fields.is_empty() {
            return false;
        }

        let _ = self.bus.send(SourceEvent::Observation {
            source_id: self.id.clone(),
            fields,
            at_epoch: now_epoch(),
        });
        true
    }
}

#[async_trait]
impl WeatherSource for HttpWebhook {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        for m in &self.config.fields {
            if let Some(f) = parse_weather_field(&m.field) {
                fields.insert(f);
            }
        }
        SourceCaps {
            live_current: true,
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, _field: WeatherField) -> i32 {
        // Mid priority: caller-defined source, no inherent quality signal.
        70
    }

    async fn run(
        self: Arc<Self>,
        _bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(source = self.id, path = self.config.path, "http webhook mounted");
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source = self.id, "http webhook shutting down");
                        return Ok(());
                    }
                }
            }
        }
    }
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
    use crate::config::schema::{HttpWebhookConfig, HttpWebhookField};

    fn build(token: Option<&str>, fields: Vec<HttpWebhookField>) -> (HttpWebhook, broadcast::Receiver<SourceEvent>) {
        let (tx, rx) = broadcast::channel::<SourceEvent>(8);
        let s = HttpWebhook::new(
            "webhook_test",
            HttpWebhookConfig {
                path: "/ingest/webhook".into(),
                token: token.map(|s| s.to_string()),
                fields,
            },
            tx,
        );
        (s, rx)
    }

    fn f(field: &str, json_path: Option<&str>, scale: f64) -> HttpWebhookField {
        HttpWebhookField {
            field: field.to_string(),
            json_path: json_path.map(|s| s.to_string()),
            scale,
            offset: 0.0,
        }
    }

    #[test]
    fn parses_json_payload() {
        let (s, mut rx) = build(None, vec![
            f("air_temp_f", Some("temperature"), 1.0),
            f("rh_pct", Some("humidity"), 1.0),
        ]);
        let body = br#"{"temperature": 72.5, "humidity": 65.0}"#;
        assert!(s.handle_post(body, None));
        let SourceEvent::Observation { fields, .. } = rx.try_recv().unwrap() else {
            panic!("expected observation");
        };
        assert_eq!(fields.len(), 2);
    }

    #[test]
    fn rejects_wrong_token() {
        let (s, mut rx) = build(Some("secret"), vec![f("air_temp_f", None, 1.0)]);
        let ok = s.handle_post(b"72.5", Some("wrong"));
        assert!(!ok);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn accepts_correct_token() {
        let (s, mut rx) = build(Some("secret"), vec![f("air_temp_f", None, 1.0)]);
        let ok = s.handle_post(b"72.5", Some("secret"));
        assert!(ok);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn applies_scale() {
        // Sensor publishes Celsius; map to F via scale=1.8, offset=32
        let (s, mut rx) = build(None, vec![
            HttpWebhookField {
                field: "air_temp_f".to_string(),
                json_path: None,
                scale: 1.8,
                offset: 32.0,
            },
        ]);
        let ok = s.handle_post(b"20.0", None);
        assert!(ok);
        let SourceEvent::Observation { fields, .. } = rx.try_recv().unwrap() else {
            panic!();
        };
        // 20C should map to 68F
        assert_eq!(fields[0].1, 68.0);
    }
}
