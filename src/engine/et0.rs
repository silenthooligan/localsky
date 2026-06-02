// Reference evapotranspiration (ET0) computation. Pure functions.
//
// Three methods:
//   1. FAO-56 Penman-Monteith reference grass ETo (Allen et al., 1998).
//      The gold standard. Requires Tmax, Tmin, RH, wind at 2m, and solar
//      radiation. Air pressure and elevation are also used.
//   2. ASCE-EWRI 2005 standardized reference ET (short-crop). Practically
//      identical to FAO-56 for daily computation; we implement them as
//      the same path.
//   3. Hargreaves-Samani 1985: temp-range fallback when RH, wind, or
//      solar data is missing. Less accurate (typical bias ~15-20%) but
//      universally applicable.
//
// Citations:
//   * FAO Irrigation and Drainage Paper No. 56 (Allen et al., 1998).
//     Chapters 3 (Meteorological Data), 4 (Reference ETo).
//   * ASCE-EWRI (2005). The ASCE Standardized Reference
//     Evapotranspiration Equation.
//   * Hargreaves G.H., Samani Z.A. (1985). Reference crop
//     evapotranspiration from temperature. Applied Eng. in Agric. 1(2): 96-99.
//
// Engine internals work in metric: degrees Celsius, kilopascals, m/s,
// MJ/m²/day, mm/day. Adapters convert at the boundary.

use crate::config::schema::Et0Method;

/// Inputs for a single-day ET0 computation. All temperatures in °C, RH
/// in %, wind in m/s at 2m height, radiation in MJ/m²/day, pressure in
/// kPa, latitude in degrees, day-of-year 1..=366.
#[derive(Debug, Clone)]
pub struct Et0Inputs {
    pub t_max_c: f64,
    pub t_min_c: f64,
    /// Optional mean temperature. Falls back to (max+min)/2 when absent.
    pub t_mean_c: Option<f64>,
    pub rh_max_pct: Option<f64>,
    pub rh_min_pct: Option<f64>,
    pub rh_mean_pct: Option<f64>,
    /// Wind at 2m above ground (m/s).
    pub u2_ms: Option<f64>,
    /// Daily incoming shortwave solar radiation (MJ/m²/day). Tempest
    /// reports W/m² instantaneous; integration to MJ/m²/day is the
    /// adapter's responsibility.
    pub solar_rad_mj_m2_day: Option<f64>,
    /// Atmospheric pressure (kPa). Falls back to elevation-derived value.
    pub pressure_kpa: Option<f64>,
    /// Elevation above mean sea level (metres).
    pub elevation_m: f64,
    /// Latitude in decimal degrees (positive north, negative south).
    pub latitude_deg: f64,
    /// Day-of-year 1..=366.
    pub doy: u16,
}

#[derive(Debug, Clone)]
pub struct Et0Result {
    pub et0_mm_day: f64,
    pub method_used: Et0Method,
    pub diagnostics: Et0Diagnostics,
}

#[derive(Debug, Clone, Default)]
pub struct Et0Diagnostics {
    pub net_radiation_mj_m2_day: Option<f64>,
    pub clear_sky_radiation_mj_m2_day: Option<f64>,
    pub vapor_pressure_deficit_kpa: Option<f64>,
    pub slope_vapor_kpa_c: Option<f64>,
    pub psychrometric_kpa_c: Option<f64>,
    pub extraterrestrial_rad_mj_m2_day: f64,
    pub pressure_kpa: f64,
}

/// Top-level dispatch. With `Et0Method::Auto`, picks Penman-Monteith if
/// all PM inputs are present, else Hargreaves-Samani.
pub fn compute(inputs: &Et0Inputs, method: Et0Method) -> Et0Result {
    use Et0Method::*;
    match method {
        PenmanMonteith | AsceSimplified => penman_monteith(inputs),
        HargreavesSamani => hargreaves_samani(inputs),
        SourceNative => Et0Result {
            et0_mm_day: f64::NAN,
            method_used: SourceNative,
            diagnostics: Et0Diagnostics {
                extraterrestrial_rad_mj_m2_day: extraterrestrial_radiation(
                    inputs.latitude_deg,
                    inputs.doy,
                ),
                ..Default::default()
            },
        },
        Auto => {
            if has_full_pm_inputs(inputs) {
                penman_monteith(inputs)
            } else {
                hargreaves_samani(inputs)
            }
        }
    }
}

fn has_full_pm_inputs(inputs: &Et0Inputs) -> bool {
    inputs.u2_ms.is_some()
        && inputs.solar_rad_mj_m2_day.is_some()
        && (inputs.rh_mean_pct.is_some()
            || (inputs.rh_max_pct.is_some() && inputs.rh_min_pct.is_some()))
}

// ----- FAO-56 Penman-Monteith reference grass ETo -----
//
// FAO-56 Equation 6:
//
//   ETo = ( 0.408 * Δ * (Rn - G)
//         + γ * (900 / (T + 273)) * u2 * (es - ea) )
//       / ( Δ + γ * (1 + 0.34 * u2) )
//
// G ≈ 0 for daily computation over grass.

pub fn penman_monteith(inputs: &Et0Inputs) -> Et0Result {
    let t_mean = inputs
        .t_mean_c
        .unwrap_or((inputs.t_max_c + inputs.t_min_c) / 2.0);
    let elev = inputs.elevation_m;
    let p = inputs
        .pressure_kpa
        .unwrap_or_else(|| pressure_from_elevation(elev));
    let gamma = psychrometric_constant(p);

    let es_tmax = saturation_vapor_pressure(inputs.t_max_c);
    let es_tmin = saturation_vapor_pressure(inputs.t_min_c);
    let es = (es_tmax + es_tmin) / 2.0;
    let ea = actual_vapor_pressure(inputs, es_tmax, es_tmin);

    let delta = slope_vapor_pressure_curve(t_mean);

    let ra = extraterrestrial_radiation(inputs.latitude_deg, inputs.doy);
    let rs = inputs
        .solar_rad_mj_m2_day
        .unwrap_or_else(|| 0.16 * (inputs.t_max_c - inputs.t_min_c).max(0.0).sqrt() * ra);
    let rso = clear_sky_radiation(ra, elev);
    let rns = (1.0 - 0.23) * rs; // albedo 0.23 for short grass (FAO-56)
    let rnl = net_longwave_radiation(inputs.t_max_c, inputs.t_min_c, ea, rs, rso);
    let rn = rns - rnl;

    let u2 = inputs.u2_ms.unwrap_or(2.0); // 2 m/s is the FAO-56 "missing wind" default.

    let numerator = 0.408 * delta * rn + gamma * (900.0 / (t_mean + 273.0)) * u2 * (es - ea);
    let denominator = delta + gamma * (1.0 + 0.34 * u2);
    let et0 = if denominator > 0.0 {
        numerator / denominator
    } else {
        0.0
    };
    let et0 = et0.max(0.0);

    Et0Result {
        et0_mm_day: et0,
        method_used: Et0Method::PenmanMonteith,
        diagnostics: Et0Diagnostics {
            net_radiation_mj_m2_day: Some(rn),
            clear_sky_radiation_mj_m2_day: Some(rso),
            vapor_pressure_deficit_kpa: Some(es - ea),
            slope_vapor_kpa_c: Some(delta),
            psychrometric_kpa_c: Some(gamma),
            extraterrestrial_rad_mj_m2_day: ra,
            pressure_kpa: p,
        },
    }
}

// ----- Hargreaves-Samani 1985 -----

pub fn hargreaves_samani(inputs: &Et0Inputs) -> Et0Result {
    let t_mean = inputs
        .t_mean_c
        .unwrap_or((inputs.t_max_c + inputs.t_min_c) / 2.0);
    let ra_mj = extraterrestrial_radiation(inputs.latitude_deg, inputs.doy);
    // Convert Ra from MJ/m²/day to "equivalent depth of water" mm/day by
    // dividing by the latent heat of vaporization (2.45 MJ/kg) which is
    // equivalent to multiplying by 0.408.
    let ra_mm = ra_mj * 0.408;
    let dt = (inputs.t_max_c - inputs.t_min_c).max(0.0);
    let et0 = 0.0023 * ra_mm * (t_mean + 17.8) * dt.sqrt();
    Et0Result {
        et0_mm_day: et0.max(0.0),
        method_used: Et0Method::HargreavesSamani,
        diagnostics: Et0Diagnostics {
            extraterrestrial_rad_mj_m2_day: ra_mj,
            pressure_kpa: inputs
                .pressure_kpa
                .unwrap_or_else(|| pressure_from_elevation(inputs.elevation_m)),
            ..Default::default()
        },
    }
}

// ----- Sub-functions -----

/// Saturation vapor pressure (kPa) at temperature T (°C). FAO-56 eq. 11.
fn saturation_vapor_pressure(t_c: f64) -> f64 {
    0.6108 * ((17.27 * t_c) / (t_c + 237.3)).exp()
}

/// Slope of the saturation vapor pressure curve (kPa/°C) at T. FAO-56 eq. 13.
fn slope_vapor_pressure_curve(t_c: f64) -> f64 {
    let num = 4098.0 * (0.6108 * ((17.27 * t_c) / (t_c + 237.3)).exp());
    let den = (t_c + 237.3).powi(2);
    num / den
}

/// Atmospheric pressure (kPa) from elevation (m). FAO-56 eq. 7.
fn pressure_from_elevation(z_m: f64) -> f64 {
    101.3 * ((293.0 - 0.0065 * z_m) / 293.0).powf(5.26)
}

/// Psychrometric constant γ (kPa/°C). FAO-56 eq. 8 with cp = 1.013e-3
/// MJ/kg/°C, ε = 0.622, λ = 2.45 MJ/kg.
fn psychrometric_constant(p_kpa: f64) -> f64 {
    0.665e-3 * p_kpa
}

/// Actual vapor pressure (kPa). Uses the best RH data available, in
/// order of FAO-56 preference: RHmax+RHmin, then RHmean, then dewpoint=Tmin.
fn actual_vapor_pressure(inputs: &Et0Inputs, es_tmax: f64, es_tmin: f64) -> f64 {
    if let (Some(rh_max), Some(rh_min)) = (inputs.rh_max_pct, inputs.rh_min_pct) {
        // FAO-56 eq. 17.
        (es_tmin * rh_max / 100.0 + es_tmax * rh_min / 100.0) / 2.0
    } else if let Some(rh_mean) = inputs.rh_mean_pct {
        let es_mean = (es_tmax + es_tmin) / 2.0;
        es_mean * rh_mean / 100.0
    } else {
        // Fallback: assume dewpoint = Tmin (FAO-56 eq. 48).
        es_tmin
    }
}

/// Extraterrestrial radiation Ra (MJ/m²/day). FAO-56 eq. 21.
pub fn extraterrestrial_radiation(lat_deg: f64, doy: u16) -> f64 {
    let phi = lat_deg.to_radians();
    let j = doy as f64;
    let dr = 1.0 + 0.033 * (2.0 * std::f64::consts::PI * j / 365.0).cos();
    let delta = 0.409 * (2.0 * std::f64::consts::PI * j / 365.0 - 1.39).sin();
    // Sunset hour angle. tan(φ) * tan(δ) can exceed 1 at high latitudes
    // around the solstices (polar day/night); clamp the cosine argument.
    let cos_arg = (-phi.tan() * delta.tan()).clamp(-1.0, 1.0);
    let omega_s = cos_arg.acos();
    // 24*60/π * 0.0820 ≈ 37.586 — Gsc * conversion in FAO-56 units.
    (24.0 * 60.0 / std::f64::consts::PI)
        * 0.0820
        * dr
        * (omega_s * phi.sin() * delta.sin() + phi.cos() * delta.cos() * omega_s.sin())
}

/// Clear-sky solar radiation Rso (MJ/m²/day). FAO-56 eq. 37.
fn clear_sky_radiation(ra: f64, elev_m: f64) -> f64 {
    (0.75 + 2.0e-5 * elev_m) * ra
}

/// Net outgoing longwave radiation (MJ/m²/day). FAO-56 eq. 39.
fn net_longwave_radiation(t_max_c: f64, t_min_c: f64, ea_kpa: f64, rs: f64, rso: f64) -> f64 {
    // Stefan-Boltzmann in FAO-56 units.
    const SIGMA: f64 = 4.903e-9;
    let t_max_k4 = (t_max_c + 273.16).powi(4);
    let t_min_k4 = (t_min_c + 273.16).powi(4);
    let temp_term = (t_max_k4 + t_min_k4) / 2.0;
    let vapor_term = 0.34 - 0.14 * ea_kpa.sqrt();
    let cloud_term = if rso > 0.0 {
        (1.35 * (rs / rso).clamp(0.3, 1.0)) - 0.35
    } else {
        // No clear-sky reference; treat as fully clear.
        1.0
    };
    SIGMA * temp_term * vapor_term * cloud_term
}

// ----- Unit conversion helpers (adapter use) -----

/// Convert °F to °C.
pub fn f_to_c(t_f: f64) -> f64 {
    (t_f - 32.0) * 5.0 / 9.0
}

/// Convert mph to m/s.
pub fn mph_to_ms(mph: f64) -> f64 {
    mph * 0.44704
}

/// Convert W/m² to MJ/m²/day for a *daily mean* solar irradiance.
/// (W/m² * 86400 s/day) / 1e6 = MJ/m²/day; equivalent to *0.0864.
pub fn wm2_mean_to_mj_day(w_m2_mean: f64) -> f64 {
    w_m2_mean * 0.0864
}

/// Convert inHg to kPa.
pub fn inhg_to_kpa(inhg: f64) -> f64 {
    inhg * 3.38639
}

/// Adjust wind from u_z at height z to u_2 at 2m. FAO-56 eq. 47.
pub fn wind_to_2m(u_z_ms: f64, height_m: f64) -> f64 {
    if (height_m - 2.0).abs() < 0.01 {
        return u_z_ms;
    }
    u_z_ms * 4.87 / (67.8 * height_m - 5.42).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    /// Penman-Monteith hand-trace at 50°N in April. Inputs:
    ///   Tmax=21.5, Tmin=12.3, RHmax=84, RHmin=63, u2=2.78 m/s,
    ///   Rs=22.07 MJ/m²/day, elev=100m, DOY=96 (April 6).
    /// Hand-calculation through FAO-56 eq. 6 (Allen et al., 1998) yields
    /// ~3.51 mm/day. This test pins our implementation against that
    /// trace; drift here means a regression in one of the sub-equations.
    #[test]
    fn pm_temperate_april_trace() {
        let inputs = Et0Inputs {
            t_max_c: 21.5,
            t_min_c: 12.3,
            t_mean_c: Some(16.9),
            rh_max_pct: Some(84.0),
            rh_min_pct: Some(63.0),
            rh_mean_pct: None,
            u2_ms: Some(2.78),
            solar_rad_mj_m2_day: Some(22.07),
            pressure_kpa: None,
            elevation_m: 100.0,
            latitude_deg: 50.80,
            doy: 96,
        };
        let r = penman_monteith(&inputs);
        assert!(
            approx(r.et0_mm_day, 3.51, 0.05),
            "expected ~3.51 mm/day, got {:.3}",
            r.et0_mm_day
        );
    }

    /// Penman-Monteith hot-tropical case. Bangkok-like inputs at 13.7°N
    /// in mid-July yield a much higher ETo (mid-summer at low latitude
    /// with high solar load). Sanity test that ET0 scales correctly with
    /// climate regime.
    #[test]
    fn pm_tropical_july_higher_than_temperate() {
        let temperate = Et0Inputs {
            t_max_c: 21.5,
            t_min_c: 12.3,
            t_mean_c: Some(16.9),
            rh_max_pct: Some(84.0),
            rh_min_pct: Some(63.0),
            rh_mean_pct: None,
            u2_ms: Some(2.78),
            solar_rad_mj_m2_day: Some(22.07),
            pressure_kpa: None,
            elevation_m: 100.0,
            latitude_deg: 50.80,
            doy: 96,
        };
        let tropical = Et0Inputs {
            t_max_c: 34.8,
            t_min_c: 25.6,
            t_mean_c: Some(30.2),
            rh_max_pct: None,
            rh_min_pct: None,
            rh_mean_pct: Some(75.0),
            u2_ms: Some(2.0),
            solar_rad_mj_m2_day: Some(22.65),
            pressure_kpa: None,
            elevation_m: 2.0,
            latitude_deg: 13.73,
            doy: 187,
        };
        let t = penman_monteith(&temperate).et0_mm_day;
        let r = penman_monteith(&tropical).et0_mm_day;
        assert!(
            r > t * 1.3,
            "tropical {r:.2} should exceed temperate {t:.2} by >30%"
        );
        // Sanity ceiling: ETo over short grass rarely exceeds 9 mm/day even in extreme heat.
        assert!(r < 9.0, "tropical case unexpectedly high: {r:.2}");
        // Sanity floor: should be well above 4 mm/day.
        assert!(r > 4.0, "tropical case unexpectedly low: {r:.2}");
    }

    #[test]
    fn hargreaves_within_band_of_penman_monteith() {
        let inputs = Et0Inputs {
            t_max_c: 25.0,
            t_min_c: 15.0,
            t_mean_c: None,
            rh_max_pct: Some(80.0),
            rh_min_pct: Some(50.0),
            rh_mean_pct: None,
            u2_ms: Some(2.0),
            solar_rad_mj_m2_day: Some(20.0),
            pressure_kpa: None,
            elevation_m: 30.0,
            latitude_deg: 28.5,
            doy: 196, // mid-July
        };
        let pm = penman_monteith(&inputs).et0_mm_day;
        let hs = hargreaves_samani(&inputs).et0_mm_day;
        // Hargreaves typically within ±20-25% of PM.
        let ratio = hs / pm;
        assert!(
            (0.7..=1.30).contains(&ratio),
            "HS/PM ratio {ratio} outside expected band (PM={pm:.2}, HS={hs:.2})"
        );
    }

    #[test]
    fn auto_picks_hargreaves_when_pm_inputs_missing() {
        let inputs = Et0Inputs {
            t_max_c: 30.0,
            t_min_c: 20.0,
            t_mean_c: None,
            rh_max_pct: None,
            rh_min_pct: None,
            rh_mean_pct: None,
            u2_ms: None,
            solar_rad_mj_m2_day: None,
            pressure_kpa: None,
            elevation_m: 30.0,
            latitude_deg: 28.5,
            doy: 196,
        };
        let r = compute(&inputs, Et0Method::Auto);
        assert!(matches!(r.method_used, Et0Method::HargreavesSamani));
        assert!(r.et0_mm_day > 0.0);
    }

    #[test]
    fn extraterrestrial_radiation_is_positive_and_seasonal() {
        // Florida lat, summer solstice vs winter solstice.
        let summer = extraterrestrial_radiation(28.5, 172); // ~Jun 21
        let winter = extraterrestrial_radiation(28.5, 355); // ~Dec 21
        assert!(summer > winter);
        assert!(summer > 30.0); // FAO-56 Table 2.6: ~40 MJ/m²/day at 30°N midsummer
        assert!(winter > 15.0);
    }

    #[test]
    fn polar_day_no_panic() {
        // High latitude near solstice can trip tan(φ)*tan(δ) outside
        // [-1,1]. The acos clamp should keep us sane.
        let summer = extraterrestrial_radiation(80.0, 172);
        let winter = extraterrestrial_radiation(80.0, 355);
        assert!(summer.is_finite());
        assert!(winter.is_finite());
        assert!(summer > 0.0);
        assert!(winter >= 0.0);
    }

    #[test]
    fn unit_helpers() {
        assert!((f_to_c(212.0) - 100.0).abs() < 1e-9);
        assert!((f_to_c(32.0) - 0.0).abs() < 1e-9);
        assert!((mph_to_ms(100.0) - 44.704).abs() < 1e-3);
        assert!((wm2_mean_to_mj_day(250.0) - 21.6).abs() < 1e-3);
        // 2m wind passthrough
        let u2 = wind_to_2m(3.5, 2.0);
        assert!((u2 - 3.5).abs() < 1e-9);
        // 10m wind reduction
        let u10 = 5.0;
        let u2 = wind_to_2m(u10, 10.0);
        assert!(u2 > 0.0 && u2 < u10);
    }
}
