// Typed irrigation snapshot. One immutable struct rebuilt every refresh
// cycle and atomically swapped into the store. Serialized to JSON for
// the /api/irrigation/snapshot endpoint and the SSE stream, mirrors the
// tempest::Snapshot pattern exactly.

use serde::{Deserialize, Serialize};

/// Per-zone state. Fields named so the JSON keys read naturally on the
/// browser side (`zone.running`, `zone.today_run_minutes`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneState {
    /// Display name from the zone's config (the Home Assistant path reads
    /// the entity's own name override). Falls back to the slug.
    pub name: String,
    /// The zone's slug: the config key, and the suffix of the controller's
    /// entity ids on the Home Assistant path.
    pub slug: String,
    /// Sticky per-zone override: "auto" | "skip" | "run". "auto" follows the
    /// global override / engine; "skip"/"run" force this one zone (beating the
    /// global). Persisted in LocalSky sqlite (zone_overrides table). The zone
    /// card renders its Auto/Skip/Force control from this.
    #[serde(default = "default_auto_override")]
    pub override_mode: String,
    /// DEPRECATED (0.9.0): always "". The last six hex characters of the
    /// original deployment's OpenSprinkler MAC, which its entity ids
    /// carried; nothing has produced it since the native path landed.
    /// v2 drops it; there is no replacement.
    pub hex: String,
    /// True when the matching `binary_sensor.<prefix>_<slug>_station_running`
    /// is `on`.
    pub running: bool,
    /// Whether `running` is a trusted readback. HA must report an explicit
    /// on/off state; missing, unknown and unavailable entities stay unknown.
    /// Native controllers need valid, recent station evidence. MQTT without
    /// an interpretable message on its current connection also stays unknown.
    #[serde(default = "default_true_running_known")]
    pub running_known: bool,
    /// When the controller actually observed `running`, when it knows.
    /// `None` means the reading was taken on demand this pass, so the
    /// snapshot's own refresh time is its time.
    ///
    /// A cloud controller is read on the interval it declares, not on
    /// every tick, so a `running` flag can be up to that interval old.
    /// The run-edge observer credits water against this rather than the
    /// tick it happened to see the flag on. Additive; absent on older
    /// snapshots.
    #[serde(default)]
    pub running_observed_epoch: Option<i64>,
    /// LocalSky commanded this zone on and has not yet commanded it off:
    /// a shutoff deadline is armed for it in the active-run ledger. Set
    /// for every zone on the native path; it is what makes a run on a
    /// controller that cannot report state visible and stoppable instead
    /// of invisible. Additive; absent on older snapshots.
    #[serde(default)]
    pub ledger_running: bool,
    /// The controller that reported this zone's state, when one did.
    /// The run-edge observer labels its rows with it instead of the
    /// historical 'ha_service_call' placeholder. Additive.
    #[serde(default)]
    pub controller_id: Option<String>,
    /// The zone's head throughput, mm/hr, so a run's applied depth can
    /// be written when the row is. Additive.
    #[serde(default)]
    pub throughput_mm_hr: Option<f64>,
    /// Today's accumulated run-minutes for this zone. `None` on every
    /// install: no producer summarizes per-zone valve-open seconds since
    /// local midnight, on either deployment path. It was a bare `f64`
    /// holding a hardcoded 0.0, which published "0 min" as if it were a
    /// measurement while the zone card's hold line above it named the
    /// inches already applied this week. Same treatment `bucket_mm` and
    /// `water_level_pct` got: serialized null, every display renders a
    /// dash, and the manifest publishes no descriptor.
    /// `#[serde(default)]` so older JSON deserializes.
    #[serde(default)]
    pub today_run_minutes: Option<f64>,
    /// Soil-water deficit (mm) for this zone. Negative = needs water,
    /// the sign convention the field has carried since the Smart
    /// Irrigation era. PRODUCER: the soil model's evidence replay
    /// (`engine::soil_schedule`, assembled in the refresher's
    /// `apply_soil_schedule`) publishes `-depletion_mm` for every zone
    /// with agronomy config, in shadow on weekly-model installs and
    /// live on soil-model ones. `None` when no bucket can be derived
    /// (no agronomy config, e.g. env-var zones): a bare f64 here once
    /// published a hardcoded 0.00 as if it were a measurement, the same
    /// defect `water_level_pct` was converted for. Serialized null;
    /// displays render a dash and the manifest publishes no descriptor.
    /// `#[serde(default)]` so older JSON deserializes.
    #[serde(default)]
    pub bucket_mm: Option<f64>,
    /// Per-zone duration the next sequence will use (seconds), from the
    /// weekly-budget allocator.
    pub planned_run_seconds: u32,
    /// End epoch (UTC) of this zone's most recent completed watering
    /// event, or 0 when the trailing window holds none. Populated from
    /// the per-tick run-history evidence the weekly balance uses, so it
    /// is the same anchor the session-spacing gate reads.
    pub last_run_epoch: i64,

    /// Per-zone math breakdown behind the "Why this duration?" panel.
    /// Two of its numbers reach the dispatch: `throughput_mm_hr`, which
    /// divides the weekly balance's session depth into seconds, and
    /// `max_duration_seconds`, the ceiling that can shorten the result.
    /// `kc`, `heat_mult` and `capture_eff` are real engine outputs that
    /// feed ETc and the soil projection; they do not scale the run, and
    /// the panel groups them apart so nobody reads them as operands.
    /// `None` before the first refresh builds one.
    #[serde(default)]
    pub math: Option<ZoneMath>,

    /// Optional zone photo URL. Sourced from `zones.<slug>.photo_url` in
    /// the config; copied here by the refresher so the dashboard can
    /// render it without a separate /api/config round-trip. Accepts any
    /// relative or absolute URL the browser can load (e.g. a local
    /// `/site/photos/back_yard.jpg` or an off-site `https://...` link).
    #[serde(default)]
    pub photo_url: Option<String>,

    /// This zone's own watering verdict for the upcoming run. `None`
    /// before the first refresh or for weather-only deployments. Lets the
    /// dashboard show that one zone is skipping (e.g. soil-saturated)
    /// while others run.
    #[serde(default)]
    pub verdict: Option<ZoneVerdict>,

    /// Native soil temperature (°F) from this zone's probe, polled directly
    /// by LocalSky (Ecowitt `ch_ec`). `None` when the probe is offline.
    /// Published to HA and used to derive the yard-min frost gate.
    #[serde(default)]
    pub soil_temp_f: Option<f64>,
    /// Native soil EC (µS/cm) from this zone's probe. Display-only.
    #[serde(default)]
    pub soil_ec: Option<f64>,
    /// Probe battery as a percentage (Ecowitt 0-5 level scaled ×20).
    #[serde(default)]
    pub soil_battery_pct: Option<f64>,

    /// REPORTING-ONLY suspect-probe indicator. `Some(reason)` when the
    /// soil-quarantine logic distrusted this zone's probe (offline, or a wild
    /// outlier vs its trustworthy siblings) REGARDLESS of the final watering
    /// verdict, so a genuinely bad probe is surfaced even when a global gate
    /// (forecast rain, etc.) masked the per-zone `verdict.source` as "global".
    /// The reason carries the engine's canonical "Soil probe suspect (28% vs
    /// yard 73%)" shape so the AnomalyBanner renders the numbers. Set by
    /// `apply_engine` from `engine::skip_rules::suspect_probes`, which is
    /// computed from the raw readings, NOT the verdict. Changes no decision.
    #[serde(default)]
    pub soil_suspect: Option<String>,

    /// Set when an enabled `Override` manual schedule suppresses smart
    /// dispatch for this zone. `Override` is the DEFAULT schedule mode
    /// and it zeroed the smart plan with nothing on any screen saying
    /// so, which turned "add a manual schedule" into a permanent
    /// lockout. Display only: the suppression itself is unchanged.
    /// Additive; absent = nothing suppressing.
    #[serde(default)]
    pub smart_suppressed: Option<SmartSuppression>,
}

/// Whether a zone is running, including the case where nobody can say.
///
/// `running` and `running_known` are a value and its validity held as
/// two fields, so a caller that reads only the value cannot tell "this
/// zone is off" from "this controller cannot report zone state". On a
/// fire-and-forget board those are very different: the second means the
/// water may be on right now and we would not know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Idle,
    /// The controller does not report station state, but LocalSky
    /// commanded the zone on and has not commanded it off: water is
    /// almost certainly moving, and nothing can confirm it.
    Unconfirmed,
    /// The controller does not report station state.
    Unknown,
}

impl ZoneState {
    /// The zone's run state, with the unknown case named.
    pub fn run_state(&self) -> RunState {
        match (self.running_known, self.running) {
            (false, _) if self.ledger_running => RunState::Unconfirmed,
            (false, _) => RunState::Unknown,
            (true, true) => RunState::Running,
            (true, false) => RunState::Idle,
        }
    }

    /// True only when the zone is KNOWN to be running.
    ///
    /// Use for anything that shows the operator water is moving.
    pub fn is_running(&self) -> bool {
        self.run_state() == RunState::Running
    }

    /// True when the zone might be running and we cannot tell.
    ///
    /// Use for anything that must fail safe, such as refusing to start a
    /// second sequence over the top of one that may still be going.
    pub fn may_be_running(&self) -> bool {
        matches!(
            self.run_state(),
            RunState::Running | RunState::Unconfirmed | RunState::Unknown
        )
    }

    /// Water is moving as far as anyone can tell: confirmed by the
    /// controller, or commanded by LocalSky with no readback to confirm
    /// it. The surfaces that offer Stop read this.
    pub fn is_running_or_unconfirmed(&self) -> bool {
        matches!(self.run_state(), RunState::Running | RunState::Unconfirmed)
    }
}

/// Which manual schedules are suppressing smart dispatch for a zone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SmartSuppression {
    /// Weekdays the suppression applies on, `0 = Sun .. 6 = Sat`
    /// (chrono's `num_days_from_sunday`), sorted and de-duplicated
    /// across every schedule below.
    pub weekdays: Vec<u8>,
    /// Display names of the Override schedules doing it.
    pub schedules: Vec<String>,
    /// True when one of those schedules covers TODAY, so the zone's
    /// planned seconds are zero for that reason right now.
    pub active_today: bool,
}

/// Per-zone math breakdown for the math-transparency tile. Every
/// number is LocalSky's own: throughput and the max-duration ceiling
/// from the zone's config, `kc` from the native species catalog,
/// `heat_mult` carried from the snapshot's `forecast.heat_multiplier`
/// (it's a global, not per-zone, but applies to every zone's ET
/// calculation), `capture_eff` the constant the soil projection uses.
/// `scheduled_seconds` is what the weekly-budget allocator planned for
/// today, which is the figure that actually dispatches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneMath {
    /// Soil-water deficit, mm. Negative = needs water. Produced by the
    /// soil model's evidence replay for every zone with agronomy config
    /// (see `ZoneState::bucket_mm`); `None` when no bucket can be
    /// derived. Serialized null, rendered as a dash.
    #[serde(default)]
    pub bucket_mm: Option<f64>,
    /// Crop coefficient for today, from the native species catalog
    /// (`kc_at_doy_lat`, hemisphere aware) using the zone's configured
    /// species. Never read from an entity.
    pub kc: f64,
    /// Sprinkler precipitation rate, mm/hr, from the zone's configured
    /// sprinkler type or its measured `precip_rate_mm_hr` override. Low
    /// values (~2-3 mm/hr) suggest rotors or drip; fixed sprays land
    /// around 20-40 mm/hr.
    pub throughput_mm_hr: f64,
    /// Legacy additional ET adjustment, now always 1.0. Reference ET
    /// already includes the weather response.
    pub heat_mult: f64,
    /// Effective rain/applied-water capture efficiency, 0..1. Constant
    /// 0.70 to match the Phase E water-balance model.
    pub capture_eff: f64,
    /// A pre-cap need in seconds from the RETIRED Smart Irrigation
    /// formula. Still 0 on every install: the soil model sizes its
    /// refills through `engine::soil_schedule` and publishes them via
    /// the budget row, never through this field. Nothing renders it and
    /// nothing reads it for a decision; it stays on the wire so the
    /// 1.25.0 shape holds.
    pub raw_seconds: u32,
    /// The zone's `max_run_minutes` ceiling in seconds, tightened by any
    /// active watering restriction. Hard safety stop; prevents a
    /// misconfigured throughput from running a zone for hours.
    pub max_duration_seconds: u32,
    /// What the zone will actually run today, seconds. The weekly-budget
    /// allocator's `today_seconds` after the seasonal dial, any Override
    /// manual schedule, and the force-run floor. Equal to
    /// `ZoneState::planned_run_seconds`.
    pub scheduled_seconds: u32,
    /// True when the ceiling is what set tonight's minutes: there is a run,
    /// `scheduled_seconds` equals `max_duration_seconds`, and some stage
    /// wanted more than the ceiling. Three stages can want more, and all
    /// three report here: the weekly allocator's `WaterBudget::session_capped`
    /// (its ideal session was wider than the ceiling), the seasonal dial
    /// scaling a run past it, and a condition-rule multiplier doing the same.
    ///
    /// False whenever `scheduled_seconds` is 0, so a zone held at zero by
    /// spacing, a rain defer, budget mode off, or an Override manual schedule
    /// never reads as shorted by its own cap; `session_capped` alone stays
    /// true in those states because it describes the ideal weekly slice, not
    /// today's plan. Also false for a force-run floor below the ceiling.
    ///
    /// The math panel's Scheduled row says "capped at N min" and renders in a
    /// warning color when true.
    pub cap_binding: bool,
}

/// One enabled script's watering hold, computed independently of which
/// built-in gate wins. Shared by the engine, manual schedules and API readers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScriptHold {
    pub id: String,
    pub name: String,
    pub reason: String,
}

/// Inputs and decision of the morning skip-check, rendered as a UI
/// breakdown. Single source of truth for the evaluation lives in
/// `skip_logic::evaluate`, both this dashboard and (Phase B) the HA
/// automation read the same verdict.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SkipCheck {
    // ── Live readings (Tempest + HA) ──
    pub temp_now_f: f64,
    pub wind_now_mph: f64,
    pub rain_today_in: f64,
    /// Today's rain as the forecast model has it. Separate from
    /// `rain_today_in`, which is what a gauge measured, so a surface can
    /// say which number held the yard instead of printing a forecast
    /// under the word "measured".
    #[serde(default)]
    pub rain_today_forecast_in: Option<f64>,
    pub rain_intensity_now_in_hr: Option<f64>,
    pub humidity_now_pct: f64,

    // ── Open-Meteo forecast inputs ──
    /// Tomorrow's rain (Open-Meteo `precipitation_sum`, today+1).
    pub forecast_in: Option<f64>,
    /// Tomorrow's max precipitation probability (0-100). `None` when the
    /// forecast provider reports no probability: the engine then weights
    /// tomorrow's rain at full value (see `engine::skip_rules`) and the
    /// reason strings omit the confidence claim. Serialized null;
    /// `#[serde(default)]` so older JSON deserializes.
    #[serde(default)]
    pub rain_tomorrow_prob_pct: Option<u32>,
    /// Σ daily[1..4] precip × prob/100, probability-weighted 3-day rollup.
    pub rain_3day_weighted_in: Option<f64>,
    /// Σ daily[1..7] precip × prob/100, probability-weighted 7-day rollup.
    pub rain_7day_weighted_in: Option<f64>,
    /// Σ hourly[0..4] precip, total expected rain in the next 4 hours.
    pub rain_next_4h_in: Option<f64>,
    /// OBSERVED rain over the recent window: today's measured total plus the
    /// last `rain_observed_window_days` of past observed daily rain. Sensor-
    /// independent backstop that lets the engine skip the morning after heavy
    /// rain even when a soil probe is bad/offline. Additive /api/v1 field;
    /// defaults to 0.0 when absent so JSON from older producers deserializes
    /// (an absent value just means the observed-rain gate sees no recent rain).
    #[serde(default)]
    pub rain_observed_recent_in: f64,
    /// Today's forecast peak wind (Open-Meteo daily[0]).
    pub wind_max_today_mph: f64,
    /// Forecast peak wind across the planned run window, mph. Additive:
    /// absent on older snapshots and when no window is knowable.
    #[serde(default)]
    pub wind_window_max_mph: Option<f64>,
    /// Which window today's run is planned into. Additive; a post-sunrise
    /// window carries its own forecast minimum in `window_min_temp_f`,
    /// which is what the freeze gate judged.
    #[serde(default)]
    pub run_window: crate::engine::dispatch_window::WindowKind,
    #[serde(default)]
    pub window_min_temp_f: Option<f64>,
    /// Min hourly forecast temperature for the next 24h (overnight low).
    /// Stays f64 on the wire for /api/v1 back-compat; 0.0 is the legacy
    /// missing-data placeholder. Check `temp_min_24h_valid` to tell a
    /// genuine 0 °F low apart from "no forecast".
    pub temp_min_24h_f: f64,
    /// False when the 24h forecast low was unavailable (the engine then
    /// treats the overnight-freeze rule as not applicable). Additive
    /// /api/v1 field; defaults true when absent so JSON from older
    /// producers keeps its prior "value is real" semantics.
    #[serde(default = "default_true")]
    pub temp_min_24h_valid: bool,
    /// Max forecast daily-high temperature across today + next 2 days.
    pub temp_max_3day_f: f64,
    /// Days since the last day with ≥ 0.05" rain (today included).
    /// 0 = wet today; saturates at past_daily window + 1.
    pub days_since_significant_rain: u32,
    /// Heat index now (NOAA Steadman), used as input to the heat advisory.
    pub heat_index_now_f: f64,
    /// Heat index for the 3-day forecast peak, drives the advisory rule
    /// that pre-waters before a multi-day heat wave.
    pub heat_index_max_3day_f: f64,

    // ── User-tunable thresholds ──
    // From `engine.skip_rules`. Before 0.7.22 a matching
    // `input_number.irrigation_*` helper outranked the config value on a
    // Home Assistant deployment; the adoption pass carried each helper's
    // value into the config once and retired the read.
    pub max_wind_mph: f64,
    pub min_temp_f: f64,
    pub rain_skip_in: f64,
    /// The already-wet floor and the next-4h skip threshold, both
    /// operator-tunable on the Skip rules page. The dashboard's rain bars
    /// draw their threshold marks from these; they used to hardcode the
    /// schema defaults, so a yard that tuned either saw a mark that no
    /// longer matched the line the engine skips on. Additive; absent
    /// reads as the schema default.
    #[serde(default = "default_already_wet_in_wire")]
    pub already_wet_in: f64,
    #[serde(default = "default_rain_next_4h_skip_in_wire")]
    pub rain_next_4h_skip_in: f64,
    /// The forecast-wind gate's slack over the wind limit, and the
    /// observed-rain gate's trailing window in days. Both are operands of
    /// reasons the engine writes; without them on the wire a metric
    /// viewer could not have those two sentences re-rendered and read
    /// miles per hour and inches instead. Additive; absent reads as the
    /// schema default.
    #[serde(default = "default_wind_forecast_slack_wire")]
    pub wind_forecast_slack_mph: f64,
    #[serde(default = "default_rain_observed_window_days_wire")]
    pub rain_observed_window_days: u32,

    // ── Soil sensor inputs (per-zone, generalized) ──
    /// Per-zone calibrated soil moisture AND saturation threshold, for EVERY
    /// configured soil zone (not a fixed set of yard slugs). Flattened so the
    /// JSON exposes `skip_check.soil_<slug>_pct` (Option: `null` when the probe
    /// is offline) and `skip_check.saturation_<slug>_pct` exactly as the
    /// manifest's per-zone soil descriptor (push_zone_entities) expects, for
    /// ANY zone slug. Replaces the old hardcoded back_yard/front_yard/
    /// side_yard/back_yard_shrubs fields (same JSON keys for those slugs).
    #[serde(flatten)]
    pub soil_fields: std::collections::BTreeMap<String, Option<f64>>,
    /// Whether each zone has a bound probe. Distinguishes an unavailable
    /// configured reading from an intentionally unprobed zone in simulations.
    #[serde(default)]
    pub soil_probe_configured: std::collections::BTreeMap<String, bool>,
    /// Engine-computed probe integrity holds by zone, even when a higher gate
    /// wins the visible verdict. Weather waivers cannot waive these data faults.
    #[serde(default)]
    pub soil_probe_holds: std::collections::BTreeMap<String, String>,
    /// Automatic zones whose required next-24h forecast rain is unavailable.
    /// Preserved by simulator round-trips; explicit manual-duration policy is
    /// separate from an automatic plan that cannot be sized safely.
    #[serde(default)]
    pub planning_forecast_unavailable: Vec<String>,
    /// Yard-wide soil temperature aggregates (min/max), computed natively
    /// from the per-zone soil temps in zones[]. Min drives the frost gate.
    pub soil_temp_yard_min_f: Option<f64>,
    pub soil_temp_yard_max_f: Option<f64>,
    /// Soil-frost skip threshold (°F). Below this, suspend the morning run.
    /// From the LocalSky skip-rules config (default 35.0).
    pub frost_skip_soil_f: f64,

    // ── Toggles ──
    pub is_paused: bool,
    pub is_dry_run: bool,

    // ── Decision ──
    /// The first script hold, including a failed script. Remains available
    /// when an earlier built-in gate wins; a weather waiver cannot clear it.
    #[serde(default)]
    pub script_hold: Option<ScriptHold>,
    /// `true` if any condition trips and the morning run will skip.
    pub will_skip: bool,
    /// Verdict tag: "skip" / "run" / "run_extended". The HA REST sensor
    /// surfaces this directly so the morning automation can branch.
    pub verdict: String,
    /// Human-readable reason. Empty when `verdict == "run"`.
    pub reason: String,
    /// P1 (units architecture): stable id of the rule that DECIDED this verdict,
    /// e.g. "wind_now", "rain_3day", "soil_saturation". Same id used in
    /// `RuleEval.id` / the gates catalog. `"run"` when nothing fired (a clean
    /// run). ADDITIVE + invisible: it mirrors the engine's existing baked
    /// `reason`/`verdict` and changes no decision; a later client phase re-renders
    /// the reason unit-aware from this code + the numeric operands. `#[serde(
    /// default)]` so older JSON (no code) deserializes to "".
    #[serde(default)]
    pub reason_code: String,
}

impl SkipCheck {
    /// The overnight low, or `None` when there is no reading.
    ///
    /// The value and its validity are two fields, so a caller reading
    /// only the value gets 0.0 for "no data" and renders it as a real
    /// temperature. Zero degrees is a plausible-looking number and a
    /// freeze gate reads it, which is the worst combination: a
    /// missing-data placeholder that looks like weather.
    pub fn temp_min_24h(&self) -> Option<f64> {
        self.temp_min_24h_valid.then_some(self.temp_min_24h_f)
    }
}

impl SkipCheck {
    /// Set the verdict and everything that has to agree with it.
    ///
    /// `will_skip`, `verdict`, `reason` and `reason_code` are four fields
    /// describing one decision, and four call sites rewrite them after
    /// the engine has spoken. Each site has to remember all four, and
    /// forgetting one does not fail: it leaves a snapshot claiming to run
    /// while a boolean says it will skip, and different surfaces read
    /// different fields.
    ///
    /// This is the release's own archetype again, so there is one way to
    /// change a verdict and it moves all of them together.
    pub fn decide(&mut self, verdict: &str, reason: String, reason_code: String) {
        self.will_skip = verdict == "skip";
        self.verdict = verdict.to_string();
        self.reason = reason;
        self.reason_code = reason_code;
    }

    /// Change the verdict, keeping the existing reason code.
    pub fn revise_verdict(&mut self, verdict: &str, reason: String) {
        let code = self.reason_code.clone();
        self.decide(verdict, reason, code);
    }

    /// True when the stored fields describe one decision.
    ///
    /// Exposed so tests can assert it after any mutation path.
    pub fn is_coherent(&self) -> bool {
        self.will_skip == (self.verdict == "skip")
    }
}

/// Live + forecast weather context. The dashboard surfaces both
/// sources separately: measured totals describe what fell, while nullable
/// model totals describe expected rain. A forecast never becomes a measurement.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    /// Tempest's local rain gauge accumulated total since midnight, in
    /// inches. Tempest reports in inches when HA is in imperial mode.
    pub rain_today_tempest_in: f64,
    /// The forecast model's available total for today, in inches; null when
    /// unavailable. Some providers cover the remaining forecast interval only.
    /// The historical field name is retained; `forecast_source_label` identifies
    /// the provider. Never used as measured rain or a gauge-error diagnosis.
    pub rain_today_om_in: Option<f64>,
    /// Provenance: display name of the live station driving
    /// `rain_today_tempest_in` (e.g. "Tempest", "Ecowitt") so the UI labels the
    /// rain comparison with the REAL source instead of hardcoding "Tempest".
    #[serde(default)]
    pub station_source_label: String,
    /// Provenance: display name of the forecast provider driving the model rain
    /// fields (e.g. "Open-Meteo", "NWS"). Empty until set.
    #[serde(default)]
    pub forecast_source_label: String,
    /// Tempest live rain rate, in/hr. Drives the "RAINING NOW" badge.
    pub rain_intensity_in_hr: Option<f64>,
    /// Tempest precipitation type: "none" / "rain" / "hail".
    pub rain_type: String,
    /// TRUE only when `rain_intensity_in_hr` / `rain_type` come from a LIVE source
    /// that currently OWNS the rain reading (the refresher's `rain_live`: a
    /// live_current writer stamped `rain_live_epoch` within the freshness window).
    /// FALSE on cloud-only / station-stale, where those two fields are filled from
    /// the Open-Meteo current-hour forecast precip (a model prediction, NOT an
    /// observation). The "RAINING NOW" badge keys on this so it only shows the
    /// live green OBSERVED state when a live source is actually measuring rain,
    /// instead of presenting a forecast fill as an observation (T3). Additive
    /// `#[serde(default)]` (absent -> false -> badge stays calm), so older
    /// producers and the engine snapshot tests deserialize unchanged.
    #[serde(default)]
    pub rain_is_live: bool,
    /// The HONEST nature of the live rain reading (`rain_intensity_in_hr` /
    /// `rain_type`): Measured (a real gauge), RadarQpe (NOAA MRMS radar), or
    /// Model (a forecast fill). The dashboard rain badge derives green
    /// "measures rain" / "radar-measured rain" / amber "forecast only" from this
    /// (NOT from `rain_is_live` alone), and NEVER says "live" on a Model nature.
    /// Defaults to Model (the honest fallback when no live measured/radar source
    /// owns the rain). Additive `#[serde(default)]` so older producers and the
    /// snapshot tests deserialize unchanged. The producer (refresher) derives this
    /// from the 3-tier rain gate's merge owner: Measured for a live LAN gauge or a
    /// fresh NWS observation, RadarQpe for a fresh NOAA MRMS radar fill, else
    /// Model.
    #[serde(default)]
    pub rain_nature: RainNature,
    /// Open-Meteo forecast for tomorrow, in inches.
    pub rain_tomorrow_in: Option<f64>,
    /// Three future local days' raw rain forecast, in; null when unavailable.
    pub rain_3day_in: Option<f64>,
    /// Reference evapotranspiration for the day, mm (FAO-56 ET₀). `None`
    /// when no source, forecast, legacy HA sensor, or native compute
    /// produced one (forecast outage / cold start): the old flat 5.0
    /// fallback published as a real measurement on the HA sensor and the
    /// water-balance tiles, indistinguishable from computed FAO-56.
    /// Serialized null; the engine keeps its own clearly named internal
    /// constant for advisory projections. `#[serde(default)]` so older
    /// JSON deserializes.
    #[serde(default)]
    pub eto_today_mm: Option<f64>,
    /// Same for tomorrow.
    pub eto_tomorrow_mm: f64,
    /// 3-day average ET₀ used by Smart Irrigation's Passthrough module.
    pub eto_3day_avg_mm: f64,
    /// Today's forecast temperature range and representative humidity.
    /// Resolved from the live forecast snapshot first, the legacy Open-Meteo
    /// HA REST sensors second; `None` when neither has the value. The old
    /// bare f64s were filled ONLY from the HA sensors with unwrap_or(0.0),
    /// so every native/standalone install rendered "Temp range 0°F / 0°F"
    /// and "Mean humidity 0%" as real forecast data (and prompted the LLM
    /// advisor with them as ground truth). Serialized null;
    /// `#[serde(default)]` so older JSON deserializes.
    #[serde(default)]
    pub temp_max_today_f: Option<f64>,
    #[serde(default)]
    pub temp_min_today_f: Option<f64>,
    #[serde(default)]
    pub wind_max_today_mph: Option<f64>,
    /// Today's forecast peak wind GUST, mph (Open-Meteo). Published as
    /// sensor.localsky_wind_gust_forecast; the high-wind push keys on this
    /// because the Tempest is wind-shadowed and under-reads real gusts.
    pub wind_gust_today_mph: f64,
    /// See `temp_max_today_f`: forecast-first, legacy HA sensor fallback,
    /// null when absent.
    #[serde(default)]
    pub humidity_mean_today_pct: Option<f64>,

    // ── Forecast intelligence (Phase A) ──
    /// Probability-weighted 3-day rain (today + next 2), in.
    pub rain_3day_weighted_in: Option<f64>,
    /// Probability-weighted 7-day rain (today + next 6), in.
    pub rain_7day_weighted_in: Option<f64>,
    /// Sum of expected precipitation for the next 4 hours, in.
    pub rain_next_4h_in: Option<f64>,
    /// Tomorrow's max precipitation probability (0-100). `None` when the
    /// provider reports no probability; the HA sensor reads unknown instead
    /// of a fabricated "0% = certainly dry". `#[serde(default)]`.
    #[serde(default)]
    pub rain_tomorrow_prob_pct: Option<u32>,
    /// Min temperature in the next 24 hourly forecast entries, °F.
    #[serde(default)]
    pub temp_min_24h_f: Option<f64>,
    /// Max daily-high across today + next 2 days, °F.
    #[serde(default)]
    pub temp_max_3day_f: Option<f64>,
    /// Resolved current humidity, absent when current evidence is incomplete.
    #[serde(default)]
    pub humidity_now_pct: Option<f64>,
    /// Heat index from available current temperature and humidity.
    #[serde(default)]
    pub heat_index_now_f: Option<f64>,
    /// Peak from each forecast day's own temperature/humidity pair. No current
    /// reading is substituted for a missing forecast.
    #[serde(default)]
    pub heat_index_max_3day_f: Option<f64>,
    /// Legacy additional ET adjustment, now 1.0: no second weather multiplier.
    pub heat_multiplier: f64,
    /// Days since last ≥ 0.05" rain day (heat-advisory input).
    pub days_since_significant_rain: u32,

    // ── Extended model context (2026-07, Open-Meteo extended variables;
    // every field additive + defaulted: 0 = provider doesn't report it) ──
    /// Model ET0 already SPENT today (local midnight to now), mm. The
    /// full-day figure stays in `eto_today_mm`; this makes the midday
    /// water-balance honest instead of charging the whole day up front.
    #[serde(default)]
    pub eto_spent_today_mm: f64,
    /// Vapour pressure deficit now, kPa. Sustained > ~1.6 kPa = high
    /// transpiration stress (advisory display only, never a skip input).
    #[serde(default)]
    pub vpd_now_kpa: f64,
    /// Peak VPD across the rest of today, kPa.
    #[serde(default)]
    pub vpd_max_today_kpa: f64,
    /// Modeled soil temperature at 6 cm now, °F. Model data, NOT a probe;
    /// zones with real probes keep their own `soil_temp_f`.
    #[serde(default)]
    pub soil_temp_6cm_now_f: f64,
    /// Modeled 3-9 cm volumetric soil moisture now, m³/m³.
    #[serde(default)]
    pub soil_moisture_3_9_now_vwc: f64,
    /// Modeled 3-9 cm volumetric soil moisture ~48 h out (dry-down read:
    /// falling vs `soil_moisture_3_9_now_vwc` means the model expects the
    /// root zone to dry).
    #[serde(default)]
    pub soil_moisture_3_9_in48h_vwc: f64,
}

/// Appended to a mixed-install forecast-rain skip (the aggregate
/// skip_check and the 7-day strip cells) when soil-governed zones ride
/// through the hold: the skip stands for the Weekly-model zones only.
/// One shared sentence so the refresher's producer and the strip's
/// footer qualifier (the cell tag that marks the skip as partial) can
/// never drift apart.
pub const MIXED_SKIP_NOTE: &str =
    "Holds Weekly-model zones only; Soil-model zones already count this rain against \
     their deficit.";

/// The shape of the answer to "when does the yard next water".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NextRunState {
    /// No positive watering plan in the available forecast horizon.
    NoWaterPlanned,
    /// A morning is named in `next_run_epoch`.
    #[default]
    At,
    /// Every day in the horizon is refused by a restriction.
    NoLegalDay,
    /// No sunrise on any day in range: a polar latitude in its dark season.
    NoSunrise,
    /// No location configured, so there is no sunrise to plan against.
    NoLocation,
}

/// One day in the 7-day forward verdict strip. Result of running the
/// skip-check engine against synthetic Inputs derived from each future
/// day's forecast, gives the user an at-a-glance preview of which
/// days are predicted to run, skip, or trigger heat-advisory pre-water.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayVerdict {
    /// 0 = today, 1 = tomorrow, ..., 6 = day +6.
    pub day_offset: u32,
    /// UTC epoch (00:00 local in the user's timezone).
    pub time_epoch: i64,
    /// WMO weather code for the day's dominant condition.
    pub weather_code: u32,
    /// Missing daily extremes stay unknown, including an NWS night-only row.
    #[serde(default)]
    pub temp_max_f: Option<f64>,
    #[serde(default)]
    pub temp_min_f: Option<f64>,
    /// Forecast daily precipitation total, in.
    pub precip_in: Option<f64>,
    /// Required rain evidence for this forecast cell is incomplete. Soil-model
    /// annotations cannot turn an unavailable-data hold into a predicted run.
    #[serde(default)]
    pub rain_evidence_incomplete: bool,
    /// Max precipitation probability for the day, 0-100. `None` when the
    /// provider reports no probability series; the strip omits the percent
    /// instead of claiming 0. `#[serde(default)]`.
    #[serde(default)]
    pub precip_probability_max: Option<u32>,
    /// "skip" / "run" / "run_extended".
    pub verdict: String,
    /// Human-readable reason; empty on plain "run".
    pub reason: String,
    /// P1 (units architecture): stable id of the rule that decided this day's
    /// cell, copied straight from the engine `SkipCheck.reason_code` the strip
    /// runs per day. `"run"` when nothing fired. ADDITIVE + invisible; mirrors the
    /// existing baked `verdict`/`reason`. `#[serde(default)]` so older JSON
    /// deserializes to "".
    #[serde(default)]
    pub reason_code: String,
    /// This day's hold covers only the zones the WEEKLY plan governs:
    /// the soil-governed zones water through it, so the cell is a
    /// partial hold rather than a whole-yard one. The refresher knows
    /// this as a bool while it rewrites the cell; the strip used to
    /// recover it by searching the sentence for the mixed-install note.
    /// Additive; absent = a whole-yard hold.
    #[serde(default)]
    pub mixed_hold: bool,
}

/// A progressive water-balance scenario. Future rain and irrigation in these
/// rows are modeled only; they never enter measured history or valve commands.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WaterPlanDay {
    pub date_local: String,
    pub day_offset: u32,
    pub time_epoch: i64,
    pub start_epoch: Option<i64>,
    pub finish_epoch: Option<i64>,
    pub forecast_rain_mm: Option<f64>,
    pub expected_rain_mm: Option<f64>,
    pub rain_probability_pct: Option<u32>,
    pub evidence_complete: bool,
    pub zones: Vec<WaterPlanZone>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WaterPlanZone {
    pub zone: String,
    pub name: String,
    pub planned_seconds: u32,
    pub reason: String,
    pub reason_code: String,
    pub water_need: String,
    pub depletion_mm: Option<f64>,
    pub depletion_range_mm: Option<(f64, f64)>,
    pub trigger_mm: Option<f64>,
    pub capacity_mm: Option<f64>,
    pub demand_mm: f64,
    pub demand_source: String,
    pub model: String,
    pub session_capped: bool,
}

/// Per-zone 7-day soil-moisture projection (Phase E predictive). Built
/// from a simple FAO-56-flavored water-balance model: subtract daily
/// ET, add captured rain, no irrigation. The user reads this as "if I
/// did nothing all week, would each zone stay in its healthy band?"
///
/// Predicted % is informational only, no new skip rules fire on it.
/// The dashboard's job is to make the trajectory visible so the user
/// can tune thresholds or queue manual runs ahead of dry stretches.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SoilForecast {
    /// Slug used in entity ids (`back_yard`, `front_yard`, etc.).
    pub zone_slug: String,
    /// Friendly name for the UI.
    pub zone_name: String,
    /// Current calibrated moisture %, today, from
    /// `sensor.<zone>_soil_moisture`. `None` when the probe is offline.
    pub current_pct: Option<f64>,
    /// Lower bound of the healthy band, at-or-below this prediction
    /// for any of the next 3 days, the dashboard surfaces a "dry" badge.
    /// Pulled from `input_number.irrigation_<zone>_target_min_pct`.
    pub target_min_pct: f64,
    /// Upper bound = the existing saturation threshold (we don't want
    /// the model to "aim" above this). Just for plotting the target band.
    pub target_max_pct: f64,
    /// 7-day predicted moisture %. Index 0 = today (the live reading),
    /// then `today + N` for N in 1..=6. Each step:
    /// `next = prev - (et_mm × Kc) / soil_depth_mm × 100 + (rain_mm × CAPTURE) / soil_depth_mm × 100`,
    /// clamped 0..100. Excludes any irrigation we might run, this is
    /// the "no-water baseline" projection.
    pub predicted_pct: Vec<f64>,
    /// Min predicted % across the 7-day window (for at-a-glance "will
    /// this zone go dry?" tile coloring).
    pub min_predicted_pct: f64,
    /// Max predicted %, same window.
    pub max_predicted_pct: f64,
    /// Days within the window predicted at or below `target_min_pct`.
    pub days_below_target: u32,
    /// Days within the window predicted above `target_max_pct` (over-
    /// saturation, usually triggered by heavy forecast rain).
    pub days_above_max: u32,
    /// At-a-glance status: "dry" | "ok" | "wet" | "no_data".
    /// - "dry": min_predicted_pct <= target_min_pct OR days_below_target >= 2
    /// - "wet": max_predicted_pct >= target_max_pct
    /// - "ok": in band for the full window
    /// - "no_data": current_pct is None (probe offline)
    pub status: String,
}

/// Weekly water balance per zone. The gross weekly target (inches,
/// homeowner semantics: "an inch a week including rain") is settled
/// against a trailing window of observed rain, irrigation already
/// applied, and a bias-corrected forecast credit covering only the days
/// until the zone's next expected session; the remainder splits across
/// the sessions still expected this week. Computed by
/// `engine::budget::compute_zone` (one implementation, live path and
/// tests alike). Sizing is gross: no capture-efficiency division, no
/// heat multiplier (both were removed; the old formula inflated
/// sessions by up to ~1.9x).
///
/// The HA budget-override automation at 23:30:25 still reads
/// `today_seconds` and calls `irrigation_unlimited.adjust_time`
/// (actual=...) with it: that contract is unchanged ("actual seconds to
/// water today").
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WaterBudget {
    pub zone_slug: String,
    pub zone_name: String,
    /// Always true on live paths (the per-zone budget-mode toggle was
    /// retired with the cutover); kept for wire compatibility.
    /// DEPRECATED (0.9.0): always true. v2 drops it.
    pub mode_active: bool,
    /// Gross weekly water target in inches (config, HA helper, or the
    /// agronomic slug default).
    pub weekly_budget_in: f64,
    /// Sessions per week (typical 1-3). Determines spacing and the
    /// per-session split of the remainder.
    pub sessions_per_week: u32,
    /// Probability-weighted forecast rain over the next 7 days, mm, at
    /// its HISTORICAL wire scaling (x 0.7 capture factor, unchanged
    /// across releases so external consumers that threshold on it keep
    /// their meaning). Informational only: the balance itself never
    /// subtracts a whole-week forecast, only the bias-corrected credit
    /// in `forecast_credit_mm`. Null when an included day has no rain amount.
    pub expected_rain_mm: Option<f64>,
    /// The balance remainder (mm): gross weekly target minus observed
    /// rain, applied irrigation, and the forecast credit, floored at 0.
    pub needed_mm: f64,
    /// Per-session gross depth (mm): remainder / remaining_sessions.
    /// seconds_per_session = mm_per_session / throughput_mm_hr x 3600,
    /// with no capture or heat factor.
    pub mm_per_session: f64,
    pub seconds_per_session: u32,
    /// True if seconds_per_session exceeded the zone's effective cap
    /// (max_run_minutes min any restriction cap): a single session
    /// cannot deliver its share. The tuning report's cap check keys on
    /// this.
    pub session_capped: bool,
    /// End epoch of the most recent completed watering event for this
    /// zone (clustered run-history evidence), or 0 if none in the
    /// trailing window. Drives the session-spacing gate.
    pub last_run_epoch: i64,
    /// Today's recommendation. `seconds == 0` means "don't run today";
    /// reason names what decided (covered / deferred for forecast rain /
    /// spaced after the last session / session M of N).
    pub today_seconds: u32,
    pub today_reason: String,

    // ---- Balance terms (additive, 1.21.0). Absent on older JSON. ----
    /// Observed rain over the trailing 7 local days including today, mm.
    #[serde(default)]
    pub observed_rain_mm: f64,
    /// Ladder rung that supplied the observed term:
    /// "gauge" | "radar" | "model_archive" | "none".
    #[serde(default)]
    pub observed_rain_source: String,

    // ---- Rain-credit cap (additive, 1.26.0). Absent on older JSON. ----
    /// Observed rain the balance actually credited (mm): each covered
    /// day held to `rain_credit_cap_mm` before summing. Equals
    /// `observed_rain_mm` whenever no single day exceeded the cap.
    #[serde(default)]
    pub observed_rain_credited_mm: f64,
    /// The per-day rain-credit cap in effect (mm): the most one day's
    /// observed or forecast rain may offset against the weekly target.
    /// 0 on JSON from an older producer = unknown/legacy (no cap
    /// applied).
    #[serde(default)]
    pub rain_credit_cap_mm: f64,
    /// True when the cap was derived from soil texture and root depth
    /// (TAW) rather than set through the zone's `rain_credit_cap_in`.
    #[serde(default)]
    pub rain_cap_inferred: bool,
    /// Gross irrigation applied over the trailing 7 days, mm
    /// (union-clustered completed watering evidence x throughput).
    #[serde(default)]
    pub applied_mm: f64,
    /// Bias-corrected forecast rain credited between now and the next
    /// expected session, mm. Zero when the next session is due now.
    #[serde(default)]
    pub forecast_credit_mm: f64,
    /// "bias_forecast" for a known forward credit, "unavailable" when missing
    /// forecast evidence earns no credit, else "none" when no credit is needed.
    #[serde(default)]
    pub forecast_credit_source: String,
    /// The current month's bias multiplier applied to the credit
    /// (1.0 = identity, including the under-trained case; see
    /// `bias_sample_count`).
    #[serde(default = "default_bias_multiplier")]
    pub bias_multiplier: f64,
    /// Observations behind the current month's multiplier. Below the
    /// training minimum the multiplier is 1.0 by design.
    #[serde(default)]
    pub bias_sample_count: u32,
    /// Sessions still expected this week: sessions_per_week minus
    /// completed watering events in the trailing window, floor 1.
    #[serde(default)]
    pub remaining_sessions: u32,
    /// True when `weekly_budget_in` and `sessions_per_week` came from the
    /// agronomic default inferred from the zone slug rather than from the
    /// operator's own config. The allocator decides dispatch on every
    /// deployment path now, so a zone watering on an inferred target is
    /// watering on a guess nobody reviewed; the Zones page raises a
    /// one-time banner naming those zones and their targets.
    #[serde(default)]
    pub target_inferred: bool,

    // ---- Soil model (additive, bucket-governs wave 3). Absent on
    // older JSON. The bucket computes in SHADOW on every install whose
    // zone carries agronomy config, whichever model governs, so these
    // ride the wire under the weekly model too. ----
    /// Which model produced this zone's `today_seconds`:
    /// "weekly" | "soil". Empty on JSON from an older producer.
    #[serde(default)]
    pub scheduling_model: String,
    /// Reconstructed root-zone depletion below field capacity (mm), in
    /// [0, TAW], from `engine::soil_schedule`'s evidence replay. `None`
    /// when the zone has no agronomy config to derive a bucket from
    /// (env-var zones) or the producer predates the soil model.
    #[serde(default)]
    pub soil_depletion_mm: Option<f64>,
    /// Bounds from unknown initial soil storage; the precise estimate stays
    /// absent until they converge. Decisions can still agree at both ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soil_depletion_range_mm: Option<(f64, f64)>,
    /// Total available water in the root zone (mm), from texture and
    /// root depth. `None` with `soil_depletion_mm`.
    #[serde(default)]
    pub soil_taw_mm: Option<f64>,
    /// Readily available water (mm): the trigger threshold depletion
    /// crosses. `None` with `soil_depletion_mm`.
    #[serde(default)]
    pub soil_raw_mm: Option<f64>,
    /// Depletion crossed RAW this tick (the soil trigger fired).
    #[serde(default)]
    pub soil_due: bool,
    /// The soil plan's seconds. Under the soil model this is what
    /// `today_seconds` starts from (window admission applied); under
    /// weekly it is the shadow figure ("would water N seconds today"),
    /// with no admission pass.
    #[serde(default)]
    pub soil_planned_seconds: u32,
    /// The hold that zeroed a due soil zone: defer-by-deficit, or the
    /// morning-window admission reason. `None` when not due or watering.
    #[serde(default)]
    pub soil_deferred_reason: Option<String>,
    /// The soil says this planting is asleep: below its species' growth
    /// threshold. The bucket is held and nothing is due. Additive.
    #[serde(default)]
    pub dormant: bool,
    /// WHICH hold it was, as data rather than as the opening words of
    /// the sentence above. Surfaces that compose their own copy in the
    /// viewer's units read this; the sentence stays for logs and
    /// external consumers. Additive; absent on older payloads, where the
    /// consumer falls back to reading the sentence.
    #[serde(default)]
    pub soil_deferred_kind: Option<crate::engine::soil_schedule::SoilDeferKind>,
    /// The EXPLICIT weekly target, honored as a rolling-7-day delivery
    /// ceiling, shorted today's soil refill.
    #[serde(default)]
    pub soil_ceiling_binding: bool,
    /// Replay-window days that carried ANY evidence (a resolved ET0
    /// rung, a nonzero rain row, or applied valve seconds). Additive
    /// (0.8.0); omitted from the JSON when zero, so a row with no soil
    /// block and the pinned wire snapshots stay byte-identical.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub soil_evidence_days: u32,
    /// Replay-window days with no evidence at all: they charged the
    /// fallback daily mean under the assumed-dry rule. When these
    /// dominate the window, the published deficit is mostly assumption
    /// and the soil panel says so. Additive (0.8.0); omitted when zero.
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub soil_fallback_days: u32,
}

fn default_wind_forecast_slack_wire() -> f64 {
    crate::config::schema::SkipRuleParams::default().wind_forecast_slack_mph
}

fn default_rain_observed_window_days_wire() -> u32 {
    crate::config::schema::SkipRuleParams::default().rain_observed_window_days
}

fn default_already_wet_in_wire() -> f64 {
    crate::config::schema::SkipRuleParams::default().already_wet_in
}

fn default_rain_next_4h_skip_in_wire() -> f64 {
    crate::config::schema::SkipRuleParams::default().rain_next_4h_skip_in
}

fn u32_is_zero(v: &u32) -> bool {
    *v == 0
}

/// Where a probe reading sits against its zone's band, right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoilBand {
    /// No reading: the probe is offline or unbound.
    Offline,
    /// At or above the saturation ceiling. Matches the engine's
    /// soil-saturation gate operator exactly (`pct >= saturation`).
    Saturated,
    /// Strictly below the dry floor. Matches the engine's soil-floor veto
    /// (`pct < target_min`), which is what lets a dry zone override a
    /// forecast-rain skip.
    Dry,
    /// Inside the band.
    Healthy,
}

impl SoilForecast {
    /// Classify this zone's CURRENT reading against its band, with the
    /// same comparisons the engine's soil gates use. The probe card used
    /// to repeat those comparisons inline, so a threshold operator could
    /// change in the engine and leave the pill saying the opposite of
    /// what the gate decided.
    pub fn current_band(&self) -> SoilBand {
        match self.current_pct {
            None => SoilBand::Offline,
            Some(c) if c >= self.target_max_pct => SoilBand::Saturated,
            Some(c) if c < self.target_min_pct => SoilBand::Dry,
            Some(_) => SoilBand::Healthy,
        }
    }
}

impl WaterBudget {
    /// Is this zone still watering on a weekly target nobody set? Only
    /// the WEEKLY plan waters toward a target, so a soil-governed zone
    /// carrying an inferred one is not on a guess for anything that
    /// matters: its runs come from its own soil deficit. An empty
    /// `scheduling_model` (an older producer) reads as weekly.
    ///
    /// Both the notice that lists such zones and the push that warns
    /// about them ask this one question here. They used to ask it
    /// separately, and the push never learned about soil governance, so
    /// a soil-run yard was told to go set weekly targets it does not
    /// use.
    pub fn on_inferred_weekly_target(&self) -> bool {
        self.target_inferred && self.scheduling_model != "soil"
    }
}

/// Does this skip-check reason code mean the operator paused watering?
/// TWO gates pause a yard and both have to count: the timed vacation
/// pause (`pause_until`) and the plain toggle (`paused`). Surfaces used
/// to ask this by testing whether the engine's sentence began with the
/// word "Paused", which made a wording change a silent behavior change
/// and skipped the `is_paused` flag sitting on the same struct. The flag
/// alone is not the answer either: it covers the toggle only, so a yard
/// on a timed vacation pause would have read as running.
pub fn is_pause_code(reason_code: &str) -> bool {
    reason_code == "paused" || reason_code == "pause_until"
}

/// How a DIVERGENT soil-vs-weekly balance line opens. The engine writes
/// these lines into the snapshot and the zone Tuning panel decides from
/// this prefix whether to place one beside the lead or behind the
/// data-notes disclosure, so it lives on the wire type both sides
/// compile: a copy edit cannot quietly file the one line that sells the
/// opt-in behind a disclosure.
pub const SOIL_DIVERGENCE_PREFIX: &str = "The soil model would ";

fn default_bias_multiplier() -> f64 {
    1.0
}

impl IrrigationSnapshot {
    /// Does this zone actually water at the next dispatch? The ONE
    /// skip-aware predicate for every rollup (hero Tonight minutes and
    /// zone count, the Zones-page KPI strip), so no surface can promise
    /// water for a zone the effective verdict skips, or count a
    /// soil-governed zone holding at zero planned seconds (the NORMAL
    /// soil-model state most mornings) as watering. A decided zone
    /// waters when its effective verdict is not a skip AND it carries
    /// planned seconds; before any verdict exists, planned seconds
    /// alone decide, so a pre-decision frame still reads sensibly.
    /// "Thu and Sun", "Mon, Wed and Fri", or None when no restriction is
    /// configured or every day is allowed.
    pub fn allowed_days_phrase(&self) -> Option<String> {
        let days = self.restriction_allowed_days.as_ref()?;
        if days.is_empty() || days.len() >= 7 {
            return None;
        }
        const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let mut sorted: Vec<u8> = days.iter().copied().filter(|d| *d < 7).collect();
        sorted.sort_unstable();
        sorted.dedup();
        let names: Vec<&str> = sorted.iter().map(|d| NAMES[*d as usize]).collect();
        Some(match names.len() {
            0 => return None,
            1 => names[0].to_string(),
            2 => format!("{} and {}", names[0], names[1]),
            n => format!("{} and {}", names[..n - 1].join(", "), names[n - 1]),
        })
    }

    pub fn zone_waters_next_run(&self, z: &ZoneState) -> bool {
        match z
            .verdict
            .as_ref()
            .or_else(|| self.zone_verdicts.iter().find(|v| v.zone_slug == z.slug))
        {
            Some(v) => v.verdict != "skip" && z.planned_run_seconds > 0,
            None => z.planned_run_seconds > 0,
        }
    }

    /// The effective verdict says this zone skips (its own back-filled
    /// verdict first, the snapshot-level list by slug as fallback, the
    /// engine's own lookup order).
    pub fn zone_skips_next_run(&self, z: &ZoneState) -> bool {
        z.verdict
            .as_ref()
            .or_else(|| self.zone_verdicts.iter().find(|v| v.zone_slug == z.slug))
            .is_some_and(|v| v.verdict == "skip")
    }
}

/// Top-level snapshot for the irrigation page. Cheap to clone (`Arc`-
/// wrapped before any client touches it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct IrrigationSnapshot {
    /// Stable flow selection for clients: actual controller meter, then the
    /// bus's fresh ranked reading. Existing raw flow fields keep their contract.
    #[serde(default)]
    pub flow: crate::model::FlowReadout,
    /// UTC epoch of the most recent successful HA poll. 0 if we've never
    /// successfully refreshed.
    pub last_refresh_epoch: i64,
    /// DEPRECATED (0.9.0): on the Home Assistant path, true when the most
    /// recent poll completed without error; on the native path always
    /// true, because there is no Home Assistant to reach. v2 drops it:
    /// reachability is per source in `GET /health` (`sources[].status`),
    /// and the snapshot's own age is `last_refresh_epoch`.
    pub ha_reachable: bool,

    /// Process-lifetime watering hold while startup configuration is pending.
    /// This is runtime evidence, not an operator override; only restart clears it.
    #[serde(default)]
    pub restart_required: bool,
    #[serde(default)]
    pub restart_reasons: Vec<String>,

    /// IANA timezone name for this deployment (e.g. "America/New_York"), mirroring
    /// `ForecastSnapshot.timezone`. The client formats every epoch in the
    /// irrigation UI against THIS zone (via `crate::timefmt`), not the viewer's
    /// browser timezone, so a phone in another timezone still shows the
    /// deployment's local 24h clock. Populated each refresh from the configured /
    /// location-derived timezone. Additive `#[serde(default)]`: absent (older
    /// producers) -> empty string, and the client falls back to browser-local.
    #[serde(default)]
    pub timezone: String,

    /// UTC epoch of the most recent Tempest UDP packet (or 0). The
    /// dashboard strip header surfaces this so a stalled local-radio
    /// path is visible without a separate /api/health round-trip.
    #[serde(default)]
    pub tempest_last_seen_epoch: i64,
    /// Serial of the live local station (Tempest / Ecowitt / ...) currently
    /// owning current-conditions, mirroring `WeatherSnapshot.station_serial`.
    /// EMPTY means no physical/live station has reported, i.e. a cloud-only
    /// install: the verdict-strip freshness pill keys on this to show
    /// provenance instead of falsely flagging a non-existent station as
    /// "stale". Populated each refresh from the live-station store. Additive
    /// `#[serde(default)]`: absent (older producers) -> "" -> cloud-only path.
    #[serde(default)]
    pub station_serial: String,
    /// UTC epoch of the most recent Open-Meteo refresh (or 0). Same
    /// observability purpose as `tempest_last_seen_epoch`.
    #[serde(default)]
    pub forecast_last_seen_epoch: i64,

    /// IU sequence's next scheduled fire time. UTC epoch.
    pub next_run_epoch: i64,
    /// Why `next_run_epoch` is what it is. `at` when it names a morning;
    /// otherwise which of the three no-answer cases: no location, no
    /// sunrise in range (polar night), or no legal day in the horizon.
    /// The epoch alone collapsed all three to 0 and one blank headline.
    /// Additive; older snapshots read as `at`.
    #[serde(default)]
    pub next_run_state: NextRunState,
    /// Days from today to the next run's morning, when there is one.
    /// 0 = today, 1 = tomorrow. Lets the UI find the strip cell and
    /// judge "pending today" without comparing clock strings. Additive.
    #[serde(default)]
    pub next_run_day_offset: Option<u32>,
    /// The weekdays the configured restrictions allow this address to
    /// water, `0 = Sun .. 6 = Sat`, judged over the coming fortnight so a
    /// seasonal rule counts only while it is in effect. `None` when no
    /// restriction is configured. The surfaces that say "not a watering
    /// day" name the allowed days from this instead of echoing the
    /// engine's "today is not an allowed watering day" on a Thursday row.
    #[serde(default)]
    pub restriction_allowed_days: Option<Vec<u8>>,
    /// Total minutes the next sequence will run if not skipped.
    pub next_run_total_minutes: f64,
    /// Master controller enable (`switch.<prefix>_enabled`).
    pub master_enable: bool,
    /// DEPRECATED (0.9.0): always false. The Irrigation Unlimited
    /// sequence's enabled flag; IU support is gone. v2 drops it.
    pub iu_enabled: bool,
    /// DEPRECATED (0.9.0): always false. IU's suspended flag; IU support
    /// is gone. v2 drops it; the engine's own hold is `skip_check.is_paused`
    /// and `pause_until_epoch`.
    pub iu_suspended: bool,
    /// Controller water level / seasonal adjust (0-250%, where 100% is the
    /// SI baseline; OpenSprinkler's `wl`). `None` when the active controller
    /// does not report one (every adapter except OpenSprinkler) or
    /// the HA bridge entity is absent: the old bare f64 fabricated 100% on
    /// the native path and 0% on the HA path, and neither was a measurement.
    /// Serialized null; the hero renders a dash and the HA sensor reads
    /// unavailable. `#[serde(default)]` so older JSON deserializes.
    #[serde(default)]
    pub water_level_pct: Option<f64>,
    /// True when the active controller declares the water-level capability
    /// (`ControllerCaps.water_level`) or a level has actually been read this
    /// refresh (HA bridge entity present). Gates the manifest's
    /// water_level_pct descriptor the same way `flow_meter` gates flow, so
    /// an install whose controller can never report one grows no phantom
    /// sensor. Additive; absent = false.
    #[serde(default)]
    pub water_level_capable: bool,

    /// True when the active controller reports a flow meter capability
    /// (`ControllerCaps.flow_meter`). Lets the UI / HA decide whether to
    /// surface any flow readout at all; false on controllers with no flow
    /// hardware. Additive; absent = false (no meter).
    #[serde(default)]
    pub flow_meter: bool,
    /// A reachable controller confirms a connected meter this refresh.
    /// Unlike `flow_meter`, this is presence evidence rather than capability.
    #[serde(default)]
    pub flow_connected: bool,
    /// Live measured flow from the controller's own flow sensor, in
    /// gallons-per-minute. `None` when the controller has no flow meter
    /// (so non-flow setups render nothing); `Some(0.0)` is a real
    /// "meter present, zero flow" reading. Populated from
    /// `ControllerStatus.flow_gpm` each refresh on native deploys.
    /// Additive; absent = null.
    #[serde(default)]
    pub flow_gpm: Option<f64>,

    pub zones: Vec<ZoneState>,
    pub skip_check: SkipCheck,
    pub forecast: Forecast,
    /// Today's chosen watering window: pre-dawn, or the first
    /// post-sunrise hour that clears a freezing morning. Written by the
    /// refresher from the same function the strip and next_run ask; the
    /// dispatcher executes it. Additive; absent on older snapshots.
    #[serde(default)]
    pub today_window: Option<crate::engine::dispatch_window::PlannedWindow>,

    /// 7-day forward verdict strip, predicted skip/run for today + 6
    /// future days. Computed server-side by running `skip_logic::evaluate`
    /// against synthetic Inputs from each daily forecast entry.
    pub seven_day_verdicts: Vec<DayVerdict>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub water_plan: Vec<WaterPlanDay>,

    /// Per-zone soil-moisture projections (Phase E predictive). One
    /// entry per WH52 zone, holding the 7-day "no irrigation" baseline
    /// + the target band so the dashboard can show whether each zone
    /// stays healthy on rain + ET alone.
    #[serde(default)]
    pub soil_forecasts: Vec<SoilForecast>,

    /// Per-zone weekly water balance. One entry per zone with the
    /// settled balance terms + today's recommendation. HA's
    /// localsky_weekly_budget_override automation reads `today_seconds`
    /// and overrides SI's value via `irrigation_unlimited.adjust_time`
    /// at 23:30:25 (contract unchanged: actual seconds to water today).
    #[serde(default)]
    pub water_budgets: Vec<WaterBudget>,

    /// The engine-level scheduling model default ("weekly" | "soil"),
    /// the baseline a per-zone pin diverges FROM: the zone card and
    /// detail render a model chip only on zones whose effective model
    /// differs from this, so single-model installs stay quiet and a
    /// mixed install becomes scannable. Additive (0.8.0); omitted from
    /// the JSON when empty, so older producers and the pinned wire
    /// snapshots stay byte-identical.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub engine_scheduling_model: String,

    /// Vacation pause expiry, UTC epoch seconds. 0 means not set / not paused.
    /// Read from `input_datetime.irrigation_pause_until` (a manually-created
    /// HA helper). When `now < pause_until_epoch` skip_logic short-circuits
    /// to "skip" with reason "Vacation until ...". The helper attribute is
    /// the canonical source of truth; HA's midnight check is unnecessary
    /// because the comparison is direct against current time.
    #[serde(default)]
    pub pause_until_epoch: i64,
    /// One-day override for tomorrow's verdict. "none" | "skip" | "run".
    /// Read from `input_select.irrigation_override_tomorrow`. An HA midnight
    /// automation should reset this to "none" each day so the override is a
    /// one-day knob, not a permanent override. When the snapshot is missing
    /// this entity (helper not created), the field stays "none".
    #[serde(default)]
    pub override_tomorrow: String,
    /// DEPRECATED (0.9.0): true when a persistence database is mounted,
    /// which is when the pause and override controls have somewhere to
    /// land. The name predates 0.7.22, when these lived in Home Assistant
    /// helpers; `controls_persisted` says the same thing under its real
    /// name, and v2 keeps only that.
    #[serde(default)]
    pub override_helpers_present: bool,
    /// The Home Assistant helpers the one-time 0.7.22 adoption pass has
    /// handled, copied verbatim from `config.ha_adoption` each refresh. Drives
    /// the migration notice. Empty on every standalone install and on a Home
    /// Assistant install before the pass runs.
    #[serde(default)]
    pub ha_adoption: Vec<HaAdoptedHelper>,
    /// True when a persistence database is mounted, i.e. the four operator
    /// controls have somewhere to land. The migration notice needs this to
    /// tell two different reasons a control can be missing from
    /// `ha_adoption` apart: false means it can never be adopted here and the
    /// helper is still deciding, true means the pass is waiting on Home
    /// Assistant to answer for it and there is nothing to do. Inferring it
    /// from the records alone told an install with `/data` mounted to mount
    /// `/data`. ADDITIVE; absent reads false, which is the old no-database
    /// wording and the safe one.
    #[serde(default)]
    pub controls_persisted: bool,
    /// DEPRECATED (0.9.0): always false. It said the 0.7.22 adoption pass
    /// was waiting for a config file; the pass no longer exists. Kept on
    /// the v1 wire so a consumer reading it keeps parsing; v2 drops it.
    #[serde(default)]
    pub ha_adoption_awaiting_config: bool,
    /// Sticky global override: "auto" | "skip" | "run". Persisted in
    /// LocalSky's own sqlite (native) so it survives redeploys and applies
    /// regardless of HA mode. Unlike `override_tomorrow` it does NOT reset
    /// nightly. The irrigation page renders the Auto/Skip/Force control from
    /// this; a per-zone override (ZoneState.override_mode) beats it.
    #[serde(default = "default_auto_override")]
    pub global_override: String,

    /// Structured provenance for today's morning skip decision: every rule
    /// the ladder walked, what it saw, and which one fired. Computed
    /// alongside `skip_check` by the refresher (same Inputs). Powers the
    /// Rule Lab UI. None until the first successful refresh.
    #[serde(default)]
    pub decision_trace: Option<DecisionTrace>,

    /// Per-zone watering verdicts for the upcoming run. One entry per
    /// configured zone (empty before the first refresh / weather-only
    /// deployments). Global gates bind every zone; per-zone soil + custom
    /// condition rules let zones diverge. Produced by `decide_per_zone`.
    #[serde(default)]
    pub zone_verdicts: Vec<ZoneVerdict>,

    /// Soil probes that are configured for a zone but have produced no
    /// valid (> 0%) reading in 24h or more. A flatlined probe resolves to
    /// `None` upstream, which silently makes the yard-wide saturation
    /// gate inapplicable; this names the dead hardware so the UI,
    /// /api/health, and push can surface it. Additive; absent = no
    /// known faults.
    #[serde(default)]
    pub soil_probe_faults: Vec<SoilProbeFault>,

    /// Household display-unit default, copied verbatim from
    /// `cfg.deployment.units` each refresh (mirror of the `photo_url` copy
    /// pattern). The client uses this as the per-device baseline: when a
    /// device has not opted into its own override, `use_unit_prefs` expands
    /// this household value (Metric -> METRIC, Imperial -> IMPERIAL). The
    /// engine never reads it; it is display-plumbing only. Additive;
    /// absent = `Units::Imperial` (the serde + struct default), which keeps
    /// the default deployment byte-identical to before this field existed.
    #[serde(default)]
    pub units: Units,

    /// Per-field current-conditions provenance: WeatherField snake_case name
    /// (see `config::field_overrides::field_name`, e.g. `wind_mph`,
    /// `rain_today_in`) -> the display label of the source CURRENTLY driving
    /// that reading ("Tempest", "Ecowitt", a cloud provider, ...). Populated
    /// each refresh from the merge layer's live ownership, so the UI can label
    /// each headline reading with where it actually comes from ("Wind: Tempest")
    /// and a per-field source picker can show the live owner. Covers the small
    /// user-facing set the override page exposes. Additive `#[serde(default)]`:
    /// absent (older producers / no live source yet) deserializes to an empty
    /// map and the UI falls back to the merged `source_label`.
    #[serde(default)]
    pub field_sources: std::collections::BTreeMap<String, String>,
    /// The exact current inputs the engine resolves, with measurement nature,
    /// report time, configured age limit and selection/fallback explanation.
    #[serde(default)]
    pub current_weather:
        Option<std::collections::BTreeMap<String, crate::weather::CurrentWeatherSample>>,

    /// Forced-run safety signal: when a sticky `global_override = "run"` is
    /// watering THROUGH a hard guard (freeze, restriction, raining-now, dry-run,
    /// ...), this carries that guard's reason string (e.g. "Freeze risk now
    /// (28°F < 35°F)") so the irrigation hero can warn the operator they are
    /// running past a real protection. `None` when there is no force-run, or when
    /// the force-run is not overriding anything (the engine would have run
    /// anyway). The override still wins, byte-for-byte; this only NAMES what it
    /// is suppressing. Produced by `engine::force_overrode_guard`. Additive
    /// `#[serde(default)]`: absent (older producers) -> `None`, no warning shown.
    #[serde(default)]
    pub force_overrode_guard: Option<String>,
}

/// Household display-unit system. Defined here in the both-features snapshot
/// module (rather than in `config::schema`, which is `ssr`-only) so the hydrate
/// client can deserialize it off the snapshot and resolve display units.
/// `config::schema` re-exports this as `config::schema::Units`, so the config
/// layer's `deployment.units` field and the wizard keep their existing path.
/// `JsonSchema` is derived only under `ssr` (schemars is an ssr-only dep, and
/// only the ssr-side `/api/config/schema` endpoint needs it).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ssr", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum Units {
    #[default]
    Imperial,
    Metric,
}

/// The HONEST nature of the LIVE current-rain reading driving the snapshot, the
/// axis the dashboard rain badge keys on (distinct from `Forecast::rain_is_live`,
/// which says only whether a live source currently owns the reading):
///
///   * `Measured`: a real instrument gauge reading (a LAN station, or an NWS
///     station observation). Green "Measures rain".
///   * `RadarQpe`: a gauge-corrected radar rain estimate (NOAA MRMS),
///     observation-grade but a radar grid, not your own gauge. Green
///     "Radar-measured rain".
///   * `Model`: a model/forecast fill for the current interval (Open-Meteo,
///     Pirate's rain, OpenWeather, WeatherKit, Met.no). Amber "Forecast only";
///     the badge must NEVER say "live" on this.
///
/// Defaults to `Model` (the honest fallback when no live measured/radar source
/// owns the rain). Lives in this both-features snapshot module (not ssr-only
/// `config::schema`) so the hydrate client deserializes it off the snapshot.
/// `JsonSchema` only under ssr (schemars is ssr-only). The producer derives this
/// per refresh in the refresher's 3-tier rain gate: `Measured` when a live LAN
/// gauge (or a fresh NWS observation) owns the current-rain field, `RadarQpe`
/// when a fresh NOAA MRMS radar fill owns it, else `Model` (the forecast
/// fallback). The same nature also drives the engine's observation-grade-only
/// hard rain skip (a `Model` rain may only soft-skip).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[cfg_attr(feature = "ssr", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum RainNature {
    Measured,
    RadarQpe,
    #[default]
    Model,
}

/// One rule's evaluation in a decision trace. Lives here (the shared
/// both-features serde contract) rather than in `engine` (ssr-only) so the
/// hydrate-side Rule Lab UI can deserialize + render it. `engine::skip_rules::decide_traced`
/// produces these.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RuleEval {
    /// Stable id (e.g. "rain_now").
    pub id: String,
    /// Human label shown in the ladder.
    pub label: String,
    /// safety | weather | soil | heat | control
    pub category: String,
    /// The data values the rule saw, vs its threshold.
    pub detail: String,
    /// fired | passed | skipped | not_reached
    pub outcome: String,
    /// This gate's own threshold IS met, yet it did not fire: a stronger
    /// rule earlier in the ladder overrode it. The engine knows this
    /// exactly, from each gate's own comparison operator, while it builds
    /// the row; renderers used to recover it by searching the margin
    /// label for the word "overridden". Additive; absent = not over its
    /// line.
    #[serde(default)]
    pub over_line: bool,
    /// Verdict produced if this rule fired.
    pub verdict: Option<String>,
    /// Plain-language "distance to flip" for the threshold gates, e.g.
    /// "0.08\" of headroom before this skips" (passed) or "skipped, 0.05 in/hr
    /// past the line" (fired). `None` for binary control/safety gates with no
    /// numeric threshold, and for inapplicable / not-reached rows. Additive
    /// serde field; absent = no margin shown.
    #[serde(default)]
    pub margin_label: Option<String>,
    /// P1 (units architecture): the structured operands behind `margin_label`,
    /// so a later client phase can re-render the margin unit-aware instead of
    /// parsing the baked string. `value` is the driving input the gate saw,
    /// `threshold` the line it compares against, both in IMPERIAL canonical units;
    /// `unit_kind` names the dimension ("temp_f","wind_mph","rain_in",
    /// "rain_rate_in_hr","pct","soil_temp_f","none"). All `None` for binary
    /// control/safety gates (override, pause, restrictions, dry_run, live_data)
    /// and for inapplicable / not-reached rows. ADDITIVE: mirrors `margin_label`'s
    /// inputs, changes no decision. `#[serde(default)]` so older JSON deserializes.
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub unit_kind: Option<String>,
}

/// Full structured trace of a morning skip decision.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DecisionTrace {
    pub verdict: String,
    pub reason: String,
    /// True when the live "now" readings were degraded for this decision
    /// (station stale/absent; forecast current-hour values stood in, or no
    /// data was available at all). Additive field; absent = false.
    #[serde(default)]
    pub degraded: bool,
    /// P1 (units architecture): stable id of the rule that DECIDED this trace,
    /// mirroring the deciding `RuleEval.id` (the one whose outcome is "fired").
    /// `"run"` when nothing fired (a clean run). ADDITIVE + invisible; mirrors
    /// the existing baked `verdict`/`reason`. `#[serde(default)]` so older JSON
    /// deserializes to "".
    #[serde(default)]
    pub reason_code: String,
    pub rules: Vec<RuleEval>,
}

/// One day on the forecast-accuracy scoreboard. The decision LocalSky made
/// that day paired with what the sky actually did, so an operator can watch the
/// rain calls pay off (or honestly miss). Shared serde type so the History UI
/// renders it directly. `correct` is `Some` only on days where rain was actually
/// a factor (forecast or observed); a dry default day or a non-rain skip
/// (restriction/freeze) is shown for context but not scored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreboardDay {
    pub date: String,
    pub verdict: String,
    pub reason: String,
    /// P1 (units architecture): stable id of the rule that decided this day,
    /// classified from the persisted `reason` (the scoreboard reconstructs days
    /// from `verdict_history` rows, which store only verdict + reason text, so the
    /// code is derived not re-emitted; no history migration). `"run"` for a clean
    /// run and `""` when the reason can't be classified. ADDITIVE + invisible;
    /// `#[serde(default)]` so older JSON deserializes to "".
    #[serde(default)]
    pub reason_code: String,
    pub predicted_in: Option<f64>,
    pub observed_in: Option<f64>,
    pub assessment: String,
    pub correct: Option<bool>,
}

/// The scoreboard window plus its honest tally. `scored` counts only the
/// rain-relevant days; `matched` is how many of those went the right way.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccuracyResult {
    pub days: Vec<ScoreboardDay>,
    pub scored: u32,
    pub matched: u32,
}

fn one_f64() -> f64 {
    1.0
}

fn default_true_running_known() -> bool {
    true
}

fn default_auto_override() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

/// Per-zone watering verdict. Unlike the aggregate `skip_check` (one
/// verdict for the whole run), this is computed per zone so a saturated
/// zone can skip while a dry zone runs. Global safety/weather gates bind
/// every zone (`source = "global"`); per-zone soil + custom condition
/// rules are layered on top. Produced by `engine::skip_rules::decide_per_zone`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneVerdict {
    pub zone_slug: String,
    pub zone_name: String,
    /// "skip" | "run" | "run_extended"
    pub verdict: String,
    pub reason: String,
    /// Which layer decided: "global" | "soil_saturation" | "soil_quarantine"
    /// (an offline/outlier probe was distrusted and this zone's soil was
    /// inferred from its trustworthy neighbors) | "condition" | "soil_floor"
    /// (dry-soil veto ran the zone despite a forecast-rain skip) | "override" |
    /// "default"
    pub source: String,
    /// Applied watering multiplier from custom AdjustMultiplier rules,
    /// clamped to [0.5, 1.5]. 1.0 = unchanged.
    #[serde(default = "one_f64")]
    pub multiplier: f64,
    /// P1 (units architecture): stable id of the rule that decided this zone:
    /// the per-zone rung's own id ("soil_saturation" / "soil_quarantine" /
    /// "soil_floor" / "override") when it diverges, else the global firing rule
    /// id carried in from the yard-wide decision ("rain_3day", "wind_now", ...),
    /// or "condition" for a custom-rule skip, and "run" on a clean run. ADDITIVE
    /// + invisible; mirrors the existing baked `verdict`/`reason`/`source`.
    /// `#[serde(default)]` so older JSON deserializes to "".
    #[serde(default)]
    pub reason_code: String,
    /// P1 (units architecture): the soil operands behind the per-zone soil
    /// decision, in PERCENT (soil-moisture % vs the saturation/target % line), so
    /// a later client phase can re-render the soil reason. `value` = the soil %
    /// the verdict rode (raw or quarantine-inferred), `threshold` = the % line it
    /// crossed (saturation_pct for a saturation skip, target_min_pct for a
    /// soil_floor run). Both `None` for non-soil (global / override / condition)
    /// decisions. ADDITIVE; changes no decision. `#[serde(default)]`.
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub threshold: Option<f64>,
}

impl Default for ZoneVerdict {
    fn default() -> Self {
        Self {
            zone_slug: String::new(),
            zone_name: String::new(),
            verdict: "run".into(),
            reason: String::new(),
            source: "default".into(),
            multiplier: 1.0,
            reason_code: String::new(),
            value: None,
            threshold: None,
        }
    }
}

/// One faulted soil probe: configured on a zone but producing no valid
/// (> 0%) reading. Detected by the refresher from sensor_history (a dead
/// WH51 keeps writing 0.0 rows, so the last above-zero epoch is the
/// persistence signal). Lives here (the shared serde contract) so the
/// hydrate-side health banner can deserialize it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SoilProbeFault {
    pub zone_slug: String,
    pub zone_name: String,
    /// The configured sensor spec (e.g. `source:ecowitt_gw:soilmoisture2`).
    pub sensor_id: String,
    /// Epoch of the channel's last reading above 0.0; None when the
    /// channel has never produced a valid value.
    #[serde(default)]
    pub since_epoch: Option<i64>,
}

/// One Home Assistant helper the 0.7.22 adoption pass has handled.
///
/// Both a marker and an audit record. The pass consults the entity ids in
/// this list to decide what is left to do, so idempotency never rests on
/// inspecting a config value: `FileConfigStore::save` serializes every
/// default out explicitly, which makes "the value still looks like the
/// default" carry no information about whether a human typed it. The same
/// rule `seeded_source_ids` and `priority_repaired_ids` already follow.
///
/// It is also what the migration notice renders, and what answers "why is my
/// max wind 12" six months later from the config file alone.
///
/// Lives here rather than in `config::schema` because `config` is ssr-only
/// and the notice component compiles for hydrate as well.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ssr", derive(schemars::JsonSchema))]
pub struct HaAdoptedHelper {
    /// The entity id, e.g. `input_number.irrigation_max_wind_mph`. The
    /// idempotency key: present here means the read is retired, whatever the
    /// outcome was.
    pub entity: String,
    /// `adopted`: the entity was present and parseable and its value is now
    /// LocalSky's. `not_found`: the entity was absent from /api/states.
    /// `unreadable`: present but holding something that is not a value this
    /// field can take. `kept_local`: LocalSky's own store already held a live
    /// operator answer, so the helper's value was not taken. Every outcome
    /// retires that entity's read; they are recorded apart so the notice can
    /// tell the truth about which happened.
    pub outcome: String,
    /// Where the value lives now, e.g. `engine.skip_rules.max_wind_mph`.
    pub target: String,
    /// The value taken from Home Assistant, as text. None unless adopted.
    #[serde(default)]
    pub adopted_value: Option<String>,
    /// What the helper actually held, when that differs from what was
    /// adopted. Set only where a threshold sat outside the range LocalSky can
    /// represent and was clamped to the nearest end, so the notice can print
    /// both numbers instead of showing a value the owner never set. ADDITIVE:
    /// absent on every record written before it existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_value: Option<String>,
    /// What LocalSky held before the pass, as text. Present on every outcome:
    /// on a non-adoption it is the value LocalSky keeps using.
    #[serde(default)]
    pub previous_value: Option<String>,
    /// When the pass handled this entity.
    #[serde(default)]
    pub epoch: i64,
}

impl HaAdoptedHelper {
    /// True when the value LocalSky now uses differs from what it held
    /// before. The notice prints both numbers for these and says "nothing
    /// changed" for the rest.
    pub fn changed_the_value(&self) -> bool {
        self.outcome == "adopted" && self.adopted_value != self.previous_value
    }
}

/// What-If overrides for the Simulator. Every field is an absolute value
/// (None = leave today's reading unchanged). The server seeds baseline
/// Inputs from the live SkipCheck, overrides the Some fields, and re-runs
/// the exact production ladder.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SimRequest {
    pub temp_now_f: Option<f64>,
    pub humidity_now_pct: Option<f64>,
    pub wind_now_mph: Option<f64>,
    pub rain_today_in: Option<f64>,
    pub rain_intensity_now_in_hr: Option<f64>,
    pub forecast_in: Option<f64>,
    pub rain_tomorrow_prob_pct: Option<u32>,
    pub rain_next_4h_in: Option<f64>,
    pub wind_max_today_mph: Option<f64>,
    pub temp_max_3day_f: Option<f64>,
    pub rain_3day_weighted_in: Option<f64>,
    /// Optional ad-hoc Rhai skip rule to test against the hypothetical
    /// inputs (author + preview a rule before adding it to config). Applied
    /// augment-only: only consulted when the built-in verdict is "run".
    #[serde(default)]
    pub test_script: Option<String>,
}

/// Simulator response: today's real decision vs the hypothetical, both as
/// full traces so the UI can diff which rules changed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SimResult {
    pub baseline: DecisionTrace,
    pub hypothetical: DecisionTrace,
}

#[cfg(test)]
mod rollup_predicate_tests {
    use super::*;

    fn verdict(slug: &str, v: &str) -> ZoneVerdict {
        ZoneVerdict {
            zone_slug: slug.into(),
            zone_name: slug.into(),
            verdict: v.into(),
            reason: String::new(),
            source: "global".into(),
            multiplier: 1.0,
            reason_code: String::new(),
            value: None,
            threshold: None,
        }
    }

    fn zone(slug: &str, planned: u32, v: Option<&str>) -> ZoneState {
        ZoneState {
            slug: slug.into(),
            planned_run_seconds: planned,
            verdict: v.map(|v| verdict(slug, v)),
            ..Default::default()
        }
    }

    /// The mixed fixture every rollup rides: one zone waters, one skips
    /// with leftover planned seconds, one holds at zero under a run
    /// verdict (the normal soil-model state), one is undecided. Only the
    /// first and the undecided-with-seconds count as watering, so the
    /// hero tiles, the Zones KPI strip, and the cards state one truth.
    #[test]
    fn waters_next_run_is_skip_aware_and_hold_aware() {
        let mut s = IrrigationSnapshot::default();
        s.zones = vec![
            zone("waters", 1200, Some("run")),
            zone("skips_with_leftover", 900, Some("skip")),
            zone("holds_at_zero", 0, Some("run")),
            zone("undecided", 600, None),
            zone("undecided_idle", 0, None),
        ];
        let watering: Vec<&str> = s
            .zones
            .iter()
            .filter(|z| s.zone_waters_next_run(z))
            .map(|z| z.slug.as_str())
            .collect();
        assert_eq!(watering, vec!["waters", "undecided"]);
        let skipping: Vec<&str> = s
            .zones
            .iter()
            .filter(|z| s.zone_skips_next_run(z))
            .map(|z| z.slug.as_str())
            .collect();
        assert_eq!(skipping, vec!["skips_with_leftover"]);
    }

    /// The evidence-census fields are additive AND self-erasing: a row
    /// that never got a soil block (every weekly row pre-soil, the
    /// pinned wire snapshots, the starved cold start) serializes
    /// byte-identically to 0.7.22, and legacy JSON without the keys
    /// reads back as zero.
    #[test]
    fn soil_evidence_fields_skip_when_zero_and_default_on_legacy_json() {
        let bare = serde_json::to_string(&WaterBudget::default()).unwrap();
        assert!(!bare.contains("soil_evidence_days"), "{bare}");
        assert!(!bare.contains("soil_fallback_days"), "{bare}");
        let b = WaterBudget {
            soil_evidence_days: 5,
            soil_fallback_days: 9,
            ..Default::default()
        };
        let full = serde_json::to_string(&b).unwrap();
        assert!(full.contains("\"soil_evidence_days\":5"), "{full}");
        assert!(full.contains("\"soil_fallback_days\":9"), "{full}");
        let back: WaterBudget = serde_json::from_str(&bare).unwrap();
        assert_eq!(back.soil_evidence_days, 0);
        assert_eq!(back.soil_fallback_days, 0);
    }

    /// The snapshot-level verdict list is the fallback when a zone's own
    /// verdict is not back-filled, mirroring the engine's lookup order.
    #[test]
    fn waters_next_run_falls_back_to_the_verdict_list() {
        let mut s = IrrigationSnapshot::default();
        s.zones = vec![zone("front", 900, None)];
        s.zone_verdicts = vec![verdict("front", "skip")];
        assert!(!s.zone_waters_next_run(&s.zones[0]));
        assert!(s.zone_skips_next_run(&s.zones[0]));
    }
}
#[cfg(test)]
mod honest_reading_tests {
    use super::SkipCheck;

    /// The placeholder for "no overnight low" is 0.0, which looks exactly
    /// like a hard freeze, and a freeze gate reads this field.
    #[test]
    fn a_missing_overnight_low_is_none_not_zero() {
        let mut s = SkipCheck::default();
        s.temp_min_24h_f = 0.0;
        s.temp_min_24h_valid = false;
        assert_eq!(s.temp_min_24h(), None);

        s.temp_min_24h_valid = true;
        assert_eq!(
            s.temp_min_24h(),
            Some(0.0),
            "a real zero is still a reading"
        );

        s.temp_min_24h_f = 41.0;
        assert_eq!(s.temp_min_24h(), Some(41.0));
    }
}

#[cfg(test)]
mod run_state_tests {
    use super::{RunState, ZoneState};

    /// A controller that cannot report state is not the same as a zone
    /// that is off, and reading the bare flag cannot tell them apart.
    ///
    /// On a fire-and-forget board the difference matters: unknown means
    /// the water may be on right now.
    #[test]
    fn unknown_is_not_idle() {
        let mut z = ZoneState::default();
        z.running_known = false;
        z.running = false;
        assert_eq!(z.run_state(), RunState::Unknown);
        assert!(!z.is_running(), "never claim water is moving on a guess");
        assert!(z.may_be_running(), "but never assume it is not, either");
    }

    #[test]
    fn a_known_zone_reads_both_ways_consistently() {
        let mut z = ZoneState::default();
        z.running_known = true;

        z.running = true;
        assert_eq!(z.run_state(), RunState::Running);
        assert!(z.is_running());
        assert!(z.may_be_running());

        z.running = false;
        assert_eq!(z.run_state(), RunState::Idle);
        assert!(!z.is_running());
        assert!(!z.may_be_running(), "a known-idle zone is safe to start");
    }
}

#[cfg(test)]
mod verdict_coherence_tests {
    use super::SkipCheck;

    /// The four fields describing one decision cannot drift, because
    /// there is one way to change them.
    #[test]
    fn deciding_moves_every_field_that_must_agree() {
        let mut s = SkipCheck::default();
        s.decide("skip", "Freeze risk now".into(), "freeze_now".into());
        assert!(s.will_skip);
        assert_eq!(s.verdict, "skip");
        assert_eq!(s.reason_code, "freeze_now");
        assert!(s.is_coherent());

        s.decide("run", String::new(), "run".into());
        assert!(!s.will_skip, "will_skip follows the verdict");
        assert!(s.is_coherent());

        s.decide(
            "run_extended",
            "Heat advisory".into(),
            "heat_advisory".into(),
        );
        assert!(!s.will_skip, "an extended run is still a run");
        assert!(s.is_coherent());
    }

    /// Nothing outside this module writes `skip_check.verdict` or
    /// `will_skip` directly.
    ///
    /// Four call sites used to rewrite the decision after the engine had
    /// spoken, each responsible for remembering all four fields.
    /// Forgetting one does not fail: it leaves a snapshot claiming to run
    /// while a boolean says it will skip, and different surfaces read
    /// different fields. `decide` and `revise_verdict` are the way in.
    #[test]
    fn no_caller_writes_the_decision_fields_by_hand() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                let name = path
                    .file_name()
                    .and_then(|f| f.to_str())
                    .unwrap_or_default()
                    .to_string();
                // This module owns the fields; demo and test fixtures build
                // whole values rather than mutating a live decision.
                if name == "snapshot.rs" || name == "demo_data.rs" {
                    continue;
                }
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                for (n, line) in body.lines().enumerate() {
                    let t = line.trim();
                    if t.starts_with("//") {
                        continue;
                    }
                    for pat in ["skip_check.verdict =", "skip_check.will_skip ="] {
                        // Assignment, not comparison: `== "run"` contains
                        // the same characters and is a read.
                        let is_assignment = t
                            .find(pat)
                            .map(|at| !t[at + pat.len()..].starts_with('='))
                            .unwrap_or(false);
                        if t.contains(pat) && is_assignment {
                            offenders.push(format!("{name}:{}: {t}", n + 1));
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "change a verdict with SkipCheck::decide or revise_verdict, so every              field that must agree moves together:
  {}",
            offenders.join("
  ")
        );
    }

    /// Revising the verdict keeps the reason code, which is what the
    /// post-engine passes actually want: the cause has not changed, the
    /// yard's answer to it has.
    #[test]
    fn revising_keeps_the_cause_and_moves_the_answer() {
        let mut s = SkipCheck::default();
        s.decide("skip", "Rain in 3 days".into(), "rain_3day".into());
        s.revise_verdict("run", "Rain in 3 days. Soil zones ride through.".into());
        assert!(!s.will_skip);
        assert_eq!(s.verdict, "run");
        assert_eq!(s.reason_code, "rain_3day", "the cause is unchanged");
        assert!(s.is_coherent());
    }
}
