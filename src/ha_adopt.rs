// The seven Home Assistant helpers LocalSky used to read, as a catalog.
//
// 0.7.22 adopted them: three skip thresholds that had outranked Settings
// and four operator controls, each read once, written into LocalSky's own
// storage and recorded in `ha_adoption`. 0.9.0 removed the adoption pass
// and the reads behind it: no helper is consulted any more, on any
// install. What remains here is the naming the surviving surfaces share:
// the entity ids the migration notice lists, the threshold keys, ranges
// and steps the dashboard sliders and the Home Assistant manifest agree
// on, and where each value lives now.

/// The three skip thresholds. Home Assistant outranked Settings for these
/// until 0.7.22 adopted them.
pub const MAX_WIND: &str = "input_number.irrigation_max_wind_mph";
pub const MIN_TEMP: &str = "input_number.irrigation_min_temp_f";
pub const RAIN_SKIP: &str = "input_number.irrigation_rain_skip_in";

/// The four operator controls, LocalSky's own store since 0.7.22.
pub const PAUSE_UNTIL: &str = "input_datetime.irrigation_pause_until";
pub const OVERRIDE_TOMORROW: &str = "input_select.irrigation_override_tomorrow";
pub const PAUSE_TOGGLE: &str = "input_boolean.irrigation_pause";
pub const DRY_RUN_TOGGLE: &str = "input_boolean.irrigation_dry_run";

/// Every entity the migration handled, in the order the notice lists them.
pub const ENTITIES: [&str; 7] = [
    MAX_WIND,
    MIN_TEMP,
    RAIN_SKIP,
    PAUSE_UNTIL,
    OVERRIDE_TOMORROW,
    PAUSE_TOGGLE,
    DRY_RUN_TOGGLE,
];

/// The four operator controls.
pub const CONTROL_ENTITIES: [&str; 4] =
    [PAUSE_UNTIL, OVERRIDE_TOMORROW, PAUSE_TOGGLE, DRY_RUN_TOGGLE];

/// Accepted range per threshold, matching what `POST /action set_threshold`
/// accepts and what the manifest publishes on its `number` descriptors.
///
/// The wind ceiling is 50 because that is the top of the slider the shipping
/// Home Assistant integration builds for `number.localsky_max_wind_mph` from
/// its own fixed limits (0 to 50 mph, 20 to 60 F, 0 to 1 in). A server bound
/// below it answered 400 to a value LocalSky's own entity offered; the other
/// two integration ranges sit inside the server's.
const MAX_WIND_RANGE: (f64, f64) = (0.0, 50.0);
const MIN_TEMP_RANGE: (f64, f64) = (20.0, 70.0);
/// The rain threshold is a free numeric input rather than a slider, so the
/// bound is physical rather than editorial: ten inches of rain in a day is
/// already past any threshold anyone means.
const RAIN_SKIP_RANGE: (f64, f64) = (0.0, 10.0);

/// Step published alongside the range on the `number` descriptors, so the
/// integration builds each threshold entity on the bound the server enforces
/// rather than on the Home Assistant platform defaults.
pub fn threshold_step(key: &str) -> Option<f64> {
    match key {
        "max_wind_mph" | "min_temp_f" => Some(1.0),
        "rain_skip_in" => Some(0.05),
        _ => None,
    }
}

/// The outcomes a 0.7.22 record can carry.
pub const OUTCOME_ADOPTED: &str = "adopted";
pub const OUTCOME_NOT_FOUND: &str = "not_found";
pub const OUTCOME_UNREADABLE: &str = "unreadable";
/// LocalSky's own store already held an operator answer for this control,
/// so the helper's value was not taken.
pub const OUTCOME_KEPT_LOCAL: &str = "kept_local";
/// The helper entity behind a threshold key ("max_wind_mph"): the name the
/// migration notice shows for it.
pub fn threshold_entity(key: &str) -> Option<&'static str> {
    match key {
        "max_wind_mph" => Some(MAX_WIND),
        "min_temp_f" => Some(MIN_TEMP),
        "rain_skip_in" => Some(RAIN_SKIP),
        _ => None,
    }
}

/// The accepted range for a threshold key. Shared with the write path, with
/// the Settings editor's own bounds, and with the manifest, so a value
/// LocalSky refuses to be given is also the bound its `number` entity carries.
pub fn threshold_range(key: &str) -> Option<(f64, f64)> {
    match key {
        "max_wind_mph" => Some(MAX_WIND_RANGE),
        "min_temp_f" => Some(MIN_TEMP_RANGE),
        "rain_skip_in" => Some(RAIN_SKIP_RANGE),
        _ => None,
    }
}

/// The helper entity behind a toggle key ("irrigation_pause").
pub fn toggle_entity(key: &str) -> Option<&'static str> {
    match key {
        "irrigation_pause" => Some(PAUSE_TOGGLE),
        "irrigation_dry_run" => Some(DRY_RUN_TOGGLE),
        _ => None,
    }
}

/// Where each entity's value lives now. Recorded so the config file answers
/// "why is this number what it is" without anyone reading the source.
pub fn target_of(entity: &str) -> &'static str {
    match entity {
        MAX_WIND => "engine.skip_rules.max_wind_mph",
        MIN_TEMP => "engine.skip_rules.min_temp_f",
        RAIN_SKIP => "engine.skip_rules.rain_skip_in",
        PAUSE_UNTIL => "irrigation_control.pause_until_epoch",
        OVERRIDE_TOMORROW => "irrigation_control.override_tomorrow",
        PAUSE_TOGGLE => "irrigation_control.is_paused",
        DRY_RUN_TOGGLE => "irrigation_control.is_dry_run",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entity_has_a_home_and_a_kind() {
        for e in ENTITIES {
            assert!(!target_of(e).is_empty(), "{e}");
        }
        assert_eq!(target_of("input_boolean.other"), "");
        for k in ["max_wind_mph", "min_temp_f", "rain_skip_in"] {
            assert!(threshold_entity(k).is_some());
            assert!(threshold_range(k).is_some());
            assert!(threshold_step(k).is_some());
        }
        assert_eq!(toggle_entity("irrigation_pause"), Some(PAUSE_TOGGLE));
        assert_eq!(toggle_entity("gravity"), None);
        assert!(CONTROL_ENTITIES.iter().all(|c| ENTITIES.contains(c)));
    }
}
