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
    /// Representative relative humidity for the day, % (0-100). Open-Meteo's
    /// daily rollup doesn't expose humidity directly, so this is derived from
    /// the hourly forecast: the humidity at the hour nearest the day's peak
    /// temperature (the afternoon high), which is the RH that physically
    /// co-occurs with `temp_max_f`. 0 when no hourly data covers this day
    /// (e.g. future days past the 48h hourly window, or older snapshots that
    /// predate this field). Used by `max_heat_index_n_day` so each day's high
    /// temp is paired with THAT day's humidity, never a stale post-rain "now".
    #[serde(default)]
    pub humidity_pct: u32,
    pub precip_sum_in: f64,
    pub precip_probability_max: u32,
    pub wind_max_mph: f64,
    /// Daily peak wind GUST, mph (Open-Meteo wind_gusts_10m_max). Higher than
    /// wind_max_mph (sustained); this is what a high-wind alert keys on. This
    /// is the modeled/forecast gust, not the station's (wind-shadowed) reading.
    pub wind_gust_max_mph: f64,
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
    /// UTC epoch of the most recent successful fetch.
    pub last_refresh_epoch: i64,
    /// True when the most recent fetch completed without error.
    pub source_reachable: bool,
    /// Display name of the forecast source currently driving this forecast
    /// (e.g. "Open-Meteo", "NWS", "Met.no"). Set by the producer; the
    /// forecast_bridge fills it from the source id if a producer left it blank.
    /// Empty only before the first forecast lands.
    #[serde(default)]
    pub source_label: String,
    /// IANA timezone name for the forecast point (e.g. America/New_York).
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
/// counter and the skip-check agree on what counts as "wet." Pub so the
/// refresher's observed-rain counter (forecast_observations) applies
/// the exact same floor as the model-based counter below.
pub const SIGNIFICANT_RAIN_IN: f64 = 0.05;

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

    /// Raw (probability-blind) sum of precipitation over the next `n` future
    /// days (skipping today, starting at daily[1]), in inches. The unweighted
    /// companion to `future_n_day_weighted_precip_in`, used for the rain-outlook
    /// display so it reads from the same live forecast as the weighted bars, the
    /// verdict strip, and the engine, rather than a separate HA template sensor.
    pub fn future_n_day_precip_in(&self, n: usize) -> f64 {
        self.daily
            .iter()
            .skip(1)
            .take(n)
            .map(|d| d.precip_sum_in)
            .sum()
    }

    /// Raw sum of OBSERVED precipitation over the last `n` past days
    /// (`past_daily`, stored earliest→latest, so the last `n` entries are
    /// the most recent), in inches. The backward-looking companion to
    /// `future_n_day_precip_in`: it reads measured rain that already fell,
    /// not the forecast. Caps `n` at the available past window; returns 0
    /// when `past_daily` is empty.
    pub fn past_n_day_precip_in(&self, n: usize) -> f64 {
        let len = self.past_daily.len();
        let start = len.saturating_sub(n);
        self.past_daily[start..]
            .iter()
            .map(|d| d.precip_sum_in)
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

    /// Maximum heat index ("feels-like" temperature) over the next `n` daily
    /// forecast entries (today + next `n-1`), in °F. Each day's high temp is
    /// paired with THAT day's humidity, so the Rothfusz regression only ever
    /// sees a physically co-occurring (temp, RH) pair.
    ///
    /// This is the correct way to compute a 3-day heat-index peak: pairing the
    /// 3-day MAX temperature with the CURRENT humidity (e.g. a saturated post-
    /// rain 3:40am reading) feeds the regression a temp/RH combination that
    /// never co-occurs and overshoots to a physically-impossible value. Returns
    /// 0.0 when no daily entry carries a humidity reading (caller falls back to
    /// the now value).
    ///
    /// Days with no derived humidity (`humidity_pct == 0`, e.g. future days
    /// past the 48h hourly window) are skipped so a hot day with a missing-data
    /// 0% RH can't masquerade as a low (and so wrong) feels-like.
    ///
    /// ssr-only: depends on the engine's `heat_index_f`, which lives behind the
    /// `ssr` feature. The browser never computes this (it reads the already-
    /// computed `SkipCheck.heat_index_max_3day_f` off the snapshot).
    #[cfg(feature = "ssr")]
    pub fn max_heat_index_n_day(&self, n: usize) -> f64 {
        self.daily
            .iter()
            .take(n)
            .filter(|d| d.humidity_pct > 0)
            .map(|d| crate::engine::skip_rules::heat_index_f(d.temp_max_f, d.humidity_pct as f64))
            .fold(0.0_f64, f64::max)
    }

    /// Fill each daily entry's `humidity_pct` from the hourly forecast: for a
    /// daily entry that still reads 0 (no humidity from the source's own daily
    /// rollup), use the humidity at the hour within that day whose temperature
    /// is closest to the day's `temp_max_f`. That is the RH that physically
    /// co-occurs with the afternoon high, which is what `max_heat_index_n_day`
    /// needs to avoid pairing the day's peak temp with a saturated post-rain
    /// "now". A daily entry already carrying humidity (a source that reports a
    /// daily RH directly) is left untouched. Idempotent. Producers (Open-Meteo
    /// + the alternate sources) call this after building both arrays so every
    /// forecast source feeds the engine the same physically-valid pairing.
    pub fn backfill_daily_humidity(&mut self) {
        if self.hourly.is_empty() {
            return;
        }
        const DAY_SECS: i64 = 24 * 3600;
        for d in self.daily.iter_mut() {
            if d.humidity_pct > 0 {
                continue;
            }
            let day_start = d.time_epoch;
            let temp_max = d.temp_max_f;
            if let Some(h) = self
                .hourly
                .iter()
                .filter(|h| h.time_epoch >= day_start && h.time_epoch < day_start + DAY_SECS)
                .min_by(|a, b| {
                    (a.temp_f - temp_max)
                        .abs()
                        .partial_cmp(&(b.temp_f - temp_max).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                d.humidity_pct = h.humidity_pct;
            }
        }
    }

    /// Today's forecast peak wind, mph. None on empty daily.
    pub fn wind_max_today_mph(&self) -> Option<f64> {
        self.daily.first().map(|d| d.wind_max_mph)
    }

    /// Today's forecast peak wind GUST, mph. None on empty daily. Drives the
    /// high-wind push (the Tempest is wind-shadowed, so gusts come from the
    /// Open-Meteo forecast instead of the station's measured value).
    pub fn wind_gust_max_today_mph(&self) -> Option<f64> {
        self.daily.first().map(|d| d.wind_gust_max_mph)
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
    ///   0 , already wet today,
    ///   1 , yesterday was wet but today isn't yet,
    ///   N , N consecutive past days dry, today dry,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn past(precip: &[f64]) -> ForecastSnapshot {
        ForecastSnapshot {
            // past_daily is stored earliest→latest.
            past_daily: precip
                .iter()
                .map(|&p| DailyEntry {
                    precip_sum_in: p,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn day(temp_max_f: f64, humidity_pct: u32) -> DailyEntry {
        DailyEntry {
            temp_max_f,
            humidity_pct,
            ..Default::default()
        }
    }

    #[test]
    fn max_heat_index_pairs_each_day_temp_with_that_day_humidity() {
        // A hot, dry afternoon (100°F @ 40% RH) vs a cooler, more humid one
        // (85°F @ 70% RH). The hotter day wins on feels-like; the per-day calc
        // pairs each day's high temp with THAT day's humidity and takes the max.
        let hot_dry = crate::engine::skip_rules::heat_index_f(100.0, 40.0);
        let cool_humid = crate::engine::skip_rules::heat_index_f(85.0, 70.0);
        assert!(hot_dry > cool_humid, "sanity: {hot_dry} > {cool_humid}");

        let fc = ForecastSnapshot {
            daily: vec![day(100.0, 40), day(85.0, 70)],
            ..Default::default()
        };
        let hi = fc.max_heat_index_n_day(3);
        assert!(
            (hi - hot_dry).abs() < 1e-9,
            "max heat index picks the higher per-day feels-like, got {hi}"
        );
    }

    #[test]
    fn max_heat_index_does_not_inflate_on_post_rain_now() {
        // The incident: a forecast high of 93.5°F whose THAT-day afternoon RH is
        // ~50%. Pairing 93.5°F with the saturated post-rain CURRENT humidity
        // (97%, a 3:40am reading) overshoots the Rothfusz regression to ~147°F.
        // The per-day calc pairs 93.5°F with the day's own ~50% RH and stays
        // realistic (~100°F), far below the bogus value.
        let realistic = crate::engine::skip_rules::heat_index_f(93.5, 50.0);
        let inflated = crate::engine::skip_rules::heat_index_f(93.5, 97.0);
        assert!(inflated > 140.0, "the buggy pairing overshoots: {inflated}");

        let fc = ForecastSnapshot {
            daily: vec![day(93.5, 50)],
            ..Default::default()
        };
        let hi = fc.max_heat_index_n_day(3);
        assert!(
            (hi - realistic).abs() < 1e-9,
            "per-day calc uses the day's own RH, got {hi}"
        );
        assert!(
            (95.0..110.0).contains(&hi),
            "post-rain-now does not inflate the per-day heat index: {hi}"
        );
        assert!(hi < inflated - 40.0, "per-day calc is far below the bug");
    }

    #[test]
    fn max_heat_index_skips_days_without_humidity_and_handles_empty() {
        // No daily entries -> 0.0 (caller falls back to the now value).
        assert!((ForecastSnapshot::default().max_heat_index_n_day(3) - 0.0).abs() < 1e-9);

        // A day with humidity_pct == 0 (no hourly coverage) is skipped, so a hot
        // day with missing humidity can't masquerade as a low feels-like.
        let only_missing = ForecastSnapshot {
            daily: vec![day(100.0, 0)],
            ..Default::default()
        };
        assert!((only_missing.max_heat_index_n_day(3) - 0.0).abs() < 1e-9);

        // n caps the window: a hot day past `n` doesn't count.
        let fc = ForecastSnapshot {
            daily: vec![day(85.0, 60), day(88.0, 60), day(110.0, 60)],
            ..Default::default()
        };
        let two = fc.max_heat_index_n_day(2);
        let three = fc.max_heat_index_n_day(3);
        assert!(three > two, "the 110°F day only counts within n=3");
    }

    #[test]
    fn past_n_day_precip_sums_most_recent_entries() {
        // earliest→latest: [0.10, 0.20, 1.50] (1.50" yesterday).
        let fc = past(&[0.10, 0.20, 1.50]);
        // n=0 includes no past days.
        assert!((fc.past_n_day_precip_in(0) - 0.0).abs() < 1e-9);
        // n=1 is yesterday only (the last entry).
        assert!((fc.past_n_day_precip_in(1) - 1.50).abs() < 1e-9);
        // n=2 is yesterday + the day before.
        assert!((fc.past_n_day_precip_in(2) - 1.70).abs() < 1e-9);
        // n beyond the window saturates at the full sum.
        assert!((fc.past_n_day_precip_in(9) - 1.80).abs() < 1e-9);
        // Empty past window is 0.
        assert!((ForecastSnapshot::default().past_n_day_precip_in(3) - 0.0).abs() < 1e-9);
    }
}
