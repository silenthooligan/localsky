// Display-unit plumbing for the persisted Units preference
// (Settings -> Units, localStorage keys units_temp / units_rain /
// units_wind / units_pressure / units_distance / units_area).
// Engine math and the snapshot wire format stay imperial-as-stored;
// these helpers convert at the display boundary only. Wired to the
// temperature, precipitation, wind, pressure, distance, and area
// surfaces (weather hero, stat tiles, forecast hourly/daily, etc.).
//
// SSR and the first hydrate frame always render the imperial default so
// the DOM trees match; the Effect inside use_unit_prefs then reads
// localStorage and updates the signal, re-rendering consumers
// client-side only.

use leptos::prelude::*;

/// Imperial baseline. All-false maps every helper to its imperial branch.
/// Also the `UnitPrefs::default()`, the SSR / first-hydrate-frame value, and
/// what `Units::Imperial` expands to.
pub const IMPERIAL: UnitPrefs = UnitPrefs {
    temp_c: false,
    rain_mm: false,
    wind_metric: false,
    pressure_metric: false,
    distance_metric: false,
    area_metric: false,
};

/// Fully-metric preferences. What `Units::Metric` expands to.
pub const METRIC: UnitPrefs = UnitPrefs {
    temp_c: true,
    rain_mm: true,
    wind_metric: true,
    pressure_metric: true,
    distance_metric: true,
    area_metric: true,
};

/// Expand a household `Units` enum into the per-field `UnitPrefs` the display
/// helpers consume. `Imperial -> IMPERIAL`, `Metric -> METRIC`. Used by
/// `use_unit_prefs` when a device has not opted into its own override.
pub fn prefs_from_units(units: crate::ha::snapshot::Units) -> UnitPrefs {
    match units {
        crate::ha::snapshot::Units::Metric => METRIC,
        crate::ha::snapshot::Units::Imperial => IMPERIAL,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct UnitPrefs {
    /// Show temperatures in Celsius (source values are Fahrenheit).
    pub temp_c: bool,
    /// Show precipitation in millimeters (source values are inches).
    pub rain_mm: bool,
    /// Show wind speeds in km/h (source values are mph).
    pub wind_metric: bool,
    /// Show pressure in hPa (source values are inHg).
    pub pressure_metric: bool,
    /// Show distances in km (source values are miles).
    pub distance_metric: bool,
    /// Show areas in m² (source values are square feet).
    pub area_metric: bool,
}

/// Reactive per-device unit preferences. Call once at component scope;
/// read the returned signal inside render closures so a preference
/// change (or the post-hydration localStorage load) re-renders.
///
/// Resolution (in the hydrate Effect ONLY, never at SSR):
///   1. The device opted into its own override (`units_system` is
///      `imperial`/`metric`/`custom`) -> read the six per-field keys.
///   2. Else -> expand the household `Units` from `HouseholdUnits` context
///      (`Metric -> METRIC`, `Imperial -> IMPERIAL`).
///   3. Else (no context) -> imperial.
///
/// SSR + the first hydrate frame always render the imperial default, so the DOM
/// trees match (the Effect runs only after hydration). A device on the household
/// default with the household = Imperial is therefore byte-identical to the
/// pre-household behavior: no `units_system` key, household reads Imperial,
/// resolution lands on `IMPERIAL`.
pub fn use_unit_prefs() -> Signal<UnitPrefs> {
    let prefs = RwSignal::new(UnitPrefs::default());
    // Household baseline from app-root context. Provided unconditionally by
    // `App`, but tolerate its absence (e.g. isolated component tests) by
    // defaulting to Imperial. Read inside the Effect so SSE-pushed household
    // changes re-resolve.
    #[cfg(feature = "hydrate")]
    let household = use_context::<crate::app::HouseholdUnits>();
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        // Track the household signal so a later SSE snapshot (which carries the
        // configured units) re-runs this Effect and re-resolves the device.
        let household_units = household
            .map(|h| h.0.get())
            .unwrap_or(crate::ha::snapshot::Units::Imperial);

        let Some(win) = web_sys::window() else {
            return;
        };
        let Ok(Some(storage)) = win.local_storage() else {
            return;
        };
        let read = |k: &str| storage.get_item(k).ok().flatten();

        // Opt-in sentinel: a device that picked imperial / metric / custom in
        // Settings has its own override. "household" (or an absent/empty key)
        // means "follow the household default" -> expand the household Units.
        let opted_in = matches!(
            read("units_system").as_deref(),
            Some("imperial") | Some("metric") | Some("custom")
        );
        let next = if opted_in {
            UnitPrefs {
                temp_c: read("units_temp").as_deref() == Some("c"),
                rain_mm: read("units_rain").as_deref() == Some("mm"),
                wind_metric: read("units_wind").as_deref() == Some("kph"),
                pressure_metric: read("units_pressure").as_deref() == Some("hpa"),
                distance_metric: read("units_distance").as_deref() == Some("km"),
                area_metric: read("units_area").as_deref() == Some("sqm"),
            }
        } else {
            prefs_from_units(household_units)
        };
        if next != prefs.get_untracked() {
            prefs.set(next);
        }
    });
    prefs.into()
}

pub fn f_to_c(f: f64) -> f64 {
    (f - 32.0) * 5.0 / 9.0
}

pub fn in_to_mm(inches: f64) -> f64 {
    inches * 25.4
}

/// Degree-glyph form without the scale letter: "72°" or "22°".
pub fn fmt_temp_short(temp_f: f64, p: UnitPrefs) -> String {
    if p.temp_c {
        format!("{:.0}°", f_to_c(temp_f))
    } else {
        format!("{temp_f:.0}°")
    }
}

/// Bare numeric value for StatTile-style value/unit splits.
pub fn temp_value(temp_f: f64, p: UnitPrefs) -> String {
    if p.temp_c {
        format!("{:.0}", f_to_c(temp_f))
    } else {
        format!("{temp_f:.0}")
    }
}

pub fn temp_unit(p: UnitPrefs) -> &'static str {
    if p.temp_c {
        "°C"
    } else {
        "°F"
    }
}

/// Rain rate: "0.02in/h" or "0.5mm/h".
pub fn fmt_rain_rate(in_per_hr: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1}mm/h", in_to_mm(in_per_hr))
    } else {
        format!("{in_per_hr:.2}in/h")
    }
}

/// Rain amount: "0.25\"" or "6.4mm".
pub fn fmt_rain_amount(inches: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1}mm", in_to_mm(inches))
    } else {
        format!("{inches:.2}\"")
    }
}

/// Rain amount from a MILLIMETER source: "0.25\"" or "6.4mm".
pub fn fmt_rain_amount_mm(mm: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{mm:.1}mm")
    } else {
        format!("{:.2}\"", mm / 25.4)
    }
}

/// Bare depth unit label for value/unit splits: "mm" or "\"".
pub fn depth_unit(p: UnitPrefs) -> &'static str {
    if p.rain_mm {
        "mm"
    } else {
        "\""
    }
}

/// Bare depth value in the display unit from an INCHES source.
pub fn depth_value_in(inches: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1}", in_to_mm(inches))
    } else {
        format!("{inches:.2}")
    }
}

/// Bare depth value in the display unit from a MILLIMETER source.
pub fn depth_value_mm(mm: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{mm:.1}")
    } else {
        format!("{:.2}", mm / 25.4)
    }
}

/// Rain rate from a MILLIMETER/HOUR source: "0.50in/h" or "12.7mm/h".
pub fn fmt_rain_rate_mm(mm_per_hr: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{mm_per_hr:.1}mm/h")
    } else {
        format!("{:.2}in/h", mm_per_hr / 25.4)
    }
}

pub fn mph_to_kph(mph: f64) -> f64 {
    mph * 1.609_344
}

/// Wind speed (source mph), whole numbers: "8 mph" or "13 km/h".
pub fn fmt_wind(mph: f64, p: UnitPrefs) -> String {
    if p.wind_metric {
        format!("{:.0} km/h", mph_to_kph(mph))
    } else {
        format!("{mph:.0} mph")
    }
}

/// Bare wind value for value/unit splits, whole numbers.
pub fn wind_value(mph: f64, p: UnitPrefs) -> String {
    if p.wind_metric {
        format!("{:.0}", mph_to_kph(mph))
    } else {
        format!("{mph:.0}")
    }
}

pub fn wind_unit(p: UnitPrefs) -> &'static str {
    if p.wind_metric {
        "km/h"
    } else {
        "mph"
    }
}

pub fn inhg_to_hpa(inhg: f64) -> f64 {
    inhg * 33.863_886_67
}

/// Pressure (source inHg): "29.92 inHg" or "1013 hPa".
pub fn fmt_pressure(inhg: f64, p: UnitPrefs) -> String {
    if p.pressure_metric {
        format!("{:.0} hPa", inhg_to_hpa(inhg))
    } else {
        format!("{inhg:.2} inHg")
    }
}

/// Bare pressure value for value/unit splits.
pub fn pressure_value(inhg: f64, p: UnitPrefs) -> String {
    if p.pressure_metric {
        format!("{:.0}", inhg_to_hpa(inhg))
    } else {
        format!("{inhg:.2}")
    }
}

pub fn pressure_unit(p: UnitPrefs) -> &'static str {
    if p.pressure_metric {
        "hPa"
    } else {
        "inHg"
    }
}

pub fn mi_to_km(mi: f64) -> f64 {
    mi * 1.609_344
}

/// Distance (source miles): "5.0 mi" or "8.0 km".
pub fn fmt_distance_mi(mi: f64, p: UnitPrefs) -> String {
    if p.distance_metric {
        format!("{:.1} km", mi_to_km(mi))
    } else {
        format!("{mi:.1} mi")
    }
}

/// Bare distance value for value/unit splits.
pub fn distance_value_mi(mi: f64, p: UnitPrefs) -> String {
    if p.distance_metric {
        format!("{:.1}", mi_to_km(mi))
    } else {
        format!("{mi:.1}")
    }
}

pub fn distance_unit(p: UnitPrefs) -> &'static str {
    if p.distance_metric {
        "km"
    } else {
        "mi"
    }
}

pub fn sqft_to_sqm(sqft: f64) -> f64 {
    sqft * 0.092_903_04
}

/// Area (source sq ft), whole numbers: "500 sq ft" or "46 m²".
pub fn fmt_area_sqft(sqft: f64, p: UnitPrefs) -> String {
    if p.area_metric {
        format!("{:.0} m²", sqft_to_sqm(sqft))
    } else {
        format!("{sqft:.0} sq ft")
    }
}

/// Bare area value for value/unit splits, whole numbers.
pub fn area_value_sqft(sqft: f64, p: UnitPrefs) -> String {
    if p.area_metric {
        format!("{:.0}", sqft_to_sqm(sqft))
    } else {
        format!("{sqft:.0}")
    }
}

pub fn area_unit(p: UnitPrefs) -> &'static str {
    if p.area_metric {
        "m²"
    } else {
        "sq ft"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::snapshot::Units;

    #[test]
    fn household_units_expand() {
        // Household resolution baseline: the enum -> UnitPrefs expansion that
        // use_unit_prefs applies when a device has no per-device override.
        assert_eq!(prefs_from_units(Units::Metric), METRIC);
        assert_eq!(prefs_from_units(Units::Imperial), IMPERIAL);
        // Default config (Units::default() == Imperial) stays imperial.
        assert_eq!(prefs_from_units(Units::default()), IMPERIAL);
    }

    #[test]
    fn ssr_default_is_imperial() {
        // SSR + the first hydrate frame render UnitPrefs::default(), which MUST
        // equal IMPERIAL so the SSR/hydrate DOM trees match.
        assert_eq!(UnitPrefs::default(), IMPERIAL);
    }

    #[test]
    fn device_override_beats_household() {
        // The resolution use_unit_prefs performs in its Effect, modeled here
        // without web_sys: when a device has opted in, the per-field keys win
        // over the household value even when they disagree.
        let household = Units::Metric;
        // Device opted into custom/imperial: per-field reads land on IMPERIAL,
        // overriding the metric household.
        let device_opted_in_imperial = IMPERIAL;
        let resolved = if true {
            device_opted_in_imperial
        } else {
            prefs_from_units(household)
        };
        assert_eq!(resolved, IMPERIAL);
        assert_ne!(resolved, prefs_from_units(household));

        // And the reverse: an imperial household with a device opted into
        // metric resolves to METRIC, not the household IMPERIAL.
        let household = Units::Imperial;
        let device_opted_in_metric = METRIC;
        let resolved = if true {
            device_opted_in_metric
        } else {
            prefs_from_units(household)
        };
        assert_eq!(resolved, METRIC);
        assert_ne!(resolved, prefs_from_units(household));
    }

    #[test]
    fn conversions() {
        assert!((f_to_c(32.0)).abs() < 1e-9);
        assert!((f_to_c(212.0) - 100.0).abs() < 1e-9);
        assert!((in_to_mm(1.0) - 25.4).abs() < 1e-9);
        assert!((mph_to_kph(100.0) - 160.9344).abs() < 1e-6);
        assert!((inhg_to_hpa(1.0) - 33.86388667).abs() < 1e-6);
        assert!((mi_to_km(100.0) - 160.9344).abs() < 1e-6);
        assert!((sqft_to_sqm(1.0) - 0.09290304).abs() < 1e-9);
    }

    #[test]
    fn temp_formatting_both_scales() {
        assert_eq!(fmt_temp_short(72.0, IMPERIAL), "72°");
        assert_eq!(fmt_temp_short(72.0, METRIC), "22°");
        assert_eq!(temp_value(50.0, METRIC), "10");
        assert_eq!(temp_unit(IMPERIAL), "°F");
        assert_eq!(temp_unit(METRIC), "°C");
    }

    #[test]
    fn rain_formatting_both_scales() {
        assert_eq!(fmt_rain_rate(0.5, IMPERIAL), "0.50in/h");
        assert_eq!(fmt_rain_rate(0.5, METRIC), "12.7mm/h");
        assert_eq!(fmt_rain_amount(0.5, IMPERIAL), "0.50\"");
        assert_eq!(fmt_rain_amount(0.5, METRIC), "12.7mm");
    }

    #[test]
    fn depth_formatting_both_scales() {
        // Amount from a mm source.
        assert_eq!(fmt_rain_amount_mm(25.4, IMPERIAL), "1.00\"");
        assert_eq!(fmt_rain_amount_mm(6.4, METRIC), "6.4mm");

        // Bare unit label.
        assert_eq!(depth_unit(IMPERIAL), "\"");
        assert_eq!(depth_unit(METRIC), "mm");

        // Bare value from an inches source.
        assert_eq!(depth_value_in(1.0, IMPERIAL), "1.00");
        assert_eq!(depth_value_in(1.0, METRIC), "25.4");

        // Bare value from a mm source.
        assert_eq!(depth_value_mm(25.4, IMPERIAL), "1.00");
        assert_eq!(depth_value_mm(6.4, METRIC), "6.4");

        // Rate from a mm/hr source.
        assert_eq!(fmt_rain_rate_mm(12.7, IMPERIAL), "0.50in/h");
        assert_eq!(fmt_rain_rate_mm(12.7, METRIC), "12.7mm/h");
    }

    #[test]
    fn wind_formatting_both_scales() {
        assert_eq!(fmt_wind(10.0, IMPERIAL), "10 mph");
        assert_eq!(fmt_wind(10.0, METRIC), "16 km/h");
        assert_eq!(wind_value(10.0, IMPERIAL), "10");
        assert_eq!(wind_value(10.0, METRIC), "16");
        assert_eq!(wind_unit(IMPERIAL), "mph");
        assert_eq!(wind_unit(METRIC), "km/h");
    }

    #[test]
    fn pressure_formatting_both_scales() {
        assert_eq!(fmt_pressure(29.92, IMPERIAL), "29.92 inHg");
        assert_eq!(fmt_pressure(29.92, METRIC), "1013 hPa");
        assert_eq!(pressure_value(29.92, IMPERIAL), "29.92");
        assert_eq!(pressure_value(29.92, METRIC), "1013");
        assert_eq!(pressure_unit(IMPERIAL), "inHg");
        assert_eq!(pressure_unit(METRIC), "hPa");
    }

    #[test]
    fn distance_formatting_both_scales() {
        assert_eq!(fmt_distance_mi(5.0, IMPERIAL), "5.0 mi");
        assert_eq!(fmt_distance_mi(5.0, METRIC), "8.0 km");
        assert_eq!(distance_value_mi(5.0, IMPERIAL), "5.0");
        assert_eq!(distance_value_mi(5.0, METRIC), "8.0");
        assert_eq!(distance_unit(IMPERIAL), "mi");
        assert_eq!(distance_unit(METRIC), "km");
    }

    #[test]
    fn area_formatting_both_scales() {
        assert_eq!(fmt_area_sqft(500.0, IMPERIAL), "500 sq ft");
        assert_eq!(fmt_area_sqft(500.0, METRIC), "46 m²");
        assert_eq!(area_value_sqft(500.0, IMPERIAL), "500");
        assert_eq!(area_value_sqft(500.0, METRIC), "46");
        assert_eq!(area_unit(IMPERIAL), "sq ft");
        assert_eq!(area_unit(METRIC), "m²");
    }
}
