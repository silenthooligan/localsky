// Smart morning dispatcher. The LocalSky-native replacement for
// Irrigation Unlimited's nightly sequence. Spawned from main.rs
// alongside the manual scheduler.
//
// Algorithm per tick (every 60s):
//   1. Compute today's local sunrise from (lat, lon) using NOAA's
//      analytical formula (no extra crates needed).
//   2. Snapshot the current IrrigationSnapshot. Sum planned_run_seconds
//      across zones to get the sequence total. Inter-zone preamble is
//      a fixed 2s, matching the IU controller's `preamble: "00:00:02"`.
//   3. target_finish = sunrise - 15min (matches IU's `anchor: finish,
//      sun: sunrise, before: 00:15`). target_start = target_finish -
//      sequence_total.
//   4. If `now` is within the ±60s window around target_start, AND we
//      haven't fired today (HashMap<NaiveDate, bool> dedupe), proceed.
//   5. If snapshot.skip_check.will_skip, log a skip row per zone with
//      source = "smart_morning" + the verdict reason, mark fired, return.
//   6. Otherwise iterate zones with planned_run_seconds > 0. For each:
//      split the zone's runtime via engine::cycle_soak so clay-soil
//      zones get cycle-and-soak treatment, then dispatch each segment
//      sequentially via controller.run_zone(slug, seg.run_seconds).
//      Sleep seg.soak_seconds between segments and
//      INTER_ZONE_PREAMBLE_S between zones.
//   7. Mark fired.
//
// Catch-up: on first tick after boot, if `now` is already past today's
// target_finish AND we haven't fired today AND there's no
// source="smart_morning" row in the runs table for today, treat the
// missed window as a catch-up opportunity. If `now < target_finish +
// CATCH_UP_GRACE` and the snapshot verdict is "run", dispatch
// immediately with whatever time remains before the forbidden-hour
// window. Past the grace cutoff we log a warning and mark the day so
// the loop doesn't repeatedly retry.
//
// LOCALSKY_SMART_DRY_RUN=1: skip the actual run_zone call; info!-log
// what would have fired. Used to validate dispatch behavior overnight
// before flipping IU off.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{Local, NaiveDate, TimeZone, Utc};
use tokio::time::interval;
use tracing::{info, warn};

use crate::config::schema::Config;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::cycle_soak;
use crate::engine::sprinkler_catalog::effective_precip_rate_mm_hr;
use crate::engine::sunrise::sunrise_utc;
use crate::ha::IrrigationStore;
use crate::ha::WateringPolicy;
use crate::persistence::runs::{NewRun, RunsStore};
use crate::push::dispatcher::{PushDispatcher, PushEvent};

/// Width of the "we are at target_start" window, in seconds. The tick
/// interval is 60s so a 90s tolerance guarantees exactly one match per
/// day even with small clock drift.
const TARGET_WINDOW_S: i64 = 90;

/// Inter-zone preamble in seconds. Matches IU's `preamble: "00:00:02"`
/// so the dispatch cadence is observable-equivalent to the prior IU
/// sequence the OS hardware was tuned against.
const INTER_ZONE_PREAMBLE_S: u64 = 2;

/// Catch-up grace window after target_finish. If LocalSky booted late
/// and there's still daylight between the dispatch window and the
/// SJRWMD forbidden-hour cutoff (typically 10am), we can still get a
/// useful run in. Two hours is enough to land before 10am for a
/// sunrise around 06:30 with a 1500s sequence.
const CATCH_UP_GRACE_S: i64 = 2 * 3600;

/// Spawn the smart-morning dispatcher. Returns immediately; the task
/// runs for the lifetime of the process. Safe to call with location
/// = (0.0, 0.0) — the formula still produces a finite sunrise; in
/// practice main.rs always passes a real lat/lon from the loaded toml.
pub fn spawn(
    irrigation_store: Arc<IrrigationStore>,
    _watering_policy: WateringPolicy,
    controllers: ControllerRegistry,
    runs: Option<RunsStore>,
    location: (f64, f64),
    cfg: Option<Arc<Config>>,
    push: Option<PushDispatcher>,
    dry_run: bool,
) {
    let (lat, lon) = location;
    info!(
        lat,
        lon,
        dry_run,
        catch_up_grace_s = CATCH_UP_GRACE_S,
        "smart morning scheduler: spawning tick"
    );
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));
        let mut last_fired: HashMap<NaiveDate, bool> = HashMap::new();
        let mut bootstrapped = false;
        loop {
            tick.tick().await;
            let now_local = Local::now();
            let today: NaiveDate = now_local.date_naive();

            let snap = irrigation_store.snapshot();
            let total_dispatch_s: u64 = snap
                .zones
                .iter()
                .map(|z| z.planned_run_seconds.max(0) as u64)
                .sum();
            let zones_to_run: usize = snap
                .zones
                .iter()
                .filter(|z| z.planned_run_seconds > 0)
                .count();
            let sequence_total_s =
                total_dispatch_s + INTER_ZONE_PREAMBLE_S * (zones_to_run.saturating_sub(1)) as u64;

            let sunrise = match sunrise_utc(today, lat, lon) {
                Some(s) => s,
                None => {
                    continue;
                }
            };
            let target_finish = sunrise - chrono::Duration::minutes(15);
            let target_start = target_finish - chrono::Duration::seconds(sequence_total_s as i64);

            let now_utc = Utc::now();
            let delta_s = (now_utc - target_start).num_seconds();
            let in_window = delta_s.abs() <= TARGET_WINDOW_S;

            // Boot-time catch-up: if we just started and today's window
            // is already past, check whether anyone already fired for
            // today. If not, treat it as a missed window and consider
            // an immediate catch-up.
            if !bootstrapped {
                bootstrapped = true;
                let already_fired_today = match runs.as_ref() {
                    Some(rs) => fired_smart_morning_today(rs, today).await,
                    None => false,
                };
                if already_fired_today {
                    // Another process (or a previous instance of this
                    // task before a restart) already handled today.
                    // Mark the dedupe slot so the regular tick path
                    // doesn't retry.
                    last_fired.insert(today, true);
                } else if (now_utc - target_finish).num_seconds() > TARGET_WINDOW_S {
                    // We're past the normal window without a row for today.
                    let past_finish_s = (now_utc - target_finish).num_seconds();
                    if past_finish_s <= CATCH_UP_GRACE_S {
                        info!(
                            past_finish_s,
                            "smart morning: catch-up — missed today's window, attempting late dispatch"
                        );
                        // Fall through into the regular dispatch path
                        // below by treating the rest of the tick body
                        // as in-window.
                        dispatch_today(
                            &snap,
                            &controllers,
                            runs.as_ref(),
                            push.as_ref(),
                            cfg.as_ref(),
                            today,
                            now_utc,
                            zones_to_run,
                            total_dispatch_s,
                            dry_run,
                            true,
                        )
                        .await;
                        last_fired.insert(today, true);
                        continue;
                    } else {
                        warn!(
                            past_finish_s,
                            grace_s = CATCH_UP_GRACE_S,
                            "smart morning: missed today's window past catch-up grace; logging missed-window row"
                        );
                        if let Some(rs) = runs.as_ref() {
                            for zone in &snap.zones {
                                if zone.planned_run_seconds == 0 {
                                    continue;
                                }
                                let row = NewRun {
                                    zone_slug: zone.slug.clone(),
                                    start_epoch: target_start.timestamp(),
                                    source: "smart_morning".into(),
                                    controller_id: controllers
                                        .default()
                                        .map(|c| c.id().to_string())
                                        .unwrap_or_default(),
                                    planned_duration_s: zone.planned_run_seconds.max(0) as u32,
                                    skip_reason: None,
                                    et0_mm: None,
                                    etc_mm: None,
                                    cycle_index: None,
                                    cycle_count: None,
                                };
                                if let Err(e) = rs
                                    .insert_skipped(
                                        row,
                                        "Missed dispatch window (LocalSky offline)".to_string(),
                                    )
                                    .await
                                {
                                    warn!(zone = %zone.slug, error = %e, "smart morning: missed-window row insert failed");
                                }
                            }
                        }
                        last_fired.insert(today, true);
                        continue;
                    }
                }
            }

            if !in_window {
                continue;
            }
            if last_fired.get(&today).copied().unwrap_or(false) {
                continue;
            }

            dispatch_today(
                &snap,
                &controllers,
                runs.as_ref(),
                push.as_ref(),
                cfg.as_ref(),
                today,
                now_utc,
                zones_to_run,
                total_dispatch_s,
                dry_run,
                false,
            )
            .await;
            last_fired.insert(today, true);
        }
    });
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_today(
    snap: &crate::ha::snapshot::IrrigationSnapshot,
    controllers: &ControllerRegistry,
    runs: Option<&RunsStore>,
    push: Option<&PushDispatcher>,
    cfg: Option<&Arc<Config>>,
    today: NaiveDate,
    now_utc: chrono::DateTime<Utc>,
    zones_to_run: usize,
    total_dispatch_s: u64,
    dry_run: bool,
    is_catch_up: bool,
) {
    let _ = today;

    // Decide skip vs run.
    if snap.skip_check.will_skip {
        let reason = if snap.skip_check.reason.is_empty() {
            "skip-rule ladder".to_string()
        } else {
            snap.skip_check.reason.clone()
        };
        if let Some(rs) = runs {
            for zone in &snap.zones {
                if zone.planned_run_seconds == 0 {
                    continue;
                }
                let row = NewRun {
                    zone_slug: zone.slug.clone(),
                    start_epoch: now_utc.timestamp(),
                    source: "smart_morning".into(),
                    controller_id: controllers
                        .default()
                        .map(|c| c.id().to_string())
                        .unwrap_or_default(),
                    planned_duration_s: zone.planned_run_seconds.max(0) as u32,
                    skip_reason: None,
                    et0_mm: None,
                    etc_mm: None,
                    cycle_index: None,
                    cycle_count: None,
                };
                if let Err(e) = rs.insert_skipped(row, reason.clone()).await {
                    warn!(zone = %zone.slug, error = %e, "smart morning: skip-row insert failed");
                }
            }
        }
        info!(
            reason = %reason,
            zones = zones_to_run,
            is_catch_up,
            "smart morning: skipped today's run"
        );
        if let Some(p) = push {
            p.emit(PushEvent::DailyVerdict {
                verdict: "skip".into(),
                reason: reason.clone(),
            });
        }
        return;
    }

    let controller = match controllers.default() {
        Some(c) => c,
        None => {
            warn!("smart morning: no default controller configured; skipping today");
            return;
        }
    };
    info!(
        zones = zones_to_run,
        total_s = total_dispatch_s,
        dry_run,
        is_catch_up,
        "smart morning: dispatching morning run"
    );
    if let Some(p) = push {
        let total_min = (total_dispatch_s as f64 / 60.0).round() as u32;
        let prefix = if is_catch_up { "Catch-up run: " } else { "" };
        p.emit(PushEvent::DailyVerdict {
            verdict: "run".into(),
            reason: format!("{prefix}{zones_to_run} zone(s), {total_min} min total"),
        });
    }

    let soak_minutes = cfg.as_ref().map(|c| c.engine.soak_minutes).unwrap_or(30);

    for zone in snap.zones.iter() {
        if zone.planned_run_seconds == 0 {
            continue;
        }
        let duration_s = zone.planned_run_seconds.max(0) as u32;

        // Build the cycle-and-soak plan if we have enough cfg context;
        // otherwise dispatch the zone in one shot.
        let segments = build_cycle_plan(cfg.as_deref(), &zone.slug, duration_s, soak_minutes);

        for (idx, seg) in segments.iter().enumerate() {
            if dry_run {
                info!(
                    zone = %zone.slug,
                    segment = idx,
                    of = segments.len(),
                    run_s = seg.run_seconds,
                    soak_s = seg.soak_seconds,
                    "smart morning [DRY_RUN]: would dispatch segment"
                );
                continue;
            }
            match controller.run_zone(&zone.slug, seg.run_seconds).await {
                Ok(handle) => {
                    info!(
                        zone = %zone.slug,
                        segment = idx,
                        of = segments.len(),
                        run_s = seg.run_seconds,
                        soak_s = seg.soak_seconds,
                        provider_ref = ?handle.provider_ref,
                        "smart morning: dispatched segment"
                    );
                }
                Err(e) => {
                    warn!(
                        zone = %zone.slug,
                        segment = idx,
                        error = %e,
                        "smart morning: controller dispatch failed"
                    );
                    break;
                }
            }
            let wait = seg.run_seconds as u64 + seg.soak_seconds as u64;
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
        if !dry_run {
            // Inter-zone preamble between zone N's last segment and
            // zone N+1's first segment. The last segment's soak is 0
            // so this is the only spacing here.
            tokio::time::sleep(Duration::from_secs(INTER_ZONE_PREAMBLE_S)).await;
        }
    }
}

/// Resolve a per-zone cycle-and-soak plan. Falls back to a single
/// no-split segment when cfg is unavailable or the zone slug doesn't
/// resolve to a configured zone (e.g. demo mode, mid-cutover state).
fn build_cycle_plan(
    cfg: Option<&Arc<Config>>,
    slug: &str,
    duration_s: u32,
    soak_minutes: u32,
) -> Vec<cycle_soak::CycleSegment> {
    let fallback = vec![cycle_soak::CycleSegment {
        run_seconds: duration_s,
        soak_seconds: 0,
    }];
    let Some(cfg) = cfg else {
        return fallback;
    };
    // The refresher underscore-normalizes slugs ("back-yard" ->
    // "back_yard") before populating the snapshot; the cfg keys may be
    // in either form. Try the slug as-given, then the dashed variant.
    let zone_cfg = cfg
        .zones
        .get(slug)
        .or_else(|| cfg.zones.get(&slug.replace('_', "-")));
    let Some(z) = zone_cfg else {
        return fallback;
    };
    let precip = effective_precip_rate_mm_hr(z.sprinkler_type, z.precip_rate_mm_hr);
    cycle_soak::split(
        duration_s,
        precip,
        z.soil_texture,
        z.slope_pct,
        soak_minutes,
    )
}

/// True when the runs table already has a smart_morning row for today
/// (any status — run/skipped/aborted all count). Used by the boot
/// catch-up path so a restart inside the same morning window doesn't
/// fire the dispatch twice.
async fn fired_smart_morning_today(runs: &RunsStore, today: NaiveDate) -> bool {
    let start_local = match today
        .and_hms_opt(0, 0, 0)
        .and_then(|d| Local.from_local_datetime(&d).single())
    {
        Some(d) => d.with_timezone(&Utc),
        None => return false,
    };
    let end_local = match today
        .succ_opt()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(|d| Local.from_local_datetime(&d).single())
    {
        Some(d) => d.with_timezone(&Utc),
        None => return false,
    };
    let rows = match runs
        .window(start_local.timestamp(), end_local.timestamp())
        .await
    {
        Ok(rs) => rs,
        Err(e) => {
            warn!(error = %e, "smart morning: catch-up window query failed");
            return false;
        }
    };
    rows.iter().any(|r| r.source == "smart_morning")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cycle_plan_fallback_when_cfg_missing() {
        let plan = build_cycle_plan(None, "back_yard", 1500, 30);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].run_seconds, 1500);
        assert_eq!(plan[0].soak_seconds, 0);
    }
}
