// Arc-swap store + watch channel. Identical shape to TempestStore /
// IrrigationStore so the SSE wiring is uniform.

use crate::forecast::snapshot::ForecastSnapshot;
use arc_swap::ArcSwap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

// A legacy bare snapshot cannot distinguish a reported 0°F from the old
// provider parser's missing-value placeholder (or WeatherKit's 32°F), nor
// actual calm/dry air from absent wind/humidity. Schema 2 still fabricated
// zero precipitation when QPF was missing, so it is also rejected. Snapshots
// written after the complete nullable critical-weather contract may rehydrate.
const CACHE_SCHEMA_VERSION: u32 = 3;

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedForecast {
    schema_version: u32,
    snapshot: ForecastSnapshot,
}

pub struct ForecastStore {
    pub tracks: Arc<super::tracks::TrackStore>,
    current: ArcSwap<ForecastSnapshot>,
    tx: watch::Sender<Arc<ForecastSnapshot>>,
    rx: watch::Receiver<Arc<ForecastSnapshot>>,
    /// When set, every successful `store()` also writes the snapshot as JSON
    /// to this path (tmp + rename), and boot rehydrates from it. The forecast
    /// otherwise lives only in memory, so a container restart during a
    /// provider outage came up EMPTY: no 48h/7-day weather, no verdict strip,
    /// nothing to decide from, until the provider answered again. The
    /// persisted snapshot carries its own `last_refresh_epoch`, so on
    /// rehydrate the existing staleness guards (forecast_is_stale, health
    /// windows) treat it honestly as an AGED forecast: the UI has data to
    /// show and the engine still refuses to trust it past its window.
    persist_path: Option<PathBuf>,
}

impl ForecastStore {
    pub fn new() -> Self {
        let initial = Arc::new(ForecastSnapshot::default());
        let (tx, rx) = watch::channel(initial.clone());
        Self {
            tracks: Arc::new(super::tracks::TrackStore::new(None)),
            current: ArcSwap::from(initial),
            tx,
            rx,
            persist_path: None,
        }
    }

    /// Enable last-good persistence at `path` and, when a previously
    /// persisted snapshot exists there with actual content, rehydrate it
    /// immediately (publishing to subscribers) so the UI and engine start
    /// from the last known forecast instead of empty.
    pub fn with_persistence(mut self, path: PathBuf) -> Self {
        self.tracks = Arc::new(super::tracks::TrackStore::new(Some(
            path.with_file_name("forecast-tracks.json"),
        )));
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<PersistedForecast>(&bytes) {
                Ok(cache) if cache.schema_version != CACHE_SCHEMA_VERSION => {
                    tracing::warn!(path = %path.display(), version = cache.schema_version,
                        "forecast cache schema unsupported; awaiting fresh provider evidence");
                }
                Ok(cache)
                    if !cache.snapshot.daily.is_empty() || !cache.snapshot.hourly.is_empty() =>
                {
                    let mut snap = cache.snapshot;
                    // Reachability describes the LIVE connection, and this
                    // process hasn't heard from the provider yet. The first
                    // successful fetch flips it back via the normal path.
                    snap.source_reachable = false;
                    tracing::info!(
                        path = %path.display(),
                        fetched_epoch = snap.last_refresh_epoch,
                        days = snap.daily.len(),
                        "rehydrated last-good forecast from disk (staleness guards apply)"
                    );
                    let arc = Arc::new(snap);
                    self.current.store(arc.clone());
                    let _ = self.tx.send(arc);
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e,
                        "persisted forecast legacy or unreadable; awaiting fresh provider evidence");
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e,
                    "persisted forecast unreadable; starting empty");
            }
        }
        self.persist_path = Some(path);
        self
    }

    pub fn snapshot(&self) -> Arc<ForecastSnapshot> {
        self.current.load_full()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<ForecastSnapshot>> {
        self.rx.clone()
    }

    pub fn store(&self, snap: ForecastSnapshot) {
        let new = Arc::new(snap);
        // Never let an EMPTY snapshot overwrite a good live one. If a source ever
        // emits an empty-but-reachable forecast (a parse yielding empty period
        // arrays, say), publishing it would blank the dashboard + push empty to
        // every SSE subscriber for a cycle. Drop it entirely when we already hold
        // real content, in memory as well as on disk (the old guard protected
        // only the disk copy, AFTER the live store had already been blanked). An
        // empty snapshot is still allowed to publish at boot, when current is
        // itself empty, so the initial "no data yet" state renders.
        if new.daily.is_empty() && new.hourly.is_empty() {
            let cur = self.current.load();
            if !cur.daily.is_empty() || !cur.hourly.is_empty() {
                return;
            }
        }
        self.current.store(new.clone());
        let _ = self.tx.send(new.clone());
        // Best-effort last-good persistence (tmp + rename so a crash mid-write
        // never leaves a torn file). Only real content is persisted, so a
        // transient empty snapshot can never clobber the last good one.
        if let Some(path) = &self.persist_path {
            if new.daily.is_empty() && new.hourly.is_empty() {
                return;
            }
            let tmp = path.with_extension("json.tmp");
            let write = || -> std::io::Result<()> {
                let bytes = serde_json::to_vec(&PersistedForecast {
                    schema_version: CACHE_SCHEMA_VERSION,
                    snapshot: (*new).clone(),
                })
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                std::fs::write(&tmp, bytes)?;
                std::fs::rename(&tmp, path)
            };
            if let Err(e) = write() {
                tracing::debug!(path = %path.display(), error = %e,
                    "forecast persistence write failed (non-fatal)");
            }
        }
    }
}

impl Default for ForecastStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::snapshot::DailyEntry;

    fn snap_with_days(epoch: i64, days: usize) -> ForecastSnapshot {
        ForecastSnapshot {
            last_refresh_epoch: epoch,
            source_reachable: true,
            daily: (0..days).map(|_| DailyEntry::default()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn persist_and_rehydrate_keeps_original_epoch() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-fcache-test-{}-roundtrip",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forecast-cache.json");
        let _ = std::fs::remove_file(&path);

        let store = ForecastStore::new().with_persistence(path.clone());
        store.store(snap_with_days(1_750_000_000, 7));
        assert!(path.exists(), "store() should write the cache file");

        // Simulated restart: a fresh store rehydrates the snapshot with the
        // ORIGINAL fetch epoch (staleness honesty) and reachable forced off
        // (this process hasn't heard from the provider yet).
        let reborn = ForecastStore::new().with_persistence(path.clone());
        let s = reborn.snapshot();
        assert_eq!(s.last_refresh_epoch, 1_750_000_000);
        assert_eq!(s.daily.len(), 7);
        assert!(!s.source_reachable);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_snapshot_never_clobbers_last_good() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-fcache-test-{}-noclobber",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forecast-cache.json");
        let _ = std::fs::remove_file(&path);

        let store = ForecastStore::new().with_persistence(path.clone());
        store.store(snap_with_days(100, 3));
        store.store(ForecastSnapshot::default());

        let reborn = ForecastStore::new().with_persistence(path.clone());
        assert_eq!(reborn.snapshot().daily.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_temperature_placeholders_cannot_rehydrate_but_fresh_zero_can() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-fcache-test-{}-temperature-schema",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forecast-cache.json");
        let mut fresh = snap_with_days(1_750_000_000, 1);
        fresh.daily[0].temp_max_f = Some(0.0);
        fresh.daily[0].temp_min_f = Some(-5.0);
        fresh.daily[0].wind_max_mph = Some(0.0);
        fresh.daily[0].humidity_pct = Some(0);
        // These exact numbers on the old wire could mean a placeholder
        // high. Even a recent, otherwise valid bare cache cannot prove it.
        std::fs::write(&path, serde_json::to_vec(&fresh).unwrap()).unwrap();
        let store = ForecastStore::new().with_persistence(path.clone());
        assert!(store.snapshot().daily.is_empty());

        // New provider evidence records provenance through the cache
        // version. True zero survives the next process restart unchanged.
        store.store(fresh);
        let reborn = ForecastStore::new().with_persistence(path.clone());
        assert_eq!(reborn.snapshot().daily[0].temp_max_f, Some(0.0));
        assert_eq!(reborn.snapshot().daily[0].temp_min_f, Some(-5.0));
        assert_eq!(reborn.snapshot().daily[0].wind_max_mph, Some(0.0));
        assert_eq!(reborn.snapshot().daily[0].humidity_pct, Some(0));

        let mut cache: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for incompatible_version in [1, 2, CACHE_SCHEMA_VERSION + 1] {
            cache["schema_version"] = serde_json::json!(incompatible_version);
            std::fs::write(&path, serde_json::to_vec(&cache).unwrap()).unwrap();
            assert!(ForecastStore::new()
                .with_persistence(path.clone())
                .snapshot()
                .daily
                .is_empty());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_cache_starts_empty_not_dead() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-fcache-test-{}-corrupt",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forecast-cache.json");
        std::fs::write(&path, b"{ not json").unwrap();

        let store = ForecastStore::new().with_persistence(path.clone());
        assert!(store.snapshot().daily.is_empty());
        // And a good store() afterwards repairs the file.
        store.store(snap_with_days(200, 2));
        let reborn = ForecastStore::new().with_persistence(path);
        assert_eq!(reborn.snapshot().daily.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
