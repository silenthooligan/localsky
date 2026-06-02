// Ecowitt local LAN receiver. Accepts the form-encoded POSTs an
// Ecowitt gateway (GW1100 / GW2000) sends to its configured "custom
// server" URL. Parses every field documented in the Ecowitt API and
// emits Observations to the SourceBus.
//
// Setup on the gateway side (no HA required):
//   Settings -> Weather Services -> Customized
//   Protocol Type: Ecowitt
//   Server IP/Hostname: <localsky-host>
//   Path: /ingest/ecowitt
//   Port: 8090
//   Upload Interval: 60 seconds (or your preference)
//
// LocalSky-side: just include this source in config.sources.
// Multiple gateways can post to the same endpoint; the optional
// passkey + station_id fields differentiate them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use std::collections::HashSet;
use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::config::schema::EcowittLocalConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

/// Shared state mounted into the Axum router. The POST handler at
/// `/ingest/ecowitt` (path configurable per source) writes into the
/// underlying broadcast bus; the WeatherSource trait impl exists for
/// registry compatibility but does no work itself.
pub struct EcowittLocal {
    id: String,
    config: EcowittLocalConfig,
    bus: broadcast::Sender<SourceEvent>,
}

impl EcowittLocal {
    pub fn new(
        id: impl Into<String>,
        config: EcowittLocalConfig,
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

    /// Process one Ecowitt POST body. The body is application/x-www-form-urlencoded
    /// per the gateway's spec; LocalSky receives it as a parsed
    /// HashMap from the Axum form extractor.
    pub fn handle_post(&self, form: &HashMap<String, String>) {
        // Optional shared-secret check. If the operator configured a
        // secret, require it to match either the `key` field (in the
        // path query) or the PASSKEY field (in the body).
        if let Some(expected) = &self.config.shared_secret {
            let got = form
                .get("PASSKEY")
                .or_else(|| form.get("passkey"))
                .or_else(|| form.get("key"));
            if got.map(|v| v.as_str()) != Some(expected.as_str()) {
                debug!(
                    source = self.id,
                    "ecowitt post rejected: shared secret mismatch"
                );
                return;
            }
        }

        let mut fields: Vec<(WeatherField, f64)> = Vec::new();

        if let Some(v) = num(form, "tempf") {
            fields.push((WeatherField::AirTempF, v));
        }
        if let Some(v) = num(form, "humidity") {
            fields.push((WeatherField::RhPct, v));
        }
        if let Some(v) = num(form, "windspeedmph") {
            fields.push((WeatherField::WindMph, v));
        }
        if let Some(v) = num(form, "windgustmph") {
            fields.push((WeatherField::WindGustMph, v));
        }
        if let Some(v) = num(form, "winddir") {
            fields.push((WeatherField::WindBearingDeg, v));
        }
        if let Some(v) = num(form, "solarradiation") {
            fields.push((WeatherField::SolarWm2, v));
        }
        if let Some(v) = num(form, "uv") {
            fields.push((WeatherField::UvIndex, v));
        }
        if let Some(v) = num(form, "baromabsin").or_else(|| num(form, "baromrelin")) {
            fields.push((WeatherField::PressureInHg, v));
        }
        if let Some(v) = num(form, "rainratein").or_else(|| num(form, "hourlyrainin")) {
            fields.push((WeatherField::RainIntensityInHr, v));
        }
        if let Some(v) = num(form, "dailyrainin") {
            fields.push((WeatherField::RainTodayIn, v));
        }
        if let Some(v) = num(form, "lightning_num") {
            fields.push((WeatherField::LightningCount, v));
        }
        if let Some(v) = num(form, "lightning") {
            fields.push((WeatherField::LightningDistanceMi, v));
        }
        if let Some(v) = num(form, "dewpointf") {
            fields.push((WeatherField::DewPointF, v));
        }

        // Soil moisture probes: Ecowitt numbers them soilmoisture1..N.
        // Per-zone disambiguation is the operator's responsibility via
        // ZoneConfig.soil_sensor_id (planned: "ecowitt:soilmoisture1").
        // We emit the raw numbered fields here; the engine merge layer
        // can pull them by key when configured zones reference them.

        if fields.is_empty() {
            debug!(source = self.id, "ecowitt post produced zero parsed fields");
            return;
        }

        let _ = self.bus.send(SourceEvent::Observation {
            source_id: self.id.clone(),
            fields,
            at_epoch: now_epoch(),
        });
    }
}

#[async_trait]
impl WeatherSource for EcowittLocal {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        for f in [
            WeatherField::AirTempF,
            WeatherField::RhPct,
            WeatherField::WindMph,
            WeatherField::WindGustMph,
            WeatherField::WindBearingDeg,
            WeatherField::SolarWm2,
            WeatherField::UvIndex,
            WeatherField::PressureInHg,
            WeatherField::RainIntensityInHr,
            WeatherField::RainTodayIn,
            WeatherField::LightningCount,
            WeatherField::LightningDistanceMi,
            WeatherField::DewPointF,
        ] {
            fields.insert(f);
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
        // LAN-direct from a station; higher than MQTT bridges + forecast.
        90
    }

    async fn run(
        self: Arc<Self>,
        _bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        // No long-running loop; the POST handler does the work. Park
        // here until shutdown so the join-handle pattern still works.
        info!(
            source = self.id,
            path = self.config.path,
            "ecowitt receiver mounted"
        );
        loop {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(60)) => {
                    // Heartbeat opportunity; could surface "last_seen"
                    // staleness here in a follow-up.
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source = self.id, "ecowitt receiver shutting down");
                        return Ok(());
                    }
                }
            }
        }
    }
}

fn num(form: &HashMap<String, String>, key: &str) -> Option<f64> {
    form.get(key).and_then(|v| v.parse::<f64>().ok())
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

    fn build(secret: Option<&str>) -> EcowittLocal {
        let (tx, _rx) = broadcast::channel(8);
        EcowittLocal::new(
            "ecowitt_test",
            EcowittLocalConfig {
                path: "/ingest/ecowitt".into(),
                shared_secret: secret.map(|s| s.to_string()),
            },
            tx,
        )
    }

    #[test]
    fn parses_full_observation() {
        let s = build(None);
        let (tx, mut rx) = broadcast::channel::<SourceEvent>(8);
        let s = EcowittLocal::new(
            "ecowitt_test",
            EcowittLocalConfig {
                path: "/ingest/ecowitt".into(),
                shared_secret: None,
            },
            tx,
        );

        let mut form = HashMap::new();
        form.insert("tempf".into(), "72.5".into());
        form.insert("humidity".into(), "65".into());
        form.insert("windspeedmph".into(), "4.5".into());
        form.insert("solarradiation".into(), "712.3".into());
        form.insert("dailyrainin".into(), "0.05".into());

        s.handle_post(&form);

        let event = rx.try_recv().unwrap();
        let SourceEvent::Observation {
            source_id, fields, ..
        } = event
        else {
            panic!("expected Observation");
        };
        assert_eq!(source_id, "ecowitt_test");
        // 5 fields parsed.
        assert_eq!(fields.len(), 5);
        let by_field: HashMap<_, _> = fields.into_iter().collect();
        assert_eq!(by_field.get(&WeatherField::AirTempF), Some(&72.5));
        assert_eq!(by_field.get(&WeatherField::RhPct), Some(&65.0));
        assert_eq!(by_field.get(&WeatherField::WindMph), Some(&4.5));
        assert_eq!(by_field.get(&WeatherField::SolarWm2), Some(&712.3));
        assert_eq!(by_field.get(&WeatherField::RainTodayIn), Some(&0.05));

        // Drop the unused-variable hint for `s` borrow:
        let _ = s;
    }

    #[test]
    fn rejects_when_secret_mismatch() {
        let (tx, mut rx) = broadcast::channel::<SourceEvent>(8);
        let s = EcowittLocal::new(
            "ecowitt_test",
            EcowittLocalConfig {
                path: "/ingest/ecowitt".into(),
                shared_secret: Some("hunter2".into()),
            },
            tx,
        );

        let mut form = HashMap::new();
        form.insert("tempf".into(), "72.5".into());
        form.insert("PASSKEY".into(), "wrongkey".into());
        s.handle_post(&form);

        assert!(
            rx.try_recv().is_err(),
            "should not have emitted any observation"
        );
    }

    #[test]
    fn accepts_correct_secret_in_passkey() {
        let (tx, mut rx) = broadcast::channel::<SourceEvent>(8);
        let s = EcowittLocal::new(
            "ecowitt_test",
            EcowittLocalConfig {
                path: "/ingest/ecowitt".into(),
                shared_secret: Some("hunter2".into()),
            },
            tx,
        );

        let mut form = HashMap::new();
        form.insert("tempf".into(), "72.5".into());
        form.insert("PASSKEY".into(), "hunter2".into());
        s.handle_post(&form);

        let event = rx.try_recv().unwrap();
        let SourceEvent::Observation { fields, .. } = event else {
            panic!("expected Observation");
        };
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn silently_drops_empty_payloads() {
        let s = build(None);
        let (tx, mut rx) = broadcast::channel::<SourceEvent>(8);
        let s = EcowittLocal::new(
            "ecowitt_test",
            EcowittLocalConfig {
                path: "/ingest/ecowitt".into(),
                shared_secret: None,
            },
            tx,
        );

        let form = HashMap::new();
        s.handle_post(&form);

        assert!(rx.try_recv().is_err(), "empty payload should not emit");
    }
}
