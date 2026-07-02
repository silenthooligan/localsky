// Single seam for normalizing a source's reading to LocalSky's canonical
// internal unit (imperial, per the WeatherField enum: AirTempF, WindMph,
// PressureInHg, RainTodayIn, LightningDistanceMi, FlowGpm...).
//
// Every source adapter is contractually required to emit canonical-imperial
// values on the bus. Adapters whose upstream can report metric (HA passthrough
// reading a `°C` sensor, an Ecowitt gateway configured for Celsius/hPa/mm,
// Open-Meteo with a metric unit param) MUST route each reading through
// `to_canonical` with the upstream's declared unit so the engine's ET/skip math
// and the dashboard never see a Celsius number treated as Fahrenheit.
//
// Unknown or absent units pass through unchanged (assume already canonical), so
// a US/imperial source is never double-converted.

use crate::ports::weather_source::WeatherField;

/// Normalize a unit label: drop the degree glyph, trim, lowercase.
/// `"°C"` -> `"c"`, `"km/h"` -> `"km/h"`, `""`/None -> None.
fn norm_unit(unit: Option<&str>) -> Option<String> {
    let u = unit?.replace('\u{00b0}', "");
    let u = u.trim().to_ascii_lowercase();
    if u.is_empty() {
        None
    } else {
        Some(u)
    }
}

/// Convert `value` (in the source's `unit`) to LocalSky's canonical imperial
/// unit for `field`. Absent/unknown/already-imperial units pass through.
pub fn to_canonical(field: WeatherField, value: f64, unit: Option<&str>) -> f64 {
    let Some(u) = norm_unit(unit) else {
        return value;
    };
    use WeatherField::*;
    match field {
        AirTempF | DewPointF => match u.as_str() {
            "c" | "celsius" => value * 9.0 / 5.0 + 32.0,
            "k" | "kelvin" => (value - 273.15) * 9.0 / 5.0 + 32.0,
            _ => value, // f / fahrenheit / unknown
        },
        WindMph | WindGustMph => match u.as_str() {
            "km/h" | "kph" | "kmh" | "kmph" => value * 0.621_371,
            "m/s" | "mps" | "ms" => value * 2.236_936,
            "kn" | "kt" | "kts" | "knot" | "knots" => value * 1.150_779,
            "ft/s" | "fps" => value * 0.681_818,
            _ => value, // mph / unknown
        },
        PressureInHg => match u.as_str() {
            "hpa" | "mbar" | "mb" => value * 0.029_529_983_071_4,
            "kpa" | "cbar" => value * 0.295_299_830_714,
            "pa" => value * 0.000_295_299_830_714,
            "bar" => value * 29.529_983_071_4,
            "mmhg" => value * 0.039_370_1,
            "psi" => value * 2.036_020_375,
            _ => value, // inhg / unknown
        },
        RainTodayIn | RainIntensityInHr => match u.as_str() {
            "mm" | "mm/h" | "mm/hr" | "millimeter" | "millimeters" => value / 25.4,
            "cm" => value / 2.54,
            _ => value, // in / unknown
        },
        LightningDistanceMi => match u.as_str() {
            "km" => value * 0.621_371,
            "m" => value * 0.000_621_371,
            _ => value, // mi / unknown
        },
        FlowGpm => match u.as_str() {
            "l/min" | "lpm" | "l/m" => value * 0.264_172,
            _ => value, // gpm / unknown
        },
        FlowTotalGalToday => match u.as_str() {
            "l" | "liter" | "liters" | "litre" | "litres" => value * 0.264_172,
            _ => value, // gal / unknown
        },
        // Unitless, already-canonical, or non-scalar fields: pass through.
        RhPct | SolarWm2 | UvIndex | Illuminance | WindBearingDeg | LightningCount | Et0Today
        | Pop | LeafWetness | RainTypeStr | ForecastDaily | ForecastHourly => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::weather_source::WeatherField::*;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < 0.01, "{a} != {b}");
    }

    #[test]
    fn temperature_celsius_and_glyph_variants() {
        approx(to_canonical(AirTempF, 22.0, Some("°C")), 71.6);
        approx(to_canonical(AirTempF, 22.0, Some("C")), 71.6);
        approx(to_canonical(AirTempF, 22.0, Some("celsius")), 71.6);
        approx(to_canonical(DewPointF, 0.0, Some("°C")), 32.0);
        approx(to_canonical(AirTempF, 295.15, Some("K")), 71.6);
    }

    #[test]
    fn fahrenheit_and_unknown_pass_through() {
        approx(to_canonical(AirTempF, 71.6, Some("°F")), 71.6);
        approx(to_canonical(AirTempF, 71.6, None), 71.6);
        approx(to_canonical(AirTempF, 71.6, Some("")), 71.6);
        approx(to_canonical(AirTempF, 71.6, Some("weird")), 71.6);
    }

    #[test]
    fn wind_pressure_rain_distance() {
        approx(to_canonical(WindMph, 10.0, Some("km/h")), 6.21);
        approx(to_canonical(WindGustMph, 5.0, Some("m/s")), 11.18);
        approx(to_canonical(WindMph, 10.0, Some("kn")), 11.51);
        approx(to_canonical(PressureInHg, 1013.0, Some("hPa")), 29.91);
        approx(to_canonical(PressureInHg, 101.3, Some("kPa")), 29.91);
        approx(to_canonical(RainTodayIn, 25.4, Some("mm")), 1.0);
        approx(to_canonical(RainIntensityInHr, 25.4, Some("mm/h")), 1.0);
        approx(to_canonical(LightningDistanceMi, 10.0, Some("km")), 6.21);
    }

    #[test]
    fn unitless_passes_through() {
        approx(to_canonical(RhPct, 55.0, Some("%")), 55.0);
        approx(to_canonical(UvIndex, 7.0, Some("")), 7.0);
        approx(to_canonical(SolarWm2, 800.0, Some("W/m²")), 800.0);
    }
}
