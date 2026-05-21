// Forecast snapshot types. Open-Meteo returns parallel arrays
// (time[], temperature_2m[], etc.); we flatten into Vec<DailyEntry>
// + Vec<HourlyEntry> for nicer iteration on the browser side.
//
// Times are stored as UTC epoch seconds; the browser uses Local for
// display so the hours line up with the user's wall clock.

use serde::{Deserialize, Serialize};

/// One row in the 7-day daily forecast strip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyEntry {
    /// UTC epoch (00:00 local for that day in the requested timezone).
    pub time_epoch: i64,
    /// WMO weather code for the day's dominant condition.
    pub weather_code: u32,
    pub temp_max_f: f64,
    pub temp_min_f: f64,
    pub precip_sum_in: f64,
    pub precip_probability_max: u32,
    pub wind_max_mph: f64,
    pub uv_index_max: f64,
    pub sunrise_epoch: i64,
    pub sunset_epoch: i64,
}

/// One hour in the 48-hour rolling forecast.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HourlyEntry {
    pub time_epoch: i64,
    pub weather_code: u32,
    pub temp_f: f64,
    pub apparent_temp_f: f64,
    pub precip_in: f64,
    pub precip_probability: u32,
    pub wind_mph: f64,
    pub wind_dir_deg: u32,
    pub humidity_pct: u32,
    pub cloud_cover_pct: u32,
}

/// Top-level forecast snapshot. Cheap to clone; arc-swapped into the
/// store on every refresh.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ForecastSnapshot {
    /// UTC epoch of the most recent successful Open-Meteo fetch.
    pub last_refresh_epoch: i64,
    /// True when the most recent fetch completed without error.
    pub source_reachable: bool,
    /// IANA timezone name returned by Open-Meteo (e.g. America/New_York).
    pub timezone: String,
    /// 7 entries: today plus next 6.
    pub daily: Vec<DailyEntry>,
    /// Past N days (most recent first reversed → earliest first). Used
    /// by the heat-advisory rule to compute `days_since_significant_rain`.
    /// Empty until past_days is added to the Open-Meteo request.
    pub past_daily: Vec<DailyEntry>,
    /// 48 entries: now plus next 47 hours, anchored on the top of
    /// the current hour.
    pub hourly: Vec<HourlyEntry>,
}

/// "Significant" rain threshold for the days-since-rain counter, in
/// inches. Same floor as the existing already-wet rule so the
/// counter and the skip-check agree on what counts as "wet."
const SIGNIFICANT_RAIN_IN: f64 = 0.05;

impl ForecastSnapshot {
    /// Sum of precipitation over the next `n` hourly entries, in inches.
    /// Saturates on short snapshots; returns 0 when hourly is empty.
    pub fn next_n_hours_precip_in(&self, n: usize) -> f64 {
        self.hourly.iter().take(n).map(|h| h.precip_in).sum()
    }

    /// Probability-weighted rain forecast over the next `n` future days
    /// (skipping today, starting at daily[1]). Σ precip × prob/100.
    /// Caps `n` at the available daily window.
    pub fn future_n_day_weighted_precip_in(&self, n: usize) -> f64 {
        self.daily
            .iter()
            .skip(1)
            .take(n)
            .map(|d| d.precip_sum_in * (d.precip_probability_max as f64) / 100.0)
            .sum()
    }

    /// Minimum hourly forecast temperature over the next 24 hours.
    /// Returns None when the hourly window is empty (caller falls back
    /// to a sensible default).
    pub fn min_temp_next_24h_f(&self) -> Option<f64> {
        self.hourly
            .iter()
            .take(24)
            .map(|h| h.temp_f)
            .fold(None, |acc, t| Some(acc.map_or(t, |a: f64| a.min(t))))
    }

    /// Maximum daily forecast temperature over today + next 2 days.
    pub fn max_temp_next_3d_f(&self) -> Option<f64> {
        self.daily
            .iter()
            .take(3)
            .map(|d| d.temp_max_f)
            .fold(None, |acc, t| Some(acc.map_or(t, |a: f64| a.max(t))))
    }

    /// Today's forecast peak wind, mph. None on empty daily.
    pub fn wind_max_today_mph(&self) -> Option<f64> {
        self.daily.first().map(|d| d.wind_max_mph)
    }

    /// Tomorrow's forecast precipitation total + probability max.
    /// Returns (0.0, 0) when daily window doesn't reach tomorrow yet.
    pub fn tomorrow_precip_with_prob_in(&self) -> (f64, u32) {
        self.daily
            .get(1)
            .map(|d| (d.precip_sum_in, d.precip_probability_max))
            .unwrap_or((0.0, 0))
    }

    /// Days since the last day with significant rain (≥ 0.05"). Walks
    /// `past_daily` newest-first, then folds in today's accumulated
    /// rain via `today_rain_in`. Returns:
    ///   0  — already wet today,
    ///   1  — yesterday was wet but today isn't yet,
    ///   N  — N consecutive past days dry, today dry,
    ///   past_daily.len() + 1 (saturating) when no past day was wet.
    pub fn days_since_significant_rain(&self, today_rain_in: f64) -> u32 {
        if today_rain_in >= SIGNIFICANT_RAIN_IN {
            return 0;
        }
        // past_daily is stored earliest→latest; iterate latest→earliest.
        for (i, d) in self.past_daily.iter().rev().enumerate() {
            if d.precip_sum_in >= SIGNIFICANT_RAIN_IN {
                return (i + 1) as u32;
            }
        }
        // No wet day in the past window. Saturate at window + 1.
        (self.past_daily.len() as u32).saturating_add(1)
    }

    /// True once the snapshot has at least today + tomorrow on hand.
    pub fn has_tomorrow(&self) -> bool {
        self.daily.len() >= 2
    }
}
