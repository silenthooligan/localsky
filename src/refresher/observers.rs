// What a stored snapshot sets in motion, beside the loop that stored it:
// the run-edge ingest (history rows from running edges), the forecast
// observation and ET0 ledger rows, the push edges (zone start and stop,
// the daily verdict, probe faults and quarantines, a stale forecast, the
// inferred-target notice) and the per-tick metrics. The loop is read,
// prefetch, decide, store; everything here subscribes to the store.

use super::*;
use crate::assembly::*;
use crate::controllers::registry::ControllerRegistry;
use crate::forecast::snapshot::ForecastSnapshot;
use crate::forecast::ForecastStore;
use crate::history::IngestState;
use crate::model::IrrigationSnapshot;
use crate::refresher::store::IrrigationStore;
use crate::tempest::state::TempestStore;
use chrono::Utc;
use rusqlite::Connection;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct ObserverDeps {
    pub history_conn: Option<Arc<Mutex<Connection>>>,
    pub push: crate::push::PushDispatcher,
    pub forecast_store: Arc<ForecastStore>,
    pub tempest_store: Arc<TempestStore>,
    pub controllers: ControllerRegistry,
    pub source: SnapshotSource,
    /// Set when a run row landed, so the loop re-reads the balance
    /// evidence on its next tick instead of after the coarse timer.
    pub balance_dirty: Arc<AtomicBool>,
}

/// The edge-detection state the push observers carry between snapshots.
#[derive(Default)]
pub(crate) struct PushEdges {
    prev_zone_running: std::collections::HashMap<String, bool>,
    zone_started_at: std::collections::HashMap<String, i64>,
    /// Daily verdict push fires once per local day; the date is the key.
    last_verdict_day: Option<String>,
    /// A probe fault pushes at most once per probe per process lifetime.
    probe_fault_notified: std::collections::HashSet<String>,
    /// A quarantine pushes once per episode: the set latches the zones
    /// currently quarantined, and a zone that leaves re-arms.
    quarantined_zones: std::collections::HashSet<String>,
    forecast_stale_notified: bool,
    inferred_plan_announced: bool,
    /// The refresh instant of the last snapshot observed. A snapshot the
    /// loop re-stored after a failed read keeps the previous instant, so
    /// it carries no new decision and is skipped.
    last_refresh_seen: i64,
}

/// Subscribe to every stored snapshot and run the observers on each.
pub fn spawn_observers(
    store: Arc<IrrigationStore>,
    deps: ObserverDeps,
) -> tokio::task::JoinHandle<()> {
    let mut rx = store.subscribe_every();
    tokio::spawn(async move {
        let forecast_obs_store = forecast_observations_store(deps.history_conn.as_ref()).await;
        let mut ingest = IngestState::new();
        let mut edges = PushEdges::default();
        loop {
            match rx.recv().await {
                Ok(snap) => {
                    observe(
                        &deps,
                        forecast_obs_store.as_ref(),
                        &mut ingest,
                        &mut edges,
                        &snap,
                    )
                    .await;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        missed = n,
                        "snapshot observers fell behind; continuing from the newest"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// One stored snapshot's side effects, in the order the loop used to run
/// them. A snapshot the loop re-stored after a failed read (same refresh
/// instant, marked unreachable) carries no new decision and triggers
/// nothing.
pub(crate) async fn observe(
    deps: &ObserverDeps,
    forecast_obs_store: Option<&crate::persistence::ForecastObservationsStore>,
    ingest: &mut IngestState,
    edges: &mut PushEdges,
    snap: &IrrigationSnapshot,
) {
    if snap.last_refresh_epoch <= edges.last_refresh_seen {
        return;
    }
    edges.last_refresh_seen = snap.last_refresh_epoch;
    if let Some(db) = deps.history_conn.as_ref() {
        // Zones whose running state is a dry-run controller's pretend
        // water this tick: the observer records those rows as source
        // 'dry_run' so they never become watering evidence.
        let simulated = IngestState::simulated_running_slugs(&deps.controllers).await;
        let runs_written = ingest.observe(db, snap, &simulated).await;
        if runs_written > 0 {
            deps.balance_dirty.store(true, Ordering::SeqCst);
        }
    }
    // Forecast-bias daily ingest. Today's predicted rain comes from the
    // forecast store's daily[0]; today's observed rain is the
    // merge-contested daily total tagged with the owning writer's nature,
    // not the station-gated skip_check value. The store keeps the day
    // max so a gauge going stale mid-storm never resets the total. See
    // ledger_observation for the midnight and plausibility gates.
    if let Some(obs_store) = forecast_obs_store {
        // Configured-timezone date, not the container's: a UTC container
        // would otherwise file evening observations under tomorrow's row.
        let today = crate::timeutil::now_local().date_naive();
        let now_epoch = Utc::now().timestamp();
        let calendar = crate::timeutil::deployment_calendar();
        // Preserve actual observations during a QPF outage. The existing
        // negative prediction marker is repaired by the first real forecast;
        // a fabricated zero would permanently poison the day's bias sample.
        let predicted_in = calendar
            .date_of(now_epoch)
            .and_then(|day| {
                deps.forecast_store
                    .snapshot()
                    .today_precip_in_at(calendar, day)
            })
            .unwrap_or(-1.0);
        let owner = deps.tempest_store.rain_today_owner(now_epoch);
        if let Some((observed_in, source)) =
            ledger_observation(&deps.tempest_store.snapshot(), owner.as_ref(), now_epoch)
        {
            let store_handle = obs_store.clone();
            tokio::spawn(async move {
                if let Err(e) = store_handle
                    .upsert(today, predicted_in, observed_in, source)
                    .await
                {
                    tracing::debug!(error = %e, "forecast observation upsert failed");
                }
            });
        }
        // ET0 self-emit: the day's resolved reference ET0 lands in the
        // ledger under source 'localsky_engine', day-max like the rain
        // total, so every install accrues a durable per-day ET0 record.
        // An unresolved day emits nothing, never a fabricated figure.
        if let Some(et0_mm) = ledger_et0_emission(
            &deps.forecast_store.snapshot(),
            snap.forecast.eto_today_mm,
            now_epoch,
        ) {
            let store_handle = obs_store.clone();
            tokio::spawn(async move {
                if let Err(e) = store_handle
                    .upsert_et0(today, et0_mm, "localsky_engine")
                    .await
                {
                    tracing::debug!(error = %e, "et0 ledger upsert failed");
                }
            });
        }
    }
    if deps.source == SnapshotSource::HomeAssistant && !edges.inferred_plan_announced {
        let planned: Vec<&crate::model::WaterBudget> = snap
            .water_budgets
            .iter()
            // A soil-governed zone waters by its own deficit, so an
            // inferred weekly target on it is nothing to warn about.
            .filter(|b| b.on_inferred_weekly_target() && b.today_seconds > 0)
            .collect();
        if !planned.is_empty() {
            edges.inferred_plan_announced = true;
            for b in &planned {
                tracing::warn!(
                    zone = %b.zone_slug,
                    weekly_budget_in = b.weekly_budget_in,
                    sessions_per_week = b.sessions_per_week,
                    today_seconds = b.today_seconds,
                    "zone plans a run on a weekly target inferred from its name; \
                     set Weekly target and Sessions per week under Settings, then \
                     Zones"
                );
            }
            deps.push
                .emit(crate::push::PushEvent::InferredTargetsPlanned {
                    zones: planned.iter().map(|b| b.zone_name.clone()).collect(),
                });
        }
    }
    emit_push_events(
        &deps.push,
        snap,
        edges,
        deps.forecast_store.snapshot().last_refresh_epoch,
    );
    // Per-tick engine metrics from the authoritative snapshot (verdict
    // mix and degraded rate are the core health signals).
    crate::metrics::inc("localsky_refresh_total", String::new());
    crate::metrics::set_gauge("localsky_last_refresh_epoch", Utc::now().timestamp() as f64);
    if let Some(t) = snap.decision_trace.as_ref() {
        if t.degraded {
            crate::metrics::inc("localsky_refresh_degraded_total", String::new());
        }
    }
    // Count the verdict the engine actually DECIDED, not the trace's own
    // (the trace ignores the sticky global override).
    crate::metrics::inc(
        "localsky_verdict_total",
        crate::metrics::label("verdict", &snap.skip_check.verdict),
    );
}

/// Physical ceiling on a plausible daily rain total (inches). Values above
/// it are garbage frames (a unit misparse, a scale/offset misconfig on an
/// MQTT or passthrough writer), and the day-max upsert would record them
/// permanently; mirror of the SOIL_PCT_PHYSICAL_MAX quality-gate pattern.
pub(crate) const RAIN_TODAY_PHYSICAL_MAX_IN: f64 = 15.0;

/// What the observations-ledger writer records this tick:
/// `Some((observed_in, source))` to upsert, `None` to skip the tick.
///
///   - gauge/radar owner (fresh) with a same-day accumulator and a
///     plausible value: the measured day total, with its provenance.
///   - model-nature owner: the 0.0/'none' placeholder. A model
///     RainTodayIn fill is the WHOLE day's forecast, including hours
///     that have not happened; recording it as observed would let
///     phantom rain persist in the day-max ledger for a full trailing
///     window. Model rain reaches the balance through the archive rung
///     and the defer gate instead.
///   - no owner, or a stale one: the 0.0/'none' placeholder.
///   - accumulator still on the previous local day (the first ticks
///     after configured-tz midnight, before the next observation resets
///     it): SKIP: a day-max write now would pin yesterday's total onto
///     the new day's row permanently.
///   - implausible value (non-finite, negative, above the physical
///     cap): SKIP with a warning naming the owner, leaving the day's
///     ledger untouched rather than pinning garbage.
pub(crate) fn ledger_observation(
    snapshot: &crate::tempest::state::Snapshot,
    owner: Option<&crate::tempest::state::RainOwner>,
    now_epoch: i64,
) -> Option<(f64, &'static str)> {
    let source = classify_rain_today_source(owner);
    if source != "gauge" && source != "radar" {
        return Some((0.0, "none"));
    }
    if snapshot.rain_today_day_ordinal != crate::timeutil::local_day_ordinal(now_epoch) {
        // Midnight carry gate: the accumulator has not rolled onto the new
        // local day yet.
        return None;
    }
    let v = snapshot.rain_in_today;
    if !v.is_finite() || !(0.0..=RAIN_TODAY_PHYSICAL_MAX_IN).contains(&v) {
        tracing::warn!(
            owner = owner.map(|o| o.label.as_str()).unwrap_or(""),
            value = v,
            cap_in = RAIN_TODAY_PHYSICAL_MAX_IN,
            "implausible daily rain total; observations-ledger write skipped"
        );
        return None;
    }
    Some((v, source))
}

/// Physical ceiling on a plausible daily reference ET0 (mm). The hottest,
/// driest, windiest irrigated climates top out around 15 mm/day; values
/// above this are unit misparses (an inches figure scaled twice, a
/// misconfigured mapping) and the day-MAX upsert would pin them for the
/// whole replay window. Mirror of `RAIN_TODAY_PHYSICAL_MAX_IN`.
pub(crate) const ET0_TODAY_PHYSICAL_MAX_MM: f64 = 20.0;

/// What the ET0 self-emit records this tick: `Some(mm)` to upsert under
/// source 'localsky_engine', `None` to skip the tick. Follows
/// `ledger_observation`'s shape:
///
///   - no resolved figure (the snapshot's `eto_today_mm` is null): SKIP.
///     The ladder found no evidence and nothing fabricated is recorded.
///   - implausible value (non-finite, non-positive, above the physical
///     cap): SKIP with a warning, leaving the day's ledger untouched.
///   - forecast still on the previous local day (the first ticks after
///     configured-tz midnight, before the provider's daily window rolls):
///     SKIP: the resolved figure describes yesterday, and a day-max write
///     now would pin yesterday's total onto the new day's row. A missing
///     daily series carries no date to gate on and writes normally (the
///     bus-owned figure's contract is today's full-day value).
pub(crate) fn ledger_et0_emission(
    fc: &ForecastSnapshot,
    eto_today_mm: Option<f64>,
    now_epoch: i64,
) -> Option<f64> {
    let v = eto_today_mm?;
    if !v.is_finite() || !(0.0..=ET0_TODAY_PHYSICAL_MAX_MM).contains(&v) {
        tracing::warn!(
            value = v,
            cap_mm = ET0_TODAY_PHYSICAL_MAX_MM,
            "implausible daily ET0; ledger self-emit skipped"
        );
        return None;
    }
    let cal = crate::timeutil::deployment_calendar();
    let today = cal.date_of(now_epoch)?;
    // A leftover previous-night row may precede a valid row for today.
    // A forecast that only covers yesterday still cannot certify today's ET.
    if !fc.daily.is_empty() && fc.aligned(cal, today).today().is_none() {
        return None;
    }
    Some(v)
}

/// Parse the canonical quarantine reason string the engine produces
/// (`quarantine_reason` in `engine::skip_rules`) back into its numbers for
/// the push payload. The format is:
///   "Soil probe suspect (<probe> vs yard <median>%); watering held until the probe is reliable"
/// where `<probe>` is either "<n>%" (a present-but-outlier reading) or the
/// literal "offline". Returns `(raw_pct, yard_pct)`: `raw_pct` is `None` for
/// the offline case. Returns `None` when the string isn't a quarantine reason
/// or can't be parsed (defensive; the caller then skips the push rather than
/// firing with bogus numbers).
pub(crate) fn parse_quarantine_reason(reason: &str) -> Option<(Option<f64>, f64)> {
    let inner = reason
        .strip_prefix("Soil probe suspect (")?
        .split_once(')')?
        .0; // "<probe> vs yard <median>%"
    let (probe_str, yard_str) = inner.split_once(" vs yard ")?;
    let yard_pct = yard_str.trim_end_matches('%').trim().parse::<f64>().ok()?;
    let raw_pct = if probe_str.trim() == "offline" {
        None
    } else {
        Some(probe_str.trim_end_matches('%').trim().parse::<f64>().ok()?)
    };
    Some((raw_pct, yard_pct))
}

/// Walk the snapshot and emit push events on edge transitions:
/// - ZoneStarted/ZoneStopped on each zone's running flag flip.
/// - DailyVerdict once per local day, the first time we see a non-empty
///   verdict for that day.
/// - SoilProbeFault when a probe first appears in soil_probe_faults
///   (once per probe per process lifetime via `probe_fault_notified`).
/// - SoilProbeSuspect when a zone's verdict source becomes "soil_quarantine"
///   (once per zone per quarantine episode via `quarantined_zones`, which
///   latches the currently-quarantined slugs and clears them on exit so a
///   later re-quarantine notifies again).
#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_push_events(
    push: &crate::push::PushDispatcher,
    snap: &IrrigationSnapshot,
    edges: &mut PushEdges,
    forecast_last_refresh_epoch: i64,
) {
    use crate::push::PushEvent;
    let PushEdges {
        prev_zone_running: prev_running,
        zone_started_at: started_at,
        last_verdict_day,
        probe_fault_notified,
        quarantined_zones,
        forecast_stale_notified,
        ..
    } = edges;
    let now = Utc::now().timestamp();
    for z in &snap.zones {
        let was = *prev_running.get(&z.slug).unwrap_or(&false);
        if z.running && !was {
            started_at.insert(z.slug.clone(), now);
            push.emit(PushEvent::ZoneStarted {
                name: z.name.clone(),
                slug: z.slug.clone(),
            });
        } else if !z.running && was {
            let dur_s = started_at
                .remove(&z.slug)
                .map(|start| (now - start).max(0))
                .unwrap_or(0);
            let duration_min = ((dur_s as f64) / 60.0).round() as u32;
            push.emit(PushEvent::ZoneStopped {
                name: z.name.clone(),
                slug: z.slug.clone(),
                duration_min,
            });
        }
        prev_running.insert(z.slug.clone(), z.running);
    }

    // Soil-probe faults: notify on the transition into faulted state,
    // at most once per probe for the life of the process.
    // The forecast going stale is the one source-silence the snapshot
    // states outright. Once per episode: the flag flipping on pushes, the
    // flag clearing re-arms.
    let stale = forecast_is_stale(forecast_last_refresh_epoch, now);
    if stale && !*forecast_stale_notified {
        *forecast_stale_notified = true;
        push.emit(PushEvent::SourceOffline {
            source_id: "forecast".into(),
            silent_s: now.saturating_sub(forecast_last_refresh_epoch.max(0)),
        });
    } else if !stale {
        *forecast_stale_notified = false;
    }

    for f in &snap.soil_probe_faults {
        if probe_fault_notified.insert(f.zone_slug.clone()) {
            push.emit(PushEvent::SoilProbeFault {
                zone_name: f.zone_name.clone(),
                zone_slug: f.zone_slug.clone(),
                since_epoch: f.since_epoch,
            });
        }
    }

    // Soil-probe QUARANTINE: a zone held for an unavailable or suspect probe
    // (source == "soil_quarantine"). Edge-triggered
    // per episode: notify only on the transition INTO quarantine; a zone
    // that was already in the latched set is skipped until it leaves and
    // re-enters. The reason string carries the suspect raw% + sibling
    // median, parsed back out for the push numbers (engine produces it).
    let now_quarantined: std::collections::HashSet<String> = snap
        .zones
        .iter()
        .filter(|z| z.verdict.as_ref().map(|v| v.source.as_str()) == Some("soil_quarantine"))
        .map(|z| z.slug.clone())
        .collect();
    for z in &snap.zones {
        let Some(v) = z.verdict.as_ref() else {
            continue;
        };
        if v.source != "soil_quarantine" {
            continue;
        }
        // Edge into quarantine: only fire when this slug wasn't already latched.
        if quarantined_zones.contains(&z.slug) {
            continue;
        }
        match parse_quarantine_reason(&v.reason) {
            Some((raw_pct, yard_pct)) => {
                push.emit(PushEvent::SoilProbeSuspect {
                    zone_name: z.name.clone(),
                    zone_slug: z.slug.clone(),
                    raw_pct,
                    yard_pct,
                });
            }
            None => {
                tracing::debug!(
                    zone = %z.slug,
                    reason = %v.reason,
                    "soil_quarantine reason unparseable; suppressing suspect push"
                );
            }
        }
    }
    // Replace the latch with the current set: slugs that left quarantine drop
    // out (so a later re-quarantine notifies again), entries we just notified
    // are now latched so the 10s poll cadence doesn't re-fire every tick.
    *quarantined_zones = now_quarantined;

    // Daily verdict fires once per local day. The "today" label is the
    // local-date YYYY-MM-DD; on the first refresh after midnight rolls
    // we emit one event with the new verdict.
    // The once-a-day dedupe rolls on the CONFIGURED-timezone date.
    let today = crate::timeutil::now_local().format("%Y-%m-%d").to_string();
    let verdict = snap.skip_check.verdict.clone();
    if !verdict.is_empty() && last_verdict_day.as_deref() != Some(today.as_str()) {
        // Carry honest confidence into the morning push. When the
        // decision ran on substituted inputs (stale station and/or aged forecast,
        // folded into the trace's degraded flag), say so up front so the
        // notification is never more confident than the data behind it.
        let degraded = snap
            .decision_trace
            .as_ref()
            .map(|t| t.degraded)
            .unwrap_or(false);
        let base = snap.skip_check.reason.clone();
        let reason = match (degraded, base.is_empty()) {
            (true, true) => {
                "Decided on backup data (lower confidence until live data returns).".to_string()
            }
            (true, false) => format!("Decided on backup data (lower confidence). {base}"),
            (false, _) => base,
        };
        push.emit(crate::push::PushEvent::DailyVerdict { verdict, reason });
        *last_verdict_day = Some(today);
    }
}
