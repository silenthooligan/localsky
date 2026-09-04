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
    /// legacy default) but absent from the loaded config file. Both
    /// numbers are the catalog's own answers for "not stated": the
    /// sprinkler catalog's rate for an unknown head, and the schema's
    /// unset run cap. The rate used to be a local 10 mm/hr while the
    /// catalog answers 25 for the same question, which made a config-less
    /// zone run two and a half times as long as a configured one asking
    /// for the same depth.
    pub fn fallback() -> Self {
        Self {
            throughput_mm_hr: crate::agronomy::sprinkler_precip_mm_hr("other"),
            max_duration_s: crate::config::schema::DEFAULT_MAX_RUN_MINUTES * 60,
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
    /// Root-depth override (mm); `None` = the species profile default.
    /// The soil model's TAW/RAW derivation reads it here so an applied
    /// zone edit reshapes the bucket on the next tick.
    pub root_depth_mm: Option<f64>,
    /// MAD override; `None` = the species default. Same hot-reload
    /// contract as `root_depth_mm`.
    pub mad_pct_override: Option<f64>,
    /// Per-zone scheduling-model pin from `ZoneConfig::scheduling_model`;
    /// `None` = the engine default (`WateringPolicy::scheduling_model`).
    pub scheduling_model: Option<crate::config::schema::SchedulingModel>,
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
    /// How this deployment maps instants to calendar days and days to
    /// UTC windows. Resolved once at policy build, where the configured
    /// timezone is known, and handed to every engine call that needs a
    /// calendar. A test overrides it to pin the morning window, which is
    /// what stops a fixture inheriting the runner's own zone.
    pub calendar: crate::engine::calendar::Calendar,
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
    /// Engine-default scheduling model from `cfg.engine.scheduling_model`.
    /// Per-zone pins ride `zone_agronomy`; `resolve_scheduling_model`
    /// composes the two. The `Default` derive yields `Weekly`, so every
    /// unconfigured path keeps the shipped allocator.
    pub scheduling_model: crate::config::schema::SchedulingModel,
    /// `cfg.engine.capture_efficiency`, read by the soil model's replay,
    /// sizing, and defer arithmetic (the field's long-standing "NOT READ
    /// BY THE WATERING DECISION" note ends where the soil model begins;
    /// the weekly allocator still never reads it). The `Default` derive
    /// yields 0.0; `effective_capture_efficiency` treats non-positive as
    /// the historical 0.70 so a Default-policy path cannot divide by zero.
    pub capture_efficiency: f64,
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
                .map(|(slug, z)| {
                    // Per-day rain-credit cap: the operator's override
                    // when set, else the root zone's own capacity, with
                    // the root depth resolved the way the tuning engine
                    // resolves it (explicit override, else the species
                    // default).
                    let root_depth_mm = z
                        .root_depth_mm
                        .unwrap_or_else(|| crate::engine::species_profile(z.species).root_depth_mm);
                    let rain_cap_mm = match z.rain_credit_cap_in {
                        Some(v) => v * 25.4,
                        None => crate::engine::taw_mm(z.soil_texture, root_depth_mm),
                    };
                    // Starting target for a zone with no explicit one,
                    // from the SPECIES the operator declared rather than
                    // from words in the zone's name.
                    let (default_budget_in, default_sessions) =
                        crate::agronomy::default_weekly_target_in(crate::engine::species_slug(
                            z.species,
                        ));
                    ZoneBudgetCfg {
                        slug: slug.replace('-', "_"),
                        name: z.display_name.clone(),
                        weekly_budget_in: z.weekly_budget_in,
                        sessions_per_week: z.sessions_per_week,
                        rain_cap_mm,
                        rain_cap_inferred: z.rain_credit_cap_in.is_none(),
                        default_budget_in,
                        default_sessions,
                    }
                })
                .collect(),
            calendar: crate::engine::calendar::Calendar {
                local_date: crate::timeutil::local_date,
                day_bounds_utc: crate::timeutil::local_day_bounds_utc,
            },
            ha_adoption: cfg.ha_adoption.clone(),
            scheduling_model: cfg.engine.effective_scheduling_model(),
            capture_efficiency: cfg.engine.capture_efficiency,
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
                            max_duration_s: z
                                .max_run_minutes
                                .unwrap_or(crate::config::schema::DEFAULT_MAX_RUN_MINUTES)
                                * 60,
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
                            root_depth_mm: z.root_depth_mm,
                            mad_pct_override: z.mad_pct_override,
                            scheduling_model: z.scheduling_model,
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

    /// The scheduling model that governs `slug`: the per-zone pin when the
    /// operator set one, else the engine default. A zone with no agronomy
    /// config at all (env-var installs, unconfigured zones) is pinned to
    /// the weekly model regardless of either knob: the bucket has no
    /// texture or species to derive TAW from, and guessing one would water
    /// on a fabricated soil.
    pub fn resolve_scheduling_model(&self, slug: &str) -> crate::config::schema::SchedulingModel {
        match self.zone_agronomy.get(slug) {
            Some(a) => a.scheduling_model.unwrap_or(self.scheduling_model),
            None => crate::config::schema::SchedulingModel::Weekly,
        }
    }

    /// Capture efficiency for the soil model's arithmetic. A non-positive
    /// configured value (including the `Default` derive's 0.0 on
    /// unconfigured paths) falls back to the historical 0.70 constant, the
    /// same treatment `defer_threshold_in` gives its knob, so a missing
    /// value can never zero every rain credit or blow up a refill
    /// division.
    /// Pin this policy's calendar to UTC. Test-only: a fixture that
    /// asserts anything about the morning window has to fix the window,
    /// or it passes on a machine whose zone matches the fixture's
    /// coordinates and fails in the build container.
    #[cfg(test)]
    pub fn with_utc_calendar(mut self) -> Self {
        self.calendar = crate::engine::calendar::Calendar::utc();
        self
    }

    pub fn effective_capture_efficiency(&self) -> f64 {
        if self.capture_efficiency > 0.0 {
            self.capture_efficiency.min(1.0)
        } else {
            0.70
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
pub(crate) fn seasonal_capped(raw_seconds: u32, seasonal_pct: u32, max_dur: u32) -> u32 {
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
    /// Per-day rain-credit cap (mm), resolved at policy-build time: the
    /// operator's `rain_credit_cap_in` override (inches x 25.4) when
    /// set, else the root zone's own capacity, TAW = (field capacity -
    /// wilting point) x root depth, from the zone's soil texture and
    /// species (root override honored). One day's rain or forecast
    /// credit never offsets more than this against the weekly target.
    pub rain_cap_mm: f64,
    /// True when `rain_cap_mm` was derived from soil texture and root
    /// depth rather than set by the operator. Display only.
    pub rain_cap_inferred: bool,
    /// Weekly target (inches) this zone waters toward while
    /// `weekly_budget_in` is unset, resolved at policy-build time from
    /// the zone's declared SPECIES (its peak crop coefficient against
    /// reference turf). A config-less env-var zone has no species, so
    /// its row carries the legacy name-based default instead.
    pub default_budget_in: f64,
    /// How many mornings `default_budget_in` splits across while
    /// `sessions_per_week` is unset. Same resolution.
    pub default_sessions: u32,
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
            // A config-less zone (env-var install) has no soil texture
            // or species to derive from, so it gets the default-texture
            // cap: sandy loam at the default turf root depth, which is
            // what such an install effectively is.
            rain_cap_mm: crate::engine::taw_mm(
                crate::config::schema::SoilTexture::SandyLoam,
                crate::agronomy::species_profile_by_slug("other").root_depth_mm,
            ),
            rain_cap_inferred: true,
            // A config-less zone has no declared species either, so the
            // zone's NAME is the only signal there is and the legacy
            // name-based default stands. Everywhere a species IS
            // declared, the target comes from that instead.
            default_budget_in: agronomic_budget_default(&z.slug).0,
            default_sessions: agronomic_budget_default(&z.slug).1,
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
    /// The RAW sum: this is what rides the wire as `observed_rain_mm`.
    pub observed_rain_mm: f64,
    /// "gauge" | "radar" | "model_archive" | "none".
    pub observed_rain_source: String,
    /// The same window as one value per covered day (mm), from the SAME
    /// ladder rung as the sum above. Feeds the per-day rain-credit cap:
    /// the balance clips each day at the zone's root-zone capacity, and
    /// only a day series can say whether the week's rain fell in one
    /// storm or six drizzles. Sums to `observed_rain_mm` (up to float
    /// rounding); empty when the rung is "none" or no store is mounted.
    pub observed_rain_days_mm: Vec<f64>,
    /// Forecast bias model (identity when under-trained or absent).
    pub bias: crate::engine::BiasModel,
    /// Per-zone run evidence, keyed by underscore-normalized slug.
    pub per_zone: HashMap<String, ZoneRunEvidence>,
    /// Day-granular evidence for the soil model's replay window, gathered
    /// on the same cached cadence as everything above so the sync
    /// snapshot build never touches SQLite.
    pub soil: SoilTickEvidence,
    /// The runs-store window read ERRORED this tick (distinct from an
    /// empty result or no store mounted). The replay would then see none
    /// of the irrigation the system itself dispatched (applied=0 on
    /// every day), so a soil-governed zone that watered yesterday could
    /// reconstruct an inflated depletion and re-dispatch a full refill.
    /// The soil pass treats a degraded tick as evidence-unavailable:
    /// buckets are not published and the governed swap stands down until
    /// a clean read.
    pub runs_degraded: bool,
}

/// Per-tick evidence for the soil model's trailing replay window,
/// gathered beside the weekly balance's figures. One entry per trailing
/// configured-tz local day (`engine::soil_schedule::RECON_WINDOW_DAYS`,
/// oldest first, today last). Every column degrades independently, the
/// BalanceTick contract: an uncovered rain day is 0.0 (the replay's
/// [0, TAW] clamp bounds the cold-start anchor either way), a day with
/// no ET0 evidence resolves through the ladder's per-zone fallback rung
/// at plan time, and an uncovered applied day is zero valve seconds.
#[derive(Debug, Clone, Default)]
pub struct SoilTickEvidence {
    /// The window's local days, oldest first, today last. Empty when the
    /// configured timezone cannot produce day bounds (never in practice).
    pub dates: Vec<chrono::NaiveDate>,
    /// Gross rain (mm) per day, aligned to `dates`. Resolved through the
    /// SAME coverage-precedence ladder as the weekly day series
    /// (`resolve_observed_rain_days`), extended to keep dates: measured
    /// rows (gauge/radar; legacy counts on station installs) win
    /// outright even at 0.00, else ONE whole model-side series (provider
    /// archive vs model-quality rows, by sum) supplies the days it
    /// covers. Today's model total stays on the forward side, exactly as
    /// the weekly rungs hold it.
    pub rain_mm: Vec<f64>,
    /// Dated ET0 ledger rows (mm) inside the window: the replay ladder's
    /// first rung, day-MAX with provenance, fed by the self-emit.
    pub et0_ledger: Vec<(chrono::NaiveDate, f64)>,
    /// Dated provider-archive ET0 (mm) for PAST days in the window: the
    /// ladder's second rung. Today never appears here; see below.
    pub et0_archive: Vec<(chrono::NaiveDate, f64)>,
    /// Today's PARTIAL ET0 charge (mm): the day's spent portion, so the
    /// intra-day replay does not charge a full day's evaporation at
    /// dawn. The hourly curve's spent figure when the provider carries
    /// one, else the resolved full-day figure scaled by the elapsed
    /// local-day fraction. `None` when nothing resolves; the plan then
    /// charges today from the fallback rung (a bounded overcharge on an
    /// input-starved install, in the direction of watering sooner).
    pub today_partial_et0_mm: Option<f64>,
    /// Union valve-open seconds per day per zone (underscore-normalized
    /// slug), aligned to `dates`: `history::rollup::applied_per_day`
    /// over the same clustered watering evidence the weekly balance
    /// credits, so a midnight-straddling run splits at the boundary and
    /// duplicate manual + observer rows count once.
    pub applied_valve_s: HashMap<String, Vec<i64>>,
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

/// Day-granular companion to `resolve_observed_rain`: the SAME ladder
/// precedence, resolved to one mm value per covered day instead of the
/// window sum. Measured coverage (gauge/radar rows; legacy rows count as
/// measured only on station installs) wins outright, even at 0.00 in.
/// The model side chooses one WHOLE series, never a day-by-day mix
/// (which could exceed the sum rung's max()): the archive when its total
/// is at least the model-quality rows' total, else the rows, mirroring
/// `archive_past_in.max(model_rows_in)` so the series sums to the same
/// pre-cap figure the sum rung resolves. `archive_days_in` is the same
/// slice of `past_daily` the sum rung reads (the last window-minus-one
/// entries; today's model total belongs to the forward side).
fn resolve_observed_rain_days(
    days: &[crate::persistence::ObservedRainDay],
    station_present: bool,
    archive_days_in: &[f64],
) -> Vec<f64> {
    let legacy_as_gauge = station_present;
    let measured: Vec<f64> = days
        .iter()
        .filter(|d| {
            matches!(d.source.as_str(), "gauge" | "radar")
                || (legacy_as_gauge && d.source == "legacy")
        })
        .map(|d| d.observed_in * 25.4)
        .collect();
    if !measured.is_empty() {
        return measured;
    }
    // Model-quality rows: everything that is not measured coverage
    // ('model', unknown tags, and legacy rows on station-less installs),
    // the same bucketing `observed_rain_window_by_source` applies.
    let model_rows_in: Vec<f64> = days
        .iter()
        .filter(|d| {
            !matches!(d.source.as_str(), "gauge" | "radar")
                && (d.source != "legacy" || !legacy_as_gauge)
        })
        .map(|d| d.observed_in)
        .collect();
    let archive_sum: f64 = archive_days_in.iter().sum();
    let rows_sum: f64 = model_rows_in.iter().sum();
    let chosen = if archive_sum >= rows_sum {
        archive_days_in
    } else {
        model_rows_in.as_slice()
    };
    chosen.iter().map(|d| d * 25.4).collect()
}

/// `resolve_observed_rain_days` extended to KEEP DATES, for the soil
/// replay's day-aligned window (the shipped resolver strips the dates
/// its rows carry). Same coverage precedence: measured rows win
/// outright, even at 0.00 in; else ONE whole model-side series (the
/// dated provider archive vs the model-quality rows, by sum) supplies
/// the days it covers. `archive_days_in` carries real dates resolved
/// from `past_daily` epochs and must already exclude today (today's
/// model total belongs to the forward side). Returns (date, gross mm)
/// pairs; days neither series covers are simply absent, and the caller
/// treats them as dry.
fn resolve_observed_rain_days_dated(
    days: &[crate::persistence::ObservedRainDay],
    station_present: bool,
    archive_days_in: &[(chrono::NaiveDate, f64)],
) -> Vec<(chrono::NaiveDate, f64)> {
    let legacy_as_gauge = station_present;
    let measured: Vec<(chrono::NaiveDate, f64)> = days
        .iter()
        .filter(|d| {
            matches!(d.source.as_str(), "gauge" | "radar")
                || (legacy_as_gauge && d.source == "legacy")
        })
        .map(|d| (d.date, d.observed_in * 25.4))
        .collect();
    if !measured.is_empty() {
        return measured;
    }
    let model_rows_in: Vec<(chrono::NaiveDate, f64)> = days
        .iter()
        .filter(|d| {
            !matches!(d.source.as_str(), "gauge" | "radar")
                && (d.source != "legacy" || !legacy_as_gauge)
        })
        .map(|d| (d.date, d.observed_in))
        .collect();
    let archive_sum: f64 = archive_days_in.iter().map(|(_, v)| v).sum();
    let rows_sum: f64 = model_rows_in.iter().map(|(_, v)| v).sum();
    let chosen = if archive_sum >= rows_sum {
        archive_days_in
    } else {
        model_rows_in.as_slice()
    };
    chosen.iter().map(|(d, v)| (*d, v * 25.4)).collect()
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
    // ONE ledger read serves both rain figures. The day rows are
    // fetched once and the per-source window sums are reconstructed
    // from the SAME rows in memory (`ObservedRainWindow::from_days`
    // groups by source exactly as the SQL GROUP BY did, so the raw wire
    // sum is unchanged), which makes the sum rung and the day series
    // describe identical rows under one window anchor by construction.
    // Two racing reads used to let the fire-and-forget day-max upsert
    // land between them: the day series could then outgrow the raw sum
    // (crediting more rain than the wire reports), and a failed day
    // read alone silently disabled the cap for a cache period. A failed
    // read now degrades BOTH figures together to the archive rung.
    let day_rows = match obs_store {
        Some(s) => s
            .observed_rain_window_days(BALANCE_WINDOW_DAYS)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "balance ledger read failed");
                Vec::new()
            }),
        None => Vec::new(),
    };
    let win = crate::persistence::ObservedRainWindow::from_days(&day_rows);
    let fc = forecast_store.snapshot();
    // Past days only (today's model total belongs to the forward side).
    let archive_past_in = fc.past_n_day_precip_in((BALANCE_WINDOW_DAYS - 1) as usize);
    // The archive's per-day view: the same last window-minus-one entries
    // `past_n_day_precip_in` sums (past_daily is stored earliest first).
    let archive_days_in: Vec<f64> = {
        let len = fc.past_daily.len();
        let start = len.saturating_sub((BALANCE_WINDOW_DAYS - 1) as usize);
        fc.past_daily[start..]
            .iter()
            .map(|d| d.precip_sum_in)
            .collect()
    };
    let t = tempest_store.snapshot();
    let station_present = t.has_live_station || !t.station_serial.is_empty();
    let (observed_rain_mm, observed_rain_source) =
        resolve_observed_rain(&win, station_present, archive_past_in);
    let observed_rain_days_mm =
        resolve_observed_rain_days(&day_rows, station_present, &archive_days_in);
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
    // ONE runs read serves the weekly evidence and the soil replay's
    // per-day buckets: the fetch covers the wider soil window (one extra
    // day of margin so an event straddling the window start is fetched
    // and then truncated, never missed), and the weekly reduction below
    // truncates its windowed sums to the 7-day window exactly as before.
    // The wider fetch does move one weekly-surface value, declared in
    // the 1.27.0 note: `last_run_epoch` reduces over ALL fetched rows,
    // so a zone whose newest run is 8-15 days old now reports that run's
    // end instead of 0 (the truthful figure; spacing and sizing are
    // unaffected because min_interval_days is at most 7).
    //
    // A read ERROR is not an empty result: it marks the tick degraded so
    // the soil pass cannot replay applied=0 for water the system itself
    // dispatched (see `BalanceTick::runs_degraded`).
    let (run_rows, runs_degraded) = match runs_store {
        Some(rs) => {
            let fetch_days =
                BALANCE_WINDOW_DAYS.max(crate::engine::soil_schedule::RECON_WINDOW_DAYS) + 1;
            match rs.window(now - fetch_days * 86400, now + 1).await {
                Ok(rows) => (rows, false),
                Err(e) => {
                    tracing::warn!(error = %e, "balance runs window read failed; soil tick degraded");
                    (Vec::new(), true)
                }
            }
        }
        None => (Vec::new(), false),
    };
    let per_zone = build_zone_run_evidence(&run_rows, now - BALANCE_WINDOW_DAYS * 86400, now);
    let soil =
        compute_soil_tick_evidence(now, &fc, obs_store, &day_rows, station_present, &run_rows)
            .await;
    BalanceTick {
        observed_rain_mm,
        observed_rain_source,
        observed_rain_days_mm,
        bias,
        per_zone,
        soil,
        runs_degraded,
    }
}

/// Gather the soil replay's day-aligned evidence window. The rain rows
/// come from their own wider ledger read (the weekly figures keep their
/// single-read invariant untouched); the ET0 ladder's ledger rung and
/// the runs buckets ride the same stores the balance already reads.
async fn compute_soil_tick_evidence(
    now: i64,
    fc: &ForecastSnapshot,
    obs_store: Option<&crate::persistence::ForecastObservationsStore>,
    weekly_day_rows: &[crate::persistence::ObservedRainDay],
    station_present: bool,
    run_rows: &[crate::persistence::RunRow],
) -> SoilTickEvidence {
    use crate::engine::soil_schedule::RECON_WINDOW_DAYS;
    let today = crate::timeutil::now_local().date_naive();
    // The window's local days with their UTC bounds, oldest first.
    let mut dates: Vec<chrono::NaiveDate> = Vec::with_capacity(RECON_WINDOW_DAYS as usize);
    let mut frames: Vec<(i64, i64)> = Vec::with_capacity(RECON_WINDOW_DAYS as usize);
    for back in (0..RECON_WINDOW_DAYS).rev() {
        let date = today - chrono::Duration::days(back);
        if let Some((start, end)) = crate::timeutil::local_day_bounds_utc(date) {
            dates.push(date);
            frames.push((start.timestamp(), end.timestamp()));
        }
    }
    // Rain: the ledger's dated rows over the soil window. The weekly
    // 7-day rows are reused when the wider read fails or no store is
    // mounted, so the soil series can never contradict rain the weekly
    // balance credits on the shared days.
    let soil_day_rows = match obs_store {
        Some(s) => s
            .observed_rain_window_days(RECON_WINDOW_DAYS)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "soil ledger read failed; reusing the weekly rows");
                weekly_day_rows.to_vec()
            }),
        None => weekly_day_rows.to_vec(),
    };
    // Dated provider archive, past days only (today's model total belongs
    // to the forward side, the same rule every rain rung applies).
    let archive_rain_in: Vec<(chrono::NaiveDate, f64)> = fc
        .past_daily
        .iter()
        .filter_map(|d| {
            let date = crate::timeutil::local_date(d.time_epoch)?;
            (date < today).then_some((date, d.precip_sum_in))
        })
        .collect();
    let rain_by_date: HashMap<chrono::NaiveDate, f64> =
        resolve_observed_rain_days_dated(&soil_day_rows, station_present, &archive_rain_in)
            .into_iter()
            .collect();
    let rain_mm: Vec<f64> = dates
        .iter()
        .map(|d| rain_by_date.get(d).copied().unwrap_or(0.0))
        .collect();
    // The ET0 ladder's evidence rungs: dated ledger rows (the self-emit
    // plus any station/provider writer), then the dated provider archive
    // for past days.
    let et0_ledger: Vec<(chrono::NaiveDate, f64)> = match obs_store {
        Some(s) => s
            .et0_window_days(RECON_WINDOW_DAYS)
            .await
            .map(|rows| rows.into_iter().map(|r| (r.date, r.et0_mm)).collect())
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "et0 ledger read failed");
                Vec::new()
            }),
        None => Vec::new(),
    };
    let et0_archive: Vec<(chrono::NaiveDate, f64)> = fc
        .past_daily
        .iter()
        .filter_map(|d| {
            let date = crate::timeutil::local_date(d.time_epoch)?;
            (date < today && d.et0_in > 0.0).then_some((date, d.et0_in * 25.4))
        })
        .collect();
    // Today's PARTIAL charge: the provider's spent-so-far figure when the
    // hourly curve exists, else the resolved full-day figure (ledger row,
    // else today's forecast daily ET0) scaled by the elapsed local-day
    // fraction. Floored at a hair above zero so the first tick after
    // midnight reads as a ~zero charge instead of falling through to the
    // fallback rung's full-day mean.
    let today_partial_et0_mm = {
        let spent = fc.eto_spent_today_mm(now);
        if spent > 0.0 {
            Some(spent)
        } else {
            let full = et0_ledger
                .iter()
                .find(|(d, _)| *d == today)
                .map(|(_, v)| *v)
                .or_else(|| {
                    fc.daily
                        .first()
                        .filter(|d| d.et0_in > 0.0)
                        .map(|d| d.et0_in * 25.4)
                });
            let elapsed_fraction = frames
                .last()
                .map(|(start, _)| ((now - start) as f64 / 86_400.0).clamp(0.0, 1.0))
                .unwrap_or(0.0);
            full.map(|v| (v * elapsed_fraction).max(0.001))
        }
    };
    // Per-day applied buckets from the SAME clustered watering evidence
    // the weekly balance credits (one filter, one union).
    let mut segments_by_zone: HashMap<String, Vec<crate::engine::tuning::RunSegment>> =
        HashMap::new();
    for r in run_rows {
        if !crate::engine::tuning::is_watering_evidence(
            &r.source,
            &r.status,
            r.skip_reason.as_deref(),
        ) {
            continue;
        }
        let end = r
            .end_epoch
            .unwrap_or(r.start_epoch + r.duration_s.unwrap_or(0) as i64);
        segments_by_zone
            .entry(r.zone_slug.replace('-', "_"))
            .or_default()
            .push(crate::engine::tuning::RunSegment {
                start_epoch: r.start_epoch,
                end_epoch: end,
            });
    }
    let applied_valve_s: HashMap<String, Vec<i64>> = segments_by_zone
        .into_iter()
        .map(|(slug, segs)| {
            let days = crate::history::rollup::applied_per_day(&segs, &frames);
            (slug, days.into_iter().map(|d| d.valve_open_s).collect())
        })
        .collect();
    SoilTickEvidence {
        dates,
        rain_mm,
        et0_ledger,
        et0_archive,
        today_partial_et0_mm,
        applied_valve_s,
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
                        // ET0 self-emit: the day's resolved reference ET0
                        // (the same source-agnostic ladder figure the
                        // snapshot publishes) lands in the observations
                        // ledger under source 'localsky_engine', day-MAX
                        // like the rain total, so every install accrues a
                        // durable per-day ET0 record from its first tick
                        // on this build. This is the soil replay's first
                        // ladder rung: without it the replay leans on the
                        // provider archive and the per-zone fallback mean.
                        // See ledger_et0_emission for the midnight and
                        // plausibility gates; an unresolved day (snapshot
                        // eto_today_mm null) emits nothing, never a
                        // fabricated figure.
                        if let Some(et0_mm) = ledger_et0_emission(
                            &forecast_store.snapshot(),
                            snap.forecast.eto_today_mm,
                            now_epoch,
                        ) {
                            let store_handle = obs_store.clone();
                            tokio::spawn(async move {
                                if let Err(e) = store_handle
                                    .upsert_et0(today, et0_mm, "localsky_engine")
                                    .await
                                {
                                    tracing::debug!(
                                        error = %e,
                                        "et0 ledger upsert failed"
                                    );
                                }
                            });
                        }
                    }
                    if source == SnapshotSource::HomeAssistant && !inferred_plan_announced {
                        let planned: Vec<&crate::ha::snapshot::WaterBudget> = snap
                            .water_budgets
                            .iter()
                            // The same question the notice asks, asked
                            // once: a soil-governed zone waters by its own
                            // deficit, so an inferred weekly target on it
                            // is nothing to warn about.
                            .filter(|b| b.on_inferred_weekly_target() && b.today_seconds > 0)
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
    // The soil deficit's producer is the soil model's evidence replay
    // (`apply_soil_schedule`, below): it fills `bucket_mm` for every
    // zone with agronomy config once the budget rows exist. Here it
    // starts absent, and it STAYS absent for zones with no agronomy
    // (env-var installs), never a fabricated 0.0. Today's run length
    // comes from the allocator rows (weekly, or soil-swapped) on both
    // paths. heat_mult is the global forecast multiplier; capture_eff
    // starts as the soil projection's fixed constant and
    // `apply_soil_schedule` overwrites it with the configured value on
    // soil-governed zones.
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
            // Pre-plan placeholder; `apply_soil_schedule` publishes the
            // replayed deficit (negative = needs water) for every zone
            // with agronomy config. Absent, not zero, until then and on
            // zones no model can derive a bucket for.
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
                // The fixed soil-projection constant on weekly-governed
                // zones (matches compute_soil_forecasts
                // CAPTURE_EFFICIENCY); `apply_soil_schedule` overwrites
                // it with the configured engine.capture_efficiency on
                // soil-governed zones, where the refill division reads it.
                capture_eff: 0.70,
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
        // The deployment's offset right now, resolved here where the
        // configured timezone is known. Watering restrictions are a legal
        // question about the operator's wall clock, so the engine is told
        // the answer rather than reading it from the process.
        utc_offset_seconds: crate::timeutil::now_local().offset().local_minus_utc(),
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
        &watering_policy.zone_agronomy,
        {
            use chrono::Datelike;
            crate::timeutil::now_local().date_naive().ordinal() as u16
        },
        watering_policy.location.0,
        watering_policy.effective_capture_efficiency(),
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
        watering_policy.calendar,
    );
    // The soil-model pass: shadow-compute the bucket for every zone with
    // agronomy config (bucket_mm's producer, plus the water_budgets soil
    // block), and swap `today_seconds` for the zones the soil model
    // governs, BEFORE apply_budget_plan so the shared downstream
    // (seasonal dial, Override zeroing, force floor, verdict multiplier)
    // applies to both producers identically on both deployment paths.
    apply_soil_schedule(
        &mut snap,
        watering_policy,
        balance,
        &fc,
        restriction_cap_seconds,
    );
    // ONE dispatch pipe on BOTH paths: the allocator's rows (weekly, or
    // soil-swapped above) become planned seconds here. The Home
    // Assistant path used to size runs from a Smart Irrigation entity's
    // bucket instead, which is the read the 0.7.22 release deleted, so
    // both paths plan from `water_budgets`.
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

/// The soil-model pass. For EVERY zone with agronomy config, whichever
/// model governs it, replay the trailing evidence through the pure
/// planner (`engine::soil_schedule::plan_zone`) and publish the result.
/// Two evidence-quality guards hold the pass back: a window with fewer
/// than `MIN_EVIDENCE_DAYS` evidenced days
/// (`SoilZonePlan::evidence_starved`) and a tick
/// whose runs read errored (`BalanceTick::runs_degraded`) both publish
/// ABSENCE (no bucket, no soil block) and leave the weekly allocator's
/// sizing in place for governed zones, because a replay built on
/// assumption alone fabricates a deficit. Otherwise:
/// `bucket_mm` gets its producer (negative = needs water, the field's
/// documented sign since the Smart Irrigation era) and the budget row
/// gains the additive soil block, so a weekly-model install accrues
/// shadow evidence ("would water N seconds today") with its decisions
/// untouched. Every budget row is also tagged with the model that
/// governs it.
///
/// Runs between the weekly allocator and `apply_budget_plan`: when a
/// zone resolves to the soil model, this pass swaps its row's
/// `today_seconds`/`today_reason` for the soil plan's figures, and the
/// shared downstream (seasonal dial, Override zeroing, force-run floor,
/// verdict multiplier, dispatch) then applies to both producers
/// identically, one truth for display and dispatch.
fn apply_soil_schedule(
    snap: &mut IrrigationSnapshot,
    watering_policy: &WateringPolicy,
    balance: Option<&BalanceTick>,
    fc: &ForecastSnapshot,
    restriction_cap_seconds: Option<u32>,
) {
    use crate::config::schema::SchedulingModel;
    use crate::engine::soil_schedule::{
        plan_zone, resolve_et0_days, ZoneDayEvidence, ZoneSoilParams,
    };
    let budget_cfg_by_slug: HashMap<&str, &ZoneBudgetCfg> = watering_policy
        .budget_zones
        .iter()
        .map(|b| (b.slug.as_str(), b))
        .collect();
    // Bias-corrected, probability-weighted next-24h rain (mm) for the
    // defer-by-deficit gate; the capture factor applies inside the gate.
    let bias_mult = {
        use chrono::Datelike;
        let month = crate::timeutil::now_local().date_naive().month();
        balance.map(|b| b.bias.multiplier_for(month)).unwrap_or(1.0)
    };
    let expected_24h_rain_mm = fc.next_n_hours_precip_weighted_in(24) * bias_mult * 25.4;
    let empty_soil = SoilTickEvidence::default();
    let soil_ev = balance.map(|b| &b.soil).unwrap_or(&empty_soil);
    // The ET0 ladder's evidence rungs resolve once for the window; the
    // per-zone fallback rung applies inside build_replay_days.
    let et0_days = resolve_et0_days(&soil_ev.dates, &soil_ev.et0_ledger, &soil_ev.et0_archive);
    let eff = watering_policy.effective_capture_efficiency();
    let site_lat = watering_policy.location.0;
    // The engine default the per-zone tags diverge from, for the model
    // chips: a chip renders only where a zone's effective model differs
    // from this baseline.
    snap.engine_scheduling_model = match watering_policy.scheduling_model {
        SchedulingModel::Weekly => "weekly",
        SchedulingModel::Soil => "soil",
    }
    .to_string();
    // Zones the soil model GOVERNS this tick, with their plans, in the
    // active-list order (the admission sort's deterministic tie-break).
    let mut governed: Vec<(String, crate::engine::soil_schedule::SoilZonePlan)> = Vec::new();
    // A failed runs read leaves the replay blind to the water the system
    // itself dispatched: applied=0 on every day would reconstruct an
    // inflated depletion for a zone that watered yesterday and can
    // re-dispatch a full refill. The degraded tick keeps its model tags
    // but publishes no buckets and swaps no governed rows; the weekly
    // allocator's sizing stands until a clean read.
    let runs_degraded = balance.is_some_and(|b| b.runs_degraded);

    for zi in 0..snap.zones.len() {
        let slug = snap.zones[zi].slug.clone();
        let model = watering_policy.resolve_scheduling_model(&slug);
        if let Some(b) = snap.water_budgets.iter_mut().find(|b| b.zone_slug == slug) {
            b.scheduling_model = match model {
                SchedulingModel::Weekly => "weekly",
                SchedulingModel::Soil => "soil",
            }
            .to_string();
        }
        if runs_degraded {
            continue;
        }
        // No agronomy config = no texture or species to derive a bucket
        // from: the zone stays weekly-governed (resolve_scheduling_model
        // already pins it) and its soil fields stay absent.
        let Some(agr) = watering_policy.zone_agronomy.get(&slug) else {
            continue;
        };
        let rt = watering_policy
            .zone_runtime
            .get(&slug)
            .copied()
            .unwrap_or_else(ZoneRuntime::fallback);
        let max_dur = match restriction_cap_seconds {
            Some(c) => rt.max_duration_s.min(c),
            None => rt.max_duration_s,
        };
        let bz = budget_cfg_by_slug.get(slug.as_str());
        let params = ZoneSoilParams {
            slug: slug.clone(),
            species: agr.species,
            texture: agr.soil_texture,
            root_depth_mm: agr.root_depth_mm,
            mad_pct: agr.mad_pct_override,
            latitude_deg: site_lat,
            capture_efficiency: eff,
            throughput_mm_hr: rt.throughput_mm_hr,
            max_dur_s: max_dur,
            // The operator's per-day rain clip, EXPLICIT only: an
            // inferred cap is already the TAW the bucket clamp encodes.
            explicit_rain_cap_mm: bz.and_then(|b| (!b.rain_cap_inferred).then_some(b.rain_cap_mm)),
            // The weekly delivery ceiling, EXPLICIT only: an inferred
            // 1.0 in target must never starve a sandy summer.
            explicit_weekly_budget_in: bz.and_then(|b| b.weekly_budget_in),
        };
        let applied = soil_ev.applied_valve_s.get(&slug);
        let evidence: Vec<ZoneDayEvidence> = soil_ev
            .dates
            .iter()
            .enumerate()
            .map(|(i, date)| {
                // Today (the window's last day) charges its PARTIAL
                // figure so a pre-dawn plan does not bill a full day's
                // evaporation; None falls to the module's fallback rung.
                let is_today = i + 1 == soil_ev.dates.len();
                let et0_mm = if is_today {
                    soil_ev.today_partial_et0_mm
                } else {
                    et0_days[i].et0_mm
                };
                ZoneDayEvidence {
                    date: *date,
                    et0_mm,
                    gross_rain_mm: soil_ev.rain_mm.get(i).copied().unwrap_or(0.0),
                    applied_valve_s: applied.and_then(|v| v.get(i).copied()).unwrap_or(0),
                }
            })
            .collect();
        // Gross trailing delivery for the explicit weekly ceiling: the
        // same union valve seconds x throughput the weekly balance
        // credits as applied_mm.
        let delivered_7d_mm = balance
            .and_then(|b| b.per_zone.get(&slug))
            .map(|e| e.applied_open_s.max(0) as f64 / 3600.0 * rt.throughput_mm_hr)
            .unwrap_or(0.0);
        let plan = plan_zone(&params, &evidence, expected_24h_rain_mm, delivered_7d_mm);
        // An evidence-starved window (fewer than MIN_EVIDENCE_DAYS days
        // carrying an ET0 rung, a rain row, or applied seconds) replays
        // to a figure made almost purely of the fallback assumption:
        // near-full TAW within days on any texture. Publishing that
        // would fabricate a confident full deficit on a zone nothing
        // measured, so the absent-not-zero contract holds: no bucket, no
        // soil block, and a governed zone rides the weekly allocator's
        // sizing until enough rungs resolve. A live install leaves this
        // state within its first few mornings as ET0 ledger rows,
        // archive days, rain, and its own runs land.
        if plan.evidence_starved() {
            continue;
        }
        // The bucket's producer, ending the 0.7.22 "nothing computes
        // one" era: depletion published under the field's documented
        // sign (negative = needs water).
        let bucket = Some(-plan.depletion_mm);
        snap.zones[zi].bucket_mm = bucket;
        if let Some(m) = snap.zones[zi].math.as_mut() {
            m.bucket_mm = bucket;
        }
        if let Some(b) = snap.water_budgets.iter_mut().find(|b| b.zone_slug == slug) {
            b.soil_depletion_mm = Some(plan.depletion_mm);
            b.soil_taw_mm = Some(plan.taw_mm);
            b.soil_raw_mm = Some(plan.raw_mm);
            b.soil_due = plan.due;
            b.soil_planned_seconds = plan.planned_seconds;
            b.soil_deferred_reason = plan.deferred_reason.clone();
            b.soil_deferred_kind = plan.deferred_kind;
            b.soil_ceiling_binding = plan.ceiling_binding;
            // The plan's confidence signal rides the wire with the block:
            // on the first post-starvation mornings the fallback days
            // dominate and the published deficit is mostly the
            // assumed-dry rule, which the soil panel's early-estimate
            // qualifier keys on; it drops on its own as coverage lands.
            b.soil_evidence_days = plan.evidence_days;
            b.soil_fallback_days = plan.fallback_days;
        }
        if model == SchedulingModel::Soil {
            // The math panel's capture efficiency reads the value the
            // refill division actually uses on this zone. Weekly-governed
            // zones keep the fixed 0.70: their minutes never divide by
            // it, and the weekly wire bytes are pinned.
            if let Some(m) = snap.zones[zi].math.as_mut() {
                m.capture_eff = eff;
            }
            governed.push((slug, plan));
        }
    }

    // ---- The soil model GOVERNS its zones ----
    //
    // Swap each governed row's today figures for the soil plan's, then
    // fit the due set into the morning window. Everything downstream is
    // shared with the weekly rows: apply_budget_plan still applies the
    // seasonal dial, Override zeroing, and the force-run floor, the
    // verdict multiplier still scales, and the dispatcher still enforces
    // every safety verdict, so display and dispatch stay one truth.
    if governed.is_empty() {
        return;
    }
    let today_weekday: u8 = {
        use chrono::Datelike;
        crate::timeutil::now_local()
            .weekday()
            .num_days_from_sunday() as u8
    };
    let mut candidates: Vec<crate::engine::soil_schedule::AdmissionCandidate> = Vec::new();
    for (slug, plan) in &governed {
        // One formula with the demo's synthesized soil zone
        // (`soil_schedule::today_row`), so the reason strings cannot
        // drift between the live path and the screenshots.
        let cap_minutes = watering_policy
            .zone_runtime
            .get(slug)
            .map(|rt| rt.max_duration_s / 60)
            .unwrap_or(crate::config::schema::DEFAULT_MAX_RUN_MINUTES);
        let (today_seconds, today_reason, session_capped) =
            crate::engine::soil_schedule::today_row(plan, cap_minutes);
        if let Some(b) = snap.water_budgets.iter_mut().find(|b| &b.zone_slug == slug) {
            b.today_seconds = today_seconds;
            b.today_reason = today_reason;
            b.session_capped = session_capped;
        }
        // Admission candidates: due, cleared the defer and ceiling
        // holds, and not suppressed by an Override schedule today (the
        // suppressed zone would be zeroed downstream anyway; keeping it
        // out frees window for zones that will actually water).
        let override_active = crate::scheduler::manual::override_active_today(
            &watering_policy.manual_schedules,
            slug,
            today_weekday,
        );
        if plan.due && plan.deferred_reason.is_none() && today_seconds > 0 && !override_active {
            candidates.push(crate::engine::soil_schedule::AdmissionCandidate {
                slug: slug.clone(),
                depletion_mm: plan.depletion_mm,
                raw_mm: plan.raw_mm,
                planned_seconds: today_seconds,
            });
        }
    }

    // Forward-rain gates and the heat-advisory extension are INERT for
    // soil-governed zones: defer-by-deficit already prices forecast rain
    // against the deficit, and measured ET0 already carries heat, so
    // those gates would count the same signal twice. Every safety gate
    // (wind, freeze, pause, dry-run, restrictions, rain-now, already-wet,
    // the observed-rain backstop, soil saturation) still binds. The
    // rewrite runs BEFORE admission so the fixed base below is priced
    // against the verdicts dispatch will actually enforce: on a
    // forecast-rain morning the weekly siblings' allocator seconds must
    // not occupy window the skip ladder has already emptied.
    apply_soil_gate_inertness(
        snap,
        &governed.iter().map(|(s, _)| s.clone()).collect::<Vec<_>>(),
    );

    // ---- Morning-window admission ----
    //
    // The window budget is exact and single-sourced: a plan fits iff its
    // TRUE wall time (cycle-soak splits, soak gaps, interleave, per-zone
    // preambles, priced by the dispatcher's own sequence_wall_seconds)
    // fits the span from local midnight to sunrise minus 15 minutes.
    // Weekly-governed zones share the same morning, so their seconds ride
    // every hypothetical set as a fixed base, priced at what they will
    // ACTUALLY dispatch: zero when an effective skip verdict blocks the
    // zone (the dispatcher's own predicate against the post-inertness
    // snapshot), zero when an Override manual schedule covers today
    // (apply_budget_plan zeroes those downstream), and otherwise the
    // seasonal-dialed figure apply_budget_plan will produce. Pricing raw
    // allocator seconds here deferred genuinely due soil zones against a
    // window that was actually empty.
    let (lat, lon) = watering_policy.location;
    if !candidates.is_empty() && !(lat == 0.0 && lon == 0.0) {
        let candidate_slugs: std::collections::HashSet<&str> =
            candidates.iter().map(|c| c.slug.as_str()).collect();
        let hypo_zone = |slug: &str, seconds: u32| crate::ha::snapshot::ZoneState {
            slug: slug.to_string(),
            planned_run_seconds: seconds,
            ..Default::default()
        };
        // The effective per-zone run cap the dispatch arithmetic clamps
        // to: the configured maximum, tightened by an active watering
        // restriction. Base rows and candidates price against the same
        // cap.
        let effective_max_dur = |slug: &str| -> u32 {
            let max_dur = watering_policy
                .zone_runtime
                .get(slug)
                .copied()
                .unwrap_or_else(ZoneRuntime::fallback)
                .max_duration_s;
            match restriction_cap_seconds {
                Some(c) => max_dur.min(c),
                None => max_dur,
            }
        };
        let base_zones: Vec<crate::ha::snapshot::ZoneState> = snap
            .water_budgets
            .iter()
            .filter(|b| !candidate_slugs.contains(b.zone_slug.as_str()))
            .map(|b| {
                let blocked = snap
                    .zones
                    .iter()
                    .find(|z| z.slug == b.zone_slug)
                    .is_some_and(|z| {
                        crate::scheduler::smart_morning::zone_skip_verdict(snap, z).is_some()
                    });
                let suppressed = crate::scheduler::manual::override_active_today(
                    &watering_policy.manual_schedules,
                    &b.zone_slug,
                    today_weekday,
                );
                let seconds = if blocked || suppressed {
                    0
                } else {
                    seasonal_capped(
                        b.today_seconds,
                        watering_policy.seasonal_adjust_pct,
                        effective_max_dur(&b.zone_slug),
                    )
                };
                hypo_zone(&b.zone_slug, seconds)
            })
            .collect();
        let wall = |set: &[crate::engine::soil_schedule::AdmissionCandidate]| -> u64 {
            let mut hz = base_zones.clone();
            // Candidates take the same dispatch-truth pricing as the
            // base: apply_budget_plan runs a soil row's today_seconds
            // through the seasonal dial (re-clamped to the effective
            // cap) exactly as it does a weekly row's, so raw planned
            // seconds would admit a set a >100% dial then overruns, and
            // defer one a <100% dial actually fits.
            hz.extend(set.iter().map(|c| {
                hypo_zone(
                    &c.slug,
                    seasonal_capped(
                        c.planned_seconds,
                        watering_policy.seasonal_adjust_pct,
                        effective_max_dur(&c.slug),
                    ),
                )
            }));
            crate::scheduler::smart_morning::sequence_wall_seconds(
                &watering_policy.zone_agronomy,
                &hz,
                watering_policy.soak_minutes,
                watering_policy.interleave_cycles,
            )
        };
        // The morning this plan actually runs: today while today's
        // window has not passed, else tomorrow (compute_next_run_epoch's
        // date logic).
        let now_utc = chrono::Utc::now();
        // Today in the DEPLOYMENT's calendar, from the same source the
        // window math uses, so the admission window and the day it
        // belongs to can never come from different clocks.
        let today_local = (watering_policy.calendar.local_date)(now_utc.timestamp())
            .unwrap_or_else(|| now_utc.date_naive());
        let morning = match crate::engine::sunrise::smart_morning_target_start(
            today_local,
            lat,
            lon,
            wall(&candidates),
            watering_policy.calendar,
        ) {
            Some(t) if t > now_utc => today_local,
            _ => today_local.succ_opt().unwrap_or(today_local),
        };
        // A probe sequence longer than any day always clamps the start
        // at local midnight, so available_s returns exactly the
        // midnight-to-finish span: the window's true budget.
        const WINDOW_PROBE_SEQ_S: u64 = 2 * 86_400;
        if let Some(available_s) = crate::engine::sunrise::smart_morning_available_s(
            morning,
            lat,
            lon,
            WINDOW_PROBE_SEQ_S,
            watering_policy.calendar,
        ) {
            let outcome = crate::engine::soil_schedule::admit_zones(
                &candidates,
                available_s.max(0) as u64,
                wall,
            );
            for deferred in &outcome.deferred {
                if let Some(b) = snap
                    .water_budgets
                    .iter_mut()
                    .find(|b| b.zone_slug == deferred.slug)
                {
                    b.today_seconds = 0;
                    b.today_reason = deferred.reason.clone();
                    // The wire's soil block reflects the post-admission
                    // plan: nothing runs today, and the window reason is
                    // the hold.
                    b.soil_planned_seconds = 0;
                    b.soil_deferred_reason = Some(deferred.reason.clone());
                    b.soil_deferred_kind =
                        Some(crate::engine::soil_schedule::SoilDeferKind::Window);
                }
            }
        }
    }
}

/// The skip-ladder gates the soil model makes INERT for its zones, by
/// stable rule id: the three forward model-rain gates (defer-by-deficit
/// prices the same forecast against the deficit, so holding again
/// double-counts) plus the heat-advisory extension (measured ET0 already
/// charges hot days into the replay; an extension would water the same
/// heat twice). The gates catalog names this on each row. Weekly zones
/// keep all four exactly as shipped.
const SOIL_MODEL_INERT_GATES: &[&str] = &["rain_next_4h", "tomorrow_rain", "rain_3day"];

/// Rewrite the verdicts the inert gates produced for soil-governed
/// zones, after `apply_engine` back-filled them. Per zone: a
/// global-source skip whose reason_code is an inert forward-rain gate
/// becomes a run (source "soil_model", the demotion named in the
/// reason); a heat-advisory run_extended becomes a plain run. The
/// AGGREGATE mirrors the soil-floor demotion morning: when the yard-wide
/// skip is an inert gate and a soil zone rides through, `will_skip`
/// drops so the dispatcher's blanket early-return does not hold the soil
/// zones; weekly zones keep their per-zone global-source skips, which
/// the dispatcher enforces on a non-blanket morning exactly as it does
/// on a demotion morning. The decision trace keeps the raw ladder (the
/// sticky-override precedent: the trace explains the weather, the
/// verdict fields say what the yard does). The seven-day verdict strip
/// is rewritten with the same knowledge so the strip, the watering-week
/// page, and the hero narrate one decision: all-soil installs demote
/// inert-gate cells to runs, mixed installs annotate them.
fn apply_soil_gate_inertness(snap: &mut IrrigationSnapshot, governed_slugs: &[String]) {
    if governed_slugs.is_empty() {
        return;
    }
    let governed: std::collections::HashSet<&str> =
        governed_slugs.iter().map(|s| s.as_str()).collect();
    let rewrite = |v: &mut crate::ha::snapshot::ZoneVerdict| {
        if !governed.contains(v.zone_slug.as_str()) {
            return;
        }
        if v.verdict == "skip"
            && v.source == "global"
            && SOIL_MODEL_INERT_GATES.contains(&v.reason_code.as_str())
        {
            v.reason = format!(
                "Waters anyway: soil zones already count this forecast rain against \
                 their deficit. ({})",
                v.reason
            );
            v.verdict = "run".into();
            v.source = "soil_model".into();
            v.reason_code = "soil_model".into();
        } else if v.verdict == "run_extended" && v.reason_code == "heat_advisory" {
            v.reason = format!(
                "Runs normally: measured water use already charges hot days into the \
                 soil deficit. ({})",
                v.reason
            );
            v.verdict = "run".into();
            v.source = "soil_model".into();
            v.reason_code = "soil_model".into();
        }
    };
    for v in snap.zone_verdicts.iter_mut() {
        rewrite(v);
    }
    for z in snap.zones.iter_mut() {
        if let Some(v) = z.verdict.as_mut() {
            rewrite(v);
        }
    }
    // Aggregate demotion, the soil-floor morning's shape: the blanket
    // skip lifts so soil zones dispatch, and the reason says who still
    // holds.
    if snap.skip_check.will_skip
        && SOIL_MODEL_INERT_GATES.contains(&snap.skip_check.reason_code.as_str())
    {
        snap.skip_check.will_skip = false;
        snap.skip_check.verdict = "run".into();
        snap.skip_check.reason = format!(
            "{}. {}",
            snap.skip_check.reason,
            crate::ha::snapshot::MIXED_SKIP_NOTE
        );
    } else if snap.skip_check.verdict == "run_extended"
        && snap.skip_check.reason_code == "heat_advisory"
        && snap
            .zones
            .iter()
            .all(|z| governed.contains(z.slug.as_str()))
    {
        // Every zone rides the soil model: an extension nothing applies
        // would be a promise on the hero, so the yard verdict is a run.
        snap.skip_check.verdict = "run".into();
        snap.skip_check.reason = format!(
            "{}. Runs keep their planned length; measured water use already counts \
             the heat.",
            snap.skip_check.reason
        );
    }
    // The seven-day strip narrates the same ladder, computed from the
    // raw rules BEFORE this pass: on a forecast-rain morning its [0]
    // cell (and the forward cells) still showed a yard-wide Skip the
    // soil zones ride through, while the demoted skip_check said run,
    // the exact display-vs-dispatch disagreement the codebase treats as
    // a defect. All-soil install: a cell an inert gate decided demotes
    // to a run sourced soil_model (and a heat-advisory extension to a
    // plain run, matching the aggregate). Mixed install: the skip
    // stands for the weekly zones and the cell's reason gains the same
    // annotation the aggregate carries.
    let all_governed = !snap.zones.is_empty()
        && snap
            .zones
            .iter()
            .all(|z| governed.contains(z.slug.as_str()));
    for cell in snap.seven_day_verdicts.iter_mut() {
        if cell.verdict == "skip" && SOIL_MODEL_INERT_GATES.contains(&cell.reason_code.as_str()) {
            if all_governed {
                cell.reason = format!(
                    "Waters anyway: soil zones already count this forecast rain against \
                     their deficit. ({})",
                    cell.reason
                );
                cell.verdict = "run".into();
                cell.reason_code = "soil_model".into();
            } else {
                cell.reason = format!("{}. {}", cell.reason, crate::ha::snapshot::MIXED_SKIP_NOTE);
                // The same fact as data, so no surface has to find the
                // note inside the sentence to know this hold is partial.
                cell.mixed_hold = true;
            }
        } else if all_governed
            && cell.verdict == "run_extended"
            && cell.reason_code == "heat_advisory"
        {
            cell.reason = format!(
                "Runs normally: measured water use already charges hot days into the \
                 soil deficit. ({})",
                cell.reason
            );
            cell.verdict = "run".into();
            cell.reason_code = "soil_model".into();
        }
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
                    over_line: false,
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
    // The deployment's calendar, so the balance's day math is the
    // caller's answer rather than the process's zone.
    calendar: crate::engine::calendar::Calendar,
) -> Vec<WaterBudget> {
    let now_epoch = chrono::Utc::now().timestamp();
    let globals = crate::engine::BalanceGlobals {
        now_epoch,
        // The DEPLOYMENT's calendar, resolved here where the configured
        // timezone is known, so the balance's day math never depends on
        // the zone the process happens to run in.
        calendar,
        session_rain_defer_in,
        observed_rain_mm: balance.map(|b| b.observed_rain_mm).unwrap_or(0.0),
        observed_rain_source: balance
            .map(|b| b.observed_rain_source.clone())
            .unwrap_or_else(|| "none".to_string()),
        observed_rain_days_mm: balance
            .map(|b| b.observed_rain_days_mm.clone())
            .unwrap_or_default(),
        bias: balance
            .map(|b| b.bias.clone())
            .unwrap_or_else(crate::engine::BiasModel::identity),
    };

    let mut out = Vec::with_capacity(budget_zones.len());
    for zone_cfg in budget_zones.iter() {
        let slug = zone_cfg.slug.as_str();
        // Resolved at policy-build time from the zone's declared species
        // (see ZoneBudgetCfg::default_budget_in); a config-less zone's row
        // carries the name-based default because it has no species.
        let (default_budget_in, default_sessions) =
            (zone_cfg.default_budget_in, zone_cfg.default_sessions);
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
            rain_cap_mm: zone_cfg.rain_cap_mm,
            rain_cap_inferred: zone_cfg.rain_cap_inferred,
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
/// The projection's crop coefficient and root depth for one zone, from
/// the same catalogs the watering decision reads: the species' FAO-56 Kc
/// curve at today's day of year, hemisphere-shifted by site latitude,
/// and the species root depth unless the zone overrides it.
///
/// This used to guess both from the slug: a flat Kc of 1.08 for anything
/// not named shrub, garden or bed. That number belonged to no species
/// and no season, so a dormant winter lawn (Kc near 0.50) projected
/// drying about twice as fast as the engine itself expected, and a
/// southern-hemisphere yard got a northern calendar. A zone with no
/// agronomy config keeps the neutral 1.0 the rest of the assembly falls
/// back to, with the generic profile's root depth.
fn kc_depth_for(
    slug: &str,
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    today_doy: u16,
    site_lat: f64,
) -> (f64, f64) {
    match agronomy.get(slug) {
        Some(a) => {
            let kc = crate::engine::kc_at_doy_lat(a.species, today_doy, site_lat);
            let depth = a
                .root_depth_mm
                .unwrap_or_else(|| crate::engine::species_profile(a.species).root_depth_mm);
            (kc, depth)
        }
        None => (
            1.0,
            crate::agronomy::species_profile_by_slug("other").root_depth_mm,
        ),
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

#[allow(clippy::too_many_arguments)]
async fn compute_soil_forecasts(
    fc: &ForecastSnapshot,
    today: &Inputs,
    map: &HashMap<String, Value>,
    zone_cfg: &[ZoneSoilCfg],
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    today_doy: u16,
    site_lat: f64,
    capture_efficiency: f64,
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
            let (kc, depth) = kc_depth_for(&z.slug, agronomy, today_doy, site_lat);
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

    // Daily ET, mm. Resolved source-agnostically by the caller (source-reported
    // > Open-Meteo HA sensor > native compute > fallback). Today's value carries
    // across the window; heat_multiplier bumps it on heat-advisory days so a
    // 95°F+ forecast tracks realistically.
    let daily_et_mm = et0_today_mm * fc_heat_multiplier(today);

    let n_days = fc.daily.len().min(7).max(1);
    let mut out = Vec::with_capacity(zones.len());

    for z in zones.iter() {
        // Resolve this zone's live reading via its assigned sensor, with
        // the same offline guard + calibration the decision path uses,
        // then hand the projection to the ENGINE. This loop used to
        // re-implement the whole water balance inline while
        // `engine::soil_forecast::project_zone` sat beside it with unit
        // tests and no caller: two versions of one curve, and the tested
        // one was not the one anybody saw.
        let current = apply_soil_quality(resolve_soil_pct(z.sensor.as_deref(), map, history).await);
        out.push(crate::engine::project_soil_forecast(
            &crate::engine::ZoneSoilInputs {
                slug: z.slug.clone(),
                name: z.name.clone(),
                kc: z.kc,
                soil_depth_mm: z.depth,
                current_pct: current,
                target_min_pct: z.target_min,
                target_max_pct: z.target_max,
            },
            fc,
            daily_et_mm,
            capture_efficiency,
            n_days,
        ));
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

    if let Some(today_target) =
        smart_morning_target_start(today_local, lat, lon, sequence_total_s, policy.calendar)
    {
        if today_target > now.with_timezone(&chrono::Utc) {
            return today_target.timestamp();
        }
    }
    // Today's window already passed; advance to tomorrow.
    if let Some(tomorrow) = today_local.succ_opt() {
        if let Some(t) =
            smart_morning_target_start(tomorrow, lat, lon, sequence_total_s, policy.calendar)
        {
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

/// Physical ceiling on a plausible daily reference ET0 (mm). The hottest,
/// driest, windiest irrigated climates top out around 15 mm/day; values
/// above this are unit misparses (an inches figure scaled twice, a
/// misconfigured mapping) and the day-MAX upsert would pin them for the
/// whole replay window. Mirror of `RAIN_TODAY_PHYSICAL_MAX_IN`.
const ET0_TODAY_PHYSICAL_MAX_MM: f64 = 20.0;

/// What the ET0 self-emit records this tick: `Some(mm)` to upsert under
/// source 'localsky_engine', `None` to skip the tick. Follows
/// `ledger_observation`'s shape:
///
///   - no resolved figure (the snapshot's `eto_today_mm` is null): SKIP.
///     The ladder found no evidence and nothing fabricated is recorded.
///   - implausible value (non-finite, non-positive, above the physical
///     cap): SKIP with a warning, leaving the day's ledger untouched.
///   - forecast still on the previous local day (the first ticks after
///     configured-tz midnight, before the provider's daily window rolls):
///     SKIP: the resolved figure describes yesterday, and a day-max write
///     now would pin yesterday's total onto the new day's row. A missing
///     daily series carries no date to gate on and writes normally (the
///     bus-owned figure's contract is today's full-day value).
fn ledger_et0_emission(
    fc: &ForecastSnapshot,
    eto_today_mm: Option<f64>,
    now_epoch: i64,
) -> Option<f64> {
    let v = eto_today_mm?;
    if !v.is_finite() || v <= 0.0 || v > ET0_TODAY_PHYSICAL_MAX_MM {
        tracing::warn!(
            value = v,
            cap_mm = ET0_TODAY_PHYSICAL_MAX_MM,
            "implausible daily ET0; ledger self-emit skipped"
        );
        return None;
    }
    if let (Some(d0_date), Some(today)) = (
        fc.daily
            .first()
            .and_then(|d| crate::timeutil::local_date(d.time_epoch)),
        crate::timeutil::local_date(now_epoch),
    ) {
        if d0_date != today {
            // Midnight carry gate: daily[0] has not rolled onto the new
            // local day yet.
            return None;
        }
    }
    Some(v)
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
    // Beds and shrubs hold water longer than turf, so they keep the
    // higher ceiling and lower floor. The TURF pair is the schema's own
    // unset band rather than a second copy of those two numbers; the bed
    // pair has no schema home because no config field distinguishes a bed
    // here, and on this path (a zone list discovered from entity names)
    // the slug is the only signal that exists.
    fn defaults(slug: &str) -> (f64, f64) {
        use crate::config::schema::{DEFAULT_SATURATION_PCT, DEFAULT_TARGET_MIN_PCT};
        if slug.contains("shrub") || slug.contains("garden") || slug.contains("bed") {
            (85.0, 25.0)
        } else {
            (DEFAULT_SATURATION_PCT, DEFAULT_TARGET_MIN_PCT)
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
                crate::engine::calendar::Calendar::utc(),
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

    /// The scheduling-model knob rides the hot-swapped policy: the engine
    /// default and the per-zone pin both map through `from_config` (so a
    /// settings save governs the next tick, no restart), the pin wins
    /// over the default in both directions, and a zone with no agronomy
    /// config at all resolves weekly no matter what either knob says.
    #[test]
    fn scheduling_model_maps_from_config_and_resolves_per_zone() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        let zone_json = |pin: serde_json::Value| {
            serde_json::from_value::<crate::config::schema::ZoneConfig>(serde_json::json!({
                "display_name": "Z",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "controller_id": "os_main",
                "controller_station": "1",
                "scheduling_model": pin
            }))
            .unwrap()
        };
        cfg.zones
            .insert("front".into(), zone_json(serde_json::Value::Null));
        cfg.zones.insert(
            "pinned_weekly".into(),
            zone_json(serde_json::json!("weekly")),
        );
        cfg.zones
            .insert("pinned_soil".into(), zone_json(serde_json::json!("soil")));

        // Engine default weekly (an untouched config): only the explicit
        // soil pin runs the bucket.
        let policy = WateringPolicy::from_config(&cfg);
        assert_eq!(policy.scheduling_model, SchedulingModel::Weekly);
        assert_eq!(
            policy.resolve_scheduling_model("front"),
            SchedulingModel::Weekly
        );
        assert_eq!(
            policy.resolve_scheduling_model("pinned_soil"),
            SchedulingModel::Soil
        );

        // Engine default soil (a wizard install, or the operator opting
        // in): the weekly pin still holds its zone back, and a zone with
        // no config row stays weekly because the bucket has no texture to
        // derive from.
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        let policy = WateringPolicy::from_config(&cfg);
        assert_eq!(
            policy.resolve_scheduling_model("front"),
            SchedulingModel::Soil
        );
        assert_eq!(
            policy.resolve_scheduling_model("pinned_weekly"),
            SchedulingModel::Weekly
        );
        assert_eq!(
            policy.resolve_scheduling_model("no_such_zone"),
            SchedulingModel::Weekly,
            "agronomy-less zones are pinned weekly"
        );

        // The capture knob maps too, with the non-positive guard.
        assert!((policy.effective_capture_efficiency() - 0.70).abs() < 1e-9);
        cfg.engine.capture_efficiency = 0.55;
        let policy = WateringPolicy::from_config(&cfg);
        assert!((policy.effective_capture_efficiency() - 0.55).abs() < 1e-9);
        assert!(
            (WateringPolicy::default().effective_capture_efficiency() - 0.70).abs() < 1e-9,
            "the Default policy's 0.0 falls back rather than zeroing every credit"
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
            soil: SoilTickEvidence::default(),
            observed_rain_mm: 0.0,
            observed_rain_source: "none".into(),
            observed_rain_days_mm: Vec::new(),
            bias: crate::engine::BiasModel::identity(),
            per_zone,
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
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
            crate::engine::calendar::Calendar::utc(),
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
            crate::engine::calendar::Calendar::utc(),
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
            soil: SoilTickEvidence::default(),
            // 3.24" of rain on the ledger (the live acceptance figure),
            // spread over four days that each sit under the zone's
            // derived rain-credit cap (bermuda on sandy loam banks
            // 26 mm a day), so no day clips and the raw sum settles the
            // week exactly as it did before the cap existed.
            observed_rain_mm: 3.24 * 25.4,
            observed_rain_source: "gauge".into(),
            observed_rain_days_mm: vec![0.81 * 25.4; 4],
            bias: crate::engine::BiasModel::identity(),
            per_zone,
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
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

    /// The acceptance week above, storm-shaped ON PURPOSE: the same
    /// 3.24" falls as one 2.0" day plus a 1.24" day, and both overrun
    /// the zone's derived 26 mm cap (bermuda on sandy loam). The credit
    /// clips to 2 x 26 = 52 mm, which still covers the 1.0" target, and
    /// the covered sentence discloses what fell versus what counted.
    /// This pins the post-cap behavior for a soaked storm week
    /// deliberately, rather than leaving the change to the changelog.
    #[test]
    fn storm_shaped_week_clips_the_credit_and_says_so() {
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
            soil: SoilTickEvidence::default(),
            observed_rain_mm: 3.24 * 25.4,
            observed_rain_source: "gauge".into(),
            observed_rain_days_mm: vec![2.0 * 25.4, 1.24 * 25.4],
            bias: crate::engine::BiasModel::identity(),
            per_zone,
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
        )
        .remove(0);
        assert!(
            (b.observed_rain_mm - 3.24 * 25.4).abs() < 1e-9,
            "the raw sum rides the wire untouched"
        );
        assert!(
            (b.observed_rain_credited_mm - 52.0).abs() < 1e-9,
            "two clipped days credit 2 x 26 mm, got {}",
            b.observed_rain_credited_mm
        );
        assert_eq!(b.today_seconds, 0, "{}", b.today_reason);
        assert_eq!(b.seconds_per_session, 0, "the remainder is zero");
        assert_eq!(
            b.today_reason,
            "covered by rain and prior watering (3.24\" fell, 2.05\" counted: the root \
             zone holds about 1.02\" a day, the rest drains past the roots + 0.25\" \
             applied against the 1.00\" weekly target)"
        );
    }

    /// END-TO-END issue #9 with the cap never hand-set anywhere: a sand
    /// zone with 150 mm roots derives its 9.0 mm cap through
    /// `WateringPolicy::from_config`, a 1.2" storm day rides a
    /// BalanceTick, and `compute_water_budgets` clips the credit and
    /// resumes the week. The assembled path from `ZoneConfig` through
    /// `ZoneBudgetCfg.rain_cap_mm` to a clipped balance runs as one,
    /// where every other clipping test either hand-sets the cap or
    /// exercises the resolution pieces separately.
    #[test]
    fn policy_derived_sand_cap_clips_a_storm_end_to_end() {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": 1.0,
                "sessions_per_week": 2,
                "root_depth_mm": 150.0
            }))
            .unwrap(),
        );
        let policy = WateringPolicy::from_config(&cfg);
        let fc = crate::forecast::snapshot::ForecastSnapshot::default();
        let tick = BalanceTick {
            soil: SoilTickEvidence::default(),
            observed_rain_mm: 1.2 * 25.4,
            observed_rain_source: "gauge".into(),
            observed_rain_days_mm: vec![1.2 * 25.4],
            bias: crate::engine::BiasModel::identity(),
            per_zone: HashMap::new(),
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
        )
        .remove(0);
        assert!(
            (b.observed_rain_mm - 1.2 * 25.4).abs() < 1e-9,
            "the raw sum rides the wire untouched"
        );
        assert!(
            (b.observed_rain_credited_mm - 9.0).abs() < 1e-9,
            "sand at 150 mm roots banks 9.0 mm, got {}",
            b.observed_rain_credited_mm
        );
        assert!(
            (b.rain_credit_cap_mm - 9.0).abs() < 1e-9,
            "got {}",
            b.rain_credit_cap_mm
        );
        assert!(b.rain_cap_inferred, "derived from soil and roots, not set");
        // remainder = 25.4 - 9.0 = 16.4 mm across the two sessions: the
        // week RESUMES mid-week instead of planning zero for seven days.
        assert!((b.needed_mm - 16.4).abs() < 1e-9, "got {}", b.needed_mm);
        assert!(
            b.today_seconds > 0,
            "the week resumes instead of holding: {}",
            b.today_reason
        );
        assert!(b.today_reason.contains("session"), "{}", b.today_reason);
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

    /// The DECLARED 1.27.0 weekly-surface delta: the runs fetch widened
    /// to the soil window and `last_run_epoch` reduces over ALL fetched
    /// rows, so a zone whose newest run ended 9 days ago reports that
    /// run's end where the 7-day fetch read 0 (the truthful figure; no
    /// golden pin can see it because the pins feed last_run_epoch as an
    /// input). The windowed figures stay 7-day truncated: that run
    /// contributes no applied seconds and no session. A 6-day-old run
    /// populates everything, exactly as before.
    #[test]
    fn last_run_epoch_populates_beyond_the_weekly_window() {
        let now = 1_700_000_000i64;
        let w_start = now - 7 * 86_400;
        let row = |slug: &str, start: i64, dur: u32| crate::persistence::RunRow {
            id: 0,
            zone_slug: slug.into(),
            start_epoch: start,
            end_epoch: Some(start + dur as i64),
            duration_s: Some(dur),
            source: "ha_refresher".into(),
            controller_id: "c".into(),
            status: "completed".into(),
            skip_reason: None,
            et0_mm: None,
            etc_mm: None,
            applied_mm: None,
            cycle_index: None,
            cycle_count: None,
        };
        // Only a 9-day-old run: outside the weekly window, inside the
        // widened fetch.
        let stale = vec![row("front", now - 9 * 86_400, 1200)];
        let ev = build_zone_run_evidence(&stale, w_start, now);
        let front = ev.get("front").copied().unwrap();
        assert_eq!(front.applied_open_s, 0, "no applied credit past the window");
        assert_eq!(front.sessions_done, 0, "no session past the window");
        assert_eq!(
            front.last_run_epoch,
            now - 9 * 86_400 + 1200,
            "the declared delta: populated, never 0"
        );
        // A newer 6-day-old run wins the reduction and counts in full.
        let mixed = vec![
            row("front", now - 9 * 86_400, 1200),
            row("front", now - 6 * 86_400, 600),
        ];
        let ev = build_zone_run_evidence(&mixed, w_start, now);
        let front = ev.get("front").copied().unwrap();
        assert_eq!(front.applied_open_s, 600);
        assert_eq!(front.sessions_done, 1);
        assert_eq!(front.last_run_epoch, now - 6 * 86_400 + 600);
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

    /// The day-granular ladder resolves the SAME rung as the sum ladder
    /// and its series always sums to the sum rung's figure: measured
    /// coverage wins outright (even a measured-dry series of zeros), the
    /// model side picks one whole series by the same max(), and legacy
    /// rows flip sides with install class. This is the invariant the
    /// per-day rain-credit cap stands on: clipping day values can only
    /// ever shrink the credit relative to the raw wire sum, never
    /// describe different rain.
    #[test]
    fn observed_rain_day_series_sums_to_the_ladder_figure() {
        use crate::persistence::{ObservedRainDay, ObservedRainWindow};
        let day = |offset: i64, observed_in: f64, source: &str| ObservedRainDay {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
                + chrono::Duration::days(offset),
            observed_in,
            source: source.into(),
        };
        // Each case: (day rows, the matching per-source window sums,
        // station_present, per-day archive).
        let cases: Vec<(Vec<ObservedRainDay>, ObservedRainWindow, bool, Vec<f64>)> = vec![
            // Gauge coverage: one storm day and one drizzle.
            (
                vec![day(0, 1.2, "gauge"), day(3, 0.2, "gauge")],
                ObservedRainWindow {
                    gauge_in: 1.4,
                    gauge_days: 2,
                    ..Default::default()
                },
                true,
                vec![0.3, 0.3],
            ),
            // Measured-dry week: rows present, all zeros, still measured.
            (
                vec![day(0, 0.0, "gauge"), day(1, 0.0, "gauge")],
                ObservedRainWindow {
                    gauge_days: 2,
                    ..Default::default()
                },
                true,
                vec![0.8],
            ),
            // No measured coverage, archive outweighs model rows.
            (
                vec![day(2, 0.1, "model")],
                ObservedRainWindow {
                    model_in: 0.1,
                    ..Default::default()
                },
                false,
                vec![0.2, 0.3],
            ),
            // No measured coverage, legacy rows (station-less = model
            // quality) outweigh the archive.
            (
                vec![day(1, 0.4, "legacy")],
                ObservedRainWindow {
                    legacy_in: 0.4,
                    legacy_days: 3,
                    ..Default::default()
                },
                false,
                vec![0.1],
            ),
            // Nothing anywhere: an empty series for the 'none' rung.
            (Vec::new(), ObservedRainWindow::default(), false, Vec::new()),
        ];
        for (rows, win, station, archive) in cases {
            let archive_sum: f64 = archive.iter().sum();
            let (sum_mm, rung) = resolve_observed_rain(&win, station, archive_sum);
            let series = resolve_observed_rain_days(&rows, station, &archive);
            let series_sum: f64 = series.iter().sum();
            assert!(
                (series_sum - sum_mm).abs() < 1e-9,
                "rung {rung}: day series sums to {series_sum}, ladder says {sum_mm}"
            );
        }
        // Legacy rows on a STATION install are measured coverage: the
        // series is the legacy days, not the (larger) archive.
        let rows = vec![day(0, 0.4, "legacy")];
        let series = resolve_observed_rain_days(&rows, true, &[0.9]);
        assert_eq!(series.len(), 1);
        assert!((series[0] - 0.4 * 25.4).abs() < 1e-9);
    }

    /// The dated resolver is the undated ladder with its dates kept: the
    /// same coverage precedence rung for rung, each value tied to the
    /// local day the soil replay charges it on. Measured rows keep their
    /// own dates (a storm lands on the storm's day, not "somewhere in
    /// the window"), and the model side's whole-series choice carries
    /// the archive's dates.
    #[test]
    fn dated_rain_resolver_keeps_the_ladder_and_the_dates() {
        use crate::persistence::ObservedRainDay;
        let d = |offset: i64| {
            chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap() + chrono::Duration::days(offset)
        };
        let day = |offset: i64, observed_in: f64, source: &str| ObservedRainDay {
            date: d(offset),
            observed_in,
            source: source.into(),
        };
        // Measured coverage wins outright and keeps its dates.
        let rows = vec![day(2, 1.2, "gauge"), day(5, 0.2, "radar")];
        let out = resolve_observed_rain_days_dated(&rows, true, &[(d(0), 0.9)]);
        assert_eq!(
            out,
            vec![(d(2), 1.2 * 25.4), (d(5), 0.2 * 25.4)],
            "the storm stays on the storm's day"
        );
        // Model side: the archive outweighs the model rows, so the whole
        // archive series (dates included) supplies the days.
        let rows = vec![day(3, 0.1, "model")];
        let out = resolve_observed_rain_days_dated(&rows, false, &[(d(1), 0.2), (d(2), 0.3)]);
        assert_eq!(out, vec![(d(1), 0.2 * 25.4), (d(2), 0.3 * 25.4)]);
        // ...and the rows win when they carry more, keeping THEIR dates.
        let rows = vec![day(3, 0.6, "model")];
        let out = resolve_observed_rain_days_dated(&rows, false, &[(d(1), 0.2), (d(2), 0.3)]);
        assert_eq!(out, vec![(d(3), 0.6 * 25.4)]);
        // Nothing anywhere: an empty series; uncovered days read dry.
        assert_eq!(resolve_observed_rain_days_dated(&[], false, &[]), vec![]);
    }

    /// Per-zone rain-cap resolution at policy-build time: without an
    /// override the cap derives as TAW = (FC - WP) x root depth,
    /// honoring a root-depth override and falling back to the species
    /// default; the operator's `rain_credit_cap_in` beats both; and a
    /// synthesized row for a config-less zone (env-var install) takes
    /// the default-texture cap.
    #[test]
    fn rain_cap_resolves_override_over_derived_taw() {
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
                "controller_station": "1"
            }))
            .unwrap(),
        );
        // Derived: bermuda's default 200 mm roots on sandy loam
        // (FC - WP = 0.13) bank 26.0 mm a day.
        let policy = WateringPolicy::from_config(&cfg);
        let row = policy
            .budget_zones
            .iter()
            .find(|z| z.slug == "front")
            .unwrap();
        assert!(
            (row.rain_cap_mm - 26.0).abs() < 1e-9,
            "got {}",
            row.rain_cap_mm
        );
        assert!(row.rain_cap_inferred);
        // A root-depth override reshapes the derived cap: 300 mm roots
        // bank 39.0 mm.
        cfg.zones.get_mut("front").unwrap().root_depth_mm = Some(300.0);
        let policy = WateringPolicy::from_config(&cfg);
        let row = policy
            .budget_zones
            .iter()
            .find(|z| z.slug == "front")
            .unwrap();
        assert!(
            (row.rain_cap_mm - 39.0).abs() < 1e-9,
            "got {}",
            row.rain_cap_mm
        );
        // The operator's own cap beats both derivations.
        cfg.zones.get_mut("front").unwrap().rain_credit_cap_in = Some(0.25);
        let policy = WateringPolicy::from_config(&cfg);
        let row = policy
            .budget_zones
            .iter()
            .find(|z| z.slug == "front")
            .unwrap();
        assert!((row.rain_cap_mm - 0.25 * 25.4).abs() < 1e-9);
        assert!(!row.rain_cap_inferred);
        // A synthesized row has no config to derive from: sandy loam at
        // the default turf root depth = 19.5 mm, marked inferred.
        let active = vec![crate::zones::ZoneIdent::new("side", "Side")];
        let rows = budget_zones_for_active(&active, &[]);
        assert!((rows[0].rain_cap_mm - 19.5).abs() < 1e-9);
        assert!(rows[0].rain_cap_inferred);
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

    /// The ET0 self-emit's per-tick decision, `ledger_observation`'s twin:
    /// a resolved, plausible figure on a rolled-over forecast day records;
    /// an unresolved day records NOTHING (never a fabricated figure); a
    /// garbage value skips; and the first ticks after configured-tz
    /// midnight skip while daily[0] still describes yesterday, so the
    /// day-max upsert can never pin yesterday's total onto the new row.
    /// Idempotence lives in the store (day-MAX upsert, pinned by
    /// `et0_upsert_keeps_the_day_max_and_its_source`): re-emitting the
    /// same figure every 10 seconds all day converges on one row holding
    /// the day's max.
    #[test]
    fn ledger_et0_emission_gates_midnight_and_garbage() {
        let now = chrono::Utc::now().timestamp();
        let today_midnight_epoch = {
            let today = crate::timeutil::local_date(now).unwrap();
            crate::timeutil::local_day_bounds_utc(today).unwrap().0
        }
        .timestamp();
        let fc_on = |day_epoch: i64| ForecastSnapshot {
            daily: vec![crate::forecast::snapshot::DailyEntry {
                time_epoch: day_epoch,
                ..Default::default()
            }],
            ..Default::default()
        };
        // A resolved plausible figure on today's forecast day records.
        let fc = fc_on(today_midnight_epoch);
        assert_eq!(ledger_et0_emission(&fc, Some(5.4), now), Some(5.4));
        // Unresolved (the ladder found nothing): no write, no fabrication.
        assert_eq!(ledger_et0_emission(&fc, None, now), None);
        // Garbage is rejected, not clamped.
        assert_eq!(ledger_et0_emission(&fc, Some(0.0), now), None);
        assert_eq!(ledger_et0_emission(&fc, Some(-1.0), now), None);
        assert_eq!(ledger_et0_emission(&fc, Some(120.0), now), None);
        assert_eq!(ledger_et0_emission(&fc, Some(f64::NAN), now), None);
        // Midnight carry: daily[0] still on yesterday skips the tick.
        let fc_yesterday = fc_on(today_midnight_epoch - 86_400);
        assert_eq!(ledger_et0_emission(&fc_yesterday, Some(5.4), now), None);
        // No daily series at all: nothing to gate on, the bus-owned
        // figure (contract: today's full-day value) writes normally.
        let fc_empty = ForecastSnapshot::default();
        assert_eq!(ledger_et0_emission(&fc_empty, Some(5.4), now), Some(5.4));
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

    /// Golden pin: `today_seconds` flows onto `planned_run_seconds`
    /// untouched under the default policy (seasonal dial 100, no manual
    /// schedules, no force overrides) for every allocator shape the
    /// budget engine produces (covered, deferred, spaced, session,
    /// capped, clipped-rain). The soil-bucket scheduling work adds a
    /// second producer for these rows, so the weekly pass-through is
    /// pinned first and every later diff is a deliberate change.
    #[test]
    fn golden_planned_seconds_pass_through_for_the_allocator_shapes() {
        use crate::ha::snapshot::{IrrigationSnapshot, WaterBudget, ZoneMath, ZoneState};
        let zone = |slug: &str, max_dur: u32| ZoneState {
            slug: slug.into(),
            override_mode: "auto".into(),
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
                zone("covered", 14_400),
                zone("deferred", 14_400),
                zone("spaced", 14_400),
                zone("session", 14_400),
                zone("capped", 3600),
                zone("clipped", 14_400),
            ],
            water_budgets: vec![
                budget("covered", 0, false),
                budget("deferred", 0, false),
                budget("spaced", 0, false),
                // The seconds are the budget engine's own golden figures
                // (see engine::budget golden_row_* pins).
                budget("session", 4572, false),
                budget("capped", 3600, true),
                budget("clipped", 2952, false),
            ],
            ..Default::default()
        };
        apply_budget_plan(&mut snap, &WateringPolicy::default());
        let planned: Vec<(String, u32)> = snap
            .zones
            .iter()
            .map(|z| (z.slug.clone(), z.planned_run_seconds))
            .collect();
        assert_eq!(
            planned,
            vec![
                ("covered".to_string(), 0),
                ("deferred".to_string(), 0),
                ("spaced".to_string(), 0),
                ("session".to_string(), 4572),
                ("capped".to_string(), 3600),
                ("clipped".to_string(), 2952),
            ]
        );
        // The capped zone sits ON its ceiling because the allocator hit
        // it; nothing else reads as ceiling-bound.
        let bindings: Vec<bool> = snap
            .zones
            .iter()
            .map(|z| z.math.as_ref().unwrap().cap_binding)
            .collect();
        assert_eq!(bindings, vec![false, false, false, false, true, false]);
        assert_eq!(
            snap.next_run_total_minutes,
            (4572u32 + 3600 + 2952) as f64 / 60.0
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
    use super::{agronomic_budget_default, kc_depth_for, ZoneAgronomyCfg};
    use crate::config::schema::{GrassSpecies, SoilTexture, SprinklerType};
    use std::collections::HashMap;

    fn agronomy_for(
        species: GrassSpecies,
        root_override: Option<f64>,
    ) -> HashMap<String, ZoneAgronomyCfg> {
        let mut m = HashMap::new();
        m.insert(
            "back_yard".to_string(),
            ZoneAgronomyCfg {
                sprinkler_type: SprinklerType::Spray,
                precip_rate_mm_hr: None,
                soil_texture: SoilTexture::SandyLoam,
                slope_pct: 0.0,
                species,
                root_depth_mm: root_override,
                mad_pct_override: None,
                scheduling_model: None,
            },
        );
        m
    }

    /// The moisture projection reads the zone's own species curve, not a
    /// constant: a dormant midwinter lawn dries at its winter Kc, and the
    /// same day in the southern hemisphere reads as summer instead.
    /// Before this, every turf zone projected at a flat 1.08 whatever the
    /// species, the season or the hemisphere.
    #[test]
    fn the_projection_reads_species_season_and_hemisphere() {
        let ag = agronomy_for(GrassSpecies::Bermuda, None);
        // Jan 15 (doy 15) and Jul 15 (doy 196), north and south.
        let (kc_north_jan, _) = kc_depth_for("back_yard", &ag, 15, 28.5);
        let (kc_north_jul, _) = kc_depth_for("back_yard", &ag, 196, 28.5);
        let (kc_south_jan, _) = kc_depth_for("back_yard", &ag, 15, -33.9);
        let (kc_south_jul, _) = kc_depth_for("back_yard", &ag, 196, -33.9);
        assert!(
            kc_north_jan < kc_north_jul,
            "north: January is dormant, July is peak ({kc_north_jan} vs {kc_north_jul})"
        );
        assert!(
            kc_south_jan > kc_south_jul,
            "south: the seasons invert ({kc_south_jan} vs {kc_south_jul})"
        );
        // The hemispheres mirror: the north's January is the south's July.
        assert!((kc_north_jan - kc_south_jul).abs() < 1e-9);
        // Nothing here is the old flat constant.
        for kc in [kc_north_jan, kc_north_jul, kc_south_jan, kc_south_jul] {
            assert!((kc - 1.08).abs() > 1e-9, "the 1.08 constant is gone");
        }
    }

    /// Root depth follows the species profile, and a per-zone override
    /// wins, the same precedence the soil model's bucket uses.
    #[test]
    fn the_projection_depth_follows_species_then_override() {
        let (_, bermuda) = kc_depth_for(
            "back_yard",
            &agronomy_for(GrassSpecies::Bermuda, None),
            196,
            28.5,
        );
        let (_, centipede) = kc_depth_for(
            "back_yard",
            &agronomy_for(GrassSpecies::Centipede, None),
            196,
            28.5,
        );
        assert_eq!(bermuda, 200.0, "Bermuda roots deeper than the old constant");
        assert_eq!(centipede, 100.0, "Centipede is shallower");
        let (_, overridden) = kc_depth_for(
            "back_yard",
            &agronomy_for(GrassSpecies::Bermuda, Some(275.0)),
            196,
            28.5,
        );
        assert_eq!(overridden, 275.0);
    }

    /// The starting weekly target scales with the species' own peak crop
    /// coefficient against reference turf, so a planting that transpires
    /// harder starts on more water. Reference turf keeps the inch a week
    /// every extension guide recommends.
    #[test]
    fn the_starting_target_follows_the_species_curve() {
        use crate::agronomy::default_weekly_target_in;
        assert_eq!(default_weekly_target_in("st_augustine"), (1.00, 2));
        assert_eq!(default_weekly_target_in("bermuda"), (0.95, 2));
        // Vegetables transpire HARDER than turf. The old name-based guess
        // gave anything containing "garden" half an inch, so a vegetable
        // bed started on well under half the water it wants.
        assert_eq!(default_weekly_target_in("vegetable_garden"), (1.15, 2));
        // Established plantings watered deeply and infrequently.
        assert_eq!(default_weekly_target_in("ornamental_shrubs"), (0.55, 1));
        assert_eq!(default_weekly_target_in("drip_xeriscape"), (0.35, 1));
        // An unknown species takes the generic profile, not turf.
        assert_eq!(default_weekly_target_in("mystery"), (0.70, 2));
    }

    /// The zone's NAME no longer decides its water. A lawn a previous
    /// owner named for a flower bed is watered as the lawn the operator
    /// declared it to be, and a bed named for its corner of the yard is
    /// watered as a bed.
    #[test]
    fn the_starting_target_ignores_what_the_zone_is_called() {
        use crate::config::schema::{Config, GrassSpecies};
        use crate::refresher::WateringPolicy;
        let mut cfg = Config::default();
        for (slug, species) in [
            ("back_yard_shrubs", GrassSpecies::Bermuda),
            ("north_corner", GrassSpecies::OrnamentalShrubs),
        ] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": crate::engine::species_slug(species),
                    "soil_texture": "sandy_loam",
                    "sprinkler_type": "spray",
                    "controller_id": "os_main",
                    "controller_station": "1"
                }))
                .unwrap(),
            );
        }
        let policy = WateringPolicy::from_config(&cfg);
        let row = |slug: &str| {
            policy
                .budget_zones
                .iter()
                .find(|b| b.slug == slug)
                .unwrap_or_else(|| panic!("no row for {slug}"))
        };
        // Named like a bed, planted as bermuda: waters as bermuda.
        assert_eq!(row("back_yard_shrubs").default_budget_in, 0.95);
        assert_eq!(row("back_yard_shrubs").default_sessions, 2);
        // Named like a lawn, planted as shrubs: waters as shrubs.
        assert_eq!(row("north_corner").default_budget_in, 0.55);
        assert_eq!(row("north_corner").default_sessions, 1);
    }

    /// A zone with no agronomy config (an install that never ran the
    /// wizard) takes the neutral coefficient the rest of the assembly
    /// falls back to, never a slug guess.
    #[test]
    fn an_unconfigured_zone_takes_the_neutral_coefficient() {
        let empty = HashMap::new();
        for slug in [
            "back_yard",
            "back_yard_shrubs",
            "front_garden",
            "flower_bed",
        ] {
            let (kc, depth) = kc_depth_for(slug, &empty, 196, 28.5);
            assert_eq!(kc, 1.0, "{slug}");
            assert_eq!(depth, 150.0, "{slug}");
        }
    }

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
        use crate::config::schema::{Config, GrassSpecies};
        use crate::refresher::WateringPolicy;
        for species in [
            GrassSpecies::StAugustine,
            GrassSpecies::Bermuda,
            GrassSpecies::TallFescue,
            GrassSpecies::OrnamentalShrubs,
            GrassSpecies::VegetableGarden,
            GrassSpecies::DripXeriscape,
        ] {
            let slug = crate::engine::species_slug(species);
            let mut cfg = Config::default();
            cfg.zones.insert(
                "z".into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": slug,
                    "soil_texture": "sandy_loam",
                    "sprinkler_type": "spray",
                    "controller_id": "os_main",
                    "controller_station": "1"
                }))
                .unwrap(),
            );
            let row = WateringPolicy::from_config(&cfg).budget_zones.remove(0);
            assert_eq!(
                inferred_weekly_target(slug),
                (row.default_budget_in, row.default_sessions),
                "{slug}: the editor's placeholder must be the target the \
                 engine actually waters on"
            );
        }
    }

    /// The zone editor derives the rain-cap placeholder client-side (the
    /// engine's soil catalog compiles only server-side), so it carries
    /// its own copy of each texture's FC-WP spread. Pinned against
    /// `soil_catalog::taw_mm` for every texture at several root depths,
    /// or the box would promise a cap the balance is not clipping at.
    #[test]
    fn the_zone_editor_rain_cap_matches_the_soil_catalog() {
        use crate::components::settings::zones::derived_rain_cap_in;
        use crate::config::schema::SoilTexture;
        let textures = [
            ("sand", SoilTexture::Sand),
            ("loamy_sand", SoilTexture::LoamySand),
            ("sandy_loam", SoilTexture::SandyLoam),
            ("loam", SoilTexture::Loam),
            ("silt_loam", SoilTexture::SiltLoam),
            ("clay_loam", SoilTexture::ClayLoam),
            ("clay", SoilTexture::Clay),
        ];
        for (slug, texture) in textures {
            for root_mm in [100.0, 150.0, 200.0, 250.0, 300.0, 400.0] {
                let editor_mm = derived_rain_cap_in(slug, root_mm) * 25.4;
                let engine_mm = crate::engine::taw_mm(texture, root_mm);
                assert!(
                    (editor_mm - engine_mm).abs() < 1e-9,
                    "{slug} at {root_mm} mm roots: editor {editor_mm}, engine {engine_mm}"
                );
            }
        }
        // An unknown texture slug takes the sandy_loam spread, the same
        // default the form loads for an unset texture.
        assert_eq!(
            derived_rain_cap_in("mystery", 150.0),
            derived_rain_cap_in("sandy_loam", 150.0)
        );
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
        assemble_with_balance(
            forecast,
            tempest,
            zones,
            policy,
            map,
            control,
            ha_helper_reads,
            None,
        )
        .await
    }

    /// `assemble_with` plus a pre-computed BalanceTick, the shape the
    /// live loop feeds every build: what the soil-model assembly tests
    /// drive their evidence through. The zone_runtime comes from the
    /// policy (the live loop passes `watering_policy.zone_runtime`).
    #[allow(clippy::too_many_arguments)]
    async fn assemble_with_balance(
        forecast: ForecastSnapshot,
        tempest: TempestSnapshot,
        zones: &[&str],
        policy: WateringPolicy,
        map: HashMap<String, Value>,
        control: Option<&crate::persistence::IrrigationControlState>,
        ha_helper_reads: bool,
        balance: Option<&BalanceTick>,
    ) -> IrrigationSnapshot {
        let fs = forecast_store_with(forecast);
        let ts = tempest_store_with(tempest);
        let zone_runtime = policy.zone_runtime.clone();
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
            balance,
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

    // ---- The soil model's assembly (bucket-governs) ----

    /// One-zone policy on FL sand with a measured 15 mm/hr spray, the
    /// issue-#9 yard shape. `model` sets the engine default.
    fn sand_zone_policy(model: crate::config::schema::SchedulingModel) -> WateringPolicy {
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(model);
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 15.0,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1"
            }))
            .unwrap(),
        );
        WateringPolicy::from_config(&cfg).with_utc_calendar()
    }

    /// A balance tick whose soil window holds `days` trailing evidence
    /// days ending today, each with an `et0_mm` ledger row; today is
    /// charged at a small partial, the pre-dawn shape. A positive
    /// `storm_mm_yesterday` puts that gross depth on yesterday's rain.
    /// The morning window the refresher will price this yard against,
    /// read from the engine's own sunrise helpers with the refresher's
    /// own date choice. The window runs local midnight to sunrise, so
    /// its length moves with the runner's clock: the same Orlando
    /// coordinates give about 7 hours on a matching host and about 11 on
    /// a UTC container, which is enough to change which zones fit. A
    /// fixture that sizes its zones against THIS number asserts the same
    /// thing wherever it runs, and pinning the process timezone instead
    /// is not an option (`set_configured_tz` is a one-shot global that
    /// would leak into every other test in the binary).
    fn morning_window_s(lat: f64, lon: f64) -> u64 {
        // A probe sequence longer than any day clamps the start at local
        // midnight, which is what the refresher does to read the
        // window's true budget.
        const PROBE_S: u64 = 2 * 86_400;
        let now_utc = chrono::Utc::now();
        let today_local = crate::timeutil::now_local().date_naive();
        let morning = match crate::engine::sunrise::smart_morning_target_start(
            today_local,
            lat,
            lon,
            PROBE_S,
            crate::engine::calendar::Calendar::utc(),
        ) {
            Some(t) if t > now_utc => today_local,
            _ => today_local.succ_opt().unwrap_or(today_local),
        };
        crate::engine::sunrise::smart_morning_available_s(
            morning,
            lat,
            lon,
            PROBE_S,
            crate::engine::calendar::Calendar::utc(),
        )
        .unwrap_or(0)
        .max(0) as u64
    }

    fn soil_tick(days: usize, et0_mm: f64, storm_mm_yesterday: f64) -> BalanceTick {
        let today = crate::timeutil::now_local().date_naive();
        let dates: Vec<chrono::NaiveDate> = (0..days)
            .rev()
            .map(|back| today - chrono::Duration::days(back as i64))
            .collect();
        let mut rain_mm = vec![0.0; dates.len()];
        if dates.len() >= 2 && storm_mm_yesterday > 0.0 {
            let y = dates.len() - 2;
            rain_mm[y] = storm_mm_yesterday;
        }
        let et0_ledger: Vec<(chrono::NaiveDate, f64)> =
            dates.iter().map(|d| (*d, et0_mm)).collect();
        BalanceTick {
            observed_rain_mm: 0.0,
            observed_rain_source: "none".into(),
            observed_rain_days_mm: Vec::new(),
            bias: crate::engine::BiasModel::identity(),
            per_zone: HashMap::new(),
            runs_degraded: false,
            soil: SoilTickEvidence {
                dates,
                rain_mm,
                et0_ledger,
                et0_archive: Vec::new(),
                today_partial_et0_mm: Some(0.2),
                applied_valve_s: HashMap::new(),
            },
        }
    }

    /// SHADOW on a weekly install: the soil block and the bucket_mm
    /// producer populate from the evidence window while every weekly
    /// decision (planned seconds, today's reason) is byte-identical to
    /// the same build with no soil evidence at all. The bucket rides the
    /// wire under the documented sign: negative = needs water.
    #[tokio::test]
    async fn soil_shadow_populates_evidence_and_leaves_weekly_decisions() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Weekly);
        // Three dry 8 mm ledger days plus today's partial: past RAW on
        // sand at any month's Kc, clamped at TAW.
        let with_soil = soil_tick(4, 8.0, 0.0);
        let mut without_soil = with_soil.clone();
        without_soil.soil = SoilTickEvidence::default();
        let (fc, ts) = calm();
        let a = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy.clone(),
            HashMap::new(),
            None,
            false,
            Some(&with_soil),
        )
        .await;
        let (fc, ts) = calm();
        let b = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            None,
            false,
            Some(&without_soil),
        )
        .await;
        // Weekly decisions identical with or without bucket evidence.
        assert_eq!(
            a.zones[0].planned_run_seconds,
            b.zones[0].planned_run_seconds
        );
        let (ba, bb) = (&a.water_budgets[0], &b.water_budgets[0]);
        assert_eq!(ba.today_seconds, bb.today_seconds);
        assert_eq!(ba.today_reason, bb.today_reason);
        assert_eq!(
            ba.today_seconds, a.zones[0].planned_run_seconds,
            "the weekly allocator still owns the plan"
        );
        // The soil block rides in shadow on the evidence build.
        assert_eq!(ba.scheduling_model, "weekly");
        assert!(ba.soil_due, "three dry 8 mm days cross sand's RAW");
        let depletion = ba.soil_depletion_mm.unwrap();
        assert!(depletion > ba.soil_raw_mm.unwrap());
        assert!(depletion <= ba.soil_taw_mm.unwrap() + 1e-9);
        assert!(
            ba.soil_planned_seconds > 0,
            "the shadow names what it would water"
        );
        assert_eq!(ba.soil_deferred_reason, None);
        // bucket_mm's producer: the replayed deficit, negative = needs
        // water, mirrored onto the math panel.
        let bucket = a.zones[0].bucket_mm.unwrap();
        assert!(bucket < 0.0);
        assert!((bucket + depletion).abs() < 1e-9);
        assert_eq!(a.zones[0].math.as_ref().unwrap().bucket_mm, Some(bucket));
        // An EMPTY evidence window is STARVED: absence, not a
        // fabricated full-capacity (or full-deficit) figure. The soil
        // block stays off the wire until a rung resolves.
        assert_eq!(bb.soil_depletion_mm, None);
        assert!(!bb.soil_due);
        assert_eq!(bb.soil_planned_seconds, 0);
        assert_eq!(b.zones[0].bucket_mm, None);
    }

    /// When a zone resolves to the soil model, the plan IS the dispatch:
    /// the budget row's `today_seconds` carries the deficit-sized refill,
    /// the reason speaks the bucket vocabulary, and the zone's
    /// planned_run_seconds equals the row after the shared downstream, on
    /// the Home Assistant path and the native path alike. The same
    /// inputs under the weekly policy plan differently, which is the
    /// knob's hot-swap in action: same stores, same tick, the arc-swapped
    /// policy alone decides the producer.
    #[tokio::test]
    async fn soil_model_governs_today_seconds_on_both_paths() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Soil);
        let tick = soil_tick(4, 8.0, 0.0);
        for ha_helper_reads in [false, true] {
            let (fc, ts) = calm();
            let snap = assemble_with_balance(
                fc,
                ts,
                &["front"],
                policy.clone(),
                HashMap::new(),
                None,
                ha_helper_reads,
                Some(&tick),
            )
            .await;
            let b = &snap.water_budgets[0];
            assert_eq!(b.scheduling_model, "soil");
            assert!(b.soil_due);
            assert!(b.today_seconds > 0);
            assert_eq!(b.today_seconds, b.soil_planned_seconds);
            assert!(
                b.today_reason.starts_with("soil refill:"),
                "{}",
                b.today_reason
            );
            // One truth: the governed row IS the dispatch figure.
            assert_eq!(snap.zones[0].planned_run_seconds, b.today_seconds);
            // Sized to the deficit through the refill arithmetic.
            let expect = crate::engine::water_balance::refill_runtime_seconds(
                b.soil_depletion_mm.unwrap(),
                15.0,
                0.70,
                3600,
            );
            assert_eq!(b.today_seconds, expect);
        }
        // The weekly policy on the SAME tick plans from the weekly
        // allocator instead: a settings save that swaps the policy
        // changes the producer on the next build, no restart.
        let weekly = sand_zone_policy(SchedulingModel::Weekly);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            weekly,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert_eq!(b.scheduling_model, "weekly");
        assert!(
            !b.today_reason.starts_with("soil"),
            "weekly vocabulary on the weekly model: {}",
            b.today_reason
        );
    }

    /// THE ISSUE #9 YARD at assembly level: a 1.2 in storm day fills the
    /// sand bucket, the next morning HOLDS with the bucket reason (no
    /// weekly quota resumes mid-storm), and one day later depletion has
    /// crossed RAW again and the yard waters sized to the actual deficit.
    #[tokio::test]
    async fn issue_9_sand_storm_holds_then_resumes_next_morning() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Soil);
        // Storm on yesterday's ledger row: the bucket clamps at field
        // capacity and today's partial charge leaves the yard far from
        // RAW.
        let storm_morning = soil_tick(4, 10.0, 1.2 * 25.4);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy.clone(),
            HashMap::new(),
            None,
            false,
            Some(&storm_morning),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert!(!b.soil_due, "the storm filled the bucket");
        assert_eq!(b.today_seconds, 0);
        assert_eq!(snap.zones[0].planned_run_seconds, 0);
        assert!(
            b.today_reason.starts_with("soil bucket holds:"),
            "{}",
            b.today_reason
        );
        assert!(
            snap.zones[0].bucket_mm.unwrap().abs() < 0.5,
            "near field capacity after the storm"
        );
        // One day on: the storm sits two days back, yesterday charged a
        // full measured 10 mm ET0 day, and sand's small RAW is crossed.
        let mut next_morning = soil_tick(5, 10.0, 0.0);
        let storm_idx = next_morning.soil.dates.len() - 3;
        next_morning.soil.rain_mm[storm_idx] = 1.2 * 25.4;
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            None,
            false,
            Some(&next_morning),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert!(b.soil_due, "yesterday's ETc re-crossed RAW");
        assert!(b.today_seconds > 0);
        assert!(
            b.today_reason.starts_with("soil refill:"),
            "{}",
            b.today_reason
        );
        // Deficit-sized: one measured day's charge (Kc-scaled, clamped
        // by TAW), nowhere near a weekly-quota session.
        let planned = snap.zones[0].planned_run_seconds;
        assert!(
            (1700..=3300).contains(&planned),
            "refill sized to the deficit, got {planned}"
        );
    }

    /// MIXED-MODE install: the engine default is soil, one zone pins
    /// weekly. The pinned zone's weekly row is identical to an all-weekly
    /// build of the same inputs, while its sibling is soil-governed.
    #[tokio::test]
    async fn mixed_mode_pins_split_the_producers() {
        use crate::config::schema::SchedulingModel;
        let zone_json = |pin: serde_json::Value| -> crate::config::schema::ZoneConfig {
            serde_json::from_value(serde_json::json!({
                "display_name": "Z",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 15.0,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "scheduling_model": pin
            }))
            .unwrap()
        };
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        cfg.zones
            .insert("front".into(), zone_json(serde_json::Value::Null));
        cfg.zones
            .insert("back".into(), zone_json(serde_json::json!("weekly")));
        let mixed = WateringPolicy::from_config(&cfg).with_utc_calendar();
        cfg.engine.scheduling_model = Some(SchedulingModel::Weekly);
        cfg.zones.get_mut("back").unwrap().scheduling_model = None;
        cfg.zones.get_mut("front").unwrap().scheduling_model = Some(SchedulingModel::Weekly);
        let all_weekly = WateringPolicy::from_config(&cfg).with_utc_calendar();

        let tick = soil_tick(4, 8.0, 0.0);
        let (fc, ts) = calm();
        let a = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            mixed,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        let (fc, ts) = calm();
        let w = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            all_weekly,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        // Budget rows ride in config order; address them by slug.
        let row = |s: &IrrigationSnapshot, slug: &str| {
            s.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .cloned()
                .unwrap()
        };
        let (front, back) = (row(&a, "front"), row(&a, "back"));
        assert_eq!(front.scheduling_model, "soil");
        assert!(
            front.today_reason.starts_with("soil"),
            "{}",
            front.today_reason
        );
        assert_eq!(back.scheduling_model, "weekly");
        // The pinned-weekly zone decides exactly as it would on an
        // all-weekly install.
        let back_w = row(&w, "back");
        assert_eq!(back.today_seconds, back_w.today_seconds);
        assert_eq!(back.today_reason, back_w.today_reason);
        assert_eq!(
            a.zones[1].planned_run_seconds,
            w.zones[1].planned_run_seconds
        );
    }

    /// ADMISSION binds to the dispatcher's own wall pricer: two clay
    /// zones each wanting a multi-hour refill cannot both fit the
    /// midnight-to-sunrise window, so the window fits one (stress ties
    /// keep active-list order) and the other carries to tomorrow with
    /// the window reason on the row and the wire's soil block.
    #[tokio::test]
    async fn admission_defers_what_the_morning_window_cannot_fit() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        // Each zone wants the whole window, so two never fit and one
        // always does (admission seats the most stressed regardless).
        // The low precip rate keeps the cap binding: the raw refill is
        // tens of hours, far past any cap this sets.
        let cap_min = morning_window_s(28.5, -81.4) / 60;
        for slug in ["front", "back"] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "clay",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 1.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1",
                    "max_run_minutes": cap_min
                }))
                .unwrap(),
            );
        }
        let policy = WateringPolicy::from_config(&cfg).with_utc_calendar();
        // Two weeks of dry 10 mm days clamp both buckets at clay's TAW.
        let tick = soil_tick(14, 10.0, 0.0);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            policy,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        // Budget rows ride in config order; address them by slug. The
        // active list runs front-then-back, so the stress tie keeps
        // "front" first in admission.
        let row = |slug: &str| {
            snap.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .unwrap()
        };
        let (front, back) = (row("front"), row("back"));
        assert!(front.today_seconds > 0, "the most stressed zone waters");
        assert_eq!(back.today_seconds, 0, "the window cannot fit both");
        assert!(
            back.today_reason.contains("the morning window fits 1 of 2"),
            "{}",
            back.today_reason
        );
        assert_eq!(back.soil_planned_seconds, 0);
        assert_eq!(
            back.soil_deferred_reason.as_deref(),
            Some(back.today_reason.as_str())
        );
        assert_eq!(snap.zones[1].planned_run_seconds, 0);
    }

    /// ADMISSION prices the candidates themselves at dispatch truth:
    /// the seasonal dial apply_budget_plan runs every row through also
    /// prices the soil candidates in the wall. Two clay zones whose raw
    /// refills cannot share the window both fit once a 50% dial halves
    /// what actually dispatches, so neither carries to tomorrow against
    /// seconds no valve will run.
    #[tokio::test]
    async fn admission_prices_candidates_at_the_seasonal_dial() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        // Just over half the window each: the raw pair overruns it and
        // the halved pair fits, on any host clock.
        let cap_min = (morning_window_s(28.5, -81.4) as f64 * 0.55 / 60.0).round() as u64;
        for slug in ["front", "back"] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "clay",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 1.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1",
                    "max_run_minutes": cap_min
                }))
                .unwrap(),
            );
        }
        let row = |s: &IrrigationSnapshot, slug: &str| {
            s.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .cloned()
                .unwrap()
        };
        // Both buckets pinned at clay's TAW: each raw refill sits on the
        // window-relative cap, and two of those cannot share it.
        let tick = soil_tick(14, 10.0, 0.0);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            WateringPolicy::from_config(&cfg).with_utc_calendar(),
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        assert!(row(&snap, "front").today_seconds > 0);
        assert_eq!(
            row(&snap, "back").today_seconds,
            0,
            "raw refills cannot share the window"
        );
        // The 50% dial halves what dispatches, so both refills fit.
        cfg.engine.seasonal_adjust_pct = 50;
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            WateringPolicy::from_config(&cfg).with_utc_calendar(),
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        let (front, back) = (row(&snap, "front"), row(&snap, "back"));
        assert!(front.today_seconds > 0, "{}", front.today_reason);
        assert!(
            back.today_seconds > 0,
            "the dialed-down refills share the window: {}",
            back.today_reason
        );
        assert!(!back.today_reason.contains("waits for tomorrow"));
    }

    /// The admission base prices non-candidates at what they will
    /// ACTUALLY dispatch. On a forecast-rain morning the weekly
    /// sibling's allocator session is blocked by its own skip verdict
    /// (post-inertness), so its seconds must not occupy the window: two
    /// due soil zones both water instead of the second deferring against
    /// a window that is actually empty.
    #[tokio::test]
    async fn admission_ignores_weekly_seconds_a_skip_verdict_blocks() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        // Two soil-governed sand zones, both pinned at TAW by the tick.
        for slug in ["front", "back"] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "sand",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 15.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1"
                }))
                .unwrap(),
            );
        }
        // A weekly-pinned sibling whose one allocator session alone
        // fills the whole pre-sunrise window (10 in over 1 session,
        // capped at 360 min).
        cfg.zones.insert(
            "lawn".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Lawn",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 15.0,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "scheduling_model": "weekly",
                "weekly_budget_in": 10.0,
                "sessions_per_week": 1,
                "max_run_minutes": 360
            }))
            .unwrap(),
        );
        let policy = WateringPolicy::from_config(&cfg).with_utc_calendar();
        // Fourteen dry 10 mm days pin both sand buckets at TAW. The
        // DAILY tomorrow entry fires the tomorrow_rain gate while the
        // hourly series stays dry, so defer-by-deficit does not hold the
        // soil zones (the gate is inert for them, priced by the defer).
        let tick = soil_tick(14, 10.0, 0.0);
        let now = Utc::now().timestamp();
        let mut fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            hourly: vec![current_hour(72.0, 4.0, 50)],
            ..Default::default()
        };
        fc.daily = vec![
            crate::forecast::snapshot::DailyEntry {
                time_epoch: now,
                ..Default::default()
            },
            crate::forecast::snapshot::DailyEntry {
                time_epoch: now + 86_400,
                precip_sum_in: 1.0,
                precip_probability_max: None,
                ..Default::default()
            },
        ];
        let snap = assemble_with_balance(
            fc,
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front", "back", "lawn"],
            policy,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        let row = |slug: &str| {
            snap.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .unwrap()
        };
        // The weekly sibling's row still carries its full allocator
        // session (display), while its verdict says the ladder blocks it
        // at dispatch.
        let lawn = row("lawn");
        assert!(
            lawn.today_seconds > 0,
            "the weekly row keeps its allocator seconds: {}",
            lawn.today_reason
        );
        let lawn_v = snap
            .zones
            .iter()
            .find(|z| z.slug == "lawn")
            .and_then(|z| z.verdict.as_ref())
            .unwrap();
        assert_eq!(lawn_v.verdict, "skip", "the weekly sibling holds");
        // Both due soil zones are admitted: the blocked weekly seconds
        // never priced the window.
        let (front, back) = (row("front"), row("back"));
        assert!(front.today_seconds > 0, "{}", front.today_reason);
        assert!(
            back.today_seconds > 0,
            "the second soil zone is admitted, not deferred: {}",
            back.today_reason
        );
        assert!(!front.today_reason.contains("waits for tomorrow"));
        assert!(!back.today_reason.contains("waits for tomorrow"));
    }

    /// An evidence-starved window publishes ABSENCE and the weekly
    /// allocator keeps sizing a governed zone: no fabricated full-TAW
    /// bucket, no refill planned on assumption alone. The absent-
    /// not-zero contract from 0.7.22 holds for the soil block itself,
    /// both on a genuinely input-free install (no ET0 ledger, no
    /// archive, no rain rows, no applied seconds, not even today's
    /// partial) and on one whose only rung is today's self-emitted
    /// partial: a single rung over thirteen fallback days stays under
    /// the `MIN_EVIDENCE_DAYS` floor instead of flipping the window to
    /// full confidence within the first morning.
    #[tokio::test]
    async fn evidence_starved_soil_zone_rides_the_weekly_allocator() {
        use crate::config::schema::SchedulingModel;
        let starved = |mut t: BalanceTick| {
            t.soil.et0_ledger.clear();
            t.soil.et0_archive.clear();
            t.soil.today_partial_et0_mm = None;
            t
        };
        let tick = starved(soil_tick(14, 8.0, 0.0));
        let (fc, ts) = calm();
        let soil_snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            sand_zone_policy(SchedulingModel::Soil),
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        let (fc, ts) = calm();
        let weekly_snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            sand_zone_policy(SchedulingModel::Weekly),
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        let b = &soil_snap.water_budgets[0];
        assert_eq!(b.scheduling_model, "soil", "the zone stays soil-resolved");
        assert_eq!(
            b.soil_depletion_mm, None,
            "absence, not a fabricated bucket"
        );
        assert!(!b.soil_due);
        assert_eq!(b.soil_planned_seconds, 0);
        assert_eq!(soil_snap.zones[0].bucket_mm, None);
        // The weekly allocator's sizing stands until a rung resolves.
        let w = &weekly_snap.water_budgets[0];
        assert_eq!(b.today_seconds, w.today_seconds);
        assert_eq!(b.today_reason, w.today_reason);
        assert!(
            !b.today_reason.starts_with("soil"),
            "weekly vocabulary while starved: {}",
            b.today_reason
        );
        // Today's partial alone (one evidenced day, thirteen fallback)
        // stays under the MIN_EVIDENCE_DAYS floor: same posture.
        let thin = |mut t: BalanceTick| {
            t.soil.et0_ledger.clear();
            t.soil.et0_archive.clear();
            t
        };
        let tick = thin(soil_tick(14, 8.0, 0.0));
        let (fc, ts) = calm();
        let thin_snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            sand_zone_policy(SchedulingModel::Soil),
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        let t = &thin_snap.water_budgets[0];
        assert_eq!(
            t.soil_depletion_mm, None,
            "one rung must not buy full confidence"
        );
        assert_eq!(t.soil_planned_seconds, 0);
        assert_eq!(thin_snap.zones[0].bucket_mm, None);
        assert_eq!(t.today_seconds, w.today_seconds);
        assert_eq!(t.today_reason, w.today_reason);
    }

    /// A runs-store read error the tick after a dispatched run must not
    /// replay applied=0 into an inflated re-dispatch: the degraded tick
    /// publishes no bucket and the governed swap stands down, so a zone
    /// that watered yesterday is not refilled again on blind evidence.
    #[tokio::test]
    async fn degraded_runs_read_never_inflates_a_governed_refill() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Soil);
        // Clean tick: yesterday's two-hour run rides the evidence, the
        // bucket clamps at field capacity, and the zone reads not due.
        let mut clean = soil_tick(4, 8.0, 0.0);
        clean
            .soil
            .applied_valve_s
            .insert("front".into(), vec![0, 0, 7200, 0]);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy.clone(),
            HashMap::new(),
            None,
            false,
            Some(&clean),
        )
        .await;
        assert_eq!(
            snap.water_budgets[0].today_seconds, 0,
            "watered yesterday, not due: {}",
            snap.water_budgets[0].today_reason
        );
        // The same tick with the runs read errored: the applied column
        // is blind. Without the degraded mark the replay reconstructs a
        // deep depletion and re-dispatches a refill for water already on
        // the ground.
        let mut degraded = clean.clone();
        degraded.soil.applied_valve_s.clear();
        degraded.runs_degraded = true;
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            None,
            false,
            Some(&degraded),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert_eq!(
            b.soil_depletion_mm, None,
            "no bucket published on a degraded tick"
        );
        assert_eq!(snap.zones[0].bucket_mm, None);
        assert!(
            !b.today_reason.starts_with("soil refill"),
            "no soil refill dispatched on blind evidence: {}",
            b.today_reason
        );
        assert_eq!(b.scheduling_model, "soil", "the model tag survives");
    }

    /// The forward-rain gates are INERT for soil-governed zones, at both
    /// layers the ladder acts on: the yard-wide blanket lifts (the
    /// soil-floor demotion morning's shape) and the per-zone verdict
    /// reads run with the demotion named, while a weekly sibling keeps
    /// its skip and a SAFETY gate (wind) still binds everything.
    #[tokio::test]
    async fn forward_rain_gates_are_inert_for_soil_zones_only() {
        use crate::config::schema::SchedulingModel;
        let rainy_tomorrow = |now: i64| {
            let mut fc = ForecastSnapshot {
                last_refresh_epoch: now,
                source_reachable: true,
                hourly: vec![current_hour(72.0, 4.0, 50)],
                ..Default::default()
            };
            fc.daily = vec![
                crate::forecast::snapshot::DailyEntry {
                    time_epoch: now,
                    ..Default::default()
                },
                crate::forecast::snapshot::DailyEntry {
                    time_epoch: now + 86_400,
                    precip_sum_in: 1.0,
                    precip_probability_max: None,
                    ..Default::default()
                },
            ];
            fc
        };
        let now = Utc::now().timestamp();
        let tick = soil_tick(4, 8.0, 0.0);
        // Sanity: the same morning under the weekly model is a blanket
        // tomorrow-rain skip.
        let weekly = sand_zone_policy(SchedulingModel::Weekly);
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front"],
            weekly,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        assert!(snap.skip_check.will_skip);
        assert_eq!(snap.skip_check.reason_code, "tomorrow_rain");
        // All-soil: the blanket lifts, the zone runs, and the reason
        // names the demotion.
        let soil = sand_zone_policy(SchedulingModel::Soil);
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front"],
            soil.clone(),
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        assert!(!snap.skip_check.will_skip);
        assert_eq!(snap.skip_check.verdict, "run");
        assert!(
            snap.skip_check
                .reason
                .contains("Soil-model zones already count this rain"),
            "{}",
            snap.skip_check.reason
        );
        let v = snap.zones[0].verdict.as_ref().unwrap();
        assert_eq!(v.verdict, "run");
        assert_eq!(v.source, "soil_model");
        assert!(
            snap.zones[0].planned_run_seconds > 0,
            "the soil zone waters"
        );
        // Mixed: the weekly sibling keeps its global-source skip, which
        // the dispatcher enforces per zone on a non-blanket morning.
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        for (slug, pin) in [
            ("front", serde_json::Value::Null),
            ("back", serde_json::json!("weekly")),
        ] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "sand",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 15.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1",
                    "scheduling_model": pin
                }))
                .unwrap(),
            );
        }
        let mixed = WateringPolicy::from_config(&cfg).with_utc_calendar();
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front", "back"],
            mixed,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        assert!(
            !snap.skip_check.will_skip,
            "the blanket lifts for the soil zone"
        );
        let front_v = snap.zones[0].verdict.as_ref().unwrap();
        assert_eq!(front_v.verdict, "run");
        assert_eq!(front_v.source, "soil_model");
        let back_v = snap.zones[1].verdict.as_ref().unwrap();
        assert_eq!(back_v.verdict, "skip", "the weekly sibling still holds");
        assert_eq!(back_v.source, "global");
        // A SAFETY gate binds soil zones exactly as before: hard wind
        // now is not in the inert set.
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 30.0, 50.0, 0.0),
            &["front"],
            soil,
            HashMap::new(),
            None,
            false,
            Some(&tick),
        )
        .await;
        assert!(snap.skip_check.will_skip, "wind still skips the yard");
        assert_eq!(
            snap.zones[0].verdict.as_ref().unwrap().verdict,
            "skip",
            "no inertness for safety gates"
        );
    }

    /// The inertness rewrite's mechanics, isolated: only the inert gate
    /// ids move, only governed zones move, a heat-advisory extension
    /// downgrades to a plain run (nothing scales a soil refill for
    /// forward heat), and a condition-rule run_extended is untouched.
    #[test]
    fn soil_gate_inertness_rewrites_only_the_inert_gates() {
        use crate::ha::snapshot::ZoneVerdict;
        let verdict = |slug: &str, v: &str, source: &str, code: &str| ZoneVerdict {
            zone_slug: slug.into(),
            zone_name: slug.into(),
            verdict: v.into(),
            reason: "Rain expected tomorrow".into(),
            source: source.into(),
            multiplier: 1.0,
            reason_code: code.into(),
            value: None,
            threshold: None,
        };
        let zone = |slug: &str, v: &ZoneVerdict| crate::ha::snapshot::ZoneState {
            slug: slug.into(),
            verdict: Some(v.clone()),
            ..Default::default()
        };
        let mut snap = IrrigationSnapshot::default();
        let fv = verdict("front", "skip", "global", "tomorrow_rain");
        let bv = verdict("back", "skip", "global", "tomorrow_rain");
        snap.zones = vec![zone("front", &fv), zone("back", &bv)];
        snap.zone_verdicts = vec![fv, bv];
        snap.skip_check.will_skip = true;
        snap.skip_check.verdict = "skip".into();
        snap.skip_check.reason = "Rain expected tomorrow".into();
        snap.skip_check.reason_code = "tomorrow_rain".into();
        apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "run");
        assert_eq!(snap.zone_verdicts[0].source, "soil_model");
        assert_eq!(
            snap.zones[0].verdict.as_ref().unwrap().verdict,
            "run",
            "the back-filled copy moves with the list"
        );
        assert_eq!(snap.zone_verdicts[1].verdict, "skip", "weekly zone holds");
        assert_eq!(snap.zone_verdicts[1].source, "global");
        assert!(!snap.skip_check.will_skip);
        assert_eq!(snap.skip_check.verdict, "run");
        // A non-inert gate is untouched even for governed zones.
        let wv = verdict("front", "skip", "global", "wind_now");
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front", &wv)];
        snap.zone_verdicts = vec![wv];
        snap.skip_check.will_skip = true;
        snap.skip_check.verdict = "skip".into();
        snap.skip_check.reason_code = "wind_now".into();
        apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "skip");
        assert!(snap.skip_check.will_skip, "safety gates never demote");
        // Heat advisory: run_extended downgrades to run for a governed
        // zone (measured ET0 already carried the heat), and the yard
        // verdict follows when EVERY zone is governed.
        let hv = verdict("front", "run_extended", "global", "heat_advisory");
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front", &hv)];
        snap.zone_verdicts = vec![hv];
        snap.skip_check.verdict = "run_extended".into();
        snap.skip_check.reason = "Heat advisory".into();
        snap.skip_check.reason_code = "heat_advisory".into();
        apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "run");
        assert_eq!(snap.skip_check.verdict, "run");
        // A condition-rule extension is not the heat gate: untouched.
        let cv = verdict("front", "run_extended", "condition", "condition");
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front", &cv)];
        snap.zone_verdicts = vec![cv];
        apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "run_extended");
    }

    /// The seven-day strip narrates the same decision as the demoted
    /// aggregate. All-soil install: inert-gate cells (today AND the
    /// forward cells) demote to runs sourced soil_model, a heat
    /// extension cell downgrades to a plain run, and a safety-gate cell
    /// is untouched. Mixed install: the skip stands for the weekly
    /// zones with the weekly-only annotation the aggregate carries.
    #[test]
    fn verdict_strip_cells_follow_the_gate_inertness() {
        let cell = |off: u32, v: &str, code: &str| crate::ha::snapshot::DayVerdict {
            day_offset: off,
            verdict: v.into(),
            reason: "Rain expected".into(),
            reason_code: code.into(),
            ..Default::default()
        };
        let zone = |slug: &str| crate::ha::snapshot::ZoneState {
            slug: slug.into(),
            ..Default::default()
        };
        let strip = || {
            vec![
                cell(0, "skip", "tomorrow_rain"),
                cell(1, "skip", "rain_3day"),
                cell(2, "skip", "wind_now"),
                cell(3, "run_extended", "heat_advisory"),
                cell(4, "run", "run"),
            ]
        };
        // All-soil: demotion, agreeing with the demoted skip_check.
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front")];
        snap.seven_day_verdicts = strip();
        snap.skip_check.will_skip = true;
        snap.skip_check.verdict = "skip".into();
        snap.skip_check.reason = "Rain expected".into();
        snap.skip_check.reason_code = "tomorrow_rain".into();
        apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert!(!snap.skip_check.will_skip);
        let cells = &snap.seven_day_verdicts;
        assert_eq!(
            cells[0].verdict, snap.skip_check.verdict,
            "the [0] cell agrees with the demoted skip_check"
        );
        assert_eq!(cells[0].reason_code, "soil_model");
        assert!(
            cells[0].reason.starts_with("Waters anyway:"),
            "{}",
            cells[0].reason
        );
        assert_eq!(cells[1].verdict, "run", "forward inert cells demote too");
        assert_eq!(cells[2].verdict, "skip", "safety gates never demote");
        assert_eq!(cells[2].reason_code, "wind_now");
        assert_eq!(cells[3].verdict, "run", "heat extension downgrades");
        assert_eq!(cells[4].verdict, "run");
        assert_eq!(cells[4].reason, "Rain expected", "plain runs untouched");
        // Mixed: the skip stands, annotated for the weekly zones.
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front"), zone("back")];
        snap.seven_day_verdicts = strip();
        apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        let cells = &snap.seven_day_verdicts;
        assert_eq!(cells[0].verdict, "skip");
        assert!(
            cells[0]
                .reason
                .ends_with(crate::ha::snapshot::MIXED_SKIP_NOTE),
            "{}",
            cells[0].reason
        );
        assert_eq!(
            cells[3].verdict, "run_extended",
            "mixed keeps the extension"
        );
        // No governed zones: nothing moves at all.
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front")];
        snap.seven_day_verdicts = strip();
        apply_soil_gate_inertness(&mut snap, &[]);
        assert_eq!(snap.seven_day_verdicts, strip());
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
        let policy = WateringPolicy::from_config(&cfg).with_utc_calendar();
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
