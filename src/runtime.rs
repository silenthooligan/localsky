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
use crate::config::schema::{Config, ControllerKind, LlmProviderKind, SourceKind};
use crate::config::FileConfigStore;
use crate::controllers::{
    Bhyve, ControllerRegistry, DryRunController, HaServiceCall, Hydrawise, MqttCommand,
    OpenSprinklerDirect, Rachio, Rainbird,
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
    MetNorway, MqttSubscribe, Netatmo, Nws, OpenWeather, PirateWeather, SourceRegistry, TempestWs,
    TuyaCloud, Yolink,
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
            // EcowittLocal + HttpWebhook are constructed in
            // build_receiver_sources() because their POST handlers
            // emit observations on each incoming request rather than
            // through the run-loop pattern. main.rs mounts the Axum
            // routes against those instances.
            SourceKind::EcowittLocal(_) | SourceKind::HttpWebhook(_) => None,
            // EcowittGwPoll is a standalone sensor_history poller (not a
            // WeatherSource); main.rs spawns it directly. Skip here.
            SourceKind::EcowittGwPoll(_) => None,
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
/// No RunsStore and an empty zone map — these endpoints only need device
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
        assert!(
            cfg.sources.iter().any(|s| s.id == "tempest_lan"),
            "env_compat should synthesize tempest_lan in config"
        );
        rt.signal_shutdown();
    }
}
