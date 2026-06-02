// Per-zone 7-day soil-moisture projection. FAO-56-flavored water
// balance: today's calibrated reading is the starting point; each day
// subtracts daily ET (scaled by zone Kc) and adds the probability-
// weighted forecast rain (scaled by capture efficiency). Irrigation is
// not modeled -- the curve answers "if I did nothing all week, would
// each zone stay in its healthy band?"
//
// Phase 3E extraction from src/ha/refresher.rs::compute_soil_forecasts.
// Pure function per zone; HA-entity reading + zone enumeration stay in
// refresher.rs (v0.1) or move to a config-driven enumeration (v2+).

use crate::forecast::snapshot::ForecastSnapshot;
use crate::ha::snapshot::SoilForecast;

#[derive(Debug, Clone)]
pub struct ZoneSoilInputs {
    pub slug: String,
    pub name: String,
    /// Crop coefficient (FAO-56 Kc) applied to ET0 for this zone.
    /// Looked up from species_catalog or overridden by operator.
    pub kc: f64,
    /// Effective root-zone depth (mm). Looked up from species_catalog
    /// or overridden by operator.
    pub soil_depth_mm: f64,
    /// Live sensor reading (%). None = probe offline / unconfigured.
    pub current_pct: Option<f64>,
    pub target_min_pct: f64,
    pub target_max_pct: f64,
}

/// Project the next `n_days` of moisture % under no-irrigation. Returns
/// a SoilForecast with the day-by-day curve, min/max, threshold
/// crossings, and a coarse status label.
pub fn project_zone(
    zone: &ZoneSoilInputs,
    fc: &ForecastSnapshot,
    daily_et_mm: f64,
    capture_efficiency: f64,
    n_days: usize,
) -> SoilForecast {
    let n_days = n_days.clamp(1, 14);

    let Some(start_pct) = zone.current_pct else {
        return SoilForecast {
            zone_slug: zone.slug.clone(),
            zone_name: zone.name.clone(),
            current_pct: None,
            target_min_pct: zone.target_min_pct,
            target_max_pct: zone.target_max_pct,
            predicted_pct: vec![0.0; n_days],
            min_predicted_pct: 0.0,
            max_predicted_pct: 0.0,
            days_below_target: 0,
            days_above_max: 0,
            status: "no_data".to_string(),
        };
    };

    let mut series = Vec::with_capacity(n_days);
    let mut moisture = start_pct;
    series.push(moisture);

    // Day 0 = today (current reading); deltas start at day 1 using
    // daily[N]'s rain projection.
    for d in fc.daily.iter().take(n_days).skip(1) {
        let rain_effective_mm = d.precip_sum_in * 25.4 * (d.precip_probability_max as f64) / 100.0;
        let captured_mm = rain_effective_mm * capture_efficiency;
        let et_loss_mm = daily_et_mm * zone.kc;
        let delta_mm = captured_mm - et_loss_mm;
        let delta_pct = delta_mm / zone.soil_depth_mm * 100.0;
        moisture = (moisture + delta_pct).clamp(0.0, 100.0);
        series.push(moisture);
    }

    let min_predicted = series
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min)
        .max(0.0);
    let max_predicted = series
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max)
        .min(100.0);
    let days_below = series.iter().filter(|p| **p <= zone.target_min_pct).count() as u32;
    let days_above = series.iter().filter(|p| **p >= zone.target_max_pct).count() as u32;

    // Status: "wet" wins over "dry" so a saturated start isn't flagged
    // dry from a forecast dry stretch that hasn't arrived yet.
    let status = if max_predicted >= zone.target_max_pct {
        "wet"
    } else if min_predicted <= zone.target_min_pct || days_below >= 2 {
        "dry"
    } else {
        "ok"
    };

    SoilForecast {
        zone_slug: zone.slug.clone(),
        zone_name: zone.name.clone(),
        current_pct: Some(start_pct),
        target_min_pct: zone.target_min_pct,
        target_max_pct: zone.target_max_pct,
        predicted_pct: series,
        min_predicted_pct: min_predicted,
        max_predicted_pct: max_predicted,
        days_below_target: days_below,
        days_above_max: days_above,
        status: status.to_string(),
    }
}
