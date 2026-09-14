// Forecast snapshot types. Open-Meteo returns parallel arrays
// (time[], temperature_2m[], etc.); we flatten into Vec<DailyEntry>
// + Vec<HourlyEntry> for nicer iteration on the browser side.
//
// Times are stored as UTC epoch seconds; the browser uses Local for
// display so the hours line up with the user's wall clock.

use serde::{Deserialize, Serialize};

use crate::engine::calendar::Calendar;
use crate::engine::clock::CivilDay;

/// One row in the 7-day daily forecast strip.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DailyEntry {
    /// The provider's stamp for this day, as a day LABEL.
    ///
    /// The doc this replaces claimed "00:00 local for that day in the
    /// requested timezone". That is true of Open-Meteo alone. NWS stamps
    /// the 06:00 local daytime period start, and 18:00 for a lone night
    /// row; met.no stamps local noon; OpenWeather a midday value. Read as
    /// an instant in the wrong frame, an NWS stamp for a US Eastern yard
    /// is hour 10, which is the first hour of a great many midday
    /// watering bans, and the whole week comes back blocked.
    ///
    /// So it is no longer a number you can ask the hour of. Ask the
    /// deployment Calendar which day it labels. Wire-identical to the
    /// `time_epoch: i64` it replaces, including 0 for unknown.
    #[serde(
        rename = "time_epoch",
        with = "crate::engine::clock::marker_epoch",
        default
    )]
    pub day_marker: crate::engine::clock::DayMarker,
    /// WMO weather code for the day's dominant condition.
    pub weather_code: u32,
    /// Reported daily high, °F. None when the provider has no daytime
    /// period or omits the value; a reported 0°F remains Some(0.0).
    #[serde(default)]
    pub temp_max_f: Option<f64>,
    /// Reported daily low, °F. None when the overnight period or value is
    /// missing; missing evidence must not become a freezing forecast.
    #[serde(default)]
    pub temp_min_f: Option<f64>,
    /// Representative relative humidity for the day, % (0-100). Open-Meteo's
    /// daily rollup doesn't expose humidity directly, so this is derived from
    /// the hourly forecast: the humidity at the hour nearest the day's peak
    /// temperature (the afternoon high), which is the RH that physically
    /// co-occurs with `temp_max_f`. None when no hourly data covers this day
    /// (e.g. future days past the 48h hourly window, or older snapshots that
    /// predate this field). Used by `max_heat_index_n_day` so each day's high
    /// temp is paired with THAT day's humidity, never a stale post-rain "now".
    #[serde(default)]
    pub humidity_pct: Option<u32>,
    /// Forecast water depth, inches, over the provider's day period. Day zero
    /// may cover only the remaining day; it is never an observed gauge total.
    /// Missing or incomplete coverage is unknown.
    #[serde(default)]
    pub precip_sum_in: Option<f64>,
    /// Max precipitation probability for the day, percent. `None` when the
    /// provider reports no probability series (custom HTTP/MQTT forecast
    /// mappings, provider gaps): the old bare 0 was ambiguous between "dry
    /// day" and "not reported", which zeroed the probability-weighted rain
    /// rollups and read as "certainly dry" on the HA sensor. A reported 0
    /// stays `Some(0)`. `#[serde(default)]` so persisted caches deserialize.
    #[serde(default)]
    pub precip_probability_max: Option<u32>,
    /// Reported peak sustained wind, mph. None means insufficient provider
    /// evidence; a measured/modelled calm is Some(0.0).
    #[serde(default)]
    pub wind_max_mph: Option<f64>,
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
    /// Positive legacy values are evidence. This flag also certifies a
    /// reported zero; absent/invalid provider data leaves it false.
    #[serde(default)]
    pub et0_reported: bool,
    /// Daily total shortwave radiation, MJ/m2. 0.0 when the provider
    /// does not report it, which is what keeps Penman-Monteith from
    /// being selected on a source that cannot feed it.
    #[serde(default)]
    pub solar_rad_mj_m2_day: f64,
    /// Daily mean wind at 2 m, m/s. A peak/gust is never a substitute.
    #[serde(default)]
    pub wind_mean_2m_ms: Option<f64>,
    /// Daily mean RH, distinct from RH at the afternoon temperature peak.
    #[serde(default)]
    pub humidity_mean_pct: Option<f64>,
}

impl DailyEntry {
    pub fn reference_et0_mm(&self) -> Option<f64> {
        reported_et0_mm(self.et0_in, self.et0_reported)
    }

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

/// Preserve old positive-valued caches while making a new reported zero real.
fn reported_et0_mm(inches: f64, reported: bool) -> Option<f64> {
    (inches.is_finite() && inches >= 0.0 && (reported || inches > 0.0))
        .then(|| crate::units::in_to_mm(inches))
}

impl HourlyEntry {
    pub fn reference_et0_mm(&self) -> Option<f64> {
        reported_et0_mm(self.et0_in, self.et0_reported)
    }
}

/// One hour in the 48-hour rolling forecast.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HourlyEntry {
    pub time_epoch: i64,
    pub weather_code: u32,
    /// Reported air temperature, °F. Missing or invalid provider values
    /// remain unknown, including gaps within an otherwise populated series.
    #[serde(default)]
    pub temp_f: Option<f64>,
    pub apparent_temp_f: f64,
    /// Forecast depth over this hour, inches; None for an uncovered interval.
    #[serde(default)]
    pub precip_in: Option<f64>,
    /// Precipitation probability for the hour, percent. `None` = provider
    /// reports no probability (see `DailyEntry::precip_probability_max`).
    #[serde(default)]
    pub precip_probability: Option<u32>,
    #[serde(default)]
    pub wind_mph: Option<f64>,
    pub wind_dir_deg: u32,
    #[serde(default)]
    pub humidity_pct: Option<u32>,
    pub cloud_cover_pct: u32,
    // ---- Extended variables (2026-07, Open-Meteo only; serde defaults =
    // "unknown" for other providers and pre-upgrade persisted caches). ----
    /// FAO-56 reference ET for this hour, inches. Summing the hours since
    /// local midnight gives "ET spent so far today", which the water
    /// balance card uses instead of charging the whole day's ET up front.
    #[serde(default)]
    pub et0_in: f64,
    /// Positive legacy values are evidence. This flag also certifies a
    /// reported zero; absent/invalid provider data leaves it false.
    #[serde(default)]
    pub et0_reported: bool,
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
/// Re-exported from the engine, which owns the meaning.
pub use crate::engine::WET_DAY_IN as SIGNIFICANT_RAIN_IN;

/// Shared trust horizon for forecast decisions and automatic planning.
pub const FORECAST_MAX_AGE_S: i64 = 6 * 3600;

/// An absent, future-dated or over-age refresh cannot certify current evidence.
pub fn forecast_is_stale(last_refresh_epoch: i64, now_epoch: i64) -> bool {
    last_refresh_epoch <= 0
        || last_refresh_epoch > now_epoch
        || now_epoch.saturating_sub(last_refresh_epoch) > FORECAST_MAX_AGE_S
}

impl ForecastSnapshot {
    /// A current calendar view of cached provider rows. Provider refresh time
    /// and historical evidence stay intact; yesterday never becomes today's
    /// first row after midnight. Gaps carry unknown values, not another day's
    /// weather. The bounded horizon also prevents malformed far-future labels
    /// from allocating an unbounded series.
    pub fn for_day(&self, cal: Calendar, today: CivilDay) -> Self {
        let mut view = self.clone();
        let last_offset = self
            .daily
            .iter()
            .filter_map(|d| cal.day_of(d.day_marker))
            .map(|day| today.days_until(day))
            .filter(|offset| *offset >= 0)
            .max();
        view.daily.clear();
        let Some(last_offset) = last_offset else {
            return view;
        };
        let aligned = self.aligned(cal, today);
        let mut day = today;
        for _ in 0..=last_offset.min(15) {
            let Some((start, _)) = cal.day_bounds_utc(day) else {
                break;
            };
            view.daily
                .push(aligned.day(day).cloned().unwrap_or_else(|| DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(start),
                    weather_code: u32::MAX,
                    ..Default::default()
                }));
            let Some(next) = day.succ() else {
                break;
            };
            day = next;
        }
        view
    }

    /// Automatic sizing requires both fresh provenance and complete QPF coverage.
    /// Raw display/window helpers retain known cached values with their timestamp.
    pub fn planning_precip_weighted_in(&self, n: usize, now_epoch: i64) -> Option<f64> {
        if forecast_is_stale(self.last_refresh_epoch, now_epoch) {
            None
        } else {
            self.next_n_hours_precip_weighted_in(n, now_epoch)
        }
    }

    /// A scenario uses a forecast issued by `as_of`, evaluated for a future
    /// morning. Its age is checked now, not against a date days in the future.
    /// Beyond hourly coverage, the day's QPF is a coarse scenario operand;
    /// this fallback is never used to authorize a live valve command.
    pub fn scenario_rain_in(&self, at: i64, as_of: i64, cal: Calendar) -> Option<f64> {
        if forecast_is_stale(self.last_refresh_epoch, as_of) {
            return None;
        }
        self.next_n_hours_precip_weighted_in(24, at).or_else(|| {
            let day = cal.date_of(at)?;
            let weather = self.aligned(cal, day).today()?;
            weather
                .precip_sum_in
                .map(|rain| rain * weather.precip_weight())
        })
    }

    /// Forecast depth over exactly the next `n` hours from the evaluation
    /// instant. Expired rows do not count and partial coverage is unknown.
    pub fn next_n_hours_precip_in(&self, n: usize, now_epoch: i64) -> Option<f64> {
        self.hourly_precip_over(n, now_epoch, false)
    }

    /// Probability-weighted depth with the same complete interval coverage.
    pub fn next_n_hours_precip_weighted_in(&self, n: usize, now_epoch: i64) -> Option<f64> {
        self.hourly_precip_over(n, now_epoch, true)
    }

    fn hourly_precip_over(&self, n: usize, now_epoch: i64, weighted: bool) -> Option<f64> {
        let end = now_epoch.checked_add(i64::try_from(n).ok()?.checked_mul(3600)?)?;
        super::precip::total_over(
            self.hourly.iter().map(|h| {
                let amount = h.precip_in.filter(|v| super::precip::valid_amount(*v));
                (
                    h.time_epoch,
                    h.time_epoch.saturating_add(3600),
                    amount.map(|v| v * if weighted { h.precip_weight() } else { 1.0 }),
                )
            }),
            now_epoch,
            end,
        )
    }

    /// The forecast, addressed by the day each row is ABOUT.
    ///
    /// Rows are read positionally almost everywhere: daily[0] is today,
    /// daily[1] is tomorrow. That holds right after a fetch and stops
    /// holding at local midnight, which this type's own documentation
    /// admits: daily[0] is still YESTERDAY until the next fetch. The
    /// refresh cadence normally closes that window quickly, but a
    /// provider outage re-emits the last good snapshot and the staleness
    /// flag only trips after six hours, and the pre-dawn dispatch window
    /// sits inside those six hours.
    ///
    /// So on exactly the mornings a forecast is least reliable, "today"
    /// silently means yesterday and "tomorrow" means today, and the run
    /// is sized against the wrong day's weather.
    ///
    /// 0.9.0 gave every row a `DayMarker` that resolves through the
    /// deployment calendar. This addresses rows by that, so a caller asks
    /// for a day and gets that day or nothing.
    pub fn aligned(&self, cal: Calendar, today: CivilDay) -> AlignedForecast<'_> {
        AlignedForecast {
            snap: self,
            cal,
            today,
        }
    }

    /// The `[start, end)` UTC instants of daily[0]'s OWN local day.
    ///
    /// This used to read tomorrow's day marker and call it "next local
    /// midnight", on the stated grounds that a daily row starts at 00:00
    /// local. That is Open-Meteo's contract and nobody else's: NWS stamps
    /// the 06:00 daytime period start, met.no local noon. On the default
    /// US install the "midnight" this returned was six hours late, which
    /// disabled the stale-anchor guard below for exactly the pre-dawn
    /// hours the morning dispatch runs in.
    ///
    /// Resolving the day through the deployment calendar instead is
    /// correct for every provider, because they all stamp an instant
    /// inside the day they label.
    fn today_bounds(&self, cal: Calendar) -> Option<(i64, i64)> {
        cal.day_bounds_utc(cal.day_of(self.daily.first()?.day_marker)?)
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
    pub fn eto_spent_today_mm(&self, now_epoch: i64, cal: Calendar) -> f64 {
        self.et0_spent_with_evidence(now_epoch, cal).unwrap_or(0.0)
    }

    /// Full-day model ET minus an entirely covered remaining-day curve.
    /// A reported zero is evidence; missing hours cannot become zero ET.
    pub fn et0_spent_with_evidence(&self, now_epoch: i64, cal: Calendar) -> Option<f64> {
        let total = self.daily.first()?.reference_et0_mm()?;
        let (start, end) = self.today_bounds(cal)?;
        if now_epoch < start || now_epoch >= end {
            return None;
        }
        let remaining = crate::forecast::precip::total_over(
            self.hourly.iter().map(|h| {
                (
                    h.time_epoch,
                    h.time_epoch.saturating_add(3600),
                    h.reference_et0_mm(),
                )
            }),
            now_epoch,
            end,
        )?;
        Some((total - remaining).max(0.0))
    }

    /// Provider adapters with a known wind measurement height may derive
    /// daily means from complete hourly coverage. Peak humidity remains
    /// independent for the heat-index display.
    pub fn backfill_daily_et0_means(&mut self, cal: Calendar, wind_height_m: f64) {
        for day in self.daily.iter_mut().chain(self.past_daily.iter_mut()) {
            let Some((start, end)) = cal
                .day_of(day.day_marker)
                .and_then(|d| cal.day_bounds_utc(d))
            else {
                continue;
            };
            let mean = |field: fn(&HourlyEntry) -> Option<f64>| {
                crate::forecast::precip::total_over(
                    self.hourly
                        .iter()
                        .map(|h| (h.time_epoch, h.time_epoch.saturating_add(3600), field(h))),
                    start,
                    end,
                )
                .map(|sum| sum * 3600.0 / (end - start) as f64)
            };
            day.wind_mean_2m_ms =
                mean(|h| h.wind_mph.filter(|v| v.is_finite() && *v >= 0.0)).map(|mph| {
                    crate::engine::et0::wind_to_2m(crate::units::mph_to_ms(mph), wind_height_m)
                });
            day.humidity_mean_pct = mean(|h| h.humidity_pct.filter(|v| *v <= 100).map(f64::from));
        }
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
    pub fn graft_extended_from(&mut self, donor: &ForecastSnapshot, cal: Calendar) {
        if !donor.timezone.is_empty()
            && !self.timezone.is_empty()
            && donor.timezone != self.timezone
        {
            return;
        }
        // Recent history, which several providers simply do not send.
        //
        // NWS ships past_daily empty, and nothing repaired it, so
        // days_since_significant_rain fell through its loop and returned
        // len() + 1, which for an empty vector is 1. On every default US
        // install the engine therefore believed it had rained yesterday,
        // every single day, forever. The dry-stretch logic could not fire
        // and the drought counter was a constant.
        //
        // Only filled when the owner has none: a provider that sends its
        // own history keeps it.
        if self.past_daily.is_empty() && !donor.past_daily.is_empty() {
            self.past_daily = donor.past_daily.clone();
        }
        for h in &mut self.hourly {
            let Some(dh) = donor.hourly.iter().find(|d| d.time_epoch == h.time_epoch) else {
                continue;
            };
            if h.reference_et0_mm().is_none() {
                h.et0_in = dh.et0_in;
                h.et0_reported = dh.et0_reported;
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
            // Match on the CIVIL DAY, which is the thing both rows are
            // actually about. The previous matcher compared raw stamps with
            // 3h and 30h slack windows, constants that encoded one specific
            // pair of provider conventions (a 00:00-local donor against a
            // period-start owner). Two providers whose anchors sat further
            // apart than the slack allowed would mis-bind or drop the graft
            // silently. Asking the calendar which day each row labels is
            // exact for every combination, and needs no constants at all.
            let Some(target) = cal.day_of(d.day_marker) else {
                continue;
            };
            let Some(dd) = donor
                .daily
                .iter()
                .find(|x| cal.day_of(x.day_marker) == Some(target))
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
            if d.reference_et0_mm().is_none() {
                d.et0_in = dd.et0_in;
                d.et0_reported = dd.et0_reported;
            }
        }
    }

    /// Vapour pressure deficit (kPa): the current hour's value and the
    /// peak across the rest of today. (0.0, 0.0) when the provider sends
    /// no VPD.
    /// Peak vapor pressure deficit over the next `days` days, kPa.
    ///
    /// The atmosphere's drying power across the stretch a heat response
    /// is about. 0.0 when the provider sends no VPD curve, which the
    /// caller reads as "no reason to extend" rather than as calm air.
    pub fn vpd_max_over(&self, days: i64) -> f64 {
        let horizon = days.max(0) * 86_400;
        let start = self.hourly.first().map(|h| h.time_epoch).unwrap_or(0);
        self.hourly
            .iter()
            .filter(|h| start == 0 || h.time_epoch <= start + horizon)
            .map(|h| h.vpd_kpa)
            .fold(0.0_f64, f64::max)
    }

    pub fn vpd_now_and_max_today(&self, cal: Calendar) -> (f64, f64) {
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
        let midnight = self
            .today_bounds(cal)
            .map(|(_, end)| end)
            .unwrap_or(i64::MAX);
        let max_today = self
            .hourly
            .iter()
            .filter(|h| h.time_epoch < midnight)
            .map(|h| h.vpd_kpa)
            .fold(0.0_f64, f64::max);
        (now, max_today)
    }

    /// Probability-weighted rain from tomorrow through up to `n` available
    /// future days. The advertised daily horizon caps the request; every day
    /// inside that horizon must exist and carry a valid amount.
    pub fn future_n_day_weighted_precip_in(
        &self,
        n: usize,
        cal: Calendar,
        today: CivilDay,
    ) -> Option<f64> {
        self.future_precip(n, cal, today, true)
    }

    pub fn future_n_day_precip_in(&self, n: usize, cal: Calendar, today: CivilDay) -> Option<f64> {
        self.future_precip(n, cal, today, false)
    }

    fn future_precip(
        &self,
        n: usize,
        cal: Calendar,
        today: CivilDay,
        weighted: bool,
    ) -> Option<f64> {
        if n == 0 {
            return Some(0.0);
        }
        let last = self
            .daily
            .iter()
            .filter_map(|d| cal.day_of(d.day_marker))
            .max()?;
        let mut day = today.succ()?;
        if day > last {
            return None;
        }
        let mut total = 0.0;
        for _ in 0..n {
            if day > last {
                break;
            }
            let mut matching = self
                .daily
                .iter()
                .filter(|d| cal.day_of(d.day_marker) == Some(day));
            let row = matching.next()?;
            if matching.next().is_some() {
                return None;
            }
            total += row
                .precip_sum_in
                .filter(|v| super::precip::valid_amount(*v))?
                * if weighted { row.precip_weight() } else { 1.0 };
            day = day.succ()?;
        }
        total.is_finite().then_some(total)
    }

    /// Optional MODEL ARCHIVE rain over up to `n` recent archived rows.
    /// No archive or an unknown included amount is not evidence of dry days.
    pub fn past_n_day_precip_in(&self, n: usize) -> Option<f64> {
        if n == 0 {
            return Some(0.0);
        }
        if self.past_daily.is_empty() {
            return None;
        }
        self.past_daily
            .iter()
            .rev()
            .take(n)
            .try_fold(0.0, |sum, d| {
                let total = sum
                    + d.precip_sum_in
                        .filter(|v| super::precip::valid_amount(*v))?;
                total.is_finite().then_some(total)
            })
    }

    /// Minimum hourly forecast temperature over the next 24 hours.
    /// Returns None when the hourly window is empty (caller falls back
    /// to a sensible default).
    pub fn min_temp_next_24h_f(&self) -> Option<f64> {
        self.hourly
            .iter()
            .take(24)
            .filter_map(|h| h.temp_f.filter(|t| t.is_finite()))
            .fold(None, |acc, t| Some(acc.map_or(t, |a: f64| a.min(t))))
    }

    /// Maximum daily forecast temperature over today + next 2 days.
    pub fn max_temp_next_3d_f(&self) -> Option<f64> {
        self.daily
            .iter()
            .take(3)
            .filter_map(|d| d.temp_max_f.filter(|t| t.is_finite()))
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
    /// None when no daily entry carries both temperature and humidity.
    ///
    /// Days with no derived humidity (`humidity_pct == None`, e.g. future days
    /// past the 48h hourly window) are skipped so a hot day with a missing-data
    /// RH can't masquerade as a low (and so wrong) feels-like. A reported
    /// 0% RH remains a valid input.
    ///
    /// ssr-only: depends on the engine's `heat_index_f`, which lives behind the
    /// `ssr` feature. The browser never computes this (it reads the already-
    /// computed `SkipCheck.heat_index_max_3day_f` off the snapshot).
    #[cfg(feature = "ssr")]
    pub fn max_heat_index_n_day(&self, n: usize) -> Option<f64> {
        self.daily
            .iter()
            .take(n)
            .filter_map(|d| {
                let temp = d.temp_max_f.filter(|t| t.is_finite())?;
                let humidity = d.humidity_pct.filter(|rh| *rh <= 100)?;
                Some(crate::engine::skip_rules::heat_index_f(
                    temp,
                    f64::from(humidity),
                ))
            })
            .filter(|hi| hi.is_finite())
            .reduce(f64::max)
    }

    /// Fill each daily entry's `humidity_pct` from the hourly forecast: for a
    /// daily entry with no humidity from the source's own daily
    /// rollup, use the humidity at the hour within that day whose temperature
    /// is closest to the day's `temp_max_f`. That is the RH that physically
    /// co-occurs with the afternoon high, which is what `max_heat_index_n_day`
    /// needs to avoid pairing the day's peak temp with a saturated post-rain
    /// "now". A daily entry already carrying humidity (a source that reports a
    /// daily RH directly, including 0%) is left untouched. Idempotent. Producers
    /// (Open-Meteo + the alternate sources) call this after building both arrays so every
    /// forecast source feeds the engine the same physically-valid pairing.
    pub fn backfill_daily_humidity(&mut self, cal: Calendar) {
        if self.hourly.is_empty() {
            return;
        }
        for d in self.daily.iter_mut() {
            if d.humidity_pct.is_some_and(|rh| rh <= 100) {
                continue;
            }
            // The day's real bounds, not marker + 24h. On a provider that
            // stamps 06:00 the old window ran 06:00 to 06:00 and pulled the
            // next morning's humidity into this day's row.
            let Some((day_start, day_end)) = cal
                .day_of(d.day_marker)
                .and_then(|day| cal.day_bounds_utc(day))
            else {
                continue;
            };
            let Some(temp_max) = d.temp_max_f.filter(|t| t.is_finite()) else {
                continue;
            };
            if let Some(h) = self
                .hourly
                .iter()
                .filter(|h| h.time_epoch >= day_start && h.time_epoch < day_end)
                .filter_map(|h| {
                    let temp = h.temp_f.filter(|t| t.is_finite())?;
                    let humidity = h.humidity_pct.filter(|rh| *rh <= 100)?;
                    Some((humidity, temp))
                })
                .min_by(|(_, a), (_, b)| {
                    (a - temp_max)
                        .abs()
                        .partial_cmp(&(b - temp_max).abs())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            {
                d.humidity_pct = Some(h.0);
            }
        }
    }

    /// Today's forecast peak wind, mph. None on empty daily.
    /// Today's forecast peak wind.
    ///
    /// Takes the calendar so "today" is the day the deployment is
    /// actually in, not whichever row happens to be first. The positional
    /// fallback covers a stored cache written before rows carried a day
    /// marker; it is the old behavior, kept only where there is nothing
    /// better to answer with.
    pub fn wind_max_today_mph_at(&self, cal: Calendar, today: CivilDay) -> Option<f64> {
        self.aligned(cal, today)
            .today()
            .or_else(|| self.daily.first())
            .and_then(|d| d.wind_max_mph.filter(|w| w.is_finite() && *w >= 0.0))
    }

    pub fn wind_max_today_mph(&self) -> Option<f64> {
        self.daily
            .first()
            .and_then(|d| d.wind_max_mph.filter(|w| w.is_finite() && *w >= 0.0))
    }

    /// Forecast minimum temperature, F, across the hours that overlap
    /// `[start, end)`. Every overlapping hour must have a finite reading,
    /// and their intervals must cover the whole requested window. Missing
    /// rows or temperatures cannot establish a safe minimum for a run.
    pub fn min_temp_over(&self, start: i64, end: i64) -> Option<f64> {
        self.hourly_extreme_over(start, end, |h| h.temp_f.filter(|t| t.is_finite()), f64::min)
    }

    /// Shared coverage proof for critical hourly weather. A reduction may
    /// not silently skip missing readings or bridge an absent interval.
    fn hourly_extreme_over(
        &self,
        start: i64,
        end: i64,
        reading: impl Fn(&HourlyEntry) -> Option<f64>,
        combine: fn(f64, f64) -> f64,
    ) -> Option<f64> {
        if end <= start {
            return None;
        }
        let mut hours: Vec<_> = self
            .hourly
            .iter()
            .filter(|h| h.time_epoch < end && h.time_epoch.saturating_add(3600) > start)
            .collect();
        hours.sort_unstable_by_key(|h| h.time_epoch);
        let mut covered_until = start;
        let mut extreme: Option<f64> = None;
        for h in hours {
            if h.time_epoch > covered_until {
                return None;
            }
            let value = reading(h)?;
            extreme = Some(extreme.map_or(value, |known| combine(known, value)));
            covered_until = covered_until.max(h.time_epoch.saturating_add(3600));
        }
        (covered_until >= end).then_some(extreme).flatten()
    }

    /// Modeled 6 cm soil temperature, F, averaged over the day around
    /// `now` (the 24 hours behind and the 24 ahead that the series
    /// holds). Rows left at the 0.0 placeholder by a provider that does
    /// not model soil are skipped; `None` when nothing is decorated.
    ///
    /// A day's mean rather than an instant, because dormancy is a state
    /// the turf settles into, not a reading that flickers with the hour.
    pub fn soil_temp_6cm_mean_f(&self, now: i64) -> Option<f64> {
        let (n, sum) = self
            .hourly
            .iter()
            .filter(|h| h.time_epoch >= now - 86_400 && h.time_epoch < now + 86_400)
            .map(|h| h.soil_temp_6cm_f)
            .filter(|t| *t != 0.0)
            .fold((0usize, 0.0f64), |(n, sum), t| (n + 1, sum + t));
        (n > 0).then(|| sum / n as f64)
    }

    /// Forecast temperature, F, for the hour containing `epoch`.
    pub fn temp_at(&self, epoch: i64) -> Option<f64> {
        self.hourly
            .iter()
            .find(|h| h.time_epoch <= epoch && epoch < h.time_epoch.saturating_add(3600))
            .and_then(|h| h.temp_f.filter(|t| t.is_finite()))
    }

    /// Peak sustained wind over the whole run window `[start, end)`.
    /// An hourly row at T covers `[T, T + 3600)`. Every overlapping row
    /// must carry finite nonnegative wind and together cover the window;
    /// missing evidence is None, while complete calm evidence is Some(0).
    pub fn wind_max_over_window_mph(&self, start: i64, end: i64) -> Option<f64> {
        self.hourly_extreme_over(
            start,
            end,
            |h| h.wind_mph.filter(|w| w.is_finite() && *w >= 0.0),
            f64::max,
        )
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
    pub fn tomorrow_precip_with_prob_in(&self) -> (Option<f64>, Option<u32>) {
        self.daily
            .get(1)
            .map(|d| {
                (
                    d.precip_sum_in.filter(|v| super::precip::valid_amount(*v)),
                    d.precip_probability_max,
                )
            })
            .unwrap_or((None, None))
    }

    /// Tomorrow's rain, addressed by day.
    ///
    /// The positional form reads daily[1], which on a stale snapshot is
    /// TODAY. That feeds the tomorrow-rain skip gate, so a run gets held
    /// for rain that is already falling or already past.
    ///
    /// Returns `None` when no row covers tomorrow, which a caller must
    /// handle rather than receive a confident zero.
    /// Today's modelled rain, in inches, addressed by the deployment's
    /// calendar. `None` when no row covers today. This is the native
    /// forecast total for the provider's day period (potentially the remaining
    /// day). This is model evidence, separate from measured rain already fallen.
    pub fn today_precip_in_at(&self, cal: Calendar, today: CivilDay) -> Option<f64> {
        self.aligned(cal, today)
            .today()
            .and_then(|d| d.precip_sum_in.filter(|v| super::precip::valid_amount(*v)))
    }

    /// Today's modelled rain by position (daily[0]), for callers with no
    /// calendar in hand. None with no known amount.
    pub fn today_precip_in(&self) -> Option<f64> {
        self.daily
            .first()
            .and_then(|d| d.precip_sum_in.filter(|v| super::precip::valid_amount(*v)))
    }

    pub fn tomorrow_precip_with_prob_in_at(
        &self,
        cal: Calendar,
        today: CivilDay,
    ) -> Option<(f64, Option<u32>)> {
        self.aligned(cal, today).tomorrow().and_then(|d| {
            d.precip_sum_in
                .filter(|v| super::precip::valid_amount(*v))
                .map(|amount| (amount, d.precip_probability_max))
        })
    }

    /// Days since the last day with significant rain (≥ 0.05"). Walks
    /// `past_daily` newest-first, then folds in today's accumulated
    /// rain via `today_rain_in`. Returns:
    ///   0 , already wet today,
    ///   1 , yesterday was wet but today isn't yet,
    ///   N , N consecutive past days dry, today dry,
    ///   past_daily.len() + 1 (saturating) when no past day was wet.
    ///
    /// Note the empty-history case: with no past days at all this returns
    /// 1, which reads as "it rained yesterday". That is deliberately the
    /// conservative direction, because it suppresses the dry-stretch
    /// boost rather than inventing one, but it is a claim made without
    /// evidence and the real fix is to HAVE history. `graft_extended_from`
    /// now supplies it from a donor when the owner sends none.
    pub fn days_since_significant_rain(&self, today_rain_in: f64) -> u32 {
        self.days_since_rain_over(today_rain_in, SIGNIFICANT_RAIN_IN)
    }

    /// Days since a day carrying at least `wet_in` inches of rain.
    ///
    /// Takes the threshold because the operator can change what counts
    /// as wet, and a drought counter using a different number from the
    /// gate that skipped this morning is two opinions about one sky.
    pub fn days_since_rain_over(&self, today_rain_in: f64, wet_in: f64) -> u32 {
        if today_rain_in >= wet_in {
            return 0;
        }
        // past_daily is stored earliest→latest; iterate latest→earliest.
        for (i, d) in self.past_daily.iter().rev().enumerate() {
            // Unknown archive cannot establish additional dry days.
            if d.precip_sum_in
                .is_none_or(|rain| !super::precip::valid_amount(rain) || rain >= wet_in)
            {
                return (i + 1) as u32;
            }
        }
        // No wet day in the past window. Saturate at window + 1.
        (self.past_daily.len() as u32).saturating_add(1)
    }
}

/// A forecast addressed by civil day rather than by position.
///
/// Borrowed, cheap, and deliberately offering no way to reach a row by
/// index: every accessor names the day it wants.
pub struct AlignedForecast<'a> {
    snap: &'a ForecastSnapshot,
    cal: Calendar,
    today: CivilDay,
}

impl<'a> AlignedForecast<'a> {
    /// The row for `day`, or `None` when the forecast does not cover it.
    ///
    /// `None` is a real answer and callers must handle it. It is what a
    /// stale snapshot looks like once you stop assuming position, and
    /// silently using the wrong row is the failure this replaces.
    pub fn day(&self, day: CivilDay) -> Option<&'a DailyEntry> {
        self.snap
            .daily
            .iter()
            .find(|d| self.cal.day_of(d.day_marker) == Some(day))
    }

    /// Today's row, by the deployment's calendar rather than by index.
    pub fn today(&self) -> Option<&'a DailyEntry> {
        self.day(self.today)
    }

    pub fn tomorrow(&self) -> Option<&'a DailyEntry> {
        self.day(self.today.succ()?)
    }

    /// `n` days out from today, forward only.
    pub fn ahead(&self, n: u16) -> Option<&'a DailyEntry> {
        let mut d = self.today;
        for _ in 0..n {
            d = d.succ()?;
        }
        self.day(d)
    }

    /// True when the snapshot has no row for today, which is what a
    /// stale forecast looks like once position is not assumed.
    pub fn is_stale_for_today(&self) -> bool {
        self.today().is_none()
    }

    /// How many whole days the first row is behind today. 0 when aligned.
    ///
    /// The number the old positional reads needed and never had.
    pub fn days_behind(&self) -> Option<i64> {
        let first = self.snap.daily.first()?;
        let first_day = self.cal.day_of(first.day_marker)?;
        Some(first_day.days_until(self.today))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_view_rolls_over_without_a_provider_refresh() {
        for offset in [-4 * 3600, 13 * 3600 + 45 * 60, 5 * 3600 + 30 * 60] {
            let cal = Calendar::fixed_offset(offset).unwrap();
            let today = CivilDay::from_naive(chrono::NaiveDate::from_ymd_opt(2026, 9, 11).unwrap());
            let yesterday = today.pred().unwrap();
            let row = |day, high| DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(
                    cal.day_bounds_utc(day).unwrap().0 + 18 * 3600,
                ),
                temp_max_f: Some(high),
                ..Default::default()
            };
            let fc = ForecastSnapshot {
                last_refresh_epoch: cal.day_bounds_utc(yesterday).unwrap().0 + 20 * 3600,
                source_reachable: false,
                daily: vec![
                    row(yesterday, 10.0),
                    row(today, 93.0),
                    row(today.succ().unwrap(), 92.0),
                ],
                ..Default::default()
            };
            let view = fc.for_day(cal, today);
            assert_eq!(view.daily.len(), 2);
            assert_eq!(view.daily[0].temp_max_f, Some(93.0));
            assert_eq!(cal.day_of(view.daily[0].day_marker), Some(today));
            assert_eq!(view.last_refresh_epoch, fc.last_refresh_epoch);
            assert!(!view.source_reachable);
            assert_eq!(fc.daily.len(), 3, "raw provider evidence remains unchanged");
            assert!(fc
                .for_day(cal, today.succ().unwrap().succ().unwrap())
                .daily
                .is_empty());
        }
    }

    #[test]
    fn calendar_view_preserves_missing_today_as_unknown() {
        let cal = Calendar::utc();
        let today = cal.date_of(1_789_084_800).unwrap();
        let tomorrow = today.succ().unwrap();
        let fc = ForecastSnapshot {
            daily: vec![DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(
                    cal.day_bounds_utc(tomorrow).unwrap().0,
                ),
                temp_max_f: Some(90.0),
                precip_sum_in: Some(0.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        let view = fc.for_day(cal, today);
        assert_eq!(view.daily.len(), 2);
        assert_eq!(view.daily[0].temp_max_f, None);
        assert_eq!(view.daily[0].temp_min_f, None);
        assert_eq!(view.daily[0].precip_sum_in, None);
        assert_eq!(view.daily[0].wind_max_mph, None);
        assert_eq!(view.daily[0].weather_code, u32::MAX);
        assert_eq!(view.daily[1].precip_sum_in, Some(0.0));
    }

    fn past(precip: &[f64]) -> ForecastSnapshot {
        ForecastSnapshot {
            // past_daily is stored earliest→latest.
            past_daily: precip
                .iter()
                .map(|&p| DailyEntry {
                    precip_sum_in: Some(p),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn day(temp_max_f: f64, humidity_pct: u32) -> DailyEntry {
        DailyEntry {
            temp_max_f: Some(temp_max_f),
            humidity_pct: Some(humidity_pct),
            ..Default::default()
        }
    }

    #[test]
    fn temperature_window_requires_complete_finite_evidence() {
        let hour = |at, temp| HourlyEntry {
            time_epoch: at,
            temp_f: temp,
            ..Default::default()
        };
        let mut forecast = ForecastSnapshot {
            // Ordering must not change the physical coverage proof.
            hourly: vec![
                hour(7200, Some(45.0)),
                hour(0, Some(0.0)),
                hour(3600, Some(42.0)),
            ],
            ..Default::default()
        };
        assert_eq!(forecast.min_temp_over(1200, 8400), Some(0.0));
        assert_eq!(forecast.min_temp_over(-1, 7200), None, "uncovered start");
        assert_eq!(forecast.min_temp_over(0, 10801), None, "uncovered end");
        assert_eq!(forecast.min_temp_over(0, 0), None);
        for invalid in [None, Some(f64::NAN), Some(f64::INFINITY)] {
            forecast.hourly[2].temp_f = invalid;
            assert_eq!(
                forecast.min_temp_over(1200, 8400),
                None,
                "missing middle hour cannot be skipped"
            );
            assert_eq!(forecast.temp_at(4000), None);
        }
        forecast.hourly.remove(2);
        assert_eq!(
            forecast.min_temp_over(1200, 8400),
            None,
            "absent row cannot be bridged"
        );
        assert_eq!(forecast.min_temp_over(0, 3600), Some(0.0));
    }

    #[test]
    fn wind_window_needs_complete_evidence_and_retains_real_calm() {
        let mut forecast = ForecastSnapshot {
            hourly: vec![0, 3600, 7200]
                .into_iter()
                .map(|at| HourlyEntry {
                    time_epoch: at,
                    wind_mph: Some(0.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(forecast.wind_max_over_window_mph(1200, 8400), Some(0.0));
        assert_eq!(forecast.wind_max_over_window_mph(-1, 8400), None);
        assert_eq!(forecast.wind_max_over_window_mph(1200, 10801), None);
        assert_eq!(forecast.wind_max_over_window_mph(1200, 1200), None);
        forecast.hourly[1].wind_mph = Some(28.0);
        assert_eq!(forecast.wind_max_over_window_mph(1200, 8400), Some(28.0));
        for invalid in [None, Some(f64::NAN), Some(f64::INFINITY), Some(-1.0)] {
            forecast.hourly[1].wind_mph = invalid;
            assert_eq!(forecast.wind_max_over_window_mph(1200, 8400), None);
        }
        forecast.hourly.remove(1);
        assert_eq!(
            forecast.wind_max_over_window_mph(1200, 8400),
            None,
            "absent hour is not calm"
        );
        assert_eq!(forecast.wind_max_over_window_mph(0, 3600), Some(0.0));
    }

    #[test]
    fn humidity_backfill_uses_valid_same_day_evidence_and_preserves_zero() {
        let day_epoch = 1_749_945_600;
        let mut forecast = ForecastSnapshot {
            daily: vec![DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(day_epoch),
                temp_max_f: Some(90.0),
                ..Default::default()
            }],
            hourly: vec![HourlyEntry {
                time_epoch: day_epoch + 12 * 3600,
                temp_f: Some(90.0),
                humidity_pct: None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let cal = Calendar::utc();
        forecast.backfill_daily_humidity(cal);
        assert_eq!(forecast.daily[0].humidity_pct, None);
        forecast.hourly[0].humidity_pct = Some(101);
        forecast.backfill_daily_humidity(cal);
        assert_eq!(forecast.daily[0].humidity_pct, None);
        forecast.hourly[0].humidity_pct = Some(0);
        forecast.backfill_daily_humidity(cal);
        assert_eq!(forecast.daily[0].humidity_pct, Some(0));
        forecast.hourly[0].humidity_pct = Some(90);
        forecast.backfill_daily_humidity(cal);
        assert_eq!(
            forecast.daily[0].humidity_pct,
            Some(0),
            "zero is not a request to overwrite"
        );
        forecast.daily[0].humidity_pct = None;
        forecast.hourly[0].time_epoch += 86400;
        forecast.backfill_daily_humidity(cal);
        assert_eq!(
            forecast.daily[0].humidity_pct, None,
            "tomorrow cannot fill today"
        );
    }

    #[test]
    fn missing_daily_high_never_becomes_zero_or_a_heat_index_input() {
        let mut forecast = ForecastSnapshot {
            daily: vec![DailyEntry {
                humidity_pct: Some(99),
                temp_min_f: Some(75.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(forecast.max_temp_next_3d_f(), None);
        assert_eq!(forecast.max_heat_index_n_day(3), None);
        forecast.daily.push(day(-5.0, 20));
        assert_eq!(
            forecast.max_temp_next_3d_f(),
            Some(-5.0),
            "unknown high cannot dominate a real subzero high"
        );
        forecast.daily.push(day(0.0, 20));
        assert_eq!(forecast.max_temp_next_3d_f(), Some(0.0));
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
        let hi = fc.max_heat_index_n_day(3).unwrap();
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
        let hi = fc.max_heat_index_n_day(3).unwrap();
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
        assert_eq!(ForecastSnapshot::default().max_heat_index_n_day(3), None);

        // A day with humidity_pct == None (no hourly coverage) is skipped, so a hot
        // day with missing humidity can't masquerade as a low feels-like.
        let only_missing = ForecastSnapshot {
            daily: vec![DailyEntry {
                temp_max_f: Some(100.0),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(only_missing.max_heat_index_n_day(3), None);

        // Reported dry air still has a heat-index value. It must not be
        // erased by the previous humidity==0 missing-data convention.
        let real_zero = ForecastSnapshot {
            daily: vec![day(100.0, 0)],
            ..Default::default()
        };
        assert_eq!(
            real_zero.max_heat_index_n_day(3),
            Some(crate::engine::skip_rules::heat_index_f(100.0, 0.0))
        );
        let invalid_rh = ForecastSnapshot {
            daily: vec![day(100.0, 101)],
            ..Default::default()
        };
        assert_eq!(invalid_rh.max_heat_index_n_day(3), None);

        // n caps the window: a hot day past `n` doesn't count.
        let fc = ForecastSnapshot {
            daily: vec![day(85.0, 60), day(88.0, 60), day(110.0, 60)],
            ..Default::default()
        };
        let two = fc.max_heat_index_n_day(2).unwrap();
        let three = fc.max_heat_index_n_day(3).unwrap();
        assert!(three > two, "the 110°F day only counts within n=3");
    }

    #[test]
    fn weighted_rollup_takes_probability_less_days_at_full_value() {
        // daily[0] is today (skipped); daily[1..] carry: a 60%-prob day, a
        // provider-gap day (no probability), and a reported-0% day.
        let mut fc = ForecastSnapshot {
            daily: vec![
                DailyEntry::default(),
                DailyEntry {
                    precip_sum_in: Some(1.0),
                    precip_probability_max: Some(60),
                    ..Default::default()
                },
                DailyEntry {
                    precip_sum_in: Some(0.5),
                    precip_probability_max: None,
                    ..Default::default()
                },
                DailyEntry {
                    precip_sum_in: Some(2.0),
                    precip_probability_max: Some(0),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        // 1.0*0.6 + 0.5*1.0 (unknown = certain, the safe skip direction)
        // + 2.0*0.0 (a REPORTED zero still zeroes).
        let cal = Calendar::utc();
        let epoch = 1_788_480_000;
        for (i, row) in fc.daily.iter_mut().enumerate() {
            row.day_marker =
                crate::engine::clock::DayMarker::inside_local_day(epoch + i as i64 * 86400);
        }
        let got = fc
            .future_n_day_weighted_precip_in(3, cal, cal.date_of(epoch).unwrap())
            .unwrap();
        assert!((got - 1.1).abs() < 1e-9, "weighted = {got}");

        // tomorrow_precip_with_prob_in carries the probability as an Option.
        let (amt, prob) = fc.tomorrow_precip_with_prob_in();
        assert!((amt.unwrap() - 1.0).abs() < 1e-9);
        assert_eq!(prob, Some(60));
        let (_, prob) = ForecastSnapshot::default().tomorrow_precip_with_prob_in();
        assert_eq!(prob, None, "no tomorrow entry = no probability claim");
    }

    #[test]
    fn past_n_day_precip_sums_most_recent_entries() {
        // earliest→latest: [0.10, 0.20, 1.50] (1.50" yesterday).
        let fc = past(&[0.10, 0.20, 1.50]);
        // n=0 includes no past days.
        assert!((fc.past_n_day_precip_in(0).unwrap() - 0.0).abs() < 1e-9);
        // n=1 is yesterday only (the last entry).
        assert!((fc.past_n_day_precip_in(1).unwrap() - 1.50).abs() < 1e-9);
        // n=2 is yesterday + the day before.
        assert!((fc.past_n_day_precip_in(2).unwrap() - 1.70).abs() < 1e-9);
        // n beyond the window saturates at the full sum.
        assert!((fc.past_n_day_precip_in(9).unwrap() - 1.80).abs() < 1e-9);
        // Empty past window is optional missing evidence.
        assert_eq!(ForecastSnapshot::default().past_n_day_precip_in(3), None);
    }

    // ---- eto_spent_today_mm (subtraction + stale-anchor guard) ----

    /// Snapshot as fetched midday: daily[0] at local-midnight `day0` carrying a
    /// 0.18 in full-day ET0, tomorrow's entry supplying the next-midnight
    /// boundary, and two remaining evening hours of 0.04 in each on the curve.
    fn et_fc(day0: i64) -> ForecastSnapshot {
        ForecastSnapshot {
            daily: vec![
                DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(day0),
                    et0_in: 0.18,
                    ..Default::default()
                },
                DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(day0 + 86_400),
                    et0_in: 0.17,
                    ..Default::default()
                },
            ],
            hourly: (18..24)
                .map(|hour| HourlyEntry {
                    time_epoch: day0 + hour * 3600,
                    et0_in: if hour == 20 || hour == 21 { 0.04 } else { 0.0 },
                    et0_reported: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn eto_spent_midday_subtracts_the_remaining_hours() {
        // A real UTC midnight. The old value was not day-aligned, so it
        // only ever worked because the code read the marker as an instant.
        let day0 = 1_749_945_600;
        let fc = et_fc(day0);
        // 18:00: (0.18 - 0.08 remaining) * 25.4 = 2.54 mm spent so far.
        let spent =
            fc.eto_spent_today_mm(day0 + 18 * 3600, crate::engine::calendar::Calendar::utc());
        assert!((spent - 2.54).abs() < 1e-9, "spent = {spent}");
    }

    #[test]
    fn eto_spent_is_zero_on_a_stale_pre_rollover_snapshot() {
        // A real UTC midnight. The old value was not day-aligned, so it
        // only ever worked because the code read the marker as an instant.
        let day0 = 1_749_945_600;
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
        let spent = fc.eto_spent_today_mm(
            day0 + 86_400 + 1800,
            crate::engine::calendar::Calendar::utc(),
        );
        assert!((spent - 0.0).abs() < 1e-9, "stale anchor spends 0: {spent}");
    }

    #[test]
    fn eto_spent_resumes_on_a_fresh_post_rollover_snapshot() {
        // The next fetch re-anchors daily[0] to the new day; midday math works
        // exactly as before the rollover.
        let day0 = 1_749_945_600 + 86_400;
        let fc = et_fc(day0);
        let spent =
            fc.eto_spent_today_mm(day0 + 18 * 3600, crate::engine::calendar::Calendar::utc());
        assert!((spent - 2.54).abs() < 1e-9, "spent = {spent}");
    }

    /// The drought counter was a constant on every default US install.
    ///
    /// NWS sends no past_daily at all. The counter walks that vector,
    /// finds nothing, and returns len() + 1, which for an empty vector is
    /// 1: "it rained yesterday". Every day. Forever. The graft carries
    /// the donor's history across so the counter has something to count.
    #[test]
    fn the_graft_supplies_history_a_provider_does_not_send() {
        let mut owner = ForecastSnapshot {
            timezone: "America/New_York".into(),
            past_daily: vec![],
            ..Default::default()
        };
        // Without history the counter claims yesterday was wet.
        assert_eq!(owner.days_since_significant_rain(0.0), 1);

        let donor = ForecastSnapshot {
            timezone: "America/New_York".into(),
            past_daily: vec![
                DailyEntry {
                    precip_sum_in: Some(0.80),
                    ..Default::default()
                },
                DailyEntry {
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                },
                DailyEntry {
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        owner.graft_extended_from(&donor, crate::engine::calendar::Calendar::utc());
        // Three days back was wet, so the dry stretch is three days.
        assert_eq!(owner.days_since_significant_rain(0.0), 3);
    }

    /// A provider that sends its own history keeps it.
    #[test]
    fn the_graft_does_not_overwrite_history_the_owner_has() {
        let mut owner = ForecastSnapshot {
            timezone: "America/New_York".into(),
            past_daily: vec![DailyEntry {
                precip_sum_in: Some(0.9),
                ..Default::default()
            }],
            ..Default::default()
        };
        let donor = ForecastSnapshot {
            timezone: "America/New_York".into(),
            past_daily: vec![
                DailyEntry {
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                },
                DailyEntry {
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        owner.graft_extended_from(&donor, crate::engine::calendar::Calendar::utc());
        assert_eq!(owner.past_daily.len(), 1);
        assert_eq!(owner.days_since_significant_rain(0.0), 1);
    }

    /// The stale-snapshot morning, which is where positional reads go
    /// wrong and never say so.
    ///
    /// A snapshot fetched yesterday evening still has yesterday in
    /// daily[0]. Read by position, "today" is yesterday's weather and
    /// "tomorrow" is today's, so a pre-dawn run is sized against the
    /// wrong day. Addressed by day, today is simply absent, which is a
    /// fact the caller can act on.
    #[test]
    fn a_stale_snapshot_has_no_today_rather_than_the_wrong_one() {
        let cal = crate::engine::calendar::Calendar::utc();
        // Rows for the 4th and 5th; "now" is the 6th.
        let d4 = 1_788_480_000; // 2026-09-04 00:00 UTC
        let fc = ForecastSnapshot {
            daily: vec![
                DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(d4),
                    precip_sum_in: Some(0.10),
                    ..Default::default()
                },
                DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(d4 + 86_400),
                    precip_sum_in: Some(0.20),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let today = cal.date_of(d4 + 2 * 86_400).expect("representable");
        let a = fc.aligned(cal, today);

        assert!(a.is_stale_for_today(), "no row covers today");
        assert_eq!(a.today(), None);
        assert_eq!(a.days_behind(), Some(2));
        // The positional read would have handed back 0.10 as "today" and
        // 0.20 as "tomorrow". Both belong to days that have passed.
        assert_eq!(fc.daily.first().and_then(|d| d.precip_sum_in), Some(0.10));
    }

    /// On a fresh snapshot the answers match the positional ones, so
    /// nothing moves for the normal case.
    #[test]
    fn a_fresh_snapshot_agrees_with_the_positional_reading() {
        let cal = crate::engine::calendar::Calendar::utc();
        let d0 = 1_788_480_000;
        let fc = ForecastSnapshot {
            daily: (0..3i64)
                .map(|i| DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(d0 + i * 86_400),
                    precip_sum_in: Some(i as f64 / 10.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let today = cal.date_of(d0).expect("representable");
        let a = fc.aligned(cal, today);
        assert_eq!(a.today().and_then(|d| d.precip_sum_in), Some(0.0));
        assert_eq!(a.tomorrow().and_then(|d| d.precip_sum_in), Some(0.1));
        assert_eq!(a.ahead(2).and_then(|d| d.precip_sum_in), Some(0.2));
        assert_eq!(a.ahead(9), None, "past the end is absent, not clamped");
        assert!(!a.is_stale_for_today());
        assert_eq!(a.days_behind(), Some(0));
    }

    /// An operator who raises what counts as wet moves the whole
    /// product, not one gate.
    ///
    /// "A day counts as wet" was spelled four times: a constant here,
    /// another in the tuning report, three bare literals in the seven-day
    /// strip, and the default for the operator's own already-wet knob.
    /// They agreed only because they were all 0.05. Raise the knob and
    /// the product held two opinions about whether it had rained.
    #[test]
    fn the_dry_streak_counts_by_the_threshold_it_is_given() {
        let day = |rain: f64| DailyEntry {
            precip_sum_in: Some(rain),
            ..Default::default()
        };
        // Three days back carried 0.07in: wet by the default, dry once
        // the operator decides it takes a tenth of an inch.
        let fc = ForecastSnapshot {
            past_daily: vec![day(0.07), day(0.0), day(0.0)],
            ..Default::default()
        };

        assert_eq!(
            fc.days_since_rain_over(0.0, 0.05),
            3,
            "0.07in is a wet day at a 0.05 threshold"
        );
        assert_eq!(
            fc.days_since_rain_over(0.0, 0.10),
            4,
            "and is not one at 0.10, so the streak runs past it"
        );
        // Today's own rain is judged by the same number.
        assert_eq!(fc.days_since_rain_over(0.07, 0.05), 0);
        assert_ne!(fc.days_since_rain_over(0.07, 0.10), 0);
    }

    // ---- extended-series graft (advisory backfill across providers) ----

    fn hourly_at(epoch: i64) -> HourlyEntry {
        HourlyEntry {
            time_epoch: epoch,
            temp_f: Some(80.0),
            precip_in: Some(0.1),
            ..Default::default()
        }
    }

    #[test]
    fn graft_fills_only_zeroed_extended_fields_by_exact_epoch() {
        // NWS-style owner: core fields present, extended series absent.
        let mut owner = ForecastSnapshot {
            hourly: vec![hourly_at(1000), hourly_at(4600)],
            daily: vec![DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(500),
                precip_sum_in: Some(0.4),
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
                day_marker: crate::engine::clock::DayMarker::inside_local_day(500),
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

        owner.graft_extended_from(&donor, crate::engine::calendar::Calendar::utc());
        // Epoch 1000 matched: extended fields filled, core untouched.
        assert!((owner.hourly[0].et0_in - 0.02).abs() < 1e-9);
        assert!((owner.hourly[0].vpd_kpa - 1.4).abs() < 1e-9);
        assert!((owner.hourly[0].soil_moisture_3_9_vwc - 0.19).abs() < 1e-9);
        assert!(
            (owner.hourly[0].temp_f.unwrap() - 80.0).abs() < 1e-9,
            "core stays owner's"
        );
        // Epoch 4600 has no donor match: stays zero.
        assert!((owner.hourly[1].et0_in - 0.0).abs() < 1e-9);
        // Daily grafted by epoch, core precip untouched.
        assert!((owner.daily[0].precip_hours - 5.0).abs() < 1e-9);
        assert!((owner.daily[0].cape_max_jkg - 2400.0).abs() < 1e-9);
        assert!(
            (owner.daily[0].precip_sum_in.unwrap() - 0.4).abs() < 1e-9,
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
        owner.graft_extended_from(&donor, crate::engine::calendar::Calendar::utc());
        assert!(
            (owner.hourly[0].vpd_kpa - 0.9).abs() < 1e-9,
            "own value wins"
        );
        assert!(
            (owner.hourly[0].et0_in - 0.02).abs() < 1e-9,
            "zeroed field fills"
        );
    }
    #[test]
    fn forecast_rain_windows_require_exact_forward_coverage() {
        let mut fc = ForecastSnapshot {
            hourly: (0..5)
                .map(|i| HourlyEntry {
                    time_epoch: i * 3600,
                    precip_in: Some(if i == 0 { 10.0 } else { 0.0 }),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(fc.next_n_hours_precip_in(4, 3600), Some(0.0));
        assert_eq!(
            fc.next_n_hours_precip_in(4, 3601),
            None,
            "last second is uncovered"
        );
        fc.hourly[2].precip_in = None;
        assert_eq!(fc.next_n_hours_precip_weighted_in(4, 3600), None);
        fc.hourly.remove(2);
        assert_eq!(
            fc.next_n_hours_precip_in(4, 3600),
            None,
            "missing row is not dry"
        );
    }

    #[test]
    fn future_rain_caps_at_advertised_horizon_without_bridging_missing_days() {
        let cal = Calendar::utc();
        let epoch = 1_788_480_000;
        let today = cal.date_of(epoch).unwrap();
        let mut fc = ForecastSnapshot {
            daily: (0..7)
                .map(|i| DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(
                        epoch + i * 86400,
                    ),
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        assert_eq!(fc.future_n_day_precip_in(7, cal, today), Some(0.0));
        fc.daily[0].precip_sum_in = None; // Partial current day does not erase future evidence.
        assert_eq!(fc.future_n_day_precip_in(7, cal, today), Some(0.0));
        fc.daily[3].precip_sum_in = None;
        assert_eq!(fc.future_n_day_weighted_precip_in(7, cal, today), None);
        fc.daily.remove(3);
        assert_eq!(fc.future_n_day_precip_in(7, cal, today), None);
    }

    #[test]
    fn unknown_archive_stops_dry_stretch_and_cannot_be_a_zero_total() {
        let fc = ForecastSnapshot {
            past_daily: vec![
                DailyEntry {
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                },
                DailyEntry::default(),
                DailyEntry {
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(fc.past_n_day_precip_in(3), None);
        assert_eq!(fc.days_since_rain_over(0.0, 0.05), 2);
        let raw = serde_json::to_value(DailyEntry::default()).unwrap();
        assert!(raw["precip_sum_in"].is_null());
    }
}
