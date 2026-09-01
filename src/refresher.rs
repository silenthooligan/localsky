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

/// Per-zone agronomy the cycle-and-soak planner reads each evaluation
/// (smart_morning::build_cycle_plan + the refresher's next-run wall-time
/// estimate). Carried on the hot-swapped WateringPolicy, keyed by the
/// underscore-normalized slug, so an applied soil_texture / slope /
/// sprinkler / precip-rate change reshapes the NEXT computed plan with
/// no restart. These fields were previously read from boot-bound
/// structures (main.rs zone_runtime + smart_morning's boot cfg Arc) and
/// silently required a restart.
#[derive(Debug, Clone, Copy)]
pub struct ZoneAgronomyCfg {
    pub sprinkler_type: crate::config::schema::SprinklerType,
    pub precip_rate_mm_hr: Option<f64>,
    pub soil_texture: crate::config::schema::SoilTexture,
    pub slope_pct: f64,
    /// Configured grass/planting species. Feeds `kc_at_doy_lat` for the
    /// zone's crop coefficient, which is where Kc comes from now that the
    /// Smart Irrigation entity's `multiplier` attribute is gone.
    pub species: crate::config::schema::GrassSpecies,
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
    /// Cycle/soak dispatch knobs from `cfg.engine`, carried here so BOTH the
    /// smart-morning scheduler's per-tick window math and the refresher's
    /// next-run estimate read the LIVE values on every evaluation (a settings
    /// save applies at the next tick, no restart). The boot Config Arc those
    /// paths also hold remains only for build_cycle_plan's per-zone lookups.
    /// The `Default` derive zeroes/falses these; every real policy comes from
    /// `from_config`, and the Default-policy paths bail on the unset location
    /// before either knob is read.
    pub soak_minutes: u32,
    pub interleave_cycles: bool,
    /// Rain-defer threshold per session (inches over the next 24 forecast
    /// hours, probability weighted), from `cfg.engine.session_rain_defer_in`.
    /// The live assembly used to pass the compile-time constant instead, so
    /// this documented, editable knob changed nothing. `Default` derives 0.0;
    /// `defer_threshold_in` treats a non-positive value as "use the built-in
    /// default" so a Default-policy path keeps the historical behavior.
    pub session_rain_defer_in: f64,
    /// Per-zone run-duration sizing (throughput + max-duration), keyed by
    /// underscore-normalized slug. Previously a boot-built HashMap moved
    /// into spawn_refresher; carried here so a hot-reloaded
    /// precip_rate_mm_hr / sprinkler_type re-sizes runs on the next tick.
    /// Empty on the Default policy (unconfigured installs); readers fall
    /// back to ZoneRuntime::fallback per missing zone, as before.
    pub zone_runtime: HashMap<String, ZoneRuntime>,
    /// Per-zone cycle/soak agronomy for build_cycle_plan, keyed by
    /// underscore-normalized slug. Same hot-reload contract as
    /// zone_runtime; empty map = every zone falls back to a single
    /// no-split segment (the pre-config behavior).
    pub zone_agronomy: HashMap<String, ZoneAgronomyCfg>,
    /// Home Assistant helper entities the 0.7.22 adoption pass has handled,
    /// from `cfg.ha_adoption`. Carried on the policy so the read gate and the
    /// snapshot's copy of the record both hot-reload with the config.
    ///
    /// The record is a migration LEDGER, so every write path carries it
    /// forward rather than accepting whatever the incoming document holds:
    /// `PUT /api/config` and `PUT /api/config/raw` restore it from the stored
    /// config, and a rollback unions the pre-rollback ledger back in. Dropping
    /// a marker would put a read back on a helper the notice invited the owner
    /// to delete, which is not "the engine on a value it can explain", it is
    /// the vacation pause reading zero.
    pub ha_adoption: Vec<crate::ha::snapshot::HaAdoptedHelper>,
}

impl WateringPolicy {
    /// Whether the legacy Home Assistant read for `entity` is still live.
    ///
    /// This is the whole cutover. While an entity is unadopted the read
    /// behaves exactly as it always did; the instant the pass records it,
    /// LocalSky's own value governs and the entity is never consulted again,
    /// whatever the pass found. That ordering is deliberate: there is no
    /// moment where the value is neither adopted nor read, so a Home
    /// Assistant outage across the upgrade decides nothing.
    ///
    /// The unreachable read paths this leaves behind come out in the next
    /// release, once installs have migrated. They stay here for one release
    /// so an install that has not run the pass yet keeps working.
    pub fn ha_read_retired(&self, entity: &str) -> bool {
        self.ha_adoption.iter().any(|h| h.entity == entity)
    }
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
            ha_adoption: cfg.ha_adoption.clone(),
            ha_sprinkler_prefix: cfg.deployment.ha_sprinkler_prefix.clone(),
            seasonal_adjust_pct: cfg.engine.seasonal_adjust_pct,
            units: cfg.deployment.units,
            soak_minutes: cfg.engine.soak_minutes,
            interleave_cycles: cfg.engine.interleave_cycles,
            session_rain_defer_in: cfg.engine.session_rain_defer_in,
            // Per-zone run sizing + cycle/soak agronomy. Underscore-normalized
            // like soil_zones/budget_zones so runtime slug lookups hit. The
            // cap comes from the zone's configured max_run_minutes; unset
            // resolves to the historical 60 minute boot value.
            zone_runtime: cfg
                .zones
                .iter()
                .map(|(slug, z)| {
                    (
                        slug.replace('-', "_"),
                        ZoneRuntime {
                            throughput_mm_hr: crate::engine::effective_precip_rate_mm_hr(
                                z.sprinkler_type,
                                z.precip_rate_mm_hr,
                            ),
                            max_duration_s: z.max_run_minutes.unwrap_or(60) * 60,
                        },
                    )
                })
                .collect(),
            zone_agronomy: cfg
                .zones
                .iter()
                .map(|(slug, z)| {
                    (
                        slug.replace('-', "_"),
                        ZoneAgronomyCfg {
                            sprinkler_type: z.sprinkler_type,
                            precip_rate_mm_hr: z.precip_rate_mm_hr,
                            soil_texture: z.soil_texture,
                            slope_pct: z.slope_pct,
                            species: z.species,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl WateringPolicy {
    /// The rain-defer threshold the balance should use (inches). A
    /// non-positive configured value (including the `Default` derive's 0.0
    /// on unconfigured paths) falls back to the engine constant, so a
    /// missing knob keeps the historical threshold rather than deferring on
    /// any trace of forecast rain.
    pub fn defer_threshold_in(&self) -> f64 {
        if self.session_rain_defer_in > 0.0 {
            self.session_rain_defer_in
        } else {
            crate::engine::SESSION_RAIN_DEFER_IN
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

/// True when `seasonal_capped`'s clamp is what set the returned seconds: the
/// dial scaled the allocator's session past the zone's ceiling.
///
/// Every clamp that can shorten a dispatched run reports itself through
/// `ZoneMath::cap_binding`, so the panel's warning does not depend on which
/// stage did the shortening. There are three: the allocator's own
/// `WaterBudget::session_capped` (the ideal weekly slice was wider than the
/// ceiling), this seasonal clamp, and the condition-rule multiplier in
/// `apply_verdict_multiplier`. Before, the dial clipped a run silently while
/// the rule multiplier said so, which made the same shortening readable or
/// invisible depending on which knob caused it.
///
/// Mirrors `seasonal_capped`'s arithmetic exactly so the two can never
/// disagree about whether the ceiling was reached.
fn seasonal_cap_binds(raw_seconds: u32, seasonal_pct: u32, max_dur: u32) -> bool {
    max_dur > 0
        && ((raw_seconds as f64 * seasonal_multiplier(seasonal_pct)).round() as u32) > max_dur
}

/// Apply each zone's custom condition-rule watering multiplier (from an
/// `AdjustMultiplier` rule action, clamped to [0.5, 1.5] by the engine in
/// `decide_per_zone`) to its dispatched run time, so a "halve the veg garden
/// when humidity is high" style rule actually shrinks (or extends) the run
/// instead of being a silent no-op. `planned_run_seconds` already reflects the
/// seasonal dial, the ET heat multiplier, and the per-zone / regulatory
/// max-duration cap; this layers the user's explicit rule on top and RE-CAPS
/// at the zone's `max_duration_seconds` so a >1.0 multiplier can never push a
/// run past its safety ceiling. A multiplier of exactly 1.0 (every zone with
/// no `AdjustMultiplier` rule) is a no-op, so installs without such a rule are
/// byte-identical. Call this ONCE per refresh, after `planned_run_seconds` is
/// finalized and `apply_engine` has back-filled `z.verdict`.
fn apply_verdict_multiplier(snap: &mut crate::ha::snapshot::IrrigationSnapshot) {
    for z in snap.zones.iter_mut() {
        if z.planned_run_seconds == 0 {
            continue;
        }
        let mult = z.verdict.as_ref().map(|v| v.multiplier).unwrap_or(1.0);
        if (mult - 1.0).abs() <= f64::EPSILON {
            continue;
        }
        let max_dur = z.math.as_ref().map(|m| m.max_duration_seconds).unwrap_or(0);
        let scaled = ((z.planned_run_seconds as f64) * mult).round().max(0.0) as u32;
        z.planned_run_seconds = if max_dur > 0 {
            scaled.min(max_dur)
        } else {
            scaled
        };
        if let Some(m) = z.math.as_mut() {
            m.scheduled_seconds = z.planned_run_seconds;
            // A rule multiplier that ran into the ceiling shortens the run
            // exactly as the allocator's own cap does, so the panel says so.
            // Assignment, not a set-only branch: a multiplier below 1.0 pulls
            // the run back under the ceiling, and then the ceiling is no
            // longer what set the minutes even if an earlier stage had
            // flagged it. Same predicate as `apply_budget_plan`: the run has
            // to sit ON the ceiling for the ceiling to be the reason.
            m.cap_binding = max_dur > 0
                && z.planned_run_seconds == max_dur
                && (scaled > max_dur || m.cap_binding);
        }
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

/// One budget row per ACTIVE zone, config-backed where a row exists and
/// synthesized (no explicit target, so the allocator resolves the
/// agronomic slug default) where it does not.
///
/// The active zone list and the budget rows are resolved independently:
/// zones come from `cfg.zones` OR the `LOCALSKY_ZONES` env var, while
/// `budget_zones` is built from `cfg.zones` alone. An install zoned by
/// the env var therefore has a non-empty zone list and an empty budget
/// list, and since 0.7.22 the allocator is what sizes dispatch on every
/// path: without a row per zone, `apply_budget_plan` would find no plan
/// for any slug and set every `planned_run_seconds` to 0, which is the
/// `<slug>_planned_run` descriptor and the `zone_<slug>_planned_seconds`
/// MQTT sensor that Irrigation Unlimited automations drive valves from.
/// Those installs would have stopped watering with nothing on screen.
///
/// Rows the config supplies are passed through untouched, and a
/// configured zone that is not in the active list keeps its row, so this
/// only ever ADDS rows.
pub fn budget_zones_for_active(
    active: &[crate::zones::ZoneIdent],
    configured: &[ZoneBudgetCfg],
) -> Vec<ZoneBudgetCfg> {
    let mut out = configured.to_vec();
    for z in active {
        if out.iter().any(|c| c.slug == z.slug) {
            continue;
        }
        out.push(ZoneBudgetCfg {
            slug: z.slug.clone(),
            name: z.display_name.clone(),
            // No explicit target: `compute_water_budgets` resolves the
            // agronomic slug default, which is what the zone waters on
            // until the operator sets one.
            weekly_budget_in: None,
            sessions_per_week: None,
        });
    }
    out
}

/// Trailing window the water balance settles against: rolling 7 local
/// days ending now (day-keyed for the rain ledger, epoch-keyed for the
/// runs evidence). No calendar-week anchor.
pub const BALANCE_WINDOW_DAYS: i64 = 7;

/// One zone's run-history evidence for the balance, pre-computed once
/// per tick (never a per-zone SQLite query inside the zone loop).
#[derive(Debug, Clone, Copy, Default)]
pub struct ZoneRunEvidence {
    /// Union valve-open seconds across clustered completed watering
    /// events, clamped to the trailing window.
    pub applied_open_s: i64,
    /// Clustered watering events inside the trailing window.
    pub sessions_done: u32,
    /// End epoch of the latest completed watering event (0 = none).
    pub last_run_epoch: i64,
}

/// Cross-zone balance inputs computed once per refresher tick from the
/// stores (runs history, rain ledger, bias model) and passed into the
/// sync snapshot build. `None` (no history DB and no forecast archive)
/// degrades the balance to target-only sizing, exactly the honest
/// fallback for an install with no evidence.
#[derive(Debug, Clone)]
pub struct BalanceTick {
    /// Observed rain over the trailing window (mm), ladder-resolved.
    pub observed_rain_mm: f64,
    /// "gauge" | "radar" | "model_archive" | "none".
    pub observed_rain_source: String,
    /// Forecast bias model (identity when under-trained or absent).
    pub bias: crate::engine::BiasModel,
    /// Per-zone run evidence, keyed by underscore-normalized slug.
    pub per_zone: HashMap<String, ZoneRunEvidence>,
}

/// Resolve the balance's observed-rain term from the per-source ledger
/// sums plus the forecast provider's past-day archive. Precedence is by
/// COVERAGE, never by value: when any measured rows (gauge/radar; legacy
/// rows count as gauge only on station installs) exist in the window,
/// the measured rung wins outright, even at 0.00 in; a yard that
/// measured a dry week is ground truth a wetter regional model must not
/// override. The model side (the max() of the provider archive and any
/// model-quality legacy rows, so neither hides rain the other saw)
/// supplies the term only when measured coverage is entirely absent.
/// Returns (mm, source rung).
fn resolve_observed_rain(
    win: &crate::persistence::ObservedRainWindow,
    station_present: bool,
    archive_past_in: f64,
) -> (f64, String) {
    let legacy_as_gauge = station_present;
    let measured_days =
        win.gauge_days + win.radar_days + if legacy_as_gauge { win.legacy_days } else { 0 };
    if measured_days > 0 {
        let gauge_in = win.gauge_in + if legacy_as_gauge { win.legacy_in } else { 0.0 };
        let radar_in = win.radar_in;
        let source = if radar_in > gauge_in {
            "radar"
        } else {
            "gauge"
        };
        ((gauge_in + radar_in) * 25.4, source.to_string())
    } else {
        let model_rows_in = win.model_in + if legacy_as_gauge { 0.0 } else { win.legacy_in };
        let model_side_in = archive_past_in.max(model_rows_in);
        if model_side_in > 0.0 {
            (model_side_in * 25.4, "model_archive".to_string())
        } else {
            (0.0, "none".to_string())
        }
    }
}

/// How long a computed BalanceTick may serve before the stores are
/// re-read (a coarse timer; a run edge also invalidates it). Keeps the
/// runs/ledger/bias SQLite reads off the 10s refresh path.
const BALANCE_CACHE_MAX_AGE_S: i64 = 60;

/// Gather the balance's store-backed inputs once per (cached) tick:
/// per-source ledger rain sums resolved through the observed-rain
/// ladder, the bias model, and the per-zone clustered run evidence.
/// Every read degrades independently (no history DB = no applied term
/// and identity bias; the archive rung still works from the forecast).
async fn compute_balance_tick(
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
    runs_store: Option<&crate::persistence::RunsStore>,
    obs_store: Option<&crate::persistence::ForecastObservationsStore>,
) -> BalanceTick {
    let now = chrono::Utc::now().timestamp();
    let win = match obs_store {
        Some(s) => s
            .observed_rain_window_by_source(BALANCE_WINDOW_DAYS)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "balance ledger read failed");
                Default::default()
            }),
        None => Default::default(),
    };
    let fc = forecast_store.snapshot();
    // Past days only (today's model total belongs to the forward side).
    let archive_past_in = fc.past_n_day_precip_in((BALANCE_WINDOW_DAYS - 1) as usize);
    let t = tempest_store.snapshot();
    let station_present = t.has_live_station || !t.station_serial.is_empty();
    let (observed_rain_mm, observed_rain_source) =
        resolve_observed_rain(&win, station_present, archive_past_in);
    let bias = match obs_store {
        Some(s) => match s
            .recent(crate::engine::forecast_bias::DEFAULT_WINDOW_DAYS)
            .await
        {
            Ok(rows) => crate::engine::BiasModel::from_observations(
                &rows,
                crate::timeutil::now_local().date_naive(),
                None,
            ),
            Err(e) => {
                tracing::debug!(error = %e, "balance bias read failed");
                crate::engine::BiasModel::identity()
            }
        },
        None => crate::engine::BiasModel::identity(),
    };
    let per_zone = match runs_store {
        Some(rs) => {
            // One extra day of margin so an event straddling the window
            // start is fetched and then truncated, never missed.
            match rs
                .window(now - (BALANCE_WINDOW_DAYS + 1) * 86400, now + 1)
                .await
            {
                Ok(rows) => build_zone_run_evidence(&rows, now - BALANCE_WINDOW_DAYS * 86400, now),
                Err(e) => {
                    tracing::debug!(error = %e, "balance runs window read failed");
                    HashMap::new()
                }
            }
        }
        None => HashMap::new(),
    };
    BalanceTick {
        observed_rain_mm,
        observed_rain_source,
        bias,
        per_zone,
    }
}

/// Group completed watering evidence per zone: filter rows through the
/// shared watering-evidence rule, truncate to the trailing window,
/// cluster (union semantics de-duplicate manual + observer rows), and
/// reduce to the per-zone evidence the balance reads.
fn build_zone_run_evidence(
    rows: &[crate::persistence::RunRow],
    window_start: i64,
    window_end: i64,
) -> HashMap<String, ZoneRunEvidence> {
    use crate::engine::tuning::{applied_in_window, is_watering_evidence, RunSegment};
    let mut segments_by_zone: HashMap<String, Vec<RunSegment>> = HashMap::new();
    let mut last_end_by_zone: HashMap<String, i64> = HashMap::new();
    for r in rows {
        if !is_watering_evidence(&r.source, &r.status, r.skip_reason.as_deref()) {
            continue;
        }
        let slug = r.zone_slug.replace('-', "_");
        let end = r
            .end_epoch
            .unwrap_or(r.start_epoch + r.duration_s.unwrap_or(0) as i64);
        segments_by_zone
            .entry(slug.clone())
            .or_default()
            .push(RunSegment {
                start_epoch: r.start_epoch,
                end_epoch: end,
            });
        let e = last_end_by_zone.entry(slug).or_insert(0);
        *e = (*e).max(end);
    }
    segments_by_zone
        .into_iter()
        .map(|(slug, segs)| {
            let applied = applied_in_window(&segs, window_start, window_end);
            let last = last_end_by_zone.get(&slug).copied().unwrap_or(0);
            (
                slug,
                ZoneRunEvidence {
                    applied_open_s: applied.valve_open_s,
                    sessions_done: applied.events,
                    last_run_epoch: last,
                },
            )
        })
        .collect()
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
    // Per-zone run sizing (zone_runtime) and cycle/soak agronomy now ride
    // the hot-swapped watering_policy instead of boot-bound arguments, so
    // an applied texture/sprinkler/precip change takes effect next tick.
    zones: Vec<crate::zones::ZoneIdent>,
    // Config store for the one-time Home Assistant helper adoption pass, the
    // sink for the three adopted thresholds and for the marker list. `None`
    // on a fresh install with no config file yet, where there is nothing to
    // adopt into and the app is still in the wizard.
    cfg_store: Option<Arc<crate::config::FileConfigStore>>,
) {
    // The swappable handle, kept beside the per-tick `load()` below: the
    // adoption pass rebuilds the policy from the freshly saved config and
    // stores it here, so the adopted thresholds AND the read cutover go live
    // on the same swap.
    let policy_handle = watering_policy.clone();
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

        // Runs handle for the balance's applied-irrigation evidence.
        let runs_store = history_conn
            .as_ref()
            .map(|c| crate::persistence::RunsStore::new(c.clone()));
        // Balance evidence cache: refreshed on a coarse timer or when a
        // run edge may have landed, never per 10s tick.
        let mut balance_tick: Option<BalanceTick> = None;
        let mut balance_fetched_epoch: i64 = 0;

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
        // The last control row that actually came back. Once the helper
        // reads are retired this store is the ONLY home of the vacation
        // pause, so resolving a failed SELECT to the default would read as
        // "not paused" and dispatch a morning somebody held. Reusing the
        // last good row keeps the hold across a transient error; it only
        // falls back to nothing before the first successful read of the
        // process, which is a window no watering decision has run in yet.
        let mut last_control: Option<crate::persistence::IrrigationControlState> = None;
        // The one-time Home Assistant helper adoption pass. Armed only on the
        // HA path with a config file to write into; `None` disarms it for the
        // life of the process, which is where it lands the moment there is
        // nothing left to adopt.
        // Once per process, the first tick on the Home Assistant path that
        // plans a run for a zone on a name-inferred target: those installs
        // dispatched nothing before 0.7.22, and an owner who never opens the
        // UI would otherwise learn it from the valves.
        let mut inferred_plan_announced = false;
        let mut adopt: Option<AdoptState> = match (source, cfg_store.as_ref(), client.as_ref()) {
            (SnapshotSource::HomeAssistant, Some(store), Some(c)) => Some(AdoptState {
                cfg_store: store.clone(),
                control_store: control_store.clone(),
                policy: policy_handle.clone(),
                helpers: HelperFetch::Live(c.clone()),
                fingerprint: None,
                stable_ticks: 0,
                awaiting_config: false,
                no_config_warned: false,
                pending_revert: None,
            }),
            _ => None,
        };
        if source == SnapshotSource::HomeAssistant {
            if adopt.is_none() {
                // No config file to write the ledger into, so the reads can
                // never retire and every helper decides forever. Visible
                // rather than silent.
                tracing::warn!(
                    "home assistant helper adoption cannot run: no config store, so the \
                     helper reads stay live indefinitely"
                );
            } else if control_store.is_none() {
                // The three thresholds still migrate; the four controls have
                // nowhere to be kept, so their helpers keep deciding. The
                // migration notice says the same thing on screen.
                tracing::warn!(
                    "home assistant helper adoption: no persistence database, so the four \
                     operator controls keep reading their helpers"
                );
            }
        }

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
            let mut readout: Option<HelperReadout> = None;
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
                        control.as_ref(),
                    )
                    .await
                    .map(|(mut snap, r)| {
                        readout = Some(r);
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
                    control.as_ref(),
                )
                .await),
            };
            // One tick of the Home Assistant helper adoption pass. Runs only
            // on a successful poll, and disarms itself for the life of the
            // process the moment there is nothing left to adopt.
            if let (Some(st), Some(r)) = (adopt.as_mut(), readout.as_ref()) {
                if adopt_tick(st, r).await {
                    adopt = None;
                }
            }
            // Carried onto the snapshot so the migration notice can say, on an
            // install with no config file, that the helpers are still deciding
            // rather than staying silent about a migration that never ran.
            let awaiting_config = adopt.as_ref().is_some_and(|s| s.awaiting_config);
            let result = result.map(|mut snap| {
                snap.ha_adoption_awaiting_config = awaiting_config;
                snap
            });
            let sleep_for = match result {
                Ok(snap) => {
                    // Shadow: build the native snapshot alongside HA for
                    // side-by-side comparison. Never drives dispatch.
                    if let Some(ss) = &shadow_store {
                        let native = refresh_once_native(
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
                            control.as_ref(),
                        )
                        .await;
                        ss.store(native);
                    }
                    if let Some(db) = history_conn.as_ref() {
                        // Zones whose running state is a dry-run
                        // controller's pretend water this tick: the
                        // observer records those rows as source
                        // 'dry_run' so they never become watering
                        // evidence.
                        let simulated = IngestState::simulated_running_slugs(&controllers).await;
                        let runs_written = ingest.observe(db, &snap, &simulated).await;
                        if runs_written > 0 {
                            // A run-end row just landed: force the next
                            // tick's balance re-read so applied credit,
                            // sessions_done, and the spacing anchor see it
                            // immediately instead of after the coarse timer.
                            balance_fetched_epoch = 0;
                        }
                    }
                    // Forecast-bias daily ingest. Today's predicted rain
                    // comes from the forecast store's daily[0]; today's
                    // observed rain is the MERGE-CONTESTED daily total
                    // (gauge and radar day products contest it with
                    // writer labels), NOT the station-gated skip_check
                    // value, tagged with the owning writer's nature. The
                    // store keeps the DAY MAX so a gauge going stale
                    // mid-storm can never reset the day's total. A day
                    // whose owner is a model fill, stale, or absent
                    // records 0.0 with source 'none' (a placeholder,
                    // excluded from bias training and the dryness
                    // counters); see ledger_observation for the midnight
                    // and plausibility gates.
                    if let Some(obs_store) = forecast_obs_store.as_ref() {
                        // Configured-timezone date, not the container's: a UTC
                        // container would otherwise file evening observations
                        // under tomorrow's row.
                        let today = crate::timeutil::now_local().date_naive();
                        let predicted_in = forecast_store
                            .snapshot()
                            .daily
                            .first()
                            .map(|d| d.precip_sum_in)
                            .unwrap_or(0.0);
                        let now_epoch = chrono::Utc::now().timestamp();
                        let owner = tempest_store.rain_today_owner(now_epoch);
                        // None = skip this tick (midnight carry gate, or an
                        // implausible value); the next same-day observation
                        // writes normally.
                        if let Some((observed_in, source)) =
                            ledger_observation(&tempest_store.snapshot(), owner.as_ref(), now_epoch)
                        {
                            let store_handle = obs_store.clone();
                            tokio::spawn(async move {
                                if let Err(e) = store_handle
                                    .upsert(today, predicted_in, observed_in, source)
                                    .await
                                {
                                    tracing::debug!(
                                        error = %e,
                                        "forecast observation upsert failed"
                                    );
                                }
                            });
                        }
                    }
                    if source == SnapshotSource::HomeAssistant && !inferred_plan_announced {
                        let planned: Vec<&crate::ha::snapshot::WaterBudget> = snap
                            .water_budgets
                            .iter()
                            .filter(|b| b.target_inferred && b.today_seconds > 0)
                            .collect();
                        if !planned.is_empty() {
                            inferred_plan_announced = true;
                            for b in &planned {
                                tracing::warn!(
                                    zone = %b.zone_slug,
                                    weekly_budget_in = b.weekly_budget_in,
                                    sessions_per_week = b.sessions_per_week,
                                    today_seconds = b.today_seconds,
                                    "zone plans a run on a weekly target inferred from its name; \
                                     set Weekly target and Sessions per week under Settings, then \
                                     Zones"
                                );
                            }
                            push.emit(crate::push::PushEvent::InferredTargetsPlanned {
                                zones: planned.iter().map(|b| b.zone_name.clone()).collect(),
                            });
                        }
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
    balance: Option<&BalanceTick>,
    controllers: &ControllerRegistry,
    control: Option<&crate::persistence::IrrigationControlState>,
) -> anyhow::Result<(IrrigationSnapshot, HelperReadout)> {
    // Sampled BEFORE the read, so a control write that lands while this call
    // is in flight moves the counter and forces the adoption commit to
    // re-earn its evidence rather than planning from the pre-write answer.
    let write_seq = crate::ha_adopt::write_seq();
    let states = client.states().await?;
    let map = states_to_map(states);
    // What the adoption pass needs, taken before the map is consumed: just
    // the seven helper entities, plus how many entities Home Assistant
    // answered with at all (a zero-entity answer is not a working HA and is
    // never evidence that a helper is missing).
    let readout = HelperReadout::from_map(&map, write_seq);
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
        balance,
        control,
        true,
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
    overlay_reporting_controllers(&mut snap, controllers).await;
    // Custom-rule watering multiplier (AdjustMultiplier condition action) is
    // applied to the finalized per-zone run time here, where both the planned
    // seconds and the back-filled verdict are final. No-op for the common case
    // (no such rule => multiplier 1.0).
    apply_verdict_multiplier(&mut snap);
    Ok((snap, readout))
}

/// `/api/states` as a map keyed by entity id. Shared by the snapshot build
/// and by the adoption pass's own commit-time re-read.
fn states_to_map(states: Vec<Value>) -> HashMap<String, Value> {
    states
        .into_iter()
        .filter_map(|v| {
            v.get("entity_id")
                .and_then(|e| e.as_str())
                .map(|id| (id.to_string(), v.clone()))
        })
        .collect()
}

/// The seven helper entities as Home Assistant last answered, plus the size
/// of the answer and the pre-adoption write counter as it stood when the
/// answer was taken. Carried out of `refresh_once` so the adoption pass runs
/// in the loop body rather than inside the snapshot build.
#[derive(Debug, Clone, Default)]
pub(crate) struct HelperReadout {
    pub total_entities: usize,
    /// `ha_adopt::write_seq()` sampled immediately BEFORE the `/api/states`
    /// call that produced this answer.
    pub write_seq: u64,
    pub helpers: HashMap<String, Value>,
}

impl HelperReadout {
    fn from_map(map: &HashMap<String, Value>, write_seq: u64) -> Self {
        HelperReadout {
            total_entities: map.len(),
            write_seq,
            helpers: crate::ha_adopt::ENTITIES
                .iter()
                .filter_map(|id| map.get(*id).map(|v| ((*id).to_string(), v.clone())))
                .collect(),
        }
    }

    /// The stability key: the seven answers AND how many entities Home
    /// Assistant answered with at all.
    ///
    /// The count is load-bearing. A Home Assistant that is up and still
    /// registering entities gives the byte-identical `<absent>` string for
    /// all seven helpers on every tick, so without the count three identical
    /// ticks of a still-starting install look exactly like three ticks of an
    /// install that has no helpers. A climbing count resets the counter; a
    /// steady-state Home Assistant has a fixed one, and the only false resets
    /// are a genuine device add or removal, which costs one extra window on a
    /// pass that runs once per install.
    fn stability_key(&self) -> String {
        format!(
            "{}|n={}",
            crate::ha_adopt::fingerprint(&self.helpers),
            self.total_entities
        )
    }
}

/// Where `adopt_tick` re-reads the answer set it commits from.
pub(crate) enum HelperFetch {
    /// Production: `/api/states` through the live client.
    Live(HaClient),
    /// Tests: the answers a re-read returns, in order. `None` is a failed
    /// read, and so is an exhausted queue.
    #[cfg(test)]
    Canned(std::sync::Mutex<std::collections::VecDeque<Option<HelperReadout>>>),
}

impl HelperFetch {
    /// Re-read the seven helpers now. `None` on any failure: the commit
    /// defers rather than planning from an answer it could not confirm.
    async fn read(&self) -> Option<HelperReadout> {
        match self {
            HelperFetch::Live(c) => {
                let write_seq = crate::ha_adopt::write_seq();
                match c.states().await {
                    Ok(states) => Some(HelperReadout::from_map(&states_to_map(states), write_seq)),
                    Err(e) => {
                        tracing::debug!(error = %e, "ha helper adoption: commit-time re-read failed");
                        None
                    }
                }
            }
            #[cfg(test)]
            HelperFetch::Canned(q) => q.lock().expect("canned queue").pop_front().flatten(),
        }
    }
}

/// Consecutive ticks the answer set must come back identical before the pass
/// concludes anything. The counter starts at 1 on the first sighting, so
/// three ticks at the 10 second cadence is twenty seconds of observation.
const ADOPT_STABLE_TICKS: u32 = 3;

/// The window that applies when a control helper is MISSING rather than
/// present and holding something. Five minutes.
///
/// Absence is the one shape where a retirement moves a PROTECTED gate onto a
/// column no human wrote: before 0.7.22 the Home Assistant path never wrote
/// the control store, so those columns hold M0017 defaults. It is also
/// exactly the answer Home Assistant gives while its `input_*` platforms are
/// still setting up, and on the Home Assistant OS add-on LocalSky and Home
/// Assistant restart together on every host reboot with the supervisor
/// starting the add-on first, so that shape is common rather than
/// theoretical. Waiting costs nothing: there is nothing to adopt from an
/// absence, and until the pass concludes, every read behaves exactly as it
/// did before the upgrade.
///
/// Thirty-one sightings, because the counter starts at 1 on the first: thirty
/// intervals at the ten second cadence, five minutes of observation.
const ADOPT_STABLE_TICKS_ABSENT: u32 = 31;

/// Live state of the one-time adoption pass. Dropped for the life of the
/// process the moment there is nothing left to adopt.
struct AdoptState {
    cfg_store: Arc<crate::config::FileConfigStore>,
    control_store: Option<crate::persistence::IrrigationControlStore>,
    policy: Arc<ArcSwap<WateringPolicy>>,
    /// Where the commit re-reads the answer set from, so it never plans from
    /// an answer taken before the write it is about to overwrite.
    helpers: HelperFetch,
    fingerprint: Option<String>,
    stable_ticks: u32,
    /// True while the commit cannot run because there is no `localsky.toml`
    /// to record it in. Copied onto every snapshot so the migration notice
    /// can say the helpers are still deciding on this install.
    awaiting_config: bool,
    /// The one warning about that, per process.
    no_config_warned: bool,
    /// Control rows a failed attempt wrote and could not put back yet. Until
    /// they are restored nothing is planned: the planner would read them as
    /// an operator answer.
    pending_revert: Option<PendingRevert>,
}

/// The control rows a failed pass has to put back: the state read under the
/// guard before anything was written, and which columns were written. Filled
/// in as each write lands, so a write that never happened is never undone.
struct PendingRevert {
    before: crate::persistence::IrrigationControlState,
    day: String,
    pause_until: bool,
    override_tomorrow: bool,
    is_paused: bool,
    is_dry_run: bool,
}

impl PendingRevert {
    fn new(before: &crate::persistence::IrrigationControlState, day: &str) -> Self {
        Self {
            before: before.clone(),
            day: day.to_string(),
            pause_until: false,
            override_tomorrow: false,
            is_paused: false,
            is_dry_run: false,
        }
    }
}

/// Put back the columns a failed pass wrote. Nothing native writes these four
/// columns on a Home Assistant deployment before the pass has adopted them
/// (every control write still routes to the helper until then), so restoring
/// the pre-write state cannot clobber an operator's answer.
async fn revert_control_writes(
    cs: &crate::persistence::IrrigationControlStore,
    p: &PendingRevert,
) -> Result<(), String> {
    if p.pause_until {
        cs.set_pause_until(p.before.pause_until_epoch)
            .await
            .map_err(|e| e.to_string())?;
    }
    if p.override_tomorrow {
        cs.set_override_tomorrow_on(p.before.override_tomorrow.clone(), p.day.clone())
            .await
            .map_err(|e| e.to_string())?;
    }
    if p.is_paused {
        cs.set_paused(p.before.is_paused)
            .await
            .map_err(|e| e.to_string())?;
    }
    if p.is_dry_run {
        cs.set_dry_run(p.before.is_dry_run)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Restore now, or park the restore so the next tick retries it before
/// planning anything. A restore that keeps failing keeps the pass parked,
/// which is the safe side: an unadopted control behaves exactly as before.
async fn revert_or_park(
    cs: &crate::persistence::IrrigationControlStore,
    slot: &mut Option<PendingRevert>,
    undo: PendingRevert,
) {
    if let Err(e) = revert_control_writes(cs, &undo).await {
        tracing::error!(
            error = %e,
            "ha helper adoption: the control rows this pass wrote could not be restored; \
             nothing is planned until they are"
        );
        *slot = Some(undo);
    }
}

/// One tick of the adoption pass. Returns true when the caller should disarm
/// it: either it committed everything, or there was never anything to do. A
/// commit that left something deferred returns false, so the pass stays armed
/// and adopts that entity once Home Assistant can answer for it.
///
/// Three orderings inside the commit are fixed and load-bearing.
///
/// The CONTROL STORE is read under the config write guard, not at the top of
/// the tick, and a failed read defers the whole pass. That state decides two
/// irreversible things: whether LocalSky's own answer outranks the helper, and
/// whether the read retires. `IrrigationControlStore::get` resolves an error to
/// the all-default state, which here reads as "the operator never set
/// anything", so a transient SQLite failure would overwrite a live native pause
/// with a legacy helper. `try_get` reports the failure instead, and a genuinely
/// missing singleton row stays a confident default.
///
/// The answer set is RE-READ under the config write guard, immediately before
/// planning, and the plan is built from that fresh answer. The tick's own
/// readout was taken before the snapshot build, the controller overlay and
/// this function's own wait on the write guard, all of which are time in
/// which a control write can land in Home Assistant. Planning from the stale
/// answer would write the pre-write value into SQLite and then retire the
/// read: the owner taps Rain delay, gets a 200, and the pause is gone. The
/// re-read has to match the answer that earned stability, and
/// `ha_adopt::write_seq` has to be unmoved, which covers a write that was
/// still in flight when the re-read happened.
///
/// SQLite first, DURABLY, and the config marker last. A crash between them
/// leaves the control values written and nothing marked, so the next tick
/// plans again from the helpers as they stand then and marks. Marking first
/// and crashing would retire a read whose value never landed, which for the
/// vacation pause means watering a yard somebody paused before they left.
/// A FAILURE (rather than a crash) after the control writes puts them back
/// before returning: left in place, a written is_paused=1 would be read on
/// the retry as LocalSky's own answer, recorded kept_local, and the read
/// retired on a value the helper may no longer hold. A crash cannot do that
/// restore, so that one window is still documented as a redo.
/// The control connection runs WAL with `synchronous=NORMAL` and does not
/// fsync on commit, while the config marker is fsynced immediately, so
/// `flush_durable` is what makes the intended ordering the one that survives
/// a power cut.
async fn adopt_tick(st: &mut AdoptState, readout: &HelperReadout) -> bool {
    // A previous attempt wrote control values, failed before it could record
    // them, and could not put them back either. Until they are back the store
    // holds this pass's own residue, which the planner would read as an
    // operator answer, so nothing is planned before the restore succeeds.
    if let Some(pending) = st.pending_revert.take() {
        if let Some(cs) = st.control_store.as_ref() {
            if let Err(e) = revert_control_writes(cs, &pending).await {
                tracing::error!(
                    error = %e,
                    "ha helper adoption: control rows written by a failed pass are still not \
                     restored; retrying before anything is planned"
                );
                st.pending_revert = Some(pending);
                return false;
            }
            tracing::info!("ha helper adoption: control rows written by a failed pass restored");
        }
    }
    // A Home Assistant that answered with nothing is not a Home Assistant we
    // can conclude anything from.
    if readout.total_entities == 0 {
        st.fingerprint = None;
        st.stable_ticks = 0;
        return false;
    }
    let fp = readout.stability_key();
    if st.fingerprint.as_deref() == Some(fp.as_str()) {
        st.stable_ticks = st.stable_ticks.saturating_add(1);
    } else {
        st.fingerprint = Some(fp);
        st.stable_ticks = 1;
    }
    // A missing control helper is held to the long window. See
    // ADOPT_STABLE_TICKS_ABSENT: absence is the only shape where retiring a
    // read moves a protected gate onto a column no human wrote, and it is
    // what a Home Assistant that has not finished starting answers with.
    let control_absent = crate::ha_adopt::CONTROL_ENTITIES
        .iter()
        .any(|id| !readout.helpers.contains_key(*id));
    let needed = if control_absent {
        ADOPT_STABLE_TICKS_ABSENT
    } else {
        ADOPT_STABLE_TICKS
    };
    if st.stable_ticks < needed {
        return false;
    }
    // A deferral below re-earns stability rather than retrying every tick:
    // whatever blocked the commit (an unreadable config, a failed write) is
    // not going to clear inside ten seconds, and re-reading the answer set
    // before trying again is the same evidence rule as the first attempt.
    st.stable_ticks = 0;

    // Hold the read-modify-write guard across the whole sequence, the same
    // way PUT /api/config and the tuning apply do, and re-plan against the
    // config as loaded under it: another writer may have committed since the
    // tick began.
    let _guard = st.cfg_store.begin_write().await;
    let mut cfg = match crate::ports::config_store::ConfigStore::load(&*st.cfg_store).await {
        Ok(c) => c,
        Err(e) => {
            if matches!(e, crate::ports::config_store::ConfigStoreError::NotFound) {
                // No config file. On an install zoned by LOCALSKY_ZONES alone
                // that is not a passing state: there is nowhere to record the
                // pass, so every helper read stays live for as long as the
                // file is missing, and the release notes' "deleting the
                // helpers is safe" is false here. Say so once in the log and
                // carry the fact onto the snapshot for the notice. The pass
                // runs on its own once a config exists; the setup wizard
                // writes one.
                st.awaiting_config = true;
                if !st.no_config_warned {
                    st.no_config_warned = true;
                    tracing::warn!(
                        "home assistant helper adoption cannot run: no localsky.toml to record \
                         it in, so all seven helper reads stay live until one exists"
                    );
                }
            } else {
                // An unreadable config. Nothing to adopt into; try again on a
                // later tick.
                tracing::debug!(error = %e, "ha helper adoption deferred: config unavailable");
            }
            return false;
        }
    };
    st.awaiting_config = false;
    // Read the control surface HERE, under the guard, rather than taking the
    // tick's opening snapshot: the override panel and the action handler write
    // this store directly, and between the tick's read and this point sit the
    // /api/states call, the snapshot build, the controller overlay and the wait
    // on the write guard.
    //
    // A failed read DEFERS the whole pass. `get` resolves an error to the
    // all-default state, which here is indistinguishable from "the operator
    // never set anything" and is the input to two irreversible decisions: the
    // helper wins over LocalSky's own answer, and the read retires for good. A
    // transient SQLite error must not be able to say that.
    let control = match st.control_store.as_ref() {
        Some(cs) => match cs.try_get().await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "ha helper adoption deferred: the control store could not be read"
                );
                st.fingerprint = None;
                return false;
            }
        },
        None => None,
    };
    let control = control.as_ref();
    let now = chrono::Utc::now().timestamp();
    // Plan against the tick's answer first, only to find out whether there is
    // anything left to commit. An empty plan needs no round trip: it either
    // disarms the pass, or, when a control is present and holding nothing
    // usable, leaves it armed at no cost.
    let dry = crate::ha_adopt::plan(&readout.helpers, &cfg, control, now);
    if dry.is_empty() {
        return !dry.deferred;
    }

    // Re-read Home Assistant here, under the guard, and commit only from an
    // answer that still matches the one that earned stability.
    let Some(fresh) = st.helpers.read().await else {
        tracing::debug!("ha helper adoption deferred: the answer set could not be re-read");
        st.fingerprint = None;
        return false;
    };
    if fresh.total_entities == 0
        || st.fingerprint.as_deref() != Some(fresh.stability_key().as_str())
    {
        tracing::debug!("ha helper adoption deferred: the answer set moved during the tick");
        st.fingerprint = None;
        return false;
    }
    if crate::ha_adopt::write_seq() != readout.write_seq {
        // A control write went to a helper this pass adopts, after the tick's
        // answer was taken. Planning from either answer risks overwriting it
        // with the value it replaced and then retiring the read, so re-earn
        // the evidence: the next window reads the post-write answer and
        // adopts that.
        tracing::debug!("ha helper adoption deferred: a control write landed during the tick");
        st.fingerprint = None;
        return false;
    }
    let plan = crate::ha_adopt::plan(&fresh.helpers, &cfg, control, now);
    if plan.is_empty() {
        return !plan.deferred;
    }
    let mut revert: Option<PendingRevert> = None;
    if plan.writes_controls() {
        let Some(cs) = st.control_store.as_ref() else {
            // Unreachable: the planner only plans control writes when it was
            // given a control state, which only exists with a store. Refuse
            // rather than mark a control whose value went nowhere.
            tracing::error!("ha helper adoption: control writes planned with no store; skipping");
            return true;
        };
        let Some(before) = control else {
            // Same argument: a plan writes controls only when planned against
            // a control state.
            tracing::error!(
                "ha helper adoption: control writes planned with no control state; skipping"
            );
            return true;
        };
        let today = crate::timeutil::now_local().date_naive().to_string();
        // What to put back if a later step fails, marked as each write lands
        // so a write that never happened is never undone.
        let mut undo = PendingRevert::new(before, &today);
        let mut failed: Option<String> = None;
        if let Some(v) = plan.pause_until_epoch {
            match cs.set_pause_until(v).await {
                Ok(()) => undo.pause_until = true,
                Err(e) => failed = Some(e.to_string()),
            }
        }
        if failed.is_none() {
            if let Some(v) = plan.override_tomorrow.clone() {
                // Stamped with today: the helper carries no day of its own,
                // and an unstamped one-day override never expires.
                match cs.set_override_tomorrow_on(v, today).await {
                    Ok(()) => undo.override_tomorrow = true,
                    Err(e) => failed = Some(e.to_string()),
                }
            }
        }
        if failed.is_none() {
            if let Some(v) = plan.is_paused {
                match cs.set_paused(v).await {
                    Ok(()) => undo.is_paused = true,
                    Err(e) => failed = Some(e.to_string()),
                }
            }
        }
        if failed.is_none() {
            if let Some(v) = plan.is_dry_run {
                match cs.set_dry_run(v).await {
                    Ok(()) => undo.is_dry_run = true,
                    Err(e) => failed = Some(e.to_string()),
                }
            }
        }
        if failed.is_none() {
            // Nothing above is durable yet, and the marker below is fsynced.
            // Flush here so a power cut cannot leave a retired read pointing
            // at a column that never got the value.
            if let Err(e) = cs.flush_durable().await {
                failed = Some(e.to_string());
            }
        }
        if let Some(e) = failed {
            // Nothing is marked, so every read stays exactly where it was.
            // Whatever did land is put back now, so the next attempt plans
            // from the operator's state and not from this pass's residue.
            tracing::warn!(error = %e, "ha helper adoption deferred: control write failed");
            revert_or_park(cs, &mut st.pending_revert, undo).await;
            return false;
        }
        revert = Some(undo);
    }

    crate::ha_adopt::apply(&plan, &mut cfg);
    if let Err(e) = crate::ports::config_store::ConfigStore::save(&*st.cfg_store, &cfg).await {
        // The control values landed and the marker did not. Same rule as a
        // failed write: put them back, or the retry reads them as an operator
        // answer, records kept_local, and retires the read on a value the
        // helper may have moved off in the meantime.
        tracing::warn!(error = %e, "ha helper adoption deferred: config save failed");
        if let (Some(cs), Some(undo)) = (st.control_store.as_ref(), revert) {
            revert_or_park(cs, &mut st.pending_revert, undo).await;
        }
        return false;
    }
    // Swap the rebuilt policy in so the adopted thresholds AND the read
    // cutover take effect without waiting for a restart. Same one-line effect
    // the config hot-reload path applies.
    st.policy.store(Arc::new(WateringPolicy::from_config(&cfg)));
    if plan
        .records
        .iter()
        .all(|r| r.outcome == crate::ha_adopt::OUTCOME_NOT_FOUND)
    {
        tracing::warn!(
            count = plan.records.len(),
            total_entities = fresh.total_entities,
            "home assistant helper adoption: none of these helpers were present; \
             LocalSky owns these controls itself now"
        );
    }
    for r in &plan.records {
        tracing::info!(
            entity = %r.entity,
            outcome = %r.outcome,
            target = %r.target,
            adopted = ?r.adopted_value,
            observed = ?r.observed_value,
            previous = ?r.previous_value,
            "home assistant helper retired"
        );
    }
    // A control that deferred is still unhandled, so the pass stays armed and
    // picks it up once Home Assistant can answer for it.
    !plan.deferred
}

/// Let a controller that actually reports outrank the Home Assistant entity
/// readbacks, per field and per zone. Unlike the native path this never
/// overwrites with an absence: a field no controller reports keeps whatever
/// the entity said, so a legacy install with no controller configured is
/// untouched.
async fn overlay_reporting_controllers(
    snap: &mut IrrigationSnapshot,
    controllers: &ControllerRegistry,
) {
    if controllers.ids().is_empty() {
        return;
    }
    let cs = native_controller_state(controllers).await;
    for z in snap.zones.iter_mut() {
        if let Some((running, known)) = cs.running.get(&z.slug) {
            z.running = *running;
            z.running_known = *known;
        }
    }
    if let Some(m) = cs.master {
        snap.master_enable = m;
    }
    if cs.water.is_some() {
        snap.water_level_pct = cs.water;
    }
    snap.water_level_capable |= cs.water_level_capable || cs.water.is_some();
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
    // Pre-computed balance evidence (observed rain, bias model, per-zone
    // run history), gathered once per tick by the async caller so the
    // sync allocator never touches SQLite. `None` in tests / before the
    // first tick: the balance degrades to target-only sizing.
    balance: Option<&BalanceTick>,
    // Native control surface. `Some` whenever a persistence DB is mounted,
    // on BOTH deployment paths now: the Home Assistant path used to pass
    // `None` unconditionally, which made the native store unreachable there
    // even when it held a value, and hardcoded the sticky global override to
    // "auto" so an override written from the Override panel was never read.
    // The sequence wall-time estimate's per-zone cycle/soak lookups read
    // watering_policy.zone_agronomy (hot-reloaded), not a boot config.
    control: Option<&crate::persistence::IrrigationControlState>,
    // Whether this build may read Home Assistant helper entities at all.
    // True on the HA path, where a helper the adoption pass has not handled
    // yet is still read exactly as before; false on the native path, where
    // the map is empty by construction and the control surface is the only
    // source. Combined with `watering_policy.ha_read_retired` per entity.
    ha_helper_reads: bool,
) -> IrrigationSnapshot {
    // Per-entity read gate: live only while this build reads helpers at all
    // AND the adoption pass has not recorded that entity.
    let helper_live = |entity: &str| ha_helper_reads && !watering_policy.ha_read_retired(entity);
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
    // the two compute sites further down. Configured-timezone clock: hour
    // windows and odd/even parity are regulatory LOCAL rules, and the
    // container clock (UTC on the common Docker setup) evaluated them
    // against the wrong wall time.
    let now_local = crate::timeutil::now_local();
    let restriction_verdict = crate::engine::restrictions::evaluate(
        now_local,
        &watering_policy.restrictions,
        watering_policy.address_parity,
    );
    let restriction_cap_seconds: Option<u32> = restriction_verdict
        .max_minutes_cap
        .map(|m| m.saturating_mul(60));
    // Today's weekday (Sun=0..Sat=6 per chrono::Weekday::num_days_from_sunday)
    // for per-zone manual-override gating below, in the CONFIGURED timezone: a
    // UTC container flips the weekday at evening local time, which shifted the
    // override day for any tz west of UTC.
    let today_weekday: u8 = {
        use chrono::Datelike;
        crate::timeutil::now_local()
            .weekday()
            .num_days_from_sunday() as u8
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
    // None when the entity is missing/unavailable: the old unwrap_or(0.0)
    // published "Water level 0%" (reads as watering fully suppressed) for a
    // sensor that simply does not exist. On this path a present entity IS the
    // capability signal; the HA refresh loop then LATCHES capability across
    // ticks (see spawn_refresher) so a transient unavailable read cannot
    // retract the manifest descriptor.
    snap.water_level_pct = state_f64(&map, &format!("sensor.{sp}_water_level"));
    snap.water_level_capable = snap.water_level_pct.is_some();

    // Vacation pause + one-day override. LocalSky's own store owns both once
    // the adoption pass has handled the matching helper; until then, on a
    // Home Assistant deployment, they still come from the entity map exactly
    // as before. The store is consulted only when it exists: if the
    // persistence DB is gone the entity read stays live rather than the pause
    // silently reading zero.
    let pause_from_store = control.filter(|_| !helper_live(crate::ha_adopt::PAUSE_UNTIL));
    let override_from_store = control.filter(|_| !helper_live(crate::ha_adopt::OVERRIDE_TOMORROW));
    snap.pause_until_epoch = match pause_from_store {
        Some(c) => c.pause_until_epoch,
        None => map
            .get(crate::ha_adopt::PAUSE_UNTIL)
            .and_then(crate::ha_adopt::timestamp_attr)
            .unwrap_or(0),
    };
    snap.override_tomorrow = match override_from_store {
        // Already expired against the local date by the store, so a one-day
        // override cannot outlive the day it was set on.
        Some(c) => c.override_tomorrow.clone(),
        None => map
            .get(crate::ha_adopt::OVERRIDE_TOMORROW)
            .and_then(|s| s.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_string(),
    };
    // The sticky global and per-zone overrides are NOT part of this release.
    //
    // They have no Home Assistant helper and never had one: `POST /action`
    // writes them to LocalSky's own sqlite on every source, while this
    // builder has always reported "auto" on the Home Assistant path, so a
    // Skip or Force set from the Override panel there is stored and never
    // acted on. That is a real defect and it is PRE-EXISTING: it has nothing
    // to do with retiring helper reads, and fixing it here would activate,
    // for the first time and with no notice, an instruction the panel showed
    // as Auto for the whole time it was set. A stale Force is the first rung
    // of the ladder: it runs every zone past rain, freeze, wind and the
    // vacation pause.
    //
    // So the gate is the deployment path, exactly as before: the native path
    // reads the store, the Home Assistant path reads "auto". This release
    // changes nothing about these two controls. The defect is fixed on its
    // own, where the change can be the only thing in the release notes.
    let sticky_from_store = control.filter(|_| !ha_helper_reads);
    snap.global_override = sticky_from_store
        .map(|c| c.global_override.clone())
        .unwrap_or_else(|| "auto".to_string());
    // True when the pause and override controls will land somewhere. Once
    // they are adopted that is always, because they write LocalSky's own
    // store; before adoption it still reports whether the helpers exist, so
    // an install mid-migration is described accurately.
    snap.override_helpers_present = match (pause_from_store, override_from_store) {
        (Some(_), Some(_)) => true,
        _ => {
            map.contains_key(crate::ha_adopt::PAUSE_UNTIL)
                && map.contains_key(crate::ha_adopt::OVERRIDE_TOMORROW)
        }
    };
    // The migration record, for the notice. Empty on every standalone install
    // and on a Home Assistant install before the pass runs.
    snap.ha_adoption = watering_policy.ha_adoption.clone();
    // Whether the four controls have a sink at all. Same condition the
    // planner gets: a control state exists only where a control store does.
    // The notice cannot work this out from the records, because a control
    // that DEFERRED (present, holding unavailable) is missing from them for a
    // completely different reason.
    snap.controls_persisted = control.is_some();

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

    // Per-zone state. Every number here is LocalSky's own: no zone field
    // is read from a Home Assistant entity, on either deployment path.
    // The soil deficit had exactly one producer, the Smart Irrigation
    // entity, and it is gone: `bucket_mm` is now absent rather than a
    // fabricated 0.0, and today's run length comes from the weekly-budget
    // allocator (applied below, once `water_budgets` exists) on both
    // paths. heat_mult is the global forecast multiplier; capture_eff is
    // the constant the soil projection uses.
    let today_doy = {
        use chrono::Datelike;
        crate::timeutil::now_local().date_naive().ordinal() as u16
    };
    let site_lat = watering_policy.location.0;
    snap.zones = zones
        .iter()
        .map(|zone| {
            let slug = zone.slug.as_str();
            let running_id = format!("binary_sensor.{sp}_{slug}_station_running");
            // No native model computes a soil deficit today. Absent, not
            // zero: a bare 0.0 read as "at field capacity" on every
            // install that could never populate it.
            let bucket_mm: Option<f64> = None;
            // Kc from the native species catalog for the zone's configured
            // species, hemisphere aware. Previously the Smart Irrigation
            // entity's `multiplier` attribute, defaulting to 1.0 whenever
            // the entity was absent (which is always, on a standalone
            // install).
            // A zone with no agronomy config (unconfigured install) keeps
            // the neutral 1.0 the entity read used to fall back to.
            let kc = watering_policy
                .zone_agronomy
                .get(slug)
                .map(|a| crate::engine::kc_at_doy_lat(a.species, today_doy, site_lat))
                .unwrap_or(1.0);
            // Throughput + max-duration resolve from LocalSky's config
            // (localsky.toml zone block -> sprinkler_catalog default by
            // sprinkler_type, or precip_rate_mm_hr override when measured).
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
            // Today's run length comes from the weekly-budget allocator,
            // the one model that governs dispatch. It is applied in
            // `apply_budget_plan` below, once `snap.water_budgets` exists;
            // `scheduled_seconds` and `cap_binding` are pre-plan
            // placeholders that it overwrites. `raw_seconds` was the Smart
            // Irrigation bucket formula and has no producer left, so it
            // stays 0 and nothing renders it.
            let raw_seconds = 0u32;
            // Which Override schedules suppress smart dispatch for this
            // zone, so the UI can say so instead of showing a silent zero.
            let smart_suppressed = crate::scheduler::manual::override_suppression(
                &watering_policy.manual_schedules,
                slug,
                today_weekday,
            );
            let planned = 0u32;
            let math = Some(crate::ha::snapshot::ZoneMath {
                bucket_mm,
                kc,
                throughput_mm_hr,
                heat_mult: zone_loop_heat_mult,
                capture_eff: 0.70, // matches compute_soil_forecasts CAPTURE_EFFICIENCY
                raw_seconds,
                max_duration_seconds: max_dur,
                scheduled_seconds: planned,
                cap_binding: false,
            });
            ZoneState {
                name: zone.display_name.clone(),
                slug: zone.slug.clone(),
                // Sticky per-zone override from the native control surface;
                // "auto" when unset, and on the Home Assistant path, where
                // this read is inert for the same reason the global one above
                // is.
                override_mode: sticky_from_store
                    .and_then(|c| c.zone_overrides.get(&zone.slug))
                    .cloned()
                    .unwrap_or_else(|| "auto".to_string()),
                hex: String::new(), // Populated in Phase 3 from device_registry if needed.
                running: state_eq(&map, &running_id, "on"),
                // HA path reads running from a binary_sensor, always a
                // trusted readback. Native may override to false.
                running_known: true,
                // No producer. Nothing summarizes per-zone valve-open
                // seconds since local midnight on either path, so this is
                // absent rather than a hardcoded 0.0 printed beside a hold
                // line naming the inches already applied this week.
                today_run_minutes: None,
                bucket_mm,
                smart_suppressed,
                planned_run_seconds: planned,
                // The latest completed watering event's end, from the
                // per-tick runs evidence. This is what makes the
                // balance's min-interval spacing real on live paths (it
                // was hardcoded 0 for two releases, so spacing never
                // fired outside demo).
                last_run_epoch: balance
                    .and_then(|b| b.per_zone.get(slug))
                    .map(|e| e.last_run_epoch)
                    .unwrap_or(0),
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
    snap.next_run_epoch = compute_next_run_epoch(watering_policy, &snap.zones);

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
    // bad/offline.
    // The PAST component is the MAX of the model's regional past_daily archive and
    // the station GAUGE's measured daily totals from forecast_observations: the
    // gauge's memory beats the regional model for hyperlocal convection (a pop-up
    // storm that soaked this yard but never showed in Open-Meteo's past_daily now
    // still carries into the next morning's hard skip), and on any non-Open-Meteo
    // forecast source (which ship an empty past_daily) the gauge is the ONLY past
    // record, so this is what keeps the backstop from degenerating to today-only.
    // Same gauge-beats-model rule already used for days_since_significant_rain.
    let window_days = watering_policy.skip_rules.rain_observed_window_days;
    let observed_past_gauge = match forecast_obs {
        Some(store) => store
            .observed_rain_last_n_days(window_days as i64)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "observed_rain_last_n_days query failed");
                0.0
            }),
        None => 0.0,
    };
    let rain_observed_recent = rain_today_used
        + fc.past_n_day_precip_in(window_days as usize)
            .max(observed_past_gauge);
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

    // Source-agnostic reference ET0 (mm): provider full-day forecast > Open-Meteo
    // HA sensor > native compute from the forecast > station accumulator; None
    // when nothing real resolved (see resolve_et0_today_mm). Display +
    // soil-projection only (the live decision bucket is HA-sourced).
    let et0_lat = watering_policy.location.0;
    let et0_base_doy = {
        use chrono::Datelike;
        // Day-of-year in the CONFIGURED timezone: the UTC ordinal is tomorrow's
        // from evening local time onward, skewing the Hargreaves solar term.
        crate::timeutil::now_local().ordinal() as u16
    };
    let et0_today_mm = resolve_et0_today_mm(tempest.et0_today, &map, &fc, et0_lat, et0_base_doy);

    let (temp_max_today, temp_min_today, humidity_mean_today) = resolve_today_range(&fc, &map);

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
                // Per-day values follow the same ladder as forecast_day_et0_mm
                // (provider daily ET0 in mm > native Hargreaves) so the 3-day
                // average agrees with the today/tomorrow tiles in method + units.
                let vals: Vec<f64> = (0..3)
                    .filter_map(|i| {
                        fc.daily.get(i).and_then(|d| {
                            if d.et0_in > 0.0 {
                                Some(d.et0_in * 25.4)
                            } else {
                                native_et0_mm(d, et0_lat, et0_base_doy + i as u16)
                            }
                        })
                    })
                    .collect();
                if vals.is_empty() {
                    0.0
                } else {
                    vals.iter().sum::<f64>() / vals.len() as f64
                }
            }),
        temp_max_today_f: temp_max_today,
        temp_min_today_f: temp_min_today,
        wind_max_today_mph: wind_max_today,
        wind_gust_today_mph: wind_gust_today,
        humidity_mean_today_pct: humidity_mean_today,

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
        // Extended model context (all 0 when the provider lacks the series).
        // ET spent stays MODEL-derived (full-day minus the remaining hourly
        // curve): the bus et0_today field's contract is the FULL-DAY figure
        // (no adapter or mapping declares accumulator semantics), so treating
        // a live-owned value as "spent" would charge a mapped full-day sensor
        // at dawn. A dedicated accumulator field can revisit this.
        eto_spent_today_mm: fc.eto_spent_today_mm(now_epoch),
        vpd_now_kpa: fc.vpd_now_and_max_today().0,
        vpd_max_today_kpa: fc.vpd_now_and_max_today().1,
        // First hour WITH a value (a non-OM owner's window can start in the
        // past, before graft coverage; see vpd_now_and_max_today).
        soil_temp_6cm_now_f: fc
            .hourly
            .iter()
            .map(|h| h.soil_temp_6cm_f)
            .find(|v| *v > 0.0)
            .unwrap_or(0.0),
        soil_moisture_3_9_now_vwc: fc
            .hourly
            .iter()
            .map(|h| h.soil_moisture_3_9_vwc)
            .find(|v| *v > 0.0)
            .unwrap_or(0.0),
        // Last NONZERO reading, not .last(): a non-OM owner's hourly window
        // can extend past the donor's 48h graft coverage, leaving trailing
        // zeros that would fake a dry-down to 0%.
        soil_moisture_3_9_in48h_vwc: fc
            .hourly
            .iter()
            .rev()
            .map(|h| h.soil_moisture_3_9_vwc)
            .find(|v| *v > 0.0)
            .unwrap_or(0.0),
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

    // Resolve each zone's live soil reading once; the engine inputs,
    // the probe-fault detector and the per-zone verdicts all consume the
    // same list. With no probe configured on any zone it is built from
    // the active zone list, so every zone still gets a verdict.
    let soil_zones_resolved = if watering_policy.soil_zones.is_empty() {
        build_legacy_soil_zones(&map, zones)
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

        // The three thresholds. Until the adoption pass records a helper it
        // still outranks Settings, exactly as it always did; afterwards
        // Settings is the only source and the two editors finally agree.
        max_wind_mph: helper_f64(&map, crate::ha_adopt::MAX_WIND, &helper_live)
            .unwrap_or(watering_policy.skip_rules.max_wind_mph),
        min_temp_f: helper_f64(&map, crate::ha_adopt::MIN_TEMP, &helper_live)
            .unwrap_or(watering_policy.skip_rules.min_temp_f),
        rain_skip_in: helper_f64(&map, crate::ha_adopt::RAIN_SKIP, &helper_live)
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

        // The two toggles. Both had no native column at all before M0017, so
        // on a standalone install they were permanently false; both are
        // PROTECTED gates in the ladder, which made them controls that did
        // not exist. Post-adoption they read LocalSky's own store on both
        // paths.
        is_paused: match control.filter(|_| !helper_live(crate::ha_adopt::PAUSE_TOGGLE)) {
            Some(c) => c.is_paused,
            None => state_eq(&map, crate::ha_adopt::PAUSE_TOGGLE, "on"),
        },
        is_dry_run: match control.filter(|_| !helper_live(crate::ha_adopt::DRY_RUN_TOGGLE)) {
            Some(c) => c.is_dry_run,
            None => state_eq(&map, crate::ha_adopt::DRY_RUN_TOGGLE, "on"),
        },

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
        // The advisory projection needs SOME daily ET to draw a curve; when
        // nothing real resolved it opts into the engine-internal constant.
        // The published eto_today_mm stays None in that case.
        et0_today_mm.unwrap_or(ENGINE_ET0_FALLBACK_MM),
    )
    .await;
    snap.water_budgets = compute_water_budgets(
        &fc,
        zone_runtime,
        watering_policy.defer_threshold_in(),
        restriction_cap_seconds,
        &budget_zones_for_active(zones, &watering_policy.budget_zones),
        balance,
    );
    // ONE model governs dispatch on BOTH paths now: the weekly-budget
    // allocator. The Home Assistant path used to size runs from a Smart
    // Irrigation entity's bucket instead, which is the read this release
    // deleted, so both paths plan from `water_budgets` here.
    apply_budget_plan(&mut snap, watering_policy);

    snap
}

/// Size every zone's run from the weekly-budget allocator's `today_seconds`,
/// then apply, in order: the seasonal trust dial (re-clamped to the cap,
/// because a >100% dial can push a capped figure back over the ceiling), an
/// Override manual schedule for today (zeroes the smart dispatch so it does
/// not run on top of the operator's own run), and the force-run floor.
/// Recomputes the snapshot's next-run rollups from the result so display and
/// dispatch cannot disagree.
fn apply_budget_plan(snap: &mut IrrigationSnapshot, watering_policy: &WateringPolicy) {
    let planned_by_slug: HashMap<String, u32> = snap
        .water_budgets
        .iter()
        .map(|b| (b.zone_slug.clone(), b.today_seconds))
        .collect();
    // The allocator is the only thing that computes a real cap collision
    // now that the soil-deficit formula is gone, so the math panel's cap
    // row reads `session_capped` off the budget row instead of a flag
    // nothing sets. Without this the "shorted by the safety ceiling"
    // signal was false on every install while the panel still promised it.
    let capped_by_slug: HashMap<String, bool> = snap
        .water_budgets
        .iter()
        .map(|b| (b.zone_slug.clone(), b.session_capped))
        .collect();
    let today_weekday: u8 = {
        use chrono::Datelike;
        // Configured timezone, not the container's.
        crate::timeutil::now_local()
            .weekday()
            .num_days_from_sunday() as u8
    };
    // Read before the mutable zone loop borrows snap. The snapshot value is
    // the gated one, so re-deriving it from `control` here would put the
    // sticky override back in force on the Home Assistant path behind the
    // gate above.
    let global_ov = snap.global_override.clone();
    for z in snap.zones.iter_mut() {
        let raw_budget = planned_by_slug.get(&z.slug).copied().unwrap_or(0);
        let max_dur = z.math.as_ref().map(|m| m.max_duration_seconds).unwrap_or(0);
        let seasonal_binds =
            seasonal_cap_binds(raw_budget, watering_policy.seasonal_adjust_pct, max_dur);
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
            // P1-9: a force-run against a zero budget still waters a
            // bounded default instead of silently dispatching nothing.
            force_run_floor(&z.override_mode, &global_ov, budget_seconds, max_dur)
        };
        if let Some(m) = z.math.as_mut() {
            m.scheduled_seconds = z.planned_run_seconds;
            // The ceiling binds only when there IS a run and that run sits ON
            // the ceiling because something wanted more: the allocator's ideal
            // weekly session (`session_capped`), or the seasonal dial scaling
            // past it (`seasonal_binds`). Both clamps report themselves here;
            // the third, the condition-rule multiplier, reports itself in
            // `apply_verdict_multiplier` with the same predicate.
            //
            // The `planned == max_dur` term is what keeps the panel from
            // describing a run that does not exist. `session_capped` is a
            // property of the IDEAL weekly slice and stays true when today's
            // plan is zero for an unrelated reason: spacing since the last
            // session, a rain defer, budget mode off, an Override schedule.
            // Reading it alone printed "0 min (capped at 60 min)". It also
            // keeps a force-run floor honest: 5 minutes over a zero budget is
            // a floor, not a run the ceiling shortened.
            m.cap_binding = !override_active
                && max_dur > 0
                && z.planned_run_seconds == max_dur
                && (capped_by_slug.get(&z.slug).copied().unwrap_or(false) || seasonal_binds);
        }
    }
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;
    snap.next_run_epoch = compute_next_run_epoch(watering_policy, &snap.zones);
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
    balance: Option<&BalanceTick>,
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
        balance,
        control,
        // No helper reads on this path: the map is empty by construction, so
        // a live gate would resolve every control to its absent-entity
        // default instead of to the store.
        false,
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
    let cs = native_controller_state(controllers).await;
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
    snap.flow_gpm = cs.flow_gpm;

    // A5: run-times come from LocalSky's own weekly-budget allocator, applied
    // inside build_from_map (`apply_budget_plan`) for both deployment paths.
    // Custom-rule watering multiplier (AdjustMultiplier), applied after that
    // plan is final, so display + dispatch agree. No-op when no zone carries
    // such a rule.
    apply_verdict_multiplier(&mut snap);
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;
    snap.next_run_epoch = compute_next_run_epoch(watering_policy, &snap.zones);

    // A6: pause / override come from `control` (threaded into build_from_map
    // above); thresholds come from cfg.engine.skip_rules via watering_policy.
    snap
}

/// Query every configured controller once for live state and merge it:
/// per-zone running (by slug), plus the first reported master-enable +
/// water-level. Errors are swallowed (best-effort, never fails a refresh).
async fn native_controller_state(controllers: &ControllerRegistry) -> NativeControllerState {
    let mut running: HashMap<String, (bool, bool)> = HashMap::new();
    let mut master: Option<bool> = None;
    let mut water: Option<f64> = None;
    let mut flow_gpm: Option<f64> = None;
    let mut flow_meter = false;
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
                for z in st.zone_states {
                    running.insert(z.slug, (z.running, z.running_known));
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
        water_level_capable,
    }
}

/// Merged live readback from all configured controllers, gathered once per
/// native refresh. Best-effort: a controller that can't report contributes
/// nothing rather than failing the refresh.
struct NativeControllerState {
    /// slug -> (running, running_known). running_known=false means the
    /// adapter carried its last known value forward this poll.
    running: HashMap<String, (bool, bool)>,
    master: Option<bool>,
    water: Option<f64>,
    flow_gpm: Option<f64>,
    flow_meter: bool,
    /// Any configured controller declares `ControllerCaps.water_level`.
    water_level_capable: bool,
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

/// Weekly water-balance assembly. Resolves each zone's target and
/// runtime inputs (LocalSky config -> agronomic slug default), joins the
/// pre-computed per-tick balance evidence, and calls the ONE pure
/// implementation (`engine::budget::compute_zone`) per zone.
///
/// Outputs `today_seconds` per zone: the run length that actually
/// dispatches. Zero means "don't run this zone today"; the reason names
/// what decided.
fn compute_water_budgets(
    fc: &ForecastSnapshot,
    zone_runtime: &HashMap<String, ZoneRuntime>,
    // Live rain-defer threshold from cfg.engine.session_rain_defer_in.
    session_rain_defer_in: f64,
    restriction_cap_seconds: Option<u32>,
    // Per-zone budget rows. The live call site passes
    // `budget_zones_for_active`, which is one row per ACTIVE zone
    // (config-backed where the operator wrote one, otherwise a row with
    // no explicit target that resolves the agronomic slug default below).
    // Empty = no zones at all -> nothing to plan until the wizard runs.
    budget_zones: &[ZoneBudgetCfg],
    // Pre-computed store evidence; `None` degrades to target-only sizing.
    balance: Option<&BalanceTick>,
) -> Vec<WaterBudget> {
    let now_epoch = chrono::Utc::now().timestamp();
    let globals = crate::engine::BalanceGlobals {
        now_epoch,
        session_rain_defer_in,
        observed_rain_mm: balance.map(|b| b.observed_rain_mm).unwrap_or(0.0),
        observed_rain_source: balance
            .map(|b| b.observed_rain_source.clone())
            .unwrap_or_else(|| "none".to_string()),
        bias: balance
            .map(|b| b.bias.clone())
            .unwrap_or_else(crate::engine::BiasModel::identity),
    };

    let mut out = Vec::with_capacity(budget_zones.len());
    for zone_cfg in budget_zones.iter() {
        let slug = zone_cfg.slug.as_str();
        let (default_budget_in, default_sessions) = agronomic_budget_default(slug);
        // Precedence: per-zone config value -> agronomic slug default.
        // Home Assistant `input_number` helpers used to win over both. They
        // no longer participate: LocalSky is the engine and reads no entity
        // to make a decision, so the weekly target and session count come
        // from LocalSky's own config on every deployment path.
        let weekly_budget_in = zone_cfg.weekly_budget_in.unwrap_or(default_budget_in);
        let sessions_per_week = zone_cfg
            .sessions_per_week
            .unwrap_or(default_sessions)
            .max(1);
        // Whether this zone waters on a target the operator set or on one
        // inferred from its slug. The allocator decides dispatch on every
        // path now, so the Zones page names the inferred ones once rather
        // than letting a yard start watering on a guess with nothing on
        // screen.
        let target_inferred =
            zone_cfg.weekly_budget_in.is_none() || zone_cfg.sessions_per_week.is_none();
        // Budget mode used to be a per-zone HA toggle while the cutover was
        // in progress. LocalSky is the only source of truth, so it is on.
        let mode_active = true;

        // Throughput + max-duration come from LocalSky's zone config
        // (catalog default by sprinkler_type, optional precip_rate_mm_hr
        // override).
        let rt = zone_runtime
            .get(slug)
            .copied()
            .unwrap_or_else(ZoneRuntime::fallback);
        // Active watering restriction cap (if any) tightens the budget-path
        // ceiling too. Same min-of-two rule as the daily-bucket path above.
        let max_dur_s = match restriction_cap_seconds {
            Some(c) => rt.max_duration_s.min(c),
            None => rt.max_duration_s,
        };

        // Per-zone run evidence from the tick (empty = no runs on record).
        let evidence = balance
            .and_then(|b| b.per_zone.get(slug))
            .copied()
            .unwrap_or_default();
        let applied_trailing_mm = if rt.throughput_mm_hr > 0.0 {
            evidence.applied_open_s as f64 / 3600.0 * rt.throughput_mm_hr
        } else {
            0.0
        };

        let zone_inputs = crate::engine::ZoneBalanceInputs {
            slug: slug.to_string(),
            name: zone_cfg.name.clone(),
            weekly_budget_in,
            sessions_per_week,
            mode_active,
            throughput_mm_hr: rt.throughput_mm_hr,
            max_dur_s,
            last_run_epoch: evidence.last_run_epoch,
            applied_trailing_mm,
            sessions_done: evidence.sessions_done,
            target_inferred,
        };
        out.push(crate::engine::compute_zone_balance(
            &zone_inputs,
            &globals,
            fc,
        ));
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
    // install -> no soil forecasts until the wizard writes zones. Zones with
    // NO bound soil sensor are excluded outright: there is no probe to
    // project, and emitting an entry made every consumer present a phantom
    // "probe offline" for a zone whose probe was deliberately removed (the
    // Sensors rail listed all four removed probes as offline).
    let zones: Vec<ForecastZone> = zone_cfg
        .iter()
        .filter(|z| z.soil_sensor_id.is_some())
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
            let rain_effective_mm = d.precip_sum_in * 25.4 * d.precip_weight();
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
fn compute_next_run_epoch(
    policy: &WateringPolicy,
    zones: &[crate::ha::snapshot::ZoneState],
) -> i64 {
    use crate::engine::sunrise::smart_morning_target_start;

    let (lat, lon) = policy.location;
    if lat == 0.0 && lon == 0.0 {
        return 0;
    }
    // True wall time of the sequence (runs + soak gaps + preambles, interleave
    // aware), from the same planner the dispatcher's window math uses, so the
    // displayed next-run time and the actual dispatch agree. Soak/interleave
    // knobs AND the per-zone cycle agronomy come from the hot-reloaded policy
    // (live on the next tick after a config apply).
    let sequence_total_s = crate::scheduler::smart_morning::sequence_wall_seconds(
        &policy.zone_agronomy,
        zones,
        policy.soak_minutes,
        policy.interleave_cycles,
    );

    let now = crate::timeutil::now_local();
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

/// `state_f64` behind the adoption gate: `None` once the entity's read is
/// retired, so the caller's `unwrap_or(config)` becomes the only source.
fn helper_f64(
    map: &HashMap<String, Value>,
    eid: &str,
    live: &impl Fn(&str) -> bool,
) -> Option<f64> {
    if !live(eid) {
        return None;
    }
    state_f64(map, eid)
}

/// Native reference ET0 (mm/day) computed from a forecast day's temperature
/// range + latitude, via the (previously unwired) engine::et0 module. Hargreaves
/// only needs Tmax/Tmin + extraterrestrial radiation (lat + day-of-year), so this
/// works for ANY forecast source (not just the Open-Meteo HA REST sensor) and
/// replaces the flat 5.0 fallback. `None` when the day has no usable temps.
pub(crate) fn native_et0_mm(
    d: &crate::forecast::snapshot::DailyEntry,
    lat: f64,
    doy: u16,
) -> Option<f64> {
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

/// Today's forecast (temp max, temp min, representative humidity) as
/// Options, forecast-first (the resolve_et0_today_mm ladder shape): the live
/// forecast snapshot's daily[0], then the legacy Open-Meteo HA REST sensors,
/// `None` when neither carries the value. The old HA-sensor-only
/// unwrap_or(0.0) fabricated a 0°F/0°F range and 0% humidity on every
/// install without those sensors (all native installs), which the Day block
/// rendered as forecast data and the LLM advisor was prompted with as
/// ground truth.
fn resolve_today_range(
    fc: &ForecastSnapshot,
    map: &HashMap<String, Value>,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let today = fc.daily.first();
    // A day whose max AND min are both 0.0 carries no temps (the same
    // absent-day test native_et0_mm applies).
    let has_temps = today
        .map(|d| d.temp_max_f != 0.0 || d.temp_min_f != 0.0)
        .unwrap_or(false);
    let temp_max = today
        .filter(|_| has_temps)
        .map(|d| d.temp_max_f)
        .or_else(|| state_f64(map, "sensor.open_meteo_temp_max_today"));
    let temp_min = today
        .filter(|_| has_temps)
        .map(|d| d.temp_min_f)
        .or_else(|| state_f64(map, "sensor.open_meteo_temp_min_today"));
    // humidity_pct == 0 means "no hourly coverage for the day" (see
    // backfill_daily_humidity), so it does not count as a reading.
    let humidity = today
        .map(|d| d.humidity_pct)
        .filter(|h| *h > 0)
        .map(f64::from)
        .or_else(|| state_f64(map, "sensor.open_meteo_humidity_mean_today"));
    (temp_max, temp_min, humidity)
}

/// Engine-internal working assumption for daily reference ET0 (mm/day) when
/// no rung of `resolve_et0_today_mm` produced a value (forecast outage or
/// cold start). Consumed ONLY by the advisory soil projection so its curve
/// can still be drawn; it is never published. The snapshot's `eto_today_mm`
/// stays `None` (serialized null) so the HA sensor reads unknown and the
/// dashboard shows a dash instead of recording a fabricated 5.0 mm/day of
/// evapotranspiration into long-term statistics.
const ENGINE_ET0_FALLBACK_MM: f64 = 5.0;

/// Today's reference ET0 (mm), source-agnostic. The contract is the FULL-DAY
/// figure (snapshot doc: eto_today_mm):
///   1. the bus et0_today: a source that reports (or a user mapping that
///      feeds) the field directly owns it. The field's contract is FULL-DAY
///      mm; the built-in Open-Meteo fill emits today's converted daily total,
///      and an explicit HA-passthrough/MQTT mapping is honored as mapped.
///      (A station accumulator mapped here reads low early in the day; issue
///      #4's actual 25x collapse was the OM fill emitting inches, fixed at
///      the emit. Accumulators belong in a dedicated field if one is added.)
///   2. the forecast provider's own daily[0] ET0 (inches -> mm),
///   3. Open-Meteo's HA REST sensor (mm, legacy HA installs),
///   4. native Hargreaves from the forecast temps.
/// `None` when every rung comes up empty: the published field carries the
/// unknown honestly; a consumer that needs a working assumption opts into
/// `ENGINE_ET0_FALLBACK_MM` explicitly.
fn resolve_et0_today_mm(
    snapshot_et0: f64,
    map: &HashMap<String, Value>,
    fc: &ForecastSnapshot,
    lat: f64,
    doy: u16,
) -> Option<f64> {
    if snapshot_et0 > 0.0 {
        return Some(snapshot_et0);
    }
    if let Some(d) = fc.daily.first() {
        if d.et0_in > 0.0 {
            return Some(d.et0_in * 25.4);
        }
    }
    if let Some(v) = state_f64(map, "sensor.open_meteo_eto_today").filter(|v| *v > 0.0) {
        return Some(v);
    }
    fc.daily.first().and_then(|d| native_et0_mm(d, lat, doy))
}

/// ET0 (mm) for forecast day `idx` (0=today): the provider's own daily ET0
/// (inches -> mm) > the matching Open-Meteo HA REST sensor (mm, legacy HA
/// installs) > native Hargreaves > `fallback`. Mirrors the forecast-side ranks
/// of resolve_et0_today_mm, so today / tomorrow / 3-day agree in method and
/// units on every install class (the old native-only tomorrow disagreed with
/// the provider-fed today, which is what made the pair collapse at rollover).
fn forecast_day_et0_mm(
    map: &HashMap<String, Value>,
    sensor_key: &str,
    fc: &ForecastSnapshot,
    idx: usize,
    lat: f64,
    base_doy: u16,
    fallback: f64,
) -> f64 {
    if let Some(d) = fc.daily.get(idx) {
        if d.et0_in > 0.0 {
            return d.et0_in * 25.4;
        }
    }
    if let Some(v) = state_f64(map, sensor_key).filter(|v| *v > 0.0) {
        return v;
    }
    let Some(d) = fc.daily.get(idx) else {
        return fallback;
    };
    native_et0_mm(d, lat, base_doy + idx as u16).unwrap_or(fallback)
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

/// The forecast-observations writer's provenance tag for the day's
/// rain total, from the merge's rain-today owner. A live station owning
/// the daily total is a gauge; a cloud owner is classified by its
/// catalog nature (radar QPE day products vs model day totals); no
/// owner at all means the install has no rain-capable source and the
/// day records a 'none' placeholder.
fn classify_rain_today_source(owner: Option<&crate::tempest::state::RainOwner>) -> &'static str {
    match owner {
        None => "none",
        // A stale owner is a writer that went silent; its frozen value must
        // not keep fabricating wet days (the 3-tier rain gate applies the
        // same freshness rule to the rain-rate owner). The live tier only
        // ever returns fresh owners, so this arm covers the fill tier.
        Some(o) if !o.is_fresh => "none",
        Some(o) if o.is_live => "gauge",
        Some(o) => match cloud_rain_nature_for_label(&o.label) {
            Some(crate::ha::snapshot::RainNature::Measured) => "gauge",
            Some(crate::ha::snapshot::RainNature::RadarQpe) => "radar",
            _ => "model",
        },
    }
}

/// Physical ceiling on a plausible daily rain total (inches). Values above
/// it are garbage frames (a unit misparse, a scale/offset misconfig on an
/// MQTT or passthrough writer), and the day-max upsert would record them
/// permanently; mirror of the SOIL_PCT_PHYSICAL_MAX quality-gate pattern.
const RAIN_TODAY_PHYSICAL_MAX_IN: f64 = 15.0;

/// What the observations-ledger writer records this tick:
/// `Some((observed_in, source))` to upsert, `None` to skip the tick.
///
///   - gauge/radar owner (fresh) with a same-day accumulator and a
///     plausible value: the measured day total, with its provenance.
///   - model-nature owner: the 0.0/'none' placeholder. A model
///     RainTodayIn fill is the WHOLE day's forecast, including hours
///     that have not happened; recording it as observed would let
///     phantom rain persist in the day-max ledger for a full trailing
///     window. Model rain reaches the balance through the archive rung
///     and the defer gate instead.
///   - no owner, or a stale one: the 0.0/'none' placeholder.
///   - accumulator still on the previous local day (the first ticks
///     after configured-tz midnight, before the next observation resets
///     it): SKIP: a day-max write now would pin yesterday's total onto
///     the new day's row permanently.
///   - implausible value (non-finite, negative, above the physical
///     cap): SKIP with a warning naming the owner, leaving the day's
///     ledger untouched rather than pinning garbage.
fn ledger_observation(
    snapshot: &crate::tempest::state::Snapshot,
    owner: Option<&crate::tempest::state::RainOwner>,
    now_epoch: i64,
) -> Option<(f64, &'static str)> {
    let source = classify_rain_today_source(owner);
    if source != "gauge" && source != "radar" {
        return Some((0.0, "none"));
    }
    if snapshot.rain_today_day_ordinal != crate::timeutil::local_day_ordinal(now_epoch) {
        // Midnight carry gate: the accumulator has not rolled onto the new
        // local day yet.
        return None;
    }
    let v = snapshot.rain_in_today;
    if !v.is_finite() || !(0.0..=RAIN_TODAY_PHYSICAL_MAX_IN).contains(&v) {
        tracing::warn!(
            owner = owner.map(|o| o.label.as_str()).unwrap_or(""),
            value = v,
            cap_in = RAIN_TODAY_PHYSICAL_MAX_IN,
            "implausible daily rain total; observations-ledger write skipped"
        );
        return None;
    }
    Some((v, source))
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

/// The four zone slugs of the deployment this app grew out of. On the one
/// install shape that reaches `build_legacy_soil_zones`, these four, and
/// only these four, read `sensor.<slug>_soil_moisture` and
/// `input_number.irrigation_<slug>_saturation_pct` from Home Assistant.
/// That is exactly the set of reads the previous release made there, so an
/// upgrade adds no read to any other zone.
const LEGACY_SOIL_SLUGS: [&str; 4] = ["back_yard", "front_yard", "side_yard", "back_yard_shrubs"];

/// One soil entry per ACTIVE zone, for an install with no zones in
/// `localsky.toml`: zones resolved from `LOCALSKY_ZONES` with no config
/// file, or a config whose zones table is empty. An install with zones in
/// its config never reaches this: `resolve_soil_zones` already emits one
/// entry per configured zone, probe or no probe.
///
/// This list is not only about soil: `skip_rules::decide_per_zone`
/// iterates it, so a zone missing from here gets no per-zone verdict at
/// all. It used to be four hardcoded slugs inherited from the original
/// deployment this app grew out of, which on this install shape meant a
/// yard with seven zones of its own received verdicts for four zones it
/// did not have and none for the seven it did. Building from the active
/// zone list instead gives every real zone a verdict.
///
/// The two Home Assistant reads stay confined to `LEGACY_SOIL_SLUGS`. Every
/// other zone carries `pct: None` and the slug's default saturation, so a
/// third-party entity that happens to be named `sensor.<slug>_soil_moisture`
/// cannot start deciding a zone the morning after an upgrade. `pct: None`
/// reads as no probe, which the soil gates treat as inapplicable rather
/// than as a probe that went quiet.
fn build_legacy_soil_zones(
    map: &HashMap<String, Value>,
    zones: &[crate::zones::ZoneIdent],
) -> Vec<ZoneSoil> {
    // Saturation/target defaults that used to be per-slug literals. Beds
    // and shrubs hold water longer than turf, so they keep the higher
    // saturation ceiling and the lower dry floor.
    fn defaults(slug: &str) -> (f64, f64) {
        if slug.contains("shrub") || slug.contains("garden") || slug.contains("bed") {
            (85.0, 25.0)
        } else {
            (70.0, 30.0)
        }
    }
    zones
        .iter()
        .map(|z| {
            let (sat_default, target_min) = defaults(&z.slug);
            let slug = &z.slug;
            let legacy = LEGACY_SOIL_SLUGS.contains(&slug.as_str());
            ZoneSoil {
                slug: slug.clone(),
                name: z.display_name.clone(),
                // Offline guard: a raw 0% reading is a disconnected probe, not
                // bone-dry soil. Absent entirely means no probe on this zone.
                pct: if legacy {
                    apply_soil_quality(state_f64(map, &format!("sensor.{slug}_soil_moisture")))
                } else {
                    None
                },
                saturation_pct: if legacy {
                    state_f64(
                        map,
                        &format!("input_number.irrigation_{slug}_saturation_pct"),
                    )
                    .unwrap_or(sat_default)
                } else {
                    sat_default
                },
                target_min_pct: target_min,
            }
        })
        .collect()
}

#[cfg(test)]
mod legacy_soil_zone_tests {
    use super::*;

    fn zones(slugs: &[&str]) -> Vec<crate::zones::ZoneIdent> {
        crate::zones::from_pairs(slugs.iter().map(|s| (*s, *s)))
    }

    fn entity(state: &str) -> Value {
        serde_json::json!({ "state": state })
    }

    /// A zone that is not one of the four legacy names ignores a Home
    /// Assistant entity that happens to match the name pattern. The previous
    /// release made no read for that zone at all, and an upgrade must not
    /// hand a third-party sensor a vote over it. The legacy names keep the
    /// read they always had.
    #[test]
    fn a_non_legacy_zone_ignores_a_matching_soil_entity() {
        let mut map = HashMap::new();
        map.insert("sensor.orchard_soil_moisture".to_string(), entity("90"));
        map.insert(
            "input_number.irrigation_orchard_saturation_pct".to_string(),
            entity("10"),
        );
        map.insert("sensor.back_yard_soil_moisture".to_string(), entity("42"));
        map.insert(
            "input_number.irrigation_back_yard_saturation_pct".to_string(),
            entity("65"),
        );
        let out = build_legacy_soil_zones(&map, &zones(&["orchard", "back_yard"]));
        let orchard = out.iter().find(|z| z.slug == "orchard").unwrap();
        assert_eq!(
            orchard.pct, None,
            "no read for a zone the previous release never read"
        );
        assert_eq!(
            orchard.saturation_pct, 70.0,
            "the slug default, not the helper"
        );
        let back = out.iter().find(|z| z.slug == "back_yard").unwrap();
        assert_eq!(back.pct, Some(42.0), "a legacy name keeps its read");
        assert_eq!(back.saturation_pct, 65.0);
    }

    /// And every active zone still gets an entry, legacy or not, because the
    /// entry is what earns the zone a per-zone verdict.
    #[test]
    fn every_active_zone_gets_an_entry() {
        let out = build_legacy_soil_zones(&HashMap::new(), &zones(&["orchard", "herb_bed"]));
        let slugs: Vec<&str> = out.iter().map(|z| z.slug.as_str()).collect();
        assert_eq!(slugs, vec!["orchard", "herb_bed"]);
        assert!(out.iter().all(|z| z.pct.is_none()));
        assert_eq!(out[1].saturation_pct, 85.0, "bed default");
    }
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
    fn from_config_maps_the_zone_run_cap_and_budget_respects_restrictions() {
        // ZoneConfig.max_run_minutes lands on ZoneRuntime in seconds; unset
        // resolves to the historical 60 minutes. The budget allocator caps
        // per-session seconds with min(zone cap, restriction cap), so an
        // active restriction keeps winning over a raised zone cap.
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": 1.5,
                "sessions_per_week": 1
            }))
            .unwrap(),
        );
        let policy_default = WateringPolicy::from_config(&cfg);
        assert_eq!(
            policy_default
                .zone_runtime
                .get("front")
                .unwrap()
                .max_duration_s,
            3600,
            "unset cap maps to 60 minutes"
        );

        cfg.zones.get_mut("front").unwrap().max_run_minutes = Some(120);
        let policy_raised = WateringPolicy::from_config(&cfg);
        assert_eq!(
            policy_raised
                .zone_runtime
                .get("front")
                .unwrap()
                .max_duration_s,
            7200,
            "a configured cap maps minutes to seconds"
        );

        // 1.5 in over 1 session at 25.4 mm/hr measured throughput wants
        // (38.1 / 25.4) * 3600 = 5400 s per session (GROSS sizing, no
        // capture or heat factor): between the two caps, so the clamp
        // state flips with the configured value.
        let fc = crate::forecast::snapshot::ForecastSnapshot::default();
        let budget = |policy: &WateringPolicy, restriction: Option<u32>| {
            compute_water_budgets(
                &fc,
                &policy.zone_runtime,
                policy.defer_threshold_in(),
                restriction,
                &policy.budget_zones,
                None,
            )
            .remove(0)
        };
        let b0 = budget(&policy_default, None);
        assert_eq!(
            b0.seconds_per_session, 5400,
            "gross sizing: 38.1 mm / 25.4 mm/hr, nothing else"
        );
        assert!(
            b0.session_capped,
            "the 60 minute default clamps the session"
        );
        let b1 = budget(&policy_raised, None);
        assert!(
            !b1.session_capped,
            "the raised cap fits the session on the next build, no restart involved"
        );
        let b2 = budget(&policy_raised, Some(3600));
        assert!(
            b2.session_capped,
            "an active restriction cap still wins min() over the raised zone cap"
        );
    }

    fn one_zone_balance_policy(weekly_in: f64, sessions: u32) -> WateringPolicy {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": weekly_in,
                "sessions_per_week": sessions
            }))
            .unwrap(),
        );
        WateringPolicy::from_config(&cfg)
    }

    /// The pacing gate finally fires on live evidence: with the last
    /// completed watering event 1 day back and a 3-day interval (2
    /// sessions/week), today is spaced to zero. For two releases
    /// last_run_epoch was hardcoded 0 on live paths, so this gate never
    /// fired outside demo.
    #[test]
    fn spacing_gate_fires_from_run_history_evidence() {
        let policy = one_zone_balance_policy(1.5, 2);
        let fc = crate::forecast::snapshot::ForecastSnapshot::default();
        let now = chrono::Utc::now().timestamp();
        let mut per_zone = HashMap::new();
        per_zone.insert(
            "front".to_string(),
            ZoneRunEvidence {
                applied_open_s: 1800,
                sessions_done: 1,
                last_run_epoch: now - 86_400,
            },
        );
        let tick = BalanceTick {
            observed_rain_mm: 0.0,
            observed_rain_source: "none".into(),
            bias: crate::engine::BiasModel::identity(),
            per_zone,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
        )
        .remove(0);
        assert_eq!(b.today_seconds, 0, "spacing must gate today");
        assert!(
            b.today_reason.contains("spaced"),
            "reason names the spacing gate: {}",
            b.today_reason
        );
        assert_eq!(b.last_run_epoch, now - 86_400, "evidence rides the wire");
        assert_eq!(b.remaining_sessions, 1, "one of two sessions already done");
        // The applied credit shrank the remainder: 1.5 in target minus
        // 1800 s x 25.4 mm/hr = 12.7 mm applied leaves 25.4 mm for the
        // one remaining session.
        assert!(
            (b.needed_mm - 25.4).abs() < 1e-6,
            "got needed {}",
            b.needed_mm
        );
        assert!((b.applied_mm - 12.7).abs() < 1e-6, "got {}", b.applied_mm);
    }

    /// `engine.session_rain_defer_in` reaches the live balance. The knob
    /// was documented, editable, and dead: the assembly passed the
    /// compile-time constant, so an operator who raised it to unstick an
    /// install saw no change at all.
    #[test]
    fn configured_rain_defer_threshold_reaches_the_balance() {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": 1.0,
                "sessions_per_week": 2
            }))
            .unwrap(),
        );
        // 0.24" of certain rain over the next 24 hours.
        let now = chrono::Utc::now().timestamp();
        let mut fc = crate::forecast::snapshot::ForecastSnapshot::default();
        fc.hourly = (0..24)
            .map(|h| crate::forecast::snapshot::HourlyEntry {
                time_epoch: now + h * 3600,
                precip_in: 0.01,
                ..Default::default()
            })
            .collect();

        // Schema default (0.10"): the session defers.
        let policy = WateringPolicy::from_config(&cfg);
        assert_eq!(policy.session_rain_defer_in, 0.10);
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            None,
        )
        .remove(0);
        assert_eq!(b.today_seconds, 0);
        assert!(b.today_reason.contains("deferred"), "{}", b.today_reason);

        // Operator raises the knob past the forecast: the session runs.
        cfg.engine.session_rain_defer_in = 0.50;
        let policy = WateringPolicy::from_config(&cfg);
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            None,
        )
        .remove(0);
        assert!(b.today_seconds > 0, "{}", b.today_reason);
        assert!(b.today_reason.contains("session"), "{}", b.today_reason);
    }

    /// Observed rain plus prior watering covering the target sizes the
    /// week to zero with the covered reason (the owner's acceptance
    /// case: a soaked week must read covered, not schedule sessions).
    #[test]
    fn observed_rain_and_applied_water_cover_the_week() {
        let policy = one_zone_balance_policy(1.0, 2);
        let fc = crate::forecast::snapshot::ForecastSnapshot::default();
        let now = chrono::Utc::now().timestamp();
        let mut per_zone = HashMap::new();
        per_zone.insert(
            "front".to_string(),
            ZoneRunEvidence {
                applied_open_s: 900,
                sessions_done: 1,
                last_run_epoch: now - 5 * 86_400,
            },
        );
        let tick = BalanceTick {
            // 3.24" of rain on the ledger (the live acceptance figure).
            observed_rain_mm: 3.24 * 25.4,
            observed_rain_source: "gauge".into(),
            bias: crate::engine::BiasModel::identity(),
            per_zone,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
        )
        .remove(0);
        assert_eq!(b.today_seconds, 0);
        assert_eq!(b.seconds_per_session, 0, "the remainder is zero");
        assert!(
            b.today_reason.contains("covered"),
            "reason names the balance coverage: {}",
            b.today_reason
        );
        assert_eq!(b.observed_rain_source, "gauge");
        assert!((b.observed_rain_mm - 3.24 * 25.4).abs() < 1e-9);
    }

    /// Run-history evidence building: watering rows cluster (manual +
    /// observer overlap counts once), dry-run rows and skip markers are
    /// excluded, and the window clamp holds.
    #[test]
    fn zone_run_evidence_filters_clusters_and_clamps() {
        let now = 1_700_000_000i64;
        let w_start = now - 7 * 86_400;
        let row = |slug: &str, start: i64, dur: u32, source: &str, status: &str| {
            crate::persistence::RunRow {
                id: 0,
                zone_slug: slug.into(),
                start_epoch: start,
                end_epoch: Some(start + dur as i64),
                duration_s: Some(dur),
                source: source.into(),
                controller_id: "c".into(),
                status: status.into(),
                skip_reason: None,
                et0_mm: None,
                etc_mm: None,
                applied_mm: None,
                cycle_index: None,
                cycle_count: None,
            }
        };
        let rows = vec![
            // Two mornings for front: one manual+observer overlap pair,
            // one plain observer row.
            row("front", w_start + 10_000, 1200, "manual", "completed"),
            row("front", w_start + 10_010, 1200, "ha_refresher", "completed"),
            row("front", w_start + 300_000, 600, "ha_refresher", "completed"),
            // Pretend water and a skip marker: never evidence.
            row("front", w_start + 400_000, 900, "dry_run", "completed"),
            row("front", w_start + 500_000, 0, "smart_morning", "skipped"),
            // An event straddling the window start: only the inside part.
            row("side", w_start - 600, 1200, "ha_refresher", "completed"),
        ];
        let ev = build_zone_run_evidence(&rows, w_start, now);
        let front = ev.get("front").copied().unwrap();
        assert_eq!(front.sessions_done, 2, "two clustered events");
        assert_eq!(front.applied_open_s, 1210 + 600, "union, not the raw sum");
        assert_eq!(front.last_run_epoch, w_start + 300_600);
        let side = ev.get("side").copied().unwrap();
        assert_eq!(side.applied_open_s, 600, "window clamp");
    }

    /// The observed-rain ladder per install class: measured COVERAGE
    /// wins outright (never a value contest), legacy rows classify by
    /// install class, the model side is the max() of archive and
    /// model-quality legacy rows, and no evidence at all reads 'none'.
    #[test]
    fn observed_rain_ladder_resolves_per_install_class() {
        use crate::persistence::ObservedRainWindow;
        let win = |g: f64, r: f64, l: f64, gd: u32, rd: u32, ld: u32| ObservedRainWindow {
            gauge_in: g,
            radar_in: r,
            model_in: 0.0,
            legacy_in: l,
            gauge_days: gd,
            radar_days: rd,
            legacy_days: ld,
        };
        // Gauge install: measured rows win and read 'gauge'.
        let (mm, src) = resolve_observed_rain(&win(1.0, 0.0, 0.0, 5, 0, 0), true, 0.3);
        assert_eq!(src, "gauge");
        assert!((mm - 25.4).abs() < 1e-9);
        // A gauge that measured LESS than the regional archive still wins:
        // coverage precedence, the yard's own record is the truth.
        let (mm, src) = resolve_observed_rain(&win(0.1, 0.0, 0.0, 6, 0, 0), true, 1.0);
        assert_eq!(src, "gauge", "an out-valued gauge is never overridden");
        assert!((mm - 0.1 * 25.4).abs() < 1e-9);
        // A measured DRY week (rows present, total 0) also wins: 0.00 in
        // gauge, never the model's wetter claim.
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.0, 7, 0, 0), true, 0.8);
        assert_eq!(src, "gauge");
        assert_eq!(mm, 0.0);
        // Radar day totals dominate the measured side: 'radar'.
        let (mm, src) = resolve_observed_rain(&win(0.1, 0.9, 0.0, 1, 4, 0), false, 0.0);
        assert_eq!(src, "radar");
        assert!((mm - 25.4).abs() < 1e-9);
        // No measured coverage at all: the archive supplies the term.
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.0, 0, 0, 0), false, 0.5);
        assert_eq!(src, "model_archive");
        assert!((mm - 0.5 * 25.4).abs() < 1e-9);
        // Legacy rows: gauge-quality coverage on a station install...
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.4, 0, 0, 3), true, 0.9);
        assert_eq!(src, "gauge");
        assert!((mm - 0.4 * 25.4).abs() < 1e-9);
        // ...model-quality (no coverage) on a station-less one.
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.4, 0, 0, 3), false, 0.1);
        assert_eq!(src, "model_archive");
        assert!((mm - 0.4 * 25.4).abs() < 1e-9, "max(archive, legacy rows)");
        // Nothing anywhere: 'none' with a zero term (never fabricated).
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.0, 0, 0, 0), false, 0.0);
        assert_eq!(src, "none");
        assert_eq!(mm, 0.0);
    }

    /// The ledger writer's per-tick decision: measured owners record the
    /// accumulator (same-day, plausible values only); model-nature,
    /// stale, or absent owners record the 'none' placeholder; the
    /// midnight-carry and garbage cases skip the write entirely.
    #[test]
    fn ledger_observation_gates_midnight_model_stale_and_garbage() {
        use crate::tempest::state::{RainOwner, Snapshot};
        let now = chrono::Utc::now().timestamp();
        let today = crate::timeutil::local_day_ordinal(now);
        let gauge = RainOwner {
            label: "Tempest".into(),
            is_live: true,
            is_fresh: true,
        };
        let mut snap = Snapshot {
            rain_in_today: 1.2,
            rain_today_day_ordinal: today,
            ..Default::default()
        };
        // Same-day gauge total records with provenance.
        assert_eq!(
            ledger_observation(&snap, Some(&gauge), now),
            Some((1.2, "gauge"))
        );
        // 23:59 rain, 00:00:10 tick: the accumulator still carries
        // YESTERDAY'S day bucket, so the write is skipped; the day-max
        // upsert can never pin yesterday's total onto the new row.
        snap.rain_today_day_ordinal = today - 1;
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
        snap.rain_today_day_ordinal = today;
        // A model-nature owner records the placeholder, never the
        // whole-day forecast.
        let model = RainOwner {
            label: "open_meteo".into(),
            is_live: false,
            is_fresh: true,
        };
        assert_eq!(
            ledger_observation(&snap, Some(&model), now),
            Some((0.0, "none"))
        );
        // A stale owner (writer went silent) is no owner: placeholder,
        // so a frozen value cannot fabricate wet days forever.
        let stale = RainOwner {
            label: "noaa_mrms".into(),
            is_live: false,
            is_fresh: false,
        };
        assert_eq!(
            ledger_observation(&snap, Some(&stale), now),
            Some((0.0, "none"))
        );
        // No owner at all: placeholder.
        assert_eq!(ledger_observation(&snap, None, now), Some((0.0, "none")));
        // Garbage frames are rejected, not clamped: the day's ledger
        // stays untouched.
        snap.rain_in_today = 30.0; // a 25.4x unit misparse class value
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
        snap.rain_in_today = -0.5;
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
        snap.rain_in_today = f64::NAN;
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
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
    fn verdict_multiplier_scales_and_caps_dispatch() {
        use crate::ha::snapshot::{IrrigationSnapshot, ZoneMath, ZoneState, ZoneVerdict};
        let mk = |slug: &str, planned: u32, mult: f64, max_dur: u32| ZoneState {
            slug: slug.into(),
            planned_run_seconds: planned,
            verdict: Some(ZoneVerdict {
                zone_slug: slug.into(),
                verdict: "run".into(),
                multiplier: mult,
                ..Default::default()
            }),
            math: Some(ZoneMath {
                max_duration_seconds: max_dur,
                scheduled_seconds: planned,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut snap = IrrigationSnapshot {
            zones: vec![
                mk("a", 600, 1.0, 1200), // no rule -> unchanged (byte-identical case)
                mk("b", 600, 0.5, 1200), // halve -> 300
                mk("c", 600, 1.5, 1200), // extend -> 900, under the cap
                mk("d", 600, 1.5, 720),  // extend -> 900 held to the 720s ceiling
                mk("e", 0, 0.5, 1200),   // a skipped zone (0s) stays 0
                mk("f", 720, 0.5, 720),  // halve -> 360, back under the ceiling
            ],
            ..Default::default()
        };
        // Zone f arrived from the allocator already sitting on its ceiling.
        snap.zones[5].math.as_mut().unwrap().cap_binding = true;
        apply_verdict_multiplier(&mut snap);
        let planned: Vec<u32> = snap.zones.iter().map(|z| z.planned_run_seconds).collect();
        assert_eq!(planned, vec![600, 300, 900, 720, 0, 360]);
        // math.scheduled_seconds mirrors the dispatched value so the "why this
        // duration" tile agrees with what the controller receives.
        assert_eq!(snap.zones[3].math.as_ref().unwrap().scheduled_seconds, 720);
        // A multiplier that ran into the ceiling reports it.
        assert!(snap.zones[3].math.as_ref().unwrap().cap_binding);
        // A multiplier that pulled the run back UNDER the ceiling clears it:
        // the ceiling is no longer what set the minutes.
        assert!(!snap.zones[5].math.as_ref().unwrap().cap_binding);
    }

    #[test]
    fn cap_binding_needs_a_run_that_sits_on_the_ceiling() {
        use crate::ha::snapshot::{IrrigationSnapshot, WaterBudget, ZoneMath, ZoneState};
        // `session_capped` describes the IDEAL weekly session, not today's
        // plan, so it stays true on a zone the allocator zeroed for an
        // unrelated reason. Reading it straight onto `cap_binding` printed
        // "0 min (capped at 60 min)": a ceiling shortening a run that does
        // not exist.
        let zone = |slug: &str, max_dur: u32, override_mode: &str| ZoneState {
            slug: slug.into(),
            override_mode: override_mode.into(),
            math: Some(ZoneMath {
                max_duration_seconds: max_dur,
                ..Default::default()
            }),
            ..Default::default()
        };
        let budget = |slug: &str, today_s: u32, capped: bool| WaterBudget {
            zone_slug: slug.into(),
            today_seconds: today_s,
            session_capped: capped,
            ..Default::default()
        };
        let mut snap = IrrigationSnapshot {
            zones: vec![
                // Spaced since the last session: the ideal slice outgrows the
                // ceiling, but nothing runs today.
                zone("spaced", 3600, "auto"),
                // The allocator's session really did hit the ceiling.
                zone("shorted", 3600, "auto"),
                // Under the ceiling with room to spare.
                zone("roomy", 3600, "auto"),
                // Force-run over a zero budget: the floor sized this, not
                // the ceiling.
                zone("forced", 3600, "run"),
            ],
            water_budgets: vec![
                budget("spaced", 0, true),
                budget("shorted", 3600, true),
                budget("roomy", 1200, false),
                budget("forced", 0, true),
            ],
            ..Default::default()
        };
        let policy = WateringPolicy::default();
        apply_budget_plan(&mut snap, &policy);
        let read = |i: usize| {
            let m = snap.zones[i].math.as_ref().unwrap();
            (m.scheduled_seconds, m.cap_binding)
        };
        assert_eq!(read(0), (0, false), "a zone at zero was not shortened");
        assert_eq!(read(1), (3600, true), "the ceiling set these minutes");
        assert_eq!(read(2), (1200, false), "room under the ceiling");
        assert_eq!(
            read(3),
            (FORCE_RUN_DEFAULT_S, false),
            "a force-run floor is not a capped run"
        );
    }

    #[test]
    fn seasonal_dial_reports_its_own_clamp_like_the_rule_multiplier_does() {
        use crate::ha::snapshot::{IrrigationSnapshot, WaterBudget, ZoneMath, ZoneState};
        // The dial scales AFTER the allocator, so it can push an uncapped
        // session into the ceiling. That clamp shortens the dispatched run
        // exactly as the rule multiplier's does, and reports itself the same
        // way instead of clipping the run silently.
        assert!(seasonal_cap_binds(3000, 150, 3600));
        assert!(!seasonal_cap_binds(3000, 100, 3600));
        assert!(!seasonal_cap_binds(3000, 150, 0), "no cap known, no clamp");
        let mut snap = IrrigationSnapshot {
            zones: vec![ZoneState {
                slug: "dialed".into(),
                override_mode: "auto".into(),
                math: Some(ZoneMath {
                    max_duration_seconds: 3600,
                    ..Default::default()
                }),
                ..Default::default()
            }],
            water_budgets: vec![WaterBudget {
                zone_slug: "dialed".into(),
                today_seconds: 3000,
                // The allocator itself did NOT hit the ceiling.
                session_capped: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        let policy = WateringPolicy {
            seasonal_adjust_pct: 150,
            ..Default::default()
        };
        apply_budget_plan(&mut snap, &policy);
        let m = snap.zones[0].math.as_ref().unwrap();
        assert_eq!(m.scheduled_seconds, 3600, "4500 s held to the ceiling");
        assert!(m.cap_binding, "the ceiling is what set these minutes");
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

    /// Two-day forecast whose entries carry usable temps plus (optionally) the
    /// provider's own daily ET0 in inches.
    fn fc_with(et0_in: f64) -> ForecastSnapshot {
        let day = |max: f64, min: f64| DailyEntry {
            temp_max_f: max,
            temp_min_f: min,
            et0_in,
            ..Default::default()
        };
        ForecastSnapshot {
            daily: vec![day(90.0, 65.0), day(88.0, 64.0)],
            ..Default::default()
        }
    }

    fn map_with(eid: &str, v: f64) -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(
            eid.to_string(),
            serde_json::json!({ "state": v.to_string() }),
        );
        m
    }

    #[test]
    fn et0_today_honors_the_mapped_bus_value_then_full_day_forecast_ranks() {
        let map = HashMap::new();
        // 1. A source-reported/mapped bus value owns the figure outright: the
        //    field's contract is FULL-DAY mm (the OM fill emits the converted
        //    daily total; an explicit HA/MQTT mapping is honored as mapped).
        let got = resolve_et0_today_mm(4.2, &map, &fc_with(0.18), 40.0, 180).expect("bus rung");
        assert!((got - 4.2).abs() < 1e-9, "bus value wins: {got}");
        // 2. No bus value -> the provider's full-day daily[0] ET0
        //    (0.18 in -> 4.572 mm). This is the rank whose UNITS issue #4
        //    broke: the OM fill emitted raw inches onto the mm bus.
        let got =
            resolve_et0_today_mm(0.0, &map, &fc_with(0.18), 40.0, 180).expect("provider rung");
        assert!((got - 4.572).abs() < 1e-9, "provider daily next: {got}");
        // 3. No provider ET0 -> the Open-Meteo HA REST sensor (mm).
        let ha = map_with("sensor.open_meteo_eto_today", 4.8);
        let got = resolve_et0_today_mm(0.0, &ha, &fc_with(0.0), 40.0, 180).expect("HA rung");
        assert!((got - 4.8).abs() < 1e-9, "HA sensor next: {got}");
        // 4. No provider / HA ET0 -> native Hargreaves (a real value).
        let native =
            resolve_et0_today_mm(0.0, &map, &fc_with(0.0), 40.0, 180).expect("native rung");
        assert!(native > 0.0, "native = {native}");
        // 5. Nothing anywhere -> None. The published eto_today_mm stays null
        //    (HA sensor unknown, dashboard dash); only the advisory soil
        //    projection opts into ENGINE_ET0_FALLBACK_MM, explicitly.
        let empty = ForecastSnapshot::default();
        assert_eq!(resolve_et0_today_mm(0.0, &map, &empty, 40.0, 180), None);
        // native_et0_mm returns None on a temps-absent day.
        assert!(native_et0_mm(&DailyEntry::default(), 40.0, 180).is_none());
    }

    #[test]
    fn today_range_resolves_forecast_first_then_ha_sensor_then_none() {
        // Forecast daily[0] carries temps: it wins outright (native installs
        // have no HA sensors, and the forecast refreshes every ~30 min vs
        // the REST sensor's 4h).
        let map = map_with("sensor.open_meteo_temp_max_today", 91.0);
        let (tmax, tmin, hum) = resolve_today_range(&fc_with(0.0), &map);
        assert_eq!(tmax, Some(90.0), "forecast beats the legacy sensor");
        assert_eq!(tmin, Some(65.0));
        // fc_with's days carry no humidity (humidity_pct 0 = no coverage).
        assert_eq!(hum, None);

        // No forecast: the legacy HA sensors fill in.
        let empty = ForecastSnapshot::default();
        let (tmax, _, _) = resolve_today_range(&empty, &map);
        assert_eq!(tmax, Some(91.0), "legacy sensor is the fallback");

        // Nothing anywhere: None, never a fabricated 0°F/0°F range or 0%
        // humidity (the pre-fix behavior on every native install).
        let (tmax, tmin, hum) = resolve_today_range(&empty, &HashMap::new());
        assert_eq!((tmax, tmin, hum), (None, None, None));
    }

    #[test]
    fn forecast_day_et0_mirrors_todays_forecast_ranks() {
        let map = HashMap::new();
        let key = "sensor.open_meteo_eto_tomorrow";
        // Provider daily[1] ET0 (inches -> mm) ranks first, mirroring the
        // forecast-side order of resolve_et0_today_mm, so "ET Tomorrow" agrees
        // with "ET Today" in method and units and the pair can no longer
        // disagree 25x across midnight.
        let ha = map_with(key, 4.4);
        let got = forecast_day_et0_mm(&ha, key, &fc_with(0.18), 1, 40.0, 180, 0.0);
        assert!((got - 4.572).abs() < 1e-9, "provider daily wins: {got}");
        // The legacy HA-install sensor fills in when the provider has no ET0.
        let got = forecast_day_et0_mm(&ha, key, &fc_with(0.0), 1, 40.0, 180, 0.0);
        assert!((got - 4.4).abs() < 1e-9, "HA sensor next: {got}");
        // No provider / HA ET0 -> native compute from the day's temps.
        let native = forecast_day_et0_mm(&map, key, &fc_with(0.0), 1, 40.0, 180, 0.0);
        assert!(native > 0.0, "native = {native}");
        // Day outside the forecast window -> the caller's fallback.
        let fb = forecast_day_et0_mm(&map, key, &ForecastSnapshot::default(), 1, 40.0, 180, 1.5);
        assert!((fb - 1.5).abs() < 1e-9);
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

    /// The zone editor shows the inferred default as the placeholder of the
    /// two budget fields. It compiles for the browser, where this module does
    /// not, so it carries its own copy of the rule; the two must never drift,
    /// or the box would promise a number the yard is not watering on.
    #[test]
    fn the_zone_editor_placeholder_matches_the_engine_default() {
        use crate::components::settings::zones::inferred_weekly_target;
        for slug in [
            "back_yard",
            "front_yard",
            "lawn",
            "orchard",
            "back_yard_shrubs",
            "front_garden",
            "flower_bed",
            "herb_bed",
        ] {
            assert_eq!(
                inferred_weekly_target(slug),
                agronomic_budget_default(slug),
                "{slug}"
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

    /// Assemble the snapshot the same way refresh_once_native does (empty HA
    /// entity map, no soil config, no scripts), but with the raw stores under
    /// test. Returns the published IrrigationSnapshot.
    async fn assemble(
        forecast: ForecastSnapshot,
        tempest: TempestSnapshot,
        zones: &[&str],
        policy: WateringPolicy,
    ) -> IrrigationSnapshot {
        assemble_with(
            forecast,
            tempest,
            zones,
            policy,
            HashMap::new(),
            None,
            false,
        )
        .await
    }

    /// `assemble` with the two arguments the migration turns on: the Home
    /// Assistant entity map, and the native control surface. `ha_helper_reads`
    /// is what the two deployment paths differ by.
    async fn assemble_with(
        forecast: ForecastSnapshot,
        tempest: TempestSnapshot,
        zones: &[&str],
        policy: WateringPolicy,
        map: HashMap<String, Value>,
        control: Option<&crate::persistence::IrrigationControlState>,
        ha_helper_reads: bool,
    ) -> IrrigationSnapshot {
        let fs = forecast_store_with(forecast);
        let ts = tempest_store_with(tempest);
        let zone_runtime: HashMap<String, ZoneRuntime> = HashMap::new();
        let scripts = CompiledScripts::compile(&[]);
        build_from_map(
            map,
            &fs,
            &ts,
            &zone_idents(zones),
            &zone_runtime,
            &policy,
            &scripts,
            None, // sensor_history
            None, // forecast_obs
            None, // balance
            control,
            ha_helper_reads,
        )
        .await
    }

    // ─────────────────────────────────────────────────────────────
    // The 0.7.22 Home Assistant helper cutover.
    //
    // Every one of these asserts the same shape twice: unadopted behaves
    // exactly as it did before, adopted reads LocalSky's own value and never
    // touches the entity again. The pair is the point. A release that only
    // proved the second half would leave a window where the value is neither
    // adopted nor read, which for a vacation pause is a watered yard.
    // ─────────────────────────────────────────────────────────────

    fn adopted(entity: &str) -> crate::ha::snapshot::HaAdoptedHelper {
        crate::ha::snapshot::HaAdoptedHelper {
            entity: entity.to_string(),
            outcome: "adopted".to_string(),
            target: crate::ha_adopt::target_of(entity).to_string(),
            adopted_value: None,
            observed_value: None,
            previous_value: None,
            epoch: 1,
        }
    }

    fn helper_map(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(id, v)| ((*id).to_string(), v.clone()))
            .collect()
    }

    fn calm() -> (ForecastSnapshot, TempestSnapshot) {
        let now = Utc::now().timestamp();
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            hourly: vec![current_hour(72.0, 4.0, 50)],
            ..Default::default()
        };
        (fc, live_station(now, 72.0, 3.0, 50.0, 0.0))
    }

    #[tokio::test]
    async fn the_vacation_pause_flips_from_the_helper_to_the_store_on_adoption() {
        let future = Utc::now().timestamp() + 86_400;
        let map = helper_map(&[(
            crate::ha_adopt::PAUSE_UNTIL,
            serde_json::json!({ "state": "x", "attributes": { "timestamp": future } }),
        )]);
        let control = crate::persistence::IrrigationControlState::default();

        // Unadopted: the helper still decides, exactly as before.
        let (fc, ts) = calm();
        let before = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            map.clone(),
            Some(&control),
            true,
        )
        .await;
        assert_eq!(before.pause_until_epoch, future);

        // Adopted: LocalSky's own store governs and the entity is not read,
        // even though it is sitting right there in the map holding a pause.
        let policy = WateringPolicy {
            ha_adoption: vec![adopted(crate::ha_adopt::PAUSE_UNTIL)],
            ..Default::default()
        };
        let (fc, ts) = calm();
        let after = assemble_with(fc, ts, &["front"], policy, map, Some(&control), true).await;
        assert_eq!(
            after.pause_until_epoch, 0,
            "an adopted pause comes from the store, not from the entity"
        );
    }

    #[tokio::test]
    async fn an_adopted_store_pause_decides_with_no_helper_in_the_map() {
        // The install shape the migration exists for: a Home Assistant deploy
        // whose owner never created the input_datetime. Before the pass, Rain
        // Delay wrote to an entity that does not exist and the next tick read
        // zero, so the owner who thought they had paused got a watered yard
        // and no error. After it, the pause lives in LocalSky and holds.
        let mut control = crate::persistence::IrrigationControlState::default();
        control.pause_until_epoch = Utc::now().timestamp() + 3_600;
        let policy = WateringPolicy {
            ha_adoption: vec![adopted(crate::ha_adopt::PAUSE_UNTIL)],
            ..Default::default()
        };
        let (fc, ts) = calm();
        let snap = assemble_with(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            Some(&control),
            true,
        )
        .await;
        assert_eq!(snap.pause_until_epoch, control.pause_until_epoch);
        assert_eq!(snap.skip_check.verdict, "skip");
        assert!(
            snap.skip_check.reason.contains("Paused"),
            "{:?}",
            snap.skip_check.reason
        );
    }

    // The sticky overrides are deliberately NOT part of this release. They
    // have no helper and no adoption marker; a store holding a Force or a
    // Skip has to stay inert on the Home Assistant path, exactly as it was
    // before, because the panel showed Auto for the whole time it was set and
    // activating it here would force-run the yard past every gate with
    // nothing on screen. The pre-existing defect is fixed on its own.
    #[tokio::test]
    async fn a_stored_sticky_override_stays_inert_on_the_home_assistant_path() {
        let mut control = crate::persistence::IrrigationControlState::default();
        control.global_override = "run".to_string();
        control
            .zone_overrides
            .insert("front".to_string(), "skip".to_string());
        let (fc, ts) = calm();
        let snap = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            HashMap::new(),
            Some(&control),
            true,
        )
        .await;
        assert_eq!(snap.global_override, "auto");
        assert_eq!(snap.zones[0].override_mode, "auto");
        assert_ne!(snap.skip_check.reason, "Manual override: run");

        // And it still decides on the native path, where it always has.
        let (fc, ts) = calm();
        let native = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            HashMap::new(),
            Some(&control),
            false,
        )
        .await;
        assert_eq!(native.global_override, "run");
        assert_eq!(native.zones[0].override_mode, "skip");
    }

    #[tokio::test]
    async fn the_two_toggles_flip_from_the_helpers_to_the_store_on_adoption() {
        let map = helper_map(&[
            (
                crate::ha_adopt::PAUSE_TOGGLE,
                serde_json::json!({ "state": "on" }),
            ),
            (
                crate::ha_adopt::DRY_RUN_TOGGLE,
                serde_json::json!({ "state": "on" }),
            ),
        ]);
        let control = crate::persistence::IrrigationControlState::default();

        let (fc, ts) = calm();
        let before = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            map.clone(),
            Some(&control),
            true,
        )
        .await;
        assert!(before.skip_check.is_paused, "the helper still decides");

        let policy = WateringPolicy {
            ha_adoption: vec![
                adopted(crate::ha_adopt::PAUSE_TOGGLE),
                adopted(crate::ha_adopt::DRY_RUN_TOGGLE),
            ],
            ..Default::default()
        };
        let (fc, ts) = calm();
        let after = assemble_with(fc, ts, &["front"], policy, map, Some(&control), true).await;
        assert!(!after.skip_check.is_paused);
        assert!(!after.skip_check.is_dry_run);

        // And with the store holding one on, it decides with no helper at
        // all, which is what makes these two work on a standalone install.
        let mut on = crate::persistence::IrrigationControlState::default();
        on.is_paused = true;
        let policy = WateringPolicy {
            ha_adoption: vec![adopted(crate::ha_adopt::PAUSE_TOGGLE)],
            ..Default::default()
        };
        let (fc, ts) = calm();
        let native =
            assemble_with(fc, ts, &["front"], policy, HashMap::new(), Some(&on), true).await;
        assert!(native.skip_check.is_paused);
        assert_eq!(native.skip_check.verdict, "skip");
    }

    #[tokio::test]
    async fn a_threshold_helper_outranks_settings_until_it_is_adopted() {
        let map = helper_map(&[(
            crate::ha_adopt::MAX_WIND,
            serde_json::json!({ "state": "5" }),
        )]);
        let mut policy = WateringPolicy::default();
        policy.skip_rules.max_wind_mph = 20.0;

        let (fc, ts) = calm();
        let before =
            assemble_with(fc, ts, &["front"], policy.clone(), map.clone(), None, true).await;
        assert_eq!(
            before.skip_check.max_wind_mph, 5.0,
            "before adoption the helper decides, exactly as it always did"
        );

        policy.ha_adoption = vec![adopted(crate::ha_adopt::MAX_WIND)];
        let (fc, ts) = calm();
        let after = assemble_with(fc, ts, &["front"], policy, map, None, true).await;
        assert_eq!(
            after.skip_check.max_wind_mph, 20.0,
            "after adoption Settings is the only source"
        );
    }

    #[tokio::test]
    async fn the_native_path_never_reads_a_helper_whatever_the_markers_say() {
        // The map is empty by construction on the native path, so a gate that
        // keyed on the marker list alone would resolve every control to its
        // absent-entity default. ha_helper_reads = false is what stops that,
        // and a standalone install carries no markers at all.
        let mut control = crate::persistence::IrrigationControlState::default();
        control.pause_until_epoch = 1_950_000_000;
        control.is_dry_run = true;
        let (fc, ts) = calm();
        let snap = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            HashMap::new(),
            Some(&control),
            false,
        )
        .await;
        assert_eq!(snap.pause_until_epoch, 1_950_000_000);
        assert!(snap.skip_check.is_dry_run);
        assert!(snap.ha_adoption.is_empty());
    }

    #[tokio::test]
    async fn the_migration_record_rides_the_snapshot_for_the_notice() {
        let policy = WateringPolicy {
            ha_adoption: vec![adopted(crate::ha_adopt::RAIN_SKIP)],
            ..Default::default()
        };
        let (fc, ts) = calm();
        let snap = assemble_with(fc, ts, &["front"], policy, HashMap::new(), None, true).await;
        assert_eq!(snap.ha_adoption.len(), 1);
        assert_eq!(snap.ha_adoption[0].entity, crate::ha_adopt::RAIN_SKIP);
        assert!(
            !snap.controls_persisted,
            "no control state means no control store, which is what the notice has to say"
        );
    }

    // The notice needs to tell "this install has no database, so the four
    // controls can never be adopted" apart from "a control was not answering
    // when the pass looked, so it was left alone". Both leave the control out
    // of the record set, so the records alone cannot say which; the snapshot
    // carries the bit.
    #[tokio::test]
    async fn a_mounted_control_store_says_so_on_the_snapshot() {
        let control = crate::persistence::IrrigationControlState::default();
        let (fc, ts) = calm();
        let snap = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            HashMap::new(),
            Some(&control),
            true,
        )
        .await;
        assert!(snap.controls_persisted);
    }

    // The per-zone ZoneMath cap follows the CONFIGURED max_run_minutes through
    // WateringPolicy::from_config (the same builder boot AND hot reload use),
    // so an applied cap change lands on the very next snapshot build.
    #[tokio::test]
    async fn zone_math_cap_follows_the_configured_run_limit() {
        let now = Utc::now().timestamp();
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            hourly: vec![current_hour(72.0, 4.0, 50)],
            ..Default::default()
        };
        let tempest = live_station(now, 72.0, 3.0, 50.0, 0.0);
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "controller_id": "os_main",
                "controller_station": "1",
                "max_run_minutes": 90
            }))
            .unwrap(),
        );
        let policy = WateringPolicy::from_config(&cfg);
        let fs = forecast_store_with(fc);
        let ts = tempest_store_with(tempest);
        let scripts = CompiledScripts::compile(&[]);
        let snap = build_from_map(
            HashMap::new(),
            &fs,
            &ts,
            &zone_idents(&["front"]),
            &policy.zone_runtime,
            &policy,
            &scripts,
            None,
            None,
            None,
            None,
            false,
        )
        .await;
        let math = snap.zones[0]
            .math
            .clone()
            .expect("math is always assembled");
        assert_eq!(
            math.max_duration_seconds, 5400,
            "ZoneMath carries the configured 90 minute cap in seconds"
        );
    }

    // ── CLEAR RUN ─────────────────────────────────────────────────────────────
    // Dry, warm, calm, fresh station, no soil config: the assembled verdict is a
    // plain "run" with an empty reason, and every per-zone verdict is run/global.
    // heat_index_max_3day_f is asserted to be the PER-DAY pairing (each day's
    // high temp × THAT day's humidity), proving the now-humidity bug is absent.
    /// Per-zone verdicts are produced one per entry in `soil_zones`. On an
    /// install with no zones in `localsky.toml` (zones from `LOCALSKY_ZONES`,
    /// the `WateringPolicy::default()` passed here) that list is the legacy
    /// fallback, which used to be four hardcoded slugs: it handed such an
    /// install verdicts for zones it did not own and none for the zones it
    /// did. Every active zone must appear, and nothing else.
    #[tokio::test]
    async fn probeless_install_gets_a_verdict_for_its_own_zones_only() {
        let now = Utc::now().timestamp();
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            daily: vec![DailyEntry {
                time_epoch: now,
                temp_max_f: 88.0,
                temp_min_f: 70.0,
                humidity_pct: 50,
                precip_sum_in: 0.0,
                precip_probability_max: Some(0),
                wind_max_mph: 4.0,
                ..Default::default()
            }],
            hourly: vec![current_hour(78.0, 4.0, 50)],
            ..Default::default()
        };
        let snap = assemble(
            fc,
            live_station(now, 78.0, 3.0, 50.0, 0.0),
            &["orchard", "west_strip", "herb_bed"],
            WateringPolicy::default(),
        )
        .await;

        let got: Vec<&str> = snap
            .zone_verdicts
            .iter()
            .map(|v| v.zone_slug.as_str())
            .collect();
        assert_eq!(got, vec!["orchard", "west_strip", "herb_bed"], "{got:?}");
        // Every zone on the snapshot carries its own verdict, not None.
        for z in &snap.zones {
            assert!(z.verdict.is_some(), "no verdict for {}", z.slug);
        }
    }

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
            precip_probability_max: Some(0),
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

// ─────────────────────────────────────────────────────────────
// The adoption COMMIT. `ha_adopt::plan` is pure and tested next to itself;
// this is the orchestration where a wrong decision actually reaches SQLite
// and the config file, and it is where both of the blockers this release
// shipped and had to fix lived. Every branch of the evidence rule, the
// ordering and the disarm is pinned here.
// ─────────────────────────────────────────────────────────────
#[cfg(test)]
mod adopt_tick_tests {
    use super::*;
    use crate::config::schema::Config;
    use crate::ha_adopt::{
        DRY_RUN_TOGGLE, MAX_WIND, MIN_TEMP, OVERRIDE_TOMORROW, PAUSE_TOGGLE, PAUSE_UNTIL, RAIN_SKIP,
    };
    use crate::persistence::IrrigationControlStore;
    use crate::ports::config_store::ConfigStore;
    use serde_json::json;
    use std::collections::VecDeque;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("localsky-adopt-tick-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// A migrated control DB on a READ-ONLY connection: every read answers,
    /// every write fails. The only way to exercise the failed-write branch
    /// now that a failed READ defers before it.
    fn readonly_control_store(tag: &str) -> IrrigationControlStore {
        let path = tmp_dir(tag).join("localsky.db");
        let mut seed = Connection::open(&path).unwrap();
        crate::persistence::run_migrations(&mut seed).unwrap();
        drop(seed);
        let ro = Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        IrrigationControlStore::new(Arc::new(Mutex::new(ro)))
    }

    async fn control_store(migrated: bool) -> IrrigationControlStore {
        let mut c = Connection::open_in_memory().unwrap();
        if migrated {
            crate::persistence::run_migrations(&mut c).unwrap();
        }
        IrrigationControlStore::new(Arc::new(Mutex::new(c)))
    }

    /// An `AdoptState` on a real tempdir config store and an in-memory
    /// migrated control DB, with a canned queue standing in for the
    /// commit-time re-read of `/api/states`.
    async fn state(
        tag: &str,
        control: Option<IrrigationControlStore>,
        canned: Vec<Option<HelperReadout>>,
    ) -> (AdoptState, Arc<crate::config::FileConfigStore>) {
        let path = tmp_dir(tag).join("localsky.toml");
        let cfg_store = Arc::new(crate::config::FileConfigStore::new(&path));
        ConfigStore::save(&*cfg_store, &Config::default())
            .await
            .unwrap();
        let st = AdoptState {
            cfg_store: cfg_store.clone(),
            control_store: control,
            policy: Arc::new(ArcSwap::from_pointee(WateringPolicy::from_config(
                &Config::default(),
            ))),
            helpers: HelperFetch::Canned(std::sync::Mutex::new(VecDeque::from(canned))),
            fingerprint: None,
            stable_ticks: 0,
            awaiting_config: false,
            no_config_warned: false,
            pending_revert: None,
        };
        (st, cfg_store)
    }

    fn entity(state: &str) -> Value {
        json!({ "state": state })
    }

    fn pause_entity(ts: i64) -> Value {
        json!({ "state": "2026-09-04 06:00:00", "attributes": { "timestamp": ts as f64 } })
    }

    /// Every helper present and holding a value, the way a working Home
    /// Assistant answers.
    fn full(total: usize) -> HelperReadout {
        let mut helpers = HashMap::new();
        helpers.insert(MAX_WIND.to_string(), entity("12"));
        helpers.insert(MIN_TEMP.to_string(), entity("38"));
        helpers.insert(RAIN_SKIP.to_string(), entity("0.3"));
        helpers.insert(PAUSE_UNTIL.to_string(), pause_entity(1_900_000_000));
        helpers.insert(OVERRIDE_TOMORROW.to_string(), entity("skip"));
        helpers.insert(PAUSE_TOGGLE.to_string(), entity("on"));
        helpers.insert(DRY_RUN_TOGGLE.to_string(), entity("off"));
        HelperReadout {
            total_entities: total,
            write_seq: crate::ha_adopt::write_seq(),
            helpers,
        }
    }

    /// A Home Assistant answering with plenty of entities and none of the
    /// seven helpers: the startup shape, and the shape of an install that
    /// simply never made them.
    fn all_absent(total: usize) -> HelperReadout {
        HelperReadout {
            total_entities: total,
            write_seq: crate::ha_adopt::write_seq(),
            helpers: HashMap::new(),
        }
    }

    async fn marked(cfg_store: &crate::config::FileConfigStore) -> Vec<String> {
        ConfigStore::load(cfg_store)
            .await
            .unwrap()
            .ha_adoption
            .into_iter()
            .map(|h| h.entity)
            .collect()
    }

    // The stability window itself: two ticks conclude nothing, the third
    // commits, and nothing is written to either store before it.
    #[tokio::test]
    async fn three_identical_ticks_before_anything_commits() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let cs = control_store(true).await;
        let (mut st, cfg_store) =
            state("commit-on-three", Some(cs.clone()), vec![Some(full(400))]).await;

        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(marked(&cfg_store).await.is_empty());
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(marked(&cfg_store).await.is_empty());
        assert_eq!(cs.get().await.pause_until_epoch, 0);

        assert!(
            adopt_tick(&mut st, &full(400)).await,
            "the third identical answer commits and disarms the pass"
        );
        let m = marked(&cfg_store).await;
        for id in crate::ha_adopt::ENTITIES {
            assert!(m.contains(&id.to_string()), "{id} not marked");
        }
        let stored = cs.get().await;
        assert_eq!(stored.pause_until_epoch, 1_900_000_000);
        assert!(stored.is_paused);
        assert_eq!(
            ConfigStore::load(&*cfg_store)
                .await
                .unwrap()
                .engine
                .skip_rules
                .max_wind_mph,
            12.0
        );
        // The policy swap is what makes the cutover live without a restart.
        assert!(st.policy.load().ha_read_retired(PAUSE_TOGGLE));
    }

    // A moving answer restarts the window, so a Home Assistant whose helpers
    // are still appearing never reaches a conclusion.
    #[tokio::test]
    async fn a_changed_answer_restarts_the_window() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let (mut st, cfg_store) = state(
            "answer-moves",
            Some(control_store(true).await),
            vec![Some(full(400))],
        )
        .await;
        let mut half = full(400);
        half.helpers.remove(PAUSE_TOGGLE);

        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &half).await);
        assert_eq!(st.stable_ticks, 1, "a different answer starts over");
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(
            marked(&cfg_store).await.is_empty(),
            "five ticks, never three the same in a row: nothing concluded"
        );
    }

    // The blocker this design exists for. `/api/states` answers long before
    // the `input_*` platforms are set up, so all seven read absent while the
    // entity COUNT climbs. Without the count in the stability key, three
    // identical ticks of a starting Home Assistant look exactly like an
    // install with no helpers, and the commit retires a live vacation pause
    // onto an M0017 default.
    #[tokio::test]
    async fn a_climbing_entity_count_is_a_home_assistant_that_is_still_starting() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let (mut st, cfg_store) = state(
            "starting-ha",
            Some(control_store(true).await),
            vec![Some(all_absent(400))],
        )
        .await;
        for n in [120, 240, 360, 400] {
            assert!(!adopt_tick(&mut st, &all_absent(n)).await);
            assert_eq!(
                st.stable_ticks, 1,
                "n={n}: a moved count restarts the window"
            );
        }
        assert!(marked(&cfg_store).await.is_empty());
    }

    // And once the count holds still, an absence is held to five minutes
    // rather than twenty seconds, because absence is the only shape where a
    // retirement moves a protected gate onto a column no human wrote.
    #[tokio::test]
    async fn an_absent_control_helper_is_held_to_the_long_window() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let (mut st, cfg_store) = state(
            "absent-long-window",
            Some(control_store(true).await),
            vec![Some(all_absent(400))],
        )
        .await;
        for tick in 1..ADOPT_STABLE_TICKS_ABSENT {
            assert!(
                !adopt_tick(&mut st, &all_absent(400)).await,
                "tick {tick} must not conclude"
            );
            assert!(marked(&cfg_store).await.is_empty());
        }
        assert!(
            adopt_tick(&mut st, &all_absent(400)).await,
            "at five minutes of an unmoving answer, absence is finally evidence"
        );
        assert_eq!(
            marked(&cfg_store).await.len(),
            crate::ha_adopt::ENTITIES.len()
        );
    }

    // A zero-entity answer is never evidence, and it cannot bank the ticks it
    // already earned either.
    #[tokio::test]
    async fn a_zero_entity_answer_clears_the_evidence_it_had() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let (mut st, cfg_store) = state(
            "zero-entities",
            Some(control_store(true).await),
            vec![Some(full(400))],
        )
        .await;
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert_eq!(st.stable_ticks, 2);

        let mut empty = full(0);
        empty.helpers.clear();
        assert!(!adopt_tick(&mut st, &empty).await);
        assert_eq!(st.stable_ticks, 0);
        assert!(st.fingerprint.is_none());

        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(
            marked(&cfg_store).await.is_empty(),
            "the window has to be re-earned from scratch"
        );
    }

    // A control present and holding `unavailable` is NOT an answer. Recording
    // it would retire the read onto the store's default, and `unavailable` is
    // a stable answer, so the window is no protection. The rest of the set
    // still commits, and the pass stays armed for the one that deferred.
    #[tokio::test]
    async fn an_unavailable_control_leaves_the_pass_armed_and_its_read_live() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let cs = control_store(true).await;
        let mut r = full(400);
        r.helpers
            .insert(PAUSE_TOGGLE.to_string(), entity("unavailable"));
        let (mut st, cfg_store) = state(
            "unavailable-control",
            Some(cs.clone()),
            // One canned answer per commit attempt: the deferring one, then
            // the recovered one after Home Assistant finishes its reload.
            vec![Some(r.clone()), Some(full(400))],
        )
        .await;

        assert!(!adopt_tick(&mut st, &r).await);
        assert!(!adopt_tick(&mut st, &r).await);
        assert!(
            !adopt_tick(&mut st, &r).await,
            "a deferral keeps the pass armed even though it committed the rest"
        );
        let m = marked(&cfg_store).await;
        assert!(
            !m.contains(&PAUSE_TOGGLE.to_string()),
            "its read stays live"
        );
        assert!(
            m.contains(&PAUSE_UNTIL.to_string()),
            "the rest still committed"
        );
        assert!(!cs.get().await.is_paused, "and nothing was written for it");

        // Home Assistant finishes its reload; the helper answers, and the
        // pass finally takes it.
        let good = full(400);
        assert!(!adopt_tick(&mut st, &good).await);
        assert!(!adopt_tick(&mut st, &good).await);
        assert!(adopt_tick(&mut st, &good).await);
        assert!(marked(&cfg_store).await.contains(&PAUSE_TOGGLE.to_string()));
        assert!(cs.get().await.is_paused);
    }

    // The safety-critical branch: a control write that fails must leave
    // NOTHING marked, so every read stays exactly where it was.
    #[tokio::test]
    async fn a_failed_control_write_marks_nothing_at_all() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        // A readable database that refuses writes, so the pass gets all the
        // way to the UPSERT and fails there rather than deferring on the read.
        let (mut st, cfg_store) = state(
            "write-fails",
            Some(readonly_control_store("write-fails-db")),
            vec![Some(full(400)), Some(full(400))],
        )
        .await;
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(
            marked(&cfg_store).await.is_empty(),
            "a marker written over a value that never landed is the whole hazard"
        );
        assert_eq!(
            ConfigStore::load(&*cfg_store)
                .await
                .unwrap()
                .engine
                .skip_rules
                .max_wind_mph,
            10.0,
            "and the thresholds are not written either, so the redo is identical"
        );
        assert_eq!(st.stable_ticks, 0, "the next attempt re-earns its evidence");
    }

    // A pass whose control writes landed and whose marker save then failed
    // must not leave its own writes behind: the helper moved to off before
    // the retry, and a retry reading is_paused=1 from the residue would have
    // recorded kept_local, ignored the helper, retired the read and held the
    // yard indefinitely with a notice saying LocalSky kept its own value.
    #[tokio::test]
    async fn a_failed_marker_save_restores_the_control_rows_it_wrote() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let cs = control_store(true).await;
        let mut off = full(400);
        off.helpers.insert(PAUSE_TOGGLE.to_string(), entity("off"));
        let (mut st, cfg_store) = state(
            "save-fails",
            Some(cs.clone()),
            vec![Some(full(400)), Some(off.clone())],
        )
        .await;
        // Make the config save fail AFTER the control writes: the atomic
        // write goes through localsky.toml.tmp, and a directory in its way
        // fails File::create while the config itself still loads.
        let tmp = cfg_store.path().with_extension("toml.tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(
            !adopt_tick(&mut st, &full(400)).await,
            "the marker save failed"
        );
        assert!(marked(&cfg_store).await.is_empty());
        let after = cs.get().await;
        assert!(!after.is_paused, "the pass's own write was put back");
        assert_eq!(after.pause_until_epoch, 0);
        assert_eq!(after.override_tomorrow, "none");
        assert!(st.pending_revert.is_none(), "restored on the spot");

        // The owner turns the pause off in Home Assistant, and the save starts
        // working. The retry adopts what the helper holds NOW.
        std::fs::remove_dir(&tmp).unwrap();
        assert!(!adopt_tick(&mut st, &off).await);
        assert!(!adopt_tick(&mut st, &off).await);
        assert!(adopt_tick(&mut st, &off).await, "everything committed");
        let cfg = ConfigStore::load(&*cfg_store).await.unwrap();
        let rec = cfg
            .ha_adoption
            .iter()
            .find(|h| h.entity == PAUSE_TOGGLE)
            .expect("the pause switch was handled");
        assert_eq!(
            rec.outcome,
            crate::ha_adopt::OUTCOME_ADOPTED,
            "the residue must not read as an operator answer"
        );
        assert_eq!(rec.adopted_value.as_deref(), Some("off"));
        assert!(!cs.get().await.is_paused, "and the yard is not held");
    }

    // A control read that FAILS is not "nothing was ever set". The pass plans
    // two irreversible things from this state: whether LocalSky's own answer
    // outranks the helper, and whether the read retires for good. An error
    // resolving to the all-default state says "the operator never set
    // anything", so a transient SQLite failure would overwrite a live native
    // pause with a legacy helper and retire the read on the way out. It defers
    // the whole pass instead, and stays armed.
    #[tokio::test]
    async fn a_control_read_that_fails_defers_the_whole_pass() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        // Unmigrated DB: the SELECT hits "no such table".
        let (mut st, cfg_store) = state(
            "control-read-fails",
            Some(control_store(false).await),
            vec![Some(full(400)), Some(full(400))],
        )
        .await;
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(
            !adopt_tick(&mut st, &full(400)).await,
            "an unreadable control store leaves the pass armed"
        );
        assert!(
            marked(&cfg_store).await.is_empty(),
            "nothing may be concluded from a state that could not be read"
        );
        assert_eq!(
            ConfigStore::load(&*cfg_store)
                .await
                .unwrap()
                .engine
                .skip_rules
                .max_wind_mph,
            10.0,
            "not even the thresholds, whose sink is the config file"
        );
        assert!(
            st.fingerprint.is_none(),
            "and the evidence is re-earned in full"
        );
    }

    // A control write landing DURING the committing tick: the answer the pass
    // is about to plan from is now stale, and committing it would write the
    // pre-write value back and then retire the read.
    #[tokio::test]
    async fn a_control_write_during_the_committing_tick_defers_the_commit() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let cs = control_store(true).await;
        let (mut st, cfg_store) =
            state("write-races", Some(cs.clone()), vec![Some(full(400))]).await;
        let r = full(400);
        assert!(!adopt_tick(&mut st, &r).await);
        assert!(!adopt_tick(&mut st, &r).await);
        // The owner taps Rain delay: the handler bumps the counter before it
        // calls Home Assistant.
        crate::ha_adopt::note_preadopt_write();
        assert!(!adopt_tick(&mut st, &r).await);
        assert!(marked(&cfg_store).await.is_empty());
        assert_eq!(cs.get().await.pause_until_epoch, 0);
        assert!(
            st.fingerprint.is_none(),
            "the evidence is re-earned in full"
        );
    }

    // The action handler holds the config write guard while it decides where
    // a control write goes and performs it, and the commit holds the same
    // guard from its re-read through the policy swap. A tap that reached the
    // handler first therefore lands, and bumps the counter, before the commit
    // can plan; the commit then finds the counter moved and refuses.
    #[tokio::test]
    async fn a_write_serialized_ahead_of_the_commit_is_seen_by_it() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let cs = control_store(true).await;
        let (mut st, cfg_store) =
            state("guarded-write", Some(cs.clone()), vec![Some(full(400))]).await;
        let r = full(400);
        assert!(!adopt_tick(&mut st, &r).await);
        assert!(!adopt_tick(&mut st, &r).await);
        // The handler got the guard first. Its helper write goes out while
        // the commit is parked behind it.
        let guard = cfg_store.begin_write().await;
        let handler = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            crate::ha_adopt::note_preadopt_write();
            drop(guard);
        };
        let ((), committed) = tokio::join!(handler, adopt_tick(&mut st, &r));
        assert!(!committed);
        assert!(
            marked(&cfg_store).await.is_empty(),
            "the commit planned from an answer taken before the write, and refused"
        );
        assert_eq!(cs.get().await.pause_until_epoch, 0);
        assert!(
            st.fingerprint.is_none(),
            "the evidence is re-earned in full"
        );
    }

    // The commit-time re-read is what makes the plan come from this tick. An
    // answer that moved between the tick and the commit is not committed.
    #[tokio::test]
    async fn an_answer_that_moved_since_the_tick_is_not_committed() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let mut moved = full(400);
        moved
            .helpers
            .insert(PAUSE_UNTIL.to_string(), pause_entity(1_950_000_000));
        let cs = control_store(true).await;
        let (mut st, cfg_store) = state(
            "answer-moved-at-commit",
            Some(cs.clone()),
            vec![Some(moved), Some(full(400))],
        )
        .await;
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(marked(&cfg_store).await.is_empty());
        assert_eq!(cs.get().await.pause_until_epoch, 0);
    }

    // And a re-read that fails commits nothing rather than falling back to
    // the answer it already had.
    #[tokio::test]
    async fn a_failed_commit_time_reread_commits_nothing() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let (mut st, cfg_store) =
            state("reread-fails", Some(control_store(true).await), vec![None]).await;
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(marked(&cfg_store).await.is_empty());
    }

    // Nothing left to do disarms the pass, and costs no round trip: a plan
    // that is empty never re-reads Home Assistant. The canned queue is empty
    // here, so a re-read would fail the commit and return false.
    #[tokio::test]
    async fn a_fully_marked_config_disarms_without_re_reading() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let cs = control_store(true).await;
        let (mut st, cfg_store) = state("already-done", Some(cs), vec![]).await;
        let mut cfg = ConfigStore::load(&*st.cfg_store).await.unwrap();
        for id in crate::ha_adopt::ENTITIES.iter() {
            cfg.ha_adoption.push(crate::ha::snapshot::HaAdoptedHelper {
                entity: (*id).to_string(),
                outcome: crate::ha_adopt::OUTCOME_NOT_FOUND.to_string(),
                target: crate::ha_adopt::target_of(id).to_string(),
                adopted_value: None,
                observed_value: None,
                previous_value: None,
                epoch: 1,
            });
        }
        ConfigStore::save(&*cfg_store, &cfg).await.unwrap();
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(
            adopt_tick(&mut st, &full(400)).await,
            "nothing to adopt disarms the pass"
        );
    }

    // No config file yet (the app is in the wizard): nothing is marked and
    // the pass stays armed.
    #[tokio::test]
    async fn no_config_file_defers_rather_than_concluding() {
        let _seq = crate::ha_adopt::SEQ_TEST_LOCK.lock().await;
        let path = tmp_dir("no-config").join("localsky.toml");
        let cfg_store = Arc::new(crate::config::FileConfigStore::new(&path));
        let mut st = AdoptState {
            cfg_store: cfg_store.clone(),
            control_store: Some(control_store(true).await),
            policy: Arc::new(ArcSwap::from_pointee(WateringPolicy::from_config(
                &Config::default(),
            ))),
            helpers: HelperFetch::Canned(std::sync::Mutex::new(VecDeque::new())),
            fingerprint: None,
            stable_ticks: 0,
            awaiting_config: false,
            no_config_warned: false,
            pending_revert: None,
        };
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert_eq!(st.stable_ticks, 0);
        assert!(!path.exists(), "and nothing was written");
        assert!(
            st.awaiting_config,
            "the snapshot has to be able to say the pass never ran here"
        );
        // A config appears (the wizard finished): the flag clears on the next
        // tick that can load it, whatever else that tick concludes.
        ConfigStore::save(&*cfg_store, &Config::default())
            .await
            .unwrap();
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!adopt_tick(&mut st, &full(400)).await);
        assert!(!st.awaiting_config);
    }
}
