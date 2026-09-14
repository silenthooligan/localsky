// The control surface: what an operator has told the engine to do
// regardless of what the weather says.
//
// A pause with an end, a pause without one, tomorrow's override, a
// sticky override for the yard and one per zone, and dry-run mode. The
// engine reads these as inputs; `persistence::irrigation_control` is
// where they are kept between restarts, and the Home Assistant path
// filled the same shape from its helper entities.

use std::collections::HashMap;

/// The native control surface: vacation pause + one-day override. Mirrors
/// the two HA helpers (`input_datetime.irrigation_pause_until` +
/// `input_select.irrigation_override_tomorrow`) so `build_from_map` can
/// consume either source identically.
#[derive(Debug, Clone)]
pub struct IrrigationControlState {
    /// UTC epoch the vacation pause runs until; 0 = no pause.
    pub pause_until_epoch: i64,
    /// One-day override for tomorrow's verdict: "none" | "skip" | "run".
    pub override_tomorrow: String,
    /// Sticky global override (holds until set back to auto):
    /// "auto" | "skip" | "run". Beats the engine verdict; a per-zone
    /// override beats this. Distinct from the one-day override_tomorrow.
    pub global_override: String,
    /// Sticky per-zone overrides: zone slug -> "skip" | "run". A zone absent
    /// from the map is "auto". Loaded alongside the singleton row so the
    /// snapshot builder + engine get the whole control surface in one read.
    pub zone_overrides: HashMap<String, String>,
    /// Indefinite vacation pause (M0017). The native home of what used to be
    /// `input_boolean.irrigation_pause`: an on/off hard skip on every zone,
    /// distinct from `pause_until_epoch`, which expires by itself.
    pub is_paused: bool,
    /// Dry-run mode (M0017). The native home of what used to be
    /// `input_boolean.irrigation_dry_run`: the engine decides normally and
    /// then returns a skip with reason "Dry-run mode", so nothing dispatches.
    /// Not the dry_run CONTROLLER kind, which produces a run verdict and
    /// synthesizes run rows; this one waters nothing and records nothing.
    pub is_dry_run: bool,
}

impl Default for IrrigationControlState {
    fn default() -> Self {
        Self {
            pause_until_epoch: 0,
            override_tomorrow: "none".to_string(),
            global_override: "auto".to_string(),
            zone_overrides: HashMap::new(),
            is_paused: false,
            is_dry_run: false,
        }
    }
}
