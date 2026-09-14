// Pure-logic engine. Populated incrementally across Phase 3.
//
// Intentional invariants:
// - No I/O, no async required.
// - No HA-specific types leak in. Inputs come from MergedSnapshot + Config.
// - All thresholds and coefficients are typed config fields, not consts.

// WHAT IS SHARED, AND THE RULE THAT DECIDES IT
//
// A module compiles for the browser too when it is a pure function of its
// arguments. It stays server-side when it reads AMBIENT state the browser
// cannot have the same answer for: the deployment's clock and timezone
// (`crate::timeutil`, which resolves the configured zone, not the
// viewer's), or the Rhai interpreter.
//
// The point of sharing is that the settings UI CALLS these instead of
// carrying a second copy of what they compute. Every hand-typed copy of a
// catalog value or a default this app has grown came from the UI being
// unable to reach the function that already knew the answer.
// Rule TYPES are plain config data and compile everywhere; the
// evaluator inside is gated, because it reads the skip-check inputs.
pub mod calendar;
pub mod clock;
pub mod conditions;
pub mod cycle_soak;
pub mod dispatch_window;
pub mod et0;
pub mod forecast_bias;
pub mod interleave;
pub mod restrictions;
pub mod schedule;
pub mod soil_catalog;
pub mod soil_decisions;
pub mod soil_forecast;
pub mod soil_outlook;
pub mod soil_schedule;
pub mod species_catalog;
pub mod sprinkler_catalog;
pub mod water_balance;
pub mod zone_slug;

// These four used to read the deployment clock directly, which is what
// kept them server-side: a browser resolving a named timezone needs the
// whole timezone database, and its answer would be the viewer's day
// rather than the yard's. They take a `Calendar` now, so the ambient read
// is gone and they compile everywhere the rest of the engine does.
/// The depth of rain that makes a day count as WET, in inches.
///
/// One day, one meaning. This was spelled four times: a constant in the
/// forecast snapshot, another in the tuning report, three bare literals
/// in the verdict strip, and the default for the operator's own
/// already-wet knob. They agreed only because they were all 0.05.
///
/// An operator who raises their already-wet threshold moves one of the
/// four. The drought counter, the seven-day strip and the accuracy
/// scorecard keep the old number, so the product holds two opinions
/// about whether it rained and shows both.
///
/// Callers that have the operator's configured threshold should pass
/// THAT. This is the default for the ones that do not.
pub const WET_DAY_IN: f64 = 0.05;

pub mod agronomy_cfg;
pub mod budget;
pub mod sequence;
pub mod sizing;
pub mod skip_rules;
pub mod sunrise;
pub mod tuning;
pub mod verdict_strip;

// The one module that genuinely cannot cross: it embeds the Rhai
// interpreter for operator condition scripts.
#[cfg(feature = "ssr")]
pub mod scripting;

pub use budget::{
    compute_zone as compute_zone_balance, BalanceGlobals, ZoneBalanceInputs, SESSION_RAIN_DEFER_IN,
};
pub use clock::Tick;
pub use cycle_soak::{split as cycle_split, CycleSegment};
pub use et0::{compute as compute_et0, Et0Diagnostics, Et0Inputs, Et0Result};
pub use forecast_bias::{BiasModel, Observation as ForecastObservation};
pub use skip_rules::et_demand_multiplier;
pub use skip_rules::{
    builtin_rule_catalog, decide_traced, et_heat_multiplier, evaluate as evaluate_skip,
    evaluate_with as evaluate_skip_with, force_overrode_guard, heat_index_f, Inputs as SkipInputs,
    PROTECTED_RULES,
};
pub use soil_catalog::{infiltration_mm_hr, lookup as soil_profile, raw_mm, taw_mm, SoilProfile};
pub use soil_forecast::{project_zone as project_soil_forecast, ZoneSoilInputs};
pub use soil_schedule::{
    admit_zones, plan_zone as plan_soil_zone, AdmissionCandidate, AdmissionOutcome, SoilZonePlan,
    ZoneDayEvidence, ZoneSoilParams, RECON_WINDOW_DAYS,
};
pub use species_catalog::{
    kc_at_doy, kc_at_doy_lat, lookup as species_profile, shift_doy_for_hemisphere, species_slug,
    SpeciesProfile,
};
pub use sprinkler_catalog::sprinkler_slug;
pub use sprinkler_catalog::{catalog_precip_rate_mm_hr, effective_precip_rate_mm_hr};
pub use verdict_strip::compute as compute_verdict_strip;
pub use water_balance::{
    etc_mm, refill_runtime_seconds, should_irrigate, step as step_water_balance, summarize,
    ZoneBalanceSummary, ZoneWaterState,
};
pub use zone_slug::ZoneSlug;
