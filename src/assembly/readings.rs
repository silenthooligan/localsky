// Readings the assembly derives from what a pass was handed: Home
// Assistant entity states, the station's live conditions against the
// forecast's current hour, ET0 from the forecast day, the rain-today
// provenance. Pure.

use crate::engine::skip_rules::{LiveReadings, ZoneSoil};
use crate::forecast::snapshot::ForecastSnapshot;
use serde_json::Value;
use std::collections::HashMap;

pub(crate) fn state_eq(map: &HashMap<String, Value>, eid: &str, expected: &str) -> bool {
    map.get(eid)
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map(|s| s == expected)
        .unwrap_or(false)
}

/// A binary sensor is evidence only when HA reports an explicit on/off state.
/// Missing, unavailable, unknown and malformed states cannot certify idle.
pub(crate) fn state_on_off(map: &HashMap<String, Value>, eid: &str) -> Option<bool> {
    match map.get(eid)?.get("state")?.as_str()? {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

pub(crate) fn state_f64(map: &HashMap<String, Value>, eid: &str) -> Option<f64> {
    map.get(eid)
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Native reference ET0 (mm/day) computed from a forecast day's temperature
/// range + latitude, via the (previously unwired) engine::et0 module. Hargreaves
/// only needs Tmax/Tmin + extraterrestrial radiation (lat + day-of-year), so this
/// works for ANY forecast source (not just the Open-Meteo HA REST sensor) and
/// replaces the flat 5.0 fallback. `None` when the day has no usable temps.
pub(crate) fn native_et0_mm(
    d: &crate::forecast::snapshot::DailyEntry,
    lat: f64,
    doy: u16,
    elevation_m: f64,
) -> Option<f64> {
    use crate::config::schema::Et0Method;
    use crate::engine::et0::{compute, f_to_c, Et0Inputs};
    let temp_max = d.temp_max_f.filter(|v| v.is_finite())?;
    let temp_min = d.temp_min_f.filter(|v| v.is_finite())?;
    if temp_max < temp_min {
        return None;
    }
    // Daily means have their own fields. Peak wind and humidity at Tmax
    // cannot feed the daily FAO-56 equation. Without real means Auto selects
    // the explicitly approximate Hargreaves temperature-range fallback.
    let rh = d
        .humidity_mean_pct
        .filter(|v| v.is_finite() && (0.0..=100.0).contains(v));
    let u2 = d.wind_mean_2m_ms.filter(|v| v.is_finite() && *v >= 0.0);
    let solar = (d.solar_rad_mj_m2_day.is_finite() && d.solar_rad_mj_m2_day > 0.0)
        .then_some(d.solar_rad_mj_m2_day);
    let inputs = Et0Inputs {
        t_max_c: f_to_c(temp_max),
        t_min_c: f_to_c(temp_min),
        t_mean_c: None,
        rh_max_pct: None,
        rh_min_pct: None,
        rh_mean_pct: rh,
        u2_ms: u2,
        solar_rad_mj_m2_day: solar,
        pressure_kpa: None,
        elevation_m,
        latitude_deg: lat,
        doy: doy.clamp(1, 366),
    };
    let r = compute(&inputs, Et0Method::Auto);
    (r.et0_mm_day.is_finite() && r.et0_mm_day >= 0.0).then_some(r.et0_mm_day)
}

/// Today's forecast (temp max, temp min, representative humidity) as
/// Options from the live forecast snapshot's daily[0]; `None` when it
/// carries no value. An unwrap_or(0.0) here once fabricated a 0°F/0°F
/// range and 0% humidity, which the Day block rendered as forecast data
/// and the LLM advisor was prompted with as ground truth.
pub(crate) fn resolve_today_range(
    fc: &ForecastSnapshot,
) -> (Option<f64>, Option<f64>, Option<f64>) {
    let today = fc.daily.first();
    let temp_max = today.and_then(|d| d.temp_max_f).filter(|v| v.is_finite());
    let temp_min = today.and_then(|d| d.temp_min_f).filter(|v| v.is_finite());
    let humidity = today
        .and_then(|d| d.humidity_pct)
        .filter(|h| *h <= 100)
        .map(f64::from);
    (temp_max, temp_min, humidity)
}

/// Engine-internal working assumption for daily reference ET0 (mm/day) when
/// no rung of `resolve_et0_today_mm` produced a value (forecast outage or
/// cold start). Consumed ONLY by the advisory soil projection so its curve
/// can still be drawn; it is never published. The snapshot's `eto_today_mm`
/// stays `None` (serialized null) so the HA sensor reads unknown and the
/// dashboard shows a dash instead of recording a fabricated 5.0 mm/day of
/// evapotranspiration into long-term statistics.
pub(crate) const ENGINE_ET0_FALLBACK_MM: f64 = 5.0;

/// Today's reference ET0 (mm), source-agnostic. The contract is the FULL-DAY
/// figure (snapshot doc: eto_today_mm):
///   1. the bus et0_today: a source that reports (or a user mapping that
///      feeds) the field directly owns it. The field's contract is FULL-DAY
///      mm; the built-in Open-Meteo fill emits today's converted daily total,
///      and an explicit HA-passthrough/MQTT mapping is honored as mapped.
///      (A station accumulator mapped here reads low early in the day; issue
///      #4's actual 25x collapse was the OM fill emitting inches, fixed at
///      the emit. Accumulators belong in a dedicated field if one is added.)
///   2. the forecast provider's own daily[0] ET0 (inches -> mm),
///   3. native Hargreaves from the forecast temps.
/// `None` when every rung comes up empty: the published field carries the
/// unknown honestly; a consumer that needs a working assumption opts into
/// `ENGINE_ET0_FALLBACK_MM` explicitly.
pub(crate) fn resolve_et0_today_mm(
    snapshot_et0: f64,
    fc: &ForecastSnapshot,
    lat: f64,
    doy: u16,
    elevation_m: f64,
) -> Option<f64> {
    if snapshot_et0.is_finite() && snapshot_et0 > 0.0 {
        return Some(snapshot_et0);
    }
    if let Some(d) = fc.daily.first() {
        if let Some(et0) = d.reference_et0_mm() {
            return Some(et0);
        }
    }
    fc.daily
        .first()
        .and_then(|d| native_et0_mm(d, lat, doy, elevation_m))
}

/// ET0 (mm) for forecast day `idx` (0=today): the provider's own daily ET0
/// (inches -> mm) > native Hargreaves > `fallback`. Mirrors the ranks of
/// resolve_et0_today_mm, so today / tomorrow / 3-day agree in method and
/// units on every install class (the old native-only tomorrow disagreed with
/// the provider-fed today, which is what made the pair collapse at rollover).
pub(crate) fn forecast_day_et0_mm(
    fc: &ForecastSnapshot,
    idx: usize,
    lat: f64,
    base_date: chrono::NaiveDate,
    fallback: f64,
    elevation_m: f64,
) -> f64 {
    if let Some(d) = fc.daily.get(idx) {
        if let Some(et0) = d.reference_et0_mm() {
            return et0;
        }
    }
    let Some(d) = fc.daily.get(idx) else {
        return fallback;
    };
    native_et0_mm(
        d,
        lat,
        chrono::Datelike::ordinal(&(base_date + chrono::Duration::days(idx as i64))) as u16,
        elevation_m,
    )
    .unwrap_or(fallback)
}

/// The forecast-observations writer's provenance tag for the day's
/// rain total, from the merge's rain-today owner. A live station owning
/// the daily total is a gauge; a cloud owner is classified by its
/// catalog nature (radar QPE day products vs model day totals); no
/// owner at all means the install has no rain-capable source and the
/// day records a 'none' placeholder.
pub(crate) fn classify_rain_today_source(
    owner: Option<&crate::tempest::state::RainOwner>,
) -> &'static str {
    match owner {
        None => "none",
        // A stale owner is a writer that went silent; its frozen value must
        // not keep fabricating wet days (the 3-tier rain gate applies the
        // same freshness rule to the rain-rate owner). The live tier only
        // ever returns fresh owners, so this arm covers the fill tier.
        Some(o) if !o.is_fresh => "none",
        Some(o) => match o.nature {
            crate::model::RainNature::Measured => "gauge",
            crate::model::RainNature::RadarQpe => "radar",
            _ => "model",
        },
    }
}

/// How old the Open-Meteo forecast may be before its forward-looking rain inputs
/// are no longer trusted for a SKIP. The store refreshes every ~30 min, so 6h is
/// 12 missed polls, well past a transient outage but short enough to catch a real
/// multi-hour staleness before a stale "rain coming" suppresses a needed run.
pub(crate) use crate::forecast::snapshot::forecast_is_stale;
#[cfg(test)]
pub(crate) use crate::forecast::snapshot::FORECAST_MAX_AGE_S;

/// Resolve current temperature, wind and humidity from the bus-selected fields.
/// Each field retains its source's age limit and measurement nature. Remote
/// station observations are measurements too. Missing/stale fields may use the
/// current forecast hour, explicitly degraded; incomplete evidence holds water.
pub(crate) fn resolve_current_conditions(
    readings: &[Option<crate::weather::CurrentWeatherSample>; 3],
    current_hour: Option<&crate::forecast::snapshot::HourlyEntry>,
    now_epoch: i64,
) -> (f64, f64, f64, LiveReadings) {
    let valid = |index: usize| {
        readings[index].as_ref().filter(|sample| {
            !crate::weather::arbitration::owner_is_stale(
                sample.max_age_s,
                sample.observed_epoch,
                now_epoch,
            ) && sample.value.is_finite()
                && (index != 1 || sample.value >= 0.0)
                && (index != 2 || (0.0..=100.0).contains(&sample.value))
        })
    };
    let selected = [valid(0), valid(1), valid(2)];
    let temp = selected[0].map(|s| s.value).or_else(|| {
        current_hour
            .and_then(|h| h.temp_f)
            .filter(|v| v.is_finite())
    });
    let wind = selected[1].map(|s| s.value).or_else(|| {
        current_hour
            .and_then(|h| h.wind_mph)
            .filter(|v| v.is_finite() && *v >= 0.0)
    });
    let rh = selected[2].map(|s| s.value).or_else(|| {
        current_hour
            .and_then(|h| h.humidity_pct)
            .filter(|v| *v <= 100)
            .map(f64::from)
    });
    if let (Some(temp), Some(wind), Some(rh)) = (temp, wind, rh) {
        let nature = if selected
            .iter()
            .all(|sample| sample.is_some_and(|s| s.measured))
        {
            LiveReadings::Station
        } else {
            LiveReadings::ForecastFallback
        };
        return (temp, wind, rh, nature);
    }
    // Missing is never calm. The integrity gate refuses these placeholders.
    (
        temp.unwrap_or(0.0),
        wind.unwrap_or(0.0),
        rh.unwrap_or(0.0),
        LiveReadings::Unavailable,
    )
}

#[cfg(test)]
pub(crate) fn test_current_samples(
    snapshot: &crate::weather::Snapshot,
    ages: [i64; 3],
) -> [Option<crate::weather::CurrentWeatherSample>; 3] {
    [
        (snapshot.air_temp_f, snapshot.air_temp_live_epoch),
        (snapshot.wind_avg_mph, snapshot.wind_live_epoch),
        (snapshot.rh_pct, snapshot.rh_live_epoch),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (value, observed_epoch))| {
        Some(crate::weather::CurrentWeatherSample {
            value,
            observed_epoch,
            source_id: "fixture".into(),
            max_age_s: ages[index],
            measured: true,
            selection_reason: String::new(),
        })
    })
    .collect::<Vec<_>>()
    .try_into()
    .unwrap()
}

/// One soil entry per ACTIVE zone for a config whose zones table is
/// empty, so every zone still gets a per-zone verdict:
/// `skip_rules::decide_per_zone` iterates this list, and a zone missing
/// from it gets no verdict at all. An install with zones in its config
/// never reaches this: `resolve_soil_zones` already emits one entry per
/// configured zone, probe or no probe.
///
/// No probe is read here. Every zone carries `pct: None`, which the soil
/// gates treat as no probe, and the slug's default saturation band. The
/// entity-name discovery this replaced (`sensor.<slug>_soil_moisture` for
/// four zone names inherited from the original deployment) is gone with
/// the environment-variable zone list it served.
pub(crate) fn unprobed_soil_zones(zones: &[crate::zones::ZoneIdent]) -> Vec<ZoneSoil> {
    // Beds and shrubs hold water longer than turf, so they keep the
    // higher ceiling and lower floor. The TURF pair is the schema's own
    // unset band; the bed pair has no schema home because no config
    // field distinguishes a bed here, and on this path the slug is the
    // only signal that exists.
    fn defaults(slug: &str) -> (f64, f64) {
        use crate::config::schema::{DEFAULT_SATURATION_PCT, DEFAULT_TARGET_MIN_PCT};
        if slug.contains("shrub") || slug.contains("garden") || slug.contains("bed") {
            (85.0, 25.0)
        } else {
            (DEFAULT_SATURATION_PCT, DEFAULT_TARGET_MIN_PCT)
        }
    }
    zones
        .iter()
        .map(|z| {
            let (saturation_pct, target_min_pct) = defaults(&z.slug);
            ZoneSoil {
                slug: z.slug.clone(),
                name: z.display_name.clone(),
                // A zone with no config row has no configured head; the
                // catalog default is the honest answer, and no restriction
                // exemption can name a head the operator never chose.
                sprinkler_type: Default::default(),
                pct: None,
                probe_configured: false,
                saturation_pct,
                target_min_pct,
                // Zones with no config row have no agronomy, so the soil
                // model never governs them.
                governed_by_soil_model: false,
                planning_forecast_unavailable: false,
            }
        })
        .collect()
}
