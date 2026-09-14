// Boot phase 4: the live stores.
//
// The three in-memory stores every reader shares (current conditions,
// forecast, the irrigation snapshot) and the push dispatcher that fans
// events out to subscribed browsers. The stores are built empty; the
// tasks that fill them belong to the control and sources phases. What
// does happen here is the seeding that only needs the stores and the
// history: the rain-today accumulator, the forecast cache, the
// arbitration priorities the config names.

use std::sync::Arc;

use crate::forecast::ForecastStore;
use crate::push::{self, PushDispatcher};
use crate::refresher::IrrigationStore;
use crate::tempest::state::TempestStore;

use super::config::BootConfig;
use super::storage::Storage;

/// What the stores phase yields.
pub struct Stores {
    pub tempest: Arc<TempestStore>,
    pub forecast: Arc<ForecastStore>,
    pub irrigation: Arc<IrrigationStore>,
    pub push: PushDispatcher,
}

pub async fn build(storage: &Storage, config: &BootConfig) -> Stores {
    let tempest = Arc::new(TempestStore::new());

    // Restore source-specific minute identities before any producer or
    // irrigation tick can race a seed. Daily/model/generic gauge ledger rows
    // cannot be attributed to a particular minute-reporting station.
    if !storage.demo_mode {
        if let Some(hc) = storage.history_conn.clone() {
            restore_rain_minutes(hc, &tempest, chrono::Utc::now().timestamp()).await;
        }
    }

    // Last-good forecast persists next to the history DB so a restart
    // during a provider outage rehydrates the previous snapshot (with its
    // ORIGINAL fetch epoch, so staleness guards stay honest) instead of
    // serving empty panels until the provider answers again. Demo mode
    // skips it: the feeder regenerates synthetic forecasts every minute
    // and should not churn writes or leak demo data into a real /data.
    let forecast = if storage.demo_mode {
        Arc::new(ForecastStore::new())
    } else {
        Arc::new(
            ForecastStore::new().with_persistence(storage.data_dir.join("forecast-cache.json")),
        )
    };

    // Non-fatal if the VAPID env is missing: the dispatcher logs once and
    // drops every event, and the rest of the app keeps running.
    let push = push::spawn_dispatcher(storage.history_conn.clone());

    // Current-conditions arbitration: each source's `priority`, staleness
    // window, per-field pin and per-field chain, installed through the
    // same `&self` setters the config hot-reload path uses, so a PUT
    // re-applies them with no restart. An empty config installs empty
    // maps and the merge is byte-identical to no overrides.
    if let Some(cfg) = config.cfg.as_ref() {
        // Kind changes require a restart; keep nature tied to the booted adapters.
        tempest.set_rain_natures(
            cfg.sources
                .iter()
                .filter_map(|source| {
                    use crate::sources::cloud_catalog::{cloud_meta, CloudDataNature};
                    let meta = cloud_meta(&source.source)?;
                    let nature = match meta.rain_nature {
                        CloudDataNature::Observation => crate::model::RainNature::Measured,
                        CloudDataNature::RadarQpe => crate::model::RainNature::RadarQpe,
                        _ => crate::model::RainNature::Model,
                    };
                    Some((source.id.clone(), nature))
                })
                .collect(),
        );
        tempest.set_priorities(crate::runtime::source_priority_map(cfg));
        tempest.set_observed_condition_fields(
            cfg.sources
                .iter()
                .filter_map(|source| {
                    use crate::sources::cloud_catalog::{cloud_meta, CloudDataNature};
                    let meta = cloud_meta(&source.source)?;
                    Some((
                        source.id.clone(),
                        ["air_temp_f", "wind_mph", "rh_pct"]
                            .map(|field| meta.field_nature(field) == CloudDataNature::Observation),
                    ))
                })
                .collect(),
        );
        tempest.set_max_ages(crate::runtime::source_max_age_map(cfg));
        tempest.set_field_overrides(crate::runtime::field_override_map(cfg));
        tempest.set_field_chains(crate::runtime::field_chain_map(cfg));
    }

    // The telemetry strip's sparklines come from sensor_history, which
    // the bus recorder writes for every source under its own id. A
    // separate sampler used to poll the snapshot for the one path that
    // did not go through the bus; there is no such path now.

    Stores {
        tempest,
        forecast,
        irrigation: Arc::new(IrrigationStore::new()),
        push,
    }
}

/// Recover only today's unique raw minute reports, grouped by bus source id.
/// Missing minutes stay missing; an unrelated gauge or a daily forecast cannot
/// raise a station's accumulator. Local-day bounds include DST correctly.
async fn restore_rain_minutes(
    hc: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    store: &TempestStore,
    now: i64,
) -> usize {
    let Some(day) = crate::timeutil::local_date(now) else {
        return 0;
    };
    let Some((start, end)) = crate::timeutil::local_day_bounds_utc(day) else {
        return 0;
    };
    let rows = tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<(String, f64, i64)>> {
        let conn = hc.blocking_lock();
        let mut stmt = conn.prepare(
            "SELECT source_id, SUM(value), MAX(epoch) FROM sensor_history
            WHERE key = 'rain_in_last_min' AND epoch >= ?1 AND epoch < ?2 AND epoch <= ?3
              AND value >= 0 AND value <= ?4 GROUP BY source_id",
        )?;
        let rows = stmt
            .query_map(
                rusqlite::params![start.timestamp(), end.timestamp(), now, f64::MAX],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?
            .collect();
        rows
    })
    .await;
    match rows {
        Ok(Ok(rows)) => {
            let count = rows
                .into_iter()
                .filter(|(source, total, epoch)| {
                    store.restore_rain_minutes(source, *total, *epoch, now)
                })
                .count();
            tracing::info!(
                sources = count,
                "restored source-specific rain minute totals"
            );
            count
        }
        error => {
            tracing::warn!(
                ?error,
                "rain minute recovery unavailable; unrecorded rain remains unknown"
            );
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::sensor_history::{Reading, SensorHistoryStore};
    use crate::ports::weather_source::WeatherField;

    #[tokio::test]
    async fn restart_restores_unique_minutes_without_borrowing_another_gauge() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        let hc = Arc::new(tokio::sync::Mutex::new(conn));
        let history = SensorHistoryStore::new(hc.clone());
        let day = crate::timeutil::now_local().date_naive();
        let start = crate::timeutil::local_day_bounds_utc(day)
            .unwrap()
            .0
            .timestamp();
        let now = start + 3600;
        let reading = |source: &str, key: &str, epoch, value| Reading {
            source_id: source.into(),
            key: key.into(),
            epoch,
            value,
        };
        history
            .insert_many(vec![
                reading("ha", "rain_in_last_min", now - 60, 0.1),
                reading("ha", "rain_in_last_min", now, 0.2),
                reading("ha", "rain_in_last_min", now, 0.2),
                reading("ha", "rain_in_last_min", start - 1, 0.8),
                reading("ha", "rain_in_last_min", now + 60, 0.9),
                reading("ha", "rain_in_last_min", now - 120, -1.0),
                reading("airport", "rain_in_last_min", now, 0.7),
                reading("dry", "rain_in_last_min", now, 0.0),
                reading("model", "rain_today_in", now, 5.0),
            ])
            .await
            .unwrap();
        crate::persistence::ForecastObservationsStore::new(hc.clone())
            .upsert(day, 0.0, 4.0, "gauge")
            .await
            .unwrap();
        let store = TempestStore::new();
        assert_eq!(restore_rain_minutes(hc, &store, now).await, 3);
        // Replayed current state is already counted. A subsequent minute adds.
        store.apply_received_fields(&[(WeatherField::RainLastMinIn, 0.2)], now, now, true, "ha");
        assert!((store.snapshot().rain_in_today - 0.3).abs() < 1e-9);
        store.apply_received_fields(
            &[(WeatherField::RainLastMinIn, 0.1)],
            now + 60,
            now + 60,
            true,
            "ha",
        );
        assert!((store.snapshot().rain_in_today - 0.4).abs() < 1e-9);
        assert_eq!(store.rain_today_owner(now + 60).unwrap().label, "ha");
    }
}
