// Forecast bias correction.
//
// Open-Meteo (or any other regional forecast source) has systematic
// bias in any given microclimate. Some yards see consistent
// overprediction in summer afternoons; others see consistent
// underprediction in winter advection events. A multiplicative
// correction factor applied before the skip rules evaluate can fold
// that bias out of the daily verdict without the operator hand-tuning
// thresholds.
//
// The model is intentionally simple and auditable:
//
//   1. Daily: record (predicted_in, observed_in) for the day's rain.
//      predicted is what the forecast said at start of day, observed is
//      the end-of-day total from the station (or fallback source).
//   2. Per calendar month, compute the median of observed/predicted
//      ratios across the last `window_days` observations.
//   3. Clamp the result to [BIAS_FLOOR, BIAS_CEIL] so single-event
//      noise (or a broken sensor) can't tank the engine.
//   4. Require at least `MIN_OBSERVATIONS` data points before any
//      correction is applied. Until then, return 1.0 (no correction).
//
// Days where both predicted and observed are below `NOISE_FLOOR_IN`
// are excluded as not informative ("forecast 0.00, observed 0.01" is
// not a signal). Days with predicted == 0 are also excluded since the
// multiplicative model can't speak to dry-forecast accuracy.

use chrono::{Datelike, NaiveDate};

/// Minimum count of qualifying observations in a month-of-year bucket
/// before the multiplier is allowed to deviate from 1.0.
pub const MIN_OBSERVATIONS: usize = 5;

/// Multiplier floor + ceiling. Real forecast bias is rarely outside
/// these bounds; values outside usually mean a broken pipeline.
pub const BIAS_FLOOR: f64 = 0.5;
pub const BIAS_CEIL: f64 = 1.5;

/// Days within `window_days` of "today" count toward the multiplier.
/// 90 days = one season; tracks seasonal microclimate shifts without
/// dragging in last summer when "this summer" has its own signature.
pub const DEFAULT_WINDOW_DAYS: i64 = 90;

/// Rain amounts below this in either column are treated as "no rain"
/// and the day is dropped from the bias calc.
pub const NOISE_FLOOR_IN: f64 = 0.02;

/// One day's predicted-vs-observed rain in inches. `date` is the local
/// calendar date the forecast covered.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Observation {
    pub date: NaiveDate,
    pub predicted_in: f64,
    pub observed_in: f64,
}

impl Observation {
    pub fn new(date: NaiveDate, predicted_in: f64, observed_in: f64) -> Self {
        Self {
            date,
            predicted_in,
            observed_in,
        }
    }
}

/// Per-month-of-year multiplier. Apply as
/// `corrected_rain = raw_rain * multiplier_for(month)` before passing
/// the forecast into the skip rules.
#[derive(Debug, Clone, Default)]
pub struct BiasModel {
    /// Index 1..=12 (month-of-year). Index 0 unused.
    multipliers: [f64; 13],
    /// Index 1..=12 of how many observations went into each month's
    /// multiplier. <MIN_OBSERVATIONS => multiplier == 1.0 by design.
    sample_counts: [usize; 13],
}

impl BiasModel {
    /// Identity model: every month returns 1.0 (no correction).
    pub fn identity() -> Self {
        Self {
            multipliers: [1.0; 13],
            sample_counts: [0; 13],
        }
    }

    /// Multiplier to apply to forecast rain for a given month
    /// (1-indexed, Jan = 1). Returns 1.0 for out-of-range months.
    pub fn multiplier_for(&self, month: u32) -> f64 {
        let m = month as usize;
        if (1..=12).contains(&m) {
            self.multipliers[m]
        } else {
            1.0
        }
    }

    /// How many observations backed this month's multiplier. Useful
    /// for the dashboard's "we don't have enough data yet" disclosure.
    pub fn sample_count_for(&self, month: u32) -> usize {
        let m = month as usize;
        if (1..=12).contains(&m) {
            self.sample_counts[m]
        } else {
            0
        }
    }

    /// Build the model from a slice of observations. `today` anchors
    /// the rolling window: only observations within `window_days` of
    /// today are considered. Pass `None` for `window_days` to use the
    /// default.
    pub fn from_observations(
        observations: &[Observation],
        today: NaiveDate,
        window_days: Option<i64>,
    ) -> Self {
        let window = window_days.unwrap_or(DEFAULT_WINDOW_DAYS);
        let mut per_month: [Vec<f64>; 13] = Default::default();
        for obs in observations {
            // Skip if outside the window.
            let age = (today - obs.date).num_days();
            if age < 0 || age > window {
                continue;
            }
            // Skip days where the multiplicative model can't speak.
            if obs.predicted_in < NOISE_FLOOR_IN && obs.observed_in < NOISE_FLOOR_IN {
                continue;
            }
            if obs.predicted_in < NOISE_FLOOR_IN {
                continue;
            }
            let ratio = obs.observed_in / obs.predicted_in;
            // Defensive cap so a single 100x outlier (sensor glitch)
            // can't pull the median; we clamp at the bias bounds
            // anyway, but pre-clamping keeps the median stable.
            let bounded = ratio.clamp(0.0, 5.0);
            let m = obs.date.month() as usize;
            per_month[m].push(bounded);
        }

        let mut out = Self::identity();
        for (m, samples) in per_month.iter_mut().enumerate().skip(1) {
            let count = samples.len();
            out.sample_counts[m] = count;
            if count >= MIN_OBSERVATIONS {
                let median = robust_median(samples);
                out.multipliers[m] = median.clamp(BIAS_FLOOR, BIAS_CEIL);
            }
        }
        out
    }

    /// One-line human description for a given month.
    pub fn describe_month(&self, month: u32) -> String {
        let m = self.multiplier_for(month);
        let n = self.sample_count_for(month);
        if n < MIN_OBSERVATIONS {
            return format!("no correction (need {MIN_OBSERVATIONS} days; have {n})");
        }
        if (m - 1.0).abs() < 0.02 {
            return format!("no measurable bias ({n} days)");
        }
        if m < 1.0 {
            let pct = ((1.0 - m) * 100.0).round() as i64;
            format!("forecast over-predicts by {pct}% (correction {m:.2}x, {n} days)")
        } else {
            let pct = ((m - 1.0) * 100.0).round() as i64;
            format!("forecast under-predicts by {pct}% (correction {m:.2}x, {n} days)")
        }
    }
}

/// Median of a slice. Mutates the slice in place (in-place sort) to
/// avoid an allocation. For even-length slices, averages the two
/// middle values. Returns 1.0 for an empty slice as a safe identity.
fn robust_median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 1.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = v.len() / 2;
    if v.len() % 2 == 1 {
        v[mid]
    } else {
        (v[mid - 1] + v[mid]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(y: i32, m: u32, d: u32, predicted: f64, observed: f64) -> Observation {
        Observation::new(
            NaiveDate::from_ymd_opt(y, m, d).unwrap(),
            predicted,
            observed,
        )
    }

    #[test]
    fn identity_returns_one_everywhere() {
        let model = BiasModel::identity();
        for m in 1..=12u32 {
            assert_eq!(model.multiplier_for(m), 1.0);
        }
    }

    #[test]
    fn insufficient_data_returns_identity() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 15).unwrap();
        let observations = vec![obs(2026, 7, 1, 0.5, 0.3), obs(2026, 7, 5, 0.4, 0.2)];
        let model = BiasModel::from_observations(&observations, today, None);
        assert_eq!(model.multiplier_for(7), 1.0);
        assert_eq!(model.sample_count_for(7), 2);
    }

    #[test]
    fn consistent_overprediction_yields_correction_below_one() {
        let today = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        // 6 days of forecast over-predicting by 40% (ratio observed/predicted = 0.6).
        let observations: Vec<Observation> = (1..=6).map(|d| obs(2026, 7, d, 0.50, 0.30)).collect();
        let model = BiasModel::from_observations(&observations, today, None);
        let m = model.multiplier_for(7);
        assert!((m - 0.6).abs() < 0.01, "expected ~0.6 multiplier, got {m}");
    }

    #[test]
    fn consistent_underprediction_yields_correction_above_one() {
        let today = NaiveDate::from_ymd_opt(2026, 2, 28).unwrap();
        // 8 days, forecast 0.20, observed 0.30 (1.5x ratio).
        let observations: Vec<Observation> = (1..=8).map(|d| obs(2026, 2, d, 0.20, 0.30)).collect();
        let model = BiasModel::from_observations(&observations, today, None);
        let m = model.multiplier_for(2);
        assert!(
            (m - 1.5).abs() < 0.01,
            "expected 1.5 multiplier (clamped to ceiling), got {m}"
        );
    }

    #[test]
    fn dry_forecast_days_are_excluded() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 30).unwrap();
        // 4 dry days + 5 wet days with predicted=0.4 observed=0.4 (1.0x).
        let mut observations: Vec<Observation> = (1..=4)
            .map(|d| obs(2026, 8, d, 0.00, 0.30)) // dry forecast, wet reality (excluded)
            .collect();
        observations.extend((10..=14).map(|d| obs(2026, 8, d, 0.40, 0.40)));
        let model = BiasModel::from_observations(&observations, today, None);
        // Only 5 wet observations should count (all 1.0x).
        assert_eq!(model.sample_count_for(8), 5);
        assert!((model.multiplier_for(8) - 1.0).abs() < 0.01);
    }

    #[test]
    fn window_excludes_old_observations() {
        let today = NaiveDate::from_ymd_opt(2026, 9, 15).unwrap();
        // Old observations (>90 days back) should not count.
        let observations: Vec<Observation> = (1..=10)
            .map(|d| obs(2025, 9, d, 0.50, 0.20)) // ~370 days back, ratio 0.4
            .collect();
        let model = BiasModel::from_observations(&observations, today, None);
        assert_eq!(model.sample_count_for(9), 0);
        assert_eq!(model.multiplier_for(9), 1.0);
    }

    #[test]
    fn multiplier_clamps_below_floor() {
        let today = NaiveDate::from_ymd_opt(2026, 4, 28).unwrap();
        // Extreme overprediction (ratio = 0.2). Clamped to BIAS_FLOOR=0.5.
        let observations: Vec<Observation> = (1..=6).map(|d| obs(2026, 4, d, 1.00, 0.20)).collect();
        let model = BiasModel::from_observations(&observations, today, None);
        assert_eq!(model.multiplier_for(4), BIAS_FLOOR);
    }

    #[test]
    fn multiplier_clamps_above_ceil() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 28).unwrap();
        // Extreme underprediction (ratio = 3.0). Clamped to BIAS_CEIL=1.5.
        let observations: Vec<Observation> =
            (1..=6).map(|d| obs(2026, 11, d, 0.10, 0.30)).collect();
        let model = BiasModel::from_observations(&observations, today, None);
        assert_eq!(model.multiplier_for(11), BIAS_CEIL);
    }

    #[test]
    fn out_of_range_month_returns_one() {
        let model = BiasModel::identity();
        assert_eq!(model.multiplier_for(0), 1.0);
        assert_eq!(model.multiplier_for(13), 1.0);
        assert_eq!(model.multiplier_for(255), 1.0);
    }

    #[test]
    fn describe_month_reads_clearly() {
        let today = NaiveDate::from_ymd_opt(2026, 6, 28).unwrap();
        let overpredict: Vec<Observation> = (1..=6).map(|d| obs(2026, 6, d, 1.00, 0.70)).collect();
        let model = BiasModel::from_observations(&overpredict, today, None);
        let desc = model.describe_month(6);
        assert!(desc.contains("over-predicts"), "got: {desc}");
        assert!(desc.contains("30%"), "got: {desc}");
    }

    #[test]
    fn robust_median_handles_odd_and_even() {
        let mut a = [1.0, 2.0, 3.0];
        assert_eq!(robust_median(&mut a), 2.0);
        let mut b = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(robust_median(&mut b), 2.5);
        let mut c: [f64; 0] = [];
        assert_eq!(robust_median(&mut c), 1.0);
    }
}
