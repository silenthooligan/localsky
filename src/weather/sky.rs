// The sky's evidence, gathered from the live store and the forecast.
//
// Every reading passes through its owner first: a value counts only while
// the source that wrote it is fresh, and it counts as observed only when
// that source measures (a live sensor, a station report, a radar) rather
// than models. A zero the store carries for a field nobody reports is never
// mistaken for a reading.

use crate::engine::sky::{classify, ForecastHour, Reading, SkyInputs, SkyNow};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::model::RainNature;

use super::live_store::{LiveWeatherStore, RainOwner, Snapshot};

const FEET_PER_MILE: f64 = 5280.0;

/// The sky for `snap`, as of `now`.
pub fn sky_now(
    store: &LiveWeatherStore,
    snap: &Snapshot,
    forecast: Option<&ForecastSnapshot>,
    now: i64,
) -> SkyNow {
    let fresh = |key: &'static str| store.field_owner(key, now).filter(|o| o.is_fresh);
    let measures = |o: &RainOwner| o.is_live || o.nature != RainNature::Model;
    let reading = |key: &'static str, value: Option<f64>| {
        let owner = fresh(key)?;
        Some(Reading {
            value: value?,
            observed: measures(&owner),
        })
    };
    let forecast_hour = forecast
        .filter(|f| !crate::forecast::snapshot::forecast_is_stale(f.last_refresh_epoch, now))
        .and_then(|f| {
            f.hourly
                .iter()
                .find(|h| h.time_epoch <= now && now.saturating_sub(h.time_epoch) < 3600)
                .map(|h| ForecastHour {
                    cloud_cover_pct: h.cloud_cover_pct.map(f64::from),
                    weather_code: Some(h.weather_code),
                    visibility_mi: (h.visibility_ft > 0.0).then(|| h.visibility_ft / FEET_PER_MILE),
                })
        });
    classify(&SkyInputs {
        now,
        site: store.site(),
        measured_solar_w_m2: store.measured_solar_mean(now),
        // Rain counts only when something measured it: a model's current
        // interval is a forecast, not rain falling now.
        observed_rain_in_hr: fresh("rain_intensity_in_hr")
            .filter(measures)
            .map(|_| snap.rain_intensity_in_hr),
        observed_hail: snap.precip_type == 2 && fresh("precip_type").is_some_and(|o| o.is_live),
        air_temp_f: fresh("air_temp_f").map(|_| snap.air_temp_f),
        wind_avg_mph: fresh("wind_avg_mph").map(|_| snap.wind_avg_mph),
        wind_gust_mph: fresh("wind_gust_mph").map(|_| snap.wind_gust_mph),
        last_strike: snap
            .last_strike_epoch
            .map(|at| (at, snap.last_strike_distance_mi)),
        cloud_cover_pct: reading("cloud_cover_pct", snap.cloud_cover_pct),
        visibility_mi: reading("visibility_mi", snap.visibility_mi),
        forecast_hour,
    })
}

/// A copy of `snap` with its sky filled in, for serving.
pub fn with_sky(
    store: &LiveWeatherStore,
    snap: &Snapshot,
    forecast: Option<&ForecastSnapshot>,
    now: i64,
) -> Snapshot {
    let mut served = snap.clone();
    served.sky = Some(sky_now(store, snap, forecast, now));
    // Derived values need actual, current inputs. Snapshot's legacy numeric
    // defaults cannot stand in for an unreported humidity or a calm wind.
    // Re-evaluate on serve so a silent sensor also expires via the heartbeat.
    let fresh = |key| {
        store
            .field_owner(key, now)
            .is_some_and(|owner| owner.is_fresh)
    };
    let temp = if fresh("air_temp_f") {
        snap.air_temp_f
    } else {
        f64::NAN
    };
    let rh = if fresh("rh_pct") {
        snap.rh_pct
    } else {
        f64::NAN
    };
    let wind = if fresh("wind_avg_mph") {
        snap.wind_avg_mph
    } else {
        f64::NAN
    };
    served.feels_like_f =
        if (temp <= 50.0 && !wind.is_finite()) || (temp >= 78.0 && !rh.is_finite()) {
            f64::NAN
        } else {
            super::derived::feels_like_f(temp, rh, wind)
        };
    served.wet_bulb_f =
        crate::units::c_to_f(super::derived::wet_bulb_c(crate::units::f_to_c(temp), rh));
    served.dew_point_f = if fresh("dew_point_f") {
        snap.dew_point_f
    } else {
        crate::units::c_to_f(super::derived::dew_point_c(crate::units::f_to_c(temp), rh))
    };
    served
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::sky::SkyCondition;
    #[test]
    fn derived_readings_require_fresh_inputs_and_survive_null_serialization() {
        use crate::ports::weather_source::WeatherField as F;
        let store = LiveWeatherStore::new();
        let now = 1_800_000_000;
        store.apply_source_fields(&[(F::AirTempF, 90.0)], now, false, "cloud");
        let missing = with_sky(&store, &store.snapshot(), None, now);
        assert!(missing.feels_like_f.is_nan());
        assert!(missing.wet_bulb_f.is_nan());
        assert!(missing.dew_point_f.is_nan());
        store.apply_source_fields(&[(F::RhPct, 70.0)], now, false, "cloud");
        let current = with_sky(&store, &store.snapshot(), None, now);
        assert!(current.feels_like_f > current.air_temp_f);
        assert!(current.dew_point_f < current.wet_bulb_f);
        assert!(current.wet_bulb_f < current.air_temp_f);
        let stale = with_sky(&store, &store.snapshot(), None, now + 86_400);
        assert!(
            stale.feels_like_f.is_nan() && stale.wet_bulb_f.is_nan() && stale.dew_point_f.is_nan()
        );
        let decoded: Snapshot =
            serde_json::from_value(serde_json::to_value(stale).unwrap()).unwrap();
        assert!(decoded.dew_point_f.is_nan());
    }

    #[test]
    fn unknown_cold_wind_is_not_calm_but_measured_zero_is() {
        use crate::ports::weather_source::WeatherField as F;
        let store = LiveWeatherStore::new();
        let now = 1_800_000_000;
        store.apply_source_fields(&[(F::AirTempF, 40.0)], now, true, "station");
        assert!(with_sky(&store, &store.snapshot(), None, now)
            .feels_like_f
            .is_nan());
        store.apply_source_fields(&[(F::WindMph, 0.0)], now, true, "station");
        assert_eq!(
            with_sky(&store, &store.snapshot(), None, now).feels_like_f,
            40.0
        );
    }

    #[test]
    fn stale_and_future_forecasts_do_not_describe_the_current_sky() {
        let store = LiveWeatherStore::new();
        let now = 1_800_000_000;
        let mut fc = ForecastSnapshot {
            hourly: vec![crate::forecast::snapshot::HourlyEntry {
                time_epoch: now - 100,
                cloud_cover_pct: Some(100),
                ..Default::default()
            }],
            ..Default::default()
        };
        for at in [
            0,
            now + 1,
            now - crate::forecast::snapshot::FORECAST_MAX_AGE_S - 1,
        ] {
            fc.last_refresh_epoch = at;
            assert_eq!(
                sky_now(&store, &Snapshot::default(), Some(&fc), now).condition,
                SkyCondition::Unknown
            );
        }
        fc.last_refresh_epoch = now - 60;
        assert_eq!(
            sky_now(&store, &Snapshot::default(), Some(&fc), now).condition,
            SkyCondition::Overcast
        );
    }
}
