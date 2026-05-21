// Phase 3D: thin re-export shim. The skip-rule engine moved to
// src/engine/skip_rules.rs with hardcoded constants extracted into
// config.engine.skip_rules (SkipRuleParams). The default values match
// the previous consts so call sites that use evaluate(inputs) without
// passing params get identical verdicts to v0.1.
//
// Existing callers (src/ha/refresher.rs) use `skip_logic::Inputs`,
// `skip_logic::evaluate`, `skip_logic::heat_index_f`, and
// `skip_logic::et_heat_multiplier` -- all re-exported here verbatim.

pub use crate::engine::skip_rules::{
    et_heat_multiplier, evaluate, evaluate_with, heat_index_f, Inputs,
};

// Compatibility constants for any code still referencing the old paths.
// Mirror SkipRuleParams::default() values; remove in a future release
// once internal callers are updated to read from config.
pub const ALREADY_WET_IN: f64 = 0.05;
pub const RAIN_NOW_IN_HR: f64 = 0.01;
pub const RAIN_NEXT_4H_SKIP_IN: f64 = 0.10;
pub const RAIN_3DAY_FACTOR: f64 = 1.5;
pub const HEAT_ADVISORY_TEMP_F: f64 = 95.0;
pub const HEAT_ADVISORY_HUMIDITY_PCT: f64 = 60.0;
pub const HEAT_ADVISORY_DRY_DAYS: u32 = 2;
