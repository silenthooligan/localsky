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
    /// Max precipitation probability for the day, percent. `None` when the
    /// provider reports no probability series (custom HTTP/MQTT forecast
    /// mappings, provider gaps): the old bare 0 was ambiguous between "dry
    /// day" and "not reported", which zeroed the probability-weighted rain
    /// rollups and read as "certainly dry" on the HA sensor. A reported 0
    /// stays `Some(0)`. `#[serde(default)]` so persisted caches deserialize.
    #[serde(default)]
    pub precip_probability_max: Option<u32>,
    pub wind_max_mph: f64,
    /// Daily peak wind GUST, mph (Open-Meteo wind_gusts_10m_max). Higher than
    /// wind_max_mph (sustained); this is what a high-wind alert keys on. This
    /// is the modeled/forecast gust, not the station's (wind-shadowed) reading.
    pub wind_gust_max_mph: f64,
    pub uv_index_max: f64,
    pub sunrise_epoch: i64,
    pub sunset_epoch: i64,
    // ---- Extended variables (2026-07, Open-Meteo only; every other
    // provider leaves them at the serde default, meaning "unknown"). All
    // additive so persisted caches and older API clients keep parsing. ----
    /// Hours of the day with measurable precipitation (Open-Meteo
    /// precipitation_hours). Distinguishes an all-day soaker from a burst:
    /// the same 0.3in over 8h infiltrates, over 20min it mostly runs off.
    /// 0 = dry day OR provider doesn't report it (check `precip_sum_in`).
    #[serde(default)]
    pub precip_hours: f64,
    /// Stratiform rain component, inches (rain_sum). With `showers_sum_in`
    /// splits the day's precip into steady vs convective character.
    #[serde(default)]
    pub rain_sum_in: f64,
    /// Convective showers component, inches (showers_sum).
    #[serde(default)]
    pub showers_sum_in: f64,
    /// Snowfall total, inches (snowfall_sum; follows precipitation_unit).
    #[serde(default)]
    pub snowfall_sum_in: f64,
    /// Seconds of actual sunshine (sunshine_duration). Compare against
    /// daylight (sunset - sunrise) for a cloudiness/solar-stress read.
    #[serde(default)]
    pub sunshine_s: f64,
    /// Peak apparent ("feels like") temperature, °F.
    #[serde(default)]
    pub apparent_temp_max_f: f64,
    /// Peak CAPE, J/kg (cape_max): thunderstorm fuel. >1000 unstable,
    /// >2500 strongly unstable. Display/advisor only; never a skip input.
    #[serde(default)]
    pub cape_max_jkg: f64,
    /// FAO-56 reference evapotranspiration for the day, inches
    /// (et0_fao_evapotranspiration; follows precipitation_unit). The
    /// provider's own ET0, useful as a cross-check against the engine's
    /// station-data FAO-56 computation.
    #[serde(default)]
    pub et0_in: f64,
}

impl DailyEntry {
    /// Weight for probability-weighting this day's rain amount, 0.0..=1.0.
    /// `None` (the provider reports no probability) weights at FULL value:
    /// treating forecast rain as certain is the conservative direction for a
    /// skip decision (hold water ahead of forecast rain), where the old
    /// missing-equals-0 zeroed the expected rain and watered ahead of
    /// storms. A reported 0 stays a real "the model says it will not rain".
    pub fn precip_weight(&self) -> f64 {
        self.precip_probability_max
            .map(|p| f64::from(p) / 100.0)
            .unwrap_or(1.0)
    }
}

/// One hour in the 48-hour rolling forecast.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HourlyEntry {
    pub time_epoch: i64,
    pub weather_code: u32,
    pub temp_f: f64,
    pub apparent_temp_f: f64,
    pub precip_in: f64,
    /// Precipitation probability for the hour, percent. `None` = provider
    /// reports no probability (see `DailyEntry::precip_probability_max`).
    #[serde(default)]
    pub precip_probability: Option<u32>,
    pub wind_mph: f64,
    pub wind_dir_deg: u32,
    pub humidity_pct: u32,
    pub cloud_cover_pct: u32,
    // ---- Extended variables (2026-07, Open-Meteo only; serde defaults =
    // "unknown" for other providers and pre-upgrade persisted caches). ----
    /// FAO-56 reference ET for this hour, inches. Summing the hours since
    /// local midnight gives "ET spent so far today", which the water
    /// balance card uses instead of charging the whole day's ET up front.
    #[serde(default)]
    pub et0_in: f64,
    /// Vapour pressure deficit, kPa. Sustained > ~1.6 kPa means high
    /// transpiration stress (plants lose water faster than typical Kc
    /// assumptions); advisor signal only.
    #[serde(default)]
    pub vpd_kpa: f64,
    /// Modeled volumetric soil moisture, m³/m³, 3-9 cm layer (turf root
    /// zone top). Model data, NOT a probe: measured soil always wins.
    #[serde(default)]
    pub soil_moisture_3_9_vwc: f64,
    /// Modeled volumetric soil moisture, m³/m³, 9-27 cm layer (deep roots).
    #[serde(default)]
    pub soil_moisture_9_27_vwc: f64,
    /// Modeled soil temperature at 6 cm, °F. Drives dormancy/germination
    /// context (cool-season vs warm-season turf activity).
    #[serde(default)]
    pub soil_temp_6cm_f: f64,
    /// Wind gusts, mph. The hourly companion to the daily gust max; spray
    /// drift timing wants the per-hour shape, not just the day peak.
    #[serde(default)]
    pub wind_gusts_mph: f64,
    /// Snowfall this hour, inches.
    #[serde(default)]
    pub snowfall_in: f64,
    // ---- Condition-awareness variables (2026-07). Same additive rules. ----
    /// Snow currently on the ground, feet (snow_depth; follows the imperial
    /// request). Mountain/winter installs; 0 elsewhere.
    #[serde(default)]
    pub snow_depth_ft: f64,
    /// Freezing level altitude, feet MSL. Rain-vs-snow line for mountain
    /// users; compare against local elevation.
    #[serde(default)]
    pub freezing_level_ft: f64,
    /// Visibility, feet. Fog/marine-layer awareness (5280 ft = 1 mile).
    #[serde(default)]
    pub visibility_ft: f64,
    /// Mean-sea-level pressure, hPa. The TREND (falling fast = storm
    /// approach) matters more than the value.
    #[serde(default)]
    pub pressure_msl_hpa: f64,
    /// Wet-bulb temperature, F. Heat-safety ceiling: evaporative cooling
    /// stops working as this approaches body temperature; sustained 80+ is
    /// dangerous for outdoor work regardless of the heat index.
    #[serde(default)]
    pub wet_bulb_f: f64,
}

impl HourlyEntry {
    /// Probability weight for this hour's precipitation, 0.0..=1.0.
    /// `None` (the provider reports no probability) weights at FULL value,
    /// the same conservative direction `DailyEntry::precip_weight` takes:
    /// unknown probability is treated as certain rain, which holds water
    /// rather than watering ahead of a storm. A reported 0 stays a real
    /// "the model says it will not rain".
    pub fn precip_weight(&self) -> f64 {
        self.precip_probability
            .map(|p| f64::from(p) / 100.0)
            .unwrap_or(1.0)
    }
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
    /// True when the source serving this forecast is NOT the top-priority
    /// enabled forecast source: the configured primary is quiet and a
    /// lower-ranked provider failed over. Stamped by the forecast_bridge
    /// from the live priority map at store time, so the UI can say
    /// "via NWS · backup" instead of presenting failover data as primary.
    #[serde(default)]
    pub source_is_backup: bool,
    /// IANA timezone name for the forecast point (e.g. America/New_York).
    pub timezone: String,
    /// 7 entries: today plus next 6.
    pub daily: Vec<DailyEntry>,
    /// Past N days (stored earliest first). The model archive: real
    /// archived daily values from the latest model run, populated only
    /// by the Open-Meteo fetch (`OpenMeteoConfig.past_days`, clamped
    /// 1..=7, default 3); every other provider ships this empty. Feeds
    /// `days_since_significant_rain` and the observed-rain ladder's
    /// model-archive rung.
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

    /// Probability-WEIGHTED forecast precipitation over the next `n`
    /// hourly entries, inches. Each hour's depth is scaled by that hour's
    /// `precip_weight()`, the same treatment the balance's forward credit
    /// and the wire's weighted 7-day total already get.
    ///
    /// The raw sum above is a deterministic model total: a 20%-probability
    /// drizzle counts the same as a certain soaking. Comparing that raw
    /// figure against a 0.10 inch threshold let a low-probability forecast
    /// cancel a whole session, every day, with nothing visible about it.
    pub fn next_n_hours_precip_weighted_in(&self, n: usize) -> f64 {
        self.hourly
            .iter()
            .take(n)
            .map(|h| h.precip_in * h.precip_weight())
            .sum()
    }

    /// Epoch of the NEXT local midnight in the model's own frame:
    /// tomorrow's daily entry starts at 00:00 local (that is Open-Meteo's
    /// daily contract under timezone=auto), so no tz math is needed here.
    /// i64::MAX when the snapshot has no tomorrow (treat "rest of today"
    /// as unbounded rather than cutting the window short).
    fn next_local_midnight_epoch(&self) -> i64 {
        self.daily.get(1).map(|d| d.time_epoch).unwrap_or(i64::MAX)
    }

    /// Model ET0 already SPENT today as of `now_epoch`, mm. The hourly window
    /// is forward-only (now +47h), so spent is derived by subtraction:
    /// today's full-day ET0 minus the remaining hourly ET0 between now
    /// and local midnight. 0 when the provider sends no hourly ET0 curve
    /// (every remaining hour 0 would otherwise claim the whole day is
    /// already spent).
    ///
    /// Stale-anchor guard: between local midnight and the next forecast fetch,
    /// daily[0] is still YESTERDAY and the evening fetch's hourly window sits
    /// past that day's midnight, so the subtraction would charge yesterday's
    /// FULL day as spent at 00:01. When `now_epoch` falls outside daily[0]'s
    /// own local day ([time_epoch, next local midnight)), spent is 0; the next
    /// fetch re-anchors daily[0] and the midday math resumes. (A snapshot with
    /// no tomorrow entry has no upper bound; that conservative edge keeps the
    /// prior behavior.)
    pub fn eto_spent_today_mm(&self, now_epoch: i64) -> f64 {
        let Some(today) = self.daily.first() else {
            return 0.0;
        };
        if today.et0_in <= 0.0 {
            return 0.0;
        }
        let midnight = self.next_local_midnight_epoch();
        if now_epoch < today.time_epoch || now_epoch >= midnight {
            return 0.0;
        }
        let remaining_in: f64 = self
            .hourly
            .iter()
            .filter(|h| h.time_epoch < midnight)
            .map(|h| h.et0_in)
            .sum();
        let has_hourly_curve = self.hourly.iter().any(|h| h.et0_in > 0.0);
        if !has_hourly_curve {
            return 0.0;
        }
        ((today.et0_in - remaining_in) * 25.4).max(0.0)
    }

    /// True when this snapshot carries any of the extended model series
    /// (ET0 curve, VPD, model soil). Today only Open-Meteo produces them;
    /// the check is capability-based, not provider-based, so any future
    /// producer that sends them counts.
    pub fn has_extended_series(&self) -> bool {
        // Check BOTH hourly and daily. `graft_extended_from` fills daily too, so
        // a daily-only owner (e.g. NWS when its optional hourly endpoint failed)
        // that received a daily graft must report `true` here, or the retro-graft
        // one-shot guard (`!current.has_extended_series()`) never trips and every
        // donor emit re-stores the same snapshot forever (a spurious SSE push +
        // disk write each cycle). The daily-extended fields below are Open-Meteo
        // only; no non-OM provider sets them, so this never false-positives on a
        // pristine owner.
        self.hourly
            .iter()
            .any(|h| h.et0_in > 0.0 || h.vpd_kpa > 0.0 || h.soil_moisture_3_9_vwc > 0.0)
            || self.daily.iter().any(|d| {
                d.precip_hours > 0.0 || d.cape_max_jkg > 0.0 || d.et0_in > 0.0 || d.sunshine_s > 0.0
            })
    }

    /// Graft the ADVISORY extended series from `donor` into this snapshot,
    /// filling only fields that are zero here and only entries whose
    /// `time_epoch` matches exactly, so mixed data can never misalign.
    ///
    /// Why: forecast arbitration is whole-snapshot (mixing core fields
    /// across providers would produce an incoherent forecast), and the US
    /// default chain ranks NWS above Open-Meteo, but the extended series
    /// are Open-Meteo-only. Without this graft, the advisory surfaces
    /// (VPD stress, model soil, ET-spent, rain character) would blank on
    /// every install whose primary is not Open-Meteo, i.e. the default US
    /// install. Core fields (temps, precip, wind, codes) are NEVER
    /// touched: the owner's forecast stays the owner's forecast.
    ///
    /// LOCATION SAFETY: the donor is task-local state in the bridge that
    /// survives a wizard location change (only the priority map hot-reloads),
    /// and hourly epochs are top-of-hour UTC, IDENTICAL across locations, so a
    /// pure epoch match would copy the OLD location's soil/VPD/fog onto the NEW
    /// location's forecast until the donor re-emits. Gate on the timezone: a
    /// forecast for a materially different location almost always carries a
    /// different IANA zone, so a mismatch means "not the same place" and the
    /// graft is skipped. (Same-zone nudges within a region still graft; the
    /// advisory conditions are near-identical there.)
    pub fn graft_extended_from(&mut self, donor: &ForecastSnapshot) {
        if !donor.timezone.is_empty()
            && !self.timezone.is_empty()
            && donor.timezone != self.timezone
        {
            return;
        }
        for h in &mut self.hourly {
            let Some(dh) = donor.hourly.iter().find(|d| d.time_epoch == h.time_epoch) else {
                continue;
            };
            if h.et0_in == 0.0 {
                h.et0_in = dh.et0_in;
            }
            if h.vpd_kpa == 0.0 {
                h.vpd_kpa = dh.vpd_kpa;
            }
            if h.soil_moisture_3_9_vwc == 0.0 {
                h.soil_moisture_3_9_vwc = dh.soil_moisture_3_9_vwc;
            }
            if h.soil_moisture_9_27_vwc == 0.0 {
                h.soil_moisture_9_27_vwc = dh.soil_moisture_9_27_vwc;
            }
            if h.soil_temp_6cm_f == 0.0 {
                h.soil_temp_6cm_f = dh.soil_temp_6cm_f;
            }
            if h.wind_gusts_mph == 0.0 {
                h.wind_gusts_mph = dh.wind_gusts_mph;
            }
            if h.snowfall_in == 0.0 {
                h.snowfall_in = dh.snowfall_in;
            }
            if h.snow_depth_ft == 0.0 {
                h.snow_depth_ft = dh.snow_depth_ft;
            }
            if h.freezing_level_ft == 0.0 {
                h.freezing_level_ft = dh.freezing_level_ft;
            }
            if h.visibility_ft == 0.0 {
                h.visibility_ft = dh.visibility_ft;
            }
            if h.pressure_msl_hpa == 0.0 {
                h.pressure_msl_hpa = dh.pressure_msl_hpa;
            }
            if h.wet_bulb_f == 0.0 {
                h.wet_bulb_f = dh.wet_bulb_f;
            }
        }
        for d in &mut self.daily {
            // Match by the donor DAY that CONTAINS this entry, not nearest-epoch.
            // Open-Meteo stamps 00:00 local; NWS stamps period starts (06:00
            // daytime, 18:00 for a lone-night "Tonight" row). Nearest-epoch sent
            // an 18:00 owner entry to the donor's NEXT 00:00 (6h away < same-day
            // 18h), grafting TOMORROW's rain/CAPE/ET onto today's card ~half of
            // every day on the default NWS install. Donor entries are 00:00-local
            // and ~24h apart, so the day containing a target T is the donor entry
            // with the greatest midnight <= T (with 3h slack for a target stamped
            // just before midnight), accepted only if within ~30h (a full day +
            // the period-start offset) so a far-future target with no donor day
            // is left ungrafted rather than matched to the last day.
            let Some(dd) = donor
                .daily
                .iter()
                .filter(|x| x.time_epoch <= d.time_epoch + 3 * 3600)
                .max_by_key(|x| x.time_epoch)
                .filter(|x| d.time_epoch - x.time_epoch < 30 * 3600)
            else {
                continue;
            };
            if d.precip_hours == 0.0 {
                d.precip_hours = dd.precip_hours;
            }
            if d.rain_sum_in == 0.0 {
                d.rain_sum_in = dd.rain_sum_in;
            }
            if d.showers_sum_in == 0.0 {
                d.showers_sum_in = dd.showers_sum_in;
            }
            if d.snowfall_sum_in == 0.0 {
                d.snowfall_sum_in = dd.snowfall_sum_in;
            }
            if d.sunshine_s == 0.0 {
                d.sunshine_s = dd.sunshine_s;
            }
            if d.apparent_temp_max_f == 0.0 {
                d.apparent_temp_max_f = dd.apparent_temp_max_f;
            }
            if d.cape_max_jkg == 0.0 {
                d.cape_max_jkg = dd.cape_max_jkg;
            }
            if d.et0_in == 0.0 {
                d.et0_in = dd.et0_in;
            }
        }
    }

    /// Vapour pressure deficit (kPa): the current hour's value and the
    /// peak across the rest of today. (0.0, 0.0) when the provider sends
    /// no VPD.
    pub fn vpd_now_and_max_today(&self) -> (f64, f64) {
        // First hour WITH a value, not first hour: a non-OM owner's hourly
        // window can start in the past, before the donor's graft coverage,
        // so hourly[0] may be an ungrafted zero while the current hour is
        // fully decorated (observed live on the NWS 156h window).
        let now = self
            .hourly
            .iter()
            .map(|h| h.vpd_kpa)
            .find(|v| *v > 0.0)
            .unwrap_or(0.0);
        let midnight = self.next_local_midnight_epoch();
        let max_today = self
            .hourly
            .iter()
            .filter(|h| h.time_epoch < midnight)
            .map(|h| h.vpd_kpa)
            .fold(0.0_f64, f64::max);
        (now, max_today)
    }

    /// Probability-weighted rain forecast over the next `n` future days
    /// (skipping today, starting at daily[1]). Σ precip × prob/100.
    /// Caps `n` at the available daily window. Days without a probability
    /// weight at full value (see [`DailyEntry::precip_weight`]).
    pub fn future_n_day_weighted_precip_in(&self, n: usize) -> f64 {
        self.daily
            .iter()
            .skip(1)
            .take(n)
            .map(|d| d.precip_sum_in * d.precip_weight())
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

    /// Tomorrow's forecast precipitation total + probability max. The
    /// probability is `None` when the daily window doesn't reach tomorrow
    /// yet OR the provider reports no probability series.
    pub fn tomorrow_precip_with_prob_in(&self) -> (f64, Option<u32>) {
        self.daily
            .get(1)
            .map(|d| (d.precip_sum_in, d.precip_probability_max))
            .unwrap_or((0.0, None))
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
    fn weighted_rollup_takes_probability_less_days_at_full_value() {
        // daily[0] is today (skipped); daily[1..] carry: a 60%-prob day, a
        // provider-gap day (no probability), and a reported-0% day.
        let fc = ForecastSnapshot {
            daily: vec![
                DailyEntry::default(),
                DailyEntry {
                    precip_sum_in: 1.0,
                    precip_probability_max: Some(60),
                    ..Default::default()
                },
                DailyEntry {
                    precip_sum_in: 0.5,
                    precip_probability_max: None,
                    ..Default::default()
                },
                DailyEntry {
                    precip_sum_in: 2.0,
                    precip_probability_max: Some(0),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // 1.0*0.6 + 0.5*1.0 (unknown = certain, the safe skip direction)
        // + 2.0*0.0 (a REPORTED zero still zeroes).
        let got = fc.future_n_day_weighted_precip_in(3);
        assert!((got - 1.1).abs() < 1e-9, "weighted = {got}");

        // tomorrow_precip_with_prob_in carries the probability as an Option.
        let (amt, prob) = fc.tomorrow_precip_with_prob_in();
        assert!((amt - 1.0).abs() < 1e-9);
        assert_eq!(prob, Some(60));
        let (_, prob) = ForecastSnapshot::default().tomorrow_precip_with_prob_in();
        assert_eq!(prob, None, "no tomorrow entry = no probability claim");
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

    // ---- eto_spent_today_mm (subtraction + stale-anchor guard) ----

    /// Snapshot as fetched midday: daily[0] at local-midnight `day0` carrying a
    /// 0.18 in full-day ET0, tomorrow's entry supplying the next-midnight
    /// boundary, and two remaining evening hours of 0.04 in each on the curve.
    fn et_fc(day0: i64) -> ForecastSnapshot {
        ForecastSnapshot {
            daily: vec![
                DailyEntry {
                    time_epoch: day0,
                    et0_in: 0.18,
                    ..Default::default()
                },
                DailyEntry {
                    time_epoch: day0 + 86_400,
                    et0_in: 0.17,
                    ..Default::default()
                },
            ],
            hourly: vec![
                HourlyEntry {
                    time_epoch: day0 + 20 * 3600,
                    et0_in: 0.04,
                    ..Default::default()
                },
                HourlyEntry {
                    time_epoch: day0 + 21 * 3600,
                    et0_in: 0.04,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn eto_spent_midday_subtracts_the_remaining_hours() {
        let day0 = 1_750_000_000;
        let fc = et_fc(day0);
        // 18:00: (0.18 - 0.08 remaining) * 25.4 = 2.54 mm spent so far.
        let spent = fc.eto_spent_today_mm(day0 + 18 * 3600);
        assert!((spent - 2.54).abs() < 1e-9, "spent = {spent}");
    }

    #[test]
    fn eto_spent_is_zero_on_a_stale_pre_rollover_snapshot() {
        let day0 = 1_750_000_000;
        let mut fc = et_fc(day0);
        // Model the last EVENING fetch still cached at 00:30 the next local
        // day: the forward-only hourly window sits entirely in the new day, so
        // "remaining before daily[0]'s midnight" sums to 0 and the subtraction
        // would charge yesterday's FULL 4.572 mm as already spent. The guard
        // returns 0 until the next fetch re-anchors daily[0].
        fc.hourly = vec![
            HourlyEntry {
                time_epoch: day0 + 86_400 + 3600,
                et0_in: 0.01,
                ..Default::default()
            },
            HourlyEntry {
                time_epoch: day0 + 86_400 + 2 * 3600,
                et0_in: 0.02,
                ..Default::default()
            },
        ];
        let spent = fc.eto_spent_today_mm(day0 + 86_400 + 1800);
        assert!((spent - 0.0).abs() < 1e-9, "stale anchor spends 0: {spent}");
    }

    #[test]
    fn eto_spent_resumes_on_a_fresh_post_rollover_snapshot() {
        // The next fetch re-anchors daily[0] to the new day; midday math works
        // exactly as before the rollover.
        let day0 = 1_750_000_000 + 86_400;
        let fc = et_fc(day0);
        let spent = fc.eto_spent_today_mm(day0 + 18 * 3600);
        assert!((spent - 2.54).abs() < 1e-9, "spent = {spent}");
    }

    // ---- extended-series graft (advisory backfill across providers) ----

    fn hourly_at(epoch: i64) -> HourlyEntry {
        HourlyEntry {
            time_epoch: epoch,
            temp_f: 80.0,
            precip_in: 0.1,
            ..Default::default()
        }
    }

    #[test]
    fn graft_fills_only_zeroed_extended_fields_by_exact_epoch() {
        // NWS-style owner: core fields present, extended series absent.
        let mut owner = ForecastSnapshot {
            hourly: vec![hourly_at(1000), hourly_at(4600)],
            daily: vec![DailyEntry {
                time_epoch: 500,
                precip_sum_in: 0.4,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(!owner.has_extended_series());

        // Open-Meteo-style donor: same epochs, extended series present,
        // plus one entry at an epoch the owner lacks (must be ignored).
        let donor = ForecastSnapshot {
            hourly: vec![
                HourlyEntry {
                    et0_in: 0.02,
                    vpd_kpa: 1.4,
                    soil_moisture_3_9_vwc: 0.19,
                    soil_temp_6cm_f: 78.0,
                    wind_gusts_mph: 14.0,
                    ..hourly_at(1000)
                },
                HourlyEntry {
                    et0_in: 0.03,
                    ..hourly_at(9999)
                },
            ],
            daily: vec![DailyEntry {
                time_epoch: 500,
                precip_hours: 5.0,
                rain_sum_in: 0.3,
                showers_sum_in: 0.1,
                sunshine_s: 20000.0,
                apparent_temp_max_f: 101.0,
                cape_max_jkg: 2400.0,
                et0_in: 0.19,
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(donor.has_extended_series());

        owner.graft_extended_from(&donor);
        // Epoch 1000 matched: extended fields filled, core untouched.
        assert!((owner.hourly[0].et0_in - 0.02).abs() < 1e-9);
        assert!((owner.hourly[0].vpd_kpa - 1.4).abs() < 1e-9);
        assert!((owner.hourly[0].soil_moisture_3_9_vwc - 0.19).abs() < 1e-9);
        assert!(
            (owner.hourly[0].temp_f - 80.0).abs() < 1e-9,
            "core stays owner's"
        );
        // Epoch 4600 has no donor match: stays zero.
        assert!((owner.hourly[1].et0_in - 0.0).abs() < 1e-9);
        // Daily grafted by epoch, core precip untouched.
        assert!((owner.daily[0].precip_hours - 5.0).abs() < 1e-9);
        assert!((owner.daily[0].cape_max_jkg - 2400.0).abs() < 1e-9);
        assert!(
            (owner.daily[0].precip_sum_in - 0.4).abs() < 1e-9,
            "core stays owner's"
        );
        assert!(
            owner.has_extended_series(),
            "owner now carries the advisory series"
        );
    }

    #[test]
    fn graft_never_overwrites_a_provider_own_extended_value() {
        let mut owner = ForecastSnapshot {
            hourly: vec![HourlyEntry {
                vpd_kpa: 0.9,
                ..hourly_at(1000)
            }],
            ..Default::default()
        };
        let donor = ForecastSnapshot {
            hourly: vec![HourlyEntry {
                vpd_kpa: 1.8,
                et0_in: 0.02,
                ..hourly_at(1000)
            }],
            ..Default::default()
        };
        owner.graft_extended_from(&donor);
        assert!(
            (owner.hourly[0].vpd_kpa - 0.9).abs() < 1e-9,
            "own value wins"
        );
        assert!(
            (owner.hourly[0].et0_in - 0.02).abs() < 1e-9,
            "zeroed field fills"
        );
    }
}
