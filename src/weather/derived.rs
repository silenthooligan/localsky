// The readings that are computed from other readings.
//
// Dew point, wet bulb and the feels-like temperature are not measured by
// anything; they are functions of the temperature, the humidity and the
// wind, whichever sources happen to own those. Keeping them here means
// the store derives them once from the merged values rather than each
// adapter deriving its own from its own, which is how a dashboard ends up
// showing a dew point that disagrees with its temperature.

/// Magnus-Tetens dew point (°C) from temperature (°C) and RH (%).
pub fn dew_point_c(t_c: f64, rh: f64) -> f64 {
    if !t_c.is_finite()
        || !rh.is_finite()
        || !(0.0..=100.0).contains(&rh)
        || rh == 0.0
        || t_c <= -243.04
    {
        return f64::NAN;
    }
    let a = 17.625;
    let b = 243.04;
    let alpha = (rh / 100.0).ln() + a * t_c / (b + t_c);
    b * alpha / (a - alpha)
}

/// Stull (2011), JAMC 50:2267-2269, doi:10.1175/JAMC-D-11-0143.1.
/// Sea-level empirical approximation: -20..50°C, RH 5..99%, excluding
/// simultaneous cold/dry conditions. Unknown outside its usable domain.
pub fn wet_bulb_c(t_c: f64, rh: f64) -> f64 {
    if !t_c.is_finite()
        || !rh.is_finite()
        || !(-20.0..=50.0).contains(&t_c)
        || !(5.0..=100.0).contains(&rh)
        || (t_c < 0.0 && rh < 20.0)
    {
        return f64::NAN;
    }
    if rh == 100.0 {
        return t_c;
    }
    let estimate = t_c * (0.151_977 * (rh + 8.313_659).sqrt()).atan() + (t_c + rh).atan()
        - (rh - 1.676_331).atan()
        + 0.003_918_38 * rh.powf(1.5) * (0.023_101 * rh).atan()
        - 4.686_035;
    estimate.min(t_c)
}

/// NWS Rothfusz heat index with the simple-formula eligibility check and
/// dry/humid adjustments; wind chill at <=50°F and wind >3mph.
/// https://www.wpc.ncep.noaa.gov/html/heatindex_equation.shtml
/// Outside those regimes this display uses air temperature.
pub fn feels_like_f(t_f: f64, rh: f64, wind_mph: f64) -> f64 {
    if !t_f.is_finite() {
        return f64::NAN;
    }
    if t_f <= 50.0 && wind_mph.is_finite() && wind_mph > 3.0 {
        return 35.74 + 0.6215 * t_f - 35.75 * wind_mph.powf(0.16)
            + 0.4275 * t_f * wind_mph.powf(0.16);
    }
    if t_f >= 80.0 && (!rh.is_finite() || !(0.0..=100.0).contains(&rh)) {
        return f64::NAN;
    }
    let simple = 0.5 * (t_f + 61.0 + (t_f - 68.0) * 1.2 + rh * 0.094);
    if rh.is_finite() && (0.0..=100.0).contains(&rh) && (simple + t_f) / 2.0 >= 80.0 {
        let mut hi = -42.379 + 2.049_015_23 * t_f + 10.143_331_27 * rh
            - 0.224_755_41 * t_f * rh
            - 0.006_837_83 * t_f * t_f
            - 0.054_817_17 * rh * rh
            + 0.001_228_74 * t_f * t_f * rh
            + 0.000_852_82 * t_f * rh * rh
            - 0.000_001_99 * t_f * t_f * rh * rh;
        if rh < 13.0 && (80.0..=112.0).contains(&t_f) {
            hi -= (13.0 - rh) / 4.0 * ((17.0 - (t_f - 95.0).abs()) / 17.0).sqrt();
        } else if rh > 85.0 && (80.0..=87.0).contains(&t_f) {
            hi += (rh - 85.0) / 10.0 * (87.0 - t_f) / 5.0;
        }
        hi
    } else {
        t_f
    }
}

/// The legacy snapshot stores numeric fields as f64. JSON null is unknown,
/// not zero, and must not make the entire browser stream fail to deserialize.
pub fn deserialize_optional_reading<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<f64, D::Error> {
    use serde::Deserialize;
    Ok(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thermal_indices_cover_hot_dry_humid_and_cold_climates() {
        // NWS equation, including low/high humidity corrections.
        assert!((feels_like_f(100.0, 10.0, 0.0) - 94.12).abs() < 0.1);
        assert!((feels_like_f(86.0, 90.0, 0.0) - 105.39).abs() < 0.1);
        // NWS published wind-chill example: 0°F, 15mph -> -19°F.
        assert!((feels_like_f(0.0, 50.0, 15.0) + 19.0).abs() < 0.5);
        assert_eq!(feels_like_f(50.0, 50.0, 3.0), 50.0);
        assert!((wet_bulb_c(30.0, 50.0) - 22.3).abs() < 0.1);
        assert_eq!(wet_bulb_c(30.0, 100.0), 30.0);
        for (temp, rh) in [
            (-30.0, 50.0),
            (55.0, 30.0),
            (25.0, 1.0),
            (-10.0, 10.0),
            (20.0, 101.0),
        ] {
            assert!(wet_bulb_c(temp, rh).is_nan());
        }
        assert!(dew_point_c(25.0, 0.0).is_nan());
        assert!((dew_point_c(25.0, 100.0) - 25.0).abs() < 1e-10);
    }

    #[test]
    fn unavailable_derived_readings_round_trip_without_breaking_the_stream() {
        let snap = crate::weather::Snapshot {
            wet_bulb_f: f64::NAN,
            feels_like_f: f64::NAN,
            ..Default::default()
        };
        let json = serde_json::to_value(snap).unwrap();
        assert!(json["wet_bulb_f"].is_null());
        let decoded: crate::weather::Snapshot = serde_json::from_value(json).unwrap();
        assert!(decoded.wet_bulb_f.is_nan() && decoded.feels_like_f.is_nan());
    }
}
