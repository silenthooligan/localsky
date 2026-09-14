// The watering policy: everything the config says about how this yard
// waters, resolved once per config load and hot-swapped by the runtime.
// Read by the assembly on every pass and by the schedulers when they
// dispatch. No I/O, no clock.

use super::*;
use crate::assembly::*;
use std::collections::HashMap;

/// Which builder fills the IrrigationSnapshot store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotSource {
    /// Poll Home Assistant `/api/states` (the legacy path).
    HomeAssistant,
    /// Build natively from local stores + controllers + the engine (no HA).
    Native,
}

/// Decide whether to source the snapshot from HA or natively:
/// `deployment.mode` in the config is the knob. `Auto` picks native
/// unless HA is configured in the environment, so an existing HA deploy
/// is unaffected by default.
pub fn resolve_snapshot_source(mode: crate::config::schema::DeploymentMode) -> SnapshotSource {
    use crate::config::schema::DeploymentMode;
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
pub use crate::engine::agronomy_cfg::ZoneAgronomyCfg;

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
    /// Site elevation in metres, for the atmospheric-pressure term in
    /// FAO-56 Penman-Monteith. Collected by the wizard and, until now,
    /// discarded at the point of use.
    pub elevation_m: f64,
    /// Per-zone soil config resolved from localsky.toml zones. Each zone's
    /// assigned sensor (`ha:` entity or `source:<id>:<key>` channel) +
    /// per-zone thresholds. Empty = no config (fall back to the legacy
    /// hardcoded soil reads).
    pub soil_zones: Vec<ZoneSoilCfg>,
    /// User-defined structured trigger rules (augment-only), from
    /// `config.conditions.rules`. Empty = none.
    pub condition_rules: Vec<crate::engine::conditions::ConditionRule>,
    /// Enabled owner scripts require a fresh engine verdict before scheduled
    /// watering, even when that schedule has a standing weather waiver.
    pub script_rules_enabled: bool,
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
    /// The controller each zone is bound to (`ZoneConfig::controller_id`),
    /// by normalized slug. Every dispatch path resolves through
    /// `controller_id_for`, so a zone bound to a second controller is
    /// watered by that controller and not by the default's matching
    /// station number.
    pub zone_controller: std::collections::HashMap<String, String>,
    /// The smallest run the default controller can be told, in seconds:
    /// 60 for the cloud adapters that take whole minutes, else 1. The
    /// sequence planner rounds every segment to it so the length on
    /// screen is the length the valves take. Derived from the configured
    /// default controller's kind; the dispatcher asks the live adapter.
    pub duration_quantum_s: u32,
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
    /// The deployment's IANA timezone name (configured, else inferred
    /// from the location), the one the snapshot carries for the client's
    /// local formatting. None when neither is known.
    pub timezone_name: Option<String>,
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
    /// config, and a rollback unions the pre-rollback ledger back in. The
    /// reads it once gated are gone (0.9.0); the record still tells the
    /// owner which helpers they can delete.
    pub ha_adoption: Vec<crate::model::HaAdoptedHelper>,
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
    /// Carry the ledger's records (the 0.7.22 migration record the notice
    /// lists) onto a policy built from the document.
    pub fn with_ledger(mut self, ledger: &crate::config::ledger::Ledger) -> Self {
        self.ha_adoption = ledger.ha_adoption.clone();
        self
    }

    /// The ledger records this policy carries, for a rebuild that has the
    /// live policy in hand but not the store.
    pub fn ledger_view(&self) -> crate::config::ledger::Ledger {
        crate::config::ledger::Ledger {
            ha_adoption: self.ha_adoption.clone(),
            ..Default::default()
        }
    }
}

/// The per-zone maps a `WateringPolicy` carries, built together.
///
/// Grouped so one iteration over the operator's zones produces all of
/// them, and so the slug is normalized exactly once by `ZoneSlug` rather
/// than by a hand-written replace at each site.
pub(crate) struct PerZone {
    soil: Vec<ZoneSoilCfg>,
    budget: Vec<ZoneBudgetCfg>,
    runtime: std::collections::HashMap<String, ZoneRuntime>,
    agronomy: std::collections::HashMap<String, ZoneAgronomyCfg>,
    /// The controller each zone is bound to, by normalized slug. Empty
    /// string = unbound, which every dispatch path resolves to the
    /// default controller.
    controller: std::collections::HashMap<String, String>,
}

pub(crate) fn build_per_zone(cfg: &crate::config::schema::Config) -> PerZone {
    let n = cfg.zones.len();
    let mut out = PerZone {
        soil: Vec::with_capacity(n),
        budget: Vec::with_capacity(n),
        runtime: std::collections::HashMap::with_capacity(n),
        agronomy: std::collections::HashMap::with_capacity(n),
        controller: std::collections::HashMap::with_capacity(n),
    };
    for (raw_slug, z) in cfg.zones.iter() {
        // One normalization, by the type that owns the rule.
        let key = crate::engine::ZoneSlug::new(raw_slug).into_string();

        out.controller
            .insert(key.clone(), z.controller_id.trim().to_string());
        out.soil.push(ZoneSoilCfg {
            slug: key.clone(),
            name: z.display_name.clone(),
            soil_sensor_id: z.soil_sensor_id.clone(),
            saturation_pct: z.saturation_pct_soil,
            target_min_pct: z.target_min_pct_soil,
            sprinkler_type: z.sprinkler_type,
        });

        // Per-day rain-credit cap: the operator's override when set, else
        // the root zone's own capacity, with the root depth resolved the
        // way the tuning engine resolves it (explicit override, else the
        // species default).
        let root_depth_mm = z
            .root_depth_mm
            .unwrap_or_else(|| crate::engine::species_profile(z.species).root_depth_mm);
        let rain_cap_mm = match z.rain_credit_cap_in {
            Some(v) => crate::units::in_to_mm(v),
            None => crate::engine::taw_mm(z.soil_texture, root_depth_mm),
        };
        // Starting target for a zone with no explicit one, from the
        // SPECIES the operator declared rather than from words in the
        // zone's name.
        let (default_budget_in, default_sessions) =
            crate::agronomy::default_weekly_target_in(crate::engine::species_slug(z.species));
        out.budget.push(ZoneBudgetCfg {
            slug: key.clone(),
            name: z.display_name.clone(),
            weekly_budget_in: z.weekly_budget_in,
            sessions_per_week: z.sessions_per_week,
            rain_cap_mm,
            rain_cap_inferred: z.rain_credit_cap_in.is_none(),
            default_budget_in,
            default_sessions,
        });

        out.runtime.insert(
            key.clone(),
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
        );

        out.agronomy.insert(
            key,
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
        );
    }
    out
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
        // Every per-zone map, built together in ONE pass.
        //
        // These were four independent iterations over cfg.zones, each
        // normalizing the slug itself with its own replace(). Four passes
        // is four chances for membership to differ, and a zone present in
        // three maps and missing from the fourth does not fail loudly: the
        // lookup misses and falls through to a default, so the zone waters
        // on catalog agronomy instead of its own. Built together, a zone is
        // in all four or in none.
        let per_zone = build_per_zone(cfg);
        WateringPolicy {
            restrictions: cfg.engine.watering_restrictions.clone(),
            address_parity: cfg.deployment.address_parity,
            manual_schedules: cfg.manual_schedules.clone(),
            location: (cfg.deployment.location.lat, cfg.deployment.location.lon),
            elevation_m: cfg.deployment.location.elevation_m.unwrap_or(0.0),
            // Per-zone soil config: each zone's assigned sensor + thresholds.
            // Slugs underscore-normalized to match the refresher's zone list.
            soil_zones: per_zone.soil,
            condition_rules: cfg.conditions.rules.clone(),
            script_rules_enabled: cfg.scripting.skip_rules.iter().any(|rule| rule.enabled),
            skip_rules: cfg.engine.skip_rules.clone(),
            // Per-zone weekly-budget config for the standalone allocator (A5b).
            // Slugs underscore-normalized to match the refresher's zone list,
            // same as soil_zones above.
            budget_zones: per_zone.budget,
            calendar: crate::timeutil::deployment_calendar(),
            timezone_name: crate::timeutil::resolve_tz(cfg).map(|tz| tz.name().to_string()),
            // From the ledger, not the document: `with_ledger` fills it.
            ha_adoption: Vec::new(),
            scheduling_model: cfg.engine.effective_scheduling_model(),
            capture_efficiency: cfg.engine.capture_efficiency,
            ha_sprinkler_prefix: cfg.deployment.ha_sprinkler_prefix.clone(),
            seasonal_adjust_pct: cfg.engine.seasonal_adjust_pct,
            units: cfg.deployment.units,
            soak_minutes: cfg.engine.soak_minutes,
            interleave_cycles: cfg.engine.interleave_cycles,
            duration_quantum_s: crate::controllers::default_duration_quantum_s(cfg),
            session_rain_defer_in: cfg.engine.session_rain_defer_in,
            // Per-zone run sizing + cycle/soak agronomy. Underscore-normalized
            // like soil_zones/budget_zones so runtime slug lookups hit. The
            // cap comes from the zone's configured max_run_minutes; unset
            // resolves to the historical 60 minute boot value.
            zone_runtime: per_zone.runtime,
            zone_agronomy: per_zone.agronomy,
            zone_controller: per_zone.controller,
        }
    }

    /// The controller id a zone is bound to, or None when unbound (or
    /// unknown to this policy), which the registry resolves to the
    /// default. Slug normalized the same way the map's keys were.
    pub fn controller_id_for(&self, slug: &str) -> Option<&str> {
        self.zone_controller
            .get(crate::engine::ZoneSlug::new(slug).as_str())
            .map(String::as_str)
            .filter(|id| !id.is_empty())
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

/// Resolve the HA controller entity prefix, falling back to a sensible
/// default when unset (the WateringPolicy::default / env-compat path).
pub(crate) fn sprinkler_prefix(policy: &WateringPolicy) -> &str {
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
/// zones come from `cfg.zones`, while
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
    /// The zone's head, carried to the engine so a restriction that
    /// exempts drip or bubbler irrigation can stand aside for it.
    pub sprinkler_type: crate::config::schema::SprinklerType,
}

/// Offline guard for a raw soil reading: a value outside the physical band
/// (exactly 0% / negative, or above SOIL_PCT_PHYSICAL_MAX) is a
/// disconnected/faulty probe (e.g. a WH51 out of soil, or a garbage
/// over-range frame), NOT bone-dry or super-saturated soil, return None so
/// the configured zone's data-integrity hold applies without claiming dry or
/// saturated ground. Real soil is essentially never
/// exactly 0.00% and can never exceed 100%. (Soil calibration itself lives
/// at the source, see `parse_soilad`'s native AD-based dry/wet calibration
/// in the Ecowitt poll adapter.)
pub(crate) fn apply_soil_quality(raw: Option<f64>) -> Option<f64> {
    raw.filter(|v| *v > 0.0 && *v <= SOIL_PCT_PHYSICAL_MAX)
}
