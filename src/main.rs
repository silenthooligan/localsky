// SSR binary entry, boots the Tempest UDP listener, then runs the Axum
// HTTP server. The shared TempestStore is created here, populated by the
// listener, and read by the API endpoints + the SSR pass for the Leptos
// app (via `provide_context`).

// THIS is the recursion_limit the release build actually needs. The
// overflow ("queries overflow the depth limit!") happens while compiling
// the `localsky` BINARY, where leptos_axum's generate_route_list +
// LeptosRoutes + the SSR shell monomorphize the whole component tree in
// one place. recursion_limit is per-crate, so the copy in lib.rs does NOT
// cover this crate root, the bin needs its own. (Three release builds
// failed before this was caught, because the attribute was only in lib.rs.)
// Compile-time query budget only, no runtime cost.
#![recursion_limit = "512"]
// Lint baseline: stylistic clippy classes the codebase predates. CI
// runs -D warnings; these allows keep that gate meaningful for new
// warning classes while the baseline is burned down over time.
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(clippy::unused_unit)]
#![allow(clippy::unit_arg)]
#![allow(clippy::manual_clamp)]

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use axum::http::{header, HeaderValue};
    use axum::routing::get;
    use axum::Router;
    use leptos::prelude::*;
    use leptos_axum::{generate_route_list, LeptosRoutes};
    use localsky::{
        api,
        app::{shell, App},
        config::{wizard::WizardStore, FileConfigStore},
        forecast::{spawn_forecast_refresher, ForecastStore},
        ha::{spawn_refresher, IrrigationStore},
        history::HistoryDb,
        llm::AdvisorState,
        ports::config_store::ConfigStore,
        push, runtime_helpers, sw,
        tempest::{listener::spawn_listener, state::TempestStore},
    };
    use std::sync::Arc;
    use tower_http::set_header::SetResponseHeaderLayer;
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let tempest_store = Arc::new(TempestStore::new());

    // Demo mode short-circuits the live data path. When set, the demo
    // feeder writes synthetic weather + irrigation + forecast snapshots
    // into the stores; the legacy listener + refreshers are not spawned.
    // Useful for screenshots, public demos, and CI smoke tests.
    let demo_mode = std::env::var("LOCALSKY_DEMO").ok().as_deref() == Some("1");
    if !demo_mode {
        spawn_listener(tempest_store.clone());
    }

    // SQLite-backed run history. Optional, if /data isn't mounted
    // or the file can't be opened, we log and run without history
    // (the rest of the irrigation page works fine).
    let history_path =
        std::env::var("HISTORY_DB_PATH").unwrap_or_else(|_| "/data/irrigation.db".to_string());
    // Stable per-install identity (mDNS TXT uuid + HACS unique_id),
    // persisted next to the DB so it survives config restores.
    localsky::instance::init(
        std::path::Path::new(&history_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("/data")),
    );
    // Staged restore swap: POST /api/v1/backup/restore writes the
    // uploaded DB to <db>.restore; the swap happens here, before
    // anything opens the live file. The helper also drops the old
    // -wal/-shm siblings so SQLite cannot replay the previous
    // database's WAL into the restored file.
    match api::backup::apply_staged_restore(&history_path) {
        Ok(Some(aside)) => tracing::info!(kept = %aside, "restored database swapped in"),
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "staged DB restore failed; keeping current"),
    }
    let history_db = match HistoryDb::open(history_path.clone().into()) {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!(
                "history db open failed at {history_path:?}: {e:#}; running without persistence"
            );
            None
        }
    };
    let history_conn = history_db.as_ref().map(|db| db.handle());

    // Sample the live Tempest snapshot into sensor_history so the Weather
    // home telemetry strip has real trend sparklines (and /api/health has
    // freshness). Spawned in demo too, the demo feeder fills the store.
    if let Some(hc) = history_conn.clone() {
        localsky::persistence::spawn_weather_sampler(hc, tempest_store.clone());
    }

    // Restart resilience for the rain-today accumulator: Tempest UDP
    // packets carry per-minute deltas, so a mid-storm restart would zero
    // the daily total the skip rules read. Seed it from today's persisted
    // MAX(rain_today_in) (recorded by the weather sampler above).
    // Best-effort and non-blocking: any failure just leaves the
    // accumulator to rebuild from live packets.
    if !demo_mode {
        if let Some(hc) = history_conn.clone() {
            let seed_store = tempest_store.clone();
            tokio::spawn(async move {
                use chrono::TimeZone;
                let hist = localsky::persistence::SensorHistoryStore::new(hc);
                let now_epoch = chrono::Utc::now().timestamp();
                let Some(from) = chrono::Local::now()
                    .date_naive()
                    .and_hms_opt(0, 0, 0)
                    .and_then(|d| chrono::Local.from_local_datetime(&d).single())
                    .map(|m| m.timestamp())
                else {
                    return;
                };
                // series() filters by key only; a local day of per-minute
                // samples is ~1440 rows, so 5000 leaves ample headroom.
                match hist
                    .series("rain_today_in".to_string(), from, now_epoch + 1, 5000)
                    .await
                {
                    Ok(rows) => {
                        let best = rows
                            .into_iter()
                            .filter(|r| r.source_id == "tempest")
                            .max_by(|a, b| {
                                a.value
                                    .partial_cmp(&b.value)
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                        if let Some(r) = best {
                            if seed_store.seed_rain_today(r.value, r.epoch) {
                                tracing::info!(
                                    rain_in = r.value,
                                    epoch = r.epoch,
                                    "seeded rain-today accumulator from sensor history"
                                );
                            }
                        }
                    }
                    // v1 schema has no sensor_history table; stay quiet.
                    Err(e) => tracing::debug!(error = %e, "rain-today seed query failed"),
                }
            });
        }
    }

    let forecast_store = Arc::new(ForecastStore::new());
    // (Refresher spawned below once the boot config is loaded, so it can
    // use the wizard-configured deployment.location.)

    // Push dispatcher. Background task that drains PushEvents from the
    // HA refresher and fans them out to subscribed PWAs via VAPID.
    // Non-fatal if VAPID env is missing, the dispatcher logs once and
    // drops every event, so the rest of the app keeps running.
    let push_dispatcher = push::spawn_dispatcher(history_conn.clone());

    // Resolve per-zone runtime info from the config file once at boot so
    // the refresher computes throughput from operator-owned config rather
    // than reading stale Smart Irrigation entity attributes. Empty map on
    // fresh installs; the refresher falls back to the rotor catalog default
    // per zone in that case.
    let boot_config_path =
        std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
    // Shared config store handle: the boot snapshot here, the settings/
    // wizard routes below, and the forecast refresher's live re-reads.
    let cfg_store = Arc::new(FileConfigStore::new(&boot_config_path));
    // Load cfg once and derive both per-zone runtime + watering policy
    // from it. Empty defaults if the toml hasn't been written yet.
    let boot_cfg = cfg_store.load().await.ok();

    // Open-Meteo forecast refresher. Coordinates come from the wizard
    // config (config first, env fallback) and are re-read from the live
    // config store each tick so a location change applies without a
    // restart. The smart-morning scheduler reads its coordinates from
    // boot_cfg separately below; only the forecast path changes here.
    if !demo_mode {
        spawn_forecast_refresher(
            forecast_store.clone(),
            boot_cfg
                .as_ref()
                .map(|c| (c.deployment.location.lat, c.deployment.location.lon)),
            Some(cfg_store.clone()),
        );
    }

    let zone_runtime = match boot_cfg.as_ref() {
        Some(cfg) => {
            let mut m = std::collections::HashMap::new();
            for (slug, z) in cfg.zones.iter() {
                let throughput = localsky::engine::effective_precip_rate_mm_hr(
                    z.sprinkler_type,
                    z.precip_rate_mm_hr,
                );
                // The refresher enumerates zones via crate::zones::configured()
                // which underscore-normalizes slugs ("back-yard" -> "back_yard");
                // mirror that here so the lookup hits.
                m.insert(
                    slug.replace('-', "_"),
                    localsky::ha::ZoneRuntime {
                        throughput_mm_hr: throughput,
                        max_duration_s: 3600,
                    },
                );
            }
            m
        }
        None => std::collections::HashMap::new(),
    };

    let watering_policy = match boot_cfg.as_ref() {
        Some(cfg) => localsky::ha::WateringPolicy {
            restrictions: cfg.engine.watering_restrictions.clone(),
            address_parity: cfg.deployment.address_parity,
            manual_schedules: cfg.manual_schedules.clone(),
            location: (cfg.deployment.location.lat, cfg.deployment.location.lon),
            // Per-zone soil config: each zone's assigned sensor + thresholds.
            // Slugs underscore-normalized to match the refresher's zone list.
            soil_zones: cfg
                .zones
                .iter()
                .map(|(slug, z)| localsky::ha::ZoneSoilCfg {
                    slug: slug.replace('-', "_"),
                    name: z.display_name.clone(),
                    soil_sensor_id: z.soil_sensor_id.clone(),
                    saturation_pct: z.saturation_pct_soil,
                    target_min_pct: z.target_min_pct_soil,
                })
                .collect(),
            condition_rules: cfg.conditions.rules.clone(),
            skip_rules: cfg.engine.skip_rules.clone(),
            // Per-zone weekly-budget config for the standalone allocator
            // (A5b). Slugs underscore-normalized to match the refresher's
            // zone list, same as soil_zones above.
            budget_zones: cfg
                .zones
                .iter()
                .map(|(slug, z)| localsky::ha::ZoneBudgetCfg {
                    slug: slug.replace('-', "_"),
                    name: z.display_name.clone(),
                    weekly_budget_in: z.weekly_budget_in,
                    sessions_per_week: z.sessions_per_week,
                })
                .collect(),
            ha_sprinkler_prefix: cfg.deployment.ha_sprinkler_prefix.clone(),
        },
        None => localsky::ha::WateringPolicy::default(),
    };

    // Compile user-defined Rhai skip rules from the boot config. Augment-
    // only: applied as a post-pass on a "run" verdict, never clearing a
    // safety gate. Deploy-time contract (recompiled on container restart).
    let user_scripts = boot_cfg
        .as_ref()
        .map(|cfg| localsky::engine::scripting::CompiledScripts::compile(&cfg.scripting.skip_rules))
        .unwrap_or_default();

    let irrigation_store = Arc::new(IrrigationStore::new());
    if !demo_mode {
        // Snapshot source: HA vs native (standalone). Auto picks native
        // only when no HA env is configured, so an existing HA deploy is
        // unaffected. Built up front because the native builder needs the
        // controller registry.
        let snapshot_source = localsky::ha::resolve_snapshot_source(
            boot_cfg
                .as_ref()
                .map(|c| c.deployment.mode)
                .unwrap_or_default(),
        );
        let runs_store = history_conn
            .as_ref()
            .map(|hc| localsky::persistence::runs::RunsStore::new(hc.clone()));
        let registry = localsky::controllers::registry::ControllerRegistry::new();
        if let (Some(cfg), Some(rs)) = (boot_cfg.as_ref(), runs_store.as_ref()) {
            registry.set(localsky::runtime::build_controllers(cfg, rs.clone()));
        } else if boot_cfg
            .as_ref()
            .map(|c| !c.controllers.is_empty())
            .unwrap_or(false)
        {
            // Controllers are configured but the persistence DB is not
            // available, so build_controllers cannot run and the registry
            // stays EMPTY: no watering (scheduled or manual) will dispatch.
            // Loud by design, a silent empty registry is a dry lawn.
            tracing::error!(
                history_db = %history_path,
                "controllers are configured but the persistence DB failed to open; the controller \
                 registry is EMPTY and NO watering (scheduled or manual) will dispatch. Fix the \
                 /data mount (HISTORY_DB_PATH) and restart."
            );
        }
        // Wire the registry + runs store into POST /api/irrigation/action
        // so manual zone Run/Stop/StopAll dispatch through the same
        // adapters the schedulers use (native installs have no HA scripts).
        localsky::api::irrigation::set_dispatch_handles(registry.clone(), runs_store.clone());

        // Shadow mode: when authoritative source is HA and shadow_native is
        // on, build the native snapshot alongside it for comparison via
        // /api/v1/irrigation/shadow/*. Never drives dispatch.
        let shadow_enabled = std::env::var("LOCALSKY_SHADOW_NATIVE").ok().as_deref() == Some("1")
            || boot_cfg
                .as_ref()
                .map(|c| c.deployment.shadow_native)
                .unwrap_or(false);
        let shadow_store =
            if shadow_enabled && snapshot_source == localsky::ha::SnapshotSource::HomeAssistant {
                let ss = Arc::new(IrrigationStore::new());
                localsky::api::irrigation::set_shadow_store(ss.clone());
                tracing::info!("shadow_native enabled: native snapshot will run alongside HA");
                Some(ss)
            } else {
                None
            };

        // Native control surface (A6): vacation pause + one-day override
        // persisted in SQLite (M0008). Read each tick by the refresher so a
        // standalone deploy can be paused; written by POST /action. Absent
        // when no persistence DB is mounted (control then falls back to
        // "no pause / auto override").
        let control_store = history_conn
            .clone()
            .map(localsky::persistence::IrrigationControlStore::new);

        spawn_refresher(
            irrigation_store.clone(),
            forecast_store.clone(),
            tempest_store.clone(),
            history_conn.clone(),
            push_dispatcher.clone(),
            zone_runtime,
            watering_policy.clone(),
            user_scripts,
            snapshot_source,
            registry.clone(),
            shadow_store,
            control_store,
        );

        // Manual schedule dispatcher + smart-morning dispatcher.
        //
        // Manual scheduler fires operator-defined weekday/time slots. No-op
        // when no schedules are configured.
        //
        // Smart-morning is the LocalSky-native replacement for IU's nightly
        // sequence: computes today's sunrise, dispatches at sunrise-15 -
        // total_sequence_length so the morning run finishes 15 min before
        // sunrise. Spawned unconditionally (the dispatcher itself checks
        // skip_check + planned_seconds; nothing fires if everything is
        // zero). Honors LOCALSKY_SMART_DRY_RUN=1 for the safety-net
        // verification window before flipping IU's master switch off.
        if let (Some(cfg), Some(runs_store)) = (boot_cfg.as_ref(), runs_store) {
            if !cfg.manual_schedules.is_empty() {
                localsky::scheduler::manual::spawn(
                    cfg.manual_schedules.clone(),
                    watering_policy.clone(),
                    registry.clone(),
                    Some(runs_store.clone()),
                );
            }
            // Optional run-history retention: prune daily when capped. The
            // default (0) keeps everything forever for long-range trends.
            let runs_retention = cfg.persistence.runs_retention_days;
            if runs_retention > 0 {
                if let Some(hc) = history_conn.clone() {
                    tokio::spawn(async move {
                        let mut tick =
                            tokio::time::interval(std::time::Duration::from_secs(86_400));
                        loop {
                            tick.tick().await;
                            let cutoff =
                                chrono::Utc::now().timestamp() - (runs_retention as i64) * 86_400;
                            match localsky::history::db::prune_older_than(hc.clone(), cutoff).await
                            {
                                Ok(n) if n > 0 => tracing::info!(rows = n, "runs retention prune"),
                                Ok(_) => {}
                                Err(e) => tracing::warn!(error = %e, "runs retention prune failed"),
                            }
                        }
                    });
                }
            }
            let dry_run = std::env::var("LOCALSKY_SMART_DRY_RUN").ok().as_deref() == Some("1");
            localsky::scheduler::smart_morning::spawn(
                irrigation_store.clone(),
                watering_policy.clone(),
                registry,
                Some(runs_store),
                (cfg.deployment.location.lat, cfg.deployment.location.lon),
                Some(std::sync::Arc::new(cfg.clone())),
                Some(push_dispatcher.clone()),
                dry_run,
            );
        }
    } else {
        localsky::demo_data::spawn(
            tempest_store.clone(),
            irrigation_store.clone(),
            forecast_store.clone(),
            history_conn.clone(),
        );
        tracing::info!("LOCALSKY_DEMO=1: live data paths disabled; demo feeder active");
    }

    let conf = get_configuration(None)
        .context("read Leptos configuration (check Cargo.toml [package.metadata.leptos] and LEPTOS_* env vars)")?;
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;
    let routes = generate_route_list(App);

    // LLM advisor: wraps an OpenAI-compatible client + a TTL cache
    // for explanations + anomalies. Lazy: never calls upstream until
    // an /explanation or /anomalies request hits.
    // Set LLM_ADVISOR_DISABLED=1 to short-circuit (tile reads "disabled").
    let advisor = AdvisorState::from_env();

    // Configured skip thresholds for POST /simulate's What-If traces, so
    // the simulator matches the production ladder rather than defaults.
    localsky::api::irrigation::set_sim_skip_params(watering_policy.skip_rules.clone());

    // Snapshot source again at router scope (the one above is scoped to the
    // refresher block). The POST /action handler uses it to route the
    // vacation pause + one-day override to local state (native) vs HA
    // helpers (HA). Pure + idempotent, so recomputing is fine.
    let router_source = localsky::ha::resolve_snapshot_source(
        boot_cfg
            .as_ref()
            .map(|c| c.deployment.mode)
            .unwrap_or_default(),
    );

    // Device topology (Phase D): derived from the configured sources +
    // controllers so /api/v1/devices shows the MA-style gateway/controller
    // view. Rebuilt on config hot-reload alongside the other registries.
    let device_registry = localsky::devices::DeviceRegistry::new();
    if let Some(cfg) = boot_cfg.as_ref() {
        device_registry.set(localsky::devices::build_devices(cfg));
    }
    // F1b: import HA devices into the registry on a background loop (no-op
    // when HA isn't configured). Makes HA's hardware appear in /devices.
    if !demo_mode {
        localsky::devices::ha_import::spawn(device_registry.clone(), 120);
    }

    // HA controller entity prefix for the POST /action handler (config-driven;
    // default "opensprinkler" so the HA path works for any operator's naming).
    let router_prefix = boot_cfg
        .as_ref()
        .map(|c| c.deployment.ha_sprinkler_prefix.clone())
        .unwrap_or_else(|| "opensprinkler".to_string());

    // Build the API router twice and mount at both /api (legacy) and
    // /api/v1 (canonical). New clients (HACS integration, third-party
    // automations) target /api/v1; the bare /api/* aliases stay until
    // we cut the legacy paths in a major release. State is Arc-shared
    // so cloning is cheap.
    let api_router = api::router(
        tempest_store.clone(),
        irrigation_store.clone(),
        forecast_store.clone(),
        advisor.clone(),
        history_conn.clone(),
        router_source,
        device_registry.clone(),
        router_prefix.clone(),
    );
    let api_router_v1 = api::router(
        tempest_store.clone(),
        irrigation_store.clone(),
        forecast_store.clone(),
        advisor,
        history_conn.clone(),
        router_source,
        device_registry.clone(),
        router_prefix.clone(),
    );

    // Settings + wizard + health + ingest routes are always mounted.
    // The wizard writes /data/localsky.toml on apply; until the file
    // exists, config endpoints return the env_compat-synthesized
    // baseline so /api/v1/health stays useful on a fresh install.
    let config_path = boot_config_path.clone();
    let draft_path = format!("{config_path}.draft");

    // Zone photos directory. Uploaded files land here and are served
    // back at /site/photos/<filename>. Configurable so a deployment
    // can point the static-serve route at e.g. an NFS share.
    let photos_dir = std::env::var("LOCALSKY_PHOTOS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/data/site/photos"));
    if let Err(e) = std::fs::create_dir_all(&photos_dir) {
        tracing::warn!(
            ?e,
            "could not create photos dir {}; uploads will fail until the path exists",
            photos_dir.display()
        );
    }
    let draft_store = Arc::new(WizardStore::new(&draft_path));

    // ----- Built-in auth -----
    // Identity store shares the history SQLite (M0009 tables). Policy is
    // hot-read from the config file every 10s; existing configs without
    // an [auth] block deserialize to mode=disabled, so nothing changes
    // until an owner account is created. The middleware is layered over
    // the WHOLE app (pages, APIs, static fallback) at the end of this fn.
    let auth_rt = history_conn.clone().map(|hc| {
        let store = localsky::auth::AuthStore::new(hc);
        let rt = std::sync::Arc::new(localsky::auth::AuthRuntime::new(store));
        rt.spawn_refresh(cfg_store.clone());
        rt
    });
    let mk_auth = |rt: std::sync::Arc<localsky::auth::AuthRuntime>| {
        api::auth::router(api::auth::AuthApiState {
            rt,
            cfg_store: cfg_store.clone(),
        })
    };

    // Receiver sources (Ecowitt local POST + generic HTTP webhook).
    // Loaded from the config file if present; absent on fresh installs.
    let (receiver_bus_tx, _rx) = tokio::sync::broadcast::channel(256);
    let (ecowitt_sources, webhook_sources) = match cfg_store.load().await {
        Ok(cfg) => runtime_helpers::build_receiver_sources(&cfg, receiver_bus_tx.clone()),
        Err(_) => {
            tracing::info!(
                config = %config_path,
                "no localsky.toml yet; receiver sources idle, /ingest/* returns 503 until the wizard writes one"
            );
            (Vec::new(), Vec::new())
        }
    };
    tracing::info!(
        config = %config_path,
        ecowitt_receivers = ecowitt_sources.len(),
        webhook_receivers = webhook_sources.len(),
        "settings/wizard/health/ingest routes mounted"
    );

    let sensor_history = match rusqlite::Connection::open(&history_path) {
        Ok(c) => Some(
            localsky::persistence::SensorHistoryStore::new(Arc::new(tokio::sync::Mutex::new(c)))
                .with_retention_days(
                    boot_cfg
                        .as_ref()
                        .map(|c| c.persistence.retention_days)
                        .unwrap_or_else(localsky::config::schema::default_retention_days),
                ),
        ),
        Err(e) => {
            tracing::warn!(
                history = %history_path,
                error = %e,
                "could not open sensor history for /api/health; freshness unavailable"
            );
            None
        }
    };

    // Ecowitt gateway pollers (Phase E1). For each configured ecowitt_gw_poll
    // source, spawn a task that polls the gateway's local /get_livedata_info
    // and writes the readings into the same sensor_history the push ingest +
    // resolve_soil_pct use. Coexists with HA's push integration. Gated on
    // !demo_mode so the demo feeder owns the data.
    if !demo_mode {
        if let Some(cfg) = boot_cfg.as_ref() {
            for entry in cfg.sources.iter().filter(|s| s.enabled) {
                if let localsky::config::schema::SourceKind::EcowittGwPoll(c) = &entry.source {
                    localsky::sources::ecowitt_gw_poll::spawn(
                        entry.id.clone(),
                        c.clone(),
                        sensor_history.clone(),
                    );
                }
            }
        }
    }

    // V2 source runtime. One recorder task consumes the shared source
    // bus and turns observations into sensor_history rows + an in-memory
    // last-seen map for /api/health, then each configured polling
    // adapter's run() loop is spawned against that bus. Receiver-POST
    // adapters (Ecowitt local, HTTP webhook) publish on the same bus
    // from /ingest/*, so one recorder covers everything. Boot-time
    // wiring: source add/remove takes a restart (same contract as the
    // ecowitt_gw_poll spawns above).
    let source_last_seen = localsky::sources::SourceLastSeen::default();
    localsky::sources::bus_recorder::spawn(
        receiver_bus_tx.clone(),
        sensor_history.clone(),
        source_last_seen.clone(),
    );
    // The watch sender must stay alive for the process lifetime:
    // dropping it closes the channel and every source's select loop
    // would spin on the closed receiver.
    let (_source_shutdown_tx, source_shutdown_rx) = tokio::sync::watch::channel(false);
    if !demo_mode {
        if let Some(cfg) = boot_cfg.as_ref() {
            let polling = localsky::runtime::build_sources(cfg);
            if !polling.is_empty() {
                tracing::info!(count = polling.len(), "spawning configured weather sources");
            }
            for source in polling {
                let bus = receiver_bus_tx.clone();
                let shut = source_shutdown_rx.clone();
                tokio::spawn(async move {
                    let id = source.id().to_string();
                    if let Err(e) = source.run(bus, shut).await {
                        tracing::warn!(source = %id, error = %e, "weather source task exited");
                    }
                });
            }
        }
    }

    let mk_health = || {
        axum::Router::new()
            .route("/", axum::routing::get(api::health::health))
            .with_state(api::health::HealthState {
                config_store: Some(cfg_store.clone()),
                sensor_history: sensor_history.clone(),
                tempest_store: Some(tempest_store.clone()),
                forecast_store: Some(forecast_store.clone()),
                irrigation_store: Some(irrigation_store.clone()),
                source_last_seen: Some(source_last_seen.clone()),
            })
    };
    let mk_ingest = || {
        api::ingest::router(api::ingest::IngestState {
            ecowitt: ecowitt_sources.clone(),
            webhooks: webhook_sources.clone(),
            sensor_history: sensor_history.clone(),
        })
    };
    let mk_config = || api::config::router(cfg_store.clone());
    let mk_location = || api::location::router(cfg_store.clone());
    let mk_wizard = || {
        api::wizard::router(api::wizard::WizardApiState {
            draft_store: draft_store.clone(),
            config_store: cfg_store.clone(),
            auth_rt: auth_rt.clone(),
            tempest_store: Some(tempest_store.clone()),
        })
    };
    let mk_photos = {
        let photos_dir = photos_dir.clone();
        move || api::photos::router(photos_dir.clone())
    };

    let app = Router::new()
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                let tempest = tempest_store.clone();
                let irrigation = irrigation_store.clone();
                let forecast = forecast_store.clone();
                let cfg = cfg_store.clone();
                move || {
                    provide_context(tempest.clone());
                    provide_context(irrigation.clone());
                    provide_context(forecast.clone());
                    provide_context(cfg.clone());
                }
            },
            {
                let opts = leptos_options.clone();
                move || shell(opts.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(shell))
        .with_state(leptos_options)
        // Service worker. Lives at the origin root so its scope is the entire
        // app, registering /sw.js scopes to /, which is what we want. The
        // handler interpolates SW_VERSION at request time so every deploy
        // forces install -> waiting -> activate and old caches get nuked.
        .route("/sw.js", get(sw::sw_js))
        // Mount the snapshot + SSE endpoints at /api/* (legacy) and
        // /api/v1/* (canonical). New clients should target /api/v1.
        // The bare /api/* paths stay until a major release cuts them so
        // the in-app radar.js + the homelab v0.1 push subscribers don't
        // break across upgrade.
        .nest("/api", api_router)
        .nest("/api/v1", api_router_v1)
        // Web Push subscribe/unsubscribe + vapid-key. Aliased under
        // both /api/push and /api/v1/push using independent state.
        .nest(
            "/api/push",
            push::router(push::api::PushState {
                history_conn: history_conn.clone(),
            }),
        )
        .nest(
            "/api/v1/push",
            push::router(push::api::PushState {
                history_conn: history_conn.clone(),
            }),
        );

    // Settings + wizard + health + ingest endpoints. Mounted at both
    // /api/* (legacy) and /api/v1/* (canonical) so clients on either
    // prefix work.
    let app = app
        .nest("/api/location", mk_location())
        .nest("/api/v1/location", mk_location())
        .nest("/api/config", mk_config())
        .nest("/api/wizard", mk_wizard())
        .nest("/api/health", mk_health())
        .nest("/ingest", mk_ingest())
        .nest("/api/zones", mk_photos())
        .nest("/api/v1/config", mk_config())
        .nest("/api/v1/wizard", mk_wizard())
        .nest("/api/v1/health", mk_health())
        .nest("/api/v1/ingest", mk_ingest())
        .nest("/api/v1/zones", mk_photos())
        .nest(
            "/api/v1/backup",
            api::backup::router(api::backup::BackupApiState {
                cfg_store: cfg_store.clone(),
                db: history_conn.clone(),
                db_path: history_path.clone(),
                snapshots: history_conn
                    .clone()
                    .map(localsky::persistence::ConfigSnapshotStore::new),
            }),
        )
        .route(
            "/api/v1/updates",
            axum::routing::get(localsky::updates::updates_handler),
        )
        // Serve uploaded zone photos as static files at /site/photos/*.
        .nest_service(
            "/site/photos",
            tower_http::services::ServeDir::new(&photos_dir),
        )
        // Force revalidation on every request. Without this, browsers
        // (notably mobile Chrome) apply heuristic caching to /pkg/*.css
        // and serve a stale stylesheet from a previous deploy. With
        // no-cache + the existing Last-Modified header, the browser
        // sends If-Modified-Since each visit; the server replies 304
        // when unchanged so the bytes are still cached, just always
        // verified fresh.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ));

    // Mount the auth API + layer the enforcement middleware over the
    // complete router so it sees pages, APIs, and the static fallback.
    // No history DB = no identity store; auth stays structurally off.
    let auth_rt_for_mdns = auth_rt.clone();
    let app = if let Some(rt) = auth_rt {
        app.nest("/api/auth", mk_auth(rt.clone()))
            .nest("/api/v1/auth", mk_auth(rt.clone()))
            .layer(axum::middleware::from_fn_with_state(
                rt,
                localsky::auth::middleware::enforce,
            ))
    } else {
        tracing::warn!("no history DB; built-in auth unavailable (mode stays disabled)");
        app
    };

    // Opt-in update check ([updates].check_enabled).
    if boot_cfg
        .as_ref()
        .map(|c| c.updates.check_enabled)
        .unwrap_or(false)
    {
        localsky::updates::spawn();
    }

    // mDNS announce so the HACS zeroconf step + LAN clients find this
    // instance. Config-gated (network.mdns_enabled, default on);
    // skipped in demo mode.
    let mdns_enabled = boot_cfg
        .as_ref()
        .map(|c| c.network.mdns_enabled)
        .unwrap_or(true);
    if mdns_enabled && !demo_mode {
        localsky::network::mdns::spawn(addr.port(), auth_rt_for_mdns);
    }

    tracing::info!("localsky listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}: is another service holding this port?"))?;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .context("axum serve loop exited unexpectedly")?;
    Ok(())
}

#[cfg(not(feature = "ssr"))]
pub fn main() {
    // The WASM client is built via `lib.rs::hydrate`; this stub is here so
    // the same binary target compiles cleanly with the `hydrate` feature.
}
