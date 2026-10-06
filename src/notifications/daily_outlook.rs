//! Optional daily outlooks: a short local-time window and durable at-most-once
//! admission. Never replay a missed outlook on restart or on another day.
use crate::{
    config::schema::DailyOutlook,
    model::IrrigationSnapshot,
    push::{PushDispatcher, PushEvent},
};
use chrono::{DateTime, FixedOffset, Timelike};
use rusqlite::Connection;
use std::sync::Arc;
use tokio::sync::Mutex;

pub(crate) async fn consider(
    push: &PushDispatcher,
    snapshot: &IrrigationSnapshot,
    connection: Option<&Arc<Mutex<Connection>>>,
    preferences: &DailyOutlook,
    now: DateTime<FixedOffset>,
) {
    if !preferences.due_at(now.hour() * 60 + now.minute())
        || snapshot.zones.is_empty()
        || !matches!(
            snapshot.skip_check.verdict.as_str(),
            "skip" | "run" | "run_extended"
        )
        || !(0..=120).contains(&(now.timestamp() - snapshot.last_refresh_epoch))
    {
        return;
    }
    let Some(connection) = connection else { return };
    let connection = connection.clone();
    let today = now.format("%Y-%m-%d").to_string();
    let claim = tokio::task::spawn_blocking(move || {
        connection.blocking_lock().execute(
            "INSERT INTO notification_delivery (kind, date_local, claimed_at_epoch)
             VALUES ('daily-outlook', ?1, ?2)
             ON CONFLICT(kind) DO UPDATE SET date_local=excluded.date_local,
                claimed_at_epoch=excluded.claimed_at_epoch
             WHERE notification_delivery.date_local < excluded.date_local",
            rusqlite::params![today, now.timestamp()],
        )
    })
    .await;
    match claim {
        Ok(Ok(1)) => {}
        Ok(Ok(_)) => return,
        error => {
            tracing::warn!(
                ?error,
                "daily outlook could not persist its delivery claim; skipping"
            );
            return;
        }
    }
    tracing::info!(date_local = %now.date_naive(), "daily watering outlook claimed");
    push.emit(PushEvent::DailyVerdict {
        verdict: snapshot.skip_check.verdict.clone(),
        reason: outlook_reason(snapshot),
    });
}

fn outlook_reason(snapshot: &IrrigationSnapshot) -> String {
    let base = crate::reason_render::plain_watering_reason(&snapshot.skip_check.reason);
    let mut reason = if snapshot
        .decision_trace
        .as_ref()
        .is_some_and(|trace| trace.degraded)
    {
        format!("Using backup weather readings. {base}")
    } else {
        base
    };
    if !reason.is_empty() {
        // Engine reasons are often fragments; end one before the next sentence.
        if !reason.ends_with(['.', '!', '?']) {
            reason.push('.');
        }
        reason.push(' ');
    }
    reason.push_str("See zone plans in LocalSky; timing may change with conditions.");
    reason
}

pub(crate) async fn consider_devices(
    push: &PushDispatcher,
    snapshot: &IrrigationSnapshot,
    connection: Option<&Arc<Mutex<Connection>>>,
    now: DateTime<FixedOffset>,
) {
    use crate::push::store;
    if snapshot.zones.is_empty()
        || !matches!(
            snapshot.skip_check.verdict.as_str(),
            "skip" | "run" | "run_extended"
        )
        || !(0..=120).contains(&(now.timestamp() - snapshot.last_refresh_epoch))
    {
        return;
    }
    let Some(conn) = connection else { return };
    let Ok(subs) = store::list_all(conn.clone()).await else {
        return;
    };
    let date_local = now.format("%Y-%m-%d").to_string();
    for sub in subs {
        if sub.preferences.outlook_due(now.hour() * 60 + now.minute())
            && store::claim_outlook(conn.clone(), sub.endpoint.clone(), date_local.clone())
                .await
                .unwrap_or(false)
        {
            push.emit(PushEvent::DeviceOutlook {
                endpoint: sub.endpoint,
                date_local: date_local.clone(),
                reason: outlook_reason(snapshot),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[tokio::test]
    async fn device_outlooks_respect_individual_times_and_persist_daily_limits() {
        use crate::push::store::{self, StoredSubscription};
        let db = database();
        for (endpoint, time) in [("phone", "09:00"), ("tablet", "10:00")] {
            store::upsert(
                db.clone(),
                StoredSubscription {
                    endpoint: endpoint.into(),
                    p256dh: "key".into(),
                    auth: "auth".into(),
                    preferences: Default::default(),
                },
            )
            .await
            .unwrap();
            let mut prefs = crate::notification_preferences::PushPreferences::default();
            prefs.daily_outlook = DailyOutlook {
                enabled: true,
                time: time.into(),
            };
            store::preferences(
                db.clone(),
                endpoint.into(),
                "key".into(),
                "auth".into(),
                Some(prefs),
            )
            .await
            .unwrap();
        }
        let (push, mut rx) = PushDispatcher::capturing();
        let now = at(5, 0, 0);
        consider_devices(&push, &snapshot(now), Some(&db), now).await;
        assert!(rx.try_recv().is_err());
        for (hour, expected) in [(9, "phone"), (10, "tablet")] {
            let now = at(5, hour, 0);
            consider_devices(&push, &snapshot(now), Some(&db), now).await;
            match rx.try_recv().unwrap() {
                PushEvent::DeviceOutlook {
                    endpoint,
                    date_local,
                    ..
                } => {
                    assert_eq!(endpoint, expected);
                    assert_eq!(date_local, "2026-10-05");
                }
                ev => panic!("wrong event: {ev:?}"),
            }
            consider_devices(&push, &snapshot(now), Some(&db), now).await;
            assert!(rx.try_recv().is_err());
        }
        let now = at(6, 10, 15);
        consider_devices(&push, &snapshot(now), Some(&db), now).await;
        assert!(rx.try_recv().is_err());
    }

    fn at(day: u32, hour: u32, minute: u32) -> DateTime<FixedOffset> {
        FixedOffset::west_opt(4 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, day, hour, minute, 0)
            .unwrap()
    }
    fn snapshot(now: DateTime<FixedOffset>) -> IrrigationSnapshot {
        let mut snapshot = IrrigationSnapshot {
            last_refresh_epoch: now.timestamp(),
            zones: vec![crate::model::ZoneState::default()],
            ..Default::default()
        };
        snapshot
            .skip_check
            .decide("run", "Weather allows watering".into(), "run".into());
        snapshot
    }
    fn database() -> Arc<Mutex<Connection>> {
        let mut db = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut db).unwrap();
        Arc::new(Mutex::new(db))
    }

    #[test]
    fn old_configs_opt_out_and_time_window_is_bounded() {
        let old: crate::config::schema::Notifications = serde_json::from_str("{}").unwrap();
        assert!(!old.daily_outlook.enabled);
        let mut prefs = DailyOutlook {
            enabled: true,
            ..Default::default()
        };
        for minute in [0, 539, 555, 1439] {
            assert!(!prefs.due_at(minute));
        }
        for minute in [540, 554] {
            assert!(prefs.due_at(minute));
        }
        for invalid in ["", "9:00", "24:00", "09:60", "aa:bb", "09:00:00"] {
            prefs.time = invalid.into();
            assert_eq!(prefs.minute_of_day(), None);
            assert!(!prefs.due_at(540));
        }
        prefs.time = "23:59".into();
        assert!(prefs.due_at(1439));
        assert!(!prefs.due_at(0));
    }

    #[tokio::test]
    async fn midnight_restarts_and_forecast_changes_do_not_repeat_outlooks() {
        let db = database();
        let prefs = DailyOutlook {
            enabled: true,
            ..Default::default()
        };
        let (push, mut rx) = PushDispatcher::capturing();
        for now in [at(5, 0, 0), at(5, 1, 9), at(5, 8, 59)] {
            consider(&push, &snapshot(now), Some(&db), &prefs, now).await;
            assert!(rx.try_recv().is_err());
        }
        let now = at(5, 9, 0);
        consider(
            &push,
            &snapshot(now),
            Some(&db),
            &DailyOutlook::default(),
            now,
        )
        .await;
        assert!(rx.try_recv().is_err());
        consider(&push, &snapshot(now), Some(&db), &prefs, now).await;
        assert!(matches!(
            rx.try_recv().unwrap(),
            PushEvent::DailyVerdict { .. }
        ));
        // A fresh dispatcher has no memory of the previous delivery.
        let (restarted, mut second_rx) = PushDispatcher::capturing();
        let mut changed = snapshot(now);
        changed
            .skip_check
            .decide("skip", "Rain is expected".into(), "rain".into());
        consider(&restarted, &changed, Some(&db), &prefs, now).await;
        assert!(second_rx.try_recv().is_err());
        let tomorrow = at(6, 9, 0);
        consider(&restarted, &snapshot(tomorrow), Some(&db), &prefs, tomorrow).await;
        assert!(matches!(
            second_rx.try_recv().unwrap(),
            PushEvent::DailyVerdict { .. }
        ));
    }

    #[tokio::test]
    async fn missing_storage_stale_data_and_missed_windows_do_not_send() {
        let db = database();
        let prefs = DailyOutlook {
            enabled: true,
            ..Default::default()
        };
        let (push, mut rx) = PushDispatcher::capturing();
        let now = at(5, 9, 0);
        consider(&push, &snapshot(now), None, &prefs, now).await;
        let broken_db = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        consider(&push, &snapshot(now), Some(&broken_db), &prefs, now).await;
        consider(&push, &snapshot(at(5, 8, 57)), Some(&db), &prefs, now).await;
        let late = at(5, 9, 15);
        consider(&push, &snapshot(late), Some(&db), &prefs, late).await;
        assert!(rx.try_recv().is_err());
        // Skipping does not consume tomorrow's delivery.
        let next = at(6, 9, 0);
        consider(&push, &snapshot(next), Some(&db), &prefs, next).await;
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn concurrent_observers_claim_one_summary_in_the_local_timezone() {
        let db = database();
        let prefs = DailyOutlook {
            enabled: true,
            ..Default::default()
        };
        let (push, mut rx) = PushDispatcher::capturing();
        // UTC is already tomorrow; the selected installation is still on Oct 5.
        let now = FixedOffset::west_opt(12 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 10, 5, 20, 0, 0)
            .unwrap();
        let evening = DailyOutlook {
            time: "20:00".into(),
            ..prefs.clone()
        };
        let snap = snapshot(now);
        tokio::join!(
            consider(&push, &snap, Some(&db), &evening, now),
            consider(&push, &snap, Some(&db), &evening, now)
        );
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
        let date: String = db
            .lock()
            .await
            .query_row("SELECT date_local FROM notification_delivery", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(date, "2026-10-05");
        let now = at(6, 9, 0);
        let mut no_zones = snapshot(now);
        no_zones.zones.clear();
        consider(&push, &no_zones, Some(&db), &prefs, now).await;
        assert!(rx.try_recv().is_err());
        let mut degraded = snapshot(now);
        degraded.decision_trace = Some(crate::model::DecisionTrace {
            degraded: true,
            ..Default::default()
        });
        consider(&push, &degraded, Some(&db), &prefs, now).await;
        match rx.try_recv().unwrap() {
            PushEvent::DailyVerdict { reason, .. } => {
                assert!(reason.starts_with("Using backup weather readings."))
            }
            event => panic!("Unexpected event: {event:?}"),
        }
    }

    #[test]
    fn outlook_text_ends_a_reason_fragment_before_the_follow_up() {
        let mut snap = snapshot(at(5, 9, 0));
        assert_eq!(
            outlook_reason(&snap),
            "Weather allows watering. See zone plans in LocalSky; timing may change with conditions."
        );
        snap.skip_check
            .decide("skip", "Rain covered today.".into(), "skip".into());
        assert_eq!(
            outlook_reason(&snap),
            "Rain covered today. See zone plans in LocalSky; timing may change with conditions."
        );
    }
}
