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
    let a = 17.625;
    let b = 243.04;
    let alpha = (rh.max(1.0) / 100.0).ln() + a * t_c / (b + t_c);
    b * alpha / (a - alpha)
}

/// Stull (2011) wet-bulb approximation, valid for normal RH/temp ranges.
pub fn wet_bulb_c(t_c: f64, rh: f64) -> f64 {
    let rh = rh.max(1.0);
    t_c * (0.151_977 * (rh + 8.313_659).sqrt()).atan() + (t_c + rh).atan() - (rh - 1.676_331).atan()
        + 0.003_918_38 * rh.powf(1.5) * (0.023_101 * rh).atan()
        - 4.686_035
}

/// NWS heat-index formula above 80 °F / 40% RH; NWS wind-chill below 50 °F
/// with wind ≥ 3 mph; otherwise just the air temperature.
pub fn feels_like_f(t_f: f64, rh: f64, wind_mph: f64) -> f64 {
    if t_f >= 80.0 && rh >= 40.0 {
        -42.379 + 2.049_015_23 * t_f + 10.143_331_27 * rh
            - 0.224_755_41 * t_f * rh
            - 0.006_837_83 * t_f * t_f
            - 0.054_817_17 * rh * rh
            + 0.001_228_74 * t_f * t_f * rh
            + 0.000_852_82 * t_f * rh * rh
            - 0.000_001_99 * t_f * t_f * rh * rh
    } else if t_f <= 50.0 && wind_mph >= 3.0 {
        35.74 + 0.6215 * t_f - 35.75 * wind_mph.powf(0.16) + 0.4275 * t_f * wind_mph.powf(0.16)
    } else {
        t_f
    }
}
