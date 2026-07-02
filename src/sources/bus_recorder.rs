// Source-bus recorder. The single consumer of the shared SourceEvent
// broadcast bus that turns adapter observations into durable state:
//
//   1. sensor_history rows (one per (epoch, source_id, field)) so the
//      Sensors page, soil pickers, and /api/health freshness all see
//      data the source actually produced, and
//   2. an in-memory per-source last-seen map that /api/health reads for
//      this-boot freshness without a SQLite round trip.
//
// Every source adapter (polling loops spawned by main.rs, plus the
// receiver-POST adapters behind /ingest/*) publishes on the same bus,
// so this is the one place observation flow is recorded. A future
// merge layer subscribes to the same bus; nothing here consumes events
// destructively (broadcast channels fan out per receiver).

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;
use tracing::{debug, warn};

use crate::persistence::sensor_history::Reading;
use crate::persistence::SensorHistoryStore;
use crate::ports::weather_source::{SourceEvent, WeatherField};

/// Shared per-source last-observation map. Cloneable handle; all clones
/// see the same state. Epochs are the adapter-reported `at_epoch`.
#[derive(Clone, Default)]
pub struct SourceLastSeen {
    inner: Arc<RwLock<HashMap<String, i64>>>,
}

impl SourceLastSeen {
    pub fn record(&self, source_id: &str, epoch: i64) {
        if let Ok(mut m) = self.inner.write() {
            let e = m.entry(source_id.to_string()).or_insert(i64::MIN);
            if epoch > *e {
                *e = epoch;
            }
        }
    }

    pub fn get(&self, source_id: &str) -> Option<i64> {
        self.inner
            .read()
            .ok()
            .and_then(|m| m.get(source_id).copied())
    }
}

/// Shared per-source last-REACHABLE map, the reachability twin of
/// `SourceLastSeen`. The adapters publish a `SourceEvent::Reachability` on every
/// successful fetch (e.g. noaa_mrms.rs, nws.rs); the bus recorder stamps the
/// receive epoch here on a `Reachability { reachable: true }` event so the
/// honest-status taxonomy can tell a reachable-but-quiet source (a dry rain
/// authority emitting no Observation) apart from a genuinely unreachable one.
/// Cloneable handle; all clones see the same state. Lives in the `sources` layer
/// (not `api`) so the bus recorder can record into it without `sources`
/// depending on `api`; main.rs threads the same handle into both
/// `HealthState.source_reachable` and the runtime so /api/config reads it too.
/// API mirrors `SourceLastSeen` exactly (default(), `record`, `get`).
#[derive(Clone, Default)]
pub struct SourceReachability {
    inner: Arc<RwLock<HashMap<String, i64>>>,
}

impl SourceReachability {
    /// Stamp `source_id` reachable as of `epoch` (monotonic: never regresses).
    pub fn record(&self, source_id: &str, epoch: i64) {
        if let Ok(mut m) = self.inner.write() {
            let e = m.entry(source_id.to_string()).or_insert(i64::MIN);
            if epoch > *e {
                *e = epoch;
            }
        }
    }

    /// Epoch this source was last reachable, or None if never recorded.
    pub fn get(&self, source_id: &str) -> Option<i64> {
        self.inner
            .read()
            .ok()
            .and_then(|m| m.get(source_id).copied())
    }
}

/// Canonical sensor_history key for a zone-bound soil-moisture channel.
/// Prefixed `soilmoisture` so the `soil_channels` discovery LIKE query
/// (`soilmoisture%`) finds it, and suffixed `_<zone_slug>` so a zone binds
/// it via `source:<source_id>:soilmoisture_<zone_slug>` the same way a
/// native Ecowitt `soilmoisture<N>` channel binds. Keeping the form, the
/// engine emit, and the resolver in agreement on this one function is what
/// makes the zone-bound MQTT soil path round-trip.
pub fn zone_soil_key(zone_slug: &str) -> String {
    format!("soilmoisture_{zone_slug}")
}

/// Canonical snake_case key for a WeatherField, used as the sensor_history
/// `key` column for bus observations. Most keys match the names the
/// MQTT/webhook field mappings accept (`parse_weather_field`); the exceptions
/// are `WindMph` and `PressureInHg`, which deliberately use the
/// sampler/api/manifest history keys (`wind_avg_mph` / `pressure_inhg`) so a
/// bus source's wind/pressure shows up in the sparkline history.
pub fn weather_field_key(f: WeatherField) -> &'static str {
    use WeatherField::*;
    match f {
        AirTempF => "air_temp_f",
        DewPointF => "dew_point_f",
        RhPct => "rh_pct",
        // Wind + pressure keys match the weather_sampler / api::weather /
        // manifest history keys so a bus source's wind/pressure also shows in
        // the sparkline history (api::weather reads "wind_avg_mph"/"pressure_inhg").
        WindMph => "wind_avg_mph",
        WindGustMph => "wind_gust_mph",
        WindBearingDeg => "wind_bearing_deg",
        SolarWm2 => "solar_w_m2",
        UvIndex => "uv_index",
        Illuminance => "illuminance",
        PressureInHg => "pressure_inhg",
        RainTodayIn => "rain_today_in",
        RainIntensityInHr => "rain_intensity_in_hr",
        RainTypeStr => "rain_type_str",
        LightningCount => "lightning_count",
        LightningDistanceMi => "lightning_distance_mi",
        Et0Today => "et0_today",
        FlowGpm => "flow_gpm",
        FlowTotalGalToday => "flow_total_gal_today",
        LeafWetness => "leaf_wetness_pct",
        ForecastDaily => "forecast_daily",
        ForecastHourly => "forecast_hourly",
        Pop => "pop",
    }
}

/// Spawn the recorder task. Subscribes to `bus` and, for every
/// Observation event, updates `last_seen` and (when a history store is
/// mounted) persists each field as a sensor_history row. A
/// `Reachability { reachable: true }` event stamps the receive epoch into
/// `source_reachable` (the reachability twin of `last_seen`), which the
/// honest-status taxonomy reads so a reachable-but-quiet source (a dry rain
/// authority emitting no Observation) reads `watching`, never `offline`. A
/// `reachable: false` is logged only and does NOT advance the epoch.
pub fn spawn(
    bus: broadcast::Sender<SourceEvent>,
    sensor_history: Option<SensorHistoryStore>,
    last_seen: SourceLastSeen,
    source_reachable: SourceReachability,
) {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(SourceEvent::Observation {
                    source_id,
                    fields,
                    at_epoch,
                }) => {
                    last_seen.record(&source_id, at_epoch);
                    if let Some(store) = sensor_history.as_ref() {
                        let readings: Vec<Reading> = fields
                            .iter()
                            .map(|(f, v)| Reading {
                                epoch: at_epoch,
                                source_id: source_id.clone(),
                                key: weather_field_key(*f).to_string(),
                                value: *v,
                            })
                            .collect();
                        if !readings.is_empty() {
                            if let Err(e) = store.insert_many(readings).await {
                                warn!(source = %source_id, "bus recorder history write failed: {e}");
                            }
                        }
                    }
                }
                Ok(SourceEvent::KeyedReading {
                    source_id,
                    key,
                    value,
                    at_epoch,
                }) => {
                    last_seen.record(&source_id, at_epoch);
                    if let Some(store) = sensor_history.as_ref() {
                        let reading = Reading {
                            epoch: at_epoch,
                            source_id: source_id.clone(),
                            key,
                            value,
                        };
                        if let Err(e) = store.insert(reading).await {
                            warn!(source = %source_id, "bus recorder keyed history write failed: {e}");
                        }
                    }
                }
                Ok(SourceEvent::Forecast {
                    source_id,
                    at_epoch,
                    ..
                }) => {
                    // Forecast is structured (handled by forecast_bridge), not a
                    // sensor_history row, but record freshness so the forecast
                    // source shows live in /api/health.
                    last_seen.record(&source_id, at_epoch);
                }
                Ok(SourceEvent::Reachability {
                    source_id,
                    reachable,
                }) => {
                    // A successful fetch stamps the source reachable as of NOW
                    // (the Reachability variant carries no epoch). The taxonomy
                    // reads this dedicated channel so a reachable-but-quiet source
                    // (no Observation this cycle) reads `watching`, not `offline`.
                    // A `reachable: false` must NOT advance the epoch, so a real
                    // connectivity fault still ages into `offline`.
                    if reachable {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0);
                        source_reachable.record(&source_id, now);
                    }
                    debug!(source = %source_id, reachable, "source reachability changed");
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(skipped = n, "bus recorder lagged; observations skipped");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    debug!("source bus closed; recorder exiting");
                    return;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;
    use crate::sources::mqtt_subscribe::parse_weather_field;
    use rusqlite::Connection;
    use tokio::sync::Mutex;

    #[test]
    fn field_keys_round_trip_through_parser() {
        use WeatherField::*;
        for f in [
            AirTempF,
            DewPointF,
            RhPct,
            WindMph,
            WindGustMph,
            WindBearingDeg,
            SolarWm2,
            UvIndex,
            Illuminance,
            PressureInHg,
            RainTodayIn,
            RainIntensityInHr,
            RainTypeStr,
            LightningCount,
            LightningDistanceMi,
            Et0Today,
            FlowGpm,
            FlowTotalGalToday,
        ] {
            let key = weather_field_key(f);
            // parse_weather_field covers the mappable subset; every key
            // it knows must round-trip to the same variant.
            if let Some(parsed) = parse_weather_field(key) {
                assert_eq!(parsed, f, "key {key} did not round-trip");
            }
        }
        // WindMph + PressureInHg deliberately use the sampler/api history keys
        // (which parse_weather_field doesn't map back), so the round-trip loop
        // above skips them; assert their canonical key explicitly so the rename
        // can't silently drift.
        assert_eq!(weather_field_key(WindMph), "wind_avg_mph");
        assert_eq!(weather_field_key(PressureInHg), "pressure_inhg");
    }

    #[test]
    fn last_seen_keeps_newest_epoch() {
        let ls = SourceLastSeen::default();
        ls.record("a", 100);
        ls.record("a", 50); // older must not regress
        ls.record("a", 200);
        assert_eq!(ls.get("a"), Some(200));
        assert_eq!(ls.get("missing"), None);
    }

    #[test]
    fn reachability_map_keeps_newest_epoch() {
        let r = SourceReachability::default();
        r.record("mrms", 100);
        r.record("mrms", 50); // older must not regress
        r.record("mrms", 200);
        assert_eq!(r.get("mrms"), Some(200));
        assert_eq!(r.get("missing"), None);
    }

    #[tokio::test]
    async fn recorder_records_reachable_true_and_ignores_false() {
        let (tx, _rx0) = broadcast::channel::<SourceEvent>(16);
        let reach = SourceReachability::default();
        spawn(tx.clone(), None, SourceLastSeen::default(), reach.clone());

        // A `reachable: false` must NOT advance the epoch (stays None).
        tx.send(SourceEvent::Reachability {
            source_id: "mrms".into(),
            reachable: false,
        })
        .unwrap();
        // A `reachable: true` stamps the source reachable as of now.
        tx.send(SourceEvent::Reachability {
            source_id: "mrms".into(),
            reachable: true,
        })
        .unwrap();

        for _ in 0..50 {
            if reach.get("mrms").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // A reachable event was recorded with a sane recent epoch.
        let stamped = reach.get("mrms").expect("reachable:true should record");
        assert!(stamped > 0, "reachable epoch should be a real timestamp");
        // The never-reachable source has no entry.
        assert_eq!(reach.get("never"), None);
    }

    #[tokio::test]
    async fn recorder_persists_observation_and_last_seen() {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        let store = SensorHistoryStore::new(Arc::new(Mutex::new(c)));
        let (tx, _rx0) = broadcast::channel::<SourceEvent>(16);
        let ls = SourceLastSeen::default();
        let reach = SourceReachability::default();
        spawn(tx.clone(), Some(store.clone()), ls.clone(), reach.clone());

        tx.send(SourceEvent::Observation {
            source_id: "nws_test".into(),
            fields: vec![(WeatherField::AirTempF, 71.5), (WeatherField::Pop, 40.0)],
            at_epoch: 1_700_000_000,
        })
        .unwrap();

        // The recorder runs on a spawned task; poll briefly.
        for _ in 0..50 {
            if ls.get("nws_test").is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(ls.get("nws_test"), Some(1_700_000_000));

        // History got one row per field, keyed canonically.
        for _ in 0..50 {
            let rows = store.latest_for_source("nws_test".into()).await.unwrap();
            if rows.len() == 2 {
                assert!(rows
                    .iter()
                    .any(|r| r.key == "air_temp_f" && (r.value - 71.5).abs() < 1e-9));
                assert!(rows
                    .iter()
                    .any(|r| r.key == "pop" && (r.value - 40.0).abs() < 1e-9));
                let seen = store
                    .last_seen_per_source(vec!["nws_test".into()])
                    .await
                    .unwrap();
                assert_eq!(seen.get("nws_test"), Some(&1_700_000_000));
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("recorder did not persist rows in time");
    }
}
