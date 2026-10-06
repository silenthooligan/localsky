//! Measured condition changes, with persistent edges and cooldowns. Forecast
//! fills and stale values cannot produce alerts or re-arm a condition.
use crate::{
    notification_preferences::EventKind,
    push::{PushDispatcher, PushEvent},
    tempest::state::TempestStore,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug)]
struct Reading {
    kind: EventKind,
    value: f64,
    epoch: i64,
    source: String,
}

fn limits(kind: EventKind, value: f64) -> (bool, bool) {
    match kind {
        EventKind::Rain => (value > 0.0, value == 0.0),
        EventKind::Wind => (value >= 25.0, value <= 20.0),
        EventKind::Freeze => (value <= 32.0, value >= 34.0),
        EventKind::Heat => (value >= 95.0, value <= 90.0),
        EventKind::Lightning => (value > 0.0, value == 0.0),
        _ => (false, false),
    }
}

fn claim(conn: &Connection, reading: &Reading, now: i64) -> rusqlite::Result<bool> {
    let key = format!("{:?}", reading.kind);
    let old: Option<(bool, i64, i64)> = conn.query_row(
        "SELECT active,last_fired_epoch,observed_epoch FROM notification_conditions WHERE kind=?1",
        [&key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).optional()?;
    let (crossed, cleared) = limits(reading.kind, reading.value);
    let Some((was_active, fired, observed)) = old else {
        // First reading establishes a baseline, including after a new install.
        conn.execute(
            "INSERT INTO notification_conditions VALUES (?1,?2,0,?3)",
            params![key, crossed, reading.epoch],
        )?;
        return Ok(false);
    };
    if reading.epoch <= observed {
        return Ok(false);
    }
    let active = crossed || (was_active && !cleared);
    let send = active && !was_active && (fired == 0 || now - fired >= 3600);
    conn.execute("UPDATE notification_conditions SET active=?2,last_fired_epoch=?3,observed_epoch=?4 WHERE kind=?1",
        params![key, active, if send { now } else { fired }, reading.epoch])?;
    Ok(send)
}

pub(crate) async fn observe(
    push: &PushDispatcher,
    store: &TempestStore,
    connection: Option<&Arc<Mutex<Connection>>>,
    now: i64,
) {
    let Some(connection) = connection else { return };
    let mut readings = Vec::new();
    for (field, sample) in store.current_weather_samples(now) {
        if !sample.measured
            || !sample.value.is_finite()
            || sample.observed_epoch <= 0
            || !(0..=sample.max_age_s.min(900)).contains(&(now - sample.observed_epoch))
        {
            continue;
        }
        let kinds: &[EventKind] = match field.as_str() {
            "air_temp_f" => &[EventKind::Freeze, EventKind::Heat],
            "wind_mph" => &[EventKind::Wind],
            _ => &[],
        };
        for kind in kinds {
            readings.push(Reading {
                kind: *kind,
                value: sample.value,
                epoch: sample.observed_epoch,
                source: sample.source_id.clone(),
            });
        }
    }
    let snap = store.snapshot();
    if let Some(owner) = store.rain_owner(now).filter(|owner| {
        owner.is_live
            && owner.is_fresh
            && snap.rain_live_epoch > 0
            && (0..=300).contains(&(now - snap.rain_live_epoch))
            && snap.rain_intensity_in_hr.is_finite()
            && snap.rain_intensity_in_hr >= 0.0
    }) {
        readings.push(Reading {
            kind: EventKind::Rain,
            value: snap.rain_intensity_in_hr,
            epoch: snap.rain_live_epoch,
            source: owner.label,
        });
    }
    // Nearby strikes come only from station observations. Keep the episode
    // active for 30 minutes; old strikes cannot trigger on a restart.
    let nearby = snap
        .lightning_recent
        .iter()
        .filter(|s| {
            (0..=300).contains(&(now - s.time_epoch))
                && s.distance_km.is_finite()
                && (0.0..=16.09344).contains(&s.distance_km)
        })
        .max_by_key(|s| s.time_epoch);
    if let Some(strike) = nearby {
        readings.push(Reading {
            kind: EventKind::Lightning,
            value: 1.0,
            epoch: strike.time_epoch,
            source: strike.source.clone(),
        });
    }
    let connection = connection.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Reading>> {
        let mut conn = connection.blocking_lock();
        let tx = conn.transaction()?;
        tx.execute("INSERT OR IGNORE INTO notification_conditions VALUES ('Lightning',0,0,0)", [])?;
        // Reset a lightning episode only after the last NEARBY strike ages out.
        tx.execute("UPDATE notification_conditions SET active=0 WHERE kind='Lightning' AND observed_epoch < ?1", [now - 1800])?;
        let mut events = Vec::new();
        for reading in readings {
            if claim(&tx, &reading, now)? { events.push(reading); }
        }
        tx.commit()?;
        Ok(events)
    }).await;
    match result {
        Ok(Ok(events)) => {
            for r in events {
                let description = match r.kind {
                    EventKind::Rain => "Rain is being measured at your station.".into(),
                    EventKind::Wind => format!(
                        "Sustained wind is {:.0} mph ({:.0} km/h).",
                        r.value,
                        r.value * 1.609344
                    ),
                    EventKind::Freeze | EventKind::Heat => format!(
                        "Air temperature is {:.0}°F ({:.0}°C).",
                        r.value,
                        (r.value - 32.0) * 5.0 / 9.0
                    ),
                    EventKind::Lightning => {
                        "Lightning was detected within 10 miles (16 km).".into()
                    }
                    _ => continue,
                };
                push.emit(PushEvent::Weather {
                    kind: r.kind,
                    body: format!(
                        "{description} Source: {}. Open Weather for current conditions.",
                        r.source
                    ),
                });
            }
        }
        error => tracing::warn!(
            ?error,
            "weather notification state could not be saved; skipping"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rain_respects_the_sources_own_expiry_and_rejects_forecast_fills() {
        use crate::ports::weather_source::WeatherField as F;
        let mut db = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut db).unwrap();
        let db = Arc::new(Mutex::new(db));
        let store = TempestStore::new();
        store.set_max_ages(std::collections::HashMap::from([("gauge".into(), 60)]));
        let (push, mut rx) = PushDispatcher::capturing();
        store.apply_received_fields(&[(F::RainIntensityInHr, 0.0)], 1000, 1000, true, "gauge");
        observe(&push, &store, Some(&db), 1000).await;
        store.apply_received_fields(&[(F::RainIntensityInHr, 0.2)], 1001, 1001, true, "gauge");
        observe(&push, &store, Some(&db), 1070).await;
        assert!(
            rx.try_recv().is_err(),
            "source expired after 60 seconds, even inside the 5-minute limit"
        );
        store.apply_received_fields(
            &[(F::RainIntensityInHr, 0.3)],
            1071,
            1071,
            false,
            "forecast",
        );
        observe(&push, &store, Some(&db), 1071).await;
        assert!(
            rx.try_recv().is_err(),
            "a forecast cannot reuse the old gauge freshness stamp"
        );
        store.apply_received_fields(&[(F::RainIntensityInHr, 0.2)], 1080, 1080, true, "gauge");
        observe(&push, &store, Some(&db), 1080).await;
        assert!(matches!(
            rx.try_recv().unwrap(),
            PushEvent::Weather {
                kind: EventKind::Rain,
                ..
            }
        ));
    }
    #[tokio::test]
    async fn only_fresh_measured_changes_can_notify() {
        use crate::ports::weather_source::WeatherField as F;
        let mut db = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut db).unwrap();
        let db = Arc::new(Mutex::new(db));
        let store = TempestStore::new();
        let (push, mut rx) = PushDispatcher::capturing();
        store.apply_received_fields(&[(F::WindMph, 15.0)], 1000, 1000, true, "station");
        observe(&push, &store, Some(&db), 1000).await;
        store.apply_received_fields(&[(F::WindMph, 30.0)], 1001, 1001, false, "forecast");
        observe(&push, &store, Some(&db), 1001).await;
        assert!(rx.try_recv().is_err());
        store.apply_received_fields(&[(F::WindMph, 30.0)], 1002, 1002, true, "station");
        observe(&push, &store, Some(&db), 3000).await;
        assert!(
            rx.try_recv().is_err(),
            "expired station reading cannot alert"
        );
        store.apply_received_fields(&[(F::WindMph, 30.0)], 3001, 3001, true, "station");
        observe(&push, &store, Some(&db), 3001).await;
        assert!(matches!(
            rx.try_recv().unwrap(),
            PushEvent::Weather {
                kind: EventKind::Wind,
                ..
            }
        ));
        observe(&push, &store, Some(&db), 3002).await;
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn persisted_crossings_do_not_repeat_on_restart_jitter_or_stale_samples() {
        let mut db = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut db).unwrap();
        let reading = |value, epoch| Reading {
            kind: EventKind::Wind,
            value,
            epoch,
            source: "station".into(),
        };
        assert!(!claim(&db, &reading(15.0, 1000), 1000).unwrap());
        assert!(claim(&db, &reading(25.0, 1100), 1100).unwrap());
        assert!(!claim(&db, &reading(30.0, 1100), 1100).unwrap());
        assert!(!claim(&db, &reading(24.0, 1200), 1200).unwrap());
        assert!(!claim(&db, &reading(26.0, 1300), 1300).unwrap());
        assert!(!claim(&db, &reading(15.0, 900), 1400).unwrap());
        assert!(!claim(&db, &reading(15.0, 1500), 1500).unwrap());
        assert!(!claim(&db, &reading(30.0, 1600), 1600).unwrap());
        assert!(!claim(&db, &reading(15.0, 4700), 4700).unwrap());
        assert!(claim(&db, &reading(30.0, 4800), 4800).unwrap());
    }
}
