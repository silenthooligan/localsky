// Boot phase 3: the configuration.
//
// One config store, the boot snapshot of the file it manages, and the
// hot-swappable handles the engine and the schedulers read from it.
// The staged restore, the legacy environment, the demo seed and the
// region-authority upgrade seeding all land here, so by the time this
// phase returns the file on disk and the value in memory agree and
// nothing later re-derives either.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use arc_swap::ArcSwap;

use crate::config::schema::{Config, ManualSchedule};
use crate::config::FileConfigStore;
use crate::ports::config_store::{ConfigStore, ConfigStoreError};
use crate::refresher::WateringPolicy;

use super::storage::Storage;

/// What the configuration phase yields.
pub struct BootConfig {
    /// The shared config store: the settings and wizard routes write it,
    /// the forecast refresher re-reads it live.
    pub store: Arc<FileConfigStore>,
    /// `CONFIG_PATH`, default `/data/localsky.toml`.
    pub path: String,
    /// The config as loaded at boot. None on a fresh install (the wizard
    /// runs) or when an existing file failed to load (logged loudly).
    pub cfg: Option<Config>,
    /// The engine-tunable subset, swapped by every config write so the
    /// next tick reads it with no restart.
    pub policy: Arc<ArcSwap<WateringPolicy>>,
    /// The manual schedule set the manual dispatcher loads each tick.
    pub manual_schedules: Arc<ArcSwap<Vec<ManualSchedule>>>,
    /// Per-source forecast priority, read by the forecast bridge on every
    /// emit; populated by the sources phase and by every config write.
    pub forecast_priority: Arc<ArcSwap<HashMap<String, i32>>>,
}

impl BootConfig {
    /// The deployment location, or the origin when unconfigured.
    pub fn location(&self) -> (f64, f64) {
        self.cfg
            .as_ref()
            .map(|c| (c.deployment.location.lat, c.deployment.location.lon))
            .unwrap_or((0.0, 0.0))
    }
}

pub async fn load(storage: &Storage) -> anyhow::Result<BootConfig> {
    let path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
    let store = Arc::new(FileConfigStore::new(&path));
    // Storage already activated the complete verified restore. Keep its
    // Applying marker if loading/migrating that config fails; do not turn a
    // failed restore into a running, partially configured instance.
    let cfg = if storage.restore.is_some() && store.path().try_exists()? {
        Some(store.load().await.context(
            "restored configuration could not load; startup refused, restore marker and recovery files retained",
        )?)
    } else {
        load_or_synthesize(&store, &path).await
    };
    let cfg = seed_demo(storage, &store, cfg).await;
    let cfg = seed_region_authorities(storage, &store, cfg).await;

    // The deployment timezone, resolved once before the schedulers spawn,
    // so wall-clock firing and day-rollover dedupe key off the configured
    // zone rather than the container's TZ.
    if let Some(cfg) = cfg.as_ref() {
        crate::timeutil::set_configured_tz(cfg);
    }

    // The engine-tunable subset, derived via the single `from_config`
    // builder the hot-reload path also uses, so a reloaded policy is
    // byte-identical to a boot policy for the same config.
    let policy = match cfg.as_ref() {
        Some(c) => WateringPolicy::from_config(c).with_ledger(&store.ledger()),
        None => WateringPolicy::default(),
    };
    let policy = Arc::new(ArcSwap::from_pointee(policy));
    let manual_schedules = Arc::new(ArcSwap::from_pointee(
        cfg.as_ref()
            .map(|c| c.manual_schedules.clone())
            .unwrap_or_default(),
    ));
    let forecast_priority = Arc::new(ArcSwap::from_pointee(HashMap::new()));

    Ok(BootConfig {
        store,
        path,
        cfg,
        policy,
        manual_schedules,
        forecast_priority,
    })
}

/// The config file, or the one synthesized from a legacy v0.1
/// environment (written down ONCE and never read from the environment
/// again). A genuinely fresh install boots into the wizard; a file that
/// exists but fails to load boots unconfigured and says so loudly.
async fn load_or_synthesize(store: &FileConfigStore, path: &str) -> Option<Config> {
    match store.load().await {
        Ok(cfg) => Some(cfg),
        Err(ConfigStoreError::NotFound) => {
            if !crate::config::env_compat::legacy_env_present() {
                return None;
            }
            let (synthesized, ledger) = crate::config::env_compat::synthesize();
            let saved = match store.save(&synthesized).await {
                Ok(_) => store.update_ledger(|l| *l = ledger).await.map(|_| ()),
                Err(e) => Err(e),
            };
            match saved {
                Ok(()) => {
                    tracing::info!(
                        config_path = %path,
                        "wrote localsky.toml from the legacy environment variables; the \
                         file is the configuration from here on and the environment is \
                         not read again"
                    );
                    Some(synthesized)
                }
                Err(e) => {
                    tracing::error!(
                        error = %e.diagnostic(),
                        "could not write the config synthesized from the environment; \
                         booting unconfigured"
                    );
                    None
                }
            }
        }
        Err(e) => {
            // The file EXISTS but failed to load: a TOML syntax error, an
            // unset ${VAR} interpolation, a hard validation failure, or a
            // source/controller kind written by a NEWER binary (an image
            // rollback). Booting as a fresh install here would disable ALL
            // irrigation and invite re-running the wizard over a
            // recoverable file, so the instance comes up degraded and
            // wizard-empty instead, with rollback and backup-restore still
            // reachable.
            tracing::error!(
                error = %e.diagnostic(),
                config_path = %path,
                "FAILED to load an existing localsky config; booting UNCONFIGURED \
                 (no zones, no controllers, no scheduled watering). This is NOT a \
                 fresh install: fix the config file (or roll back the image) before \
                 re-running the setup wizard."
            );
            None
        }
    }
}

/// `LOCALSKY_DEMO=1` on an empty volume: seed a synthetic config (four
/// zones keyed to the slugs the feeder emits, a dry-run controller, a
/// spread of sources) so the demo shows the whole product rather than a
/// weather-only shell. Only when no controllers and no zones are
/// configured, so a persisted demo volume, and any working install, is
/// left alone. A direct save, so the demo read-only middleware never
/// sees it.
async fn seed_demo(
    storage: &Storage,
    store: &FileConfigStore,
    cfg: Option<Config>,
) -> Option<Config> {
    let empty = cfg
        .as_ref()
        .map(|c| c.controllers.is_empty() && c.zones.is_empty())
        .unwrap_or(true);
    if !(storage.demo_mode && empty) {
        return cfg;
    }
    let synthetic = crate::demo_data::seed_config();
    match store.save(&synthetic).await {
        Ok(_) => {
            tracing::info!(
                "LOCALSKY_DEMO=1: seeded synthetic demo config (4 zones, 4 sources, dry-run controller)"
            );
            Some(synthetic)
        }
        Err(e) => {
            tracing::warn!(
                "LOCALSKY_DEMO=1: demo config seed failed ({e}); continuing weather-only"
            );
            cfg
        }
    }
}

/// Upgrade seeding for configured installs that predate the region
/// keyless forecast authorities: append NWS (US) / Met.no (Nordics)
/// once, record the ids in the ledger so a user deletion sticks, lift a
/// region authority still at the flat default priority to its
/// researched rank, persist, and boot from the updated config. Demo
/// mode is excluded (its synthetic config owns the source list).
async fn seed_region_authorities(
    storage: &Storage,
    store: &FileConfigStore,
    cfg: Option<Config>,
) -> Option<Config> {
    let mut cfg = cfg?;
    if storage.demo_mode {
        return Some(cfg);
    }
    let mut ledger = store.ledger();
    let (appended, changed) =
        crate::config::region::seed_missing_forecast_authorities(&mut cfg, &mut ledger);
    for line in &appended {
        tracing::info!("{line}");
    }
    let (repaired, repaired_changed) =
        crate::config::region::repair_flat_default_priorities(&mut cfg, &mut ledger);
    for line in &repaired {
        tracing::info!("{line}");
    }
    if changed || repaired_changed {
        let persisted = match store.save(&cfg).await {
            Ok(_) => store.update_ledger(|l| *l = ledger).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = persisted {
            tracing::warn!(
                error = %e.diagnostic(),
                "failed to persist region authority seeding; sources still \
                 active this boot, will retry next boot"
            );
        }
    }
    Some(cfg)
}
