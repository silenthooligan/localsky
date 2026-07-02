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

/// P4-1: Prometheus exposition endpoint. Public (auth-exempt in middleware);
/// renders the process-global metrics registry.
#[cfg(feature = "ssr")]
async fn metrics_handler() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        localsky::metrics::render(),
    )
}

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use axum::http::{header, HeaderName, HeaderValue};
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

    // Sample the live snapshot into sensor_history so the Weather home
    // telemetry strip has real trend sparklines. Only records when the off-bus
    // Tempest path (or the "Demo" feeder) owns the snapshot; bus sources are
    // persisted by the bus recorder under their own id, so the sampler skips
    // them to avoid double-recording. Spawned in demo too.
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

    // Current-conditions arbitration priorities + per-field USER overrides:
    // each source's config `priority` keyed by the writer label ("Tempest" for
    // the UDP path, the source id otherwise), and the `field_source_overrides`
    // pin map. Lets the user pick which LIVE source drives current conditions
    // (the arbiter fails over on staleness) and pin a specific field to a
    // specific source. Both are installed into the SHARED TempestStore via its
    // `&self` arc-swap setters, so the SAME builders the config hot-reload path
    // uses (runtime::source_priority_map / field_override_map) re-apply them on
    // a PUT with no restart. An empty config installs empty maps -> the priority
    // merge is byte-identical to no overrides.
    if let Some(cfg) = boot_cfg.as_ref() {
        tempest_store.set_priorities(localsky::runtime::source_priority_map(cfg));
        tempest_store.set_max_ages(localsky::runtime::source_max_age_map(cfg));
        tempest_store.set_field_overrides(localsky::runtime::field_override_map(cfg));
        tempest_store.set_field_chains(localsky::runtime::field_chain_map(cfg));
    }

    // Forecast is now source-agnostic: the Open-Meteo refresher + every other
    // forecast source emit SourceEvent::Forecast onto the bus, and the
    // forecast_bridge arbitrates them by priority into the ForecastStore. That
    // wiring lives further down (after the bus is created), alongside the
    // snapshot bridge. See "Forecast bus wiring" below.

    // Active zone list for the refresher: config zones when localsky.toml
    // exists (the wizard is the source of truth), LOCALSKY_ZONES env as the
    // no-config override, empty on a fresh unconfigured install (the UI
    // shows empty states until the wizard runs).
    let boot_zones = match boot_cfg.as_ref() {
        Some(cfg) => localsky::zones::from_pairs(
            cfg.zones
                .iter()
                .map(|(slug, z)| (slug.as_str(), z.display_name.as_str())),
        ),
        None => localsky::zones::configured(),
    };

    let zone_runtime = match boot_cfg.as_ref() {
        Some(cfg) => {
            let mut m = std::collections::HashMap::new();
            for (slug, z) in cfg.zones.iter() {
                let throughput = localsky::engine::effective_precip_rate_mm_hr(
                    z.sprinkler_type,
                    z.precip_rate_mm_hr,
                );
                // zones::from_pairs underscore-normalizes slugs
                // ("back-yard" -> "back_yard"); mirror that here so the
                // lookup hits.
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

    // P1-8c: resolve the deployment timezone once, before the schedulers spawn,
    // so wall-clock firing + day-rollover dedupe key off the configured tz rather
    // than the container TZ env.
    if let Some(cfg) = boot_cfg.as_ref() {
        localsky::timeutil::set_configured_tz(cfg);
    }

    // The engine-tunable subset of config (skip-rule thresholds, restrictions,
    // seasonal dial, manual schedules, per-zone soil/budget, units), derived via
    // the single `from_config` builder that the config hot-reload path also uses
    // (so a reloaded policy is byte-identical to a boot policy for the same
    // config). Wrapped in an Arc<ArcSwap<_>> HANDLE so a PUT /api/config can swap
    // a new policy in and the refresher picks it up on its next tick, no restart.
    let watering_policy = match boot_cfg.as_ref() {
        Some(cfg) => localsky::ha::WateringPolicy::from_config(cfg),
        None => localsky::ha::WateringPolicy::default(),
    };
    let watering_policy_handle = Arc::new(arc_swap::ArcSwap::from_pointee(watering_policy.clone()));

    // Shared manual-schedule handle. The manual dispatcher loads this live at the
    // top of every tick and the config hot-reload path swaps it, so an added or
    // edited schedule (including the FIRST one on a previously-empty config) takes
    // effect on the next tick with no restart. Wrapped in Arc<ArcSwap<_>> exactly
    // like the watering policy above.
    let manual_schedules_handle: Arc<
        arc_swap::ArcSwap<Vec<localsky::config::schema::ManualSchedule>>,
    > = Arc::new(arc_swap::ArcSwap::from_pointee(
        boot_cfg
            .as_ref()
            .map(|cfg| cfg.manual_schedules.clone())
            .unwrap_or_default(),
    ));

    // Shared forecast-priority handle (forecast provider ranking + pin). Created
    // here so the forecast bridge reads it live and the config hot-reload path
    // can swap it; populated from the boot config below alongside the bridge.
    let forecast_priority_handle: Arc<arc_swap::ArcSwap<std::collections::HashMap<String, i32>>> =
        Arc::new(arc_swap::ArcSwap::from_pointee(
            std::collections::HashMap::new(),
        ));

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
        // P0-1b: the commanded-valve deadline ledger, shared by the dispatch path
        // (arms on Run) and the reaper (enforces past-deadline shutoff).
        let active_runs_store = history_conn
            .as_ref()
            .map(|hc| localsky::persistence::ActiveRunsStore::new(hc.clone()));
        let registry = localsky::controllers::registry::ControllerRegistry::new();
        if let (Some(cfg), Some(rs)) = (boot_cfg.as_ref(), runs_store.as_ref()) {
            registry.set(localsky::runtime::build_controllers(cfg, rs.clone()));
            // P0-1: boot reconciliation. Close every zone on every controller and
            // clear the stale deadline ledger before the schedulers or the API can
            // dispatch, so a valve left open by a crash/redeploy mid-run (the MQTT
            // path's shutoff is an in-process timer that dies with the process) is
            // closed on the next start instead of staying open until a human
            // notices. Best-effort, never fatal. (See reaper::boot_reconcile.)
            let failed = localsky::controllers::reaper::boot_reconcile(
                &registry,
                active_runs_store.as_ref(),
            )
            .await;
            if !failed.is_empty() {
                tracing::warn!(
                    controllers = ?failed,
                    "boot reconcile: some controllers did not confirm stop_all (unreachable at boot)"
                );
            }
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
        localsky::api::irrigation::set_dispatch_handles(
            registry.clone(),
            runs_store.clone(),
            active_runs_store.clone(),
        );
        // P0-1b: the deadline reaper. Enforces the active-run ledger's shutoff
        // deadlines independent of any controller's own (in-process) timer, so a
        // valve cannot stay open past its deadline while the process is alive; boot
        // reconcile + the watchdog restart cover the process-death case.
        if let Some(ar) = active_runs_store.as_ref() {
            localsky::controllers::reaper::spawn_run_reaper(ar.clone(), registry.clone());
        }
        // Wire the config store + registry into GET /api/v1/sensors/inventory
        // so the unified Sensors view can resolve zone bindings, per-source
        // labels, and per-controller flow-meter capability + live GPM.
        localsky::api::sensors::set_inventory_handles(cfg_store.clone(), registry.clone());

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
            // Pass the SWAPPABLE handle (not a boot clone): the refresher loads
            // it each tick so a hot-reloaded watering policy takes effect live.
            watering_policy_handle.clone(),
            user_scripts,
            snapshot_source,
            registry.clone(),
            shadow_store,
            control_store,
            boot_zones,
        );
        // P0-8b: supervise the refresher. If its heartbeat goes stale (panic or
        // hang in the one task that produces all live data + the today verdict),
        // force-exit so restart:unless-stopped brings the process back and boot
        // reconciliation runs, instead of the system silently freezing on a stale
        // snapshot until a human notices.
        localsky::ha::spawn_refresher_watchdog();

        // Boot step 6: outbound HA MQTT discovery publisher. The HA-optional
        // bridge the release advertises: with a broker configured, HA users
        // get auto-created sensor.localsky_* / binary_sensor.localsky_*
        // entities (discovery + live state on each engine tick) without
        // LocalSky reading HA. Gated on three signals so a no-MQTT deploy is
        // untouched: a [notifications.mqtt] block exists, the global
        // features.enable_mqtt_publish toggle is on (default true), and the
        // broker's own publish_enabled is on (default true). It subscribes to
        // the same IrrigationStore snapshot signal the rest of boot uses and
        // is fully self-supervising (reconnects on eventloop error; never
        // panics boot).
        if let Some(cfg) = boot_cfg.as_ref() {
            if cfg.features.enable_mqtt_publish {
                if let Some(mqtt_cfg) = cfg.notifications.mqtt.as_ref() {
                    if mqtt_cfg.publish_enabled {
                        localsky::ha::mqtt_publish::spawn(
                            mqtt_cfg.clone(),
                            cfg.deployment.display_name.clone(),
                            irrigation_store.subscribe(),
                        );
                    } else {
                        tracing::info!(
                            "ha mqtt publisher: [notifications.mqtt] present but publish_enabled=false; not started"
                        );
                    }
                }
            } else {
                tracing::info!(
                    "ha mqtt publisher: features.enable_mqtt_publish=false; not started"
                );
            }
        }

        // Manual schedule dispatcher + smart-morning dispatcher.
        //
        // Manual scheduler fires operator-defined weekday/time slots. Spawned
        // UNCONDITIONALLY (no !is_empty() guard) so a FIRST schedule added to a
        // previously-empty config actuates on the next tick with no restart. The
        // tick loads the live schedule set from the swappable handle each cycle and
        // early-returns when it is empty, so an empty boot config costs one idle
        // task. The config hot-reload path (apply_runtime_config) swaps the handle
        // so an added/edited schedule is picked up on the next tick.
        //
        // Smart-morning is the LocalSky-native replacement for IU's nightly
        // sequence: computes today's sunrise, dispatches at sunrise-15 -
        // total_sequence_length so the morning run finishes 15 min before
        // sunrise. Spawned unconditionally (the dispatcher itself checks
        // skip_check + planned_seconds; nothing fires if everything is
        // zero). Honors LOCALSKY_SMART_DRY_RUN=1 for the safety-net
        // verification window before flipping IU's master switch off.
        if let (Some(cfg), Some(runs_store)) = (boot_cfg.as_ref(), runs_store) {
            localsky::scheduler::manual::spawn(
                manual_schedules_handle.clone(),
                // Pass the SWAPPABLE handle (not a boot clone): the dispatcher
                // load_full()s it each tick so a hot-reloaded watering restriction /
                // cap / skip reaches SCHEDULED valves on the next tick with no
                // restart, mirroring the refresher's use of the same handle above. A
                // boot-frozen value previously let a restriction meant to BLOCK
                // watering bypass scheduled runs until a container restart.
                watering_policy_handle.clone(),
                registry.clone(),
                Some(runs_store.clone()),
                // P0-1b: thread the deadline ledger so operator recurring
                // runs arm the reaper backstop (stuck-valve safety on
                // MQTT/DIY controllers), mirroring the API path.
                active_runs_store.clone(),
            );
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
                active_runs_store.clone(),
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
    // Static-site root the Leptos fallback resolves against
    // (LEPTOS_SITE_ROOT; "site" in-container -> /app/site, "target/site"
    // in dev). The bundled docs live under <site_root>/docs; capture the
    // string here because leptos_options is moved into the router state.
    let site_root = leptos_options.site_root.to_string();
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
        Ok(c) => {
            // Match the primary HistoryDb pragmas: this second handle to the same
            // file was opening raw, so it took no busy timeout and dropped reads on
            // contention. (Collapsing this onto history_db.handle() is a follow-up.)
            c.busy_timeout(std::time::Duration::from_secs(5)).ok();
            c.pragma_update(None, "journal_mode", "WAL").ok();
            c.pragma_update(None, "synchronous", "NORMAL").ok();
            Some(
                localsky::persistence::SensorHistoryStore::new(Arc::new(tokio::sync::Mutex::new(
                    c,
                )))
                .with_retention_days(
                    boot_cfg
                        .as_ref()
                        .map(|c| c.persistence.retention_days)
                        .unwrap_or_else(localsky::config::schema::default_retention_days),
                ),
            )
        }
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
                        // Publish the gateway's outdoor weather onto the bus so
                        // the snapshot bridge populates the dashboard + HA
                        // entities (an EcowittGwPoll-only deployment otherwise
                        // showed empty current conditions).
                        Some(receiver_bus_tx.clone()),
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
    // The reachability twin of source_last_seen: the bus recorder stamps it on
    // every `Reachability { reachable: true }` event the adapters publish on a
    // successful fetch. Threaded into BOTH HealthState.source_reachable and the
    // runtime handles below so /api/health and /api/config/source_catalog compute
    // the honest source-status taxonomy off the SAME reachability facts (a
    // reachable-but-quiet rain source reads `watching`, never `offline`).
    let source_reachable = localsky::sources::SourceReachability::default();
    localsky::sources::bus_recorder::spawn(
        receiver_bus_tx.clone(),
        sensor_history.clone(),
        source_last_seen.clone(),
        source_reachable.clone(),
    );
    // Blitzortung community lightning (Blitzortung.org, CC BY-SA 4.0).
    // Spawned directly like the ecowitt_gw_poll pollers because it
    // feeds the TempestStore lightning buffer (display layer only),
    // not the merge bus, and never irrigation logic. Double opt-in:
    // the entry's enabled flag AND the config's own enabled (default
    // false) must both be true before spawn() connects; frame arrivals
    // are recorded into source_last_seen so /api/health shows feed
    // liveness even while no strikes land inside the radius.
    if !demo_mode {
        if let Some(cfg) = boot_cfg.as_ref() {
            let station = (cfg.deployment.location.lat, cfg.deployment.location.lon);
            for entry in cfg.sources.iter().filter(|s| s.enabled) {
                if let localsky::config::schema::SourceKind::Blitzortung(c) = &entry.source {
                    localsky::sources::blitzortung::spawn(
                        entry.id.clone(),
                        c.clone(),
                        tempest_store.clone(),
                        station,
                        Some(source_last_seen.clone()),
                    );
                }
            }
        }
    }

    // The watch sender must stay alive for the process lifetime:
    // dropping it closes the channel and every source's select loop
    // would spin on the closed receiver.
    let (_source_shutdown_tx, source_shutdown_rx) = tokio::sync::watch::channel(false);
    // Per-source forecast priority, fed to the forecast_bridge so a configured
    // forecast source (NWS/OpenWeather/Pirate/Met.no) outranks the Open-Meteo
    // default. Built from each forecast-capable source's own priority(), with
    // the user's forecast_provider pin (if any) bumped to the winning priority.
    // See runtime::forecast_priority_map. The boot map is published into
    // `forecast_priority_handle` (created above) inside the block below; the
    // bridge reads that swappable handle so a hot-reload re-ranks it live.
    // Subscribe the forecast bridge BEFORE any forecast source spawns below, so
    // the first emit isn't dropped (broadcast delivers only to live receivers,
    // but buffers from the moment of subscribe()).
    let forecast_bridge_rx = receiver_bus_tx.subscribe();
    if !demo_mode {
        if let Some(cfg) = boot_cfg.as_ref() {
            let polling = localsky::runtime::build_sources(cfg);

            // Map every bus-publishing source to its live_current capability so
            // the snapshot bridge only lets a real live station claim
            // station-liveness (forecast sources populate display only). Built
            // from each source's own capabilities() via a dyn coercion.
            let mut source_live_current: std::collections::HashMap<String, bool> =
                std::collections::HashMap::new();
            for s in &polling {
                source_live_current.insert(s.id().to_string(), s.capabilities().live_current);
            }
            for s in &ecowitt_sources {
                let d: Arc<dyn localsky::ports::weather_source::WeatherSource> = s.clone();
                source_live_current.insert(d.id().to_string(), d.capabilities().live_current);
            }
            for s in &webhook_sources {
                let d: Arc<dyn localsky::ports::weather_source::WeatherSource> = s.clone();
                source_live_current.insert(d.id().to_string(), d.capabilities().live_current);
            }
            // EcowittGwPoll publishes weather on the bus (see its spawn above)
            // but isn't a WeatherSource instance; it's a live local poll, so
            // mark its ids live_current=true.
            for entry in cfg.sources.iter().filter(|s| s.enabled) {
                if matches!(
                    entry.source,
                    localsky::config::schema::SourceKind::EcowittGwPoll(_)
                ) {
                    source_live_current.insert(entry.id.clone(), true);
                }
            }

            // Forecast priority is USER-controlled: each enabled forecast
            // source's config `priority` decides which one drives the forecast
            // (higher wins; ties keep the incumbent), and the forecast_provider
            // pin (if set) is bumped to the winning priority. An implicit
            // Open-Meteo (no [[sources]] entry) isn't here, so it defaults to 0
            // in the bridge -> lowest, the failover. ADDITIVE: an unset pin
            // leaves this byte-identical to the per-source ranking.
            let forecast_priority = localsky::runtime::forecast_priority_map(cfg);
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
            // Publish the boot priority into the SWAPPABLE handle the bridge
            // reads, so a hot-reloaded forecast-provider change re-ranks it live.
            forecast_priority_handle.store(Arc::new(forecast_priority));

            // Bridge non-Tempest bus observations into the live snapshot so they
            // populate the dashboard, /api/snapshot, and the HA weather entities
            // (previously only Tempest UDP/demo/Blitzortung wrote the snapshot).
            localsky::sources::snapshot_bridge::spawn(
                receiver_bus_tx.subscribe(),
                tempest_store.clone(),
                source_live_current,
            );

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

    // ---- Forecast bus wiring ----
    // Every forecast-capable source (Open-Meteo + NWS/OpenWeather/Pirate/Met.no)
    // emits SourceEvent::Forecast; the forecast_bridge arbitrates them by
    // priority into the ForecastStore (replacing the old single hardcoded
    // Open-Meteo writer), so the user's chosen forecast source drives forecast.
    if !demo_mode {
        localsky::sources::forecast_bridge::spawn(
            forecast_bridge_rx,
            forecast_store.clone(),
            // The SWAPPABLE handle (not the boot map): the bridge re-reads it on
            // every forecast emit so a hot-reloaded provider/ranking change
            // re-arbitrates without a restart.
            forecast_priority_handle.clone(),
        );

        // Open-Meteo stays the no-auth default + lowest-priority failover.
        // Opt out with an explicit disabled OpenMeteo source entry; otherwise it
        // runs even pre-config (re-reading coords from the live config store).
        let om = boot_cfg.as_ref().and_then(|c| {
            c.sources
                .iter()
                .find(|s| matches!(s.source, localsky::config::schema::SourceKind::OpenMeteo(_)))
        });
        if om.map(|s| s.enabled).unwrap_or(true) {
            let om_id = om
                .map(|s| s.id.clone())
                .unwrap_or_else(|| "open_meteo".to_string());
            spawn_forecast_refresher(
                receiver_bus_tx.clone(),
                om_id,
                boot_cfg
                    .as_ref()
                    .map(|c| (c.deployment.location.lat, c.deployment.location.lon)),
                Some(cfg_store.clone()),
            );
        } else {
            tracing::info!(
                "Open-Meteo forecast disabled by config; relying on other forecast sources"
            );
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
                source_reachable: Some(source_reachable.clone()),
            })
    };
    let mk_ingest = || {
        api::ingest::router(api::ingest::IngestState {
            ecowitt: ecowitt_sources.clone(),
            webhooks: webhook_sources.clone(),
            sensor_history: sensor_history.clone(),
        })
    };
    // Live runtime handles for config hot-reload: the SHARED tempest store +
    // the swappable forecast-priority and watering-policy handles the
    // background tasks read. Passed to the config + wizard APIs so a PUT
    // /api/config / wizard apply / rollback re-applies the engine-tunable
    // subset to the RUNNING system with no restart.
    let runtime_handles = localsky::runtime::RuntimeHandles {
        tempest_store: tempest_store.clone(),
        forecast_priority: forecast_priority_handle.clone(),
        watering_policy: watering_policy_handle.clone(),
        manual_schedules: manual_schedules_handle.clone(),
        source_reachable: source_reachable.clone(),
        // Thread the SAME observation last-seen handle the bus recorder records
        // into and /api/health reads, so /api/config/source_catalog feeds
        // compute_source_status the same observation-liveness proof health does
        // (a recently-observing source reads its calm status, not offline).
        source_last_seen: Some(source_last_seen.clone()),
    };
    let mk_config = {
        let cfg_store = cfg_store.clone();
        let runtime_handles = runtime_handles.clone();
        move || {
            api::config::router(api::config::ConfigApiState {
                store: cfg_store.clone(),
                runtime: Some(runtime_handles.clone()),
            })
        }
    };
    let mk_location = || api::location::router(cfg_store.clone());
    let mk_wizard = {
        let runtime_handles = runtime_handles.clone();
        let draft_store = draft_store.clone();
        let cfg_store = cfg_store.clone();
        let auth_rt = auth_rt.clone();
        let tempest_store = tempest_store.clone();
        move || {
            api::wizard::router(api::wizard::WizardApiState {
                draft_store: draft_store.clone(),
                config_store: cfg_store.clone(),
                auth_rt: auth_rt.clone(),
                tempest_store: Some(tempest_store.clone()),
                runtime: Some(runtime_handles.clone()),
            })
        }
    };
    let mk_photos = {
        let photos_dir = photos_dir.clone();
        move || api::photos::router(photos_dir.clone())
    };

    let app = Router::new()
        .route("/metrics", axum::routing::get(metrics_handler))
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
        // Radar map data services, canonical-prefix only (both shipped
        // after the /api/v1 split, so there is no legacy /api alias to
        // honor). windgrid: leaflet-velocity U/V field (config store
        // for the pinned model + 30-minute cache). tropical: all-basin
        // tropical cyclone GeoJSON normalized from the verified
        // NHC/CPHC + JMA + JTWC feeds (own 10-minute cache).
        .nest(
            "/api/v1/radar",
            api::windgrid::router(api::windgrid::WindGridState::new(Some(cfg_store.clone())))
                .merge(api::tropical::router(api::tropical::TropicalState::new()))
                .merge(api::precip::router(api::precip::PrecipState::new(Some(
                    cfg_store.clone(),
                )))),
        )
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
        // Bundled documentation, served same-origin so in-app help is
        // version-matched to the running build and works offline / on
        // LAN-only / air-gapped installs. The Dockerfile runs
        // `mdbook build docs` and copies the output into
        // <site_root>/docs; this router resolves extensionless
        // /docs/<slug> -> <slug>.html (try-files), matching what
        // crate::docs::doc_url emits and what the public Caddy site
        // serves. Mounted ahead of the Leptos SSR fallback so /docs/*
        // never resolves to the app shell. Ingress strips the prefix
        // before forwarding, so the server route stays plain /docs.
        .nest("/docs", localsky::docs_serve::router(&site_root))
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
        ))
        // App-baseline security headers (WH-01 / LS-REC-02). Conservative,
        // app-wide, and deliberately limited to headers that cannot break
        // the deploy:
        //   - X-Content-Type-Options: nosniff -> stop MIME sniffing.
        //   - Referrer-Policy: strict-origin-when-cross-origin -> don't
        //     leak full URLs (which can carry ?access_token on /stream)
        //     to third-party origins.
        //   - Permissions-Policy: deny geolocation/camera/microphone; the
        //     app uses none of them, so an injected/embedded context can't
        //     either.
        // if_not_present so an upstream proxy that already sets a stricter
        // value wins (the operator's edge stays authoritative).
        //
        // DELIBERATELY NOT SET in-app (see report):
        //   - HSTS: would break LAN-HTTP self-hosters; belongs at the TLS
        //     edge, not here.
        //   - X-Frame-Options / CSP frame-ancestors: a blanket DENY /
        //     'none' would break the HAOS ingress iframe (Home Assistant
        //     embeds the addon cross-origin). Framing is left to the
        //     edge/operator. See the WH-02 note.
        //   - script/style CSP: risks breaking Leptos hydration and the
        //     /pkg crossorigin-nonce behavior; intentionally omitted.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("geolocation=(), camera=(), microphone=()"),
        ));

    // Mount the auth API + layer the enforcement middleware over the
    // complete router so it sees pages, APIs, and the static fallback.
    //
    // No history DB = no identity store, so credential-based login is
    // unavailable. But the STRUCTURAL protections (Origin check + the
    // privileged config/backup/push/photo/actuation gate) must still layer
    // and FAIL CLOSED: previously this branch layered nothing, so with no DB
    // every state-changing route became reachable cross-origin and
    // unauthenticated. We now layer a store-independent gate that admits only
    // IP-vouched callers on privileged routes and refuses everyone else
    // (LS-REC-05). It reads trusted_networks/proxies/origins from the config
    // store on the same cadence as the full refresher.
    let auth_rt_for_mdns = auth_rt.clone();
    let app = if let Some(rt) = auth_rt {
        app.nest("/api/auth", mk_auth(rt.clone()))
            .nest("/api/v1/auth", mk_auth(rt.clone()))
            .layer(axum::middleware::from_fn_with_state(
                rt,
                localsky::auth::middleware::enforce,
            ))
    } else {
        tracing::warn!(
            "no history DB; built-in login unavailable (mode stays disabled), but the Origin \
             check + privileged-route gate still layer and FAIL CLOSED for anonymous callers"
        );
        let gate = std::sync::Arc::new(localsky::auth::NoStoreGate::new());
        gate.spawn_refresh(cfg_store.clone());
        app.layer(axum::middleware::from_fn_with_state(
            gate,
            localsky::auth::middleware::enforce_no_store,
        ))
    };

    // Public-demo read-only gate. Outermost layer so it short-circuits
    // state-changing + outbound-probe requests before auth or any handler
    // runs. No-op when LOCALSKY_DEMO != 1, so prod / the owner's live
    // instance / self-hosters are untouched; only demo.localsky.io is
    // locked to read browsing.
    let app = if demo_mode {
        tracing::info!(
            "LOCALSKY_DEMO=1: demo read-only gate active (mutations + probes return 403)"
        );
        app.layer(axum::middleware::from_fn(
            localsky::auth::demo_guard::block_when_demo,
        ))
    } else {
        app
    };

    // P3-1: gzip/brotli response compression. The ~24.5 MB hydrate wasm bundle
    // ships ~2.8 MB (brotli) over the wire, the single highest cold-load win on
    // every RPi / HA-OS-ingress deploy (one-time per version per client; the
    // browser + service worker cache the compressed asset). Outermost layer so it
    // covers the static /pkg fallback, the APIs, and the SSR pages alike.
    // CompressionLayer::new() uses DefaultPredicate, which already skips
    // text/event-stream (the live /stream SSE), images, gRPC, and tiny bodies,
    // and respects Accept-Encoding (brotli preferred) -- so the /pkg crossorigin
    // headers and hashed-asset cache discipline are untouched.
    let app = app.layer(tower_http::compression::CompressionLayer::new());

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
