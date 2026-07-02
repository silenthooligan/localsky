// Runtime composition root. The single place where all the modules
// are assembled into a running system. main.rs calls Runtime::boot()
// unconditionally and receives a fully-wired Runtime that exposes the
// Axum router state plus a handle to the spawned background tasks.
//
// Boot sequence:
//   1. Load Config: try /data/localsky.toml first, fall back to
//      env_compat synthesis from legacy env vars.
//   2. Open SQLite, run migrations.
//   3. Build the SourceRegistry from cfg.sources and spawn each
//      adapter's run() task.
//   4. Build the ControllerRegistry from cfg.controllers (no spawn;
//      controllers respond on dispatch).
//   5. Build the LlmProvider via Auto/Ollama/OpenaiCompat.
//   6. Connect to MQTT if cfg.notifications.mqtt is set; publish
//      discovery for every configured zone + the verdict sensor.
//   7. Start the engine ticker (60s default).
//   8. Hand back to main.rs to mount the Axum router.
//
// Shutdown is cooperative via tokio::sync::watch<bool>; tasks observe
// the channel and drop within 5s.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use arc_swap::ArcSwap;
use rusqlite::Connection;
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::env_compat;
use crate::config::schema::{Config, ControllerKind, LlmProviderKind, SourceEntry, SourceKind};
use crate::config::FileConfigStore;
use crate::controllers::{
    Bhyve, ControllerRegistry, DryRunController, HaServiceCall, HttpGeneric, Hydrawise,
    MqttCommand, OpenSprinklerDirect, Rachio, Rainbird,
};
use crate::llm::providers::{
    auto_detect::{default_probe_targets, detect, ProbeKind, ProbeTarget},
    OllamaProvider, OpenaiCompatProvider,
};
use crate::persistence::{
    runner as migration_runner, ConfigSnapshotStore, RunsStore, SensorHistoryStore,
    VerdictHistoryStore,
};
use crate::ports::config_store::ConfigStore;
use crate::ports::irrigation_controller::IrrigationController;
use crate::ports::llm_provider::LlmProvider;
use crate::ports::weather_source::{SourceEvent, WeatherSource};
use crate::sources::{
    AmbientWeather, DavisWll, DemoReplay, EcowittLocal, HaPassthrough, HttpWebhook, Lacrosse,
    MetNorway, MqttSubscribe, Netatmo, Nws, OpenWeather, PirateWeather, SourceRegistry, Synoptic,
    TempestWs, TuyaCloud, Yolink,
};

pub struct Runtime {
    pub config: Arc<ArcSwap<Config>>,
    pub config_store: Arc<FileConfigStore>,
    pub config_snapshots: ConfigSnapshotStore,
    pub sources: SourceRegistry,
    pub controllers: ControllerRegistry,
    pub llm: Option<Arc<dyn LlmProvider>>,
    pub runs: RunsStore,
    pub sensor_history: SensorHistoryStore,
    pub verdict_history: VerdictHistoryStore,
    /// Broadcast bus for source observations. Engine + MQTT publisher
    /// subscribe.
    pub source_bus: broadcast::Sender<SourceEvent>,
    pub shutdown_tx: watch::Sender<bool>,
    /// Shared DB connection wrapped for spawn_blocking callers.
    pub db: Arc<Mutex<Connection>>,
}

impl Runtime {
    /// Compose every module from config + persistence. Returns a Runtime
    /// ready to spawn background tasks against. Does not block on
    /// network probes for the LLM (auto-detect runs in a spawn).
    pub async fn boot(config_path: PathBuf, db_path: PathBuf) -> anyhow::Result<Self> {
        // ----- Step 1: persistence -----
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut conn =
            Connection::open(&db_path).with_context(|| format!("open sqlite at {db_path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        let applied = migration_runner::run(&mut conn).with_context(|| "run migrations")?;
        if !applied.is_empty() {
            info!(applied = ?applied, "applied schema migrations");
        }
        let db: Arc<Mutex<Connection>> = Arc::new(Mutex::new(conn));

        // ----- Step 2: config -----
        let config_store = Arc::new(FileConfigStore::new(&config_path));
        let cfg = if config_store.is_initialized() {
            match config_store.load().await {
                Ok(c) => {
                    info!("loaded /data/localsky.toml");
                    c
                }
                Err(e) => {
                    warn!(error = %e, "failed to load config; falling back to env_compat");
                    env_compat::synthesize()
                }
            }
        } else {
            info!("no config file; synthesizing from environment");
            env_compat::synthesize()
        };
        let config = Arc::new(ArcSwap::from_pointee(cfg));

        // ----- Step 3: stores -----
        let runs = RunsStore::new(db.clone());
        let sensor_history = SensorHistoryStore::new(db.clone());
        let verdict_history = VerdictHistoryStore::new(db.clone());
        let config_snapshots = ConfigSnapshotStore::new(db.clone());

        // ----- Step 3a: reconcile interrupted runs -----
        // Any row still marked 'running' or 'intended' represents an
        // irrigation that was in flight when the process previously
        // exited (kill -9, deploy, OOM, host reboot). Mark them aborted
        // before anything else reads the runs table, otherwise the
        // dashboard renders zombie active runs and history shows runs
        // that never ended.
        let now_epoch = chrono::Utc::now().timestamp();
        match runs.reconcile_in_flight(now_epoch).await {
            Ok(0) => {}
            Ok(n) => info!(reconciled = n, "marked interrupted runs as aborted"),
            Err(e) => warn!(error = %e, "in-flight run reconciliation failed"),
        }

        // ----- Step 4: registries -----
        let sources = SourceRegistry::new();
        sources.set(build_sources(&config.load()));
        let controllers = ControllerRegistry::new();
        controllers.set(build_controllers(&config.load(), runs.clone()));

        // ----- Step 5: LLM provider -----
        let llm = build_llm(&config.load()).await;

        // ----- Step 6: source bus + shutdown -----
        let (source_bus, _rx0) = broadcast::channel::<SourceEvent>(256);
        let (shutdown_tx, _) = watch::channel(false);

        Ok(Self {
            config,
            config_store,
            config_snapshots,
            sources,
            controllers,
            llm,
            runs,
            sensor_history,
            verdict_history,
            source_bus,
            shutdown_tx,
            db,
        })
    }

    /// Spawn every background task: each weather source's run() loop,
    /// the engine tick (60s default), and the MQTT publish loop when
    /// configured. Returns the JoinHandles so main can await them on
    /// shutdown.
    pub fn spawn_background_tasks(&self) -> Vec<JoinHandle<()>> {
        let mut handles = Vec::new();
        let shutdown_rx = self.shutdown_tx.subscribe();

        // One task per WeatherSource.
        for source in self.sources.all() {
            let bus = self.source_bus.clone();
            let shut = shutdown_rx.clone();
            handles.push(tokio::spawn(async move {
                let id = source.id().to_string();
                if let Err(e) = source.run(bus, shut).await {
                    warn!(source = %id, error = %e, "source task exited with error");
                }
            }));
        }

        // Engine tick. Every 60s recompute the merged snapshot ->
        // engine verdict + budgets + soil. Persist verdict_history at
        // sunset; deferred for now.
        let _ = shutdown_rx.clone();
        // Full engine tick wiring follows when the snapshot adapter
        // bridges MergedSnapshot -> IrrigationSnapshot for the UI.
        // Until then the dashboard still consumes the v0.1 refresher
        // output.

        handles
    }

    /// Cooperative shutdown. Drops the watch sender so every spawned
    /// task with a receiver observes the close. Caller awaits the join
    /// handles afterward.
    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
    }
}

/// Construct the HTTP-receiver-style sources (Ecowitt local + HTTP
/// webhook). main.rs mounts Axum routes against the returned adapters
/// because their POST handlers emit observations on each request
/// rather than through a poll loop.
pub fn build_receiver_sources(
    cfg: &Config,
    bus: tokio::sync::broadcast::Sender<SourceEvent>,
) -> (Vec<Arc<EcowittLocal>>, Vec<Arc<HttpWebhook>>) {
    let mut eco = Vec::new();
    let mut webhook = Vec::new();
    for entry in &cfg.sources {
        if !entry.enabled {
            continue;
        }
        match &entry.source {
            SourceKind::EcowittLocal(c) => {
                eco.push(Arc::new(EcowittLocal::new(
                    entry.id.clone(),
                    c.clone(),
                    bus.clone(),
                )));
            }
            SourceKind::HttpWebhook(c) => {
                webhook.push(Arc::new(HttpWebhook::new(
                    entry.id.clone(),
                    c.clone(),
                    bus.clone(),
                )));
            }
            _ => {}
        }
    }
    (eco, webhook)
}

/// Construct every poll/listen-style WeatherSource adapter from config.
/// main.rs spawns each returned adapter's run() loop against the shared
/// source bus at boot (receiver-POST kinds come from
/// build_receiver_sources instead; legacy v0.1 paths are skipped below).
pub fn build_sources(cfg: &Config) -> Vec<Arc<dyn WeatherSource>> {
    let mut out: Vec<Arc<dyn WeatherSource>> = Vec::new();
    for entry in &cfg.sources {
        if !entry.enabled {
            continue;
        }
        let constructed: Option<Arc<dyn WeatherSource>> = match &entry.source {
            SourceKind::DemoReplay(c) => {
                Some(Arc::new(DemoReplay::new(entry.id.clone(), c.clone())))
            }
            SourceKind::Mqtt(c) => Some(Arc::new(MqttSubscribe::new(entry.id.clone(), c.clone()))),
            SourceKind::Nws(c) => Some(Arc::new(Nws::new(
                entry.id.clone(),
                c.clone(),
                cfg.deployment.location.clone(),
            ))),
            SourceKind::MetNorway(c) => Some(Arc::new(MetNorway::new(
                entry.id.clone(),
                c.clone(),
                cfg.deployment.location.clone(),
            ))),
            SourceKind::NoaaMrms(c) => Some(Arc::new(crate::sources::noaa_mrms::NoaaMrms::new(
                entry.id.clone(),
                c.clone(),
                cfg.deployment.location.clone(),
            ))),
            SourceKind::OpenWeather(c) => Some(Arc::new(OpenWeather::new(
                entry.id.clone(),
                c.clone(),
                cfg.deployment.location.clone(),
            ))),
            SourceKind::PirateWeather(c) => Some(Arc::new(PirateWeather::new(
                entry.id.clone(),
                c.clone(),
                cfg.deployment.location.clone(),
            ))),
            SourceKind::Synoptic(c) => Some(Arc::new(Synoptic::new(
                entry.id.clone(),
                c.clone(),
                cfg.deployment.location.clone(),
            ))),
            SourceKind::AmbientWeather(c) => {
                Some(Arc::new(AmbientWeather::new(entry.id.clone(), c.clone())))
            }
            SourceKind::Netatmo(c) => Some(Arc::new(Netatmo::new(entry.id.clone(), c.clone()))),
            SourceKind::DavisWll(c) => Some(Arc::new(DavisWll::new(entry.id.clone(), c.clone()))),
            SourceKind::TempestWs(c) => Some(Arc::new(TempestWs::new(entry.id.clone(), c.clone()))),
            SourceKind::Yolink(c) => Some(Arc::new(Yolink::new(entry.id.clone(), c.clone()))),
            SourceKind::Lacrosse(c) => Some(Arc::new(Lacrosse::new(entry.id.clone(), c.clone()))),
            SourceKind::TuyaCloud(c) => Some(Arc::new(TuyaCloud::new(entry.id.clone(), c.clone()))),
            SourceKind::HaPassthrough(c) => {
                Some(Arc::new(HaPassthrough::new(entry.id.clone(), c.clone())))
            }
            SourceKind::RestPoll(c) => Some(Arc::new(crate::sources::rest_poll::RestPoll::new(
                entry.id.clone(),
                c.clone(),
            ))),
            SourceKind::Prometheus(c) => Some(Arc::new(
                crate::sources::prometheus::Prometheus::new(entry.id.clone(), c.clone()),
            )),
            SourceKind::InfluxDb(c) => Some(Arc::new(crate::sources::influxdb::InfluxDb::new(
                entry.id.clone(),
                c.clone(),
            ))),
            SourceKind::WeatherKit(c) => {
                Some(Arc::new(crate::sources::weatherkit::WeatherKit::new(
                    entry.id.clone(),
                    c.clone(),
                    cfg.deployment.location.clone(),
                    cfg.deployment.timezone.clone(),
                )))
            }
            // EcowittLocal + HttpWebhook are constructed in
            // build_receiver_sources() because their POST handlers
            // emit observations on each incoming request rather than
            // through the run-loop pattern. main.rs mounts the Axum
            // routes against those instances.
            SourceKind::EcowittLocal(_) | SourceKind::HttpWebhook(_) => None,
            // EcowittGwPoll is a standalone sensor_history poller (not a
            // WeatherSource); main.rs spawns it directly. Skip here.
            SourceKind::EcowittGwPoll(_) => None,
            // Blitzortung is a standalone display-only feed into the
            // TempestStore lightning buffer (never the merge bus, never
            // irrigation input); main.rs spawns it directly. Skip here.
            SourceKind::Blitzortung(_) => None,
            // Already-wired v0.1 paths: TempestUdp + OpenMeteo run as
            // their own tasks in main.rs; they are not yet expressed
            // via WeatherSource. Silent skip is correct.
            SourceKind::TempestUdp(_) | SourceKind::OpenMeteo(_) => None,
            // All schema source kinds now have implementations.
        };
        if let Some(s) = constructed {
            out.push(s);
        }
    }
    out
}

/// Canonical WeatherField names a single configured source PROVIDES, for the
/// "available sources per field" picker in the Data sources settings page.
///
/// For the kinds `build_sources` constructs, this reuses the adapter's real
/// `capabilities().fields` so the candidate set is exactly what the source can
/// emit. For the receiver / standalone kinds `build_sources` skips
/// (EcowittLocal, HttpWebhook, EcowittGwPoll, TempestUdp, OpenMeteo), the field
/// set is declared here from the same sets those adapters use, so the picker
/// offers a complete, accurate list regardless of how a source is wired.
///
/// Open-Meteo IS a per-field current-conditions owner (its forecast refresher
/// also emits a live `current=` scalar block, live_current=false), so it gets
/// an explicit declared set below. The FORECAST-ONLY cloud kinds (NWS /
/// OpenWeather / Pirate / Met.no / WeatherKit) emit only forecast highs as
/// pseudo-"current" observations, so they return an EMPTY current-field list:
/// the per-field current picker must never offer them (pinning current temp to
/// NWS would be a silent no-op). They participate only in the forecast-source
/// picker keyed on `is_forecast()`.
pub fn source_field_names(cfg: &Config, entry: &SourceEntry) -> Vec<&'static str> {
    use crate::ports::weather_source::WeatherField as F;

    // Fields straight off a constructed adapter's capabilities (the source of
    // truth for the kinds build_sources handles). We rebuild a one-entry config
    // and reuse build_sources so we never drift from the real capability set.
    let from_caps = || -> Vec<F> {
        let mut single = cfg.clone();
        single.sources = vec![entry.clone()];
        build_sources(&single)
            .first()
            .map(|s| s.capabilities().fields.into_iter().collect())
            .unwrap_or_default()
    };

    // Declared sets for the kinds build_sources intentionally skips.
    let declared: Vec<F> = match &entry.source {
        SourceKind::EcowittLocal(_) => vec![
            F::AirTempF,
            F::RhPct,
            F::WindMph,
            F::WindGustMph,
            F::WindBearingDeg,
            F::SolarWm2,
            F::UvIndex,
            F::PressureInHg,
            F::RainIntensityInHr,
            F::RainTodayIn,
            F::LightningCount,
            F::LightningDistanceMi,
            F::DewPointF,
        ],
        SourceKind::EcowittGwPoll(_) => vec![
            F::AirTempF,
            F::DewPointF,
            F::RhPct,
            F::WindMph,
            F::WindGustMph,
            F::PressureInHg,
            F::RainTodayIn,
            F::RainIntensityInHr,
            F::SolarWm2,
            F::UvIndex,
        ],
        SourceKind::TempestUdp(_) => vec![
            F::AirTempF,
            F::DewPointF,
            F::RhPct,
            F::PressureInHg,
            F::WindMph,
            F::WindGustMph,
            F::WindBearingDeg,
            F::UvIndex,
            F::SolarWm2,
            F::Illuminance,
            F::RainTodayIn,
            F::RainIntensityInHr,
            F::LightningCount,
            F::LightningDistanceMi,
        ],
        // Open-Meteo now ALSO ingests LIVE current conditions (the `current=`
        // block in the forecast refresher) and emits them as scalar Observation
        // fields into the merge, so it IS a per-field current-conditions owner.
        // It is a cloud/model-derived current source (not a LAN station), so it
        // carries live_current=false in the picker and a sensibly LOW priority:
        // a real station outranks it by default, but the owner can pin a field
        // (e.g. a wind-shadowed Tempest -> WIND = Open-Meteo) to it. The exact set
        // is OWNED by the refresher (A2): the const it emits is returned verbatim
        // here so the picker can never drift from what `current_fields` actually
        // emits (cross-agent contract).
        SourceKind::OpenMeteo(_) => {
            return field_set_names(crate::forecast::refresher::OPEN_METEO_CURRENT_FIELDS.to_vec())
        }
        // NWS/OpenWeather/Pirate/Met.no/WeatherKit are ALL selectable per-field
        // current-conditions sources now: each emits real current scalars (A4
        // makes NWS emit current temp/RH/wind/etc. and reflect them in
        // capabilities()), so they fall through to the `_` arm below and the
        // picker offers exactly each one's `capabilities().fields`. This is the
        // CLOUD-ONLY tier the owner ranks + pins fields to; the per-field merge
        // demotes through it (fix #3/#4) and never reverts to an un-chosen live
        // station. They remain selectable in the FORECAST-source picker too.
        //
        // HttpWebhook + the mapping kinds (MQTT/HA/Yolink/Tuya/REST/Prom/Influx)
        // ARE constructed by build_sources, so their dynamic mapped field set
        // comes from capabilities() above. Blitzortung is display-only and emits
        // no overrideable scalar current-conditions fields.
        _ => return field_set_names(from_caps()),
    };
    field_set_names(declared)
}

/// Build the forecast-source priority map fed to `forecast_bridge`: each
/// ENABLED forecast-capable source's id -> its configured `priority`. The
/// bridge arbitrates the whole-snapshot forecast by this map (higher wins; an
/// implicit Open-Meteo with no entry defaults to 0, the failover).
///
/// FORECAST-PROVIDER PIN (additive): when `cfg.forecast_provider` names an
/// enabled forecast source already in the map, that source is bumped to a
/// strictly-winning priority (max of all entries + 1) so it OWNS the forecast
/// regardless of the configured ranking. `None`, an unknown id, or a disabled
/// source leaves the map exactly as the per-source priorities define it, so an
/// unset pin is byte-identical to the prior behavior and a stale pinned source
/// still fails over to the next-highest in the bridge.
pub fn forecast_priority_map(cfg: &Config) -> std::collections::HashMap<String, i32> {
    let mut map: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for entry in cfg.sources.iter().filter(|e| e.enabled) {
        if entry.source.is_forecast() {
            map.insert(entry.id.clone(), entry.priority);
        }
    }
    if let Some(pinned) = cfg.forecast_provider.as_deref() {
        if map.contains_key(pinned) {
            let winning = map.values().copied().max().unwrap_or(0).saturating_add(1);
            map.insert(pinned.to_string(), winning);
        }
    }
    map
}

/// The writer LABEL a source's observations carry on the merge bus, the single
/// key every merge-layer map (priorities, max-ages, field overrides) uses so
/// "config id == writer label" is a true invariant for ALL sources. The Tempest
/// UDP path is the lone special case: its `apply_obs` writer stamps the shared
/// `TEMPEST_LABEL` constant rather than the config id, so this returns that same
/// constant for the UDP kind. EVERY other source writes under its config `id`
/// (the bus `source_id`), so this returns `entry.id`. Centralizing it here fixes
/// `source_priority_map` (and friends) silently defaulting any source whose id
/// happened to differ from a hand-written label.
///
/// Public so the honest-status taxonomy (api::health + api::config) tests THIS
/// label (the one the merge actually stamps into `field_provenance`) against the
/// snapshot's complete owner-label set, instead of a friendly display name that
/// never matches the writer's label.
pub fn writer_label(entry: &SourceEntry) -> String {
    if matches!(entry.source, SourceKind::TempestUdp(_)) {
        crate::tempest::state::TEMPEST_LABEL.to_string()
    } else {
        entry.id.clone()
    }
}

/// Build the CURRENT-conditions arbitration priority map for the merge layer:
/// each ENABLED source's `priority`, keyed by the LABEL its writer uses
/// (`TEMPEST_LABEL` for the UDP path, the source id otherwise). Fed to
/// `TempestStore::set_priorities`. Mirrors the boot logic so a hot-reload of the
/// source ranking re-ranks the live merge identically to a restart.
pub fn source_priority_map(cfg: &Config) -> std::collections::HashMap<String, i32> {
    let mut prios: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for entry in cfg.sources.iter().filter(|e| e.enabled) {
        prios.insert(writer_label(entry), entry.priority);
    }
    prios
}

/// Build the CURRENT-conditions MAX-AGE map for the merge layer: each ENABLED
/// source's `max_age_s` (seconds), keyed by the same writer LABEL as
/// `source_priority_map` (`TEMPEST_LABEL` for the UDP path, the source id
/// otherwise). Sources with no configured `max_age_s` fall back to the per-kind
/// region default (`default_max_age_for`) when one exists (the cloud authorities),
/// else are absent and the store uses `LIVE_FRESHNESS_SECS` (`max_age_for`). Fed
/// to `TempestStore::set_max_ages`. Mirrors `source_priority_map` so a hot-reload
/// of the freshness windows re-ranks the live merge identically to a restart.
///
/// This is what fixes the owner's wind-pin bug (fix #2): A3 sets ~2100 on the
/// 1800s-cadence cloud sources (Open-Meteo / NWS / Met.no), so a pinned cloud
/// stays fresh through its full refresh interval instead of being judged stale at
/// the hardcoded 600s mark and demoted out from under the pin. `max_age_s` is
/// `Option<u64>` in config; values are clamped into `i32` (the store key type),
/// saturating at `i32::MAX` for any absurdly large configured age.
pub fn source_max_age_map(cfg: &Config) -> std::collections::HashMap<String, i32> {
    let mut ages: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
    for entry in cfg.sources.iter().filter(|e| e.enabled) {
        let secs = match entry.max_age_s {
            Some(s) => i32::try_from(s).unwrap_or(i32::MAX),
            // No explicit max_age_s: fall back to the per-kind region default for
            // the cloud authorities (NOAA MRMS 7200, the 1800s forecast kinds
            // 2100), so a source whose config never stamped a window (an older
            // seed, or a normalize that did not set it) still gets the correct
            // freshness instead of the ~600s live default and reading stale. Kinds
            // with no region default (live stations, keyed clouds) stay absent.
            None => match crate::config::region::default_max_age_for(&entry.source) {
                Some(d) => i32::try_from(d).unwrap_or(i32::MAX),
                None => continue,
            },
        };
        ages.insert(writer_label(entry), secs);
    }
    ages
}

/// Build the PER-FIELD user-override map for the merge layer: translate
/// `cfg.field_source_overrides` (WeatherField name -> source id) into the merge
/// layer's (snapshot-field key -> writer LABEL) map. An override whose source
/// id, field name, or owner key can't be resolved is skipped (never an error);
/// an empty config map yields an empty override map (priority merge unchanged).
/// Fed to `TempestStore::set_field_overrides`. Mirrors the boot logic so a
/// hot-reload re-pins fields identically to a restart.
pub fn field_override_map(cfg: &Config) -> std::collections::HashMap<&'static str, String> {
    use crate::config::field_overrides::parse_field_name;
    // id -> writer label, for enabled sources only.
    let mut id_label: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for entry in cfg.sources.iter().filter(|e| e.enabled) {
        id_label.insert(entry.id.as_str(), writer_label(entry));
    }
    let mut field_overrides: std::collections::HashMap<&'static str, String> =
        std::collections::HashMap::new();
    for (field_name, source_id) in cfg.field_source_overrides.iter() {
        let Some(field) = parse_field_name(field_name) else {
            continue; // unknown field name -> ignore
        };
        let Some(key) = crate::tempest::state::override_owner_key(field) else {
            continue; // field has no scalar owner key (forecast/string)
        };
        let Some(label) = id_label.get(source_id.as_str()) else {
            continue; // override points at a disabled/unknown source
        };
        field_overrides.insert(key, label.clone());
    }
    field_overrides
}

/// Build the PER-FIELD user CHAIN map for the merge layer: translate
/// `cfg.field_source_chains` (WeatherField name -> ORDERED list of source ids)
/// into the merge layer's (snapshot-field key -> ORDERED list of writer LABELs)
/// map. The ordered-list generalization of `field_override_map`: each source id
/// in a chain is resolved to its writer LABEL (`writer_label`, "Tempest" for the
/// UDP path, the source id otherwise) preserving order; a chain entry whose
/// source id resolves to a disabled/unknown source is dropped from the chain
/// (never an error), a chain whose field name has no scalar owner key is skipped,
/// and a chain that ends up EMPTY after resolution is omitted entirely (so it can
/// never blank a field). An empty config map yields an empty chain map (priority
/// merge unchanged). Fed to `TempestStore::set_field_chains`. Mirrors the boot
/// logic so a hot-reload re-chains fields identically to a restart.
pub fn field_chain_map(cfg: &Config) -> std::collections::HashMap<&'static str, Vec<String>> {
    use crate::config::field_overrides::parse_field_name;
    // id -> writer label, for enabled sources only (same table as
    // field_override_map, so a chain and a pin resolve ids identically).
    let mut id_label: std::collections::HashMap<&str, String> = std::collections::HashMap::new();
    for entry in cfg.sources.iter().filter(|e| e.enabled) {
        id_label.insert(entry.id.as_str(), writer_label(entry));
    }
    let mut chains: std::collections::HashMap<&'static str, Vec<String>> =
        std::collections::HashMap::new();
    for (field_name, source_ids) in cfg.field_source_chains.iter() {
        let Some(field) = parse_field_name(field_name) else {
            continue; // unknown field name -> ignore
        };
        let Some(key) = crate::tempest::state::override_owner_key(field) else {
            continue; // field has no scalar owner key (forecast/string)
        };
        // Resolve each id in order; drop ids pointing at a disabled/unknown source
        // so the chain never carries a dead entry. Order is preserved.
        let labels: Vec<String> = source_ids
            .iter()
            .filter_map(|id| id_label.get(id.as_str()).cloned())
            .collect();
        if labels.is_empty() {
            continue; // an all-dead chain must not blank the field
        }
        chains.insert(key, labels);
    }
    chains
}

/// The live runtime handles the config-apply path swaps into. Created once at
/// boot in main.rs and cloned into both the background tasks (refresher,
/// forecast bridge) and the config API state, so a PUT /api/config (or wizard
/// apply) can re-apply the engine-tunable subset of config to the RUNNING
/// system with no container restart.
///
/// What each handle drives:
///   * `tempest_store`     -> live current-conditions merge (source priorities
///                            + per-field overrides; setters are `&self`).
///   * `forecast_priority` -> the forecast bridge's provider ranking + pin.
///   * `watering_policy`   -> the engine skip-rule params the refresher reads
///                            each tick (thresholds, restrictions, seasonal
///                            dial, manual schedules, soil/budget zones).
#[derive(Clone)]
pub struct RuntimeHandles {
    pub tempest_store: Arc<crate::tempest::state::TempestStore>,
    pub forecast_priority: Arc<ArcSwap<std::collections::HashMap<String, i32>>>,
    pub watering_policy: Arc<ArcSwap<crate::refresher::WateringPolicy>>,
    /// The manual-schedule set the manual dispatcher loads at the top of every
    /// tick (src/scheduler/manual.rs). Swapped here on every config-write path so
    /// adding or editing a schedule takes effect on the next tick with no restart
    /// (the dispatcher is spawned unconditionally at boot; see main.rs). Mirrors
    /// the watering_policy / forecast_priority ArcSwap pattern above.
    pub manual_schedules: Arc<ArcSwap<Vec<crate::config::schema::ManualSchedule>>>,
    /// Live per-source last-REACHABLE map (the same handle the bus recorder
    /// records into and /api/health reads). Threaded here so
    /// /api/config/source_catalog computes the honest source-status taxonomy from
    /// the SAME reachability facts as /api/health, keeping the row UI and the
    /// health rollup congruent. Cloneable; all clones see the same state.
    pub source_reachable: crate::sources::SourceReachability,
    /// Live per-source last-OBSERVATION map (the same handle the bus recorder
    /// records into and `HealthState.source_last_seen` reads). Threaded here so
    /// /api/config/source_catalog can feed `compute_source_status` the SAME
    /// observation-liveness proof /api/health uses (`last_obs_epoch` +
    /// `obs_alive_window_s`): a source that has OBSERVED within its kind-aware
    /// window reads its calm status (active/standby/watching), NOT `offline`,
    /// even when its Reachability epoch has gone stale (the adapters publish
    /// Reachability only on state CHANGE, so a stably-reachable source can carry a
    /// stale reachability epoch while still observing every few minutes). This is
    /// the catalog-vs-health congruence fix for the MRMS-reads-offline bug.
    /// `Option` (mirroring `HealthState.source_last_seen`): `None` on a build that
    /// does not wire it, in which case the catalog keeps its prior reachability-only
    /// behavior. Cloneable; all clones see the same state.
    pub source_last_seen: Option<crate::sources::SourceLastSeen>,
}

/// Outcome of a config hot-reload: which tunables were re-applied live, and
/// whether the applied config ALSO touched something only a boot can wire, so
/// the caller can tell the UI a restart is still required for those parts.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ConfigApplyOutcome {
    /// True when the new config differs from the previous on a field that ONLY
    /// the boot path wires (a new/removed/re-kinded source connection or
    /// controller, the zone set, listen address, auth mode, MQTT publisher
    /// gating, ...). The Wave-2 UI shows a "restart required" banner when true.
    /// The hot-reloadable tunables (priorities, per-field overrides, forecast
    /// provider, watering policy) are ALWAYS applied live regardless of this
    /// flag; it reports only the residue that a live apply cannot reach.
    pub restart_required: bool,
    /// Human-readable reasons restart_required is true, for the UI banner +
    /// logs. Empty when restart_required is false.
    pub restart_reasons: Vec<String>,
}

/// Re-apply the engine-tunable subset of `new_cfg` to the LIVE running system,
/// and compute whether anything that can only be wired at boot also changed.
///
/// Call this AFTER persisting `new_cfg` (PUT /api/config + wizard apply). It is
/// the single hot-reload entry point so the PUT path and the wizard apply path
/// behave identically, and so a reloaded value can never diverge from the value
/// boot would have produced for the same config (all four mappings reuse the
/// same builders boot uses).
///
/// `prev` is the config as it was BEFORE this apply (the on-disk config the
/// caller loaded to round-trip redacted secrets). `None` on a fresh install
/// (no prior config), in which case the connection/zone diff is skipped and
/// only the live re-apply runs.
pub fn apply_runtime_config(
    handles: &RuntimeHandles,
    prev: Option<&Config>,
    new_cfg: &Config,
) -> ConfigApplyOutcome {
    // --- Hot-reload the tunables that drive the LIVE engine/merge. ---
    // 1. Source priorities + per-field overrides: mutate the SHARED TempestStore
    //    so the current-conditions merge re-ranks on the next packet.
    handles
        .tempest_store
        .set_priorities(source_priority_map(new_cfg));
    // Per-source freshness windows (fix #2): re-rank max-ages alongside priorities
    // so a hot-reload of a source's max_age_s takes effect on the next packet,
    // identically to a restart. Without this a pinned cloud's max_age change would
    // need a container restart and the wind-pin demote could resurface live.
    handles
        .tempest_store
        .set_max_ages(source_max_age_map(new_cfg));
    handles
        .tempest_store
        .set_field_overrides(field_override_map(new_cfg));
    // Per-field PRIORITY CHAINS (the ordered-failover generalization of the
    // single pin): install alongside the overrides so a hot-reload of a chain
    // takes effect on the next packet identically to a restart. An empty chain map
    // leaves the priority merge unchanged; a chain and a legacy pin coexist (the
    // merge treats a lone pin as a 1-element chain).
    handles
        .tempest_store
        .set_field_chains(field_chain_map(new_cfg));
    // 2. Forecast provider / ranking: swap the bridge's priority handle.
    handles
        .forecast_priority
        .store(Arc::new(forecast_priority_map(new_cfg)));
    // 3. Watering policy (skip-rule thresholds, restrictions, seasonal dial,
    //    manual schedules, soil/budget zones, units): swap the handle the
    //    refresher loads each tick.
    handles
        .watering_policy
        .store(Arc::new(crate::refresher::WateringPolicy::from_config(
            new_cfg,
        )));
    // 4. Manual schedules: swap the handle the manual dispatcher loads at the top
    //    of each tick. Editing or adding a schedule (including the FIRST one on a
    //    previously-empty config) is picked up on the next tick with no restart;
    //    the dispatcher is spawned unconditionally at boot so a first schedule can
    //    actuate. This is why manual_schedules is NOT a restart_required field.
    handles
        .manual_schedules
        .store(Arc::new(new_cfg.manual_schedules.clone()));

    // --- Compute the boot-only residue for the restart-required contract. ---
    let mut reasons: Vec<String> = Vec::new();
    if let Some(prev) = prev {
        // Source CONNECTIONS are spawned once at boot (one task per adapter).
        // Re-ranking an existing source hot-reloads above, but adding/removing
        // a source, toggling enabled, or changing a source's kind needs a boot
        // to (de)spawn its adapter task. Compare the (id, kind tag, enabled)
        // identity set, ignoring priority + the per-field override map (both
        // hot-reloaded).
        if source_wiring_fingerprint(prev) != source_wiring_fingerprint(new_cfg) {
            reasons.push(
                "a weather source was added, removed, enabled/disabled, or changed kind \
                 (source connections are wired at boot)"
                    .to_string(),
            );
        }
        // Controllers are built once at boot (build_controllers); the dispatch
        // registry is not hot-swapped here.
        if controller_wiring_fingerprint(prev) != controller_wiring_fingerprint(new_cfg) {
            reasons.push(
                "an irrigation controller was added, removed, enabled/disabled, or changed kind \
                 (controllers are wired at boot)"
                    .to_string(),
            );
        }
        // The refresher's active zone list is resolved once at spawn time.
        // Adding/removing/renaming a zone slug needs a restart (the per-zone
        // soil/budget params inside an EXISTING zone DO hot-reload via the
        // watering policy above; only the zone SET is boot-bound).
        let prev_zones: std::collections::BTreeSet<&String> = prev.zones.keys().collect();
        let new_zones: std::collections::BTreeSet<&String> = new_cfg.zones.keys().collect();
        if prev_zones != new_zones {
            reasons.push(
                "the set of zones changed (the refresher's zone list is resolved at boot)"
                    .to_string(),
            );
        }
        // Listen address / Leptos site config is read once at boot.
        if prev.deployment.mode != new_cfg.deployment.mode {
            reasons.push(
                "the snapshot mode (HA vs native) changed (the refresher's source is chosen at boot)"
                    .to_string(),
            );
        }
        // MQTT discovery publisher is spawned (or not) once at boot.
        if mqtt_publish_fingerprint(prev) != mqtt_publish_fingerprint(new_cfg) {
            reasons.push(
                "the HA MQTT discovery publisher configuration changed \
                 (the publisher is started at boot)"
                    .to_string(),
            );
        }
    }
    ConfigApplyOutcome {
        restart_required: !reasons.is_empty(),
        restart_reasons: reasons,
    }
}

/// Identity of the source-connection wiring: (id, kind tag, enabled) per source,
/// sorted. Excludes priority + field overrides (both hot-reloaded), so this only
/// changes when a connection must be (de)spawned at boot.
fn source_wiring_fingerprint(cfg: &Config) -> Vec<(String, &'static str, bool)> {
    let mut v: Vec<(String, &'static str, bool)> = cfg
        .sources
        .iter()
        .map(|e| {
            (
                e.id.clone(),
                crate::config::kind_labels::source_kind_label(&e.source),
                e.enabled,
            )
        })
        .collect();
    v.sort();
    v
}

/// Identity of the controller wiring: (id, kind tag, enabled, default) per
/// controller, sorted. Changes only when a controller must be rebuilt at boot.
fn controller_wiring_fingerprint(cfg: &Config) -> Vec<(String, &'static str, bool, bool)> {
    let mut v: Vec<(String, &'static str, bool, bool)> = cfg
        .controllers
        .iter()
        .map(|e| {
            (
                e.id.clone(),
                crate::config::kind_labels::controller_kind_label(&e.controller),
                e.enabled,
                e.default,
            )
        })
        .collect();
    v.sort();
    v
}

/// Whether the HA MQTT discovery publisher would be started, plus its broker
/// identity, so a change to the gating or the broker connection flags a restart
/// (the publisher task is spawned once at boot).
fn mqtt_publish_fingerprint(cfg: &Config) -> (bool, Option<(String, u16, String, bool)>) {
    let enabled = cfg.features.enable_mqtt_publish;
    let broker = cfg.notifications.mqtt.as_ref().map(|m| {
        (
            m.host.clone(),
            m.port,
            m.discovery_prefix.clone(),
            m.publish_enabled,
        )
    });
    (enabled, broker)
}

/// Map a WeatherField list to its canonical override names, dropping any field
/// without a name (the structured-forecast variants), de-duplicated + sorted
/// for a stable picker order.
fn field_set_names(fields: Vec<crate::ports::weather_source::WeatherField>) -> Vec<&'static str> {
    use crate::config::field_overrides::field_name;
    let mut out: Vec<&'static str> = fields.into_iter().filter_map(field_name).collect();
    out.sort_unstable();
    out.dedup();
    out
}

pub fn build_controllers(
    cfg: &Config,
    runs: RunsStore,
) -> Vec<(Arc<dyn IrrigationController>, bool)> {
    let mut out: Vec<(Arc<dyn IrrigationController>, bool)> = Vec::new();
    for entry in &cfg.controllers {
        if !entry.enabled {
            continue;
        }
        // Each adapter's new() is fallible (typically the reqwest
        // Client builder rejecting on TLS root loading), and a single
        // bad controller must not take the whole runtime down. On Err
        // we log with the controller id and the failure reason, then
        // drop the entry from the active set so the rest of the
        // irrigation stack stays up.
        fn skip<E: std::fmt::Display>(
            id: &str,
            kind: &'static str,
            e: E,
        ) -> Option<Arc<dyn IrrigationController>> {
            warn!(controller = id, kind = kind, error = %e, "controller init failed; skipping");
            None
        }
        let constructed: Option<Arc<dyn IrrigationController>> = match &entry.controller {
            ControllerKind::DryRun(c) => Some(Arc::new(DryRunController::new(
                entry.id.clone(),
                c.clone(),
                Some(runs.clone()),
            ))),
            ControllerKind::OpensprinklerDirect(c) => {
                // Build zone -> station map from cfg.zones for this
                // controller. Station strings parsed as 1-based ints.
                let mut zone_to_station = std::collections::HashMap::new();
                for (slug, zone) in &cfg.zones {
                    if zone.controller_id != entry.id {
                        continue;
                    }
                    if let Ok(s) = zone.controller_station.parse::<u32>() {
                        // OpenSprinkler stations are 1-based; station "0" is
                        // not a valid station. Without this guard a `0` would
                        // pass through to the adapter and alias onto station 1
                        // (the GET /cm sid is 0-based), silently watering the
                        // WRONG zone. Reject it here at load and drop the
                        // mapping so the zone is simply unbound (loud, no
                        // mis-actuation) rather than firing a sibling valve.
                        if s == 0 {
                            warn!(
                                controller = %entry.id,
                                zone = %slug,
                                "opensprinkler controller_station=\"0\" is invalid (stations are \
                                 1-based); dropping the zone mapping. Set a station >= 1."
                            );
                            continue;
                        }
                        // Normalize to underscore slugs so the map matches
                        // zones::configured() + the snapshot + the schedulers
                        // (config keys may be hyphenated, e.g. "back-yard").
                        // Without this, status() readback AND native dispatch
                        // (run_zone) miss with a slug mismatch.
                        zone_to_station.insert(slug.replace('-', "_"), s);
                    }
                }
                match OpenSprinklerDirect::new(entry.id.clone(), c.clone(), zone_to_station) {
                    Ok(ctl) => Some(Arc::new(ctl)),
                    Err(e) => skip(&entry.id, "opensprinkler_direct", e),
                }
            }
            ControllerKind::HaServiceCall(c) => {
                match HaServiceCall::new(entry.id.clone(), c.clone()) {
                    Ok(ctl) => Some(Arc::new(ctl)),
                    Err(e) => skip(&entry.id, "ha_service_call", e),
                }
            }
            ControllerKind::Rachio(c) => match Rachio::new(entry.id.clone(), c.clone()) {
                Ok(ctl) => Some(Arc::new(ctl)),
                Err(e) => skip(&entry.id, "rachio", e),
            },
            ControllerKind::Hydrawise(c) => match Hydrawise::new(entry.id.clone(), c.clone()) {
                Ok(ctl) => Some(Arc::new(ctl)),
                Err(e) => skip(&entry.id, "hydrawise", e),
            },
            ControllerKind::Bhyve(c) => match Bhyve::new(entry.id.clone(), c.clone()) {
                Ok(ctl) => Some(Arc::new(ctl)),
                Err(e) => skip(&entry.id, "bhyve", e),
            },
            ControllerKind::Rainbird(c) => match Rainbird::new(entry.id.clone(), c.clone()) {
                Ok(ctl) => Some(Arc::new(ctl)),
                Err(e) => skip(&entry.id, "rainbird", e),
            },
            ControllerKind::MqttCommand(c) => {
                Some(Arc::new(MqttCommand::new(entry.id.clone(), c.clone())))
            }
            ControllerKind::HttpGeneric(c) => {
                // Build zone -> board-station map from cfg.zones for this
                // controller. Station ids are opaque strings the board uses.
                let mut zone_to_station = std::collections::HashMap::new();
                for (slug, zone) in &cfg.zones {
                    if zone.controller_id != entry.id {
                        continue;
                    }
                    let station = zone.controller_station.trim();
                    if !station.is_empty() {
                        // Normalize to underscore slugs so the map matches
                        // zones::configured() + the snapshot + the schedulers,
                        // mirroring the OpenSprinkler path.
                        zone_to_station.insert(slug.replace('-', "_"), station.to_string());
                    }
                }
                match HttpGeneric::new(entry.id.clone(), c.clone(), zone_to_station) {
                    Ok(ctl) => Some(Arc::new(ctl)),
                    Err(e) => skip(&entry.id, "http_generic", e),
                }
            }
            ControllerKind::EsphomeNative(_) => {
                // Deferred: ESPHome native API uses a custom binary
                // protocol; needs a dedicated crate or hand-rolled
                // implementation. Tracked separately.
                warn!(controller = %entry.id, "ESPHome native controller not yet built; skipping");
                None
            }
        };
        if let Some(c) = constructed {
            out.push((c, entry.default));
        }
    }
    out
}

/// Build a single controller adapter for the wizard's test/scan endpoints.
/// No RunsStore and an empty zone map, these endpoints only need device
/// reachability (`status()`) and zone enumeration (`discover_zones()`),
/// neither of which depends on the zone mapping. Returns Err for kinds
/// that can't be probed (fire-and-forget / HA-mediated / not-yet-built).
pub fn build_test_controller(
    entry: &crate::config::schema::ControllerEntry,
) -> Result<Arc<dyn IrrigationController>, String> {
    let c: Arc<dyn IrrigationController> = match &entry.controller {
        // DryRun is the zero-hardware sandbox: always reachable, and its
        // discover_zones returns sample stations so the wizard's whole
        // test + scan + import flow works before any real gear exists.
        ControllerKind::DryRun(c) => {
            Arc::new(DryRunController::new(entry.id.clone(), c.clone(), None))
        }
        ControllerKind::OpensprinklerDirect(c) => Arc::new(
            OpenSprinklerDirect::new(entry.id.clone(), c.clone(), Default::default())
                .map_err(|e| e.to_string())?,
        ),
        ControllerKind::Rachio(c) => {
            Arc::new(Rachio::new(entry.id.clone(), c.clone()).map_err(|e| e.to_string())?)
        }
        ControllerKind::Hydrawise(c) => {
            Arc::new(Hydrawise::new(entry.id.clone(), c.clone()).map_err(|e| e.to_string())?)
        }
        ControllerKind::Bhyve(c) => {
            Arc::new(Bhyve::new(entry.id.clone(), c.clone()).map_err(|e| e.to_string())?)
        }
        ControllerKind::Rainbird(c) => {
            Arc::new(Rainbird::new(entry.id.clone(), c.clone()).map_err(|e| e.to_string())?)
        }
        // DIY HTTP boards are pollable: GET /status proves reachability and
        // GET /zones backs the wizard's "scan zones" import.
        ControllerKind::HttpGeneric(c) => Arc::new(
            HttpGeneric::new(entry.id.clone(), c.clone(), Default::default())
                .map_err(|e| e.to_string())?,
        ),
        _ => {
            return Err(
                "this controller kind can't be probed (fire-and-forget or HA-mediated)".into(),
            )
        }
    };
    Ok(c)
}

async fn build_llm(cfg: &Config) -> Option<Arc<dyn LlmProvider>> {
    build_llm_from(cfg.llm.as_ref()?).await
}

/// Build a provider straight from an `LlmConfig`, independent of a full
/// `Config`. Used by `build_llm` at boot and by the wizard's test_llm
/// endpoint to probe a draft provider before it is applied.
pub async fn build_llm_from(
    llm_cfg: &crate::config::schema::LlmConfig,
) -> Option<Arc<dyn LlmProvider>> {
    match &llm_cfg.provider {
        LlmProviderKind::Auto(auto_cfg) => {
            let targets = if auto_cfg.probe_order.is_empty() {
                default_probe_targets()
            } else {
                auto_cfg
                    .probe_order
                    .iter()
                    .map(|url| ProbeTarget {
                        kind: if url.contains("11434") {
                            ProbeKind::Ollama
                        } else {
                            ProbeKind::OpenaiCompat
                        },
                        base_url: url.clone(),
                    })
                    .collect()
            };
            // Time-box the probe so a slow boot doesn't block the runtime.
            tokio::time::timeout(Duration::from_secs(10), detect(targets, String::new()))
                .await
                .ok()
                .flatten()
        }
        LlmProviderKind::Ollama(c) => Some(Arc::new(OllamaProvider::new(
            "ollama",
            c.base_url.clone(),
            c.model.clone(),
        ))),
        LlmProviderKind::Llamacpp(c) => Some(Arc::new(OpenaiCompatProvider::new(
            "llamacpp",
            c.base_url.clone(),
            c.model.clone().unwrap_or_default(),
            None,
        ))),
        LlmProviderKind::OpenaiCompat(c) => Some(Arc::new(OpenaiCompatProvider::new(
            "openai_compat",
            c.base_url.clone(),
            c.model.clone(),
            c.api_key.clone(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DryRunConfig;

    #[tokio::test]
    async fn boot_with_demo_only_config() {
        let dir = std::env::temp_dir().join(format!("ls-runtime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("localsky.toml");
        let db_path = dir.join("test.db");

        // Pre-create a minimal config so boot doesn't have to synthesize
        // from env.
        let mut cfg = Config::default();
        cfg.features.demo_mode = true;
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "demo".into(),
            priority: 100,
            max_age_s: None,
            enabled: true,
            source: crate::config::schema::SourceKind::DemoReplay(Default::default()),
        });
        cfg.controllers
            .push(crate::config::schema::ControllerEntry {
                id: "dry".into(),
                default: true,
                enabled: true,
                controller: crate::config::schema::ControllerKind::DryRun(DryRunConfig {
                    simulate_runs: false,
                }),
            });
        let store = FileConfigStore::new(&cfg_path);
        store.save(&cfg).await.unwrap();

        let rt = Runtime::boot(cfg_path, db_path).await.unwrap();
        assert_eq!(rt.controllers.ids(), vec!["dry".to_string()]);
        assert_eq!(rt.sources.ids(), vec!["demo".to_string()]);
        rt.signal_shutdown();
    }

    #[tokio::test]
    async fn boot_falls_back_to_env_compat_when_no_config() {
        let dir = std::env::temp_dir().join(format!("ls-runtime-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("doesnotexist.toml");
        let db_path = dir.join("env.db");
        let rt = Runtime::boot(cfg_path, db_path).await.unwrap();
        // env_compat synthesizes tempest_lan + open_meteo SourceEntries
        // into the Config, but build_sources only constructs the v2-
        // ready adapters (DemoReplay today). The legacy paths continue
        // to serve weather data into the UI until the remaining
        // adapters are promoted to WeatherSource.
        let cfg = rt.config.load();
        // With no config file and no TEMPEST_* env, env_compat now synthesizes a
        // cloud-first default (Open-Meteo) and does NOT synthesize a passive
        // tempest_lan listener (which would otherwise report offline on a
        // no-hardware install). A Tempest source is only synthesized when
        // TEMPEST_BIND_ADDR / TEMPEST_HUB_SERIAL is set.
        assert!(
            cfg.sources.iter().any(|s| s.id == "open_meteo"),
            "env_compat should synthesize a cloud-first open_meteo source"
        );
        assert!(
            !cfg.sources.iter().any(|s| s.id == "tempest_lan"),
            "no-hardware env_compat must not synthesize a phantom tempest_lan"
        );
        rt.signal_shutdown();
    }

    #[test]
    fn build_test_controller_dispatches_probeable_vs_unsupported() {
        use crate::config::schema::{
            ControllerEntry, ControllerKind, HaServiceCallConfig, HttpGenericConfig,
            MqttCommandConfig,
        };
        let entry = |id: &str, k: ControllerKind| ControllerEntry {
            id: id.into(),
            default: false,
            enabled: true,
            controller: k,
        };

        // Pollable kinds build a probe controller (status()/discover_zones()).
        let http = entry(
            "diy",
            ControllerKind::HttpGeneric(HttpGenericConfig {
                base_url: "http://192.0.2.50".into(),
                bearer_token: None,
                poll_interval_s: 10,
            }),
        );
        assert!(
            build_test_controller(&http).is_ok(),
            "http_generic is probeable"
        );

        let dry = entry("dry", ControllerKind::DryRun(DryRunConfig::default()));
        assert!(build_test_controller(&dry).is_ok(), "dry_run is probeable");

        // Fire-and-forget / HA-mediated kinds can't be probed without side effects.
        let mqtt = entry(
            "mq",
            ControllerKind::MqttCommand(MqttCommandConfig {
                broker_host: "broker.local".into(),
                broker_port: 1883,
                username: None,
                password: None,
                client_id: None,
                availability_topic: None,
                payload_available: "online".into(),
                payload_not_available: "offline".into(),
                flow_topic: None,
                zone_command_map: Default::default(),
            }),
        );
        assert!(
            build_test_controller(&mqtt).is_err(),
            "mqtt_command is not probeable"
        );

        let ha = entry(
            "ha",
            ControllerKind::HaServiceCall(HaServiceCallConfig {
                base_url: "http://ha.local:8123".into(),
                bearer_token: "t".into(),
                start_service: "script.os_zone_toggle".into(),
                stop_service: "opensprinkler.stop".into(),
                zone_entity_map: Default::default(),
            }),
        );
        assert!(
            build_test_controller(&ha).is_err(),
            "ha_service_call is not probeable"
        );
    }

    // --- forecast_priority_map (forecast-source pin) ---

    fn forecast_entry(id: &str, prio: i32, enabled: bool) -> SourceEntry {
        use crate::config::schema::{NwsConfig, OpenMeteoConfig, OpenWeatherConfig};
        // Pick a kind per id so the map covers multiple forecast kinds.
        let source = match id {
            "nws" => SourceKind::Nws(NwsConfig {
                user_agent: "test".into(),
            }),
            "openweather" => SourceKind::OpenWeather(OpenWeatherConfig {
                api_key: "k".into(),
            }),
            _ => SourceKind::OpenMeteo(OpenMeteoConfig {
                forecast_days: 7,
                forecast_hours: 48,
                past_days: 1,
                include_radar: false,
                model: crate::forecast::model_catalog::DEFAULT_MODEL.to_string(),
            }),
        };
        SourceEntry {
            id: id.into(),
            priority: prio,
            max_age_s: None,
            enabled,
            source,
        }
    }

    #[test]
    fn forecast_map_unset_pin_is_per_source_priority() {
        // ADDITIVE invariant: no forecast_provider -> the map is exactly each
        // enabled forecast source's own priority, nothing bumped.
        let mut cfg = Config::default();
        cfg.sources.push(forecast_entry("open_meteo", 50, true));
        cfg.sources.push(forecast_entry("nws", 60, true));
        let map = forecast_priority_map(&cfg);
        assert_eq!(map.get("open_meteo"), Some(&50));
        assert_eq!(map.get("nws"), Some(&60));
    }

    #[test]
    fn forecast_map_pin_wins_strictly() {
        // Pinning the lower-priority Open-Meteo bumps it ABOVE the highest
        // (60) so it owns the forecast in the bridge (strict >).
        let mut cfg = Config::default();
        cfg.sources.push(forecast_entry("open_meteo", 50, true));
        cfg.sources.push(forecast_entry("nws", 60, true));
        cfg.forecast_provider = Some("open_meteo".into());
        let map = forecast_priority_map(&cfg);
        let pinned = map["open_meteo"];
        assert!(
            pinned > map["nws"],
            "pinned open_meteo({pinned}) must outrank nws({})",
            map["nws"]
        );
    }

    #[test]
    fn forecast_map_unknown_or_disabled_pin_is_ignored() {
        // A pin naming a disabled source (or an unknown id) leaves the map
        // unchanged: the pin never blanks the forecast.
        let mut cfg = Config::default();
        cfg.sources.push(forecast_entry("open_meteo", 50, true));
        cfg.sources.push(forecast_entry("nws", 60, false)); // disabled
        cfg.forecast_provider = Some("nws".into());
        let map = forecast_priority_map(&cfg);
        assert!(!map.contains_key("nws"), "disabled source absent from map");
        assert_eq!(map.get("open_meteo"), Some(&50), "untouched by ignored pin");

        cfg.forecast_provider = Some("does_not_exist".into());
        let map = forecast_priority_map(&cfg);
        assert_eq!(map.get("open_meteo"), Some(&50), "unknown pin is a no-op");
    }

    // ── source_max_age_map (fix #2 freshness contract) ──

    #[test]
    fn max_age_map_keys_by_writer_label_and_skips_unset() {
        // A3 sets max_age_s on the 1800s-cadence sources; A1 reads it here. The
        // map is keyed by the WRITER LABEL (TEMPEST_LABEL for the UDP path, the id
        // otherwise), matching source_priority_map, and a source with no max_age_s
        // is simply absent (the store falls back to LIVE_FRESHNESS_SECS for it).
        let mut cfg = Config::default();
        // A UDP Tempest with NO max_age_s -> absent (writer label = TEMPEST_LABEL).
        cfg.sources.push(SourceEntry {
            id: "tempest_lan".into(),
            priority: 100,
            max_age_s: None,
            enabled: true,
            source: SourceKind::TempestUdp(crate::config::schema::TempestUdpConfig {
                bind_addr: "0.0.0.0:50222".into(),
                hub_serial: None,
            }),
        });
        // A cloud with a 2100s max_age -> present under its id.
        cfg.sources.push(SourceEntry {
            id: "open_meteo".into(),
            priority: 50,
            max_age_s: Some(2100),
            enabled: true,
            source: SourceKind::OpenMeteo(crate::config::schema::OpenMeteoConfig {
                forecast_days: 7,
                forecast_hours: 48,
                past_days: 1,
                include_radar: false,
                model: crate::forecast::model_catalog::DEFAULT_MODEL.to_string(),
            }),
        });
        // A cloud AUTHORITY (NOAA MRMS) with NO max_age_s -> present at its per-kind
        // region default (7200), NOT absent: an older seed that never stamped a
        // window must still get the wide MRMS freshness or its hourly accumulation
        // reads stale in the merge.
        cfg.sources.push(SourceEntry {
            id: "noaa_mrms".into(),
            priority: 50,
            max_age_s: None,
            enabled: true,
            source: SourceKind::NoaaMrms(crate::config::schema::NoaaMrmsConfig::default()),
        });
        // The keyed current-conditions providers (OpenWeather, Pirate, WeatherKit)
        // with NO max_age_s -> now seeded at the 3900s health-bucket window (NOT
        // absent / 600s), so a normal ~10 to 60 min poll gap does not read stale
        // and flap the rain owner (plan section 1.4). Regression for the new
        // default_max_age_for arms.
        cfg.sources.push(SourceEntry {
            id: "openweather".into(),
            priority: 55,
            max_age_s: None,
            enabled: true,
            source: SourceKind::OpenWeather(crate::config::schema::OpenWeatherConfig {
                api_key: "k".into(),
            }),
        });
        cfg.sources.push(SourceEntry {
            id: "pirate".into(),
            priority: 60,
            max_age_s: None,
            enabled: true,
            source: SourceKind::PirateWeather(crate::config::schema::PirateWeatherConfig {
                api_key: "k".into(),
            }),
        });
        cfg.sources.push(SourceEntry {
            id: "weatherkit".into(),
            priority: 55,
            max_age_s: None,
            enabled: true,
            source: SourceKind::WeatherKit(crate::config::schema::WeatherKitConfig {
                key_id: "k".into(),
                team_id: "t".into(),
                service_id: "s".into(),
                private_key_pem: "p".into(),
                language: "en".into(),
            }),
        });
        let ages = source_max_age_map(&cfg);
        assert_eq!(
            ages.get("open_meteo"),
            Some(&2100),
            "a configured cloud max_age is keyed by its id"
        );
        assert_eq!(
            ages.get(crate::tempest::state::TEMPEST_LABEL),
            None,
            "an unset max_age leaves the source absent (store falls back to 600)"
        );
        assert_eq!(
            ages.get("noaa_mrms"),
            Some(&7200),
            "a cloud authority with no max_age_s falls back to its region default"
        );
        // The keyed providers now seed at 3900s instead of falling absent (the new
        // default_max_age_for arms), so they no longer read stale between polls.
        assert_eq!(
            ages.get("openweather"),
            Some(&3900),
            "an unset-max_age OpenWeather now seeds at the 3900s health-bucket window"
        );
        assert_eq!(
            ages.get("pirate"),
            Some(&3900),
            "an unset-max_age PirateWeather now seeds at the 3900s health-bucket window"
        );
        assert_eq!(
            ages.get("weatherkit"),
            Some(&3900),
            "an unset-max_age WeatherKit now seeds at the 3900s health-bucket window"
        );
        // The UDP source's id must NOT leak as a key: only the writer label does.
        assert!(!ages.contains_key("tempest_lan"));
    }

    #[test]
    fn max_age_map_skips_disabled_and_clamps_to_i32() {
        // Disabled sources never enter the map; an absurd configured age saturates
        // at i32::MAX rather than wrapping (the store key type is i32).
        let mut cfg = Config::default();
        cfg.sources.push(SourceEntry {
            id: "nws".into(),
            priority: 40,
            max_age_s: Some(u64::MAX),
            enabled: false, // disabled -> excluded
            source: SourceKind::Nws(crate::config::schema::NwsConfig {
                user_agent: "test".into(),
            }),
        });
        assert!(
            source_max_age_map(&cfg).is_empty(),
            "disabled source excluded"
        );

        cfg.sources[0].enabled = true;
        let ages = source_max_age_map(&cfg);
        assert_eq!(
            ages.get("nws"),
            Some(&i32::MAX),
            "an absurd max_age saturates into i32 rather than wrapping"
        );
    }

    // ── Config hot-reload (apply_runtime_config) ──────────────────────────────
    //
    // These prove the genuine runtime hot-reload (review #10): calling the SAME
    // re-apply function the PUT /api/config + wizard-apply paths call updates the
    // LIVE state with no task re-spawn.

    use crate::config::schema::{EcowittLocalConfig, SourceKind, TempestUdpConfig};
    use crate::ports::weather_source::WeatherField;

    /// A config with a UDP-Tempest live source + an Ecowitt live source, plus the
    /// supplied per-field overrides. Tempest priority < Ecowitt so an UN-pinned
    /// field follows Ecowitt; a pinned field must follow Tempest.
    fn cfg_two_live_sources(
        tempest_prio: i32,
        ecowitt_prio: i32,
        overrides: &[(&str, &str)],
    ) -> Config {
        let mut cfg = Config::default();
        cfg.sources.push(SourceEntry {
            id: "tempest".into(),
            priority: tempest_prio,
            max_age_s: None,
            enabled: true,
            source: SourceKind::TempestUdp(TempestUdpConfig {
                bind_addr: "0.0.0.0:50222".into(),
                hub_serial: None,
            }),
        });
        cfg.sources.push(SourceEntry {
            id: "ecowitt".into(),
            priority: ecowitt_prio,
            max_age_s: None,
            enabled: true,
            source: SourceKind::EcowittLocal(EcowittLocalConfig {
                path: "/ingest/ecowitt".into(),
                shared_secret: None,
            }),
        });
        for (field, source_id) in overrides {
            cfg.field_source_overrides
                .insert((*field).to_string(), (*source_id).to_string());
        }
        cfg
    }

    fn handles_with(cfg: &Config) -> RuntimeHandles {
        let h = RuntimeHandles {
            tempest_store: Arc::new(crate::tempest::state::TempestStore::new()),
            forecast_priority: Arc::new(ArcSwap::from_pointee(std::collections::HashMap::new())),
            watering_policy: Arc::new(ArcSwap::from_pointee(
                crate::refresher::WateringPolicy::default(),
            )),
            manual_schedules: Arc::new(ArcSwap::from_pointee(Vec::new())),
            source_reachable: crate::sources::SourceReachability::default(),
            source_last_seen: Some(crate::sources::SourceLastSeen::default()),
        };
        // Seed the boot state, as main.rs does, so the test starts from a
        // realistic "booted" baseline before the hot-reload.
        apply_runtime_config(&h, None, cfg);
        h
    }

    #[test]
    fn hot_reload_field_override_changes_live_merge_without_respawn() {
        // (a) A per-field override change is reflected in the LIVE merge's field
        // ownership when re-applied via apply_runtime_config (the PUT path), with
        // NO new TempestStore / no re-spawn -- the SAME shared store updates.
        use WeatherField as F;

        // Boot config: NO override. Tempest(60) < Ecowitt(90).
        let cfg0 = cfg_two_live_sources(60, 90, &[]);
        let h = handles_with(&cfg0);
        let store = h.tempest_store.clone();

        // Both sources report wind. With no override, the higher-priority Ecowitt
        // owns wind. The Tempest UDP source's writer label is "Tempest" (capital),
        // which is how source_priority_map + field_override_map key it.
        store.apply_source_fields(&[(F::WindMph, 5.0)], 1_000, true, "Tempest");
        store.apply_source_fields(&[(F::WindMph, 22.0)], 1_010, true, "ecowitt");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            22.0,
            "pre-reload: priority gives wind to the higher-priority Ecowitt"
        );

        // HOT RELOAD: pin Wind to the LOWER-priority Tempest via the same
        // re-apply the PUT handler calls. No re-spawn, same shared store.
        let cfg1 = cfg_two_live_sources(60, 90, &[("wind_mph", "tempest")]);
        let outcome = apply_runtime_config(&h, Some(&cfg0), &cfg1);
        assert!(
            !outcome.restart_required,
            "a pure field-override change must NOT require a restart: {:?}",
            outcome.restart_reasons
        );

        // The override is installed on the live store: the next ticks have
        // Tempest reclaim wind and Ecowitt must no longer seize it.
        store.apply_source_fields(&[(F::WindMph, 7.0)], 1_020, true, "Tempest");
        store.apply_source_fields(&[(F::WindMph, 30.0)], 1_030, true, "ecowitt");
        let s = store.snapshot();
        assert_eq!(
            s.wind_avg_mph, 7.0,
            "post-reload: the pinned lower-priority Tempest owns wind on the LIVE merge"
        );
        // Provenance (the field-ownership map the Data sources page reads) now
        // attributes wind to Tempest, proving live ownership changed in place.
        assert_eq!(
            store.field_source_map().get("wind_mph").map(String::as_str),
            Some("Tempest"),
            "live field ownership reflects the hot-reloaded override"
        );

        // And removing the override again (another PUT) hands wind back to
        // priority on the SAME store -- still no restart.
        let cfg2 = cfg_two_live_sources(60, 90, &[]);
        apply_runtime_config(&h, Some(&cfg1), &cfg2);
        store.apply_source_fields(&[(F::WindMph, 8.0)], 1_040, true, "Tempest");
        store.apply_source_fields(&[(F::WindMph, 40.0)], 1_050, true, "ecowitt");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            40.0,
            "clearing the override returns wind to the higher-priority source live"
        );
    }

    // ── field_chain_map translation (id -> ORDERED writer labels) ─────────────

    /// A config with an MRMS + NWS + Open-Meteo source, for chain-translation
    /// tests. All three are cloud sources keyed by their config id (no Tempest
    /// relabel), so the chain labels equal the ids one-for-one.
    fn cfg_three_clouds() -> Config {
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5; // US -> region priorities apply
        cfg.deployment.location.lon = -81.4;
        cfg.sources.push(SourceEntry {
            id: "noaa_mrms".into(),
            priority: 75,
            max_age_s: None,
            enabled: true,
            source: SourceKind::NoaaMrms(Default::default()),
        });
        cfg.sources.push(forecast_entry("nws", 70, true));
        cfg.sources.push(forecast_entry("open_meteo", 50, true));
        cfg
    }

    #[test]
    fn field_chain_map_translates_ordered_ids_to_writer_labels() {
        // An ordered chain [noaa_mrms, nws, open_meteo] for rain translates to the
        // SAME ordered list of writer labels (these clouds are keyed by id), under
        // the snapshot-field owner key the arbiter tracks (rain_today_in).
        let mut cfg = cfg_three_clouds();
        cfg.field_source_chains.insert(
            "rain_today_in".to_string(),
            vec![
                "noaa_mrms".to_string(),
                "nws".to_string(),
                "open_meteo".to_string(),
            ],
        );
        let map = field_chain_map(&cfg);
        assert_eq!(
            map.get("rain_in_today"),
            Some(&vec![
                "noaa_mrms".to_string(),
                "nws".to_string(),
                "open_meteo".to_string(),
            ]),
            "the chain preserves ORDER and maps the field name to its owner key"
        );
    }

    #[test]
    fn field_chain_map_resolves_tempest_id_to_its_writer_label() {
        // A Tempest UDP source writes under the TEMPEST_LABEL constant, not its id,
        // so a chain entry naming the Tempest id must translate to "Tempest"
        // (mirrors field_override_map).
        let mut cfg = cfg_two_live_sources(60, 90, &[]);
        cfg.field_source_chains.insert(
            "wind_mph".to_string(),
            vec!["tempest".to_string(), "ecowitt".to_string()],
        );
        let map = field_chain_map(&cfg);
        assert_eq!(
            map.get("wind_avg_mph"),
            Some(&vec![
                crate::tempest::state::TEMPEST_LABEL.to_string(),
                "ecowitt".to_string(),
            ]),
            "the Tempest id resolves to its writer label; order is preserved"
        );
    }

    #[test]
    fn field_chain_map_drops_dead_ids_and_omits_all_dead_chains() {
        // A chain entry pointing at a disabled/unknown source is dropped (never an
        // error); a chain that ends up entirely dead is omitted so it can never
        // blank the field (it must fall through to the priority merge).
        let mut cfg = cfg_three_clouds();
        cfg.sources[1].enabled = false; // disable nws
        cfg.field_source_chains.insert(
            "rain_today_in".to_string(),
            vec![
                "noaa_mrms".to_string(),
                "nws".to_string(),   // disabled -> dropped
                "ghost".to_string(), // unknown -> dropped
                "open_meteo".to_string(),
            ],
        );
        // An all-dead chain for a second field is omitted entirely.
        cfg.field_source_chains.insert(
            "wind_mph".to_string(),
            vec!["ghost".to_string(), "nws".to_string()],
        );
        let map = field_chain_map(&cfg);
        assert_eq!(
            map.get("rain_in_today"),
            Some(&vec!["noaa_mrms".to_string(), "open_meteo".to_string()]),
            "dead entries are dropped but the live order is preserved"
        );
        assert!(
            !map.contains_key("wind_avg_mph"),
            "an all-dead chain is omitted so it can never blank the field"
        );
    }

    #[test]
    fn field_chain_map_empty_config_is_empty() {
        // The additive contract: no chains configured -> an empty map -> the
        // priority merge is unchanged.
        let cfg = cfg_three_clouds();
        assert!(
            field_chain_map(&cfg).is_empty(),
            "no field_source_chains yields an empty chain map"
        );
    }

    #[test]
    fn hot_reload_field_chain_changes_live_merge_without_respawn() {
        // An ordered chain re-applied via apply_runtime_config (the PUT path)
        // installs on the LIVE shared store: pin wind to the LOWER-priority Tempest
        // as a 2-element chain [tempest, ecowitt] and the live merge follows it,
        // with NO re-spawn.
        use WeatherField as F;
        let cfg0 = cfg_two_live_sources(60, 90, &[]);
        let h = handles_with(&cfg0);
        let store = h.tempest_store.clone();
        // Baseline: no chain -> higher-priority Ecowitt owns wind.
        store.apply_source_fields(&[(F::WindMph, 5.0)], 1_000, true, "Tempest");
        store.apply_source_fields(&[(F::WindMph, 22.0)], 1_010, true, "ecowitt");
        assert_eq!(store.snapshot().wind_avg_mph, 22.0);

        // HOT RELOAD a chain [tempest, ecowitt] for wind.
        let mut cfg1 = cfg_two_live_sources(60, 90, &[]);
        cfg1.field_source_chains.insert(
            "wind_mph".to_string(),
            vec!["tempest".to_string(), "ecowitt".to_string()],
        );
        let outcome = apply_runtime_config(&h, Some(&cfg0), &cfg1);
        assert!(
            !outcome.restart_required,
            "a pure field-chain change must not require a restart: {:?}",
            outcome.restart_reasons
        );
        // The primary (Tempest) now owns wind on the live merge.
        store.apply_source_fields(&[(F::WindMph, 7.0)], 1_020, true, "Tempest");
        store.apply_source_fields(&[(F::WindMph, 30.0)], 1_030, true, "ecowitt");
        assert_eq!(
            store.snapshot().wind_avg_mph,
            7.0,
            "post-reload: the chain primary Tempest owns wind on the LIVE merge"
        );
    }

    #[test]
    fn hot_reload_swaps_watering_policy_for_the_refresher_to_read() {
        // (b) The handle the refresher loads() each tick carries the swapped
        // WateringPolicy after a re-apply: a changed skip threshold is visible to
        // the next evaluation with no re-spawn. (The refresher loop does exactly
        // this load() per tick; see spawn_refresher.)
        let mut cfg0 = Config::default();
        cfg0.engine.skip_rules.rain_skip_in = 0.25;
        cfg0.engine.seasonal_adjust_pct = 100;
        let h = handles_with(&cfg0);

        // Boot value the refresher would read on its first tick.
        assert_eq!(
            h.watering_policy.load().skip_rules.rain_skip_in,
            0.25,
            "booted policy carries the configured skip threshold"
        );

        // HOT RELOAD: raise the rain-skip threshold (the engine skip-rule param
        // the refresher uses each tick) via the same re-apply the PUT path calls.
        let mut cfg1 = cfg0.clone();
        cfg1.engine.skip_rules.rain_skip_in = 0.80;
        cfg1.engine.seasonal_adjust_pct = 120;
        let outcome = apply_runtime_config(&h, Some(&cfg0), &cfg1);
        assert!(
            !outcome.restart_required,
            "changing skip thresholds / seasonal dial must hot-reload, not restart: {:?}",
            outcome.restart_reasons
        );

        // The NEXT load() (what the refresher does on its next tick) sees the new
        // threshold without re-spawning the task.
        let live = h.watering_policy.load();
        assert_eq!(
            live.skip_rules.rain_skip_in, 0.80,
            "the refresher's per-tick load() reads the hot-reloaded skip threshold"
        );
        assert_eq!(
            live.seasonal_adjust_pct, 120,
            "the seasonal dial hot-reloads too"
        );
    }

    #[test]
    fn hot_reload_swaps_forecast_priority_for_the_bridge() {
        // The forecast bridge reads the priority handle on every emit; a
        // re-applied forecast_provider pin must re-rank it live.
        let mut cfg0 = Config::default();
        cfg0.sources.push(forecast_entry("open_meteo", 50, true));
        cfg0.sources.push(forecast_entry("nws", 60, true));
        let h = handles_with(&cfg0);
        // No pin: nws(60) leads on its own priority.
        let m = h.forecast_priority.load();
        assert_eq!(m.get("nws"), Some(&60));
        assert_eq!(m.get("open_meteo"), Some(&50));
        drop(m);

        // HOT RELOAD: pin open_meteo. It must be bumped to a strictly-winning
        // priority on the LIVE handle the bridge reads.
        let mut cfg1 = cfg0.clone();
        cfg1.forecast_provider = Some("open_meteo".into());
        let outcome = apply_runtime_config(&h, Some(&cfg0), &cfg1);
        assert!(
            !outcome.restart_required,
            "re-pinning an existing forecast source must hot-reload: {:?}",
            outcome.restart_reasons
        );
        let m = h.forecast_priority.load();
        assert!(
            m.get("open_meteo").copied().unwrap_or(0) > m.get("nws").copied().unwrap_or(0),
            "the hot-reloaded pin makes the chosen forecast source win on the live handle"
        );
    }

    #[test]
    fn hot_reload_swaps_manual_schedules_for_the_dispatcher_to_read() {
        // The manual dispatcher load_full()s this handle at the top of every tick.
        // A re-applied config must swap the new schedule set in live (no restart),
        // including adding the FIRST schedule to a previously-EMPTY config, and it
        // must NOT flag restart_required (manual_schedules hot-reloads).
        use crate::config::schema::{ManualMode, ManualSchedule};

        let sched = |id: &str, zone: &str| ManualSchedule {
            id: id.into(),
            name: id.into(),
            zone_slug: zone.into(),
            enabled: true,
            weekdays: vec![3],
            start_hour: 5,
            start_minute: 0,
            duration_minutes: 30,
            mode: ManualMode::Override,
        };

        // Boot config has NO schedules: the dispatcher's load reads an empty set.
        let cfg0 = Config::default();
        let h = handles_with(&cfg0);
        assert!(
            h.manual_schedules.load().is_empty(),
            "previously-empty config: the dispatcher loads an empty schedule set"
        );

        // HOT RELOAD: add the FIRST schedule via the same re-apply the PUT /
        // wizard-apply paths call.
        let mut cfg1 = cfg0.clone();
        cfg1.manual_schedules = vec![sched("a", "back_yard")];
        let outcome = apply_runtime_config(&h, Some(&cfg0), &cfg1);
        assert!(
            !outcome.restart_required,
            "adding/editing a manual schedule must hot-reload, not restart: {:?}",
            outcome.restart_reasons
        );

        // The dispatcher's next-tick load() now reads the swapped-in first schedule.
        let live = h.manual_schedules.load();
        assert_eq!(live.len(), 1, "the first schedule is live on the next tick");
        assert_eq!(live[0].id, "a");
        assert_eq!(live[0].zone_slug, "back_yard");
        drop(live);

        // Editing the set (second apply) is likewise picked up live.
        let mut cfg2 = cfg1.clone();
        cfg2.manual_schedules = vec![sched("a", "front_yard"), sched("b", "side_yard")];
        apply_runtime_config(&h, Some(&cfg1), &cfg2);
        let live = h.manual_schedules.load();
        assert_eq!(live.len(), 2, "an edit re-swaps the live schedule set");
        assert_eq!(live[0].zone_slug, "front_yard");
    }

    #[test]
    fn restart_required_true_when_a_source_connection_is_added() {
        // Adding a brand-new source connection cannot hot-reload (its adapter
        // task is spawned at boot): the apply re-loads the tunables AND flags
        // restart_required so the Wave-2 UI shows a restart banner.
        let cfg0 = cfg_two_live_sources(60, 90, &[]);
        let h = handles_with(&cfg0);

        // New config adds a third source.
        let mut cfg1 = cfg0.clone();
        cfg1.sources.push(forecast_entry("nws", 70, true));
        let outcome = apply_runtime_config(&h, Some(&cfg0), &cfg1);
        assert!(
            outcome.restart_required,
            "adding a source connection must flag restart_required"
        );
        assert!(
            outcome.restart_reasons.iter().any(|r| r.contains("source")),
            "the reason names the source change: {:?}",
            outcome.restart_reasons
        );
    }

    #[test]
    fn restart_required_false_for_pure_tunable_change() {
        // A change that only touches hot-reloadable tunables (priority + an
        // override) must NOT flag restart_required even though the source set is
        // otherwise identical.
        let cfg0 = cfg_two_live_sources(60, 90, &[]);
        let h = handles_with(&cfg0);
        // Same sources, only the priorities + an override changed.
        let cfg1 = cfg_two_live_sources(95, 90, &[("wind_mph", "tempest")]);
        let outcome = apply_runtime_config(&h, Some(&cfg0), &cfg1);
        assert!(
            !outcome.restart_required,
            "re-ranking + pinning existing sources is a pure hot-reload: {:?}",
            outcome.restart_reasons
        );
    }
}
