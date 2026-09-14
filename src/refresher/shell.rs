// The IO shell around a pass: the 10 s tick, the watchdog, the Home
// Assistant read, the controller overlay, the finalize stage, and the
// side effects a stored snapshot triggers (push edges, ledger rows,
// metrics). This is the one place in a tick that reads the clock, and it
// reads it once (a guard test pins that).

use super::*;
use crate::assembly::*;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::scripting::CompiledScripts;
use crate::forecast::ForecastStore;
use crate::integrations::home_assistant::rest::HaClient;
use crate::model::IrrigationSnapshot;
use crate::refresher::store::IrrigationStore;
use crate::tempest::state::TempestStore;
use arc_swap::ArcSwap;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Default poll interval. Irrigation state is low-frequency so 10s is
/// plenty; manual zone runs surface within a tap-of-an-eyeblink.
pub(crate) const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

/// Backoff ceiling so a long HA outage never sleeps the refresher
/// longer than its happy-path cadence by more than ~3 minutes.
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(180);

/// Last wall-clock epoch the refresher loop began an iteration. The watchdog
/// (`spawn_refresher_watchdog`) reads this to detect a dead/hung refresher (a
/// panic kills the spawned task and freezes this value, since errors are handled
/// in-loop and never unwind). 0 = not started yet.
pub(crate) static REFRESHER_HEARTBEAT: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(0);

/// How long the refresher heartbeat may go stale before the watchdog force-exits
/// the process. Must exceed the worst-case tick gap (BACKOFF_MAX 180s during an
/// outage) by a wide margin so a legitimately-degraded refresher is never killed.
pub(crate) const REFRESHER_STALL_MAX_S: i64 = 600;

/// Grace period after the watchdog starts before a still-zero heartbeat (the
/// refresher never produced a first tick, e.g. it panicked in setup) is treated
/// as a stall.
pub(crate) const REFRESHER_STARTUP_GRACE_S: i64 = 120;

#[allow(clippy::too_many_arguments)]
pub fn spawn_refresher(
    store: Arc<IrrigationStore>,
    forecast_store: Arc<ForecastStore>,
    tempest_store: Arc<TempestStore>,
    history_conn: Option<Arc<Mutex<Connection>>>,
    push: crate::push::PushDispatcher,
    // Hot-reloadable engine tunables (skip-rule thresholds, restrictions,
    // seasonal dial, manual schedules, soil/budget zones, units). Read fresh
    // each tick via `load()` so a PUT /api/config (or wizard apply) that swaps
    // a new policy in takes effect on the LIVE engine on the very next
    // evaluation, with no container restart. The handle is shared with the
    // config-apply path (see `runtime::apply_runtime_config`).
    watering_policy: Arc<ArcSwap<WateringPolicy>>,
    scripts: CompiledScripts,
    source: SnapshotSource,
    controllers: ControllerRegistry,
    // Locally persisted pause + one-day override. Read each tick so a
    // native build (and the shadow build) honors operator pauses. `None`
    // only when no persistence DB is mounted.
    control_store: Option<crate::persistence::IrrigationControlStore>,
    // Active zone list, resolved by the caller (config.zones when
    // localsky.toml exists, empty on a fresh
    // unconfigured install). Resolved once at spawn time; changing it
    // requires a restart, the same contract every deploy-time input has.
    // Per-zone run sizing (zone_runtime) and cycle/soak agronomy now ride
    // the hot-swapped watering_policy instead of boot-bound arguments, so
    // an applied texture/sprinkler/precip change takes effect next tick.
    zones: Vec<crate::zones::ZoneIdent>,
) {
    tokio::spawn(async move {
        // HA client only when sourcing from Home Assistant. Native builds
        // the snapshot from local stores + controllers and needs no HA.
        let client = match source {
            SnapshotSource::HomeAssistant => match HaClient::from_env() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::error!("ha_client init failed: {e:#}");
                    return;
                }
            },
            SnapshotSource::Native => None,
        };
        tracing::info!(?source, "irrigation refresher snapshot source");

        tracing::info!(
            zone_count = zones.len(),
            zones = ?zones.iter().map(|z| z.slug.as_str()).collect::<Vec<_>>(),
            "irrigation refresher resolved zone list"
        );

        // Forecast-bias ingest: each refresh, record today's
        // (predicted, observed) rain pair. The first write of each day
        // captures the morning prediction; subsequent writes update
        // observed_in as the day's total accumulates. The bias engine
        // reads these rows to compute a per-month correction
        // multiplier (engine::forecast_bias).
        //
        // The forecast_observations table is created by M0006, which
        // runs only on the v2 boot path. On a v1-only install the
        // table is absent; we probe once at spawn time and skip the
        // ingest rather than logging a debug error every refresh.
        // The shutoff-deadline ledger, read (never written) by the snapshot
        // builder so a run on a controller with no state readback is
        // visible and stoppable.
        let active_runs: Option<crate::persistence::ActiveRunsStore> = history_conn
            .as_ref()
            .map(|c| crate::persistence::ActiveRunsStore::new(c.clone()));
        let forecast_obs_store = forecast_observations_store(history_conn.as_ref()).await;

        // Sensor-history handle for resolving `source:<id>:<key>` soil
        // sensors (Ecowitt etc. recorded by the ingest path). HA-entity
        // sensors don't need it. None on a v1-only install without history.
        let sensor_history = history_conn
            .as_ref()
            .map(|c| crate::persistence::SensorHistoryStore::new(c.clone()));

        // Runs handle for the balance's applied-irrigation evidence.
        let runs_store = history_conn
            .as_ref()
            .map(|c| crate::persistence::RunsStore::new(c.clone()));
        // Balance evidence cache: refreshed on a coarse timer or when a
        // run edge may have landed, never per 10s tick.
        let mut balance_tick: Option<BalanceTick> = None;
        let mut balance_fetched_epoch: i64 = 0;

        // Circuit-breaker state. Single warn on first failure ("entering
        // degraded mode"), single info on recovery ("recovered"), with
        // exponential backoff between attempts while degraded.
        let mut consecutive_failures: u32 = 0;
        let mut degraded: bool = false;
        // The last control row that actually came back. Once the helper
        // reads are retired this store is the ONLY home of the vacation
        // pause, so resolving a failed SELECT to the default would read as
        // "not paused" and dispatch a morning somebody held. Reusing the
        // last good row keeps the hold across a transient error; it only
        // falls back to nothing before the first successful read of the
        // process, which is a window no watering decision has run in yet.
        let mut last_control: Option<crate::model::IrrigationControlState> = None;
        // The observers run beside the loop, fed by every stored snapshot.
        // The one thing they tell the loop: a run row landed, so the
        // balance evidence is stale.
        let balance_dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
        super::observers::spawn_observers(
            store.clone(),
            super::observers::ObserverDeps {
                history_conn: history_conn.clone(),
                push: push.clone(),
                forecast_store: forecast_store.clone(),
                tempest_store: tempest_store.clone(),
                controllers: controllers.clone(),
                source,
                balance_dirty: balance_dirty.clone(),
            },
        );
        loop {
            // Watchdog heartbeat: stamp the start of every iteration so a stalled
            // or panicked refresher (the spawned task dies, freezing this value)
            // is detectable and forces a restart instead of silently freezing all
            // live data + the today verdict.
            REFRESHER_HEARTBEAT.store(
                chrono::Utc::now().timestamp(),
                std::sync::atomic::Ordering::Relaxed,
            );
            // Load the hot-reloadable watering policy once per tick. A PUT
            // /api/config (or wizard apply) arc-swaps a new policy in; reading
            // it here means a changed skip threshold / restriction / seasonal
            // dial is honored on THIS evaluation, not at the next restart. The
            // guard derefs to &WateringPolicy, matching the old by-value param,
            // so every downstream call below is unchanged.
            let watering_policy = watering_policy.load();
            let watering_policy: &WateringPolicy = &watering_policy;
            // Read the local control surface (vacation pause + one-day
            // override) once per tick. Used by the native builder and, when
            // shadowing, the shadow build too. Cheap single-row select.
            let control = match control_store.as_ref() {
                Some(cs) => match cs.try_get().await {
                    Ok(c) => {
                        last_control = Some(c.clone());
                        Some(c)
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            reusing = last_control.is_some(),
                            "control state read failed; holding the last known pause and override"
                        );
                        last_control.clone()
                    }
                },
                None => None,
            };
            // Refresh the balance evidence when the cache is stale or a
            // run-end row landed last tick (the ingest below zeroes
            // balance_fetched_epoch AFTER persisting the row, so this
            // re-read always sees the new evidence, never a tick early).
            let tick_now = chrono::Utc::now().timestamp();
            if balance_dirty.swap(false, std::sync::atomic::Ordering::SeqCst) {
                // A run-end row just landed: re-read the balance now so
                // applied credit, sessions_done and the spacing anchor
                // see it, not after the coarse timer.
                balance_fetched_epoch = 0;
            }
            if balance_tick.is_none() || tick_now - balance_fetched_epoch >= BALANCE_CACHE_MAX_AGE_S
            {
                balance_tick = Some(
                    compute_balance_tick(
                        &forecast_store,
                        &tempest_store,
                        runs_store.as_ref(),
                        forecast_obs_store.as_ref(),
                    )
                    .await,
                );
                balance_fetched_epoch = tick_now;
            }
            let result = match source {
                SnapshotSource::HomeAssistant => {
                    refresh_once(
                        client.as_ref().expect("HA client present for HA source"),
                        &forecast_store,
                        &tempest_store,
                        &zones,
                        &watering_policy.zone_runtime,
                        watering_policy,
                        &scripts,
                        sensor_history.as_ref(),
                        forecast_obs_store.as_ref(),
                        balance_tick.as_ref(),
                        &controllers,
                        active_runs.as_ref(),
                        control.as_ref(),
                    )
                    .await
                    .map(|mut snap| {
                        // Latch the water-level capability across refreshes on
                        // the HA path: its only evidence is the per-tick entity
                        // read, and a transient unavailable (HA restart,
                        // integration reload) would otherwise retract the
                        // manifest descriptor and churn the HA entity registry.
                        // Once seen, the sensor stays advertised and reads
                        // unavailable (value stays honestly null) through the
                        // outage. Un-advertising a removed integration takes a
                        // LocalSky restart. The native path keeps its stable
                        // ControllerCaps-derived value (refresh_once_native
                        // overwrites both fields), so the latch is HA-only.
                        snap.water_level_capable |= store.snapshot().water_level_capable;
                        snap
                    })
                }
                SnapshotSource::Native => Ok(refresh_once_native(
                    &forecast_store,
                    &tempest_store,
                    &zones,
                    &watering_policy.zone_runtime,
                    watering_policy,
                    &scripts,
                    sensor_history.as_ref(),
                    forecast_obs_store.as_ref(),
                    balance_tick.as_ref(),
                    &controllers,
                    active_runs.as_ref(),
                    control.as_ref(),
                )
                .await),
            };
            let sleep_for = match result {
                Ok(snap) => {
                    // Read, prefetch, decide, store. Everything a stored
                    // snapshot triggers (push edges, ledger rows, metrics,
                    // the run-edge ingest) happens in the observers.
                    store.store(snap);
                    if degraded {
                        tracing::info!(consecutive_failures, "ha source recovered");
                        degraded = false;
                    }
                    consecutive_failures = 0;
                    REFRESH_INTERVAL
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    // Mark the existing snapshot as stale rather than
                    // overwriting it with empty data; the UI shows the
                    // last good values with an "HA unreachable" badge.
                    let mut prev = (*store.snapshot()).clone();
                    prev.ha_reachable = false;
                    // No controller poll completed on this failed pass. The
                    // resolved meter must not freeze its old controller rate;
                    // independently fresh bus measurements may still report.
                    prev.flow_connected = false;
                    prev.flow_gpm = None;
                    prev.flow = tempest_store.flow_readout(Utc::now().timestamp());
                    prev.restart_reasons = controllers.restart_hold().reasons();
                    prev.restart_required = !prev.restart_reasons.is_empty();
                    store.store(prev);
                    if !degraded {
                        tracing::warn!(
                            error = %format!("{e:#}"),
                            "ha source unreachable; entering degraded mode"
                        );
                        degraded = true;
                    } else {
                        tracing::debug!(
                            consecutive_failures,
                            error = %format!("{e:#}"),
                            "ha still unreachable"
                        );
                    }
                    backoff(consecutive_failures)
                }
            };
            tokio::time::sleep(sleep_for).await;
        }
    });
}

/// Pure stall decision for the watchdog, factored out so it is testable without
/// exiting the process. `heartbeat == 0` means the refresher never produced a
/// first tick, judged against the startup grace; otherwise judge the gap since
/// the last tick against the stall ceiling.
pub(crate) fn refresher_stalled(heartbeat: i64, watchdog_started: i64, now: i64) -> bool {
    if heartbeat == 0 {
        now - watchdog_started > REFRESHER_STARTUP_GRACE_S
    } else {
        now - heartbeat > REFRESHER_STALL_MAX_S
    }
}

/// Supervise the refresher. If its heartbeat goes stale (the spawned task
/// panicked or hung), force-exit so the container restart policy
/// (`restart: unless-stopped`) brings the process back fresh, where boot
/// reconciliation then closes any valve a crash left open. This is the
/// process-level analogue of an in-task restart: `CompiledScripts` is not `Clone`,
/// so re-spawning the loop body in place is not available, and a full restart is
/// both simpler and strictly safer (it re-runs every boot invariant, including
/// `reconcile_stop_all`). The orchestrator, not an unsupervised task, owns recovery.
pub fn spawn_refresher_watchdog() {
    tokio::spawn(async move {
        let started = Utc::now().timestamp();
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let now = Utc::now().timestamp();
            let hb = REFRESHER_HEARTBEAT.load(std::sync::atomic::Ordering::Relaxed);
            let stale_for = if hb == 0 { now - started } else { now - hb };
            if refresher_stalled(hb, started, now) {
                tracing::error!(
                    last_heartbeat = hb,
                    stale_for_s = stale_for,
                    "refresher heartbeat stalled (panic or hang); force-exiting so the container \
                     restarts the process and boot reconciliation runs"
                );
                std::process::exit(1);
            }
        }
    });
}

/// Exponential backoff for the HA refresher. Base 10s, doubling each
/// consecutive failure, jittered ~10%, capped at BACKOFF_MAX.
pub(crate) fn backoff(n: u32) -> Duration {
    let base = 10u64;
    let mult = 1u64.checked_shl(n.min(16)).unwrap_or(u64::MAX);
    let secs = base.saturating_mul(mult).min(BACKOFF_MAX.as_secs());
    let jitter = (secs / 10).max(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let off = nanos % (2 * jitter + 1);
    Duration::from_secs(secs.saturating_sub(jitter).saturating_add(off))
}

/// Pull /api/states once, blend with the in-process forecast + tempest
/// stores, and build the snapshot. Pure read-only with respect to HA
/// (we don't mutate any HA state from here). `zones` is the resolved
/// active zone list passed down from spawn_refresher.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refresh_once(
    client: &HaClient,
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
    zones: &[crate::zones::ZoneIdent],
    zone_runtime: &HashMap<String, ZoneRuntime>,
    watering_policy: &WateringPolicy,
    scripts: &CompiledScripts,
    sensor_history: Option<&crate::persistence::SensorHistoryStore>,
    forecast_obs: Option<&crate::persistence::ForecastObservationsStore>,
    balance: Option<&BalanceTick>,
    controllers: &ControllerRegistry,
    active_runs: Option<&crate::persistence::ActiveRunsStore>,
    control: Option<&crate::model::IrrigationControlState>,
) -> anyhow::Result<IrrigationSnapshot> {
    let states = client.states().await?;
    let map = states_to_map(states);
    let (mut snap, finalize) = build_from_map(
        map,
        forecast_store,
        tempest_store,
        zones,
        zone_runtime,
        watering_policy,
        scripts,
        sensor_history,
        forecast_obs,
        balance,
        control,
        controllers.restart_hold().reasons(),
    )
    .await;
    // A configured controller that actually reports outranks the entity
    // readback. Two reasons. It is the better source: an adapter reports its
    // own running_known, where the entity path hardcodes `true` whether or
    // not the readback means anything. And it closes a live defect: an
    // install with HA_URL set AND a Rachio or a direct OpenSprinkler
    // configured resolves to the Home Assistant source, so Run/Stop already
    // dispatch through the registry while `running` was read from a
    // binary_sensor that does not exist. Running was permanently false, so no
    // run row was ever written, and that cost those installs twice over. The
    // weekly balance credited none of the water it had applied, so every
    // session was sized as if the week were untouched; and, the larger of the
    // two, `last_run_epoch` stayed 0, so the session spacing gate never held
    // and the zone planned a full session every morning it was otherwise
    // clear to water. Both correct themselves from the first morning after
    // the upgrade, which means those zones water substantially less often.
    // The entity read stays underneath for the legacy Home-Assistant-only
    // install that has no controller in LocalSky at all.
    overlay_reporting_controllers(&mut snap, controllers, active_runs, zone_runtime).await;
    // Custom-rule watering multiplier (AdjustMultiplier condition action) is
    // applied to the finalized per-zone run time here, where both the planned
    // seconds and the back-filled verdict are final. No-op for the common case
    // (no such rule => multiplier 1.0).
    apply_verdict_multiplier(&mut snap);
    // The one place the next run is decided, same as the native path.
    finalize.apply(&mut snap, watering_policy);
    Ok(snap)
}

/// `/api/states` as a map keyed by entity id.
pub(crate) fn states_to_map(states: Vec<Value>) -> HashMap<String, Value> {
    states
        .into_iter()
        .filter_map(|v| {
            v.get("entity_id")
                .and_then(|e| e.as_str())
                .map(|id| (id.to_string(), v.clone()))
        })
        .collect()
}

/// Let a controller that actually reports outrank the Home Assistant entity
/// readbacks, per field and per zone. Unlike the native path this never
/// overwrites with an absence: a field no controller reports keeps whatever
/// the entity said, so a legacy install with no controller configured is
/// untouched.
pub(crate) async fn overlay_reporting_controllers(
    snap: &mut IrrigationSnapshot,
    controllers: &ControllerRegistry,
    active_runs: Option<&crate::persistence::ActiveRunsStore>,
    zone_runtime: &HashMap<String, ZoneRuntime>,
) {
    if controllers.ids().is_empty() {
        return;
    }
    let cs = native_controller_state(controllers, active_runs).await;
    for z in snap.zones.iter_mut() {
        if let Some((running, known)) = cs.running.get(&z.slug) {
            z.running = *running;
            z.running_known = *known;
        }
        z.running_observed_epoch = cs.observed.get(&z.slug).copied();
        z.ledger_running = cs.ledger.contains(&z.slug);
        z.controller_id = cs.reporter.get(&z.slug).cloned();
        z.throughput_mm_hr = zone_runtime
            .get(crate::engine::ZoneSlug::new(&z.slug).as_str())
            .map(|rt| rt.throughput_mm_hr);
    }
    if let Some(m) = cs.master {
        snap.master_enable = m;
    }
    if cs.water.is_some() {
        snap.water_level_pct = cs.water;
    }
    snap.water_level_capable |= cs.water_level_capable || cs.water.is_some();
    snap.flow_meter = cs.flow_meter;
    snap.flow_connected = cs.flow_connected;
    snap.flow_gpm = cs.flow_gpm;
    if let Some(id) = cs.flow_source_id.as_deref() {
        snap.flow.prefer_controller(id, cs.flow_gpm);
    }
}

/// The IO shell around `assembly::assemble`: mint the one instant, take
/// the store snapshots, fetch what lives in a database, and hand it all
/// to the pure assembly. This is the only place in a tick that reads the
/// clock.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_from_map(
    map: HashMap<String, Value>,
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
    zones: &[crate::zones::ZoneIdent],
    zone_runtime: &HashMap<String, ZoneRuntime>,
    watering_policy: &WateringPolicy,
    scripts: &CompiledScripts,
    sensor_history: Option<&crate::persistence::SensorHistoryStore>,
    // Station-gauge daily rain history (forecast_observations rows). Used
    // to floor days_since_significant_rain with what the local gauge
    // actually measured; `None` on a v1 schema / no persistence DB.
    forecast_obs: Option<&crate::persistence::ForecastObservationsStore>,
    // Pre-computed balance evidence (observed rain, bias model, per-zone
    // run history), gathered once per tick so the sync allocator never
    // touches SQLite. `None` in tests / before the first tick: the balance
    // degrades to target-only sizing.
    balance: Option<&BalanceTick>,
    // Native control surface. `Some` whenever a persistence DB is mounted,
    // on BOTH deployment paths.
    control: Option<&crate::model::IrrigationControlState>,
    restart_reasons: Vec<String>,
) -> (IrrigationSnapshot, crate::assembly::Finalize) {
    let now_epoch = crate::timeutil::now_local().timestamp();
    let prefetched = prefetch(watering_policy, &map, sensor_history, forecast_obs).await;
    let forecast = forecast_store.snapshot();
    // What the finalize stage needs after the shell's overlay: the same
    // forecast this pass assembled from, the week's watered days, the
    // pass's instant.
    let finalize = crate::assembly::Finalize {
        watered: crate::assembly::watered_days(balance),
        forecast: forecast.clone(),
        now_epoch,
    };
    let mut snap = crate::assembly::assemble(crate::assembly::AssemblyInput {
        map,
        forecast,
        tempest: tempest_store.snapshot(),
        current_weather: tempest_store.current_weather_samples(now_epoch),
        field_sources: tempest_store.field_source_map(),
        rain_owner: tempest_store.rain_owner(now_epoch),
        rain_today_owner: tempest_store.rain_today_owner(now_epoch),
        zones,
        zone_runtime,
        watering_policy,
        scripts,
        balance,
        control,
        restart_reasons,
        now_epoch,
        prefetched,
    });
    snap.flow = tempest_store.flow_readout(now_epoch);
    (snap, finalize)
}

/// Everything the assembly needs from a database, fetched before it runs.
pub(crate) async fn prefetch(
    watering_policy: &WateringPolicy,
    map: &HashMap<String, Value>,
    sensor_history: Option<&crate::persistence::SensorHistoryStore>,
    forecast_obs: Option<&crate::persistence::ForecastObservationsStore>,
) -> crate::assembly::Prefetched {
    let window_days = watering_policy.skip_rules.rain_observed_window_days;
    let observed_past_gauge_in = match forecast_obs {
        Some(store) => store
            .observed_rain_last_n_days(window_days as i64)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "observed_rain_last_n_days query failed");
                0.0
            }),
        None => 0.0,
    };
    let observed_days_since_rain = match forecast_obs {
        Some(store) => store
            .days_since_observed_rain(crate::forecast::snapshot::SIGNIFICANT_RAIN_IN)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "days_since_observed_rain query failed");
                None
            }),
        None => None,
    };
    let soil_extras = resolve_soil_extras(&watering_policy.soil_zones, sensor_history).await;
    let soil_zones = if watering_policy.soil_zones.is_empty() {
        Vec::new()
    } else {
        resolve_soil_zones(&watering_policy.soil_zones, map, sensor_history).await
    };
    let soil_probe_faults =
        detect_soil_probe_faults(&watering_policy.soil_zones, &soil_zones, sensor_history).await;
    crate::assembly::Prefetched {
        observed_past_gauge_in,
        observed_days_since_rain,
        soil_extras,
        soil_zones,
        soil_probe_faults,
    }
}

/// Native (no-Home-Assistant) snapshot builder. Reuses `build_from_map`
/// with an EMPTY entity map so every store-preferred read works (weather
/// from ForecastStore/TempestStore; soil via `source:` channels), then
/// overrides the genuinely HA-only fields. Running-state, run-times, and
/// control surfaces are filled by follow-up increments (A4-A6); until then
/// they hold safe defaults (running=false, planned=0 -> nothing waters,
/// master off), so a partially-built native path can never mis-water.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn refresh_once_native(
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
    zones: &[crate::zones::ZoneIdent],
    zone_runtime: &HashMap<String, ZoneRuntime>,
    watering_policy: &WateringPolicy,
    scripts: &CompiledScripts,
    sensor_history: Option<&crate::persistence::SensorHistoryStore>,
    forecast_obs: Option<&crate::persistence::ForecastObservationsStore>,
    balance: Option<&BalanceTick>,
    controllers: &ControllerRegistry,
    // The shutoff-deadline ledger, for zones whose controller cannot
    // report state. None without a persistence DB.
    active_runs: Option<&crate::persistence::ActiveRunsStore>,
    // Locally persisted pause + one-day override. `None` only when no
    // persistence DB is mounted, in which case the snapshot falls back to
    // "no pause / auto override" (and the API rejects pause writes).
    control: Option<&crate::model::IrrigationControlState>,
) -> IrrigationSnapshot {
    let map: HashMap<String, Value> = HashMap::new();
    let (mut snap, finalize) = build_from_map(
        map,
        forecast_store,
        tempest_store,
        zones,
        zone_runtime,
        watering_policy,
        scripts,
        sensor_history,
        forecast_obs,
        balance,
        control,
        controllers.restart_hold().reasons(),
    )
    .await;
    // Native builds have no remote dependency; the engine is always reachable.
    snap.ha_reachable = true;

    // A4: per-zone running-state + master/water_level from the controllers
    // directly (no HA binary_sensors). Best-effort: a controller that can't
    // report leaves running=false + running_known=false; a status() error
    // is swallowed so a flaky controller never stalls the refresh. An
    // adapter can also report a zone with its OWN running_known=false (a
    // cloud running-state read it could not interpret this poll): the value
    // is its last known state carried forward, surfaced as unknown.
    let cs = native_controller_state(controllers, active_runs).await;
    for z in snap.zones.iter_mut() {
        match cs.running.get(&z.slug) {
            Some((r, known)) => {
                z.running = *r;
                z.running_known = *known;
            }
            None => {
                z.running = false;
                z.running_known = false;
            }
        }
        z.running_observed_epoch = cs.observed.get(&z.slug).copied();
        z.ledger_running = cs.ledger.contains(&z.slug);
        z.controller_id = cs.reporter.get(&z.slug).cloned();
        z.throughput_mm_hr = zone_runtime
            .get(crate::engine::ZoneSlug::new(&z.slug).as_str())
            .map(|rt| rt.throughput_mm_hr);
    }
    // Default to enabled when no controller reports, so a missing readback
    // never silently suppresses watering (a control fail-safe, not a
    // displayed measurement). The water level is the opposite case, a
    // DISPLAYED measurement: every adapter except OpenSprinkler reports
    // None, and the old unwrap_or(100.0) published a fabricated healthy
    // "100%" readback for all of them. None stays None.
    snap.master_enable = cs.master.unwrap_or(true);
    snap.water_level_pct = cs.water;
    snap.water_level_capable = cs.water_level_capable || cs.water.is_some();
    // Flow: capability flag + live GPM straight from the controller. Stays
    // None when no meter so the UI / HA surface nothing for non-flow setups.
    snap.flow_meter = cs.flow_meter;
    snap.flow_connected = cs.flow_connected;
    snap.flow_gpm = cs.flow_gpm;

    if let Some(id) = cs.flow_source_id.as_deref() {
        snap.flow.prefer_controller(id, cs.flow_gpm);
    }

    // A5: run-times come from LocalSky's own weekly-budget allocator, applied
    // inside build_from_map (`apply_budget_plan`) for both deployment paths.
    // Custom-rule watering multiplier (AdjustMultiplier), applied after that
    // plan is final, so display + dispatch agree. No-op when no zone carries
    // such a rule.
    apply_verdict_multiplier(&mut snap);
    // The one place the next run is decided: after the controller overlay
    // and the multiplier, with the week's watered days and the forecast
    // (a freezing pre-dawn moves the window after sunrise) in hand.
    finalize.apply(&mut snap, watering_policy);

    // A6: pause / override come from `control` (threaded into build_from_map
    // above); thresholds come from cfg.engine.skip_rules via watering_policy.
    snap
}

/// Query every configured controller once for live state and merge it:
/// per-zone running (by slug), plus the first reported master-enable +
/// water-level. Errors are swallowed (best-effort, never fails a refresh).
pub(crate) async fn native_controller_state(
    controllers: &ControllerRegistry,
    active_runs: Option<&crate::persistence::ActiveRunsStore>,
) -> NativeControllerState {
    let mut running: HashMap<String, (bool, bool)> = HashMap::new();
    // Which controller reported each zone, so the observer can label
    // its rows with the truth instead of a placeholder.
    let mut reporter: HashMap<String, String> = HashMap::new();
    // When each controller took the reading, for the zones it reported.
    let mut observed: HashMap<String, i64> = HashMap::new();
    // The zones LocalSky has commanded on and not yet off, from the
    // shutoff-deadline ledger. A controller that cannot report state
    // leaves its zones out of status(); this is the only record that
    // water is moving on them.
    let mut ledger: std::collections::HashSet<String> = std::collections::HashSet::new();
    let now = Utc::now().timestamp();
    if let Some(ar) = active_runs {
        match ar.armed().await {
            Ok(rows) => {
                for r in rows {
                    if r.off_deadline_epoch > now {
                        ledger.insert(r.zone_slug);
                    }
                }
            }
            Err(e) => tracing::debug!(error = %e, "active-run ledger read failed"),
        }
    }
    let mut master: Option<bool> = None;
    let mut water: Option<f64> = None;
    let mut flow_gpm: Option<f64> = None;
    let mut flow_source_id: Option<String> = None;
    let mut flow_meter = false;
    let mut flow_connected = false;
    let mut water_level_capable = false;
    for id in controllers.ids() {
        let Some(c) = controllers.get(&id) else {
            continue;
        };
        // The capability flags come from supports(), not status(), so a
        // controller with a meter that momentarily reports flow_gpm=None
        // (or a water level between reads) still advertises the capability.
        let caps = c.supports();
        if caps.flow_meter {
            flow_meter = true;
        }
        if caps.water_level {
            water_level_capable = true;
        }
        match c.status().await {
            Ok(st) => {
                let (meter_connected, meter_rate) = crate::controllers::flow::current_flow(
                    &st,
                    Utc::now().timestamp(),
                    c.status_poll_interval_s(),
                );
                // A cloud adapter is polled on its own interval, so its
                // reading can predate this pass; an adapter read on demand
                // says nothing and is as of now.
                let observed_at = st.observed_epoch;
                for z in st.zone_states {
                    reporter.insert(z.slug.clone(), id.clone());
                    if let Some(at) = observed_at {
                        observed.insert(z.slug.clone(), at);
                    }
                    running.insert(z.slug, (z.running, z.running_known && st.reachable));
                }
                // A cloud fallback may carry yesterday's complete status in
                // Ok with reachable=false. Preserve its running flag as unknown
                // for display, but do not promote controls or measurements.
                if !st.reachable {
                    continue;
                }
                if master.is_none() {
                    master = st.master_enabled;
                }
                if water.is_none() {
                    water = st.water_level_pct;
                }
                // First controller to report measured flow wins (matches the
                // master/water "first non-None" merge above).
                flow_connected |= meter_connected;
                if flow_gpm.is_none() {
                    if let Some(rate) = meter_rate {
                        flow_gpm = Some(rate);
                        flow_source_id = Some(id.clone());
                    }
                }
            }
            // A controller that can't report is a real ops signal (the
            // status is otherwise silently swallowed). Track it per controller.
            Err(_) => {
                crate::metrics::inc(
                    "localsky_controller_errors_total",
                    format!(
                        "{},{}",
                        crate::metrics::label("controller", &id),
                        crate::metrics::label("op", "status")
                    ),
                );
            }
        }
    }
    NativeControllerState {
        running,
        observed,
        master,
        water,
        flow_gpm,
        flow_source_id,
        flow_meter,
        flow_connected,
        water_level_capable,
        ledger,
        reporter,
    }
}

/// Merged live readback from all configured controllers, gathered once per
/// native refresh. Best-effort: a controller that can't report contributes
/// nothing rather than failing the refresh.
pub(crate) struct NativeControllerState {
    /// slug -> (running, running_known). running_known=false means the
    /// adapter carried its last known value forward this poll.
    running: HashMap<String, (bool, bool)>,
    /// slug -> the epoch the controller took that reading, for the
    /// adapters that know. Absent for one read on demand.
    observed: HashMap<String, i64>,
    master: Option<bool>,
    water: Option<f64>,
    flow_gpm: Option<f64>,
    flow_source_id: Option<String>,
    flow_meter: bool,
    flow_connected: bool,
    /// Any configured controller declares `ControllerCaps.water_level`.
    water_level_capable: bool,
    /// Zones with a live shutoff deadline in the ledger.
    ledger: std::collections::HashSet<String>,
    /// The controller that reported each zone.
    reporter: HashMap<String, String>,
}

#[cfg(test)]
mod controller_state_tests {
    use super::*;
    use crate::ports::irrigation_controller::{
        ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
        RunHandle, RunRecord, ZoneRuntimeStatus,
    };

    struct Readback(std::sync::Mutex<ControllerStatus>);

    #[async_trait::async_trait]
    impl IrrigationController for Readback {
        fn id(&self) -> &str {
            "readback"
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: true,
                rain_sensor: false,
                master_valve: true,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
                water_level: true,
                per_zone_stop: true,
                duration_quantum_s: 1,
            }
        }
        async fn run_zone(&self, _: &str, _: u32) -> ControllerResult<RunHandle> {
            Err(ControllerError::Unsupported("read-only test".into()))
        }
        async fn stop_zone(&self, _: &str) -> ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            Ok(())
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            Ok(self.0.lock().unwrap().clone())
        }
        async fn run_history(&self, _: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn disconnected_controller_cannot_refresh_known_valves_controls_or_flow() {
        let observed_epoch = Utc::now().timestamp();
        let adapter = Arc::new(Readback(std::sync::Mutex::new(ControllerStatus {
            observed_epoch: Some(observed_epoch),
            reachable: true,
            master_enabled: Some(true),
            water_level_pct: Some(80.0),
            rain_sensor_tripped: None,
            current_program: None,
            zone_states: vec![ZoneRuntimeStatus {
                slug: "front".into(),
                running: true,
                running_known: true,
                remaining_s: Some(30),
                last_run_epoch: None,
            }],
            flow_gpm: Some(4.0),
            flow_connected: true,
            firmware: None,
        })));
        let registry = ControllerRegistry::new();
        registry.set(vec![(adapter.clone(), true)]);
        let live = native_controller_state(&registry, None).await;
        assert_eq!(live.running.get("front"), Some(&(true, true)));
        assert_eq!(live.flow_gpm, Some(4.0));
        assert!(live.flow_connected);

        // A real cloud failure shape: Ok carries the entire previous status.
        adapter.0.lock().unwrap().reachable = false;
        let stale = native_controller_state(&registry, None).await;
        assert_eq!(stale.running.get("front"), Some(&(true, false)));
        assert_eq!(stale.observed.get("front"), Some(&observed_epoch));
        assert_eq!(stale.master, None);
        assert_eq!(stale.water, None);
        assert_eq!(stale.flow_gpm, None);
        assert!(!stale.flow_connected);
        assert!(
            stale.flow_meter,
            "capability remains separate from presence"
        );

        {
            let mut recovered = adapter.0.lock().unwrap();
            recovered.reachable = true;
            recovered.flow_connected = false;
            recovered.flow_gpm = None;
        }
        let no_meter = native_controller_state(&registry, None).await;
        assert!(no_meter.flow_meter);
        assert!(!no_meter.flow_connected);
        assert_eq!(no_meter.flow_gpm, None);
    }
}
