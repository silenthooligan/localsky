// Pure-logic engine. Populated incrementally across Phase 3.
//
// Intentional invariants:
// - No I/O, no async required.
// - No HA-specific types leak in. Inputs come from MergedSnapshot + Config.
// - All thresholds and coefficients are typed config fields, not consts.

pub mod budget;
pub mod conditions;
pub mod cycle_soak;
pub mod et0;
pub mod forecast_bias;
pub mod restrictions;
pub mod scripting;
pub mod skip_rules;
pub mod soil_catalog;
pub mod soil_forecast;
pub mod species_catalog;
pub mod sprinkler_catalog;
pub mod sunrise;
pub mod verdict_strip;
pub mod water_balance;

pub use budget::{compute_zone as compute_zone_budget, BudgetGlobals, ZoneBudgetInputs};
pub use cycle_soak::{split as cycle_split, CycleSegment};
pub use et0::{compute as compute_et0, Et0Diagnostics, Et0Inputs, Et0Result};
pub use forecast_bias::{BiasModel, Observation as ForecastObservation};
pub use skip_rules::{
    builtin_rule_catalog, decide_traced, et_heat_multiplier, evaluate as evaluate_skip,
    evaluate_with as evaluate_skip_with, force_overrode_guard, heat_index_f, Inputs as SkipInputs,
    PROTECTED_RULES,
};
pub use soil_catalog::{infiltration_mm_hr, lookup as soil_profile, raw_mm, taw_mm, SoilProfile};
pub use soil_forecast::{project_zone as project_soil_forecast, ZoneSoilInputs};
pub use species_catalog::{
    kc_at_doy, kc_at_doy_lat, lookup as species_profile, shift_doy_for_hemisphere, SpeciesProfile,
};
pub use sprinkler_catalog::{catalog_precip_rate_mm_hr, effective_precip_rate_mm_hr};
pub use verdict_strip::compute as compute_verdict_strip;
pub use water_balance::{
    etc_mm, refill_runtime_seconds, should_irrigate, step as step_water_balance, summarize,
    ZoneBalanceSummary, ZoneWaterState,
};
