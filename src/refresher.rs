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
use arc_swap::ArcSwap;
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

/// Last wall-clock epoch the refresher loop began an iteration. The watchdog
/// (`spawn_refresher_watchdog`) reads this to detect a dead/hung refresher (a
/// panic kills the spawned task and freezes this value, since errors are handled
/// in-loop and never unwind). 0 = not started yet.
static REFRESHER_HEARTBEAT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// How long the refresher heartbeat may go stale before the watchdog force-exits
/// the process. Must exceed the worst-case tick gap (BACKOFF_MAX 180s during an
/// outage) by a wide margin so a legitimately-degraded refresher is never killed.
const REFRESHER_STALL_MAX_S: i64 = 600;
/// Grace period after the watchdog starts before a still-zero heartbeat (the
/// refresher never produced a first tick, e.g. it panicked in setup) is treated
/// as a stall.
const REFRESHER_STARTUP_GRACE_S: i64 = 120;

/// Light run a deliberate force-run falls back to when the soil-based budget came
/// out 0 (soil already satisfied). Without this a Force on a wet yard flips the
/// verdict to "run" but dispatches nothing, since the scheduler skips zones with
/// planned_run_seconds == 0. Clamped to the zone's max_duration.
const FORCE_RUN_DEFAULT_S: u32 = 300;

/// P1-9: decouple the forced-run VERDICT from its DURATION. If an operator
/// override forces a zone to run (per-zone "run", or global "run" with the zone on
/// auto) but the computed budget is 0, water a bounded default so the Force is
/// never a silent no-op. Natural 0-budget zones (no force) stay 0.
fn force_run_floor(zone_override: &str, global_override: &str, computed: u32, max_dur: u32) -> u32 {
    if computed > 0 {
        return computed;
    }
    let forced = zone_override == "run" || (zone_override == "auto" && global_override == "run");
    if forced {
        if max_dur > 0 {
            FORCE_RUN_DEFAULT_S.min(max_dur)
        } else {
            FORCE_RUN_DEFAULT_S
        }
    } else {
        computed
    }
}

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
    /// Seasonal water-budget adjustment ("trust dial"), percent of computed run
    /// depth, from `cfg.engine.seasonal_adjust_pct`. The `Default` derive makes
    /// this 0; `seasonal_multiplier` treats 0 as "no adjustment" (100%) so the
    /// default/no-config path never zeroes a run.
    pub seasonal_adjust_pct: u32,
    /// Household display-unit default from `cfg.deployment.units`. Copied
    /// verbatim into `IrrigationSnapshot.units` each refresh so the client can
    /// resolve a device's display units (household baseline vs. a per-device
    /// override) without a separate fetch. Display-plumbing only; never read
    /// by the engine. `Default` is `Units::Imperial`.
    pub units: crate::config::schema::Units,
}

impl WateringPolicy {
    /// Derive a `WateringPolicy` from the live `Config`. This is the single
    /// source of truth for the engine-tunable subset of config: boot builds it
    /// here, and the config hot-reload path (PUT /api/config + wizard apply)
    /// rebuilds it from the freshly-saved config and arc-swaps it into the live
    /// refresher (see `runtime::apply_runtime_config`). Keeping the mapping in
    /// one place means a boot policy and a hot-reloaded policy are byte-for-byte
    /// identical for the same config, so a reload can never silently diverge
    /// from a restart.
    pub fn from_config(cfg: &crate::config::schema::Config) -> Self {
        WateringPolicy {
            restrictions: cfg.engine.watering_restrictions.clone(),
            address_parity: cfg.deployment.address_parity,
            manual_schedules: cfg.manual_schedules.clone(),
            location: (cfg.deployment.location.lat, cfg.deployment.location.lon),
            // Per-zone soil config: each zone's assigned sensor + thresholds.
            // Slugs underscore-normalized to match the refresher's zone list.
            soil_zones: cfg
                .zones
                .iter()
                .map(|(slug, z)| ZoneSoilCfg {
                    slug: slug.replace('-', "_"),
                    name: z.display_name.clone(),
                    soil_sensor_id: z.soil_sensor_id.clone(),
                    saturation_pct: z.saturation_pct_soil,
                    target_min_pct: z.target_min_pct_soil,
                })
                .collect(),
            condition_rules: cfg.conditions.rules.clone(),
            skip_rules: cfg.engine.skip_rules.clone(),
            // Per-zone weekly-budget config for the standalone allocator (A5b).
            // Slugs underscore-normalized to match the refresher's zone list,
            // same as soil_zones above.
            budget_zones: cfg
                .zones
                .iter()
                .map(|(slug, z)| ZoneBudgetCfg {
                    slug: slug.replace('-', "_"),
                    name: z.display_name.clone(),
                    weekly_budget_in: z.weekly_budget_in,
                    sessions_per_week: z.sessions_per_week,
                })
                .collect(),
            ha_sprinkler_prefix: cfg.deployment.ha_sprinkler_prefix.clone(),
            seasonal_adjust_pct: cfg.engine.seasonal_adjust_pct,
            units: cfg.deployment.units,
        }
    }
}

/// Seasonal multiplier (0.50..=1.50) from a percent. The `WateringPolicy::Default`
/// and any unset config produce 0, which is treated as 100% (no adjustment), so a
/// missing dial never starves the yard.
fn seasonal_multiplier(pct: u32) -> f64 {
    if pct == 0 {
        1.0
    } else {
        (pct as f64 / 100.0).clamp(0.5, 1.5)
    }
}

/// Apply the seasonal dial to a run-depth budget, THEN re-clamp to the per-zone
/// max-duration cap. The clamp MUST follow the scaling: a >100% dial can push an
/// already-capped budget back over the ceiling (which also folds in any
/// regulatory restriction cap). Used by BOTH the HA path (raw_seconds) and the
/// native weekly-allocator path (today_seconds) so neither can dispatch past the
/// safety ceiling.
///
/// `max_dur == 0` means "no cap known" and leaves the value as-is (never zeroes a
/// run): this matches `force_run_floor`'s own convention (a forced run with
/// max_dur 0 returns the bounded default, not 0). A literal "cap of zero minutes"
/// is not a supported way to disable a zone -- that goes through the verdict/skip
/// ladder -- so the two readings of 0 never conflict in practice.
fn seasonal_capped(raw_seconds: u32, seasonal_pct: u32, max_dur: u32) -> u32 {
    let scaled = (raw_seconds as f64 * seasonal_multiplier(seasonal_pct)).round() as u32;
    if max_dur > 0 {
        scaled.min(max_dur)
    } else {
        scaled
    }
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

/// Offline guard for a raw soil reading: a value outside the physical band
/// (exactly 0% / negative, or above SOIL_PCT_PHYSICAL_MAX) is a
/// disconnected/faulty probe (e.g. a WH51 out of soil, or a garbage
/// over-range frame), NOT bone-dry or super-saturated soil, return None so
/// the zone falls back to weather/modeled rather than over-watering or
/// falsely satisfying the saturation skip. Real soil is essentially never
/// exactly 0.00% and can never exceed 100%. (Soil calibration itself lives
/// at the source, see `parse_soilad`'s native AD-based dry/wet calibration
/// in the Ecowitt poll adapter.)
fn apply_soil_quality(raw: Option<f64>) -> Option<f64> {
    raw.filter(|v| *v > 0.0 && *v <= SOIL_PCT_PHYSICAL_MAX)
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_refresher(
    store: Arc<IrrigationStore>,
    forecast_store: Arc<ForecastStore>,
    tempest_store: Arc<TempestStore>,
    history_conn: Option<Arc<Mutex<Connection>>>,
    push: crate::push::PushDispatcher,
    zone_runtime: HashMap<String, ZoneRuntime>,
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
        // Soil-probe QUARANTINE push fires once per zone per quarantine
        // episode. Unlike a fault (a process-lifetime latch), a quarantine
        // is a recoverable condition: a probe can drift into and out of
        // outlier-territory. We latch the SET of currently-quarantined zone
        // slugs and only notify a zone on the edge INTO quarantine; when a
        // zone leaves quarantine its slug drops from the set so a later
        // re-quarantine notifies again (one push per episode, not per poll).
        let mut quarantined_zones: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        // Circuit-breaker state. Single warn on first failure ("entering
        // degraded mode"), single info on recovery ("recovered"), with
        // exponential backoff between attempts while degraded.
        let mut consecutive_failures: u32 = 0;
        let mut degraded: bool = false;

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
                        watering_policy,
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
                    watering_policy,
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
                            watering_policy,
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
                        &mut quarantined_zones,
                    );
                    // P4-1: per-tick engine metrics from the authoritative
                    // snapshot (verdict mix + degraded-rate are the core health
                    // signals scraped by the monitoring host).
                    crate::metrics::inc("localsky_refresh_total", String::new());
                    crate::metrics::set_gauge(
                        "localsky_last_refresh_epoch",
                        chrono::Utc::now().timestamp() as f64,
                    );
                    if let Some(t) = snap.decision_trace.as_ref() {
                        if t.degraded {
                            crate::metrics::inc("localsky_refresh_degraded_total", String::new());
                        }
                    }
                    // Count the verdict the engine actually DECIDED, not the
                    // trace's own verdict (the trace ignores the sticky global
                    // override, so t.verdict can disagree with the real outcome;
                    // see review #3). skip_check.verdict is the authoritative
                    // decided verdict the rest of the app surfaces.
                    crate::metrics::inc(
                        "localsky_verdict_total",
                        crate::metrics::label("verdict", &snap.skip_check.verdict),
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

/// Pure stall decision for the watchdog, factored out so it is testable without
/// exiting the process. `heartbeat == 0` means the refresher never produced a
/// first tick, judged against the startup grace; otherwise judge the gap since
/// the last tick against the stall ceiling.
fn refresher_stalled(heartbeat: i64, watchdog_started: i64, now: i64) -> bool {
    if heartbeat == 0 {
        now - watchdog_started > REFRESHER_STARTUP_GRACE_S
    } else {
        now - heartbeat > REFRESHER_STALL_MAX_S
    }
}

/// Supervise the refresher (P0-8b). If its heartbeat goes stale (the spawned task
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

/// Parse the canonical quarantine reason string the engine produces
/// (`quarantine_reason` in `engine::skip_rules`) back into its numbers for
/// the push payload. The format is:
///   "Soil probe suspect (<probe> vs yard <median>%); inferred from neighbors -> ..."
/// where `<probe>` is either "<n>%" (a present-but-outlier reading) or the
/// literal "offline". Returns `(raw_pct, yard_pct)`: `raw_pct` is `None` for
/// the offline case. Returns `None` when the string isn't a quarantine reason
/// or can't be parsed (defensive; the caller then skips the push rather than
/// firing with bogus numbers).
fn parse_quarantine_reason(reason: &str) -> Option<(Option<f64>, f64)> {
    let inner = reason
        .strip_prefix("Soil probe suspect (")?
        .split_once(')')?
        .0; // "<probe> vs yard <median>%"
    let (probe_str, yard_str) = inner.split_once(" vs yard ")?;
    let yard_pct = yard_str.trim_end_matches('%').trim().parse::<f64>().ok()?;
    let raw_pct = if probe_str.trim() == "offline" {
        None
    } else {
        Some(probe_str.trim_end_matches('%').trim().parse::<f64>().ok()?)
    };
    Some((raw_pct, yard_pct))
}

/// Walk the snapshot and emit push events on edge transitions:
/// - ZoneStarted/ZoneStopped on each zone's running flag flip.
/// - DailyVerdict once per local day, the first time we see a non-empty
///   verdict for that day.
/// - SoilProbeFault when a probe first appears in soil_probe_faults
///   (once per probe per process lifetime via `probe_fault_notified`).
/// - SoilProbeSuspect when a zone's verdict source becomes "soil_quarantine"
///   (once per zone per quarantine episode via `quarantined_zones`, which
///   latches the currently-quarantined slugs and clears them on exit so a
///   later re-quarantine notifies again).
#[allow(clippy::too_many_arguments)]
fn emit_push_events(
    push: &crate::push::PushDispatcher,
    snap: &IrrigationSnapshot,
    prev_running: &mut std::collections::HashMap<String, bool>,
    started_at: &mut std::collections::HashMap<String, i64>,
    last_verdict_day: &mut Option<String>,
    probe_fault_notified: &mut std::collections::HashSet<String>,
    quarantined_zones: &mut std::collections::HashSet<String>,
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

    // Soil-probe QUARANTINE: a zone whose per-zone verdict was decided on
    // inferred neighbor soil (source == "soil_quarantine"). Edge-triggered
    // per episode: notify only on the transition INTO quarantine; a zone
    // that was already in the latched set is skipped until it leaves and
    // re-enters. The reason string carries the suspect raw% + sibling
    // median, parsed back out for the push numbers (engine produces it).
    let now_quarantined: std::collections::HashSet<String> = snap
        .zones
        .iter()
        .filter(|z| z.verdict.as_ref().map(|v| v.source.as_str()) == Some("soil_quarantine"))
        .map(|z| z.slug.clone())
        .collect();
    for z in &snap.zones {
        let Some(v) = z.verdict.as_ref() else {
            continue;
        };
        if v.source != "soil_quarantine" {
            continue;
        }
        // Edge into quarantine: only fire when this slug wasn't already latched.
        if quarantined_zones.contains(&z.slug) {
            continue;
        }
        match parse_quarantine_reason(&v.reason) {
            Some((raw_pct, yard_pct)) => {
                push.emit(PushEvent::SoilProbeSuspect {
                    zone_name: z.name.clone(),
                    zone_slug: z.slug.clone(),
                    raw_pct,
                    yard_pct,
                });
            }
            None => {
                tracing::debug!(
                    zone = %z.slug,
                    reason = %v.reason,
                    "soil_quarantine reason unparseable; suppressing suspect push"
                );
            }
        }
    }
    // Replace the latch with the current set: slugs that left quarantine drop
    // out (so a later re-quarantine notifies again), entries we just notified
    // are now latched so the 10s poll cadence doesn't re-fire every tick.
    *quarantined_zones = now_quarantined;

    // Daily verdict fires once per local day. The "today" label is the
    // local-date YYYY-MM-DD; on the first refresh after midnight rolls
    // we emit one event with the new verdict.
    // P1-8c: the once-a-day dedupe rolls on the CONFIGURED-timezone date.
    let today = crate::timeutil::now_local().format("%Y-%m-%d").to_string();
    let verdict = snap.skip_check.verdict.clone();
    if !verdict.is_empty() && last_verdict_day.as_deref() != Some(today.as_str()) {
        // P1-7/P1-1: carry honest confidence into the morning push. When the
        // decision ran on substituted inputs (stale station and/or aged forecast,
        // folded into the trace's degraded flag), say so up front so the
        // notification is never more confident than the data behind it.
        let degraded = snap
            .decision_trace
            .as_ref()
            .map(|t| t.degraded)
            .unwrap_or(false);
        let base = snap.skip_check.reason.clone();
        let reason = match (degraded, base.is_empty()) {
            (true, true) => {
                "Decided on backup data (lower confidence until live data returns).".to_string()
            }
            (true, false) => format!("Decided on backup data (lower confidence). {base}"),
            (false, _) => base,
        };
        push.emit(crate::push::PushEvent::DailyVerdict { verdict, reason });
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
        // Live local-station serial (empty on cloud-only installs), so the
        // verdict-strip freshness pill knows whether a station exists at all
        // before it can call one "stale".
        station_serial: tempest_store.snapshot().station_serial.clone(),
        forecast_last_seen_epoch: forecast_store.snapshot().last_refresh_epoch,
        // Household display-unit default, copied verbatim from config (mirror of
        // the per-zone photo_url copy). Display-plumbing only; the engine never
        // reads it. Default config -> Units::Imperial, so this is a no-op for
        // the default deployment.
        units: watering_policy.units,
        // Per-field provenance: which source currently owns each headline
        // reading (keyed by WeatherField name), so the UI can label "Wind:
        // Tempest" and the source picker shows the live owner. Empty until a
        // source has written a field.
        field_sources: tempest_store.field_source_map(),
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
        // Per-day 3-day peak heat index (each day's high temp × THAT day's
        // humidity), NOT the impossible tmax × humidity-now pairing. Falls back
        // to the live now value when no daily forecast humidity is available.
        let per_day = fc_peek.max_heat_index_n_day(3);
        let hi = if per_day > 0.0 {
            per_day
        } else {
            let humidity_peek = tempest_store.snapshot().rh_pct;
            let tmax_peek = fc_peek.max_temp_next_3d_f().unwrap_or(0.0);
            heat_index_f(tmax_peek, humidity_peek)
        };
        et_heat_multiplier(hi)
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
            // P2-6: scale the depth by the seasonal trust dial, then re-clamp to
            // the safety cap (raw_seconds stays the pre-dial engine value for the
            // math view). seasonal_capped enforces the cap-after-scaling order.
            let raw_planned =
                seasonal_capped(raw_seconds, watering_policy.seasonal_adjust_pct, max_dur);
            let override_active = crate::scheduler::manual::override_active_today(
                &watering_policy.manual_schedules,
                slug,
                today_weekday,
            );
            let planned = if override_active {
                0
            } else {
                // P1-9: a force-run with a 0 soil budget still waters a bounded
                // default instead of silently dispatching nothing.
                let zone_ov = control
                    .and_then(|c| c.zone_overrides.get(slug))
                    .map(|s| s.as_str())
                    .unwrap_or("auto");
                let global_ov = control
                    .map(|c| c.global_override.as_str())
                    .unwrap_or("auto");
                force_run_floor(zone_ov, global_ov, raw_planned, max_dur)
            };
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
                // Verdict-independent suspect-probe flag, back-filled by
                // apply_engine (suspect_probes) before the snapshot publishes.
                soil_suspect: None,
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

    // Deployment IANA timezone for the client's 24h-local formatting (fix #5).
    // Prefer the forecast snapshot's timezone (Open-Meteo `timezone=auto`, the
    // canonical IANA name for the point); on a fresh install with no forecast yet,
    // derive it from the configured location. Empty string -> client falls back to
    // browser-local (the prior behavior).
    snap.timezone = if !fc.timezone.is_empty() {
        fc.timezone.clone()
    } else {
        let (lat, lon) = watering_policy.location;
        crate::timeutil::tz_name_for(lat, lon).unwrap_or_default()
    };

    // The legacy sensor.open_meteo_rain_3day HA template sensor is absent in many
    // setups (LocalSky standalone, non-Open-Meteo forecast sources), so the raw
    // 3-day rain-outlook bar read 0 while the weighted bar, the verdict strip, and
    // the engine all read LocalSky's own live forecast. Prefer the live forecast
    // for this display field so the bar agrees with the rest of the UI, keeping the
    // HA sensor only when it reports more. Display-only: the engine's 3-day rule
    // uses the probability-weighted total, not this raw value.
    let rain_3day = rain_3day.max(fc.future_n_day_precip_in(3));

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
    // PER-FIELD live-rain freshness (fix #5 regression guard): the current-rain
    // reads (intensity / type) gate on rain_live_epoch, which a LIVE writer
    // stamps ONLY when it actually reports rain. The whole-snapshot
    // last_packet_epoch is NOT sufficient: a barometer-only live source keeps it
    // fresh while Open-Meteo current (a forecast fill) sits in rain_intensity_in_hr,
    // so gating on it would mislabel a stale cloud rate as live station rain and
    // could hard-skip a dry day.
    let rain_live = tempest.rain_live_epoch > 0
        && station_now.saturating_sub(tempest.rain_live_epoch) < TEMPEST_LIVE_MAX_AGE_S;
    let rain_today_station = if station_fresh {
        tempest.rain_in_today
    } else {
        0.0
    };
    // 3-TIER HONEST RAIN GATE (current-rain rate + its honest nature):
    //   1. LIVE LAN GAUGE: a live station owns the rain rate freshly (rain_live).
    //      A real gauge on the yard, observation-grade -> Measured.
    //   2. OBSERVATION / RADAR fill: no live gauge, but the fresh merge owner of
    //      rain_intensity_in_hr is an observation-grade cloud (NWS observation or
    //      NOAA MRMS radar QPE). Surface THAT measured/radar rate (already filled
    //      into the store's rain_intensity_in_hr) ABOVE the model fallback, with
    //      its honest nature (Measured for NWS, RadarQpe for MRMS).
    //   3. MODEL FORECAST fallback: no measured/radar rain owner. Fall back to the
    //      live forecast's current-hour precip rate (a model estimate) -> Model.
    //      This is never presented as "live" rain (the badge keys on rain_nature).
    // The observation tier is what lets an MRMS/NWS-only deploy (no LAN gauge)
    // still HARD-skip on truly measured rain, while a model-only deploy soft-skips.
    let rain_owner = tempest_store.rain_owner(station_now);
    // Map a fresh observation-grade cloud rain owner (tier 2) to its honest
    // nature, or None when the owner is a model cloud / stale / absent.
    let observed_rain_nature = if rain_live {
        // Tier 1: the live gauge owns rain, always Measured (set below).
        None
    } else {
        rain_owner.as_ref().and_then(|owner| {
            if !owner.is_fresh {
                return None;
            }
            // A live (non-rain_live but rain-owning) station is still a real gauge:
            // observation-grade Measured. Otherwise map the cloud owner label to
            // its catalog rain nature; only NWS (observation) and NOAA MRMS (radar
            // QPE) are observation-grade, every model provider is Model.
            if owner.is_live {
                Some(crate::ha::snapshot::RainNature::Measured)
            } else {
                cloud_rain_nature_for_label(&owner.label)
            }
        })
    };
    let rain_nature = if rain_live {
        // Tier 1: a live LAN gauge owns the rain rate -> truly Measured.
        crate::ha::snapshot::RainNature::Measured
    } else {
        // Tier 2 if an observation/radar owner is fresh, else tier 3 Model.
        observed_rain_nature.unwrap_or(crate::ha::snapshot::RainNature::Model)
    };
    let rain_intensity = if rain_live {
        // Tier 1: the live LAN gauge's measured rate.
        tempest.rain_intensity_in_hr
    } else if observed_rain_nature.is_some() {
        // Tier 2: a fresh NWS observation / NOAA MRMS radar rate already filled the
        // store's rain_intensity_in_hr (the cloud-fill owner wrote it). Surface
        // that measured/radar rate, NOT the model forecast.
        tempest.rain_intensity_in_hr
    } else {
        // Tier 3: no measured/radar rain owner. Fall back to the live forecast's
        // current-hour precip rate (inches this hour ~= in/hr), NOT a hardcoded
        // one-install HA entity. The old fallback hard-coded a single developer
        // station's precip-intensity entity; on any other deploy it was missing and
        // pinned active rain to 0, so the rain_now skip gate could never fire from
        // the fallback path. This is a model estimate (rain_nature == Model).
        fc.next_n_hours_precip_in(1)
    };
    let rain_type = if rain_live {
        match tempest.precip_type {
            1 => "rain".to_string(),
            2 => "hail".to_string(),
            _ => "none".to_string(),
        }
    } else if rain_intensity > 0.0 {
        "rain".to_string()
    } else {
        "none".to_string()
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
    // OBSERVED-recent-rain backstop (sensor-independent): today's measured rain
    // (already the max of station + model) plus the configured window of PAST
    // observed daily rain. Feeds the engine's hard observed-rain skip gate so a
    // soaking the morning before still suppresses the run even if a soil probe is
    // bad/offline. Reads measured rain, not the forecast, so an Open-Meteo outage
    // cannot inflate it.
    let rain_observed_recent = rain_today_used
        + fc.past_n_day_precip_in(watering_policy.skip_rules.rain_observed_window_days as usize);
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
    // 3-day peak heat index computed PER DAY (each day's high temp paired with
    // THAT day's humidity) instead of the old heat_index_f(temp_max_3day,
    // humidity_now): that pairing of the 3-day MAX temp with the CURRENT (often
    // saturated post-rain) humidity overshoots the Rothfusz regression to a
    // physically-impossible value (~147°F) that inflated both the ET heat
    // multiplier and the hero "HEAT INDEX 3D" display. Falls back to the now
    // value when no daily forecast humidity is available.
    let heat_index_3day = {
        let per_day = fc.max_heat_index_n_day(3);
        if per_day > 0.0 {
            per_day
        } else {
            heat_index_now
        }
    };
    let heat_mult = et_heat_multiplier(heat_index_3day);

    // Source-agnostic reference ET0 (mm): source-reported > Open-Meteo HA sensor
    // > native compute from the forecast > fallback. Activates the previously
    // unwired engine::et0 so ET0 works for any forecast source, not just
    // Open-Meteo-via-HA. Display + soil-projection only (the live decision bucket
    // is HA-sourced).
    let et0_lat = watering_policy.location.0;
    let et0_base_doy = {
        use chrono::Datelike;
        chrono::Utc::now().ordinal() as u16
    };
    let et0_today_mm = resolve_et0_today_mm(tempest.et0_today, &map, &fc, et0_lat, et0_base_doy);

    let forecast = Forecast {
        rain_today_tempest_in: rain_today_station,
        rain_today_om_in: rain_today_om,
        // Provenance for the rain comparison cards: the live station's label and
        // the forecast provider's label (real sources, not hardcoded names).
        station_source_label: if tempest.source_label.is_empty() {
            "Station".to_string()
        } else {
            tempest.source_label.clone()
        },
        forecast_source_label: if fc.source_label.is_empty() {
            "Forecast".to_string()
        } else {
            fc.source_label.clone()
        },
        rain_intensity_in_hr: rain_intensity,
        rain_type,
        // TRUE only when a LIVE source owns the current-rain reading this refresh
        // (rain_live, gated on rain_live_epoch). On cloud-only / station-stale the
        // intensity/type above are an Open-Meteo forecast FILL, not an
        // observation, so the dashboard's "RAINING NOW" badge must not present
        // them as live observed rain (T3).
        rain_is_live: rain_live,
        // HONEST rain nature derived by the 3-tier gate above: Measured when a live
        // LAN gauge (or a fresh NWS observation) owns the rain rate, RadarQpe when
        // a fresh NOAA MRMS radar fill owns it, else Model (the forecast fallback).
        // The dashboard rain badge keys on THIS (not rain_is_live alone) and never
        // says "live" on a Model nature.
        rain_nature,
        rain_tomorrow_in: rain_tomorrow_used,
        rain_3day_in: rain_3day,
        eto_today_mm: et0_today_mm,
        eto_tomorrow_mm: forecast_day_et0_mm(
            &map,
            "sensor.open_meteo_eto_tomorrow",
            &fc,
            1,
            et0_lat,
            et0_base_doy,
            0.0,
        ),
        eto_3day_avg_mm: state_f64(&map, "sensor.open_meteo_eto_3day_avg")
            .filter(|v| *v > 0.0)
            .unwrap_or_else(|| {
                let vals: Vec<f64> = (0..3)
                    .filter_map(|i| {
                        fc.daily
                            .get(i)
                            .and_then(|d| native_et0_mm(d, et0_lat, et0_base_doy + i as u16))
                    })
                    .collect();
                if vals.is_empty() {
                    0.0
                } else {
                    vals.iter().sum::<f64>() / vals.len() as f64
                }
            }),
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

    // P0-2: the forecast store re-emits its last-good payload during an
    // Open-Meteo outage (last_refresh_epoch only advances on a successful fetch),
    // so age past the trust horizon means the forward-looking rain inputs are
    // untrustworthy. This marks the trace degraded and suppresses the predictive
    // rain SKIPs so a frozen "rain coming" cannot starve the yard.
    let forecast_stale = forecast_is_stale(fc.last_refresh_epoch, now_epoch);

    let inputs = Inputs {
        temp_now_f: temp_now,
        wind_now_mph: wind_now,
        rain_today_in: rain_today_used,
        rain_intensity_now_in_hr: rain_intensity,
        // Honest nature of the live rain rate (same 3-tier derivation that fills
        // the snapshot's rain_nature): Measured / RadarQpe gate a HARD rain_now
        // skip; Model only a demotable soft skip. Carried so the engine's
        // observation-grade-only hard-skip rule reads the merge owner's truth.
        rain_nature,
        humidity_now_pct: humidity_now,

        forecast_in: rain_tomorrow_used,
        rain_tomorrow_prob_pct: rain_tomorrow_prob,
        rain_3day_weighted_in: rain_3day_weighted,
        rain_7day_weighted_in: rain_7day_weighted,
        rain_next_4h_in: rain_next_4h,
        rain_observed_recent_in: rain_observed_recent,
        forecast_stale,
        wind_max_today_mph: wind_max_today,
        temp_min_24h_f: temp_min_24h,
        temp_max_3day_f: temp_max_3day,
        // Forecast-derived per-day 3-day peak heat index (corrected pairing).
        heat_index_max_3day_f: heat_index_3day,
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
        et0_today_mm,
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
    // Read before the mutable zone loop borrows snap.
    let global_ov = snap.global_override.clone();
    for z in snap.zones.iter_mut() {
        // P2-6: the native (weekly-allocator) path applies the seasonal dial here,
        // then re-clamps to the cap via seasonal_capped (today_seconds was already
        // capped inside compute_water_budgets, but a >100% dial can push it back
        // over the per-zone / regulatory ceiling, so the cap MUST be re-applied
        // after the scaling, same contract as the HA path).
        let raw_budget = planned_by_slug.get(&z.slug).copied().unwrap_or(0);
        let max_dur = z.math.as_ref().map(|m| m.max_duration_seconds).unwrap_or(0);
        let budget_seconds =
            seasonal_capped(raw_budget, watering_policy.seasonal_adjust_pct, max_dur);
        let override_active = crate::scheduler::manual::override_active_today(
            &watering_policy.manual_schedules,
            &z.slug,
            today_weekday,
        );
        z.planned_run_seconds = if override_active {
            0
        } else {
            // P1-9: same force-run floor as the HA path (z.override_mode is the
            // resolved per-zone override; global from the snapshot).
            force_run_floor(&z.override_mode, &global_ov, budget_seconds, max_dur)
        };
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
        match c.status().await {
            Ok(st) => {
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
            // P4-1: a controller that can't report is a real ops signal (the
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
    // Forced-run safety signal: if a sticky global_override="run" is watering
    // THROUGH a hard guard, name that guard so the hero can warn the operator.
    // None when there is no force-run or it isn't suppressing anything. The
    // override still wins; this only surfaces what it overrides. (Computed from
    // the deterministic ladder; the Rhai augment pass below only ADDS skips on a
    // clean run, never clears a hard guard, so it cannot change this signal.)
    snap.force_overrode_guard = crate::engine::skip_rules::force_overrode_guard(inputs, params);

    // Augment-only user scripts: consulted ONLY when the deterministic
    // ladder said "run", so a script can ADD a skip but can never clear a
    // freeze / wind / restriction gate. Fail-safe: errors are no-ops.
    if !scripts.is_empty() && snap.skip_check.verdict == "run" {
        if let Some(us) = scripts.apply_user_skip(inputs) {
            snap.skip_check.verdict = "skip".to_string();
            snap.skip_check.will_skip = true;
            snap.skip_check.reason = us.reason.clone();
            // P1: a user Rhai rule overrode the clean run with a skip; mirror its
            // id into both the SkipCheck and the trace reason_code. User-defined
            // metric -> no canonical engine operands on the RuleEval.
            snap.skip_check.reason_code = us.id.clone();
            if let Some(t) = snap.decision_trace.as_mut() {
                t.verdict = "skip".to_string();
                t.reason = us.reason.clone();
                t.reason_code = us.id.clone();
                t.rules.push(RuleEval {
                    id: us.id,
                    label: us.name,
                    category: "script".to_string(),
                    detail: "user Rhai rule".to_string(),
                    outcome: "fired".to_string(),
                    verdict: Some("skip".to_string()),
                    margin_label: None,
                    value: None,
                    threshold: None,
                    unit_kind: None,
                });
            }
        }
    }

    // Per-zone verdicts: global gates bind every zone, then per-zone soil
    // saturation + user condition rules let zones diverge. Augment-only.
    let verdicts = crate::engine::skip_rules::decide_per_zone(inputs, params, condition_rules);
    // Verdict-INDEPENDENT suspect-probe surface (reporting only): a probe the
    // quarantine logic distrusts (offline / wild outlier vs siblings) is flagged
    // here REGARDLESS of which gate ultimately decided the zone, so a bad probe
    // shows on the anomaly banner even when a global gate masked
    // `verdict.source` away from "soil_quarantine". Computed from raw readings,
    // parallel to inputs.soil_zones; changes no decision.
    let suspects = crate::engine::skip_rules::suspect_probes(inputs, params);
    let suspect_by_slug: std::collections::HashMap<&str, &str> = inputs
        .soil_zones
        .iter()
        .zip(suspects.iter())
        .filter_map(|(z, s)| s.as_deref().map(|r| (z.slug.as_str(), r)))
        .collect();
    for z in snap.zones.iter_mut() {
        z.verdict = verdicts.iter().find(|v| v.zone_slug == z.slug).cloned();
        z.soil_suspect = suspect_by_slug.get(z.slug.as_str()).map(|r| r.to_string());
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

    // The per-day 3-day peak heat index (already corrected on today_inputs by
    // the refresher) drives the ET bump; NOT heat_index_f(temp_max_3day,
    // humidity_now), which pairs the 3-day max temp with the current humidity
    // and overshoots. Falls back to the now value (its serialized default 0.0
    // maps to no bump) when no forecast humidity was available.
    let heat_mult_eff = et_heat_multiplier(today_inputs.heat_index_max_3day_f);

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
    et0_today_mm: f64,
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

    // Daily ET, mm. Resolved source-agnostically by the caller (source-reported
    // > Open-Meteo HA sensor > native compute > fallback). Today's value carries
    // across the window; heat_multiplier bumps it on heat-advisory days so a
    // 95°F+ forecast tracks realistically.
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
    // Use the corrected per-day 3-day peak heat index carried on `today` (each
    // day's high temp × THAT day's humidity), NOT heat_index_f(temp_max_3day,
    // humidity_now) which pairs the 3-day max with the current humidity and
    // overshoots. The engine sets this from ForecastSnapshot::max_heat_index_n_day.
    et_heat_multiplier(today.heat_index_max_3day_f)
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

/// Native reference ET0 (mm/day) computed from a forecast day's temperature
/// range + latitude, via the (previously unwired) engine::et0 module. Hargreaves
/// only needs Tmax/Tmin + extraterrestrial radiation (lat + day-of-year), so this
/// works for ANY forecast source (not just the Open-Meteo HA REST sensor) and
/// replaces the flat 5.0 fallback. `None` when the day has no usable temps.
fn native_et0_mm(d: &crate::forecast::snapshot::DailyEntry, lat: f64, doy: u16) -> Option<f64> {
    use crate::config::schema::Et0Method;
    use crate::engine::et0::{compute, f_to_c, Et0Inputs};
    if d.temp_max_f == 0.0 && d.temp_min_f == 0.0 {
        return None;
    }
    let inputs = Et0Inputs {
        t_max_c: f_to_c(d.temp_max_f),
        t_min_c: f_to_c(d.temp_min_f),
        t_mean_c: None,
        rh_max_pct: None,
        rh_min_pct: None,
        rh_mean_pct: None,
        // Forecast daily lacks reliable RH/solar, so Auto -> Hargreaves-Samani.
        u2_ms: None,
        solar_rad_mj_m2_day: None,
        pressure_kpa: None,
        elevation_m: 0.0,
        latitude_deg: lat,
        doy: doy.clamp(1, 366),
    };
    let r = compute(&inputs, Et0Method::Auto);
    (r.et0_mm_day.is_finite() && r.et0_mm_day > 0.0).then_some(r.et0_mm_day)
}

/// Today's reference ET0 (mm), source-agnostic. Priority: a source that
/// reports ET0 directly (HA-passthrough `et0today` / MQTT `et0_today` ->
/// snapshot.et0_today) > Open-Meteo's HA REST sensor > native compute from the
/// forecast > 5.0 fallback. Generalizes ET0 off the Open-Meteo-only path.
fn resolve_et0_today_mm(
    snapshot_et0: f64,
    map: &HashMap<String, Value>,
    fc: &ForecastSnapshot,
    lat: f64,
    doy: u16,
) -> f64 {
    if snapshot_et0 > 0.0 {
        return snapshot_et0;
    }
    if let Some(v) = state_f64(map, "sensor.open_meteo_eto_today").filter(|v| *v > 0.0) {
        return v;
    }
    fc.daily
        .first()
        .and_then(|d| native_et0_mm(d, lat, doy))
        .unwrap_or(5.0)
}

/// ET0 (mm) for forecast day `idx` (0=today): the matching Open-Meteo HA sensor
/// when present, else native compute, else `fallback`. For the display tiles.
fn forecast_day_et0_mm(
    map: &HashMap<String, Value>,
    sensor_key: &str,
    fc: &ForecastSnapshot,
    idx: usize,
    lat: f64,
    base_doy: u16,
    fallback: f64,
) -> f64 {
    if let Some(v) = state_f64(map, sensor_key).filter(|v| *v > 0.0) {
        return v;
    }
    fc.daily
        .get(idx)
        .and_then(|d| native_et0_mm(d, lat, base_doy + idx as u16))
        .unwrap_or(fallback)
}

/// How fresh the latest Tempest packet must be (seconds) for the station
/// to keep driving the live "now" inputs. Tempest obs_st arrives every
/// minute under normal conditions; 10 minutes of silence means the radio
/// path is down and the readings are no longer "now".
const TEMPEST_LIVE_MAX_AGE_S: i64 = 600;

/// The honest CURRENT-RAIN nature for a cloud source that owns the rain rate in
/// the merge, keyed by the source's config-id LABEL. Returns `Some` ONLY for the
/// two observation-grade cloud kinds: NWS (a real instrument observation ->
/// `Measured`) and NOAA MRMS (gauge-corrected radar QPE -> `RadarQpe`). Every
/// model / nowcast provider (Open-Meteo, Met.no, Pirate-rain, OpenWeather,
/// WeatherKit) maps to `None` so the 3-tier gate falls through to the model
/// fallback (nature `Model`) and the badge never reads "live" on a forecast.
///
/// The label->nature map is built from the cloud catalog's own `rain_nature`
/// honesty facts (the single source of truth) keyed on each kind's canonical id,
/// which is exactly the stable id the region auto-seeder stamps on NWS / NOAA
/// MRMS (`"nws"` / `"noaa_mrms"`). A renamed observation source simply falls
/// through to `Model`, the safe (never over-claims "measured") default.
fn cloud_rain_nature_for_label(label: &str) -> Option<crate::ha::snapshot::RainNature> {
    use crate::sources::cloud_catalog::{cloud_kinds, cloud_meta, CloudDataNature};
    cloud_kinds().iter().find_map(|kind| {
        let meta = cloud_meta(kind)?;
        if meta.kind != label {
            return None;
        }
        match meta.rain_nature {
            CloudDataNature::Observation => Some(crate::ha::snapshot::RainNature::Measured),
            CloudDataNature::RadarQpe => Some(crate::ha::snapshot::RainNature::RadarQpe),
            // Nowcast / Forecast: a model estimate, not observation-grade rain.
            CloudDataNature::Nowcast | CloudDataNature::Forecast => None,
        }
    })
}

/// How old the Open-Meteo forecast may be before its forward-looking rain inputs
/// are no longer trusted for a SKIP. The store refreshes every ~30 min, so 6h is
/// 12 missed polls, well past a transient outage but short enough to catch a real
/// multi-hour staleness before a stale "rain coming" suppresses a needed run.
const FORECAST_MAX_AGE_S: i64 = 6 * 3600;

/// True when the cached forecast is too old to base a SKIP on: never refreshed
/// (epoch <= 0) or older than [`FORECAST_MAX_AGE_S`]. Extracted from the snapshot
/// assembly so the P0-2 staleness threshold is unit-pinned at the seam, since the
/// forward-looking rain rules gate on it and it folds into the decision's
/// `degraded` flag. `>` is strict, so an age exactly at the bound is still fresh.
fn forecast_is_stale(last_refresh_epoch: i64, now_epoch: i64) -> bool {
    last_refresh_epoch <= 0 || now_epoch.saturating_sub(last_refresh_epoch) > FORECAST_MAX_AGE_S
}

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
/// entities with `unwrap_or(70.0)` / `0.0`, which fabricated
/// 70 °F / 0 mph for every standalone non-Tempest user.
fn resolve_current_conditions(
    tempest: &crate::tempest::state::Snapshot,
    current_hour: Option<&crate::forecast::snapshot::HourlyEntry>,
    now_epoch: i64,
) -> (f64, f64, f64, LiveReadings) {
    // PER-FIELD liveness: a field is a live reading only when a LIVE source
    // wrote it within the freshness window. This is stricter than the old
    // whole-snapshot last_packet_epoch: a partial live source (e.g. a barometer-
    // only gateway) keeps the snapshot "fresh" but does NOT make temp/wind/RH
    // live, and a forecast-filled field is never trusted as a live reading.
    let fresh = |epoch: i64| epoch > 0 && now_epoch.saturating_sub(epoch) < TEMPEST_LIVE_MAX_AGE_S;
    let temp_live = fresh(tempest.air_temp_live_epoch);
    let wind_live = fresh(tempest.wind_live_epoch);
    let rh_live = fresh(tempest.rh_live_epoch);
    if temp_live && wind_live && rh_live {
        return (
            tempest.air_temp_f,
            tempest.wind_avg_mph,
            tempest.rh_pct,
            LiveReadings::Station,
        );
    }
    // Mixed/absent live coverage: live value per field where fresh, else the
    // current-hour forecast for that field (degraded), so a forecast-filled or
    // never-provided field can't masquerade as a live station reading.
    if let Some(h) = current_hour {
        return (
            if temp_live {
                tempest.air_temp_f
            } else {
                h.temp_f
            },
            if wind_live {
                tempest.wind_avg_mph
            } else {
                h.wind_mph
            },
            if rh_live {
                tempest.rh_pct
            } else {
                h.humidity_pct as f64
            },
            LiveReadings::ForecastFallback,
        );
    }
    // No forecast either: fail safe with whatever live fields exist.
    (
        if temp_live { tempest.air_temp_f } else { 0.0 },
        if wind_live { tempest.wind_avg_mph } else { 0.0 },
        if rh_live { tempest.rh_pct } else { 0.0 },
        LiveReadings::Unavailable,
    )
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
    let now = Utc::now().timestamp();
    let mut out = Vec::with_capacity(cfg.len());
    for z in cfg {
        let raw = resolve_soil_pct(z.soil_sensor_id.as_deref(), map, history).await;
        let mut pct = apply_soil_quality(raw);
        // P1-2: a stale `source:` reading (no fresh sample within the fault
        // window) must fail safe to offline so it can never drive the dry-soil
        // veto or a saturation skip on data the gateway stopped refreshing. The
        // engine's zone_healthy_dry / soil_saturation then treat it as absent,
        // and detect_soil_probe_faults reports it. HA entities have no local
        // history to judge recency, so they are left to apply_soil_quality.
        if pct.is_some() && soil_reading_stale(z.soil_sensor_id.as_deref(), history, now).await {
            pct = None;
        }
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

/// True when a `source:` soil channel's most recent sample is older than the
/// fault window, so the cached value is stale and must not drive a watering
/// decision. `ha:` entities and missing/non-source specs are never considered
/// stale here (no local history to judge recency).
async fn soil_reading_stale(
    spec: Option<&str>,
    history: Option<&crate::persistence::SensorHistoryStore>,
    now: i64,
) -> bool {
    let Some(rest) = spec.and_then(|s| s.strip_prefix("source:")) else {
        return false;
    };
    let Some((sid, key)) = rest.split_once(':') else {
        return false;
    };
    let Some(h) = history else {
        return false;
    };
    match h.last_value(sid.to_string(), key.to_string()).await {
        Ok(Some(r)) => now.saturating_sub(r.epoch) >= SOIL_PROBE_FAULT_AFTER_S,
        _ => false,
    }
}

/// How long a configured soil channel may go without a valid (> 0)
/// reading before it is reported as faulted. One missed gateway poll is
/// noise; a full day of zeros is dead hardware.
const SOIL_PROBE_FAULT_AFTER_S: i64 = 24 * 3600;

/// Upper physical bound for a soil-moisture percentage. A reading above this
/// is not super-saturated soil, it is a garbage / over-range frame, so
/// `apply_soil_quality` nulls it to None and it feeds the same disconnected
/// fault path as a 0% / negative reading. Soil moisture is a percentage and
/// can never physically exceed 100%.
const SOIL_PCT_PHYSICAL_MAX: f64 = 100.0;

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
        // TODO(G1 flatline): a probe stuck at a plausible constant (e.g. 45%)
        // keeps refreshing, so stale=false and it slips through here. Detecting
        // it needs a windowed read of the last N source: samples for this
        // (sid, key) pair; SensorHistoryStore only exposes last_value /
        // last_value_above (single row) and series (windowed but key-only, not
        // source-scoped, so it collides across gateways sharing a soilmoisture
        // key). Adding a source-scoped windowed read is new store plumbing;
        // deferred per spec D1 to a fast-follow once that read exists.
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
mod watchdog_tests {
    use super::*;

    #[test]
    fn seasonal_multiplier_zero_is_no_adjustment_and_clamps() {
        // The WateringPolicy::Default / unset-config path produces 0; it MUST be
        // treated as 100% (no adjustment), never 0% (which would zero every run).
        assert_eq!(seasonal_multiplier(0), 1.0);
        assert_eq!(seasonal_multiplier(100), 1.0);
        assert_eq!(seasonal_multiplier(80), 0.8);
        assert_eq!(seasonal_multiplier(150), 1.5);
        // Out-of-range values clamp to the safe [0.5, 1.5] band.
        assert_eq!(seasonal_multiplier(10), 0.5);
        assert_eq!(seasonal_multiplier(500), 1.5);
    }

    #[test]
    fn seasonal_capped_reclamps_after_scaling() {
        // SAFETY contract: a >100% dial must never push a budget past the cap.
        // 600s base x 150% = 900s, held to the 720s ceiling.
        assert_eq!(seasonal_capped(600, 150, 720), 720);
        // Under the cap, scaling applies in full.
        assert_eq!(seasonal_capped(600, 150, 1200), 900);
        // A <100% dial reduces below the cap.
        assert_eq!(seasonal_capped(600, 80, 1200), 480);
        // max_dur == 0 ("no cap known") must NOT zero the run.
        assert_eq!(seasonal_capped(600, 150, 0), 900);
        // Default/no-config dial (0 => 100%) is a no-op, still capped.
        assert_eq!(seasonal_capped(600, 0, 1200), 600);
        assert_eq!(seasonal_capped(1000, 0, 720), 720);
    }

    #[test]
    fn force_run_floor_decouples_verdict_from_duration() {
        // No force + 0 budget stays 0 (a wet yard normally waters nothing).
        assert_eq!(force_run_floor("auto", "auto", 0, 1200), 0);
        // A non-zero budget is never altered, regardless of override.
        assert_eq!(force_run_floor("run", "auto", 600, 1200), 600);
        // Zone force + 0 budget -> bounded default, clamped to the zone max.
        assert_eq!(force_run_floor("run", "auto", 0, 1200), FORCE_RUN_DEFAULT_S);
        assert_eq!(force_run_floor("run", "auto", 0, 120), 120);
        // Global force with the zone on auto -> forced.
        assert_eq!(force_run_floor("auto", "run", 0, 1200), FORCE_RUN_DEFAULT_S);
        // A per-zone skip beats a global run -> not forced.
        assert_eq!(force_run_floor("skip", "run", 0, 1200), 0);
        // Unset max_dur falls back to the default, not 0.
        assert_eq!(force_run_floor("run", "auto", 0, 0), FORCE_RUN_DEFAULT_S);
    }

    #[test]
    fn apply_soil_quality_bands_to_physical_range() {
        // In-range readings pass through untouched (the boundaries are valid:
        // just-above-0 and exactly the physical max are real soil values).
        assert_eq!(apply_soil_quality(Some(45.0)), Some(45.0));
        assert_eq!(apply_soil_quality(Some(0.01)), Some(0.01));
        assert_eq!(apply_soil_quality(Some(SOIL_PCT_PHYSICAL_MAX)), Some(100.0));
        // Disconnected (exactly 0%) and negative readings null to None so the
        // zone fails safe to weather/modeled instead of reading as bone-dry.
        assert_eq!(apply_soil_quality(Some(0.0)), None);
        assert_eq!(apply_soil_quality(Some(-5.0)), None);
        // G2: an over-range frame (> physical max) is garbage, not
        // super-saturated soil, so it nulls to None and cannot falsely
        // satisfy the saturation skip.
        assert_eq!(apply_soil_quality(Some(150.0)), None);
        assert_eq!(apply_soil_quality(Some(100.01)), None);
        // A missing reading stays missing.
        assert_eq!(apply_soil_quality(None), None);
    }

    #[test]
    fn watchdog_stall_decision() {
        let now = 1_000_000i64;
        // Never-started, within grace: not stalled.
        assert!(!refresher_stalled(0, now - 10, now));
        // Never-started, past grace: stalled (setup-time panic).
        assert!(refresher_stalled(
            0,
            now - (REFRESHER_STARTUP_GRACE_S + 1),
            now
        ));
        // Fresh heartbeat: not stalled.
        assert!(!refresher_stalled(now - 5, now - 9_999, now));
        // A degraded refresher tick gap (worst case BACKOFF_MAX 180s) is NOT a
        // stall, so a legitimately-backed-off refresher is never killed.
        assert!(!refresher_stalled(now - 180, now - 9_999, now));
        // Past the stall ceiling: stalled (panic or hang).
        assert!(refresher_stalled(
            now - (REFRESHER_STALL_MAX_S + 1),
            now - 9_999,
            now
        ));
    }
}

#[cfg(test)]
mod et0_resolution_tests {
    use super::*;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot};

    #[test]
    fn et0_resolution_priority() {
        let map = HashMap::new();
        let fc = ForecastSnapshot {
            daily: vec![DailyEntry {
                temp_max_f: 90.0,
                temp_min_f: 65.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        // 1. A source-reported ET0 wins outright.
        assert_eq!(resolve_et0_today_mm(4.2, &map, &fc, 40.0, 180), 4.2);
        // 2. No source + no HA sensor -> native compute (a real value, not 5.0).
        let native = resolve_et0_today_mm(0.0, &map, &fc, 40.0, 180);
        assert!(
            native > 0.0 && (native - 5.0).abs() > 0.01,
            "native = {native}"
        );
        // 3. No source + no forecast -> 5.0 fallback.
        let empty = ForecastSnapshot::default();
        assert_eq!(resolve_et0_today_mm(0.0, &map, &empty, 40.0, 180), 5.0);
        // native_et0_mm returns None on a temps-absent day.
        assert!(native_et0_mm(&DailyEntry::default(), 40.0, 180).is_none());
    }
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
    use super::{
        forecast_is_stale, resolve_current_conditions, LiveReadings, FORECAST_MAX_AGE_S,
        TEMPEST_LIVE_MAX_AGE_S,
    };
    use crate::forecast::snapshot::HourlyEntry;
    use crate::tempest::state::Snapshot as TempestSnapshot;

    const NOW: i64 = 1_700_000_000;

    fn tempest(last_packet_epoch: i64) -> TempestSnapshot {
        TempestSnapshot {
            last_packet_epoch,
            // A full live station owns all engine-critical fields at this epoch.
            air_temp_live_epoch: last_packet_epoch,
            wind_live_epoch: last_packet_epoch,
            rh_live_epoch: last_packet_epoch,
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
    fn partial_live_station_does_not_force_station_readings() {
        // The latent HIGH: a barometer-only live source keeps last_packet_epoch
        // fresh but provides no live air_temp/wind/rh. The engine must fall back
        // to the forecast for those fields PER FIELD, never treating the
        // forecast-filled / zero snapshot values as a live station reading.
        let mut t = tempest(NOW - 90);
        t.air_temp_live_epoch = 0;
        t.wind_live_epoch = 0;
        t.rh_live_epoch = 0;
        t.air_temp_f = 0.0;
        t.wind_avg_mph = 0.0;
        let h = hour();
        let (temp, wind, rh, src) = resolve_current_conditions(&t, Some(&h), NOW);
        assert_eq!(
            src,
            LiveReadings::ForecastFallback,
            "partial station != Station"
        );
        assert_eq!(temp, 55.0, "forecast temp, not the 0 snapshot value");
        assert_eq!(wind, 7.5);
        assert_eq!(rh, 64.0);
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

    // P0-2/P1-5: pin the forecast-staleness threshold at the assembly seam. A
    // stale forecast both gates the forward-looking rain SKIP rules and marks the
    // decision degraded, so the boundary behavior is safety-relevant.
    #[test]
    fn fresh_forecast_is_not_stale() {
        assert!(!forecast_is_stale(1_000, 1_000 + 3_600)); // 1h old
    }

    #[test]
    fn forecast_just_past_max_age_is_stale() {
        assert!(forecast_is_stale(1_000, 1_000 + FORECAST_MAX_AGE_S + 1));
    }

    #[test]
    fn forecast_exactly_at_max_age_is_still_fresh() {
        // `>` is strict: an age exactly at the bound is usable, not stale.
        assert!(!forecast_is_stale(1_000, 1_000 + FORECAST_MAX_AGE_S));
    }

    #[test]
    fn never_refreshed_forecast_is_stale() {
        // The "never refreshed" sentinel must fail safe regardless of `now`,
        // including a zero/negative clock that would make the age subtraction
        // misbehave without the explicit epoch <= 0 guard.
        assert!(forecast_is_stale(0, 99_999));
        assert!(forecast_is_stale(-1, 99_999));
        assert!(forecast_is_stale(0, 0));
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

// P1-5: end-to-end SNAPSHOT ASSEMBLY. These assert that `build_from_map`
// correctly assembles the published IrrigationSnapshot from the raw
// forecast/tempest stores + entity map: the aggregate verdict + reason for a
// clear skip and a clear run, the per-zone verdicts (source + verdict), and the
// two forecast-derived fields the refresher itself computes from the raw stores:
// `rain_observed_recent_in` (today's measured rain + the past observed window)
// and `heat_index_max_3day_f` (per-day temp×humidity pairing, NOT the now-
// humidity bug). build_from_map is private to this module, so the test calls it
// directly; no production seam is added.
#[cfg(test)]
mod snapshot_assembly_tests {
    use super::*;
    use crate::engine::skip_rules::heat_index_f;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
    use crate::tempest::state::Snapshot as TempestSnapshot;

    /// A fresh full-coverage live station packet at `now`: all three engine-
    /// critical fields (temp/wind/rh) carry the supplied values with live epochs
    /// at `now`, so resolve_current_conditions yields LiveReadings::Station and
    /// the decision is never failed-safe to "skip" on missing live data.
    fn live_station(
        now: i64,
        temp_f: f64,
        wind_mph: f64,
        rh_pct: f64,
        rain_today_in: f64,
    ) -> TempestSnapshot {
        TempestSnapshot {
            last_packet_epoch: now,
            air_temp_live_epoch: now,
            wind_live_epoch: now,
            rh_live_epoch: now,
            air_temp_f: temp_f,
            wind_avg_mph: wind_mph,
            rh_pct,
            rain_in_today: rain_today_in,
            source_label: "TestStation".into(),
            ..Default::default()
        }
    }

    /// One current hour of forecast so resolve_current_conditions always has a
    /// fallback (it never reaches Unavailable in these tests; the station is the
    /// live source). Mirrors the live station so a fallback would be benign.
    fn current_hour(temp_f: f64, wind_mph: f64, rh_pct: u32) -> HourlyEntry {
        HourlyEntry {
            temp_f,
            wind_mph,
            humidity_pct: rh_pct,
            ..Default::default()
        }
    }

    fn forecast_store_with(fc: ForecastSnapshot) -> ForecastStore {
        let s = ForecastStore::new();
        s.store(fc);
        s
    }

    fn tempest_store_with(t: TempestSnapshot) -> TempestStore {
        let s = TempestStore::new();
        s.store(t);
        s
    }

    fn zone_idents(slugs: &[&str]) -> Vec<crate::zones::ZoneIdent> {
        slugs
            .iter()
            .map(|s| crate::zones::ZoneIdent::new(*s, *s))
            .collect()
    }

    /// Assemble the snapshot the same way refresh_once does (empty HA entity map,
    /// no soil config, no scripts), but with the raw stores under test. Returns
    /// the published IrrigationSnapshot.
    async fn assemble(
        forecast: ForecastSnapshot,
        tempest: TempestSnapshot,
        zones: &[&str],
        policy: WateringPolicy,
    ) -> IrrigationSnapshot {
        let fs = forecast_store_with(forecast);
        let ts = tempest_store_with(tempest);
        let zone_runtime: HashMap<String, ZoneRuntime> = HashMap::new();
        let scripts = CompiledScripts::compile(&[]);
        build_from_map(
            HashMap::new(),
            &fs,
            &ts,
            &zone_idents(zones),
            &zone_runtime,
            &policy,
            &scripts,
            None, // sensor_history
            None, // forecast_obs
            None, // control
        )
        .await
    }

    // ── CLEAR RUN ─────────────────────────────────────────────────────────────
    // Dry, warm, calm, fresh station, no soil config: the assembled verdict is a
    // plain "run" with an empty reason, and every per-zone verdict is run/global.
    // heat_index_max_3day_f is asserted to be the PER-DAY pairing (each day's
    // high temp × THAT day's humidity), proving the now-humidity bug is absent.
    #[tokio::test]
    async fn assembles_clear_run_with_per_day_heat_index() {
        let now = Utc::now().timestamp();
        // Daily forecast: a hot-but-DRY-air day and a cooler humid day. The
        // hottest FEELS-LIKE day wins. Kept below the 95°F heat-advisory temp
        // gate so the verdict is a plain "run", not run_extended.
        let day_hi = DailyEntry {
            time_epoch: now,
            temp_max_f: 90.0,
            temp_min_f: 70.0,
            humidity_pct: 45, // the day's OWN afternoon RH
            precip_sum_in: 0.0,
            precip_probability_max: 0,
            wind_max_mph: 5.0,
            ..Default::default()
        };
        let day_cool = DailyEntry {
            time_epoch: now + 86_400,
            temp_max_f: 80.0,
            temp_min_f: 66.0,
            humidity_pct: 70,
            precip_sum_in: 0.0,
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            last_refresh_epoch: now, // fresh, so forecast rules are live
            source_reachable: true,
            daily: vec![day_hi.clone(), day_cool.clone()],
            hourly: vec![current_hour(72.0, 4.0, 50)],
            ..Default::default()
        };
        // Live station carries a SATURATED post-rain "now" humidity (97%), wildly
        // different from any day's afternoon RH. The buggy pairing (day max temp ×
        // now humidity) would inflate the 3-day heat index; the correct per-day
        // pairing uses the day's own RH.
        let tempest = live_station(now, 72.0, 3.0, 97.0, 0.0);

        let snap = assemble(
            fc,
            tempest,
            &["back_yard", "front_yard"],
            WateringPolicy::default(),
        )
        .await;

        // Aggregate verdict: a clean run, no skip reason.
        assert_eq!(
            snap.skip_check.verdict, "run",
            "reason: {}",
            snap.skip_check.reason
        );
        assert!(!snap.skip_check.will_skip);
        assert!(snap.skip_check.reason.is_empty());

        // Per-zone verdicts: one per resolved soil zone (with no soil config that
        // is the legacy 4-zone fallback), all run/global on a clean morning.
        assert!(!snap.zone_verdicts.is_empty());
        for v in &snap.zone_verdicts {
            assert_eq!(v.verdict, "run", "zone {} should run", v.zone_slug);
            assert_eq!(v.source, "global");
        }
        // The per-zone verdict is back-filled onto each configured ZoneState that
        // has a matching soil-zone verdict (the legacy fallback covers back_yard +
        // front_yard, the two zones configured here).
        for z in &snap.zones {
            assert_eq!(
                z.verdict.as_ref().map(|v| v.verdict.as_str()),
                Some("run"),
                "zone {} should have a run verdict back-filled",
                z.slug
            );
        }

        // heat_index_max_3day_f: the correct per-day pairing. The hot-dry day
        // (90°F @ 45%) out-feels the cool-humid day (80°F @ 70%).
        let expected_per_day = heat_index_f(90.0, 45.0).max(heat_index_f(80.0, 70.0));
        assert!(
            (snap.skip_check.heat_index_max_3day_f - expected_per_day).abs() < 1e-6,
            "assembled heat index {} must equal the per-day max {expected_per_day}",
            snap.skip_check.heat_index_max_3day_f
        );
        // And it must be the hot-dry day, not the cool-humid one.
        assert!((expected_per_day - heat_index_f(90.0, 45.0)).abs() < 1e-9);
        // The now-humidity bug (90°F paired with the saturated 97% "now") would be
        // MUCH higher. The assembled value must stay well below it.
        let buggy_now_pairing = heat_index_f(90.0, 97.0);
        assert!(
            snap.skip_check.heat_index_max_3day_f < buggy_now_pairing - 5.0,
            "per-day heat index {} must be far below the now-humidity bug {buggy_now_pairing}",
            snap.skip_check.heat_index_max_3day_f
        );
        // The Forecast block mirrors the same value.
        assert!(
            (snap.forecast.heat_index_max_3day_f - expected_per_day).abs() < 1e-6,
            "forecast block heat index must match the skip_check value"
        );

        // No observed rain anywhere -> the recent-rain backstop sees nothing.
        assert!((snap.skip_check.rain_observed_recent_in - 0.0).abs() < 1e-9);
    }

    // ── CLEAR SKIP (observed-rain backstop) ─────────────────────────────────────
    // Today's measured rain is small (below the already-wet floor) but the past
    // observed-rain window pushes the recent total over rain_skip_in, so the
    // sensor-independent observed-rain gate fires. This pins the assembly of
    // rain_observed_recent_in = today_used + past_n_day_precip_in(window).
    #[tokio::test]
    async fn assembles_clear_skip_from_observed_recent_rain() {
        let now = Utc::now().timestamp();
        // Past day (yesterday): a heavy 0.30" soaking, measured.
        let yesterday = DailyEntry {
            time_epoch: now - 86_400,
            precip_sum_in: 0.30,
            ..Default::default()
        };
        let today = DailyEntry {
            time_epoch: now,
            temp_max_f: 82.0,
            temp_min_f: 68.0,
            humidity_pct: 55,
            precip_sum_in: 0.0,
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            daily: vec![today.clone()],
            past_daily: vec![yesterday.clone()],
            hourly: vec![current_hour(74.0, 4.0, 60)],
            ..Default::default()
        };
        // Live station: today only 0.04" so far (below the 0.05" already-wet floor),
        // so the observed-recent gate, not already_wet, is the one that fires.
        let today_station_rain = 0.04;
        let tempest = live_station(now, 74.0, 3.0, 60.0, today_station_rain);

        // Default policy: rain_observed_window_days = 1, rain_skip_in = 0.25.
        let snap = assemble(
            fc,
            tempest,
            &["back_yard", "front_yard"],
            WateringPolicy::default(),
        )
        .await;

        // rain_observed_recent_in = today_used (max station/model = 0.04) + past 1 day (0.30).
        let expected_recent = today_station_rain + 0.30;
        assert!(
            (snap.skip_check.rain_observed_recent_in - expected_recent).abs() < 1e-6,
            "assembled observed-recent rain {} must equal today + past-window {expected_recent}",
            snap.skip_check.rain_observed_recent_in
        );
        // It clears the rain_skip_in threshold, so the run skips.
        assert!(
            expected_recent >= 0.25,
            "fixture must exceed the skip threshold"
        );
        assert_eq!(snap.skip_check.verdict, "skip");
        assert!(snap.skip_check.will_skip);
        // The reason names the observed-rain backstop (today + past window), not
        // already_wet (today alone, which is below its floor).
        assert!(
            snap.skip_check.reason.contains("in the last 2 day(s)"),
            "skip reason should be the observed-recent backstop, got: {}",
            snap.skip_check.reason
        );

        // Every zone inherits the global skip (the observed-rain backstop is a
        // hard pre-soil gate that binds every zone).
        assert!(!snap.zone_verdicts.is_empty());
        for v in &snap.zone_verdicts {
            assert_eq!(v.verdict, "skip", "zone {} should skip", v.zone_slug);
            assert_eq!(v.source, "global");
        }
    }

    // Guard: today's rain alone (below the observed-window total) must NOT skip,
    // so the skip in the test above is genuinely driven by the PAST observed
    // window, not by today's measured rain leaking past a threshold.
    #[tokio::test]
    async fn observed_recent_without_past_window_runs() {
        let now = Utc::now().timestamp();
        let today = DailyEntry {
            time_epoch: now,
            temp_max_f: 82.0,
            temp_min_f: 68.0,
            humidity_pct: 55,
            precip_sum_in: 0.0,
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            daily: vec![today],
            past_daily: vec![], // no past observed rain
            hourly: vec![current_hour(74.0, 4.0, 60)],
            ..Default::default()
        };
        // 0.04" today, below the 0.05" already-wet floor and far below 0.25".
        let tempest = live_station(now, 74.0, 3.0, 60.0, 0.04);
        let snap = assemble(fc, tempest, &["back_yard"], WateringPolicy::default()).await;

        assert!((snap.skip_check.rain_observed_recent_in - 0.04).abs() < 1e-6);
        assert_eq!(
            snap.skip_check.verdict, "run",
            "today-only 0.04\" must not trip any rain gate; reason: {}",
            snap.skip_check.reason
        );
    }
}
