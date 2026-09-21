//! Declared model forecasts. Producers publish on the source bus; this bridge
//! stores only track events. Neither current observations nor irrigation
//! arbitration can consume a track result.
use super::snapshot::ForecastSnapshot;
use crate::config::{Config, FileConfigStore};
use crate::ports::config_store::ConfigStore;
use crate::ports::weather_source::{SourceBus, SourceEvent};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct TrackSpec {
    id: String,
    model: String,
    lat: f64,
    lon: f64,
    endpoint: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    spec: TrackSpec,
    snapshot: Option<ForecastSnapshot>,
    #[serde(skip)]
    generation: u64,
    #[serde(skip)]
    last_error: Option<String>,
    #[serde(skip)]
    diagnostic: Option<crate::failure::FailureRecord>,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

pub use super::window::TrackStatus;

pub struct TrackStore {
    entries: RwLock<BTreeMap<String, Entry>>,
    generation: AtomicU64,
    path: Option<PathBuf>,
    changed: watch::Sender<u64>,
}

impl TrackStore {
    pub fn new(path: Option<PathBuf>) -> Self {
        let mut entries: BTreeMap<String, Entry> = path
            .as_ref()
            .and_then(|p| {
                std::fs::read(p)
                    .inspect_err(|error| {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            let failure =
                                crate::diagnostics::from_error(error, "forecast track cache load");
                            tracing::warn!(%failure, "forecast track cache could not be read");
                        }
                    })
                    .ok()
            })
            .and_then(|bytes| {
                serde_json::from_slice::<Cache>(&bytes)
                    .inspect_err(|error| {
                        let failure =
                            crate::diagnostics::from_error(error, "forecast track cache decode");
                        tracing::warn!(%failure, "forecast track cache could not be decoded");
                    })
                    .ok()
            })
            .filter(|cache| cache.version == 1)
            .map(|cache| cache.entries)
            .unwrap_or_default();
        for entry in entries.values_mut() {
            if let Some(snapshot) = &mut entry.snapshot {
                snapshot.source_reachable = false;
            }
        }
        let (changed, _) = watch::channel(0);
        Self {
            entries: RwLock::new(entries),
            generation: AtomicU64::new(1),
            path,
            changed,
        }
    }

    /// Called before the API is mounted, then on each config poll. Identity
    /// includes the location so a moved installation cannot serve old local data.
    pub fn configure(&self, config: &Config) {
        let location = &config.deployment.location;
        let located = (location.lat != 0.0 || location.lon != 0.0)
            && (-90.0..=90.0).contains(&location.lat)
            && (-180.0..=180.0).contains(&location.lon);
        let specs: BTreeMap<_, _> = config
            .forecast_tracks
            .iter()
            .map(|track| {
                (
                    track.id.clone(),
                    TrackSpec {
                        id: track.id.clone(),
                        model: track.model.clone(),
                        lat: location.lat,
                        lon: location.lon,
                        endpoint: super::open_meteo::configured_open_meteo_endpoint(config),
                    },
                )
            })
            .collect();
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        let mut changed = false;
        entries.retain(|id, _| {
            let keep = specs.contains_key(id);
            changed |= !keep;
            keep
        });
        for (id, spec) in specs {
            if entries.get(&id).is_none_or(|entry| entry.spec != spec) {
                entries.insert(
                    id.clone(),
                    Entry {
                        spec,
                        snapshot: None,
                        generation: 0,
                        last_error: None,
                        diagnostic: None,
                    },
                );
                changed = true;
            }
            let entry = entries.get_mut(&id).unwrap();
            if entry.generation == 0 {
                entry.generation = self.generation.fetch_add(1, Ordering::Relaxed);
                if !located {
                    entry.last_error =
                        Some("Set the installation location to fetch this model".into());
                    entry.diagnostic = Some(crate::failure::FailureRecord::now(
                        crate::failure::Failure::new(
                            crate::failure::FailureCode::ConfigField,
                            "forecast track location",
                        )
                        .with_field("deployment.location"),
                    ));
                }
                changed = true;
            }
        }
        if changed {
            self.persist(&entries);
            self.notify();
        }
    }

    fn notify(&self) {
        self.changed
            .send_modify(|version| *version = version.wrapping_add(1));
    }

    fn persist(&self, entries: &BTreeMap<String, Entry>) {
        let Some(path) = &self.path else {
            return;
        };
        let result = serde_json::to_vec(&Cache {
            version: 1,
            entries: entries.clone(),
        })
        .map_err(std::io::Error::other)
        .and_then(|bytes| crate::config::store::write_atomic_durable(path, &bytes));
        if let Err(error) = result {
            let failure = crate::diagnostics::from_error(&error, "forecast track cache save");
            tracing::warn!(%failure, "forecast track cache could not be saved");
        }
    }

    fn accept(
        &self,
        id: &str,
        generation: u64,
        result: Result<ForecastSnapshot, Box<crate::failure::Failure>>,
    ) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = entries.get_mut(id).filter(|e| e.generation == generation) else {
            return;
        };
        match result {
            Ok(snapshot) if !snapshot.hourly.is_empty() => {
                entry.snapshot = Some(snapshot);
                entry.last_error = None;
                entry.diagnostic = None;
                self.persist(&entries);
            }
            result => {
                let failure = result.err().map(|failure| *failure).unwrap_or_else(|| {
                    crate::failure::Failure::new(
                        crate::failure::FailureCode::MissingField,
                        "forecast track response",
                    )
                    .with_field("hourly")
                });
                entry.last_error = Some(format!("{}: {}", failure.code.as_str(), failure.message));
                entry.diagnostic = Some(crate::failure::FailureRecord::now(failure));
                if let Some(snapshot) = &mut entry.snapshot {
                    snapshot.source_reachable = false;
                }
            }
        }
        self.notify();
    }

    pub fn snapshot(&self, id: &str) -> Option<(String, ForecastSnapshot)> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .map(|e| (e.spec.model.clone(), e.snapshot.clone().unwrap_or_default()))
    }

    pub fn snapshots(&self) -> Vec<(String, String, ForecastSnapshot)> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|(id, e)| {
                e.snapshot
                    .clone()
                    .map(|s| (id.clone(), e.spec.model.clone(), s))
            })
            .collect()
    }

    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn status(&self, now: i64) -> Vec<TrackStatus> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(id, entry)| {
                let snapshot = entry.snapshot.as_ref();
                let fetched_at = snapshot.map(|s| s.last_refresh_epoch).filter(|at| *at > 0);
                let age_s = fetched_at.map(|at| now.saturating_sub(at).max(0));
                let label = snapshot.map(|s| s.source_label.as_str()).unwrap_or("");
                let tier = if label.contains("self-hosted") {
                    "self-hosted"
                } else if label.contains("mirror") {
                    "mirror"
                } else if snapshot.is_some() {
                    "primary"
                } else {
                    "unavailable"
                };
                TrackStatus {
                    id: id.clone(),
                    model: entry.spec.model.clone(),
                    provider_label: label.into(),
                    fetched_at,
                    age_s,
                    tier: tier.into(),
                    degraded: age_s.is_none_or(|age| {
                        age > crate::sources::forecast_bridge::FORECAST_OWNER_STALE_SECS
                    }) || snapshot.is_none_or(|s| !s.source_reachable),
                    last_error: entry.last_error.clone(),
                    diagnostic: entry
                        .diagnostic
                        .as_ref()
                        .and_then(|record| serde_json::to_value(record).ok()),
                }
            })
            .collect()
    }

    fn requests(&self) -> Vec<(TrackSpec, u64)> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|e| e.spec.lat != 0.0 || e.spec.lon != 0.0)
            .map(|e| (e.spec.clone(), e.generation))
            .collect()
    }
}

pub fn spawn(bus: SourceBus, store: Arc<TrackStore>, config: Arc<FileConfigStore>) {
    let mut receiver = bus.subscribe();
    let bridge = store.clone();
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(SourceEvent::ForecastTrack {
                    track_id,
                    generation,
                    result,
                }) => bridge.accept(&track_id, generation, result),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(count)) => {
                    tracing::warn!(count, "forecast track bus lagged")
                }
                Ok(_) => {}
            }
        }
    });
    tokio::spawn(async move {
        let mut workers: BTreeMap<String, (u64, tokio::task::JoinHandle<()>)> = BTreeMap::new();
        loop {
            if let Ok(config) = config.load().await {
                store.configure(&config);
            }
            let requests = store.requests();
            workers.retain(|id, (generation, task)| {
                let keep = requests
                    .iter()
                    .any(|(spec, current)| &spec.id == id && current == generation)
                    && !task.is_finished();
                if !keep {
                    task.abort();
                }
                keep
            });
            for (index, (spec, generation)) in requests.into_iter().enumerate() {
                workers.entry(spec.id.clone()).or_insert_with(|| {
                    (
                        generation,
                        spawn_worker(bus.clone(), spec, generation, index),
                    )
                });
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}

fn spawn_worker(
    bus: SourceBus,
    spec: TrackSpec,
    generation: u64,
    index: usize,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5 + index as u64 * 5)).await;
        let client = crate::net::client_opts(
            Duration::from_secs(10),
            Some(Duration::from_secs(4)),
            "localsky/forecast-track",
        );
        let mut failures = 0_u32;
        loop {
            let result = super::open_meteo::refresh_once(
                &client,
                spec.lat,
                spec.lon,
                &spec.model,
                spec.endpoint.as_deref(),
                1,
            )
            .await
            .map(|(snapshot, _current)| snapshot)
            .map_err(|e| {
                Box::new(crate::diagnostics::from_anyhow(
                    &e,
                    "forecast track refresh",
                ))
            });
            let wait = if result.is_ok() {
                failures = 0;
                super::open_meteo::REFRESH_INTERVAL
            } else {
                failures = failures.saturating_add(1);
                super::open_meteo::backoff(failures)
            };
            if bus
                .send(SourceEvent::ForecastTrack {
                    track_id: spec.id.clone(),
                    generation,
                    result,
                })
                .is_err()
            {
                break;
            }
            tokio::time::sleep(wait).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> Config {
        let mut config = Config::default();
        config.deployment.location.lat = 39.0;
        config.deployment.location.lon = -104.0;
        config.forecast_tracks.push(crate::config::ForecastTrack {
            id: "nbm".into(),
            model: "ncep_nbm_conus".into(),
        });
        config
    }
    fn snapshot() -> ForecastSnapshot {
        ForecastSnapshot {
            last_refresh_epoch: 1000,
            source_reachable: true,
            hourly: vec![super::super::snapshot::HourlyEntry {
                time_epoch: 3600,
                precip_in: Some(0.0),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    #[test]
    fn original_age_survives_failure_restart_and_removal_is_durable() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-track-test-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tracks.json");
        let store = TrackStore::new(Some(path.clone()));
        store.configure(&config());
        let generation = store.requests()[0].1;
        store.accept("nbm", generation, Ok(snapshot()));
        store.accept(
            "nbm",
            generation,
            Err(Box::new(crate::failure::Failure::http(
                503,
                None,
                "forecast track fetch",
            ))),
        );
        assert_eq!(store.status(7000)[0].age_s, Some(6000));
        assert!(!store.status(7000)[0]
            .last_error
            .as_ref()
            .unwrap()
            .contains("private"));
        let restored = TrackStore::new(Some(path.clone()));
        restored.configure(&config());
        assert_eq!(restored.status(8000)[0].fetched_at, Some(1000));
        assert!(restored.status(8000)[0].degraded);
        restored.configure(&Config::default());
        assert!(TrackStore::new(Some(path)).status(8000).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn late_result_cannot_resurrect_removed_or_relocated_track() {
        let store = TrackStore::new(None);
        let mut config = config();
        store.configure(&config);
        let old = store.requests()[0].1;
        config.deployment.location.lat = 40.0;
        store.configure(&config);
        store.accept("nbm", old, Ok(snapshot()));
        assert_eq!(store.status(8000)[0].fetched_at, None);
        store.configure(&Config::default());
        store.accept("nbm", old, Ok(snapshot()));
        assert!(store.snapshot("nbm").is_none());
    }
}
