// Weekly tuning-report notification. An hourly tick (backup.rs shape:
// consumed first tick + catch_unwind supervision) gated to a local
// morning window; when the freshly generated report carries at least one
// recommendation and at least 7 local days have passed since the last
// notice, the last-notified stamp is persisted FIRST (M0014) and only
// then is PushEvent::TuningReportReady emitted. The push channel is
// lossy fire-and-forget, so persist-then-emit trades a rare lost push
// for never double-notifying across the restart-heavy deploy cadence.

use std::time::Duration;

use chrono::Timelike;

use crate::persistence::TuningReportStateStore;
use crate::push::dispatcher::{PushDispatcher, PushEvent};

/// Minimum local days between notifications.
const NOTIFY_INTERVAL_DAYS: i64 = 7;
/// The notice lands in a morning window (local hours, half-open) so it
/// reads alongside the daily verdict rather than waking anyone at 02:00.
const MORNING_START_HOUR: u32 = 7;
const MORNING_END_HOUR: u32 = 10;

/// Spawn the weekly tuning-report notifier. Ticks hourly for the process
/// lifetime; every decision below re-reads persisted state, so restarts
/// need no boot reconciliation (the dedupe IS the DB row).
pub fn spawn(state: TuningReportStateStore, push: PushDispatcher) {
    tracing::info!(
        interval_days = NOTIFY_INTERVAL_DAYS,
        "tuning report notifier: spawning hourly tick"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        // interval fires immediately; consume it so boot (often seconds
        // after a redeploy) never races the report against half-started
        // stores.
        tick.tick().await;
        loop {
            tick.tick().await;
            use futures::FutureExt;
            let outcome = std::panic::AssertUnwindSafe(run_once(&state, &push))
                .catch_unwind()
                .await;
            if outcome.is_err() {
                tracing::error!("tuning report notifier: pass panicked; continuing");
            }
        }
    });
}

async fn run_once(state: &TuningReportStateStore, push: &PushDispatcher) {
    let now_local = crate::timeutil::now_local();
    if !(MORNING_START_HOUR..MORNING_END_HOUR).contains(&now_local.hour()) {
        return;
    }
    // 7 LOCAL days since the last notice, counted on the configured-tz
    // calendar (never the container's).
    let last = state.last_notified_epoch().await;
    if last > 0 {
        if let Some(last_day) = crate::timeutil::local_date(last) {
            let elapsed = (now_local.date_naive() - last_day).num_days();
            if elapsed < NOTIFY_INTERVAL_DAYS {
                return;
            }
        }
    }
    let report =
        match crate::tuning::generate_report(crate::engine::tuning::DEFAULT_WINDOW_DAYS).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "tuning report notifier: generation unavailable");
                return;
            }
        };
    let recommendation_count = report
        .zones
        .iter()
        .filter(|z| z.recommendation.is_some())
        .count();
    if recommendation_count == 0 {
        // Nothing actionable: no stamp, no push. The next week with a
        // recommendation notifies immediately.
        return;
    }
    // Persist FIRST. If the stamp cannot be written, do not emit: an
    // unstamped emit would re-notify on the next tick (and on every
    // redeploy), and a delayed notice is the cheaper failure.
    let now_epoch = chrono::Utc::now().timestamp();
    if let Err(e) = state.set_last_notified_epoch(now_epoch).await {
        tracing::warn!(error = %e, "tuning report notifier: stamp write failed; skipping emit");
        return;
    }
    push.emit(PushEvent::TuningReportReady {
        recommendation_count,
    });
    tracing::info!(
        recommendations = recommendation_count,
        "tuning report notification emitted"
    );
}
