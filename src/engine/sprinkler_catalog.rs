// Catalog defaults for sprinkler precipitation rate, mm/hr. Mid-band
// typical residential values: operators with measurements override per
// zone via ZoneConfig.precip_rate_mm_hr from the settings UI.

use crate::config::schema::SprinklerType;

/// Resolve the precipitation rate (mm/hr) for a sprinkler type when the
/// operator has not measured it. Numbers are middle-of-band typical
/// values for residential heads; the engine will use these to size run
/// durations until the catch-cup measurement is supplied.
pub fn catalog_precip_rate_mm_hr(t: SprinklerType) -> f64 {
    use SprinklerType::*;
    match t {
        // Gear-driven and impact rotors: slower coverage, distributed
        // throw. ~10 mm/hr is a sane Hunter/Rain Bird mid-range.
        Rotor => 10.0,
        // Fixed-head spray nozzles: tight throw, fast precipitation.
        // 38 mm/hr is the Hunter Pro-Adjustable / MP-1500 typical.
        Spray => 38.0,
        // Matched-precipitation rotators: designed to match spray
        // throughput at slower coverage; ~14 mm/hr typical.
        MpRotator => 14.0,
        // Residential drip / dripline: gallons per hour translates to a
        // very low equivalent precipitation rate. 6 mm/hr is the
        // catalog default for 1 gph emitters at 12" spacing.
        Drip => 6.0,
        // Bubblers throw a lot of water in a small area; treat as a
        // high effective precipitation rate.
        Bubbler => 50.0,
        // Unknown / mixed setups: pick a generic middle value so the
        // engine still produces a reasonable schedule.
        Other => 25.0,
    }
}

/// Effective precipitation rate (mm/hr) for a zone: explicit
/// `precip_rate_mm_hr` override when present, otherwise catalog default
/// keyed off `sprinkler_type`.
pub fn effective_precip_rate_mm_hr(
    sprinkler_type: SprinklerType,
    override_value: Option<f64>,
) -> f64 {
    override_value
        .filter(|v| *v > 0.0)
        .unwrap_or_else(|| catalog_precip_rate_mm_hr(sprinkler_type))
}
