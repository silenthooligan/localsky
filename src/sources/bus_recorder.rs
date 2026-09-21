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
/// `SourceLastSeen`. Adapters publish both connectivity edges. The recorder
/// retains the latest verdict and stamps the last-success epoch so the
/// honest-status taxonomy can tell a reachable-but-quiet source (a dry rain
/// authority emitting no Observation) apart from a genuinely unreachable one.
/// Cloneable handle; all clones see the same state. Lives in the `sources` layer
/// (not `api`) so the bus recorder can record into it without `sources`
/// depending on `api`; main.rs threads the same handle into both
/// `HealthState.source_reachable` and the runtime so /api/config reads it too.
/// `get` returns the last success; `reachable` returns the current verdict.
#[derive(Clone, Default)]
pub struct SourceReachability {
    inner: Arc<RwLock<HashMap<String, ReachabilityState>>>,
    diagnostics: Arc<RwLock<HashMap<String, (i64, Option<SourceFailureRecord>)>>>,
}

pub use crate::failure::FailureRecord as SourceFailureRecord;

#[derive(Clone, Copy)]
struct ReachabilityState {
    last_success: Option<i64>,
    verdict: bool,
    at_epoch: i64,
}

impl SourceReachability {
    pub fn report_failure(
        &self,
        source_id: &str,
        failure: Option<crate::ports::source_error::SourceFailure>,
        at_epoch: i64,
    ) {
        if let Ok(mut map) = self.diagnostics.write() {
            if map.get(source_id).is_some_and(|(at, _)| *at > at_epoch) {
                return;
            }
            map.insert(
                source_id.to_owned(),
                (
                    at_epoch,
                    failure.map(|failure| SourceFailureRecord { at_epoch, failure }),
                ),
            );
        }
    }

    pub fn failure(&self, source_id: &str) -> Option<SourceFailureRecord> {
        self.diagnostics
            .read()
            .ok()
            .and_then(|map| map.get(source_id).and_then(|(_, record)| record.clone()))
    }
    /// Stamp `source_id` reachable as of `epoch` (monotonic: never regresses).
    pub fn record(&self, source_id: &str, epoch: i64) {
        self.report(source_id, true, epoch);
    }

    /// Preserve both edges of the adapter's verdict. Reachability is state,
    /// not a heartbeat: a quiet healthy adapter need not repeat `true`.
    pub fn report(&self, source_id: &str, reachable: bool, epoch: i64) {
        if let Ok(mut m) = self.inner.write() {
            if m.get(source_id).is_some_and(|s| s.at_epoch > epoch) {
                return;
            }
            let last_success = if reachable {
                Some(epoch)
            } else {
                m.get(source_id).and_then(|s| s.last_success)
            };
            m.insert(
                source_id.to_string(),
                ReachabilityState {
                    last_success,
                    verdict: reachable,
                    at_epoch: epoch,
                },
            );
        }
    }

    /// Epoch this source was last reachable, or None if never recorded.
    pub fn get(&self, source_id: &str) -> Option<i64> {
        self.inner
            .read()
            .ok()
            .and_then(|m| m.get(source_id).and_then(|s| s.last_success))
    }

    /// Last explicit verdict, or None before the adapter has reported one.
    pub fn reachable(&self, source_id: &str) -> Option<bool> {
        self.inner
            .read()
            .ok()
            .and_then(|m| m.get(source_id).map(|s| s.verdict))
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
    f.history_key()
}

/// Spawn the recorder task. Subscribes to `bus` and, for every
/// Observation event, updates `last_seen` and (when a history store is
/// mounted) persists each field as a sensor_history row. A
/// `Reachability { reachable: true }` event stamps the receive epoch into
/// `source_reachable` (the reachability twin of `last_seen`), which the
/// honest-status taxonomy reads so a reachable-but-quiet source (a dry rain
/// authority emitting no Observation) reads `watching`, never `offline`. A
/// `reachable: false` preserves the failure verdict without advancing the
/// last-success epoch. Both health surfaces read that verdict directly.
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
                Ok(SourceEvent::Strikes { source_id, strikes }) => {
                    // A strike is an observation like any other: it proves
                    // the feed is producing, so it keeps the source's
                    // liveness current. The strikes themselves are display
                    // data (the snapshot bridge holds the hour's ring) and
                    // are not sensor_history rows.
                    if let Some(newest) = strikes.iter().map(|s| s.time_epoch).max() {
                        last_seen.record(&source_id, newest);
                    }
                }
                // Which hardware is talking is not a reading, and carries
                // no time of its own; the observations that come with it
                // are what prove the source is alive.
                Ok(SourceEvent::Identity { .. } | SourceEvent::ForecastTrack { .. }) => {}
                Ok(SourceEvent::Reachability {
                    source_id,
                    reachable,
                }) => {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    source_reachable.report(&source_id, reachable, now);
                    debug!(source = %source_id, reachable, "source reachability changed");
                }
                Ok(SourceEvent::Diagnostic {
                    source_id,
                    failure,
                    at_epoch,
                }) => {
                    source_reachable.report_failure(&source_id, failure.map(|f| *f), at_epoch);
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

    #[tokio::test]
    async fn diagnostic_bus_records_changes_and_recovery_without_refreshing_weather() {
        use crate::ports::source_error::SourceFailure;
        let (bus, _rx) = broadcast::channel(16);
        let seen = SourceLastSeen::default();
        let reach = SourceReachability::default();
        spawn(bus.clone(), None, seen.clone(), reach.clone());
        bus.send(SourceEvent::Diagnostic {
            source_id: "ha".into(),
            failure: Some(Box::new(SourceFailure::http(
                500,
                Some("plain text"),
                "HA GET /api/states",
            ))),
            at_epoch: 100,
        })
        .unwrap();
        // An observation after the diagnostic proves the recorder processed
        // that diagnostic too, without relying on task scheduling sleeps.
        bus.send(SourceEvent::Observation {
            source_id: "barrier".into(),
            fields: vec![],
            at_epoch: 1,
        })
        .unwrap();
        while seen.get("barrier") != Some(1) {
            tokio::task::yield_now().await;
        }
        let record = reach.failure("ha").unwrap();
        assert_eq!(record.failure.http_status, Some(500));
        assert_eq!(record.at_epoch, 100);
        assert_eq!(seen.get("ha"), None);
        assert_eq!(reach.get("ha"), None);
        reach.report_failure("ha", Some(SourceFailure::http(401, None, "poll")), 101);
        assert_eq!(reach.failure("ha").unwrap().failure.http_status, Some(401));
        bus.send(SourceEvent::Diagnostic {
            source_id: "ha".into(),
            failure: None,
            at_epoch: 102,
        })
        .unwrap();
        bus.send(SourceEvent::Observation {
            source_id: "barrier".into(),
            fields: vec![],
            at_epoch: 2,
        })
        .unwrap();
        while seen.get("barrier") != Some(2) {
            tokio::task::yield_now().await;
        }
        assert!(reach.failure("ha").is_none());
        reach.report_failure("ha", Some(SourceFailure::http(500, None, "poll")), 100);
        assert!(
            reach.failure("ha").is_none(),
            "old events cannot revive a cleared failure"
        );
    }

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

    #[test]
    fn reachability_preserves_both_edges_without_rewriting_last_success() {
        let r = SourceReachability::default();
        r.report("station", true, 100);
        r.report("station", false, 110);
        assert_eq!(r.get("station"), Some(100));
        assert_eq!(r.reachable("station"), Some(false));
        r.report("station", true, 105);
        assert_eq!(
            r.reachable("station"),
            Some(false),
            "an older success cannot revive it"
        );
        r.report("station", true, 110);
        assert_eq!(
            r.reachable("station"),
            Some(true),
            "same-second bus ordering is retained"
        );
        assert_eq!(r.get("station"), Some(110));
        assert_eq!(r.reachable("never"), None);
    }

    #[tokio::test]
    async fn recorder_records_both_reachability_verdicts() {
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
        assert_eq!(reach.reachable("mrms"), Some(true));
        tx.send(SourceEvent::Reachability {
            source_id: "mrms".into(),
            reachable: false,
        })
        .unwrap();
        for _ in 0..50 {
            if reach.reachable("mrms") == Some(false) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(reach.reachable("mrms"), Some(false));
        assert_eq!(
            reach.get("mrms"),
            Some(stamped),
            "failure keeps the last success"
        );
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
