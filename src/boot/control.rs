// Boot phase 5: the engine and the valves.
//
// The controller registry (reconciled closed before anything can
// dispatch), the deadline reaper, the refresher that produces every
// live snapshot, its watchdog, the MQTT publisher, and the two
// schedulers. In demo mode none of that runs: the feeder writes
// synthetic snapshots and the registry stays empty, so every dispatch
// surface answers 503 rather than pretending.

use std::sync::Arc;

use crate::controllers::registry::ControllerRegistry;
use crate::persistence::{ActiveRunsStore, IrrigationControlStore, RunsStore};
use crate::refresher::{resolve_snapshot_source, SnapshotSource};
use crate::tuning::TuningHandles;

use super::config::BootConfig;
use super::storage::Storage;
use super::stores::Stores;

/// What the control phase yields: the dispatch plumbing every zone
/// action goes through, and the report-generation handles.
pub struct Control {
    /// Where the irrigation snapshot comes from: LocalSky's own
    /// controllers, or Home Assistant. Pure and idempotent on the config.
    pub snapshot_source: SnapshotSource,
    /// Hot-swappable; the same instance the schedulers dispatch through.
    /// Empty in demo mode and when the persistence DB failed to open.
    pub registry: ControllerRegistry,
    pub runs: Option<RunsStore>,
    /// The commanded-valve deadline ledger, shared by dispatch (arms on
    /// Run) and the reaper (enforces past-deadline shutoff).
    pub active_runs: Option<ActiveRunsStore>,
    /// Everything the tuning report needs, in both live and demo postures
    /// whenever a history DB is mounted; None means the endpoints and the
    /// weekly notifier answer "requires the history database".
    pub tuning: Option<Arc<TuningHandles>>,
}

pub async fn start(storage: &Storage, config: &BootConfig, stores: &Stores) -> Control {
    let snapshot_source = resolve_snapshot_source(
        config
            .cfg
            .as_ref()
            .map(|c| c.deployment.mode)
            .unwrap_or_default(),
    );
    let tuning = storage.history_conn.clone().map(|hc| {
        Arc::new(TuningHandles {
            history_conn: hc,
            cfg_store: config.store.clone(),
            irrigation: stores.irrigation.clone(),
            forecast: stores.forecast.clone(),
            location: config.location(),
        })
    });
    let registry = ControllerRegistry::new();

    if storage.demo_mode {
        crate::demo_data::spawn(
            stores.tempest.clone(),
            stores.irrigation.clone(),
            stores.forecast.clone(),
            storage.history_conn.clone(),
        );
        tracing::info!("LOCALSKY_DEMO=1: live data paths disabled; demo feeder active");
        return Control {
            snapshot_source,
            registry,
            runs: None,
            active_runs: None,
            tuning,
        };
    }

    let runs = storage
        .history_conn
        .as_ref()
        .map(|hc| RunsStore::new(hc.clone()));
    let active_runs = storage
        .history_conn
        .as_ref()
        .map(|hc| ActiveRunsStore::new(hc.clone()));

    reconcile_controllers(
        storage,
        config,
        &registry,
        runs.as_ref(),
        active_runs.as_ref(),
    )
    .await;

    // The deadline reaper enforces the ledger's shutoff deadlines
    // independent of any controller's own in-process timer, so a valve
    // cannot stay open past its deadline while the process is alive; boot
    // reconcile plus the watchdog restart cover the process-death case.
    if let Some(ar) = active_runs.as_ref() {
        crate::controllers::reaper::spawn_run_reaper(
            ar.clone(),
            registry.clone(),
            runs.clone(),
            Some(stores.push.clone()),
        );
    }

    // Active zone list for the refresher: config zones when the file
    // exists (the wizard is the source of truth); empty on a fresh install.
    let zones = match config.cfg.as_ref() {
        Some(cfg) => crate::zones::from_pairs(
            cfg.zones
                .iter()
                .map(|(slug, z)| (slug.as_str(), z.display_name.as_str())),
        ),
        None => Vec::new(),
    };
    // Vacation pause, Rain delay and the sticky overrides, persisted in
    // SQLite and read each tick by BOTH unattended dispatch paths (the
    // refresher below and the manual scheduler); None without a persistence
    // DB (no pause / auto override).
    //
    // Both readers fail CLOSED on a read they cannot complete: the refresher
    // holds the last state it saw, the manual scheduler holds every schedule
    // due that tick. A hold the database can dissolve by being busy is not a
    // hold. A missing store is a different fact -- with no DB there is no
    // surface to set a hold on -- and stays "nothing set".
    let control_store = storage
        .history_conn
        .clone()
        .map(crate::persistence::IrrigationControlStore::new);
    // User Rhai skip rules. Augment-only: a post-pass on a "run" verdict
    // that never clears a safety gate. Compiled once per boot.
    let scripts = config
        .cfg
        .as_ref()
        .map(|c| crate::engine::scripting::CompiledScripts::compile(&c.scripting.skip_rules))
        .unwrap_or_default();
    crate::refresher::spawn_refresher(
        stores.irrigation.clone(),
        stores.forecast.clone(),
        stores.tempest.clone(),
        storage.history_conn.clone(),
        stores.push.clone(),
        // The SWAPPABLE handle, not a boot clone: a hot-reloaded policy
        // (including the per-zone runtime and agronomy maps) is live on
        // the next tick.
        config.policy.clone(),
        scripts,
        snapshot_source,
        registry.clone(),
        // Cloned, not moved: the manual scheduler holds the same surface, so
        // a hold the owner sets binds BOTH unattended dispatch paths.
        control_store.clone(),
        zones,
    );
    // If the refresher's heartbeat goes stale (a panic or hang in the one
    // task that produces all live data and the verdict), force-exit so
    // restart:unless-stopped brings the process back and boot reconciliation
    // runs, instead of freezing on a stale snapshot until a human notices.
    crate::refresher::spawn_refresher_watchdog();

    spawn_mqtt_publisher(config, stores);
    spawn_schedulers(
        storage,
        config,
        stores,
        &registry,
        runs.as_ref(),
        active_runs.as_ref(),
        tuning.as_ref(),
        control_store,
    );

    Control {
        snapshot_source,
        registry,
        runs,
        active_runs,
        tuning,
    }
}

/// Populate the registry and close every zone on every controller before
/// a scheduler or the API can dispatch, so a valve left open by a crash
/// or redeploy mid-run (the MQTT path's shutoff is an in-process timer
/// that dies with the process) is closed on the next start instead of
/// staying open until a human notices. Best-effort, never fatal.
async fn reconcile_controllers(
    storage: &Storage,
    config: &BootConfig,
    registry: &ControllerRegistry,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
) {
    match (config.cfg.as_ref(), runs) {
        (Some(cfg), Some(rs)) => {
            registry.set(crate::runtime::build_controllers(cfg, Some(rs.clone())));
            let failed = crate::controllers::reaper::boot_reconcile_audited(
                registry,
                active_runs,
                Some(rs),
                chrono::Utc::now().timestamp(),
            )
            .await;
            if !failed.is_empty() {
                tracing::warn!(
                    controllers = ?failed,
                    "boot reconcile: some controllers did not confirm stop_all (unreachable at boot)"
                );
            }
        }
        (Some(cfg), None) if !cfg.controllers.is_empty() => {
            // The persistence DB failed to open, so the registry stays
            // EMPTY: no scheduled or manual watering dispatches without a
            // run history and a deadline backstop. But a valve left open
            // by a crash must still be closed: the disk fault that crashed
            // the process and the DB-open failure are one correlated
            // event. A throwaway stop-only registry closes every zone and
            // is discarded. Loud by design.
            let reconcile_registry = ControllerRegistry::new();
            reconcile_registry.set(crate::runtime::build_controllers(cfg, None));
            let failed =
                crate::controllers::reaper::boot_reconcile(&reconcile_registry, None).await;
            tracing::error!(
                history_db = %storage.history_path,
                reconcile_failed = ?failed,
                "controllers are configured but the persistence DB failed to open; the controller \
                 registry is EMPTY and NO watering (scheduled or manual) will dispatch. Any valve \
                 left open by a crash was reconciled closed best-effort at boot. Fix the /data \
                 mount (HISTORY_DB_PATH) and restart."
            );
        }
        _ => {}
    }
}

/// Outbound Home Assistant MQTT discovery: with a broker configured, HA
/// users get auto-created sensor.localsky_* entities without LocalSky
/// reading HA. Gated on a [notifications.mqtt] block, the global
/// features.enable_mqtt_publish toggle and the broker's own
/// publish_enabled, so a no-MQTT deploy is untouched. Self-supervising.
fn spawn_mqtt_publisher(config: &BootConfig, stores: &Stores) {
    let Some(cfg) = config.cfg.as_ref() else {
        return;
    };
    if !cfg.features.enable_mqtt_publish {
        tracing::info!("ha mqtt publisher: features.enable_mqtt_publish=false; not started");
        return;
    }
    let Some(mqtt_cfg) = cfg.notifications.mqtt.as_ref() else {
        return;
    };
    if !mqtt_cfg.publish_enabled {
        tracing::info!(
            "ha mqtt publisher: [notifications.mqtt] present but publish_enabled=false; not started"
        );
        return;
    }
    crate::integrations::home_assistant::mqtt_publish::spawn(
        mqtt_cfg.clone(),
        cfg.deployment.display_name.clone(),
        stores.irrigation.subscribe(),
    );
}

/// The manual schedule dispatcher and the smart morning, plus the two
/// housekeeping tasks that share their gate (a config and a runs store).
///
/// The manual scheduler is spawned UNCONDITIONALLY so a FIRST schedule
/// added to a previously-empty config actuates on the next tick with no
/// restart: it loads the live set from the swappable handle each cycle.
/// The smart morning computes today's sunrise and dispatches so the run
/// finishes fifteen minutes before it; nothing fires if every zone's
/// plan is zero. `LOCALSKY_SMART_DRY_RUN=1` plans and logs without
/// commanding a valve.
///
/// `control` is the owner's control surface, threaded to the manual
/// scheduler so a hold binds it too. The smart morning reads the same
/// state off the snapshot the refresher publishes.
#[allow(clippy::too_many_arguments)]
fn spawn_schedulers(
    storage: &Storage,
    config: &BootConfig,
    stores: &Stores,
    registry: &ControllerRegistry,
    runs: Option<&RunsStore>,
    active_runs: Option<&ActiveRunsStore>,
    tuning: Option<&Arc<TuningHandles>>,
    control: Option<IrrigationControlStore>,
) {
    let (Some(cfg), Some(runs)) = (config.cfg.as_ref(), runs) else {
        return;
    };
    crate::scheduler::manual::spawn(
        config.manual_schedules.clone(),
        // The SWAPPABLE handle: a hot-reloaded restriction, cap or skip
        // reaches SCHEDULED valves on the next tick. A boot-frozen value
        // once let a restriction meant to BLOCK watering bypass scheduled
        // runs until a restart.
        config.policy.clone(),
        registry.clone(),
        Some(runs.clone()),
        active_runs.cloned(),
        // The same control surface the refresher reads, with the same
        // fail-closed posture on a failed read. Without it the manual scheduler
        // held nothing but the watering restrictions, so Rain delay, the
        // Vacation pause toggle and a global or per-zone Skip override stopped
        // the smart morning and left every enabled manual schedule opening
        // valves on its own clock.
        control,
        // The published snapshot, so the schedule can consult the SAFETY
        // gates the morning uses: freeze, wind, rain now, and the
        // live-data fail-safe. Without it this dispatcher had no path to
        // the weather at all and would open valves in a hard freeze.
        stores.irrigation.clone(),
        Some(stores.push.clone()),
    );
    // Optional run-history retention: prune daily when capped. The
    // default (0) keeps everything forever for long-range trends.
    let runs_retention = cfg.persistence.runs_retention_days;
    if runs_retention > 0 {
        if let Some(hc) = storage.history_conn.clone() {
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(86_400));
                loop {
                    tick.tick().await;
                    let cutoff = chrono::Utc::now().timestamp() - (runs_retention as i64) * 86_400;
                    let runs = crate::persistence::RunsStore::new(hc.clone());
                    match runs.prune_older_than(cutoff).await {
                        Ok(n) if n > 0 => tracing::info!(rows = n, "runs retention prune"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "runs retention prune failed"),
                    }
                    let verdicts = crate::persistence::VerdictHistoryStore::new(hc.clone());
                    match verdicts.prune_older_than(cutoff).await {
                        Ok(n) if n > 0 => tracing::info!(rows = n, "verdict retention prune"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "verdict retention prune failed"),
                    }
                }
            });
        }
    }
    // Weekly tuning-report notification: hourly tick, 7-local-day dedupe
    // persisted so redeploys never double-notify.
    if let (Some(hc), Some(tuning)) = (storage.history_conn.clone(), tuning) {
        crate::scheduler::tuning_report::spawn(
            crate::persistence::TuningReportStateStore::new(hc),
            stores.push.clone(),
            tuning.clone(),
        );
    }
    let dry_run = std::env::var("LOCALSKY_SMART_DRY_RUN").ok().as_deref() == Some("1");
    crate::scheduler::smart_morning::spawn(
        stores.irrigation.clone(),
        config.policy.clone(),
        registry.clone(),
        Some(runs.clone()),
        active_runs.cloned(),
        config.location(),
        Some(stores.push.clone()),
        dry_run,
    );
}
