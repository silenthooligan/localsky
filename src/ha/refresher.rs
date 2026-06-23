// Long-running task that polls HA REST and rebuilds the irrigation
// snapshot on each cycle. One spawn per process; failures back off and
// keep going (we'd rather show stale data than crash the whole app).

use crate::controllers::registry::ControllerRegistry;
use crate::engine::scripting::CompiledScripts;
use crate::engine::skip_rules::{LiveReadings, ZoneSoil};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::forecast::ForecastStore;
use crate::ha::rest::HaClient;
use crate::ha::skip_logic::{self, et_heat_multiplier, heat_index_f, Inputs};
use crate::ha::snapshot::{
    DayVerdict, Forecast, IrrigationSnapshot, RuleEval, SoilForecast, WaterBudget, ZoneState,
};
use crate::ha::store::IrrigationStore;
use crate::history::IngestState;
use crate::tempest::state::TempestStore;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

// Zone list is resolved by the caller and passed into spawn_refresher
// (config.zones when localsky.toml exists, LOCALSKY_ZONES otherwise,
// empty on a fresh unconfigured install).
// Snapshot zones are computed by iterating the resolved list rather than
// a compile-time constant; operators with more or fewer zones can override
// without recompiling.

/// Which builder fills the IrrigationSnapshot store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSource {
    /// Poll Home Assistant `/api/states` (the legacy path).
    HomeAssistant,
    /// Build natively from local stores + controllers + the engine (no HA).
    Native,
}

/// Decide whether to source the snapshot from HA or natively. The
/// `LOCALSKY_STANDALONE=1` env override wins; otherwise `Auto` picks
/// native only when no HA env is configured, so an existing HA deploy is
/// unaffected by default.
pub fn resolve_snapshot_source(mode: crate::config::schema::DeploymentMode) -> SnapshotSource {
    use crate::config::schema::DeploymentMode;
    if std::env::var("LOCALSKY_STANDALONE").ok().as_deref() == Some("1") {
        return SnapshotSource::Native;
    }
    let ha_present = std::env::var("HA_URL").is_ok()
        && (std::env::var("HA_TOKEN").is_ok() || std::env::var("HA_LONG_LIVED_TOKEN").is_ok());
    match mode {
        DeploymentMode::HomeAssistant => SnapshotSource::HomeAssistant,
        DeploymentMode::Standalone => SnapshotSource::Native,
        DeploymentMode::Auto => {
            if ha_present {
                SnapshotSource::HomeAssistant
            } else {
                SnapshotSource::Native
            }
        }
    }
}

/// Default poll interval. Irrigation state is low-frequency so 10s is
/// plenty; manual zone runs surface within a tap-of-an-eyeblink.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);
/// Backoff ceiling so a long HA outage never sleeps the refresher
/// longer than its happy-path cadence by more than ~3 minutes.
const BACKOFF_MAX: Duration = Duration::from_secs(180);

/// Per-zone runtime parameters resolved at boot from localsky.toml.
/// The refresher uses these to size run durations instead of reading
/// stale Smart Irrigation entity attributes.
#[derive(Debug, Clone, Copy)]
pub struct ZoneRuntime {
    /// Precipitation rate in mm/hr; either the zone's measured override
    /// or the catalog default for its sprinkler_type. See
    /// engine::effective_precip_rate_mm_hr.
    pub throughput_mm_hr: f64,
    /// Safety cap on a single dispatch (seconds). Engine refuses to
    /// queue runs longer than this even if the deficit would justify
    /// it. Default 3600 (60min) per zone.
    pub max_duration_s: u32,
}

impl ZoneRuntime {
    /// Conservative fallback when a zone is enumerated (via env var or
    /// legacy default) but absent from the loaded config file. Treat
    /// the zone as a rotor at 10 mm/hr with a 60-minute safety cap.
    pub fn fallback() -> Self {
        Self {
            throughput_mm_hr: 10.0,
            max_duration_s: 3600,
        }
    }
}

/// Watering policy snapshot resolved at boot from localsky.toml. The
/// refresher evaluates this against the current wall clock every tick:
///   - `restrictions` + `address_parity` feed the skip-rule ladder and
///     the per-zone `max_duration_s` cap (Phase C).
///   - `manual_schedules` are checked per zone via
///     `crate::scheduler::manual::override_active_today`. When an enabled
///     Override schedule applies today for a zone, the refresher zeros
///     `scheduled_seconds` so smart-irrigation doesn't dispatch on top
///     of the operator's manual run; math still computes for visibility.
///     The actual manual dispatch fires from `scheduler::manual::spawn`.
#[derive(Debug, Clone, Default)]
pub struct WateringPolicy {
    pub restrictions: Vec<crate::config::schema::WateringRestriction>,
    pub address_parity: crate::config::schema::AddressParity,
    pub manual_schedules: Vec<crate::config::schema::ManualSchedule>,
    /// (lat, lon), used by the refresher to compute the LocalSky-native
    /// next_run_epoch from sunrise + sequence_total. (0.0, 0.0) keeps the
    /// pre-cutover semantics: next_run_epoch stays at whatever upstream
    /// produced (legacy IU path before strip; 0 after).
    pub location: (f64, f64),
    /// Per-zone soil config resolved from localsky.toml zones. Each zone's
    /// assigned sensor (`ha:` entity or `source:<id>:<key>` channel) +
    /// per-zone thresholds. Empty = no config (fall back to the legacy
    /// hardcoded soil reads).
    pub soil_zones: Vec<ZoneSoilCfg>,
    /// User-defined structured trigger rules (augment-only), from
    /// `config.conditions.rules`. Empty = none.
    pub condition_rules: Vec<crate::engine::conditions::ConditionRule>,
    /// Engine skip-rule thresholds from `cfg.engine.skip_rules`. The HA
    /// path still prefers the live `input_number` helpers when present and
    /// only falls back to these; the native (empty-map) path has no helpers
    /// so these config values are what the engine actually uses. Defaults
    /// equal the historical hardcoded literals (10 mph / 38F / 0.25in /
    /// 35F), so an HA deploy on default config is unchanged.
    pub skip_rules: crate::config::schema::SkipRuleParams,
    /// Per-zone weekly-budget config from `cfg.zones` (A5b). Drives the
    /// standalone water-budget allocator so any configured zone (not just
    /// the legacy four) gets a run-time. Empty = no config; the allocator
    /// falls back to its legacy hardcoded four-zone defaults.
    pub budget_zones: Vec<ZoneBudgetCfg>,
    /// HA-mode controller entity prefix (from `cfg.deployment.ha_sprinkler_prefix`):
    /// the snapshot reads `switch.<prefix>_enabled`, `sensor.<prefix>_water_level`,
    /// and `binary_sensor.<prefix>_<zone>_station_running`. Empty (the Default)
    /// is treated as "opensprinkler" by the reader, so the HA path works for
    /// any operator's controller naming.
    pub ha_sprinkler_prefix: String,
}

/// Resolve the HA controller entity prefix, falling back to a sensible
/// default when unset (the WateringPolicy::default / env-compat path).
fn sprinkler_prefix(policy: &WateringPolicy) -> &str {
    if policy.ha_sprinkler_prefix.is_empty() {
        "opensprinkler"
    } else {
        &policy.ha_sprinkler_prefix
    }
}

/// One zone's weekly-budget configuration for the standalone allocator.
/// `weekly_budget_in` / `sessions_per_week` are `None` when the operator
/// hasn't set them, in which case the allocator uses an agronomic default
/// inferred from the slug (turf 1.0"/2 sessions, shrub/garden/bed 0.5"/1).
#[derive(Debug, Clone)]
pub struct ZoneBudgetCfg {
    pub slug: String,
    pub name: String,
    pub weekly_budget_in: Option<f64>,
    pub sessions_per_week: Option<u32>,
}

/// One zone's soil configuration resolved at boot from `ZoneConfig`. The
/// refresher resolves `soil_sensor_id` to a live % each tick and pairs it
/// with the per-zone thresholds to build the engine's `ZoneSoil`.
#[derive(Debug, Clone)]
pub struct ZoneSoilCfg {
    pub slug: String,
    pub name: String,
    pub soil_sensor_id: Option<String>,
    pub saturation_pct: f64,
    pub target_min_pct: f64,
}

/// Offline guard for a raw soil reading: a non-positive value (exactly 0% /
/// negative) is a disconnected/faulty probe (e.g. a WH51 out of soil), NOT
/// bone-dry soil, return None so the zone falls back to weather/modeled
/// rather than over-watering. Real soil is essentially never exactly 0.00%.
/// (Soil calibration itself lives at the source, see `parse_soilad`'s
/// native AD-based dry/wet calibration in the Ecowitt poll adapter.)
fn apply_soil_quality(raw: Option<f64>) -> Option<f64> {
    raw.filter(|v| *v > 0.0)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_refresher(
    store: Arc<IrrigationStore>,
    forecast_store: Arc<ForecastStore>,
    tempest_store: Arc<TempestStore>,
    history_conn: Option<Arc<Mutex<Connection>>>,
    push: crate::push::PushDispatcher,
    zone_runtime: HashMap<String, ZoneRuntime>,
    watering_policy: WateringPolicy,
    scripts: CompiledScripts,
    source: SnapshotSource,
    controllers: ControllerRegistry,
    // When set (HA source + shadow_native), the native snapshot is built
    // each tick and written here for comparison, never drives dispatch.
    shadow_store: Option<Arc<IrrigationStore>>,
    // Locally persisted pause + one-day override (A6). Read each tick so a
    // native build (and the shadow build) honors operator pauses. `None`
    // only when no persistence DB is mounted.
    control_store: Option<crate::persistence::IrrigationControlStore>,
    // Active zone list, resolved by the caller (config.zones when
    // localsky.toml exists, LOCALSKY_ZONES otherwise, empty on a fresh
    // unconfigured install). Resolved once at spawn time; changing it
    // requires a restart, the same contract every deploy-time input has.
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
            "ha refresher resolved zone list"
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
        let forecast_obs_store = match history_conn.as_ref() {
            Some(c) => {
                // `c` is an Arc<tokio::sync::Mutex<rusqlite::Connection>>; calling
                // blocking_lock() from inside a tokio task panics ("Cannot block
                // the current thread from within a runtime"). The table-existence
                // probe is a one-shot at spawn time, so await the async lock
                // instead. rusqlite's query_row is synchronous and briefly blocks
                // the worker thread, which is acceptable for a single SELECT.
                let exists = {
                    let conn = c.lock().await;
                    conn.query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='forecast_observations'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|n| n > 0)
                    .unwrap_or(false)
                };
                if exists {
                    Some(crate::persistence::ForecastObservationsStore::new(
                        c.clone(),
                    ))
                } else {
                    tracing::info!(
                        "forecast_observations table absent (v1 schema); skipping bias ingest"
                    );
                    None
                }
            }
            None => None,
        };

        // Sensor-history handle for resolving `source:<id>:<key>` soil
        // sensors (Ecowitt etc. recorded by the ingest path). HA-entity
        // sensors don't need it. None on a v1-only install without history.
        let sensor_history = history_conn
            .as_ref()
            .map(|c| crate::persistence::SensorHistoryStore::new(c.clone()));

        let mut ingest = IngestState::new();
        // Edge-detection state for push events. Tracks per-zone running
        // and the start_epoch when each zone last transitioned to running
        // so ZoneStopped can include duration_min.
        let mut prev_zone_running: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        let mut zone_started_at: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        // Daily verdict push fires once per local-day; the date string
        // is the dedupe key.
        let mut last_verdict_day: Option<String> = None;
        // Soil-probe fault push fires at most once per probe per process
        // lifetime (the fault persists across refreshes; re-notifying
        // every 10s tick would be noise).
        let mut probe_fault_notified: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // Circuit-breaker state. Single warn on first failure ("entering
        // degraded mode"), single info on recovery ("recovered"), with
        // exponential backoff between attempts while degraded.
        let mut consecutive_failures: u32 = 0;
        let mut degraded: bool = false;

        loop {
            // Read the local control surface (vacation pause + one-day
            // override) once per tick. Used by the native builder and, when
            // shadowing, the shadow build too. Cheap single-row select.
            let control = match control_store.as_ref() {
                Some(cs) => Some(cs.get().await),
                None => None,
            };
            let result = match source {
                SnapshotSource::HomeAssistant => {
                    refresh_once(
                        client.as_ref().expect("HA client present for HA source"),
                        &forecast_store,
                        &tempest_store,
                        &zones,
                        &zone_runtime,
                        &watering_policy,
                        &scripts,
                        sensor_history.as_ref(),
                        forecast_obs_store.as_ref(),
                    )
                    .await
                }
                SnapshotSource::Native => Ok(refresh_once_native(
                    &forecast_store,
                    &tempest_store,
                    &zones,
                    &zone_runtime,
                    &watering_policy,
                    &scripts,
                    sensor_history.as_ref(),
                    forecast_obs_store.as_ref(),
                    &controllers,
                    control.as_ref(),
                )
                .await),
            };
            let sleep_for = match result {
                Ok(snap) => {
                    // Shadow: build the native snapshot alongside HA for
                    // side-by-side comparison. Never drives dispatch.
                    if let Some(ss) = &shadow_store {
                        let native = refresh_once_native(
                            &forecast_store,
                            &tempest_store,
                            &zones,
                            &zone_runtime,
                            &watering_policy,
                            &scripts,
                            sensor_history.as_ref(),
                            forecast_obs_store.as_ref(),
                            &controllers,
                            control.as_ref(),
                        )
                        .await;
                        ss.store(native);
                    }
                    if let Some(db) = history_conn.as_ref() {
                        ingest.observe(db, &snap).await;
                    }
                    // Forecast-bias daily ingest. Today's predicted rain
                    // comes from the forecast store's daily[0]; today's
                    // observed rain comes from the snapshot's
                    // forecast.rain_today_in (which the refresher itself
                    // populated from Tempest / HA). UPSERT semantics
                    // preserve the morning prediction across the day.
                    if let Some(obs_store) = forecast_obs_store.as_ref() {
                        let today = chrono::Local::now().date_naive();
                        let predicted_in = forecast_store
                            .snapshot()
                            .daily
                            .first()
                            .map(|d| d.precip_sum_in)
                            .unwrap_or(0.0);
                        let observed_in = snap.skip_check.rain_today_in;
                        let store_handle = obs_store.clone();
                        tokio::spawn(async move {
                            if let Err(e) =
                                store_handle.upsert(today, predicted_in, observed_in).await
                            {
                                tracing::debug!(
                                    error = %e,
                                    "forecast observation upsert failed"
                                );
                            }
                        });
                    }
                    emit_push_events(
                        &push,
                        &snap,
                        &mut prev_zone_running,
                        &mut zone_started_at,
                        &mut last_verdict_day,
                        &mut probe_fault_notified,
                    );
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

/// Exponential backoff for the HA refresher. Base 10s, doubling each
/// consecutive failure, jittered ~10%, capped at BACKOFF_MAX.
fn backoff(n: u32) -> Duration {
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

/// Walk the snapshot and emit push events on edge transitions:
/// - ZoneStarted/ZoneStopped on each zone's running flag flip.
/// - DailyVerdict once per local day, the first time we see a non-empty
///   verdict for that day.
/// - SoilProbeFault when a probe first appears in soil_probe_faults
///   (once per probe per process lifetime via `probe_fault_notified`).
fn emit_push_events(
    push: &crate::push::PushDispatcher,
    snap: &IrrigationSnapshot,
    prev_running: &mut std::collections::HashMap<String, bool>,
    started_at: &mut std::collections::HashMap<String, i64>,
    last_verdict_day: &mut Option<String>,
    probe_fault_notified: &mut std::collections::HashSet<String>,
) {
    use crate::push::PushEvent;
    let now = Utc::now().timestamp();
    for z in &snap.zones {
        let was = *prev_running.get(&z.slug).unwrap_or(&false);
        if z.running && !was {
            started_at.insert(z.slug.clone(), now);
            push.emit(PushEvent::ZoneStarted {
                name: z.name.clone(),
                slug: z.slug.clone(),
            });
        } else if !z.running && was {
            let dur_s = started_at
                .remove(&z.slug)
                .map(|start| (now - start).max(0))
                .unwrap_or(0);
            let duration_min = ((dur_s as f64) / 60.0).round() as u32;
            push.emit(PushEvent::ZoneStopped {
                name: z.name.clone(),
                slug: z.slug.clone(),
                duration_min,
            });
        }
        prev_running.insert(z.slug.clone(), z.running);
    }

    // Soil-probe faults: notify on the transition into faulted state,
    // at most once per probe for the life of the process.
    for f in &snap.soil_probe_faults {
        if probe_fault_notified.insert(f.zone_slug.clone()) {
            push.emit(PushEvent::SoilProbeFault {
                zone_name: f.zone_name.clone(),
                zone_slug: f.zone_slug.clone(),
                since_epoch: f.since_epoch,
            });
        }
    }

    // Daily verdict fires once per local day. The "today" label is the
    // local-date YYYY-MM-DD; on the first refresh after midnight rolls
    // we emit one event with the new verdict.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let verdict = snap.skip_check.verdict.clone();
    if !verdict.is_empty() && last_verdict_day.as_deref() != Some(today.as_str()) {
        push.emit(crate::push::PushEvent::DailyVerdict {
            verdict,
            reason: snap.skip_check.reason.clone(),
        });
        *last_verdict_day = Some(today);
    }
}

/// Pull /api/states once, blend with the in-process forecast + tempest
/// stores, and build the snapshot. Pure read-only with respect to HA
/// (we don't mutate any HA state from here). `zones` is the resolved
/// active zone list passed down from spawn_refresher.
#[allow(clippy::too_many_arguments)]
async fn refresh_once(
    client: &HaClient,
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
    zones: &[crate::zones::ZoneIdent],
    zone_runtime: &HashMap<String, ZoneRuntime>,
    watering_policy: &WateringPolicy,
    scripts: &CompiledScripts,
    sensor_history: Option<&crate::persistence::SensorHistoryStore>,
    forecast_obs: Option<&crate::persistence::ForecastObservationsStore>,
) -> anyhow::Result<IrrigationSnapshot> {
    let states = client.states().await?;
    let map: HashMap<String, Value> = states
        .into_iter()
        .filter_map(|v| {
            v.get("entity_id")
                .and_then(|e| e.as_str())
                .map(|id| (id.to_string(), v.clone()))
        })
        .collect();
    Ok(build_from_map(
        map,
        forecast_store,
        tempest_store,
        zones,
        zone_runtime,
        watering_policy,
        scripts,
        sensor_history,
        forecast_obs,
        None,
    )
    .await)
}

/// Build the `IrrigationSnapshot` from a pre-fetched entity `map` plus the
/// native stores/config. The HA path passes HA `/api/states`; the native
/// (standalone) path passes an empty map and then overrides the HA-only
/// fields (running-state, run-times, control surfaces). All the
/// store-preferred reads (weather from ForecastStore/TempestStore, soil via
/// `source:` channels) work identically either way. Decision logic is the
/// shared `apply_engine`, so the verdict never depends on the source.
#[allow(clippy::too_many_arguments)]
async fn build_from_map(
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
    // Native control surface. `Some` (standalone path) overrides the
    // HA-helper-derived pause/override below with locally persisted state;
    // `None` (HA path) reads them from the entity map as before.
    control: Option<&crate::persistence::IrrigationControlState>,
) -> IrrigationSnapshot {
    let mut snap = IrrigationSnapshot {
        last_refresh_epoch: Utc::now().timestamp(),
        ha_reachable: true,
        tempest_last_seen_epoch: tempest_store.snapshot().last_packet_epoch,
        forecast_last_seen_epoch: forecast_store.snapshot().last_refresh_epoch,
        ..Default::default()
    };

    // Evaluate watering restrictions once per refresh. The verdict feeds
    // skip-logic via Inputs.watering_restrictions below; the cap (when
    // a rule limits run length) tightens each zone's max_duration_s at
    // the two compute sites further down.
    let now_local = chrono::Local::now();
    let restriction_verdict = crate::engine::restrictions::evaluate(
        now_local,
        &watering_policy.restrictions,
        watering_policy.address_parity,
    );
    let restriction_cap_seconds: Option<u32> = restriction_verdict
        .max_minutes_cap
        .map(|m| m.saturating_mul(60));
    // Today's weekday (Sun=0..Sat=6 per chrono::Weekday::num_days_from_sunday)
    // for per-zone manual-override gating below.
    let today_weekday: u8 = {
        use chrono::Datelike;
        now_local.weekday().num_days_from_sunday() as u8
    };

    // next_run_epoch is computed below (after the per-zone planned
    // durations are known) from LocalSky's own smart-morning anchor
    // (sunrise - 15min - sequence_total). The IU bridge was the prior
    // source; it was stripped in the 2026-05-26 cutover.
    snap.iu_enabled = false;
    snap.iu_suspended = false;

    // Master enable + water level, from the operator's controller integration
    // in HA (entity prefix configurable; default "opensprinkler").
    let sp = sprinkler_prefix(watering_policy);
    snap.master_enable = state_eq(&map, &format!("switch.{sp}_enabled"), "on");
    snap.water_level_pct = state_f64(&map, &format!("sensor.{sp}_water_level")).unwrap_or(0.0);

    // Vacation pause + one-day override helpers. Both are user-created
    // HA helpers (input_datetime + input_select). When missing, the snapshot
    // exposes override_helpers_present=false so the mobile UI can disable
    // the controls with a "(HA helper not configured)" hint rather than
    // letting the action POST fail with a 502 on tap.
    match control {
        // Native (standalone) path: the control surface lives in local
        // persisted state, not HA helpers. The controls always "exist"
        // (the UI is fully functional without HA), so present = true.
        Some(c) => {
            snap.override_helpers_present = true;
            snap.pause_until_epoch = c.pause_until_epoch;
            snap.override_tomorrow = c.override_tomorrow.clone();
            // Sticky global override is always LocalSky-native (its own sqlite),
            // so it rides the native control surface even in HA mode.
            snap.global_override = c.global_override.clone();
        }
        // HA path: read both from the entity map exactly as before.
        None => {
            let pause_state = map.get("input_datetime.irrigation_pause_until");
            let override_state = map.get("input_select.irrigation_override_tomorrow");
            snap.override_helpers_present = pause_state.is_some() && override_state.is_some();
            snap.pause_until_epoch = pause_state
                .and_then(|s| s.get("attributes"))
                .and_then(|a| a.get("timestamp"))
                .and_then(Value::as_i64)
                .unwrap_or(0);
            snap.override_tomorrow = override_state
                .and_then(|s| s.get("state"))
                .and_then(Value::as_str)
                .unwrap_or("none")
                .to_string();
            snap.global_override = "auto".to_string();
        }
    }

    // Pre-compute the heat multiplier here (the snapshot.forecast struct
    // also recomputes this later, the dupe is intentional because the
    // zone loop needs it before forecast_store.snapshot() is consumed
    // below, and the cost is one heat-index calc per refresh).
    let zone_loop_heat_mult = {
        let fc_peek = forecast_store.snapshot();
        let humidity_peek = tempest_store.snapshot().rh_pct;
        let tmax_peek = fc_peek.max_temp_next_3d_f().unwrap_or(0.0);
        et_heat_multiplier(heat_index_f(tmax_peek, humidity_peek))
    };

    // Per-zone state. Sum planned_run_seconds across the four zones to
    // get the real next-run total (since IU's zones array carries the
    // YAML placeholder until SI's nightly sync overwrites it).
    //
    // The math tile reads SI's per-zone attributes directly so the
    // displayed formula matches SI's internal compute. heat_mult is the
    // global forecast multiplier (same one SI multiplies into ET via
    // the Phase C HA automation); capture_efficiency is the constant
    // LocalSky uses in the Phase E water-balance projection.
    snap.zones = zones
        .iter()
        .map(|zone| {
            let slug = zone.slug.as_str();
            let running_id = format!("binary_sensor.{sp}_{slug}_station_running");
            let si_id = format!("sensor.smart_irrigation_{slug}");
            let attrs = map.get(&si_id).and_then(|s| s.get("attributes"));
            let bucket_mm = attrs
                .and_then(|a| a.get("bucket"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let kc = attrs
                .and_then(|a| a.get("multiplier"))
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            // Throughput + max-duration resolve from LocalSky's config
            // (localsky.toml zone block -> sprinkler_catalog default by
            // sprinkler_type, or precip_rate_mm_hr override when measured),
            // NOT from SI entity attributes. SI's `throughput` and
            // `maximum_duration` attrs are ignored; SI's `bucket` +
            // `multiplier` are still consumed as a transitional bridge
            // until the v2 water-balance loop owns them too.
            let rt = zone_runtime
                .get(slug)
                .copied()
                .unwrap_or_else(ZoneRuntime::fallback);
            let throughput_mm_hr = rt.throughput_mm_hr;
            // Apply the active watering restriction cap (if any) on top of
            // the per-zone safety ceiling. The tighter of the two wins so
            // a regulatory "no more than 60 min per zone" rule overrides a
            // bigger operator-set ceiling.
            let max_dur = match restriction_cap_seconds {
                Some(c) => rt.max_duration_s.min(c),
                None => rt.max_duration_s,
            };
            // Per-zone session sizing:
            //   seconds = (|bucket_mm| / throughput_mm_hr) * 3600 * multiplier
            // Then capped at max_duration_s to keep a single dispatch from
            // running away on a misconfigured throughput.
            let raw_seconds = if throughput_mm_hr > 0.0 && bucket_mm < 0.0 {
                (bucket_mm.abs() / throughput_mm_hr * 3600.0 * kc) as u32
            } else {
                0
            };
            // Scheduled run uses the engine's own computation, capped at the
            // safety ceiling. If an Override-mode manual schedule applies
            // today for this zone, zero the scheduled dispatch so smart
            // doesn't run on top of the operator's planned manual run; the
            // smart math chain (raw_seconds, cap_binding) still computes
            // for nerd visibility.
            let raw_planned = raw_seconds.min(max_dur);
            let override_active = crate::scheduler::manual::override_active_today(
                &watering_policy.manual_schedules,
                slug,
                today_weekday,
            );
            let planned = if override_active { 0 } else { raw_planned };
            let math = Some(crate::ha::snapshot::ZoneMath {
                bucket_mm,
                kc,
                throughput_mm_hr,
                heat_mult: zone_loop_heat_mult,
                capture_eff: 0.70, // matches compute_soil_forecasts CAPTURE_EFFICIENCY
                raw_seconds,
                max_duration_seconds: max_dur,
                scheduled_seconds: planned,
                cap_binding: raw_seconds > max_dur,
            });
            ZoneState {
                name: zone.display_name.clone(),
                slug: zone.slug.clone(),
                // Sticky per-zone override from the native control surface;
                // "auto" when unset or in HA mode (control = None).
                override_mode: control
                    .and_then(|c| c.zone_overrides.get(&zone.slug))
                    .cloned()
                    .unwrap_or_else(|| "auto".to_string()),
                hex: String::new(), // Populated in Phase 3 from device_registry if needed.
                running: state_eq(&map, &running_id, "on"),
                // HA path reads running from a binary_sensor, always a
                // trusted readback. Native may override to false.
                running_known: true,
                today_run_minutes: 0.0, // Populated by SQLite history in Phase 3.
                bucket_mm,
                planned_run_seconds: planned,
                last_run_epoch: 0, // Populated by SQLite history in Phase 3.
                math,
                // photo_url is read by the dashboard from /api/config on
                // mount and joined to each zone by slug. Kept None here so
                // the snapshot remains a pure runtime-state object.
                photo_url: None,
                // Per-zone verdict is back-filled by apply_engine (which
                // runs decide_per_zone) before the snapshot is published;
                // None only until that pass. The smart-morning dispatcher
                // enforces these at dispatch time.
                verdict: None,
                // Native soil temp/EC/battery merged in after the gateway poll
                // resolves them (resolve_soil_extras, below).
                soil_temp_f: None,
                soil_ec: None,
                soil_battery_pct: None,
            }
        })
        .collect();
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;

    // LocalSky-native next_run_epoch. Compute target_start for today;
    // if today's window has already passed, advance to tomorrow.
    // sequence_total = sum(planned_run_seconds) + 2s inter-zone preamble.
    snap.next_run_epoch = compute_next_run_epoch(watering_policy.location, &snap.zones);

    // Forecast block. Aggregates Tempest live + Open-Meteo regional
    // forecast into one struct the UI can render directly.
    let rain_today_om = state_f64(&map, "sensor.open_meteo_rain_today")
        .map(|mm| mm / 25.4)
        .unwrap_or(0.0);
    let rain_tomorrow = state_f64(&map, "sensor.open_meteo_rain_tomorrow")
        .map(|mm| mm / 25.4)
        .unwrap_or(0.0);
    let rain_3day = state_f64(&map, "sensor.open_meteo_rain_3day")
        .map(|mm| mm / 25.4)
        .unwrap_or(0.0);
    // Phase A: pull forecast intelligence directly from the in-process
    // ForecastStore (Open-Meteo 7-day + 48h + 3-day past) and the live
    // Tempest store. No round-trip via HA REST sensors, single source,
    // fewer moving parts.
    let fc = forecast_store.snapshot();
    let tempest = tempest_store.snapshot();

    // Rain comes from the in-process Tempest listener, which integrates
    // the per-minute rain packets into a true daily total. The HA
    // WeatherFlow `precipitation` entity is the rain in the LAST
    // REPORTING MINUTE, not a daily accumulation; reading it as one
    // capped storm days at ~0.05" (3 in/h over one minute) and let the
    // engine schedule a full run the morning after heavy rain. Recency
    // gated like every other live reading; the regional model is the
    // floor either way, so a station outage degrades to model rain
    // instead of silently to zero.
    let station_now = Utc::now().timestamp();
    let station_fresh = tempest.last_packet_epoch > 0
        && station_now.saturating_sub(tempest.last_packet_epoch) < TEMPEST_LIVE_MAX_AGE_S;
    let rain_today_station = if station_fresh {
        tempest.rain_in_today
    } else {
        0.0
    };
    let rain_intensity = if station_fresh {
        tempest.rain_intensity_in_hr
    } else {
        state_f64(&map, "sensor.st_00206451_precipitation_intensity").unwrap_or(0.0)
    };
    let rain_type = if station_fresh {
        match tempest.precip_type {
            1 => "rain".to_string(),
            2 => "hail".to_string(),
            _ => "none".to_string(),
        }
    } else {
        map.get("sensor.st_00206451_precipitation_type")
            .and_then(|s| s.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_string()
    };

    let rain_today_used = rain_today_station.max(rain_today_om);
    // Live "now" readings. Prefer the in-process Tempest listener while
    // its packets are fresh (recency-gated: a station that stopped
    // reporting hours ago must not keep driving freeze/wind gates).
    // When stale or absent, fall back to the current-hour forecast and
    // mark the inputs degraded; with no forecast either, mark them
    // unavailable so the engine fails safe (skip, never a phantom run
    // on fabricated 70 °F / 0 mph defaults).
    let now_epoch = Utc::now().timestamp();
    let (temp_now, wind_now, humidity_now, live_readings) =
        resolve_current_conditions(&tempest, fc.hourly.first(), now_epoch);
    if live_readings != LiveReadings::Station {
        tracing::debug!(
            ?live_readings,
            tempest_last_packet_epoch = tempest.last_packet_epoch,
            "live station readings unavailable or stale; inputs degraded"
        );
    }

    let (rain_tomorrow_om_in, rain_tomorrow_prob) = fc.tomorrow_precip_with_prob_in();
    let rain_3day_weighted = fc.future_n_day_weighted_precip_in(3);
    let rain_7day_weighted = fc.future_n_day_weighted_precip_in(7);
    let rain_next_4h = fc.next_n_hours_precip_in(4);
    // Option end-to-end: None = no hourly forecast window. The engine's
    // overnight-freeze gate keys applicability off is_some(), so a real
    // sub-zero low is no longer confused with "no data".
    let temp_min_24h: Option<f64> = fc.min_temp_next_24h_f();
    let temp_max_3day = fc.max_temp_next_3d_f().unwrap_or(0.0);
    let wind_max_today = fc.wind_max_today_mph().unwrap_or(0.0);
    let wind_gust_today = fc.wind_gust_max_today_mph().unwrap_or(0.0);
    // Days since significant rain: take the MIN of the regional model's
    // counter and the station-gauge counter from forecast_observations.
    // The gauge's memory beats the regional model for hyperlocal
    // convection: a pop-up storm that soaked this yard but never showed
    // in Open-Meteo's past_daily still counts as recent rain, so the
    // heat-advisory extend can't fire the morning after a soaking.
    let days_since_rain = {
        let model_days = fc.days_since_significant_rain(rain_today_used);
        let observed_days = match forecast_obs {
            Some(store) => store
                .days_since_observed_rain(crate::forecast::snapshot::SIGNIFICANT_RAIN_IN)
                .await
                .unwrap_or_else(|e| {
                    tracing::debug!(error = %e, "days_since_observed_rain query failed");
                    None
                }),
            None => None,
        };
        match observed_days {
            Some(obs) => model_days.min(obs),
            None => model_days,
        }
    };

    // Tomorrow's rain: prefer the live OM forecast snapshot over HA's
    // REST sensor (the latter only refreshes every 4h vs our 30 min).
    let rain_tomorrow_used = if fc.has_tomorrow() {
        rain_tomorrow_om_in
    } else {
        rain_tomorrow
    };

    let heat_index_now = heat_index_f(temp_now, humidity_now);
    let heat_index_3day = heat_index_f(temp_max_3day, humidity_now);
    let heat_mult = et_heat_multiplier(heat_index_3day);

    let forecast = Forecast {
        rain_today_tempest_in: rain_today_station,
        rain_today_om_in: rain_today_om,
        rain_intensity_in_hr: rain_intensity,
        rain_type,
        rain_tomorrow_in: rain_tomorrow_used,
        rain_3day_in: rain_3day,
        eto_today_mm: state_f64(&map, "sensor.open_meteo_eto_today").unwrap_or(0.0),
        eto_tomorrow_mm: state_f64(&map, "sensor.open_meteo_eto_tomorrow").unwrap_or(0.0),
        eto_3day_avg_mm: state_f64(&map, "sensor.open_meteo_eto_3day_avg").unwrap_or(0.0),
        temp_max_today_f: state_f64(&map, "sensor.open_meteo_temp_max_today").unwrap_or(0.0),
        temp_min_today_f: state_f64(&map, "sensor.open_meteo_temp_min_today").unwrap_or(0.0),
        wind_max_today_mph: wind_max_today,
        wind_gust_today_mph: wind_gust_today,
        humidity_mean_today_pct: state_f64(&map, "sensor.open_meteo_humidity_mean_today")
            .unwrap_or(0.0),

        rain_3day_weighted_in: rain_3day_weighted,
        rain_7day_weighted_in: rain_7day_weighted,
        rain_next_4h_in: rain_next_4h,
        rain_tomorrow_prob_pct: rain_tomorrow_prob,
        // Wire shape stays f64 (0.0 = legacy missing-data placeholder);
        // skip_check.temp_min_24h_valid carries the validity bit.
        temp_min_24h_f: temp_min_24h.unwrap_or(0.0),
        temp_max_3day_f: temp_max_3day,
        humidity_now_pct: humidity_now,
        heat_index_now_f: heat_index_now,
        heat_index_max_3day_f: heat_index_3day,
        heat_multiplier: heat_mult,
        days_since_significant_rain: days_since_rain,
    };

    // Native per-zone soil extras (temp/EC/battery) from the gateway poll.
    // Merge them onto the published zones[] and derive the frost gate's yard
    // min/max soil temperature natively, no dependency on an HA soil-temp
    // aggregate (which used to come from the ecowitt2mqtt sidecar).
    let soil_extras = resolve_soil_extras(&watering_policy.soil_zones, sensor_history).await;
    for z in &mut snap.zones {
        if let Some(e) = soil_extras.iter().find(|e| e.slug == z.slug) {
            z.soil_temp_f = e.temp_f;
            z.soil_ec = e.ec;
            z.soil_battery_pct = e.battery_pct;
        }
    }
    let soil_temps: Vec<f64> = soil_extras.iter().filter_map(|e| e.temp_f).collect();
    let soil_temp_yard_min_f = soil_temps.iter().copied().reduce(f64::min);
    let soil_temp_yard_max_f = soil_temps.iter().copied().reduce(f64::max);

    // Resolve each zone's live soil reading once; the engine inputs and
    // the probe-fault detector consume the same list. Falls back to the
    // legacy hardcoded reads when no zone config is present.
    let soil_zones_resolved = if watering_policy.soil_zones.is_empty() {
        build_legacy_soil_zones(&map)
    } else {
        resolve_soil_zones(&watering_policy.soil_zones, &map, sensor_history).await
    };
    // Probe health: a zone with a sensor configured but no usable reading
    // silently widens the yard-wide saturation gate (it goes inapplicable
    // when any zone lacks a reading). Name the dead hardware on the
    // snapshot so the UI, /api/health, and push can surface it.
    snap.soil_probe_faults = detect_soil_probe_faults(
        &watering_policy.soil_zones,
        &soil_zones_resolved,
        sensor_history,
    )
    .await;

    let inputs = Inputs {
        temp_now_f: temp_now,
        wind_now_mph: wind_now,
        rain_today_in: rain_today_used,
        rain_intensity_now_in_hr: rain_intensity,
        humidity_now_pct: humidity_now,

        forecast_in: rain_tomorrow_used,
        rain_tomorrow_prob_pct: rain_tomorrow_prob,
        rain_3day_weighted_in: rain_3day_weighted,
        rain_7day_weighted_in: rain_7day_weighted,
        rain_next_4h_in: rain_next_4h,
        wind_max_today_mph: wind_max_today,
        temp_min_24h_f: temp_min_24h,
        temp_max_3day_f: temp_max_3day,
        days_since_significant_rain: days_since_rain,

        max_wind_mph: state_f64(&map, "input_number.irrigation_max_wind_mph")
            .unwrap_or(watering_policy.skip_rules.max_wind_mph),
        min_temp_f: state_f64(&map, "input_number.irrigation_min_temp_f")
            .unwrap_or(watering_policy.skip_rules.min_temp_f),
        rain_skip_in: state_f64(&map, "input_number.irrigation_rain_skip_in")
            .unwrap_or(watering_policy.skip_rules.rain_skip_in),

        // Per-zone soil readings + thresholds. Resolved above from each
        // zone's assigned sensor (`ha:` entity or `source:<id>:<key>`
        // channel) + ZoneConfig thresholds. None when a sensor is offline;
        // the skip-logic rules silently no-op so missing data falls back
        // to weather-only (with the fault surfaced via soil_probe_faults).
        soil_zones: soil_zones_resolved,
        soil_temp_yard_min_f,
        soil_temp_yard_max_f,
        frost_skip_soil_f: watering_policy.skip_rules.frost_skip_soil_f,

        // Provenance of the live "now" readings (resolved above). The
        // ladder fails safe (skip) when Unavailable and marks the trace
        // degraded on ForecastFallback.
        live_readings,

        is_paused: state_eq(&map, "input_boolean.irrigation_pause", "on"),
        is_dry_run: state_eq(&map, "input_boolean.irrigation_dry_run", "on"),

        // Phase 4 control surfaces. Today's verdict ignores the tomorrow
        // override (is_tomorrow=false); the verdict-strip path below sets
        // it true on the [+1] cell.
        pause_until_epoch: snap.pause_until_epoch,
        now_epoch,
        override_tomorrow: snap.override_tomorrow.clone(),
        is_tomorrow: false,
        // Sticky overrides (native sqlite; set on snap above). The global rides
        // pre_soil; the per-zone map (auto entries dropped) rides decide_per_zone.
        global_override: snap.global_override.clone(),
        zone_overrides: snap
            .zones
            .iter()
            .filter(|z| z.override_mode != "auto")
            .map(|z| (z.slug.clone(), z.override_mode.clone()))
            .collect(),

        // Watering restrictions resolved at boot from localsky.toml and
        // plumbed through spawn_refresher. The skip-rule ladder uses
        // these to short-circuit the live verdict with reason
        // "Watering restriction: <name>" when an active rule blocks
        // today. The seven-day strip path (verdict_strip.rs) gets its
        // own copies from `today`.
        watering_restrictions: watering_policy.restrictions.clone(),
        address_parity: watering_policy.address_parity,
    };
    apply_engine(
        &mut snap,
        &inputs,
        scripts,
        &watering_policy.condition_rules,
        &watering_policy.skip_rules,
    );

    snap.forecast = forecast;
    snap.seven_day_verdicts = compute_seven_day_verdicts(&fc, &inputs, &watering_policy.skip_rules);
    snap.soil_forecasts = compute_soil_forecasts(
        &fc,
        &inputs,
        &map,
        &watering_policy.soil_zones,
        sensor_history,
    )
    .await;
    snap.water_budgets = compute_water_budgets(
        &fc,
        &inputs,
        &map,
        &snap.zones,
        zone_runtime,
        restriction_cap_seconds,
        &watering_policy.budget_zones,
    );

    snap
}

/// Native (no-Home-Assistant) snapshot builder. Reuses `build_from_map`
/// with an EMPTY entity map so every store-preferred read works (weather
/// from ForecastStore/TempestStore; soil via `source:` channels), then
/// overrides the genuinely HA-only fields. Running-state, run-times, and
/// control surfaces are filled by follow-up increments (A4-A6); until then
/// they hold safe defaults (running=false, planned=0 -> nothing waters,
/// master off), so a partially-built native path can never mis-water.
#[allow(clippy::too_many_arguments)]
async fn refresh_once_native(
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
    zones: &[crate::zones::ZoneIdent],
    zone_runtime: &HashMap<String, ZoneRuntime>,
    watering_policy: &WateringPolicy,
    scripts: &CompiledScripts,
    sensor_history: Option<&crate::persistence::SensorHistoryStore>,
    forecast_obs: Option<&crate::persistence::ForecastObservationsStore>,
    controllers: &ControllerRegistry,
    // Locally persisted pause + one-day override (A6). `None` only when no
    // persistence DB is mounted, in which case the snapshot falls back to
    // "no pause / auto override" (and the API rejects pause writes).
    control: Option<&crate::persistence::IrrigationControlState>,
) -> IrrigationSnapshot {
    let map: HashMap<String, Value> = HashMap::new();
    let mut snap = build_from_map(
        map,
        forecast_store,
        tempest_store,
        zones,
        zone_runtime,
        watering_policy,
        scripts,
        sensor_history,
        forecast_obs,
        control,
    )
    .await;
    // Native builds have no remote dependency; the engine is always reachable.
    snap.ha_reachable = true;

    // A4: per-zone running-state + master/water_level from the controllers
    // directly (no HA binary_sensors). Best-effort: a controller that can't
    // report leaves running=false + running_known=false; a status() error
    // is swallowed so a flaky controller never stalls the refresh.
    let cs = native_controller_state(controllers).await;
    for z in snap.zones.iter_mut() {
        match cs.running.get(&z.slug) {
            Some(r) => {
                z.running = *r;
                z.running_known = true;
            }
            None => {
                z.running = false;
                z.running_known = false;
            }
        }
    }
    // Default to enabled / full when no controller reports, so a missing
    // readback never silently suppresses watering.
    snap.master_enable = cs.master.unwrap_or(true);
    snap.water_level_pct = cs.water.unwrap_or(100.0);
    // Flow: capability flag + live GPM straight from the controller. Stays
    // None when no meter so the UI / HA surface nothing for non-flow setups.
    snap.flow_meter = cs.flow_meter;
    snap.flow_gpm = cs.flow_gpm;

    // A5: native run-times. Without HA there's no Smart Irrigation bucket,
    // so size each zone's run from LocalSky's own weekly-budget allocator
    // (`compute_water_budgets` already ran inside build_from_map and is in
    // `snap.water_budgets`; its `today_seconds` is the capped per-zone
    // recommendation, rain-defer, session spacing, and max-duration all
    // applied). A manual Override schedule for today still zeroes the smart
    // dispatch so it doesn't run on top of the operator's planned run.
    let planned_by_slug: HashMap<String, u32> = snap
        .water_budgets
        .iter()
        .map(|b| (b.zone_slug.clone(), b.today_seconds))
        .collect();
    let today_weekday: u8 = {
        use chrono::Datelike;
        chrono::Local::now().weekday().num_days_from_sunday() as u8
    };
    for z in snap.zones.iter_mut() {
        let budget_seconds = planned_by_slug.get(&z.slug).copied().unwrap_or(0);
        let override_active = crate::scheduler::manual::override_active_today(
            &watering_policy.manual_schedules,
            &z.slug,
            today_weekday,
        );
        z.planned_run_seconds = if override_active { 0 } else { budget_seconds };
        if let Some(m) = z.math.as_mut() {
            m.scheduled_seconds = z.planned_run_seconds;
        }
    }
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;
    snap.next_run_epoch = compute_next_run_epoch(watering_policy.location, &snap.zones);

    // A6: pause / override come from `control` (threaded into build_from_map
    // above); thresholds come from cfg.engine.skip_rules via watering_policy.
    snap
}

/// Query every configured controller once for live state and merge it:
/// per-zone running (by slug), plus the first reported master-enable +
/// water-level. Errors are swallowed (best-effort, never fails a refresh).
async fn native_controller_state(controllers: &ControllerRegistry) -> NativeControllerState {
    let mut running: HashMap<String, bool> = HashMap::new();
    let mut master: Option<bool> = None;
    let mut water: Option<f64> = None;
    let mut flow_gpm: Option<f64> = None;
    let mut flow_meter = false;
    for id in controllers.ids() {
        let Some(c) = controllers.get(&id) else {
            continue;
        };
        // The capability flag comes from supports(), not status(), so a
        // controller with a meter that momentarily reports flow_gpm=None
        // still advertises the capability.
        if c.supports().flow_meter {
            flow_meter = true;
        }
        if let Ok(st) = c.status().await {
            for z in st.zone_states {
                running.insert(z.slug, z.running);
            }
            if master.is_none() {
                master = st.master_enabled;
            }
            if water.is_none() {
                water = st.water_level_pct;
            }
            // First controller to report measured flow wins (matches the
            // master/water "first non-None" merge above).
            if flow_gpm.is_none() {
                flow_gpm = st.flow_gpm;
            }
        }
    }
    NativeControllerState {
        running,
        master,
        water,
        flow_gpm,
        flow_meter,
    }
}

/// Merged live readback from all configured controllers, gathered once per
/// native refresh. Best-effort: a controller that can't report contributes
/// nothing rather than failing the refresh.
struct NativeControllerState {
    running: HashMap<String, bool>,
    master: Option<bool>,
    water: Option<f64>,
    flow_gpm: Option<f64>,
    flow_meter: bool,
}

/// Run the decision engine against `inputs` and write the results into the
/// snapshot: aggregate skip_check + decision_trace, the augment-only Rhai
/// script pass, and per-zone verdicts (back-filled onto each ZoneState).
/// Shared by the HA and native snapshot builders so the watering decision
/// is byte-identical regardless of how the inputs were gathered.
fn apply_engine(
    snap: &mut IrrigationSnapshot,
    inputs: &Inputs,
    scripts: &CompiledScripts,
    condition_rules: &[crate::engine::conditions::ConditionRule],
    // Operator-tuned thresholds from cfg.engine.skip_rules (threaded via
    // WateringPolicy). Previously this constructed SkipRuleParams::default()
    // locally, which silently discarded 8 of the 12 user-tunable knobs
    // (already_wet_in, rain_now_in_hr, rain_next_4h_skip_in,
    // rain_3day_factor, the three heat-advisory gates, and
    // wind_forecast_slack_mph). Defaults are unchanged, so untouched
    // configs decide identically.
    params: &crate::config::schema::SkipRuleParams,
) {
    snap.skip_check = skip_logic::evaluate_with(inputs, params);
    // Structured provenance for the same decision (powers Rule Lab).
    snap.decision_trace = Some(crate::engine::skip_rules::decide_traced(inputs, params));

    // Augment-only user scripts: consulted ONLY when the deterministic
    // ladder said "run", so a script can ADD a skip but can never clear a
    // freeze / wind / restriction gate. Fail-safe: errors are no-ops.
    if !scripts.is_empty() && snap.skip_check.verdict == "run" {
        if let Some(us) = scripts.apply_user_skip(inputs) {
            snap.skip_check.verdict = "skip".to_string();
            snap.skip_check.will_skip = true;
            snap.skip_check.reason = us.reason.clone();
            if let Some(t) = snap.decision_trace.as_mut() {
                t.verdict = "skip".to_string();
                t.reason = us.reason.clone();
                t.rules.push(RuleEval {
                    id: us.id,
                    label: us.name,
                    category: "script".to_string(),
                    detail: "user Rhai rule".to_string(),
                    outcome: "fired".to_string(),
                    verdict: Some("skip".to_string()),
                });
            }
        }
    }

    // Per-zone verdicts: global gates bind every zone, then per-zone soil
    // saturation + user condition rules let zones diverge. Augment-only.
    let verdicts = crate::engine::skip_rules::decide_per_zone(inputs, params, condition_rules);
    for z in snap.zones.iter_mut() {
        z.verdict = verdicts.iter().find(|v| v.zone_slug == z.slug).cloned();
    }
    snap.zone_verdicts = verdicts;
}

/// Phase H, weekly water-budget plan per zone. Replaces SI's daily-bucket
/// flex math with a deep-and-infrequent schedule that allocates a weekly
/// water target across N sessions, defers when rain is forecast, and
/// spaces sessions by `7 / sessions_per_week` days so each run is a real
/// soak rather than a daily light sprinkle.
///
/// Outputs `today_seconds` per zone, what the HA budget-override
/// automation at 23:30:25 calls `IU.adjust_time(actual=...)` with. Zero
/// means "don't run this zone today" (rain incoming, recently watered,
/// or mode is off).
fn compute_water_budgets(
    fc: &ForecastSnapshot,
    today_inputs: &Inputs,
    map: &HashMap<String, Value>,
    zones: &[ZoneState],
    zone_runtime: &HashMap<String, ZoneRuntime>,
    restriction_cap_seconds: Option<u32>,
    // A5b: per-zone budget config from cfg.zones. Drives which zones the
    // allocator plans for (so any configured zone gets a run-time). Empty =
    // unconfigured install -> nothing to plan until the wizard writes zones.
    budget_zones: &[ZoneBudgetCfg],
) -> Vec<WaterBudget> {
    // Iteration list: every configured zone (A5b). Per-zone budget/sessions
    // are None here and resolved below from HA input_number -> config ->
    // agronomic slug default.
    let iter_zones: Vec<ZoneBudgetCfg> = budget_zones.to_vec();
    const CAPTURE_EFFICIENCY: f64 = 0.7;
    const SESSION_RAIN_DEFER_IN: f64 = 0.10; // ≥0.10" forecast next 24h → defer

    let heat_mult = today_inputs.temp_max_3day_f.max(0.0); // dummy to silence unused; real heat below
    let _ = heat_mult; // suppress warning
    let heat_mult_eff = {
        let hi = heat_index_f(today_inputs.temp_max_3day_f, today_inputs.humidity_now_pct);
        et_heat_multiplier(hi)
    };

    let now_epoch = chrono::Utc::now().timestamp();
    // Forecast: next-24h rain (sum of hourly[0..24] precip).
    let next_24h_rain_in = fc.next_n_hours_precip_in(24);
    // 7-day probability-weighted total rain.
    let week_rain_weighted_in: f64 = fc
        .daily
        .iter()
        .take(7)
        .map(|d| d.precip_sum_in * (d.precip_probability_max as f64) / 100.0)
        .sum();

    let mut out = Vec::with_capacity(iter_zones.len());
    for zone_cfg in iter_zones.iter() {
        let slug = zone_cfg.slug.as_str();
        let name = zone_cfg.name.as_str();
        let (default_budget_in, default_sessions) = agronomic_budget_default(slug);
        // Precedence: live HA input_number helper (HA path, unchanged) ->
        // per-zone config value (native + HA fallback, A5b) -> agronomic
        // slug default. (HA helpers carry no initial: per the established
        // convention so recorder restore_state preserves operator edits.)
        let weekly_budget_in = state_f64(
            map,
            &format!("input_number.irrigation_{slug}_weekly_budget_in"),
        )
        .or(zone_cfg.weekly_budget_in)
        .unwrap_or(default_budget_in);
        let sessions_per_week = state_f64(
            map,
            &format!("input_number.irrigation_{slug}_sessions_per_week"),
        )
        .map(|v| v.round() as u32)
        .or(zone_cfg.sessions_per_week)
        .unwrap_or(default_sessions)
        .max(1);
        // Budget mode used to be a per-zone HA toggle while the SI -> LocalSky
        // cutover was in progress (off = SI owned the zone's daily flex, on =
        // LocalSky's weekly budget did). Post-cutover LocalSky is the only
        // source of truth, so the toggle is force-on regardless of the HA
        // helper's state. The off-mode branch below is kept as a defensive
        // fallback only.
        let mode_active = true;
        let _ = state_eq(
            map,
            &format!("input_boolean.irrigation_{slug}_weekly_budget_mode"),
            "on",
        );

        // Throughput + max-duration come from LocalSky's zone config
        // (catalog default by sprinkler_type, optional precip_rate_mm_hr
        // override). SI's `throughput` and `maximum_duration` attrs are
        // ignored so the budget allocator no longer drifts when SI is
        // paused or its zone is misconfigured.
        let rt = zone_runtime
            .get(slug)
            .copied()
            .unwrap_or_else(ZoneRuntime::fallback);
        let throughput_mm_hr = rt.throughput_mm_hr;
        // Active watering restriction cap (if any) tightens the budget-path
        // ceiling too. Same min-of-two rule as the daily-bucket path above.
        let max_dur_s = match restriction_cap_seconds {
            Some(c) => rt.max_duration_s.min(c),
            None => rt.max_duration_s,
        };

        // Water-balance math: weekly budget, minus expected captured rain.
        let weekly_budget_mm = weekly_budget_in * 25.4;
        let expected_rain_mm = week_rain_weighted_in * 25.4 * CAPTURE_EFFICIENCY;
        let needed_mm = (weekly_budget_mm - expected_rain_mm).max(0.0);
        let mm_per_session = needed_mm / sessions_per_week as f64;
        let seconds_per_session = if throughput_mm_hr > 0.0 {
            // Multiply by heat_mult to compensate for accelerated ET
            // (same Kc-style bias SI applies). Divide by CAPTURE_EFFICIENCY
            // so that the *root-zone* depth matches mm_per_session after
            // runoff/canopy losses.
            ((mm_per_session / throughput_mm_hr) * 3600.0 * heat_mult_eff / CAPTURE_EFFICIENCY)
                as u32
        } else {
            0
        };
        let session_capped = seconds_per_session > max_dur_s;
        let session_final = seconds_per_session.min(max_dur_s);

        // Last run epoch for this zone, pulled from ZoneState (which the
        // history ingest populates) so we don't have to round-trip SQLite.
        let last_run_epoch = zones
            .iter()
            .find(|z| z.slug == slug)
            .map(|z| z.last_run_epoch)
            .unwrap_or(0);

        // Today's recommendation.
        let min_interval_days = (7.0 / sessions_per_week as f64).floor() as i64;
        let days_since_last_run = if last_run_epoch > 0 {
            (now_epoch - last_run_epoch) / 86400
        } else {
            i64::MAX / 2
        };
        let (today_seconds, today_reason) = if !mode_active {
            // Defensive only, `mode_active` is hard-coded above. Kept for
            // future re-introduction of a per-zone pause toggle.
            (0u32, "budget mode off".to_string())
        } else if next_24h_rain_in >= SESSION_RAIN_DEFER_IN {
            (
                0,
                format!(
                    "rain expected next 24h ({:.2}\" forecast ≥ {:.2}\")",
                    next_24h_rain_in, SESSION_RAIN_DEFER_IN
                ),
            )
        } else if days_since_last_run < min_interval_days {
            (
                0,
                format!(
                    "last run {} day(s) ago, minimum interval is {} days at {} sessions/wk",
                    days_since_last_run, min_interval_days, sessions_per_week
                ),
            )
        } else if needed_mm <= 0.0 {
            (
                0,
                format!(
                    "forecast rain {:.2}\" covers the {:.2}\" weekly budget",
                    week_rain_weighted_in, weekly_budget_in
                ),
            )
        } else {
            (
                session_final,
                format!(
                    "scheduled session {} of {} this week, {:.2} mm depth = {:.0} min",
                    1, // session_index, proper allocation logic deferred
                    sessions_per_week,
                    mm_per_session,
                    session_final as f64 / 60.0
                ),
            )
        };

        out.push(WaterBudget {
            zone_slug: slug.to_string(),
            zone_name: name.to_string(),
            mode_active,
            weekly_budget_in,
            sessions_per_week,
            expected_rain_mm,
            needed_mm,
            mm_per_session,
            seconds_per_session,
            session_capped,
            last_run_epoch,
            today_seconds,
            today_reason,
        });
    }
    out
}

/// Phase E predictive, per-zone 7-day soil-moisture projection. Uses a
/// FAO-56-flavored water balance: today's calibrated reading is the
/// starting point; each day subtracts the daily ET (scaled by zone Kc)
/// and adds the probability-weighted forecast rain (scaled by a capture
/// efficiency factor to account for runoff). Irrigation is not modeled
///, the curve answers "if I did nothing all week, would each zone stay
/// in its healthy band?"
///
/// Assumptions baked into the heuristic:
///   - Single ET value (today's, from HA's open-meteo eto_today sensor)
///     carries across the full 7-day window. Open-Meteo's daily-ET vector
///     isn't currently in localsky's ForecastSnapshot; the constant
///     approximation is good enough for the dashboard view.
///   - Per-zone soil depth + Kc are hardcoded to match SI's zone
///     multipliers (turf 1.08 / shrubs 0.50) so the predicted depletion
///     matches what SI would have computed in mm.
///   - Rain capture efficiency 0.7, empirical, accounts for runoff,
///     slope, and canopy interception. Knock-down values not modeled.
///   - Probe placement at root depth (operator's responsibility).
/// Effective Kc + root-zone depth (mm) for a zone, inferred from its slug.
/// Turf has shallower active roots than mulched shrubs/beds so equivalent
/// ET drops its moisture % faster. Heuristic so config-driven zones get
/// sensible projection tuning without extra config fields.
fn kc_depth_for(slug: &str) -> (f64, f64) {
    if slug.contains("shrub") || slug.contains("garden") || slug.contains("bed") {
        (0.50, 200.0)
    } else {
        (1.08, 150.0)
    }
}

/// Agronomic weekly-budget default `(weekly_budget_in, sessions_per_week)`
/// for a zone, inferred from its slug when neither an HA helper nor config
/// sets one (A5b). Mirrors the same shrub/garden/bed heuristic as
/// `kc_depth_for`: mulched beds need less water, less often than turf.
/// The values reproduce the legacy hardcoded compute_water_budgets defaults
/// (turf 1.0"/2 sessions, shrub/garden/bed 0.5"/1) so existing zones are
/// unchanged.
fn agronomic_budget_default(slug: &str) -> (f64, u32) {
    if slug.contains("shrub") || slug.contains("garden") || slug.contains("bed") {
        (0.50, 1)
    } else {
        (1.00, 2)
    }
}

/// One zone's soil-forecast inputs, resolved from config (or the legacy
/// hardcoded 4 when no zone config is present).
struct ForecastZone {
    slug: String,
    name: String,
    sensor: Option<String>,
    target_min: f64,
    target_max: f64,
    kc: f64,
    depth: f64,
}

async fn compute_soil_forecasts(
    fc: &ForecastSnapshot,
    today: &Inputs,
    map: &HashMap<String, Value>,
    zone_cfg: &[ZoneSoilCfg],
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Vec<SoilForecast> {
    // Build the working zone list from config. Empty config = unconfigured
    // install -> no soil forecasts until the wizard writes zones.
    let zones: Vec<ForecastZone> = zone_cfg
        .iter()
        .map(|z| {
            let (kc, depth) = kc_depth_for(&z.slug);
            ForecastZone {
                slug: z.slug.clone(),
                name: z.name.clone(),
                sensor: z.soil_sensor_id.clone(),
                target_min: z.target_min_pct,
                target_max: z.saturation_pct,
                kc,
                depth,
            }
        })
        .collect();
    const CAPTURE_EFFICIENCY: f64 = 0.7;

    // Daily ET, mm. Today's value carries across the window. heat_multiplier
    // bumps it on heat-advisory days so a 95°F+ forecast tracks realistically.
    let et0_today_mm = state_f64(map, "sensor.open_meteo_eto_today").unwrap_or(5.0);
    let daily_et_mm = et0_today_mm * fc_heat_multiplier(today);

    let n_days = fc.daily.len().min(7).max(1);
    let mut out = Vec::with_capacity(zones.len());

    for z in zones.iter() {
        let slug = &z.slug;
        let name = &z.name;
        let kc = z.kc;
        let soil_depth_mm = z.depth;
        let target_min = z.target_min;
        let target_max = z.target_max;
        // Resolve this zone's live reading via its assigned sensor, with the
        // same offline guard + calibration the decision path uses.
        let current = apply_soil_quality(resolve_soil_pct(z.sensor.as_deref(), map, history).await);

        // No probe data → emit a no_data entry the dashboard renders as
        // a grey "(probe offline)" tile rather than rendering a flat zero.
        let Some(start_pct) = current else {
            out.push(SoilForecast {
                zone_slug: slug.to_string(),
                zone_name: name.to_string(),
                current_pct: None,
                target_min_pct: target_min,
                target_max_pct: target_max,
                predicted_pct: vec![0.0; n_days],
                min_predicted_pct: 0.0,
                max_predicted_pct: 0.0,
                days_below_target: 0,
                days_above_max: 0,
                status: "no_data".to_string(),
            });
            continue;
        };

        let mut series = Vec::with_capacity(n_days);
        let mut moisture = start_pct;
        series.push(moisture);

        // Step through each future day applying the water-balance delta.
        // Day 0 is "today" (the current reading), so the deltas start at
        // day 1 using daily[0]'s rain prediction (the rest of today) and
        // daily[N]'s rain for day N onward.
        for d in fc.daily.iter().take(n_days).skip(1) {
            let rain_effective_mm =
                d.precip_sum_in * 25.4 * (d.precip_probability_max as f64) / 100.0;
            let captured_mm = rain_effective_mm * CAPTURE_EFFICIENCY;
            let et_loss_mm = daily_et_mm * kc;
            let delta_mm = captured_mm - et_loss_mm;
            let delta_pct = delta_mm / soil_depth_mm * 100.0;
            moisture = (moisture + delta_pct).clamp(0.0, 100.0);
            series.push(moisture);
        }

        let min_predicted = series
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .max(0.0);
        let max_predicted = series
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
            .min(100.0);
        let days_below = series.iter().filter(|p| **p <= target_min).count() as u32;
        let days_above = series.iter().filter(|p| **p >= target_max).count() as u32;

        // Status classification: "wet" wins over "dry" so a saturated
        // start doesn't get flagged as dry from a forecast dry stretch
        // that hasn't happened yet. "dry" requires either crossing the
        // target_min floor at any point OR ≥2 days under it.
        let status = if max_predicted >= target_max {
            "wet"
        } else if min_predicted <= target_min || days_below >= 2 {
            "dry"
        } else {
            "ok"
        };

        out.push(SoilForecast {
            zone_slug: slug.to_string(),
            zone_name: name.to_string(),
            current_pct: Some(start_pct),
            target_min_pct: target_min,
            target_max_pct: target_max,
            predicted_pct: series,
            min_predicted_pct: min_predicted,
            max_predicted_pct: max_predicted,
            days_below_target: days_below,
            days_above_max: days_above,
            status: status.to_string(),
        });
    }

    out
}

/// Pull the heat_multiplier the engine has already computed for today's
/// Inputs (avoids recomputing the NOAA Steadman heat index from scratch).
/// The multiplier bumps daily ET on heat-advisory days so the projection
/// tracks the same depletion-acceleration SI applies to its bucket math.
fn fc_heat_multiplier(today: &Inputs) -> f64 {
    let hi = heat_index_f(today.temp_max_3day_f, today.humidity_now_pct);
    et_heat_multiplier(hi)
}

/// Compute the 7-day forward verdict strip. For each daily forecast
/// entry (today + 6 future days), construct synthetic Inputs that
/// answer "would I water on this day?" and run the same evaluate()
/// the morning skip-check uses. Same engine, same rules, the strip
/// is a *preview* of the actual decision, not a separate heuristic.
///
/// Synthetic-input rules:
///   - rain_today = daily[N].precip_sum
///   - forecast_in = daily[N+1].precip_sum (or 0 if past horizon)
///   - rain_3day_weighted = Σ daily[N+1..N+4] × prob/100
///   - temp_min_24h = daily[N].temp_min  (best stand-in we have)
///   - temp_max_3day = max(daily[N..N+3].temp_max)
///   - wind_max_today = daily[N].wind_max
///   - humidity_now: carry today's value (forecast humidity not in OM daily)
///   - days_since_significant_rain: scan the past+now window forward through
///     daily[..N] looking for ≥0.05 days, falling back to past_daily.
///   - rain_intensity_now/wind_now/temp_now: 0 / forecast_wind / temp_min
///     respectively (so the live-only rules don't fire on a forecast day).
fn compute_seven_day_verdicts(
    fc: &ForecastSnapshot,
    today: &Inputs,
    // Operator-tuned thresholds (cfg.engine.skip_rules), same params the
    // live decision uses, so the strip previews the real ladder rather
    // than a defaults-only shadow of it.
    params: &crate::config::schema::SkipRuleParams,
) -> Vec<DayVerdict> {
    crate::engine::compute_verdict_strip(fc, today, params)
}

/// Smart-morning target_start epoch for the next morning that hasn't
/// already passed. Returns 0 when location is unset or sunrise can't be
/// computed (polar latitudes on the date in question), matching the
/// snapshot's default sentinel.
fn compute_next_run_epoch(location: (f64, f64), zones: &[crate::ha::snapshot::ZoneState]) -> i64 {
    use crate::engine::sunrise::smart_morning_target_start;
    const INTER_ZONE_PREAMBLE_S: u64 = 2;

    let (lat, lon) = location;
    if lat == 0.0 && lon == 0.0 {
        return 0;
    }
    let total_dispatch_s: u64 = zones.iter().map(|z| z.planned_run_seconds as u64).sum();
    let zones_to_run = zones.iter().filter(|z| z.planned_run_seconds > 0).count();
    let sequence_total_s =
        total_dispatch_s + INTER_ZONE_PREAMBLE_S * (zones_to_run.saturating_sub(1)) as u64;

    let now = chrono::Local::now();
    let today_local = now.date_naive();

    if let Some(today_target) = smart_morning_target_start(today_local, lat, lon, sequence_total_s)
    {
        if today_target > now.with_timezone(&chrono::Utc) {
            return today_target.timestamp();
        }
    }
    // Today's window already passed; advance to tomorrow.
    if let Some(tomorrow) = today_local.succ_opt() {
        if let Some(t) = smart_morning_target_start(tomorrow, lat, lon, sequence_total_s) {
            return t.timestamp();
        }
    }
    0
}

fn state_eq(map: &HashMap<String, Value>, eid: &str, expected: &str) -> bool {
    map.get(eid)
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map(|s| s == expected)
        .unwrap_or(false)
}

fn state_f64(map: &HashMap<String, Value>, eid: &str) -> Option<f64> {
    map.get(eid)
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
}

/// How fresh the latest Tempest packet must be (seconds) for the station
/// to keep driving the live "now" inputs. Tempest obs_st arrives every
/// minute under normal conditions; 10 minutes of silence means the radio
/// path is down and the readings are no longer "now".
const TEMPEST_LIVE_MAX_AGE_S: i64 = 600;

/// Resolve the live current conditions (temp °F, wind mph, humidity %)
/// plus their provenance:
///   1. Tempest station, when its last packet is within
///      `TEMPEST_LIVE_MAX_AGE_S` of `now_epoch` → `Station`.
///   2. Current-hour forecast (Open-Meteo hourly[0]) → `ForecastFallback`
///      (decision trace marked degraded; rules still evaluate).
///   3. Neither → `Unavailable` with neutral zeros; the engine's
///      live-data gate then fails safe with a skip, so the placeholder
///      values never reach a run/skip comparison.
/// This replaced hard-coded fallbacks to one specific install's HA
/// entities (`sensor.st_00206451_*`) with `unwrap_or(70.0)` / `0.0`,
/// which fabricated 70 °F / 0 mph for every standalone non-Tempest user.
fn resolve_current_conditions(
    tempest: &crate::tempest::state::Snapshot,
    current_hour: Option<&crate::forecast::snapshot::HourlyEntry>,
    now_epoch: i64,
) -> (f64, f64, f64, LiveReadings) {
    let station_fresh = tempest.last_packet_epoch > 0
        && now_epoch.saturating_sub(tempest.last_packet_epoch) < TEMPEST_LIVE_MAX_AGE_S;
    if station_fresh {
        return (
            tempest.air_temp_f,
            tempest.wind_avg_mph,
            tempest.rh_pct,
            LiveReadings::Station,
        );
    }
    if let Some(h) = current_hour {
        return (
            h.temp_f,
            h.wind_mph,
            h.humidity_pct as f64,
            LiveReadings::ForecastFallback,
        );
    }
    (0.0, 0.0, 0.0, LiveReadings::Unavailable)
}

/// Resolve a zone's assigned soil sensor to a live %. Supports three
/// address forms:
///   - `ha:sensor.x`        → HA entity state
///   - `source:<id>:<key>`  → latest sensor_history reading for that
///                            source channel (Ecowitt etc.)
///   - bare `sensor.x`      → HA entity (legacy / back-compat)
/// `None` when unassigned or the reading is unavailable.
async fn resolve_soil_pct(
    spec: Option<&str>,
    map: &HashMap<String, Value>,
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Option<f64> {
    let spec = spec?;
    if let Some(entity) = spec.strip_prefix("ha:") {
        return state_f64(map, entity);
    }
    if let Some(rest) = spec.strip_prefix("source:") {
        let (sid, key) = rest.split_once(':')?;
        let h = history?;
        return h
            .last_value(sid.to_string(), key.to_string())
            .await
            .ok()
            .flatten()
            .map(|r| r.value);
    }
    // Bare string: treat as an HA entity id (legacy configs).
    state_f64(map, spec)
}

/// Build the engine's per-zone soil list from the boot-resolved zone
/// config, pulling each zone's live reading via `resolve_soil_pct`.
async fn resolve_soil_zones(
    cfg: &[ZoneSoilCfg],
    map: &HashMap<String, Value>,
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Vec<ZoneSoil> {
    let mut out = Vec::with_capacity(cfg.len());
    for z in cfg {
        let raw = resolve_soil_pct(z.soil_sensor_id.as_deref(), map, history).await;
        let pct = apply_soil_quality(raw);
        out.push(ZoneSoil {
            slug: z.slug.clone(),
            name: z.name.clone(),
            pct,
            saturation_pct: z.saturation_pct,
            target_min_pct: z.target_min_pct,
        });
    }
    out
}

/// How long a configured soil channel may go without a valid (> 0)
/// reading before it is reported as faulted. One missed gateway poll is
/// noise; a full day of zeros is dead hardware.
const SOIL_PROBE_FAULT_AFTER_S: i64 = 24 * 3600;

/// Detect configured-but-dead soil probes. A zone is faulted when it has
/// a soil sensor configured, its resolved pct is None (missing or <= 0.0,
/// see `apply_soil_quality`), AND sensor_history confirms persistence:
/// the channel's last reading above 0.0 is older than 24h, or it never
/// produced one. A dead WH51 keeps writing 0.0 rows, so the last
/// above-zero epoch is the signal. Only `source:` channels are checked;
/// an `ha:` entity has no local history to distinguish a flatline from a
/// transient blip, so it is never flagged here.
async fn detect_soil_probe_faults(
    cfg: &[ZoneSoilCfg],
    resolved: &[ZoneSoil],
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Vec<crate::ha::snapshot::SoilProbeFault> {
    let Some(h) = history else {
        return Vec::new();
    };
    let now = Utc::now().timestamp();
    let mut out = Vec::new();
    for z in cfg {
        let Some(spec) = z.soil_sensor_id.as_deref() else {
            continue;
        };
        // Healthy: the resolved reading is usable.
        if resolved
            .iter()
            .find(|r| r.slug == z.slug)
            .and_then(|r| r.pct)
            .is_some()
        {
            continue;
        }
        let Some((sid, key)) = spec
            .strip_prefix("source:")
            .and_then(|rest| rest.split_once(':'))
        else {
            continue;
        };
        let since_epoch = h
            .last_value_above(sid.to_string(), key.to_string(), 0.0)
            .await
            .ok()
            .flatten()
            .map(|r| r.epoch);
        let stale = match since_epoch {
            Some(e) => now.saturating_sub(e) >= SOIL_PROBE_FAULT_AFTER_S,
            None => true,
        };
        if !stale {
            continue;
        }
        out.push(crate::ha::snapshot::SoilProbeFault {
            zone_slug: z.slug.clone(),
            zone_name: z.name.clone(),
            sensor_id: spec.to_string(),
            since_epoch,
        });
    }
    out
}

/// Native per-zone soil extras (temp / EC / battery) resolved alongside
/// moisture but kept OFF the engine's `ZoneSoil` (no skip rule consumes them).
/// Published to HA via the snapshot `zones[]` and used to derive the frost
/// gate's yard-min soil temperature.
#[derive(Debug, Clone, Default)]
struct ZoneSoilExtra {
    slug: String,
    temp_f: Option<f64>,
    ec: Option<f64>,
    battery_pct: Option<f64>,
}

/// Resolve the native temp/EC/battery sibling channels for every configured
/// zone whose moisture is a `source:<id>:soilmoisture<N>` channel.
async fn resolve_soil_extras(
    cfg: &[ZoneSoilCfg],
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Vec<ZoneSoilExtra> {
    let mut out = Vec::with_capacity(cfg.len());
    for z in cfg {
        let spec = z.soil_sensor_id.as_deref();
        out.push(ZoneSoilExtra {
            slug: z.slug.clone(),
            temp_f: resolve_soil_sibling(spec, |n| format!("soiltemp{n}f"), history).await,
            ec: resolve_soil_sibling(spec, |n| format!("soilec{n}"), history).await,
            battery_pct: resolve_soil_sibling(spec, |n| format!("soilbatt{n}"), history).await,
        });
    }
    out
}

/// Resolve a per-channel sibling reading (soil temp / EC / battery) for a zone
/// whose moisture sensor is a native `source:<id>:soilmoisture<N>` channel, by
/// swapping the key suffix and reading the latest history value for the same
/// source + channel. Returns `None` for non-`source:` specs (e.g. an `ha:`
/// entity has no native sibling) or when the reading is unavailable.
async fn resolve_soil_sibling(
    moisture_spec: Option<&str>,
    sibling_key: impl Fn(&str) -> String,
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Option<f64> {
    let rest = moisture_spec?.strip_prefix("source:")?;
    let (sid, key) = rest.split_once(':')?;
    let n = key.strip_prefix("soilmoisture")?;
    let h = history?;
    h.last_value(sid.to_string(), sibling_key(n))
        .await
        .ok()
        .flatten()
        .map(|r| r.value)
}

/// Build the legacy four soil zones from their hardcoded HA entities +
/// input_number thresholds. Fallback when no zone config is present.
fn build_legacy_soil_zones(map: &HashMap<String, Value>) -> Vec<ZoneSoil> {
    [
        ("back_yard", "back yard", 70.0, 30.0),
        ("front_yard", "front yard", 70.0, 30.0),
        ("side_yard", "side yard", 70.0, 30.0),
        ("back_yard_shrubs", "back yard shrubs", 85.0, 25.0),
    ]
    .into_iter()
    .map(|(slug, name, sat_default, target_min)| ZoneSoil {
        slug: slug.into(),
        name: name.into(),
        // Offline guard: a raw 0% reading is a disconnected probe, not
        // bone-dry soil.
        pct: apply_soil_quality(state_f64(map, &format!("sensor.{slug}_soil_moisture"))),
        saturation_pct: state_f64(
            map,
            &format!("input_number.irrigation_{slug}_saturation_pct"),
        )
        .unwrap_or(sat_default),
        target_min_pct: target_min,
    })
    .collect()
}

#[cfg(test)]
mod engine_params_tests {
    use super::*;

    fn base_inputs() -> Inputs {
        Inputs {
            temp_now_f: 70.0,
            wind_now_mph: 3.0,
            wind_max_today_mph: 6.0,
            temp_min_24h_f: Some(60.0),
            temp_max_3day_f: 80.0,
            humidity_now_pct: 55.0,
            days_since_significant_rain: 1,
            max_wind_mph: 10.0,
            min_temp_f: 38.0,
            rain_skip_in: 0.25,
            frost_skip_soil_f: 35.0,
            now_epoch: 1_700_000_000,
            ..Default::default()
        }
    }

    /// Regression for the params-threading fix: a non-default
    /// `already_wet_in` must reach the live decision. Before the fix,
    /// apply_engine constructed SkipRuleParams::default() locally, so
    /// the operator's config value never changed any verdict.
    #[test]
    fn user_already_wet_threshold_flips_verdict() {
        let scripts = CompiledScripts::compile(&[]);
        let mut inputs = base_inputs();
        inputs.rain_today_in = 0.07;

        // Default threshold (0.05"): 0.07" today is "already wet" -> skip.
        let mut snap = IrrigationSnapshot::default();
        let defaults = crate::config::schema::SkipRuleParams::default();
        apply_engine(&mut snap, &inputs, &scripts, &[], &defaults);
        assert_eq!(snap.skip_check.verdict, "skip");
        assert!(snap.skip_check.reason.starts_with("Already wet"));

        // Operator raises the floor to 0.10": the same inputs must run.
        let mut tuned = crate::config::schema::SkipRuleParams::default();
        tuned.already_wet_in = 0.10;
        let mut snap2 = IrrigationSnapshot::default();
        apply_engine(&mut snap2, &inputs, &scripts, &[], &tuned);
        assert_eq!(snap2.skip_check.verdict, "run");
        // The trace must agree (same params reach decide_traced).
        assert_eq!(snap2.decision_trace.as_ref().unwrap().verdict, "run");
    }
}

#[cfg(test)]
mod current_conditions_tests {
    use super::{resolve_current_conditions, LiveReadings, TEMPEST_LIVE_MAX_AGE_S};
    use crate::forecast::snapshot::HourlyEntry;
    use crate::tempest::state::Snapshot as TempestSnapshot;

    const NOW: i64 = 1_700_000_000;

    fn tempest(last_packet_epoch: i64) -> TempestSnapshot {
        TempestSnapshot {
            last_packet_epoch,
            air_temp_f: 61.5,
            wind_avg_mph: 4.2,
            rh_pct: 71.0,
            ..Default::default()
        }
    }

    fn hour() -> HourlyEntry {
        HourlyEntry {
            temp_f: 55.0,
            wind_mph: 7.5,
            humidity_pct: 64,
            ..Default::default()
        }
    }

    #[test]
    fn fresh_station_drives_live_inputs() {
        let t = tempest(NOW - 90);
        let h = hour();
        let (temp, wind, rh, src) = resolve_current_conditions(&t, Some(&h), NOW);
        assert_eq!(src, LiveReadings::Station);
        assert_eq!(temp, 61.5);
        assert_eq!(wind, 4.2);
        assert_eq!(rh, 71.0);
    }

    #[test]
    fn stale_station_falls_back_to_current_hour_forecast() {
        // Packet seen, but older than the recency window: the old
        // "ever-seen" check (last_packet_epoch > 0) would have kept the
        // dead station's readings live forever.
        let t = tempest(NOW - TEMPEST_LIVE_MAX_AGE_S - 1);
        let h = hour();
        let (temp, wind, rh, src) = resolve_current_conditions(&t, Some(&h), NOW);
        assert_eq!(src, LiveReadings::ForecastFallback);
        assert_eq!(temp, 55.0);
        assert_eq!(wind, 7.5);
        assert_eq!(rh, 64.0);
    }

    #[test]
    fn never_seen_station_with_forecast_is_fallback() {
        let t = tempest(0);
        let h = hour();
        let (_, _, _, src) = resolve_current_conditions(&t, Some(&h), NOW);
        assert_eq!(src, LiveReadings::ForecastFallback);
    }

    #[test]
    fn no_station_and_no_forecast_is_unavailable() {
        let t = tempest(0);
        let (temp, wind, _, src) = resolve_current_conditions(&t, None, NOW);
        assert_eq!(src, LiveReadings::Unavailable);
        // Neutral zeros, never the old fabricated 70 °F.
        assert_eq!(temp, 0.0);
        assert_eq!(wind, 0.0);
    }

    #[test]
    fn boundary_age_is_stale() {
        let t = tempest(NOW - TEMPEST_LIVE_MAX_AGE_S);
        let (_, _, _, src) = resolve_current_conditions(&t, None, NOW);
        assert_eq!(src, LiveReadings::Unavailable);
    }
}

#[cfg(test)]
mod budget_default_tests {
    use super::agronomic_budget_default;

    #[test]
    fn turf_slugs_get_legacy_one_inch_two_sessions() {
        for slug in ["back_yard", "front_yard", "side_yard", "lawn"] {
            assert_eq!(
                agronomic_budget_default(slug),
                (1.00, 2),
                "turf slug {slug} must reproduce the legacy 1.0\"/2 default"
            );
        }
    }

    #[test]
    fn bed_slugs_get_legacy_half_inch_one_session() {
        for slug in ["back_yard_shrubs", "front_garden", "flower_bed"] {
            assert_eq!(
                agronomic_budget_default(slug),
                (0.50, 1),
                "shrub/garden/bed slug {slug} must reproduce the legacy 0.5\"/1 default"
            );
        }
    }
}

/// End-to-end binding: a zone-bound MQTT soil subscription lands in
/// sensor_history under the canonical `soilmoisture_<zone_slug>` key (the
/// bus recorder does this from a KeyedReading event), and a zone whose
/// `soil_sensor_id` points at `source:<mqtt_src>:soilmoisture_<zone_slug>`
/// resolves it through the SAME `resolve_soil_pct` path native channels use.
/// This is the engine half of the MQTT-soil fix; mqtt_subscribe.rs covers
/// the parse->emit half.
#[cfg(test)]
mod mqtt_soil_binding_tests {
    use super::resolve_soil_pct;
    use crate::persistence::runner;
    use crate::persistence::SensorHistoryStore;
    use crate::sources::bus_recorder::zone_soil_key;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn fresh_store() -> SensorHistoryStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        SensorHistoryStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn zone_bound_mqtt_soil_resolves_to_zone_reading() {
        let store = fresh_store().await;
        // Simulate the bus recorder persisting a KeyedReading from a
        // zone-bound MQTT soil subscription on source "garden_mqtt".
        let key = zone_soil_key("back_yard");
        assert_eq!(key, "soilmoisture_back_yard");
        store
            .insert(crate::persistence::sensor_history::Reading {
                epoch: 1_700_000_000,
                source_id: "garden_mqtt".into(),
                key: key.clone(),
                value: 37.0,
            })
            .await
            .unwrap();

        // The zone binds the canonical channel id and resolves it exactly
        // like a native `source:` channel.
        let spec = format!("source:garden_mqtt:{key}");
        let map: HashMap<String, serde_json::Value> = HashMap::new();
        let pct = resolve_soil_pct(Some(&spec), &map, Some(&store)).await;
        assert_eq!(pct, Some(37.0));
    }

    #[tokio::test]
    async fn zone_bound_mqtt_soil_is_discoverable_as_soil_channel() {
        let store = fresh_store().await;
        store
            .insert(crate::persistence::sensor_history::Reading {
                epoch: 1_700_000_100,
                source_id: "garden_mqtt".into(),
                key: zone_soil_key("front_yard"),
                value: 52.0,
            })
            .await
            .unwrap();
        // The soil-channel discovery (LIKE 'soilmoisture%') must surface it,
        // so it shows up in /sensors/soil + the inventory + the picker.
        let chans = store.soil_channels().await.unwrap();
        let found = chans
            .iter()
            .find(|r| r.source_id == "garden_mqtt" && r.key == "soilmoisture_front_yard")
            .expect("zone-bound MQTT soil channel is discoverable");
        assert_eq!(found.value, 52.0);
    }
}
