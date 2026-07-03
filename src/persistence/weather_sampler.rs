// Weather sampler. The live path populates the Tempest snapshot but never
// recorded a time series, so the Weather home had no trend data. This task
// samples the snapshot once a packet (deduped by last_packet_epoch) into
// the shared sensor_history table, giving the telemetry strip real
// sparklines and feeding /api/health freshness. Cheap: INSERT OR IGNORE,
// a handful of rows per packet.

use std::sync::Arc;
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::persistence::sensor_history::{Reading, SensorHistoryStore};
use crate::tempest::state::TempestStore;

const SOURCE_ID: &str = "tempest";

/// Spawn the sampler. Records ~one sample per Tempest packet, polling every
/// 60s. No-op rows (INSERT OR IGNORE) make restarts and overlapping epochs
/// harmless. `retention_days` MUST be the configured `[persistence].retention_days`:
/// the sampler's hourly piggyback prune runs a TABLE-WIDE delete over
/// sensor_history, so building the store with the default (90) instead of the
/// configured value would silently delete every source's rows older than 90
/// days even when the operator set retention to 0 (keep forever) or a larger
/// window, contradicting the documented knob.
pub fn spawn_weather_sampler(
    conn: Arc<Mutex<Connection>>,
    tempest: Arc<TempestStore>,
    retention_days: u32,
) {
    let store = SensorHistoryStore::new(conn).with_retention_days(retention_days);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        let mut last_epoch = 0i64;
        loop {
            tick.tick().await;
            let s = tempest.snapshot();
            if s.last_packet_epoch <= 0 || s.last_packet_epoch == last_epoch {
                continue;
            }
            // Only sample the off-bus paths that own the snapshot: the live
            // Tempest UDP path, and the demo feeder ("Demo") so the demo
            // dashboard keeps its telemetry sparklines. Bus sources
            // (Ecowitt/Davis/...) are already persisted to sensor_history by the
            // bus recorder under their own id; sampling them here would
            // double-record and mis-attribute their data to "tempest".
            if !matches!(s.source_label.as_str(), "Tempest" | "Demo") {
                continue;
            }
            last_epoch = s.last_packet_epoch;
            let e = s.last_packet_epoch;
            let mk = |key: &str, value: f64| Reading {
                epoch: e,
                source_id: SOURCE_ID.to_string(),
                key: key.to_string(),
                value,
            };
            let readings = vec![
                mk("air_temp_f", s.air_temp_f),
                mk("rh_pct", s.rh_pct),
                mk("wind_avg_mph", s.wind_avg_mph),
                mk("pressure_inhg", s.pressure_inhg),
                mk("solar_w_m2", s.solar_w_m2),
                mk("uv_index", s.uv_index),
                // Rain history. rain_today_in doubles as the restart seed
                // for the in-process accumulator (main.rs queries today's
                // MAX so a mid-storm reboot doesn't zero the daily total).
                mk("rain_today_in", s.rain_in_today),
                mk("rain_intensity_in_hr", s.rain_intensity_in_hr),
            ];
            if let Err(err) = store.insert_many(readings).await {
                tracing::warn!("weather sampler insert failed: {err:#}");
            }
        }
    });
}
