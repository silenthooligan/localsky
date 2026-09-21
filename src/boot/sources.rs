// Boot phase 6: the weather sources.
//
// One bus. The receiver adapters (Ecowitt local POST, HTTP webhook)
// publish on it from /ingest; every polling adapter's run loop is
// spawned against it under a restarting supervisor; the recorder turns
// its observations into sensor_history rows and the two liveness maps
// /api/health reads; the snapshot bridge and the forecast bridge
// arbitrate it into the live stores. Source add/remove takes a restart,
// the same contract every deploy-time input has.

use std::collections::HashMap;
use std::sync::Arc;

use crate::persistence::SensorHistoryStore;
use crate::ports::weather_source::{SourceBus, WeatherSource};
use crate::sources::{
    ecowitt_local::EcowittLocal, http_webhook::HttpWebhook, SourceLastSeen, SourceReachability,
};

use super::config::BootConfig;
use super::storage::Storage;
use super::stores::Stores;

/// What the sources phase yields.
pub struct Sources {
    pub bus: SourceBus,
    pub ecowitt: Vec<Arc<EcowittLocal>>,
    pub webhooks: Vec<Arc<HttpWebhook>>,
    /// The sensor history handle /api/health and /ingest write and read;
    /// None when the database could not be opened.
    pub sensor_history: Option<SensorHistoryStore>,
    /// Per-source last observation, this boot only.
    pub last_seen: SourceLastSeen,
    /// Per-source last successful fetch, the reachability twin of
    /// `last_seen`: a reachable-but-quiet rain authority reads
    /// `watching`, never `offline`.
    pub reachable: SourceReachability,
    /// Held for the process lifetime: dropping it closes the channel and
    /// every source's select loop would spin on the closed receiver.
    _shutdown: tokio::sync::watch::Sender<bool>,
}

pub async fn start(storage: &Storage, config: &BootConfig, stores: &Stores) -> Sources {
    let (bus, _rx) = tokio::sync::broadcast::channel(256);
    let (ecowitt, webhooks) = match config.cfg.as_ref() {
        Some(cfg) => crate::runtime::build_receiver_sources(cfg, bus.clone()),
        None => {
            tracing::info!(
                config = %config.path,
                "no localsky.toml yet; receiver sources idle, /ingest/* returns 503 until the wizard writes one"
            );
            (Vec::new(), Vec::new())
        }
    };
    tracing::info!(
        config = %config.path,
        ecowitt_receivers = ecowitt.len(),
        webhook_receivers = webhooks.len(),
        "settings/wizard/health/ingest routes mounted"
    );

    let sensor_history = open_sensor_history(storage, config);
    let last_seen = SourceLastSeen::default();
    let reachable = SourceReachability::default();
    crate::sources::bus_recorder::spawn(
        bus.clone(),
        sensor_history.clone(),
        last_seen.clone(),
        reachable.clone(),
    );
    // Subscribe the bridges BEFORE any source spawns, so the first emit
    // is not dropped (broadcast delivers only to live receivers, but
    // buffers from the moment of subscribe()).
    let forecast_rx = bus.subscribe();
    if let Some(conn) = storage.history_conn.clone() {
        crate::forecast::archive::spawn(
            stores.forecast.clone(),
            crate::persistence::forecast_archive::ForecastArchiveStore::new(conn),
        );
    }
    let snapshot_rx = bus.subscribe();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    stores
        .forecast
        .tracks
        .configure(&config.cfg.clone().unwrap_or_default());

    if !storage.demo_mode {
        if let Some(cfg) = config.cfg.as_ref() {
            let polling = crate::runtime::build_sources(cfg, Some(config.store.clone()));
            // Which bus-publishing sources may claim station liveness: a
            // real live station yes, a forecast source (display only) no.
            let mut live_current: HashMap<String, bool> = HashMap::new();
            for s in &polling {
                live_current.insert(s.id().to_string(), s.capabilities().live_current);
            }
            for s in &ecowitt {
                let d: Arc<dyn WeatherSource> = s.clone();
                live_current.insert(d.id().to_string(), d.capabilities().live_current);
            }
            for s in &webhooks {
                let d: Arc<dyn WeatherSource> = s.clone();
                live_current.insert(d.id().to_string(), d.capabilities().live_current);
            }
            publish_forecast_priority(cfg, config);
            crate::sources::snapshot_bridge::spawn(
                snapshot_rx,
                stores.tempest.clone(),
                live_current,
            );
            if !polling.is_empty() {
                tracing::info!(count = polling.len(), "spawning configured weather sources");
            }
            for source in polling {
                supervise(source, bus.clone(), shutdown_rx.clone());
            }
        }
        crate::sources::forecast_bridge::spawn(
            forecast_rx,
            stores.forecast.clone(),
            // The SWAPPABLE handle: a hot-reloaded provider or ranking
            // change re-arbitrates without a restart.
            config.forecast_priority.clone(),
        );
        spawn_open_meteo(config, &bus);
        crate::forecast::tracks::spawn(
            bus.clone(),
            stores.forecast.tracks.clone(),
            config.store.clone(),
        );
    }

    Sources {
        bus,
        ecowitt,
        webhooks,
        sensor_history,
        last_seen,
        reachable,
        _shutdown: shutdown_tx,
    }
}

/// A second handle to the history file for the receiver and health
/// paths, with the primary's pragmas: it used to open raw, took no busy
/// timeout and dropped reads on contention. Collapsing it onto the one
/// connection is a follow-up.
fn open_sensor_history(storage: &Storage, config: &BootConfig) -> Option<SensorHistoryStore> {
    match rusqlite::Connection::open(&storage.history_path) {
        Ok(c) => {
            c.busy_timeout(std::time::Duration::from_secs(5)).ok();
            c.pragma_update(None, "journal_mode", "WAL").ok();
            c.pragma_update(None, "synchronous", "NORMAL").ok();
            Some(
                SensorHistoryStore::new(Arc::new(tokio::sync::Mutex::new(c))).with_retention_days(
                    config
                        .cfg
                        .as_ref()
                        .map(|c| c.persistence.retention_days)
                        .unwrap_or_else(crate::config::schema::default_retention_days),
                ),
            )
        }
        Err(e) => {
            tracing::warn!(
                history = %storage.history_path,
                error = %e,
                "could not open sensor history for /api/health; freshness unavailable"
            );
            None
        }
    }
}

/// Forecast priority is USER-controlled: each enabled forecast source's
/// `priority` decides which drives the forecast (higher wins; ties keep
/// the incumbent), and the forecast_provider pin, if set, is bumped to
/// the winning priority. An implicit Open-Meteo has no entry, so it
/// defaults to 0 in the bridge: lowest, the failover.
fn publish_forecast_priority(cfg: &crate::config::schema::Config, config: &BootConfig) {
    let forecast_priority = crate::runtime::forecast_priority_map(cfg);
    if let Some(pinned) = cfg.forecast_provider.as_deref() {
        if forecast_priority.contains_key(pinned) {
            tracing::info!(
                provider = %pinned,
                priority = forecast_priority.get(pinned).copied().unwrap_or_default(),
                "forecast_provider pin: forcing chosen forecast source to win"
            );
        } else {
            tracing::warn!(
                provider = %pinned,
                "forecast_provider names no enabled forecast source; ignoring pin"
            );
        }
    }
    config.forecast_priority.store(Arc::new(forecast_priority));
}

/// A source's run() returning Err or PANICKING (a parser bug on a
/// malformed provider payload) must not silently drop that source until
/// a restart, reading "offline" in /api/health for days. Restart it with
/// capped exponential backoff; a run that stayed healthy for a while
/// resets the backoff so a flaky provider recovers quickly after a good
/// stretch. Shutdown ends the loop.
fn supervise(
    source: Arc<dyn WeatherSource>,
    bus: SourceBus,
    mut shut: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        use futures::FutureExt;
        let id = source.id().to_string();
        let base = std::time::Duration::from_secs(2);
        let max = std::time::Duration::from_secs(300);
        let mut backoff = base;
        loop {
            if *shut.borrow() {
                break;
            }
            let started = tokio::time::Instant::now();
            let outcome =
                std::panic::AssertUnwindSafe(source.clone().run(bus.clone(), shut.clone()))
                    .catch_unwind()
                    .await;
            // A stopped or failed adapter cannot keep its last successful
            // reachability verdict while the supervisor waits to restart it.
            let _ = bus.send(crate::ports::weather_source::SourceEvent::Reachability {
                source_id: id.clone(),
                reachable: false,
            });
            match outcome {
                // Clean return: the source stopped itself (shutdown).
                Ok(Ok(())) => break,
                Ok(Err(e)) => {
                    let failure = crate::diagnostics::from_anyhow(&e, "weather source worker");
                    tracing::warn!(source = %id, %failure, "weather source task errored; restarting after backoff");
                    crate::sources::poll::report_diagnostic(&bus, &id, Some(failure));
                }
                Err(_) => {
                    let failure = crate::failure::Failure::new(
                        crate::failure::FailureCode::TaskPanic,
                        "weather source worker",
                    );
                    tracing::error!(source = %id, %failure, "weather source task panicked; restarting after backoff");
                    crate::sources::poll::report_diagnostic(&bus, &id, Some(failure));
                }
            }
            if started.elapsed() >= std::time::Duration::from_secs(120) {
                backoff = base;
            }
            tokio::select! {
                _ = tokio::time::sleep(backoff) => {}
                _ = shut.changed() => {}
            }
            backoff = (backoff * 2).min(max);
        }
    });
}

/// Open-Meteo stays the no-auth default and lowest-priority failover.
/// Opt out with an explicit disabled OpenMeteo source entry; otherwise
/// it runs even pre-config, re-reading coordinates from the live store.
fn spawn_open_meteo(config: &BootConfig, bus: &SourceBus) {
    use crate::config::schema::SourceKind;
    let om = config.cfg.as_ref().and_then(|c| {
        c.sources
            .iter()
            .find(|s| matches!(s.source, SourceKind::OpenMeteo(_)))
    });
    if om.map(|s| s.enabled).unwrap_or(true) {
        let om_id = om
            .map(|s| s.id.clone())
            .unwrap_or_else(|| "open_meteo".to_string());
        crate::forecast::spawn_forecast_refresher(
            bus.clone(),
            om_id,
            config
                .cfg
                .as_ref()
                .map(|c| (c.deployment.location.lat, c.deployment.location.lon)),
            Some(config.store.clone()),
        );
    } else {
        tracing::info!("Open-Meteo forecast disabled by config; relying on other forecast sources");
    }
}
