// Per-zone 7-day soil-moisture projection. FAO-56-flavored water
// balance: today's calibrated reading is the starting point; each day
// subtracts daily ET (scaled by zone Kc) and adds the probability-
// weighted forecast rain (scaled by capture efficiency). Irrigation is
// not modeled -- the curve answers "if I did nothing all week, would
// each zone stay in its healthy band?"
//
// Phase 3E extraction from src/refresher.rs::compute_soil_forecasts.
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
    //
    // KNOWN LIMITATION (#4b, display-only; do NOT "fix" the math here):
    // `start_pct` is a probe reading on a RELATIVE scale (the sensor's own
    // 0-100% calibration), but the daily deltas are derived from absolute mm of
    // water (rain mm captured minus ET mm lost) converted to a percent of
    // `soil_depth_mm` of VWC. Adding an absolute-VWC delta onto a relative-scale
    // baseline is a unit mismatch: only correct if the probe's % happens to be
    // true volumetric water content, which most consumer probes are not. The
    // longer the horizon, the more the curve can drift from reality. This is
    // acceptable because the projection is advisory ("if I did nothing all week")
    // and never gates a real watering decision (the live skip ladder reads the
    // probe directly). A future pass should anchor both to the same scale (e.g.
    // derive deltas in the probe's relative units, or calibrate the probe to
    // VWC); until then, treat the 7-day curve as a trend, not a measurement.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::snapshot::DailyEntry;

    fn zone() -> ZoneSoilInputs {
        ZoneSoilInputs {
            slug: "back_yard".into(),
            name: "Back yard".into(),
            kc: 0.8,
            soil_depth_mm: 150.0,
            current_pct: Some(60.0),
            target_min_pct: 30.0,
            target_max_pct: 70.0,
        }
    }

    /// Rain-free 7-day daily window (only the day count matters here).
    fn dry_week() -> ForecastSnapshot {
        ForecastSnapshot {
            daily: (0..7i64)
                .map(|i| DailyEntry {
                    time_epoch: 1_750_000_000 + i * 86_400,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn realistic_summer_et_declines_the_curve_materially() {
        // Magnitude pin against the mm/inches ET regression (issue #4): a
        // realistic summer ET0 of 4.6 mm/day loses 4.6 * 0.8 / 150 mm of the
        // root zone per dry day, ~2.45%/day, ~14.7% over the 6 projected steps.
        let out = project_zone(&zone(), &dry_week(), 4.6, 0.7, 7);
        assert_eq!(out.predicted_pct.len(), 7);
        let start = out.predicted_pct[0];
        let end = *out.predicted_pct.last().unwrap();
        assert!((start - 60.0).abs() < 1e-9, "day 0 is the current reading");
        let decline = start - end;
        assert!((13.0..17.0).contains(&decline), "7-day decline = {decline}");
        // The same week at the 25x-too-small regression scale (4.6 / 25.4
        // "mm"/day, i.e. inches mislabeled as mm) barely moves the curve; a
        // future unit slip fails the materiality assertion above, and this one
        // documents what the broken projection looked like.
        let tiny = project_zone(&zone(), &dry_week(), 4.6 / 25.4, 0.7, 7);
        let tiny_decline = tiny.predicted_pct[0] - tiny.predicted_pct.last().unwrap();
        assert!(
            tiny_decline < 1.0,
            "regression-scale decline = {tiny_decline}"
        );
    }
}
