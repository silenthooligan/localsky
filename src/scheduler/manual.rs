// Manual schedule dispatcher. Spawned alongside the live refresher in
// main.rs; ticks every 60 seconds and fires zone runs from operator-
// defined `ManualSchedule` entries.
//
// Workflow on each tick:
//   1. Resolve `chrono::Local::now()` into (weekday u8, hour u8, min u8,
//      date_key NaiveDate).
//   2. For each enabled schedule: skip if weekday isn't in the list, or
//      if the (hour, minute) doesn't match the current minute exactly.
//   3. Dedupe: a HashMap<schedule_id, NaiveDate> remembers the date on
//      which each schedule last fired. The same minute getting two
//      ticks in a row (clock skew, leap seconds) doesn't double-dispatch.
//   4. Evaluate the watering policy. If a Phase C restriction blocks
//      today (`skip = true`), persist a `status="skipped"` runs row with
//      the reason and skip dispatch.
//   5. Otherwise call the default controller's `run_zone(slug, duration)`.
//      The controller adapter logs to the runs table itself; the
//      schedule's `id` is included in `source = "manual:<id>"` so the
//      dashboard can attribute the run.
//
// `ManualMode::Override` is honored elsewhere: src/ha/refresher.rs reads
// the policy + schedules at boot to decide whether to suppress the smart
// engine's dispatch for a zone with an enabled Override schedule today.
// This file only fires the manual run; it does not suppress smart.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{Datelike, Local, NaiveDate, Timelike};
use tokio::time::interval;
use tracing::{info, warn};

use crate::config::schema::ManualSchedule;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::restrictions;
use crate::ha::WateringPolicy;
use crate::persistence::runs::{NewRun, RunsStore};

/// Spawn the manual schedule tick. Returns immediately; the background
/// task lives for the lifetime of the process. Safe to call with empty
/// schedules — the tick early-returns when the list is empty.
pub fn spawn(
    schedules: Vec<ManualSchedule>,
    watering_policy: WateringPolicy,
    controllers: ControllerRegistry,
    runs: Option<RunsStore>,
) {
    if schedules.is_empty() {
        info!("manual scheduler: no schedules configured; task not spawned");
        return;
    }
    info!(count = schedules.len(), "manual scheduler: spawning tick");
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));
        let mut last_fired: HashMap<String, NaiveDate> = HashMap::new();
        loop {
            tick.tick().await;
            let now = Local::now();
            let weekday = now.weekday().num_days_from_sunday() as u8;
            let hour = now.hour() as u8;
            let minute = now.minute() as u8;
            let today: NaiveDate = now.date_naive();

            // Cap minutes from the policy (if any rule is active right now).
            // Each schedule's duration is min'd with this so a 90-min
            // schedule on a 60-min-capped jurisdiction doesn't overrun.
            let v = restrictions::evaluate(
                now,
                &watering_policy.restrictions,
                watering_policy.address_parity,
            );
            let cap_seconds: Option<u32> = v.max_minutes_cap.map(|m| m.saturating_mul(60));

            for s in &schedules {
                if !s.enabled {
                    continue;
                }
                if !s.weekdays.contains(&weekday) {
                    continue;
                }
                if s.start_hour != hour || s.start_minute != minute {
                    continue;
                }
                if last_fired.get(&s.id) == Some(&today) {
                    continue;
                }

                // Evaluate restrictions per-schedule. The same policy is
                // checked, but we want to log the skip reason per
                // schedule rather than once globally.
                if v.skip {
                    let reason = v
                        .reason
                        .clone()
                        .unwrap_or_else(|| "watering restriction".to_string());
                    if let Some(rs) = runs.as_ref() {
                        let row = NewRun {
                            zone_slug: s.zone_slug.clone(),
                            start_epoch: now.timestamp(),
                            source: format!("manual:{}", s.id),
                            controller_id: controllers
                                .default()
                                .map(|c| c.id().to_string())
                                .unwrap_or_default(),
                            planned_duration_s: s.duration_minutes.saturating_mul(60),
                            skip_reason: None,
                            et0_mm: None,
                            etc_mm: None,
                            cycle_index: None,
                            cycle_count: None,
                        };
                        if let Err(e) = rs.insert_skipped(row, reason.clone()).await {
                            warn!(schedule = %s.id, error = %e, "manual scheduler: skip-row insert failed");
                        }
                    }
                    info!(
                        schedule = %s.id,
                        zone = %s.zone_slug,
                        reason = %reason,
                        "manual scheduler: skipped run (watering restriction)"
                    );
                    last_fired.insert(s.id.clone(), today);
                    continue;
                }

                // Dispatch through the configured default controller.
                let controller = match controllers.default() {
                    Some(c) => c,
                    None => {
                        warn!(schedule = %s.id, "manual scheduler: no default controller configured; skipping");
                        last_fired.insert(s.id.clone(), today);
                        continue;
                    }
                };
                let mut duration_s = s.duration_minutes.saturating_mul(60);
                if let Some(c) = cap_seconds {
                    if c < duration_s {
                        duration_s = c;
                    }
                }
                info!(
                    schedule = %s.id,
                    zone = %s.zone_slug,
                    duration_s,
                    "manual scheduler: dispatching run"
                );
                match controller.run_zone(&s.zone_slug, duration_s).await {
                    Ok(handle) => {
                        info!(
                            schedule = %s.id,
                            zone = %s.zone_slug,
                            controller = %handle.controller_id,
                            provider_ref = ?handle.provider_ref,
                            "manual scheduler: run dispatched"
                        );
                    }
                    Err(e) => {
                        warn!(
                            schedule = %s.id,
                            zone = %s.zone_slug,
                            error = %e,
                            "manual scheduler: controller dispatch failed"
                        );
                    }
                }
                last_fired.insert(s.id.clone(), today);
            }
        }
    });
}

/// Returns true when any enabled `Override` schedule fires for `zone_slug`
/// on `weekday`. The refresher uses this to decide whether to suppress the
/// smart engine's dispatch for the zone — manual takes precedence under
/// Override, but smart math still computes for nerd visibility.
pub fn override_active_today(schedules: &[ManualSchedule], zone_slug: &str, weekday: u8) -> bool {
    schedules.iter().any(|s| {
        s.enabled
            && s.zone_slug == zone_slug
            && s.weekdays.contains(&weekday)
            && matches!(s.mode, crate::config::schema::ManualMode::Override)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::ManualMode;

    fn sched(id: &str, zone: &str, weekdays: Vec<u8>, mode: ManualMode) -> ManualSchedule {
        ManualSchedule {
            id: id.into(),
            name: id.into(),
            zone_slug: zone.into(),
            enabled: true,
            weekdays,
            start_hour: 5,
            start_minute: 0,
            duration_minutes: 30,
            mode,
        }
    }

    #[test]
    fn override_active_matches_zone_and_weekday() {
        let s = vec![sched("a", "back_yard", vec![3, 6], ManualMode::Override)];
        assert!(override_active_today(&s, "back_yard", 3));
        assert!(override_active_today(&s, "back_yard", 6));
        assert!(!override_active_today(&s, "back_yard", 4));
        assert!(!override_active_today(&s, "front_yard", 3));
    }

    #[test]
    fn override_active_ignores_floor_mode() {
        let s = vec![sched("a", "back_yard", vec![3], ManualMode::Floor)];
        assert!(!override_active_today(&s, "back_yard", 3));
    }

    #[test]
    fn override_active_ignores_disabled() {
        let mut s = vec![sched("a", "back_yard", vec![3], ManualMode::Override)];
        s[0].enabled = false;
        assert!(!override_active_today(&s, "back_yard", 3));
    }
}
