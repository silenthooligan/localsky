//! The sky right now, from whatever evidence a deployment has.
//!
//! Every input is optional and arrives already judged: the caller passes a
//! reading only while it is fresh, and says whether it was observed or
//! modelled. Precipitation and storms are reported only when observed (a
//! gauge, a radar, a lightning sensor); a forecast's chance of rain is not
//! rain. Sky cover may come from an observation, measured sunlight, a model's
//! current analysis or the forecast hour, in that order of trust. When there
//! is no evidence about the sky, the answer is `Unknown`, never a guess.
//!
//! Day and night come from the sun's position at the site, never from how
//! bright it is: an overcast morning is dim, but it is still day.

use serde::{Deserialize, Serialize};

use super::sunrise::{clear_sky_ghi_w_m2, solar_elevation_deg, HORIZON_DEG};

/// Below this sun height the measured light is mostly scattered dawn or dusk
/// light, which cannot say how cloudy the sky is.
pub const LOW_SUN_DEG: f64 = 5.0;

/// A strike this recent and this close makes the sky a thunderstorm.
const THUNDER_WINDOW_S: i64 = 15 * 60;
const THUNDER_RANGE_MI: f64 = 10.0;

/// Low-visibility threshold (1 km); visibility alone does not identify fog.
const FOG_VISIBILITY_MI: f64 = 0.62;

/// Sustained wind or gusts at which the sky is also called windy.
const WINDY_AVG_MPH: f64 = 20.0;
const WINDY_GUST_MPH: f64 = 30.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkyCondition {
    Clear,
    MostlyClear,
    PartlyCloudy,
    MostlyCloudy,
    Overcast,
    Fog,
    /// Visibility is low; visibility alone cannot distinguish fog, smoke or dust.
    LowVisibility,
    LightRain,
    Rain,
    HeavyRain,
    Snow,
    WintryMix,
    Hail,
    Thunderstorm,
    /// Nothing the deployment has can say what the sky is doing.
    Unknown,
}

/// Where the sun is at the site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SunPhase {
    Day,
    Night,
    /// Above the horizon but lower than `LOW_SUN_DEG`, rising.
    Dawn,
    /// Above the horizon but lower than `LOW_SUN_DEG`, setting.
    Dusk,
}

/// What decided the sky cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkyBasis {
    /// Measured sunlight compared with clear-sky sunlight for the sun height.
    MeasuredSunlight,
    /// An observation: a sky or visibility sensor, or a nearby weather
    /// station report.
    Observation,
    /// A model's current analysis.
    Model,
    /// The forecast for the current hour.
    Forecast,
    /// No sky evidence.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkyNow {
    pub condition: SkyCondition,
    /// Where the sun is; `None` when the site location is not configured.
    pub phase: Option<SunPhase>,
    /// Daytime by the sun when the site is known; otherwise unknown.
    pub is_day: Option<bool>,
    /// The cloud cover the answer rests on, percent, when there is one.
    pub cloud_cover_pct: Option<u8>,
    pub cover_basis: SkyBasis,
    /// Precipitation is being observed now.
    pub precipitating: bool,
    pub windy: bool,
    /// The instant the evidence was evaluated, epoch seconds.
    pub at_epoch: i64,
}

/// One reading of a current value with how it was obtained.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub value: f64,
    /// Observed (a sensor or a station report) rather than modelled.
    pub observed: bool,
}

/// The forecast for the hour containing now.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ForecastHour {
    pub cloud_cover_pct: Option<f64>,
    pub weather_code: Option<u32>,
    pub visibility_mi: Option<f64>,
}

/// Evidence for `classify`. Pass a value only while it is fresh.
#[derive(Debug, Clone, Default)]
pub struct SkyInputs {
    pub now: i64,
    pub site: Option<(f64, f64)>,
    /// Irradiance from a live sensor, W/m2, averaged over a few minutes.
    pub measured_solar_w_m2: Option<f64>,
    /// An observed precipitation rate (gauge or radar), in/hr.
    pub observed_rain_in_hr: Option<f64>,
    pub observed_hail: bool,
    pub air_temp_f: Option<f64>,
    pub wind_avg_mph: Option<f64>,
    pub wind_gust_mph: Option<f64>,
    /// The most recent lightning strike: epoch and distance in miles when
    /// the detector reports one.
    pub last_strike: Option<(i64, Option<f64>)>,
    pub cloud_cover_pct: Option<Reading>,
    pub visibility_mi: Option<Reading>,
    pub forecast_hour: Option<ForecastHour>,
}

/// The sun's phase and whether it is rising at the site.
pub fn sun_phase(now: i64, site: Option<(f64, f64)>) -> Option<(SunPhase, f64)> {
    let (lat, lon) = site?;
    let elevation = solar_elevation_deg(now, lat, lon)?;
    let rising = solar_elevation_deg(now.checked_add(600)?, lat, lon)? > elevation;
    let phase = if elevation <= HORIZON_DEG {
        SunPhase::Night
    } else if elevation < LOW_SUN_DEG {
        if rising {
            SunPhase::Dawn
        } else {
            SunPhase::Dusk
        }
    } else {
        SunPhase::Day
    };
    Some((phase, elevation))
}

/// Cloud cover, percent, to a sky class (oktas-style bands).
fn cover_condition(pct: f64) -> SkyCondition {
    match pct {
        p if p < 12.5 => SkyCondition::Clear,
        p if p < 37.5 => SkyCondition::MostlyClear,
        p if p < 62.5 => SkyCondition::PartlyCloudy,
        p if p < 87.5 => SkyCondition::MostlyCloudy,
        _ => SkyCondition::Overcast,
    }
}

/// Qualitative sunlight estimate. Irradiance is not cloud fraction: shade,
/// aerosols, altitude and sensor siting can change it independently of clouds.
fn sunlight_condition(share: f64) -> SkyCondition {
    match share {
        s if s >= 0.75 => SkyCondition::Clear,
        s if s >= 0.55 => SkyCondition::MostlyClear,
        s if s >= 0.35 => SkyCondition::PartlyCloudy,
        s if s >= 0.15 => SkyCondition::MostlyCloudy,
        _ => SkyCondition::Overcast,
    }
}

/// A forecast weather code to sky cover, when the code describes the sky
/// itself. Precipitation codes describe a chance, not the current sky.
fn code_sky(code: u32) -> Option<SkyCondition> {
    match code {
        0 => Some(SkyCondition::Clear),
        1 => Some(SkyCondition::MostlyClear),
        2 => Some(SkyCondition::PartlyCloudy),
        3 => Some(SkyCondition::Overcast),
        45 | 48 => Some(SkyCondition::Fog),
        _ => None,
    }
}

fn pct(value: f64) -> u8 {
    value.round().clamp(0.0, 100.0) as u8
}

pub fn classify(i: &SkyInputs) -> SkyNow {
    // Treat corrupt or physically impossible inputs as absent, never as an
    // extreme weather event. JSON excludes NaN, but internal sensor math can
    // still produce it before serialization.
    let finite_nonnegative = |v: f64| v.is_finite() && v >= 0.0;
    let cover = |v: f64| v.is_finite() && (0.0..=100.0).contains(&v);
    let i = &SkyInputs {
        measured_solar_w_m2: i.measured_solar_w_m2.filter(|v| finite_nonnegative(*v)),
        observed_rain_in_hr: i.observed_rain_in_hr.filter(|v| finite_nonnegative(*v)),
        wind_avg_mph: i.wind_avg_mph.filter(|v| finite_nonnegative(*v)),
        wind_gust_mph: i.wind_gust_mph.filter(|v| finite_nonnegative(*v)),
        cloud_cover_pct: i.cloud_cover_pct.filter(|v| cover(v.value)),
        visibility_mi: i.visibility_mi.filter(|v| finite_nonnegative(v.value)),
        forecast_hour: i.forecast_hour.map(|f| ForecastHour {
            cloud_cover_pct: f.cloud_cover_pct.filter(|v| cover(*v)),
            visibility_mi: f.visibility_mi.filter(|v| finite_nonnegative(*v)),
            ..f
        }),
        ..i.clone()
    };
    let sun = sun_phase(i.now, i.site);
    let phase = sun.map(|(p, _)| p);
    let is_day = phase.map(|p| p != SunPhase::Night);
    let windy = i.wind_avg_mph.is_some_and(|w| w >= WINDY_AVG_MPH)
        || i.wind_gust_mph.is_some_and(|g| g >= WINDY_GUST_MPH);
    let rain = i.observed_rain_in_hr.filter(|r| *r > 0.0);
    let precipitating = rain.is_some() || i.observed_hail;

    let (sky, cloud_cover_pct, cover_basis) = sky_cover(i, sun);
    let observed = if i.last_strike.is_some_and(|(at, mi)| {
        at > 0
            && at <= i.now
            && i.now.saturating_sub(at) <= THUNDER_WINDOW_S
            && mi.is_some_and(|d| d.is_finite() && (0.0..=THUNDER_RANGE_MI).contains(&d))
    }) {
        Some(SkyCondition::Thunderstorm)
    } else if i.observed_hail {
        Some(SkyCondition::Hail)
    } else {
        // A surface thermometer cannot determine precipitation phase. Snow,
        // sleet and freezing rain require an actual type report or vertical
        // temperature profile; do not invent them from a rain rate.
        rain.map(|rate| match rate {
            r if r >= 0.30 => SkyCondition::HeavyRain,
            r if r >= 0.10 => SkyCondition::Rain,
            _ => SkyCondition::LightRain,
        })
    };

    SkyNow {
        condition: observed.unwrap_or(sky),
        phase,
        is_day,
        cloud_cover_pct,
        cover_basis,
        precipitating,
        windy,
        at_epoch: i.now,
    }
}

/// The sky cover from the most trusted evidence available.
fn sky_cover(i: &SkyInputs, sun: Option<(SunPhase, f64)>) -> (SkyCondition, Option<u8>, SkyBasis) {
    let fog = |mi: f64| mi < FOG_VISIBILITY_MI;
    // Low visibility is observed; its cause is not necessarily fog.
    if i.visibility_mi.is_some_and(|v| v.observed && fog(v.value)) {
        return (SkyCondition::LowVisibility, None, SkyBasis::Observation);
    }
    if let Some(c) = i.cloud_cover_pct.filter(|c| c.observed) {
        return (
            cover_condition(c.value),
            Some(pct(c.value)),
            SkyBasis::Observation,
        );
    }
    // Measured sunlight, while the sun is high enough for it to mean anything.
    if let (Some(w), Some((SunPhase::Day, elevation))) = (i.measured_solar_w_m2, sun) {
        let clear = clear_sky_ghi_w_m2(elevation);
        if clear > 0.0 {
            return (
                sunlight_condition(w / clear),
                None,
                SkyBasis::MeasuredSunlight,
            );
        }
    }
    let forecast = i.forecast_hour.unwrap_or_default();
    if i.visibility_mi.is_some_and(|v| fog(v.value)) {
        return (SkyCondition::LowVisibility, None, SkyBasis::Model);
    }
    if let Some(c) = i.cloud_cover_pct {
        return (
            cover_condition(c.value),
            Some(pct(c.value)),
            SkyBasis::Model,
        );
    }
    if forecast.visibility_mi.is_some_and(fog) {
        return (SkyCondition::LowVisibility, None, SkyBasis::Forecast);
    }
    if let Some(c) = forecast.cloud_cover_pct {
        return (cover_condition(c), Some(pct(c)), SkyBasis::Forecast);
    }
    if let Some(sky) = forecast.weather_code.and_then(code_sky) {
        return (sky, None, SkyBasis::Forecast);
    }
    (SkyCondition::Unknown, None, SkyBasis::None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    /// A Florida site (the demo's) on the mornings that motivated this:
    /// sunrise 07:20 EDT, sunset 19:06.
    const SITE: Option<(f64, f64)> = Some((28.54, -81.38));
    fn edt(day: u32, hour: u32, minute: u32) -> i64 {
        Utc.with_ymd_and_hms(2026, 10, day, hour + 4, minute, 0)
            .unwrap()
            .timestamp()
    }
    fn at(now: i64) -> SkyInputs {
        SkyInputs {
            now,
            site: SITE,
            ..Default::default()
        }
    }
    fn observed(value: f64) -> Option<Reading> {
        Some(Reading {
            value,
            observed: true,
        })
    }
    fn modelled(value: f64) -> Option<Reading> {
        Some(Reading {
            value,
            observed: false,
        })
    }

    #[test]
    fn an_overcast_morning_is_a_cloudy_day() {
        // 2026-10-07 08:00: the station read 4 W/m2 of about 89 clear-sky.
        let mut i = at(edt(7, 8, 0));
        i.measured_solar_w_m2 = Some(4.0);
        let sky = classify(&i);
        assert_eq!(sky.condition, SkyCondition::Overcast);
        assert_eq!(sky.phase, Some(SunPhase::Day));
        assert_eq!(sky.cover_basis, SkyBasis::MeasuredSunlight);
        // A clear low sun is clear, not cloud.
        i.measured_solar_w_m2 = Some(85.0);
        assert_eq!(classify(&i).condition, SkyCondition::Clear);
    }

    #[test]
    fn dawn_light_does_not_grade_the_sky() {
        let mut i = at(edt(6, 7, 30));
        i.measured_solar_w_m2 = Some(3.0);
        let sky = classify(&i);
        assert_eq!(sky.phase, Some(SunPhase::Dawn));
        assert_eq!(sky.condition, SkyCondition::Unknown);
        // Other cover evidence still answers at dawn.
        i.cloud_cover_pct = modelled(90.0);
        assert_eq!(classify(&i).condition, SkyCondition::Overcast);
        let dusk = classify(&at(edt(6, 19, 0)));
        assert_eq!(dusk.phase, Some(SunPhase::Dusk));
    }

    #[test]
    fn night_cover_comes_from_cloud_evidence_and_never_defaults_to_clear() {
        let night = at(edt(7, 2, 0));
        let sky = classify(&night);
        assert_eq!(sky.phase, Some(SunPhase::Night));
        assert_eq!(sky.is_day, Some(false));
        assert_eq!(sky.condition, SkyCondition::Unknown);

        let mut i = night.clone();
        i.forecast_hour = Some(ForecastHour {
            cloud_cover_pct: Some(70.0),
            ..Default::default()
        });
        let sky = classify(&i);
        assert_eq!(
            (sky.condition, sky.cover_basis),
            (SkyCondition::MostlyCloudy, SkyBasis::Forecast)
        );

        // A model's current analysis beats the forecast hour, and an
        // observation (an airport's cloud layers) beats the model.
        i.cloud_cover_pct = modelled(20.0);
        assert_eq!(classify(&i).condition, SkyCondition::MostlyClear);
        i.cloud_cover_pct = observed(100.0);
        let sky = classify(&i);
        assert_eq!(
            (sky.condition, sky.cover_basis),
            (SkyCondition::Overcast, SkyBasis::Observation)
        );
    }

    #[test]
    fn a_forecast_chance_of_storms_is_not_a_storm() {
        let mut i = at(edt(7, 14, 0));
        i.forecast_hour = Some(ForecastHour {
            weather_code: Some(95),
            cloud_cover_pct: Some(40.0),
            ..Default::default()
        });
        let sky = classify(&i);
        assert_eq!(sky.condition, SkyCondition::PartlyCloudy);
        assert!(!sky.precipitating);
        // A storm code alone says nothing about the sky.
        i.forecast_hour = Some(ForecastHour {
            weather_code: Some(95),
            ..Default::default()
        });
        assert_eq!(classify(&i).condition, SkyCondition::Unknown);
    }

    #[test]
    fn observed_weather_outranks_the_sky() {
        let mut i = at(edt(7, 14, 0));
        i.measured_solar_w_m2 = Some(900.0);
        i.observed_rain_in_hr = Some(0.05);
        assert_eq!(classify(&i).condition, SkyCondition::LightRain);
        i.observed_rain_in_hr = Some(0.5);
        assert_eq!(classify(&i).condition, SkyCondition::HeavyRain);
        i.air_temp_f = Some(31.0);
        assert_eq!(classify(&i).condition, SkyCondition::HeavyRain);
        i.air_temp_f = Some(34.0);
        assert_eq!(classify(&i).condition, SkyCondition::HeavyRain);
        i.observed_hail = true;
        assert_eq!(classify(&i).condition, SkyCondition::Hail);
        // A close, recent strike is a thunderstorm; a distant or old one is not.
        i.last_strike = Some((i.now - 300, Some(4.0)));
        assert_eq!(classify(&i).condition, SkyCondition::Thunderstorm);
        i.observed_hail = false;
        i.observed_rain_in_hr = None;
        i.last_strike = Some((i.now - 300, Some(25.0)));
        assert_ne!(classify(&i).condition, SkyCondition::Thunderstorm);
        i.last_strike = Some((i.now - 3600, Some(2.0)));
        assert_ne!(classify(&i).condition, SkyCondition::Thunderstorm);
    }

    #[test]
    fn visibility_alone_does_not_identify_fog_and_sun_outranks_modelled_visibility() {
        let mut i = at(edt(7, 2, 0));
        i.visibility_mi = observed(0.3);
        assert_eq!(classify(&i).condition, SkyCondition::LowVisibility);
        i.visibility_mi = observed(5.0);
        assert_ne!(classify(&i).condition, SkyCondition::Fog);
        // Bright measured sun outranks a model's fog.
        let mut day = at(edt(7, 13, 0));
        day.measured_solar_w_m2 = Some(800.0);
        day.visibility_mi = modelled(0.2);
        assert_eq!(classify(&day).condition, SkyCondition::Clear);
    }

    #[test]
    fn windy_is_a_qualifier_not_a_sky() {
        let mut i = at(edt(7, 2, 0));
        i.cloud_cover_pct = modelled(5.0);
        i.wind_avg_mph = Some(22.0);
        let sky = classify(&i);
        assert_eq!(sky.condition, SkyCondition::Clear);
        assert!(sky.windy);
        i.wind_avg_mph = Some(8.0);
        i.wind_gust_mph = Some(31.0);
        assert!(classify(&i).windy);
    }

    #[test]
    fn without_a_site_light_alone_cannot_say_day_night_or_cloud_cover() {
        let mut i = SkyInputs {
            now: edt(7, 13, 0),
            ..Default::default()
        };
        assert_eq!(classify(&i).is_day, None);
        i.measured_solar_w_m2 = Some(0.0);
        assert_eq!(classify(&i).is_day, None);
        i.measured_solar_w_m2 = Some(700.0);
        let sky = classify(&i);
        assert_eq!((sky.is_day, sky.condition), (None, SkyCondition::Unknown));
        // Cloud evidence still wins over raw light.
        i.cloud_cover_pct = modelled(95.0);
        assert_eq!(classify(&i).condition, SkyCondition::Overcast);
    }

    #[test]
    fn invalid_or_unlocated_lightning_is_not_a_nearby_storm() {
        let mut i = at(edt(7, 12, 0));
        i.cloud_cover_pct = observed(20.0);
        for strike in [
            (i.now + 1, Some(1.0)),
            (i64::MIN, Some(1.0)),
            (i.now - 10, None),
            (i.now - 10, Some(-1.0)),
            (i.now - 10, Some(f64::NAN)),
        ] {
            i.last_strike = Some(strike);
            assert_eq!(classify(&i).condition, SkyCondition::MostlyClear);
        }
        i.last_strike = Some((i.now - 900, Some(10.0)));
        assert_eq!(classify(&i).condition, SkyCondition::Thunderstorm);
    }

    #[test]
    fn corrupt_readings_do_not_become_weather() {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let i = SkyInputs {
                now: edt(7, 12, 0),
                site: SITE,
                measured_solar_w_m2: Some(invalid),
                observed_rain_in_hr: Some(invalid),
                wind_avg_mph: Some(invalid),
                wind_gust_mph: Some(invalid),
                cloud_cover_pct: observed(invalid),
                visibility_mi: observed(invalid),
                ..Default::default()
            };
            let sky = classify(&i);
            assert_eq!(sky.condition, SkyCondition::Unknown);
            assert!(!sky.windy && !sky.precipitating);
        }
    }

    #[test]
    fn cloud_reports_outrank_shaded_sunlight_and_light_does_not_invent_a_percentage() {
        let mut i = at(edt(7, 13, 0));
        i.measured_solar_w_m2 = Some(1.0);
        i.cloud_cover_pct = observed(10.0);
        let sky = classify(&i);
        assert_eq!(
            (sky.condition, sky.cover_basis, sky.cloud_cover_pct),
            (SkyCondition::Clear, SkyBasis::Observation, Some(10))
        );
        i.cloud_cover_pct = None;
        let sky = classify(&i);
        assert_eq!(sky.cover_basis, SkyBasis::MeasuredSunlight);
        assert_eq!(sky.cloud_cover_pct, None);
    }

    #[test]
    fn polar_seasons_follow_the_hemisphere_without_a_sunrise_event() {
        for (month, north_day) in [(6, true), (12, false)] {
            let now = Utc
                .with_ymd_and_hms(2026, month, 21, 0, 0, 0)
                .unwrap()
                .timestamp();
            for (lat, is_day) in [(80.0, north_day), (-80.0, !north_day)] {
                let sky = classify(&SkyInputs {
                    now,
                    site: Some((lat, 0.0)),
                    cloud_cover_pct: observed(100.0),
                    ..Default::default()
                });
                assert_eq!(sky.is_day, Some(is_day));
                assert_eq!(sky.condition, SkyCondition::Overcast);
            }
        }
    }
}
