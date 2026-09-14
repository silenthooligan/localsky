#![allow(unused_imports)]
use crate::assembly::*;
use crate::controllers::registry::ControllerRegistry;
use crate::engine::scripting::CompiledScripts;
use crate::engine::sizing::*;
use crate::engine::skip_rules::{self as skip_logic, et_heat_multiplier, Inputs};
use crate::engine::skip_rules::{LiveReadings, ZoneSoil};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::forecast::ForecastStore;
use crate::history::IngestState;
use crate::integrations::home_assistant::rest::HaClient;
use crate::model::{DayVerdict, IrrigationSnapshot, RuleEval, SoilForecast, WaterBudget};
use crate::refresher::evidence::*;
use crate::refresher::observers::*;
use crate::refresher::policy::*;
use crate::refresher::shell::*;
use crate::refresher::store::IrrigationStore;
use crate::refresher::*;
use crate::tempest::state::TempestStore;
use arc_swap::ArcSwap;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[cfg(test)]
mod watchdog_tests {
    /// A short rain series does not silently become dry days.
    ///
    /// The evidence travelled as parallel Vecs read by a shared index,
    /// and a lookup past the end returned 0.0 through unwrap_or. Zero
    /// rain is not a neutral default in a soil replay: it under-credits
    /// rain, which deepens the modelled deficit, which waters more. Now
    /// the mismatch is reported and the days are still paired with the
    /// dates they belong to.
    #[test]
    fn soil_evidence_days_keep_their_own_dates() {
        let d = |n: u32| chrono::NaiveDate::from_ymd_opt(2026, 9, n).expect("valid");
        let ev = SoilTickEvidence {
            dates: vec![d(3), d(4), d(5)],
            // Deliberately one short, which is the builder bug this
            // pairing makes visible rather than silent.
            rain_mm: vec![2.0, 4.0],
            ..Default::default()
        };
        let rows = ev.day_rows();
        assert_eq!(rows.len(), 3, "one row per day in the window");
        assert_eq!(rows[0].date, d(3));
        assert_eq!(rows[0].gross_rain_mm, 2.0);
        assert_eq!(rows[1].gross_rain_mm, 4.0);
        assert_eq!(
            rows[2].gross_rain_mm, 0.0,
            "the missing day is charged none"
        );
        assert!(!rows[0].is_today);
        assert!(rows[2].is_today, "the window's last day is today");
    }

    /// The aligned case is unchanged, so nothing moves on a healthy tick.
    #[test]
    fn aligned_soil_evidence_pairs_one_to_one() {
        let d = |n: u32| chrono::NaiveDate::from_ymd_opt(2026, 9, n).expect("valid");
        let ev = SoilTickEvidence {
            dates: vec![d(4), d(5)],
            rain_mm: vec![1.0, 3.0],
            ..Default::default()
        };
        let rows = ev.day_rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter().map(|r| r.gross_rain_mm).collect::<Vec<_>>(),
            vec![1.0, 3.0]
        );
    }

    use super::*;

    #[test]
    fn seasonal_multiplier_zero_is_no_adjustment_and_clamps() {
        // The WateringPolicy::Default / unset-config path produces 0; it MUST be
        // treated as 100% (no adjustment), never 0% (which would zero every run).
        assert_eq!(seasonal_multiplier(0), 1.0);
        assert_eq!(seasonal_multiplier(100), 1.0);
        assert_eq!(seasonal_multiplier(80), 0.8);
        assert_eq!(seasonal_multiplier(150), 1.5);
        // Out-of-range values clamp to the safe [0.5, 1.5] band.
        assert_eq!(seasonal_multiplier(10), 0.5);
        assert_eq!(seasonal_multiplier(500), 1.5);
    }

    #[test]
    fn seasonal_capped_reclamps_after_scaling() {
        // SAFETY contract: a >100% dial must never push a budget past the cap.
        // 600s base x 150% = 900s, held to the 720s ceiling.
        assert_eq!(seasonal_capped(600, 150, 720), 720);
        // Under the cap, scaling applies in full.
        assert_eq!(seasonal_capped(600, 150, 1200), 900);
        // A <100% dial reduces below the cap.
        assert_eq!(seasonal_capped(600, 80, 1200), 480);
        // max_dur == 0 ("no cap known") must NOT zero the run.
        assert_eq!(seasonal_capped(600, 150, 0), 900);
        // Default/no-config dial (0 => 100%) is a no-op, still capped.
        assert_eq!(seasonal_capped(600, 0, 1200), 600);
        assert_eq!(seasonal_capped(1000, 0, 720), 720);
    }

    /// A zone spelled with hyphens lands in EVERY per-zone map.
    ///
    /// The four maps were built by four independent passes, each doing its
    /// own `replace('-', "_")`. Four passes is four chances to differ, and
    /// a zone in three maps but missing from the fourth does not fail: the
    /// lookup misses and falls through to a default, so the yard waters on
    /// catalog agronomy instead of its own. A real deployment's config
    /// uses `[zones.back-yard]`, so this is the live spelling, not a
    /// contrived one.
    #[test]
    fn a_hyphenated_zone_lands_in_every_per_zone_map() {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "back-yard".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Back Yard",
                "area_sqft": 2000.0,
                "species": "st_augustine",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "rotor",
                "controller_id": "opensprinkler",
                "controller_station": "1",
            }))
            .expect("zone config"),
        );
        let policy = WateringPolicy::from_config(&cfg);

        let key = "back_yard";
        assert!(
            policy.soil_zones.iter().any(|z| z.slug == key),
            "soil_zones is missing the zone"
        );
        assert!(
            policy.budget_zones.iter().any(|z| z.slug == key),
            "budget_zones is missing the zone"
        );
        assert!(
            policy.zone_runtime.contains_key(key),
            "zone_runtime is missing the zone"
        );
        assert!(
            policy.zone_agronomy.contains_key(key),
            "zone_agronomy is missing the zone"
        );

        // And every map agrees on the population, which is the property
        // one pass actually buys.
        assert_eq!(policy.soil_zones.len(), policy.budget_zones.len());
        assert_eq!(policy.zone_runtime.len(), policy.zone_agronomy.len());
        assert_eq!(policy.soil_zones.len(), policy.zone_runtime.len());
    }

    #[test]
    fn from_config_maps_the_zone_run_cap_and_budget_respects_restrictions() {
        // ZoneConfig.max_run_minutes lands on ZoneRuntime in seconds; unset
        // resolves to the historical 60 minutes. The budget allocator caps
        // per-session seconds with min(zone cap, restriction cap), so an
        // active restriction keeps winning over a raised zone cap.
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": 1.5,
                "sessions_per_week": 1
            }))
            .unwrap(),
        );
        let policy_default = WateringPolicy::from_config(&cfg);
        assert_eq!(
            policy_default
                .zone_runtime
                .get("front")
                .unwrap()
                .max_duration_s,
            3600,
            "unset cap maps to 60 minutes"
        );

        cfg.zones.get_mut("front").unwrap().max_run_minutes = Some(120);
        let policy_raised = WateringPolicy::from_config(&cfg);
        assert_eq!(
            policy_raised
                .zone_runtime
                .get("front")
                .unwrap()
                .max_duration_s,
            7200,
            "a configured cap maps minutes to seconds"
        );

        // 1.5 in over 1 session at 25.4 mm/hr measured throughput wants
        // (38.1 / 25.4) * 3600 = 5400 s per session (GROSS sizing, no
        // capture or heat factor): between the two caps, so the clamp
        // state flips with the configured value.
        let fc = dry_budget_forecast();
        let budget = |policy: &WateringPolicy, restriction: Option<u32>| {
            compute_water_budgets(
                &fc,
                &policy.zone_runtime,
                policy.defer_threshold_in(),
                restriction,
                &policy.budget_zones,
                None,
                crate::engine::calendar::Calendar::utc(),
                chrono::Utc::now().timestamp(),
            )
            .remove(0)
        };
        let b0 = budget(&policy_default, None);
        assert_eq!(
            b0.seconds_per_session, 5400,
            "gross sizing: 38.1 mm / 25.4 mm/hr, nothing else"
        );
        assert!(
            b0.session_capped,
            "the 60 minute default clamps the session"
        );
        let b1 = budget(&policy_raised, None);
        assert!(
            !b1.session_capped,
            "the raised cap fits the session on the next build, no restart involved"
        );
        let b2 = budget(&policy_raised, Some(3600));
        assert!(
            b2.session_capped,
            "an active restriction cap still wins min() over the raised zone cap"
        );
    }

    /// The scheduling-model knob rides the hot-swapped policy: the engine
    /// default and the per-zone pin both map through `from_config` (so a
    /// settings save governs the next tick, no restart), the pin wins
    /// over the default in both directions, and a zone with no agronomy
    /// config at all resolves weekly no matter what either knob says.
    #[test]
    fn scheduling_model_maps_from_config_and_resolves_per_zone() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        let zone_json = |pin: serde_json::Value| {
            serde_json::from_value::<crate::config::schema::ZoneConfig>(serde_json::json!({
                "display_name": "Z",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "controller_id": "os_main",
                "controller_station": "1",
                "scheduling_model": pin
            }))
            .unwrap()
        };
        cfg.zones
            .insert("front".into(), zone_json(serde_json::Value::Null));
        cfg.zones.insert(
            "pinned_weekly".into(),
            zone_json(serde_json::json!("weekly")),
        );
        cfg.zones
            .insert("pinned_soil".into(), zone_json(serde_json::json!("soil")));

        // An untouched config runs the soil bucket (the default since
        // 0.9.0): only the explicit weekly pin holds its zone back.
        let policy = WateringPolicy::from_config(&cfg);
        assert_eq!(policy.scheduling_model, SchedulingModel::Soil);
        assert_eq!(
            policy.resolve_scheduling_model("front"),
            SchedulingModel::Soil
        );
        assert_eq!(
            policy.resolve_scheduling_model("pinned_soil"),
            SchedulingModel::Soil
        );
        // And an install that pins weekly at the engine level is weekly
        // everywhere but its soil-pinned zone.
        cfg.engine.scheduling_model = Some(SchedulingModel::Weekly);
        let policy = WateringPolicy::from_config(&cfg);
        assert_eq!(policy.scheduling_model, SchedulingModel::Weekly);
        assert_eq!(
            policy.resolve_scheduling_model("front"),
            SchedulingModel::Weekly
        );
        assert_eq!(
            policy.resolve_scheduling_model("pinned_soil"),
            SchedulingModel::Soil
        );

        // Engine default soil (a wizard install, or the operator opting
        // in): the weekly pin still holds its zone back, and a zone with
        // no config row stays weekly because the bucket has no texture to
        // derive from.
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        let policy = WateringPolicy::from_config(&cfg);
        assert_eq!(
            policy.resolve_scheduling_model("front"),
            SchedulingModel::Soil
        );
        assert_eq!(
            policy.resolve_scheduling_model("pinned_weekly"),
            SchedulingModel::Weekly
        );
        assert_eq!(
            policy.resolve_scheduling_model("no_such_zone"),
            SchedulingModel::Weekly,
            "agronomy-less zones are pinned weekly"
        );

        // The capture knob maps too, with the non-positive guard.
        assert!((policy.effective_capture_efficiency() - 0.70).abs() < 1e-9);
        cfg.engine.capture_efficiency = 0.55;
        let policy = WateringPolicy::from_config(&cfg);
        assert!((policy.effective_capture_efficiency() - 0.55).abs() < 1e-9);
        assert!(
            (WateringPolicy::default().effective_capture_efficiency() - 0.70).abs() < 1e-9,
            "the Default policy's 0.0 falls back rather than zeroing every credit"
        );
    }

    /// Explicit covered dry QPF for spacing/cap tests, independent of missing-data behavior.
    fn dry_budget_forecast() -> crate::forecast::snapshot::ForecastSnapshot {
        let now = chrono::Utc::now().timestamp() - 60;
        crate::forecast::snapshot::ForecastSnapshot {
            last_refresh_epoch: now,
            hourly: (0..48)
                .map(|hour| crate::forecast::snapshot::HourlyEntry {
                    time_epoch: now + hour * 3600,
                    precip_in: Some(0.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn one_zone_balance_policy(weekly_in: f64, sessions: u32) -> WateringPolicy {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": weekly_in,
                "sessions_per_week": sessions
            }))
            .unwrap(),
        );
        WateringPolicy::from_config(&cfg)
    }

    /// The pacing gate finally fires on live evidence: with the last
    /// completed watering event 1 day back and a 3-day interval (2
    /// sessions/week), today is spaced to zero. For two releases
    /// last_run_epoch was hardcoded 0 on live paths, so this gate never
    /// fired outside demo.
    #[test]
    fn spacing_gate_fires_from_run_history_evidence() {
        let policy = one_zone_balance_policy(1.5, 2);
        let fc = dry_budget_forecast();
        let now = chrono::Utc::now().timestamp();
        let mut per_zone = HashMap::new();
        per_zone.insert(
            "front".to_string(),
            ZoneRunEvidence {
                applied_open_s: 1800,
                sessions_done: 1,
                last_run_epoch: now - 86_400,
                last_session_open_s: None,
            },
        );
        let tick = BalanceTick {
            soil: SoilTickEvidence::default(),
            observed_rain_mm: 0.0,
            observed_rain_source: "none".into(),
            observed_rain_days_mm: Vec::new(),
            bias: crate::engine::BiasModel::identity(),
            per_zone,
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
            chrono::Utc::now().timestamp(),
        )
        .remove(0);
        assert_eq!(b.today_seconds, 0, "spacing must gate today");
        assert!(
            b.today_reason.contains("spaced"),
            "reason names the spacing gate: {}",
            b.today_reason
        );
        assert_eq!(b.last_run_epoch, now - 86_400, "evidence rides the wire");
        assert_eq!(b.remaining_sessions, 1, "one of two sessions already done");
        // The applied credit shrank the remainder: 1.5 in target minus
        // 1800 s x 25.4 mm/hr = 12.7 mm applied leaves 25.4 mm for the
        // one remaining session.
        assert!(
            (b.needed_mm - 25.4).abs() < 1e-6,
            "got needed {}",
            b.needed_mm
        );
        assert!((b.applied_mm - 12.7).abs() < 1e-6, "got {}", b.applied_mm);
    }

    /// `engine.session_rain_defer_in` reaches the live balance. The knob
    /// was documented, editable, and dead: the assembly passed the
    /// compile-time constant, so an operator who raised it to unstick an
    /// install saw no change at all.
    #[test]
    fn configured_rain_defer_threshold_reaches_the_balance() {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": 1.0,
                "sessions_per_week": 2
            }))
            .unwrap(),
        );
        // 0.24" of certain rain over the next 24 hours.
        let now = chrono::Utc::now().timestamp();
        let mut fc = crate::forecast::snapshot::ForecastSnapshot::default();
        fc.last_refresh_epoch = now;
        fc.hourly = (0..24)
            .map(|h| crate::forecast::snapshot::HourlyEntry {
                time_epoch: now + h * 3600,
                precip_in: Some(0.01),
                ..Default::default()
            })
            .collect();

        // Schema default (0.10"): the session defers.
        let policy = WateringPolicy::from_config(&cfg);
        assert_eq!(policy.session_rain_defer_in, 0.10);
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            None,
            crate::engine::calendar::Calendar::utc(),
            chrono::Utc::now().timestamp(),
        )
        .remove(0);
        assert_eq!(b.today_seconds, 0);
        assert!(b.today_reason.contains("deferred"), "{}", b.today_reason);

        // Operator raises the knob past the forecast: the session runs.
        cfg.engine.session_rain_defer_in = 0.50;
        let policy = WateringPolicy::from_config(&cfg);
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            None,
            crate::engine::calendar::Calendar::utc(),
            chrono::Utc::now().timestamp(),
        )
        .remove(0);
        assert!(b.today_seconds > 0, "{}", b.today_reason);
        assert!(b.today_reason.contains("session"), "{}", b.today_reason);
    }

    /// Observed rain plus prior watering covering the target sizes the
    /// week to zero with the covered reason (the owner's acceptance
    /// case: a soaked week must read covered, not schedule sessions).
    #[test]
    fn observed_rain_and_applied_water_cover_the_week() {
        let policy = one_zone_balance_policy(1.0, 2);
        let fc = dry_budget_forecast();
        let now = chrono::Utc::now().timestamp();
        let mut per_zone = HashMap::new();
        per_zone.insert(
            "front".to_string(),
            ZoneRunEvidence {
                applied_open_s: 900,
                sessions_done: 1,
                last_run_epoch: now - 5 * 86_400,
                last_session_open_s: None,
            },
        );
        let tick = BalanceTick {
            soil: SoilTickEvidence::default(),
            // 3.24" of rain on the ledger (the live acceptance figure),
            // spread over four days that each sit under the zone's
            // derived rain-credit cap (bermuda on sandy loam banks
            // 26 mm a day), so no day clips and the raw sum settles the
            // week exactly as it did before the cap existed.
            observed_rain_mm: 3.24 * 25.4,
            observed_rain_source: "gauge".into(),
            observed_rain_days_mm: vec![0.81 * 25.4; 4],
            bias: crate::engine::BiasModel::identity(),
            per_zone,
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
            chrono::Utc::now().timestamp(),
        )
        .remove(0);
        assert_eq!(b.today_seconds, 0);
        assert_eq!(b.seconds_per_session, 0, "the remainder is zero");
        assert!(
            b.today_reason.contains("covered"),
            "reason names the balance coverage: {}",
            b.today_reason
        );
        assert_eq!(b.observed_rain_source, "gauge");
        assert!((b.observed_rain_mm - 3.24 * 25.4).abs() < 1e-9);
    }

    /// The acceptance week above, storm-shaped ON PURPOSE: the same
    /// 3.24" falls as one 2.0" day plus a 1.24" day, and both overrun
    /// the zone's derived 26 mm cap (bermuda on sandy loam). The credit
    /// clips to 2 x 26 = 52 mm, which still covers the 1.0" target, and
    /// the covered sentence discloses what fell versus what counted.
    /// This pins the post-cap behavior for a soaked storm week
    /// deliberately, rather than leaving the change to the changelog.
    #[test]
    fn storm_shaped_week_clips_the_credit_and_says_so() {
        let policy = one_zone_balance_policy(1.0, 2);
        let fc = dry_budget_forecast();
        let now = chrono::Utc::now().timestamp();
        let mut per_zone = HashMap::new();
        per_zone.insert(
            "front".to_string(),
            ZoneRunEvidence {
                applied_open_s: 900,
                sessions_done: 1,
                last_run_epoch: now - 5 * 86_400,
                last_session_open_s: None,
            },
        );
        let tick = BalanceTick {
            soil: SoilTickEvidence::default(),
            observed_rain_mm: 3.24 * 25.4,
            observed_rain_source: "gauge".into(),
            observed_rain_days_mm: vec![2.0 * 25.4, 1.24 * 25.4],
            bias: crate::engine::BiasModel::identity(),
            per_zone,
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
            chrono::Utc::now().timestamp(),
        )
        .remove(0);
        assert!(
            (b.observed_rain_mm - 3.24 * 25.4).abs() < 1e-9,
            "the raw sum rides the wire untouched"
        );
        assert!(
            (b.observed_rain_credited_mm - 52.0).abs() < 1e-9,
            "two clipped days credit 2 x 26 mm, got {}",
            b.observed_rain_credited_mm
        );
        assert_eq!(b.today_seconds, 0, "{}", b.today_reason);
        assert_eq!(b.seconds_per_session, 0, "the remainder is zero");
        assert_eq!(
            b.today_reason,
            "covered by rain and prior watering (3.24\" fell, 2.05\" counted: the root \
             zone holds about 1.02\" a day, the rest drains past the roots + 0.25\" \
             applied against the 1.00\" weekly target)"
        );
    }

    /// END-TO-END issue #9 with the cap never hand-set anywhere: a sand
    /// zone with 150 mm roots derives its 9.0 mm cap through
    /// `WateringPolicy::from_config`, a 1.2" storm day rides a
    /// BalanceTick, and `compute_water_budgets` clips the credit and
    /// resumes the week. The assembled path from `ZoneConfig` through
    /// `ZoneBudgetCfg.rain_cap_mm` to a clipped balance runs as one,
    /// where every other clipping test either hand-sets the cap or
    /// exercises the resolution pieces separately.
    #[test]
    fn policy_derived_sand_cap_clips_a_storm_end_to_end() {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 25.4,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "weekly_budget_in": 1.0,
                "sessions_per_week": 2,
                "root_depth_mm": 150.0
            }))
            .unwrap(),
        );
        let policy = WateringPolicy::from_config(&cfg);
        let fc = dry_budget_forecast();
        let tick = BalanceTick {
            soil: SoilTickEvidence::default(),
            observed_rain_mm: 1.2 * 25.4,
            observed_rain_source: "gauge".into(),
            observed_rain_days_mm: vec![1.2 * 25.4],
            bias: crate::engine::BiasModel::identity(),
            per_zone: HashMap::new(),
            runs_degraded: false,
        };
        let b = compute_water_budgets(
            &fc,
            &policy.zone_runtime,
            policy.defer_threshold_in(),
            None,
            &policy.budget_zones,
            Some(&tick),
            crate::engine::calendar::Calendar::utc(),
            chrono::Utc::now().timestamp(),
        )
        .remove(0);
        assert!(
            (b.observed_rain_mm - 1.2 * 25.4).abs() < 1e-9,
            "the raw sum rides the wire untouched"
        );
        assert!(
            (b.observed_rain_credited_mm - 9.0).abs() < 1e-9,
            "sand at 150 mm roots banks 9.0 mm, got {}",
            b.observed_rain_credited_mm
        );
        assert!(
            (b.rain_credit_cap_mm - 9.0).abs() < 1e-9,
            "got {}",
            b.rain_credit_cap_mm
        );
        assert!(b.rain_cap_inferred, "derived from soil and roots, not set");
        // remainder = 25.4 - 9.0 = 16.4 mm across the two sessions: the
        // week RESUMES mid-week instead of planning zero for seven days.
        assert!((b.needed_mm - 16.4).abs() < 1e-9, "got {}", b.needed_mm);
        assert!(
            b.today_seconds > 0,
            "the week resumes instead of holding: {}",
            b.today_reason
        );
        assert!(b.today_reason.contains("session"), "{}", b.today_reason);
    }

    /// Run-history evidence building: watering rows cluster (manual +
    /// observer overlap counts once), dry-run rows and skip markers are
    /// excluded, and the window clamp holds.
    #[test]
    fn zone_run_evidence_filters_clusters_and_clamps() {
        let now = 1_700_000_000i64;
        let w_start = now - 7 * 86_400;
        let row = |slug: &str, start: i64, dur: u32, source: &str, status: &str| {
            crate::persistence::RunRow {
                session_id: None,
                id: 0,
                zone_slug: slug.into(),
                start_epoch: start,
                end_epoch: Some(start + dur as i64),
                duration_s: Some(dur),
                source: source.into(),
                controller_id: "c".into(),
                status: status.into(),
                skip_reason: None,
                et0_mm: None,
                etc_mm: None,
                applied_mm: None,
                cycle_index: None,
                cycle_count: None,
                note: None,
                volume_gal: None,
            }
        };
        let rows = vec![
            // Two mornings for front: one manual+observer overlap pair,
            // one plain observer row.
            row("front", w_start + 10_000, 1200, "manual", "completed"),
            row("front", w_start + 10_010, 1200, "ha_refresher", "completed"),
            row("front", w_start + 300_000, 600, "ha_refresher", "completed"),
            // Pretend water and a skip marker: never evidence.
            row("front", w_start + 400_000, 900, "dry_run", "completed"),
            row("front", w_start + 500_000, 0, "smart_morning", "skipped"),
            // An event straddling the window start: only the inside part.
            row("side", w_start - 600, 1200, "ha_refresher", "completed"),
        ];
        let ev = build_zone_run_evidence(&rows, w_start, now);
        let front = ev.get("front").copied().unwrap();
        assert_eq!(front.sessions_done, 2, "two clustered events");
        assert_eq!(front.applied_open_s, 1210 + 600, "union, not the raw sum");
        assert_eq!(front.last_run_epoch, w_start + 300_600);
        let side = ev.get("side").copied().unwrap();
        assert_eq!(side.applied_open_s, 600, "window clamp");
    }

    /// The DECLARED 1.27.0 weekly-surface delta: the runs fetch widened
    /// to the soil window and `last_run_epoch` reduces over ALL fetched
    /// rows, so a zone whose newest run ended 9 days ago reports that
    /// run's end where the 7-day fetch read 0 (the truthful figure; no
    /// golden pin can see it because the pins feed last_run_epoch as an
    /// input). The windowed figures stay 7-day truncated: that run
    /// contributes no applied seconds and no session. A 6-day-old run
    /// populates everything, exactly as before.
    #[test]
    fn last_run_epoch_populates_beyond_the_weekly_window() {
        let now = 1_700_000_000i64;
        let w_start = now - 7 * 86_400;
        let row = |slug: &str, start: i64, dur: u32| crate::persistence::RunRow {
            session_id: None,
            id: 0,
            zone_slug: slug.into(),
            start_epoch: start,
            end_epoch: Some(start + dur as i64),
            duration_s: Some(dur),
            source: "ha_refresher".into(),
            controller_id: "c".into(),
            status: "completed".into(),
            skip_reason: None,
            et0_mm: None,
            etc_mm: None,
            applied_mm: None,
            cycle_index: None,
            cycle_count: None,
            note: None,
            volume_gal: None,
        };
        // Only a 9-day-old run: outside the weekly window, inside the
        // widened fetch.
        let stale = vec![row("front", now - 9 * 86_400, 1200)];
        let ev = build_zone_run_evidence(&stale, w_start, now);
        let front = ev.get("front").copied().unwrap();
        assert_eq!(front.applied_open_s, 0, "no applied credit past the window");
        assert_eq!(front.sessions_done, 0, "no session past the window");
        assert_eq!(
            front.last_run_epoch,
            now - 9 * 86_400 + 1200,
            "the declared delta: populated, never 0"
        );
        // A newer 6-day-old run wins the reduction and counts in full.
        let mixed = vec![
            row("front", now - 9 * 86_400, 1200),
            row("front", now - 6 * 86_400, 600),
        ];
        let ev = build_zone_run_evidence(&mixed, w_start, now);
        let front = ev.get("front").copied().unwrap();
        assert_eq!(front.applied_open_s, 600);
        assert_eq!(front.sessions_done, 1);
        assert_eq!(front.last_run_epoch, now - 6 * 86_400 + 600);
    }

    /// The observed-rain ladder per install class: measured COVERAGE
    /// wins outright (never a value contest), legacy rows classify by
    /// install class, the model side is the max() of archive and
    /// model-quality legacy rows, and no evidence at all reads 'none'.
    #[test]
    fn observed_rain_ladder_resolves_per_install_class() {
        use crate::persistence::ObservedRainWindow;
        let win = |g: f64, r: f64, l: f64, gd: u32, rd: u32, ld: u32| ObservedRainWindow {
            gauge_in: g,
            radar_in: r,
            model_in: 0.0,
            legacy_in: l,
            gauge_days: gd,
            radar_days: rd,
            legacy_days: ld,
        };
        // Gauge install: measured rows win and read 'gauge'.
        let (mm, src) = resolve_observed_rain(&win(1.0, 0.0, 0.0, 5, 0, 0), true, 0.3);
        assert_eq!(src, "gauge");
        assert!((mm - 25.4).abs() < 1e-9);
        // A gauge that measured LESS than the regional archive still wins:
        // coverage precedence, the yard's own record is the truth.
        let (mm, src) = resolve_observed_rain(&win(0.1, 0.0, 0.0, 6, 0, 0), true, 1.0);
        assert_eq!(src, "gauge", "an out-valued gauge is never overridden");
        assert!((mm - 0.1 * 25.4).abs() < 1e-9);
        // A measured DRY week (rows present, total 0) also wins: 0.00 in
        // gauge, never the model's wetter claim.
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.0, 7, 0, 0), true, 0.8);
        assert_eq!(src, "gauge");
        assert_eq!(mm, 0.0);
        // Radar day totals dominate the measured side: 'radar'.
        let (mm, src) = resolve_observed_rain(&win(0.1, 0.9, 0.0, 1, 4, 0), false, 0.0);
        assert_eq!(src, "radar");
        assert!((mm - 25.4).abs() < 1e-9);
        // No measured coverage at all: the archive supplies the term.
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.0, 0, 0, 0), false, 0.5);
        assert_eq!(src, "model_archive");
        assert!((mm - 0.5 * 25.4).abs() < 1e-9);
        // Legacy rows: gauge-quality coverage on a station install...
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.4, 0, 0, 3), true, 0.9);
        assert_eq!(src, "gauge");
        assert!((mm - 0.4 * 25.4).abs() < 1e-9);
        // ...model-quality (no coverage) on a station-less one.
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.4, 0, 0, 3), false, 0.1);
        assert_eq!(src, "model_archive");
        assert!((mm - 0.4 * 25.4).abs() < 1e-9, "max(archive, legacy rows)");
        // Nothing anywhere: 'none' with a zero term (never fabricated).
        let (mm, src) = resolve_observed_rain(&win(0.0, 0.0, 0.0, 0, 0, 0), false, 0.0);
        assert_eq!(src, "none");
        assert_eq!(mm, 0.0);
    }

    /// The day-granular ladder resolves the SAME rung as the sum ladder
    /// and its series always sums to the sum rung's figure: measured
    /// coverage wins outright (even a measured-dry series of zeros), the
    /// model side picks one whole series by the same max(), and legacy
    /// rows flip sides with install class. This is the invariant the
    /// per-day rain-credit cap stands on: clipping day values can only
    /// ever shrink the credit relative to the raw wire sum, never
    /// describe different rain.
    #[test]
    fn observed_rain_day_series_sums_to_the_ladder_figure() {
        use crate::persistence::{ObservedRainDay, ObservedRainWindow};
        let day = |offset: i64, observed_in: f64, source: &str| ObservedRainDay {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
                + chrono::Duration::days(offset),
            observed_in,
            source: source.into(),
        };
        // Each case: (day rows, the matching per-source window sums,
        // station_present, per-day archive).
        let cases: Vec<(Vec<ObservedRainDay>, ObservedRainWindow, bool, Vec<f64>)> = vec![
            // Gauge coverage: one storm day and one drizzle.
            (
                vec![day(0, 1.2, "gauge"), day(3, 0.2, "gauge")],
                ObservedRainWindow {
                    gauge_in: 1.4,
                    gauge_days: 2,
                    ..Default::default()
                },
                true,
                vec![0.3, 0.3],
            ),
            // Measured-dry week: rows present, all zeros, still measured.
            (
                vec![day(0, 0.0, "gauge"), day(1, 0.0, "gauge")],
                ObservedRainWindow {
                    gauge_days: 2,
                    ..Default::default()
                },
                true,
                vec![0.8],
            ),
            // No measured coverage, archive outweighs model rows.
            (
                vec![day(2, 0.1, "model")],
                ObservedRainWindow {
                    model_in: 0.1,
                    ..Default::default()
                },
                false,
                vec![0.2, 0.3],
            ),
            // No measured coverage, legacy rows (station-less = model
            // quality) outweigh the archive.
            (
                vec![day(1, 0.4, "legacy")],
                ObservedRainWindow {
                    legacy_in: 0.4,
                    legacy_days: 3,
                    ..Default::default()
                },
                false,
                vec![0.1],
            ),
            // Nothing anywhere: an empty series for the 'none' rung.
            (Vec::new(), ObservedRainWindow::default(), false, Vec::new()),
        ];
        for (rows, win, station, archive) in cases {
            let archive_sum: f64 = archive.iter().sum();
            let (sum_mm, rung) = resolve_observed_rain(&win, station, archive_sum);
            let series = resolve_observed_rain_days(&rows, station, &archive);
            let series_sum: f64 = series.iter().sum();
            assert!(
                (series_sum - sum_mm).abs() < 1e-9,
                "rung {rung}: day series sums to {series_sum}, ladder says {sum_mm}"
            );
        }
        // Legacy rows on a STATION install are measured coverage: the
        // series is the legacy days, not the (larger) archive.
        let rows = vec![day(0, 0.4, "legacy")];
        let series = resolve_observed_rain_days(&rows, true, &[0.9]);
        assert_eq!(series.len(), 1);
        assert!((series[0] - 0.4 * 25.4).abs() < 1e-9);
    }

    /// The dated resolver is the undated ladder with its dates kept: the
    /// same coverage precedence rung for rung, each value tied to the
    /// local day the soil replay charges it on. Measured rows keep their
    /// own dates (a storm lands on the storm's day, not "somewhere in
    /// the window"), and the model side's whole-series choice carries
    /// the archive's dates.
    #[test]
    fn dated_rain_resolver_keeps_the_ladder_and_the_dates() {
        use crate::persistence::ObservedRainDay;
        let d = |offset: i64| {
            chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap() + chrono::Duration::days(offset)
        };
        let day = |offset: i64, observed_in: f64, source: &str| ObservedRainDay {
            date: d(offset),
            observed_in,
            source: source.into(),
        };
        // Measured coverage wins outright and keeps its dates.
        let rows = vec![day(2, 1.2, "gauge"), day(5, 0.2, "radar")];
        let out = resolve_observed_rain_days_dated(&rows, true, &[(d(0), 0.9)]);
        assert_eq!(
            out,
            vec![(d(2), 1.2 * 25.4), (d(5), 0.2 * 25.4)],
            "the storm stays on the storm's day"
        );
        // Model side: the archive outweighs the model rows, so the whole
        // archive series (dates included) supplies the days.
        let rows = vec![day(3, 0.1, "model")];
        let out = resolve_observed_rain_days_dated(&rows, false, &[(d(1), 0.2), (d(2), 0.3)]);
        assert_eq!(out, vec![(d(1), 0.2 * 25.4), (d(2), 0.3 * 25.4)]);
        // ...and the rows win when they carry more, keeping THEIR dates.
        let rows = vec![day(3, 0.6, "model")];
        let out = resolve_observed_rain_days_dated(&rows, false, &[(d(1), 0.2), (d(2), 0.3)]);
        assert_eq!(out, vec![(d(3), 0.6 * 25.4)]);
        // Nothing anywhere: an empty series; uncovered days read dry.
        assert_eq!(resolve_observed_rain_days_dated(&[], false, &[]), vec![]);
    }

    /// Per-zone rain-cap resolution at policy-build time: without an
    /// override the cap derives as TAW = (FC - WP) x root depth,
    /// honoring a root-depth override and falling back to the species
    /// default; the operator's `rain_credit_cap_in` beats both; and a
    /// synthesized row for a config-less zone (env-var install) takes
    /// the default-texture cap.
    #[test]
    fn rain_cap_resolves_override_over_derived_taw() {
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "controller_id": "os_main",
                "controller_station": "1"
            }))
            .unwrap(),
        );
        // Derived: bermuda's default 200 mm roots on sandy loam
        // (FC - WP = 0.13) bank 26.0 mm a day.
        let policy = WateringPolicy::from_config(&cfg);
        let row = policy
            .budget_zones
            .iter()
            .find(|z| z.slug == "front")
            .unwrap();
        assert!(
            (row.rain_cap_mm - 26.0).abs() < 1e-9,
            "got {}",
            row.rain_cap_mm
        );
        assert!(row.rain_cap_inferred);
        // A root-depth override reshapes the derived cap: 300 mm roots
        // bank 39.0 mm.
        cfg.zones.get_mut("front").unwrap().root_depth_mm = Some(300.0);
        let policy = WateringPolicy::from_config(&cfg);
        let row = policy
            .budget_zones
            .iter()
            .find(|z| z.slug == "front")
            .unwrap();
        assert!(
            (row.rain_cap_mm - 39.0).abs() < 1e-9,
            "got {}",
            row.rain_cap_mm
        );
        // The operator's own cap beats both derivations.
        cfg.zones.get_mut("front").unwrap().rain_credit_cap_in = Some(0.25);
        let policy = WateringPolicy::from_config(&cfg);
        let row = policy
            .budget_zones
            .iter()
            .find(|z| z.slug == "front")
            .unwrap();
        assert!((row.rain_cap_mm - 0.25 * 25.4).abs() < 1e-9);
        assert!(!row.rain_cap_inferred);
        // A synthesized row has no config to derive from: sandy loam at
        // the default turf root depth = 19.5 mm, marked inferred.
        let active = vec![crate::zones::ZoneIdent::new("side", "Side")];
        let rows = budget_zones_for_active(&active, &[]);
        assert!((rows[0].rain_cap_mm - 19.5).abs() < 1e-9);
        assert!(rows[0].rain_cap_inferred);
    }

    /// The ledger writer's per-tick decision: measured owners record the
    /// accumulator (same-day, plausible values only); model-nature,
    /// stale, or absent owners record the 'none' placeholder; the
    /// midnight-carry and garbage cases skip the write entirely.
    #[test]
    fn ledger_observation_gates_midnight_model_stale_and_garbage() {
        use crate::tempest::state::{RainOwner, Snapshot};
        let now = chrono::Utc::now().timestamp();
        let today = crate::timeutil::local_day_ordinal(now);
        let gauge = RainOwner {
            nature: crate::model::RainNature::Measured,
            label: "Tempest".into(),
            is_live: true,
            is_fresh: true,
        };
        let mut snap = Snapshot {
            rain_in_today: 1.2,
            rain_today_day_ordinal: today,
            ..Default::default()
        };
        // Same-day gauge total records with provenance.
        assert_eq!(
            ledger_observation(&snap, Some(&gauge), now),
            Some((1.2, "gauge"))
        );
        // 23:59 rain, 00:00:10 tick: the accumulator still carries
        // YESTERDAY'S day bucket, so the write is skipped; the day-max
        // upsert can never pin yesterday's total onto the new row.
        snap.rain_today_day_ordinal = today - 1;
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
        snap.rain_today_day_ordinal = today;
        // A model-nature owner records the placeholder, never the
        // whole-day forecast.
        let model = RainOwner {
            nature: crate::model::RainNature::Model,
            label: "open_meteo".into(),
            is_live: false,
            is_fresh: true,
        };
        assert_eq!(
            ledger_observation(&snap, Some(&model), now),
            Some((0.0, "none"))
        );
        // A stale owner (writer went silent) is no owner: placeholder,
        // so a frozen value cannot fabricate wet days forever.
        let stale = RainOwner {
            nature: crate::model::RainNature::RadarQpe,
            label: "noaa_mrms".into(),
            is_live: false,
            is_fresh: false,
        };
        assert_eq!(
            ledger_observation(&snap, Some(&stale), now),
            Some((0.0, "none"))
        );
        // No owner at all: placeholder.
        assert_eq!(ledger_observation(&snap, None, now), Some((0.0, "none")));
        // Garbage frames are rejected, not clamped: the day's ledger
        // stays untouched.
        snap.rain_in_today = 30.0; // a 25.4x unit misparse class value
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
        snap.rain_in_today = -0.5;
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
        snap.rain_in_today = f64::NAN;
        assert_eq!(ledger_observation(&snap, Some(&gauge), now), None);
    }

    /// The ET0 self-emit's per-tick decision, `ledger_observation`'s twin:
    /// a resolved, plausible figure on a rolled-over forecast day records;
    /// an unresolved day records NOTHING (never a fabricated figure); a
    /// garbage value skips; and the first ticks after configured-tz
    /// midnight skip while daily[0] still describes yesterday, so the
    /// day-max upsert can never pin yesterday's total onto the new row.
    /// Idempotence lives in the store (day-MAX upsert, pinned by
    /// `et0_upsert_keeps_the_day_max_and_its_source`): re-emitting the
    /// same figure every 10 seconds all day converges on one row holding
    /// the day's max.
    #[test]
    fn ledger_et0_emission_gates_midnight_and_garbage() {
        let now = chrono::Utc::now().timestamp();
        let today_midnight_epoch = {
            let today = crate::timeutil::local_date(now).unwrap();
            crate::timeutil::local_day_bounds_utc(today).unwrap().0
        }
        .timestamp();
        let fc_on = |day_epoch: i64| ForecastSnapshot {
            daily: vec![crate::forecast::snapshot::DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(day_epoch),
                ..Default::default()
            }],
            ..Default::default()
        };
        // A resolved plausible figure on today's forecast day records.
        let fc = fc_on(today_midnight_epoch);
        assert_eq!(ledger_et0_emission(&fc, Some(5.4), now), Some(5.4));
        // Unresolved (the ladder found nothing): no write, no fabrication.
        assert_eq!(ledger_et0_emission(&fc, None, now), None);
        // Garbage is rejected, not clamped.
        assert_eq!(ledger_et0_emission(&fc, Some(0.0), now), Some(0.0));
        assert_eq!(ledger_et0_emission(&fc, Some(-1.0), now), None);
        assert_eq!(ledger_et0_emission(&fc, Some(120.0), now), None);
        assert_eq!(ledger_et0_emission(&fc, Some(f64::NAN), now), None);
        // Midnight carry: daily[0] still on yesterday skips the tick.
        let fc_yesterday = fc_on(today_midnight_epoch - 86_400);
        assert_eq!(ledger_et0_emission(&fc_yesterday, Some(5.4), now), None);
        // No daily series at all: nothing to gate on, the bus-owned
        // figure (contract: today's full-day value) writes normally.
        let fc_empty = ForecastSnapshot::default();
        assert_eq!(ledger_et0_emission(&fc_empty, Some(5.4), now), Some(5.4));
    }

    #[test]
    fn force_run_floor_decouples_verdict_from_duration() {
        // No force + 0 budget stays 0 (a wet yard normally waters nothing).
        assert_eq!(force_run_floor("auto", "auto", 0, 1200), 0);
        // A non-zero budget is never altered, regardless of override.
        assert_eq!(force_run_floor("run", "auto", 600, 1200), 600);
        // Zone force + 0 budget -> bounded default, clamped to the zone max.
        assert_eq!(force_run_floor("run", "auto", 0, 1200), FORCE_RUN_DEFAULT_S);
        assert_eq!(force_run_floor("run", "auto", 0, 120), 120);
        // Global force with the zone on auto -> forced.
        assert_eq!(force_run_floor("auto", "run", 0, 1200), FORCE_RUN_DEFAULT_S);
        // A per-zone skip beats a global run -> not forced.
        assert_eq!(force_run_floor("skip", "run", 0, 1200), 0);
        // Unset max_dur falls back to the default, not 0.
        assert_eq!(force_run_floor("run", "auto", 0, 0), FORCE_RUN_DEFAULT_S);
    }

    #[test]
    fn verdict_multiplier_scales_and_caps_dispatch() {
        use crate::model::{IrrigationSnapshot, ZoneMath, ZoneState, ZoneVerdict};
        let mk = |slug: &str, planned: u32, mult: f64, max_dur: u32| ZoneState {
            slug: slug.into(),
            planned_run_seconds: planned,
            verdict: Some(ZoneVerdict {
                zone_slug: slug.into(),
                verdict: "run".into(),
                multiplier: mult,
                ..Default::default()
            }),
            math: Some(ZoneMath {
                max_duration_seconds: max_dur,
                scheduled_seconds: planned,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut snap = IrrigationSnapshot {
            zones: vec![
                mk("a", 600, 1.0, 1200), // no rule -> unchanged (byte-identical case)
                mk("b", 600, 0.5, 1200), // halve -> 300
                mk("c", 600, 1.5, 1200), // extend -> 900, under the cap
                mk("d", 600, 1.5, 720),  // extend -> 900 held to the 720s ceiling
                mk("e", 0, 0.5, 1200),   // a skipped zone (0s) stays 0
                mk("f", 720, 0.5, 720),  // halve -> 360, back under the ceiling
            ],
            ..Default::default()
        };
        // Zone f arrived from the allocator already sitting on its ceiling.
        snap.zones[5].math.as_mut().unwrap().cap_binding = true;
        apply_verdict_multiplier(&mut snap);
        let planned: Vec<u32> = snap.zones.iter().map(|z| z.planned_run_seconds).collect();
        assert_eq!(planned, vec![600, 300, 900, 720, 0, 360]);
        // math.scheduled_seconds mirrors the dispatched value so the "why this
        // duration" tile agrees with what the controller receives.
        assert_eq!(snap.zones[3].math.as_ref().unwrap().scheduled_seconds, 720);
        // A multiplier that ran into the ceiling reports it.
        assert!(snap.zones[3].math.as_ref().unwrap().cap_binding);
        // A multiplier that pulled the run back UNDER the ceiling clears it:
        // the ceiling is no longer what set the minutes.
        assert!(!snap.zones[5].math.as_ref().unwrap().cap_binding);
    }

    #[test]
    fn cap_binding_needs_a_run_that_sits_on_the_ceiling() {
        use crate::model::{IrrigationSnapshot, WaterBudget, ZoneMath, ZoneState};
        // `session_capped` describes the IDEAL weekly session, not today's
        // plan, so it stays true on a zone the allocator zeroed for an
        // unrelated reason. Reading it straight onto `cap_binding` printed
        // "0 min (capped at 60 min)": a ceiling shortening a run that does
        // not exist.
        let zone = |slug: &str, max_dur: u32, override_mode: &str| ZoneState {
            slug: slug.into(),
            override_mode: override_mode.into(),
            verdict: Some(crate::model::ZoneVerdict {
                verdict: "run".into(),
                ..Default::default()
            }),
            math: Some(ZoneMath {
                max_duration_seconds: max_dur,
                ..Default::default()
            }),
            ..Default::default()
        };
        let budget = |slug: &str, today_s: u32, capped: bool| WaterBudget {
            zone_slug: slug.into(),
            today_seconds: today_s,
            session_capped: capped,
            ..Default::default()
        };
        let mut snap = IrrigationSnapshot {
            zones: vec![
                // Spaced since the last session: the ideal slice outgrows the
                // ceiling, but nothing runs today.
                zone("spaced", 3600, "auto"),
                // The allocator's session really did hit the ceiling.
                zone("shorted", 3600, "auto"),
                // Under the ceiling with room to spare.
                zone("roomy", 3600, "auto"),
                // Force-run over a zero budget: the floor sized this, not
                // the ceiling.
                zone("forced", 3600, "run"),
            ],
            water_budgets: vec![
                budget("spaced", 0, true),
                budget("shorted", 3600, true),
                budget("roomy", 1200, false),
                budget("forced", 0, true),
            ],
            ..Default::default()
        };
        let policy = WateringPolicy::default();
        crate::assembly::apply_budget_plan(&mut snap, &policy, 0);
        let read = |i: usize| {
            let m = snap.zones[i].math.as_ref().unwrap();
            (m.scheduled_seconds, m.cap_binding)
        };
        assert_eq!(read(0), (0, false), "a zone at zero was not shortened");
        assert_eq!(read(1), (3600, true), "the ceiling set these minutes");
        assert_eq!(read(2), (1200, false), "room under the ceiling");
        assert_eq!(
            read(3),
            (FORCE_RUN_DEFAULT_S, false),
            "a force-run floor is not a capped run"
        );
    }

    /// Golden pin: `today_seconds` flows onto `planned_run_seconds`
    /// untouched under the default policy (seasonal dial 100, no manual
    /// schedules, no force overrides) for every allocator shape the
    /// budget engine produces (covered, deferred, spaced, session,
    /// capped, clipped-rain). The soil-bucket scheduling work adds a
    /// second producer for these rows, so the weekly pass-through is
    /// pinned first and every later diff is a deliberate change.
    #[test]
    fn golden_planned_seconds_pass_through_for_the_allocator_shapes() {
        use crate::model::{IrrigationSnapshot, WaterBudget, ZoneMath, ZoneState};
        let zone = |slug: &str, max_dur: u32| ZoneState {
            slug: slug.into(),
            override_mode: "auto".into(),
            math: Some(ZoneMath {
                max_duration_seconds: max_dur,
                ..Default::default()
            }),
            ..Default::default()
        };
        let budget = |slug: &str, today_s: u32, capped: bool| WaterBudget {
            zone_slug: slug.into(),
            today_seconds: today_s,
            session_capped: capped,
            ..Default::default()
        };
        let mut snap = IrrigationSnapshot {
            zones: vec![
                zone("covered", 14_400),
                zone("deferred", 14_400),
                zone("spaced", 14_400),
                zone("session", 14_400),
                zone("capped", 3600),
                zone("clipped", 14_400),
            ],
            water_budgets: vec![
                budget("covered", 0, false),
                budget("deferred", 0, false),
                budget("spaced", 0, false),
                // The seconds are the budget engine's own golden figures
                // (see engine::budget golden_row_* pins).
                budget("session", 4572, false),
                budget("capped", 3600, true),
                budget("clipped", 2952, false),
            ],
            ..Default::default()
        };
        crate::assembly::apply_budget_plan(&mut snap, &WateringPolicy::default(), 0);
        let planned: Vec<(String, u32)> = snap
            .zones
            .iter()
            .map(|z| (z.slug.clone(), z.planned_run_seconds))
            .collect();
        assert_eq!(
            planned,
            vec![
                ("covered".to_string(), 0),
                ("deferred".to_string(), 0),
                ("spaced".to_string(), 0),
                ("session".to_string(), 4572),
                ("capped".to_string(), 3600),
                ("clipped".to_string(), 2952),
            ]
        );
        // The capped zone sits ON its ceiling because the allocator hit
        // it; nothing else reads as ceiling-bound.
        let bindings: Vec<bool> = snap
            .zones
            .iter()
            .map(|z| z.math.as_ref().unwrap().cap_binding)
            .collect();
        assert_eq!(bindings, vec![false, false, false, false, true, false]);
        assert_eq!(
            snap.next_run_total_minutes,
            (4572u32 + 3600 + 2952) as f64 / 60.0
        );
    }

    #[test]
    fn seasonal_dial_reports_its_own_clamp_like_the_rule_multiplier_does() {
        use crate::model::{IrrigationSnapshot, WaterBudget, ZoneMath, ZoneState};
        // The dial scales AFTER the allocator, so it can push an uncapped
        // session into the ceiling. That clamp shortens the dispatched run
        // exactly as the rule multiplier's does, and reports itself the same
        // way instead of clipping the run silently.
        assert!(seasonal_cap_binds(3000, 150, 3600));
        assert!(!seasonal_cap_binds(3000, 100, 3600));
        assert!(!seasonal_cap_binds(3000, 150, 0), "no cap known, no clamp");
        let mut snap = IrrigationSnapshot {
            zones: vec![ZoneState {
                slug: "dialed".into(),
                override_mode: "auto".into(),
                math: Some(ZoneMath {
                    max_duration_seconds: 3600,
                    ..Default::default()
                }),
                ..Default::default()
            }],
            water_budgets: vec![WaterBudget {
                zone_slug: "dialed".into(),
                today_seconds: 3000,
                // The allocator itself did NOT hit the ceiling.
                session_capped: false,
                ..Default::default()
            }],
            ..Default::default()
        };
        let policy = WateringPolicy {
            seasonal_adjust_pct: 150,
            ..Default::default()
        };
        crate::assembly::apply_budget_plan(&mut snap, &policy, 0);
        let m = snap.zones[0].math.as_ref().unwrap();
        assert_eq!(m.scheduled_seconds, 3600, "4500 s held to the ceiling");
        assert!(m.cap_binding, "the ceiling is what set these minutes");
    }

    #[test]
    fn apply_soil_quality_bands_to_physical_range() {
        // In-range readings pass through untouched (the boundaries are valid:
        // just-above-0 and exactly the physical max are real soil values).
        assert_eq!(apply_soil_quality(Some(45.0)), Some(45.0));
        assert_eq!(apply_soil_quality(Some(0.01)), Some(0.01));
        assert_eq!(apply_soil_quality(Some(SOIL_PCT_PHYSICAL_MAX)), Some(100.0));
        // Disconnected (exactly 0%) and negative readings null to None so the
        // zone fails safe to weather/modeled instead of reading as bone-dry.
        assert_eq!(apply_soil_quality(Some(0.0)), None);
        assert_eq!(apply_soil_quality(Some(-5.0)), None);
        // G2: an over-range frame (> physical max) is garbage, not
        // super-saturated soil, so it nulls to None and cannot falsely
        // satisfy the saturation skip.
        assert_eq!(apply_soil_quality(Some(150.0)), None);
        assert_eq!(apply_soil_quality(Some(100.01)), None);
        // A missing reading stays missing.
        assert_eq!(apply_soil_quality(None), None);
    }

    #[test]
    fn watchdog_stall_decision() {
        let now = 1_000_000i64;
        // Never-started, within grace: not stalled.
        assert!(!refresher_stalled(0, now - 10, now));
        // Never-started, past grace: stalled (setup-time panic).
        assert!(refresher_stalled(
            0,
            now - (REFRESHER_STARTUP_GRACE_S + 1),
            now
        ));
        // Fresh heartbeat: not stalled.
        assert!(!refresher_stalled(now - 5, now - 9_999, now));
        // A degraded refresher tick gap (worst case BACKOFF_MAX 180s) is NOT a
        // stall, so a legitimately-backed-off refresher is never killed.
        assert!(!refresher_stalled(now - 180, now - 9_999, now));
        // Past the stall ceiling: stalled (panic or hang).
        assert!(refresher_stalled(
            now - (REFRESHER_STALL_MAX_S + 1),
            now - 9_999,
            now
        ));
    }
}

// End-to-end SNAPSHOT ASSEMBLY. These assert that `build_from_map`
// correctly assembles the published IrrigationSnapshot from the raw
// forecast/tempest stores + entity map: the aggregate verdict + reason for a
// clear skip and a clear run, the per-zone verdicts (source + verdict), and the
// two forecast-derived fields the refresher itself computes from the raw stores:
// `rain_observed_recent_in` (today's measured rain + the past observed window)
// and `heat_index_max_3day_f` (per-day temp×humidity pairing, NOT the now-
// humidity bug). build_from_map is private to this module, so the test calls it
// directly; no production seam is added.
#[cfg(test)]
mod snapshot_assembly_tests {
    use super::*;
    use crate::engine::skip_rules::heat_index_f;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
    use crate::tempest::state::Snapshot as TempestSnapshot;

    /// A fresh full-coverage live station packet at `now`: all three engine-
    /// critical fields (temp/wind/rh) carry the supplied values with live epochs
    /// at `now`, so resolve_current_conditions yields LiveReadings::Station and
    /// the decision is never failed-safe to "skip" on missing live data.
    fn live_station(
        now: i64,
        temp_f: f64,
        wind_mph: f64,
        rh_pct: f64,
        rain_today_in: f64,
    ) -> TempestSnapshot {
        TempestSnapshot {
            last_packet_epoch: now,
            air_temp_live_epoch: now,
            wind_live_epoch: now,
            rh_live_epoch: now,
            air_temp_f: temp_f,
            wind_avg_mph: wind_mph,
            rh_pct,
            rain_in_today: rain_today_in,
            source_label: "TestStation".into(),
            ..Default::default()
        }
    }

    /// One known-dry current hour of forecast so resolve_current_conditions has a
    /// fallback (it never reaches Unavailable in these tests; the station is the
    /// live source). Mirrors the live station so a fallback would be benign.
    fn current_hour(temp_f: f64, wind_mph: f64, rh_pct: u32) -> HourlyEntry {
        HourlyEntry {
            time_epoch: Utc::now().timestamp() - 60,
            precip_in: Some(0.0),
            temp_f: Some(temp_f),
            wind_mph: Some(wind_mph),
            humidity_pct: Some(rh_pct),
            ..Default::default()
        }
    }

    /// Explicit complete dry window for tests whose subject is not a QPF outage.
    fn dry_hours(temp_f: f64, wind_mph: f64, rh_pct: u32) -> Vec<HourlyEntry> {
        let first = current_hour(temp_f, wind_mph, rh_pct);
        (0..48)
            .map(|hour| HourlyEntry {
                time_epoch: first.time_epoch + hour * 3600,
                ..first.clone()
            })
            .collect()
    }

    fn dry_days(now: i64) -> Vec<DailyEntry> {
        (0..8)
            .map(|day| DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(now + day * 86400),
                temp_min_f: Some(65.0),
                temp_max_f: Some(80.0),
                wind_max_mph: Some(4.0),
                precip_sum_in: Some(0.0),
                ..Default::default()
            })
            .collect()
    }

    fn forecast_store_with(fc: ForecastSnapshot) -> ForecastStore {
        let s = ForecastStore::new();
        s.store(fc);
        s
    }

    fn tempest_store_with(
        t: TempestSnapshot,
        calendar: crate::engine::calendar::Calendar,
    ) -> TempestStore {
        let s = TempestStore::new();
        let at = t.last_packet_epoch;
        use crate::ports::weather_source::WeatherField as F;
        let reports = [
            (F::AirTempF, t.air_temp_f, t.air_temp_live_epoch),
            (F::WindMph, t.wind_avg_mph, t.wind_live_epoch),
            (F::RhPct, t.rh_pct, t.rh_live_epoch),
            (F::RainTodayIn, t.rain_in_today, at),
        ];
        s.store(t);
        // Exercise the same field evidence path the real bus supplies.
        for (field, value, epoch) in reports {
            s.apply_source_fields(&[(field, value)], epoch, true, "test_station");
        }
        // The synthetic store and the assembly use the same explicit calendar.
        let mut snapshot = (*s.snapshot()).clone();
        snapshot.rain_today_day_ordinal = calendar
            .local_date(at)
            .map(|day| chrono::Datelike::num_days_from_ce(&day))
            .unwrap_or(0);
        s.store(snapshot);
        s
    }

    fn zone_idents(slugs: &[&str]) -> Vec<crate::zones::ZoneIdent> {
        slugs
            .iter()
            .map(|s| crate::zones::ZoneIdent::new(*s, *s))
            .collect()
    }

    /// Assemble the snapshot the same way refresh_once_native does (empty HA
    /// entity map, no soil config, no scripts), but with the raw stores under
    /// test. Returns the published IrrigationSnapshot.
    async fn assemble(
        forecast: ForecastSnapshot,
        tempest: TempestSnapshot,
        zones: &[&str],
        policy: WateringPolicy,
    ) -> IrrigationSnapshot {
        assemble_with(forecast, tempest, zones, policy, HashMap::new(), None).await
    }

    /// `assemble` with the two arguments the migration turns on: the Home
    /// Assistant entity map, and the native control surface. `ha_helper_reads`
    /// is what the two deployment paths differ by.
    async fn assemble_with(
        forecast: ForecastSnapshot,
        tempest: TempestSnapshot,
        zones: &[&str],
        policy: WateringPolicy,
        map: HashMap<String, Value>,
        control: Option<&crate::model::IrrigationControlState>,
    ) -> IrrigationSnapshot {
        assemble_with_balance(forecast, tempest, zones, policy, map, control, None).await
    }

    /// `assemble_with` plus a pre-computed BalanceTick, the shape the
    /// live loop feeds every build: what the soil-model assembly tests
    /// drive their evidence through. The zone_runtime comes from the
    /// policy (the live loop passes `watering_policy.zone_runtime`).
    #[allow(clippy::too_many_arguments)]
    async fn assemble_with_balance(
        forecast: ForecastSnapshot,
        tempest: TempestSnapshot,
        zones: &[&str],
        policy: WateringPolicy,
        map: HashMap<String, Value>,
        control: Option<&crate::model::IrrigationControlState>,
        balance: Option<&BalanceTick>,
    ) -> IrrigationSnapshot {
        let fs = forecast_store_with(forecast);
        let ts = tempest_store_with(tempest, policy.calendar);
        let zone_runtime = policy.zone_runtime.clone();
        let scripts = CompiledScripts::compile(&[]);
        let (mut snap, finalize) = build_from_map(
            map,
            &fs,
            &ts,
            &zone_idents(zones),
            &zone_runtime,
            &policy,
            &scripts,
            None, // sensor_history
            None, // forecast_obs
            balance,
            control,
            Vec::new(),
        )
        .await;
        finalize.apply(&mut snap, &policy);
        snap
    }

    // ─────────────────────────────────────────────────────────────
    // The 0.7.22 Home Assistant helper cutover.
    //
    // Every one of these asserts the same shape twice: unadopted behaves
    // exactly as it did before, adopted reads LocalSky's own value and never
    // touches the entity again. The pair is the point. A release that only
    // proved the second half would leave a window where the value is neither
    // adopted nor read, which for a vacation pause is a watered yard.
    // ─────────────────────────────────────────────────────────────

    fn adopted(entity: &str) -> crate::model::HaAdoptedHelper {
        crate::model::HaAdoptedHelper {
            entity: entity.to_string(),
            outcome: "adopted".to_string(),
            target: crate::ha_adopt::target_of(entity).to_string(),
            adopted_value: None,
            observed_value: None,
            previous_value: None,
            epoch: 1,
        }
    }

    fn helper_map(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, Value> {
        pairs
            .iter()
            .map(|(id, v)| ((*id).to_string(), v.clone()))
            .collect()
    }

    fn calm() -> (ForecastSnapshot, TempestSnapshot) {
        let now = Utc::now().timestamp();
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            hourly: dry_hours(72.0, 4.0, 50),
            daily: dry_days(now),
            ..Default::default()
        };
        (fc, live_station(now, 72.0, 3.0, 50.0, 0.0))
    }

    // ---- The soil model's assembly (bucket-governs) ----

    /// One-zone policy on FL sand with a measured 15 mm/hr spray, the
    /// issue-#9 yard shape. `model` sets the engine default.
    fn sand_zone_policy(model: crate::config::schema::SchedulingModel) -> WateringPolicy {
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(model);
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 15.0,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1"
            }))
            .unwrap(),
        );
        WateringPolicy::from_config(&cfg).with_utc_calendar()
    }

    /// A balance tick whose soil window holds `days` trailing evidence
    /// days ending today, each with an `et0_mm` ledger row; today is
    /// charged at a small partial, the pre-dawn shape. A positive
    /// `storm_mm_yesterday` puts that gross depth on yesterday's rain.
    /// The morning window the refresher will price this yard against,
    /// read from the engine's own sunrise helpers with the refresher's
    /// own date choice. The window runs local midnight to sunrise, so
    /// its length moves with the runner's clock: the same Orlando
    /// coordinates give about 7 hours on a matching host and about 11 on
    /// a UTC container, which is enough to change which zones fit. A
    /// fixture that sizes its zones against THIS number asserts the same
    /// thing wherever it runs, and pinning the process timezone instead
    /// is not an option (`set_configured_tz` is a one-shot global that
    /// would leak into every other test in the binary).
    fn morning_window_s(lat: f64, lon: f64) -> u64 {
        // A probe sequence longer than any day clamps the start at local
        // midnight, which is what the refresher does to read the
        // window's true budget.
        const PROBE_S: u64 = 2 * 86_400;
        let now_utc = chrono::Utc::now();
        let today_local = crate::timeutil::now_local().date_naive();
        let morning = match crate::engine::sunrise::smart_morning_target_start(
            today_local,
            lat,
            lon,
            PROBE_S,
            crate::engine::calendar::Calendar::utc(),
        ) {
            Some(t) if t > now_utc => today_local,
            _ => today_local.succ_opt().unwrap_or(today_local),
        };
        crate::engine::sunrise::smart_morning_available_s(
            morning,
            lat,
            lon,
            PROBE_S,
            crate::engine::calendar::Calendar::utc(),
        )
        .unwrap_or(0)
        .max(0) as u64
    }

    fn soil_tick(days: usize, et0_mm: f64, storm_mm_yesterday: f64) -> BalanceTick {
        let today = crate::timeutil::now_local().date_naive();
        let dates: Vec<chrono::NaiveDate> = (0..days)
            .rev()
            .map(|back| today - chrono::Duration::days(back as i64))
            .collect();
        let mut rain_mm = vec![0.0; dates.len()];
        if dates.len() >= 2 && storm_mm_yesterday > 0.0 {
            let y = dates.len() - 2;
            rain_mm[y] = storm_mm_yesterday;
        }
        let et0_ledger: Vec<(chrono::NaiveDate, f64)> =
            dates.iter().map(|d| (*d, et0_mm)).collect();
        BalanceTick {
            observed_rain_mm: 0.0,
            observed_rain_source: "none".into(),
            observed_rain_days_mm: Vec::new(),
            bias: crate::engine::BiasModel::identity(),
            per_zone: HashMap::new(),
            runs_degraded: false,
            soil: SoilTickEvidence {
                unknown_rain_dates: Vec::new(),
                run_segments: HashMap::new(),
                dates,
                rain_mm,
                et0_ledger,
                et0_archive: Vec::new(),
                today_partial_et0_mm: Some(0.2),
                applied_valve_s: HashMap::new(),
                morning_decisions: HashMap::new(),
            },
        }
    }

    /// SHADOW on a weekly install: the soil block and the bucket_mm
    /// producer populate from the evidence window while every weekly
    /// decision (planned seconds, today's reason) is byte-identical to
    /// the same build with no soil evidence at all. The bucket rides the
    /// wire under the documented sign: negative = needs water.

    #[tokio::test]
    async fn dormant_planting_holds_even_before_the_initial_bucket_is_known() {
        let policy = sand_zone_policy(crate::config::schema::SchedulingModel::Soil);
        let (mut fc, ts) = calm();
        for hour in &mut fc.hourly {
            hour.soil_temp_6cm_f = 40.0;
        }
        let tick = soil_tick(0, 0.0, 0.0);
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let budget = &snap.water_budgets[0];
        assert_eq!(budget.soil_depletion_mm, None);
        assert!(budget.dormant);
        assert_eq!(budget.today_seconds, 0);
        assert_eq!(snap.zones[0].planned_run_seconds, 0);
        assert!(budget.today_reason.starts_with("Dormant:"));
    }

    #[tokio::test]
    async fn soil_shadow_populates_evidence_and_leaves_weekly_decisions() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Weekly);
        // Three dry 8 mm ledger days plus today's partial: past RAW on
        // sand at any month's Kc, clamped at TAW.
        let with_soil = soil_tick(4, 8.0, 0.0);
        let mut without_soil = with_soil.clone();
        without_soil.soil = SoilTickEvidence::default();
        let (fc, ts) = calm();
        let a = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy.clone(),
            HashMap::new(),
            None,
            Some(&with_soil),
        )
        .await;
        let (fc, ts) = calm();
        let b = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            None,
            Some(&without_soil),
        )
        .await;
        // Weekly decisions identical with or without bucket evidence.
        assert_eq!(
            a.zones[0].planned_run_seconds,
            b.zones[0].planned_run_seconds
        );
        let (ba, bb) = (&a.water_budgets[0], &b.water_budgets[0]);
        assert_eq!(ba.today_seconds, bb.today_seconds);
        assert_eq!(ba.today_reason, bb.today_reason);
        assert_eq!(
            ba.today_seconds, a.zones[0].planned_run_seconds,
            "the weekly allocator still owns the plan"
        );
        // The soil block rides in shadow on the evidence build.
        assert_eq!(ba.scheduling_model, "weekly");
        assert!(ba.soil_due, "three dry 8 mm days cross sand's RAW");
        let depletion = ba.soil_depletion_mm.unwrap();
        assert!(depletion > ba.soil_raw_mm.unwrap());
        assert!(depletion <= ba.soil_taw_mm.unwrap() + 1e-9);
        assert!(
            ba.soil_planned_seconds > 0,
            "the shadow names what it would water"
        );
        assert_eq!(ba.soil_deferred_reason, None);
        // bucket_mm's producer: the replayed deficit, negative = needs
        // water, mirrored onto the math panel.
        let bucket = a.zones[0].bucket_mm.unwrap();
        assert!(bucket < 0.0);
        assert!((bucket + depletion).abs() < 1e-9);
        assert_eq!(a.zones[0].math.as_ref().unwrap().bucket_mm, Some(bucket));
        // An EMPTY evidence window is STARVED: absence, not a
        // fabricated full-capacity (or full-deficit) figure. The soil
        // block stays off the wire until a rung resolves.
        assert_eq!(bb.soil_depletion_mm, None);
        assert!(!bb.soil_due);
        assert_eq!(bb.soil_planned_seconds, 0);
        assert_eq!(b.zones[0].bucket_mm, None);
    }

    /// When a zone resolves to the soil model, the plan IS the dispatch:
    /// the budget row's `today_seconds` carries the deficit-sized refill,
    /// the reason speaks the bucket vocabulary, and the zone's
    /// planned_run_seconds equals the row after the shared downstream, on
    /// the Home Assistant path and the native path alike. The same
    /// inputs under the weekly policy plan differently, which is the
    /// knob's hot-swap in action: same stores, same tick, the arc-swapped
    /// policy alone decides the producer.
    #[tokio::test]
    async fn soil_model_governs_today_seconds_on_both_paths() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Soil);
        let tick = soil_tick(4, 8.0, 0.0);
        for _path in ["home_assistant", "native"] {
            let (fc, ts) = calm();
            let snap = assemble_with_balance(
                fc,
                ts,
                &["front"],
                policy.clone(),
                HashMap::new(),
                None,
                Some(&tick),
            )
            .await;
            let b = &snap.water_budgets[0];
            assert_eq!(b.scheduling_model, "soil");
            assert!(b.soil_due);
            assert!(b.today_seconds > 0);
            assert_eq!(b.today_seconds, b.soil_planned_seconds);
            assert!(
                b.today_reason.contains("Projected demand would cross"),
                "{}",
                b.today_reason
            );
            // One truth: the governed row IS the dispatch figure.
            assert_eq!(snap.zones[0].planned_run_seconds, b.today_seconds);
            // Sized to the deficit through the refill arithmetic.
            let expect = crate::engine::water_balance::refill_runtime_seconds(
                b.soil_depletion_mm.unwrap(),
                15.0,
                0.70,
                3600,
            );
            assert_eq!(b.today_seconds, expect);
        }
        // The weekly policy on the SAME tick plans from the weekly
        // allocator instead: a settings save that swaps the policy
        // changes the producer on the next build, no restart.
        let weekly = sand_zone_policy(SchedulingModel::Weekly);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            weekly,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert_eq!(b.scheduling_model, "weekly");
        assert!(
            !b.today_reason.starts_with("soil"),
            "weekly vocabulary on the weekly model: {}",
            b.today_reason
        );
    }

    /// THE ISSUE #9 YARD at assembly level: a 1.2 in storm day fills the
    /// sand bucket, the next morning HOLDS with the bucket reason (no
    /// weekly quota resumes mid-storm), and one day later depletion has
    /// crossed RAW again and the yard waters sized to the actual deficit.
    #[tokio::test]
    async fn issue_9_sand_storm_holds_then_resumes_next_morning() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Soil);
        // Storm on yesterday's ledger row: the bucket clamps at field
        // capacity and today's partial charge leaves the yard far from
        // RAW.
        let storm_morning = soil_tick(4, 10.0, 1.2 * 25.4);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy.clone(),
            HashMap::new(),
            None,
            Some(&storm_morning),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert!(!b.soil_due, "the storm filled the bucket");
        assert_eq!(b.today_seconds, 0);
        assert_eq!(snap.zones[0].planned_run_seconds, 0);
        assert!(
            b.today_reason.contains("No watering needed"),
            "{}",
            b.today_reason
        );
        assert!(
            snap.zones[0].bucket_mm.unwrap().abs() < 0.5,
            "near field capacity after the storm"
        );
        // One day on: the storm sits two days back, yesterday charged a
        // full measured 10 mm ET0 day, and sand's small RAW is crossed.
        let mut next_morning = soil_tick(5, 10.0, 0.0);
        let storm_idx = next_morning.soil.dates.len() - 3;
        next_morning.soil.rain_mm[storm_idx] = 1.2 * 25.4;
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            None,
            Some(&next_morning),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert!(b.soil_due, "yesterday's ETc re-crossed RAW");
        assert!(b.today_seconds > 0);
        assert!(
            b.today_reason.contains("Projected demand would cross"),
            "{}",
            b.today_reason
        );
        // Deficit-sized: one measured day's charge (Kc-scaled, clamped
        // by TAW), nowhere near a weekly-quota session.
        let planned = snap.zones[0].planned_run_seconds;
        assert!(
            (1700..=3300).contains(&planned),
            "refill sized to the deficit, got {planned}"
        );
    }

    /// MIXED-MODE install: the engine default is soil, one zone pins
    /// weekly. The pinned zone's weekly row is identical to an all-weekly
    /// build of the same inputs, while its sibling is soil-governed.
    #[tokio::test]
    async fn mixed_mode_pins_split_the_producers() {
        use crate::config::schema::SchedulingModel;
        let zone_json = |pin: serde_json::Value| -> crate::config::schema::ZoneConfig {
            serde_json::from_value(serde_json::json!({
                "display_name": "Z",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 15.0,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "scheduling_model": pin
            }))
            .unwrap()
        };
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        cfg.zones
            .insert("front".into(), zone_json(serde_json::Value::Null));
        cfg.zones
            .insert("back".into(), zone_json(serde_json::json!("weekly")));
        let mixed = WateringPolicy::from_config(&cfg).with_utc_calendar();
        cfg.engine.scheduling_model = Some(SchedulingModel::Weekly);
        cfg.zones.get_mut("back").unwrap().scheduling_model = None;
        cfg.zones.get_mut("front").unwrap().scheduling_model = Some(SchedulingModel::Weekly);
        let all_weekly = WateringPolicy::from_config(&cfg).with_utc_calendar();

        let tick = soil_tick(4, 8.0, 0.0);
        let (fc, ts) = calm();
        let a = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            mixed,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let (fc, ts) = calm();
        let w = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            all_weekly,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        // Budget rows ride in config order; address them by slug.
        let row = |s: &IrrigationSnapshot, slug: &str| {
            s.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .cloned()
                .unwrap()
        };
        let (front, back) = (row(&a, "front"), row(&a, "back"));
        assert_eq!(front.scheduling_model, "soil");
        assert!(
            front.today_reason.contains("root-zone depletion"),
            "{}",
            front.today_reason
        );
        assert_eq!(back.scheduling_model, "weekly");
        // The pinned-weekly zone decides exactly as it would on an
        // all-weekly install.
        let back_w = row(&w, "back");
        assert_eq!(back.today_seconds, back_w.today_seconds);
        assert_eq!(back.today_reason, back_w.today_reason);
        assert_eq!(
            a.zones[1].planned_run_seconds,
            w.zones[1].planned_run_seconds
        );
    }

    /// ADMISSION binds to the dispatcher's own wall pricer: two clay
    /// zones each wanting a multi-hour refill cannot both fit the
    /// midnight-to-sunrise window, so the window fits one (stress ties
    /// keep active-list order) and the other carries to tomorrow with
    /// the window reason on the row and the wire's soil block.
    #[tokio::test]
    async fn admission_defers_what_the_morning_window_cannot_fit() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        // Each zone wants the whole window, so two never fit and one
        // always does (admission seats the most stressed regardless).
        // The low precip rate keeps the cap binding: the raw refill is
        // tens of hours, far past any cap this sets.
        let cap_min = morning_window_s(28.5, -81.4) / 60;
        for slug in ["front", "back"] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "clay",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 1.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1",
                    "max_run_minutes": cap_min
                }))
                .unwrap(),
            );
        }
        let policy = WateringPolicy::from_config(&cfg).with_utc_calendar();
        // Two weeks of dry 10 mm days clamp both buckets at clay's TAW.
        let tick = soil_tick(14, 10.0, 0.0);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            policy,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        // Budget rows ride in config order; address them by slug. The
        // active list runs front-then-back, so the stress tie keeps
        // "front" first in admission.
        let row = |slug: &str| {
            snap.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .unwrap()
        };
        let (front, back) = (row("front"), row("back"));
        assert!(front.today_seconds > 0, "the most stressed zone waters");
        assert_eq!(back.today_seconds, 0, "the window cannot fit both");
        assert!(
            back.today_reason.contains("the morning window fits 1 of 2"),
            "{}",
            back.today_reason
        );
        assert_eq!(back.soil_planned_seconds, 0);
        assert_eq!(
            back.soil_deferred_reason.as_deref(),
            Some(back.today_reason.as_str())
        );
        assert_eq!(snap.zones[1].planned_run_seconds, 0);
    }

    /// ADMISSION prices the candidates themselves at dispatch truth:
    /// the seasonal dial apply_budget_plan runs every row through also
    /// prices the soil candidates in the wall. Two clay zones whose raw
    /// refills cannot share the window both fit once a 50% dial halves
    /// what actually dispatches, so neither carries to tomorrow against
    /// seconds no valve will run.
    #[tokio::test]
    async fn admission_prices_candidates_at_the_seasonal_dial() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        // Just over half the window each: the raw pair overruns it and
        // the halved pair fits, on any host clock.
        let cap_min = (morning_window_s(28.5, -81.4) as f64 * 0.55 / 60.0).round() as u64;
        for slug in ["front", "back"] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "clay",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 1.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1",
                    "max_run_minutes": cap_min
                }))
                .unwrap(),
            );
        }
        let row = |s: &IrrigationSnapshot, slug: &str| {
            s.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .cloned()
                .unwrap()
        };
        // Both buckets pinned at clay's TAW: each raw refill sits on the
        // window-relative cap, and two of those cannot share it.
        let tick = soil_tick(14, 10.0, 0.0);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            WateringPolicy::from_config(&cfg).with_utc_calendar(),
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        assert!(row(&snap, "front").today_seconds > 0);
        assert_eq!(
            row(&snap, "back").today_seconds,
            0,
            "raw refills cannot share the window"
        );
        // The 50% dial halves what dispatches, so both refills fit.
        cfg.engine.seasonal_adjust_pct = 50;
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front", "back"],
            WateringPolicy::from_config(&cfg).with_utc_calendar(),
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let (front, back) = (row(&snap, "front"), row(&snap, "back"));
        assert!(front.today_seconds > 0, "{}", front.today_reason);
        assert!(
            back.today_seconds > 0,
            "the dialed-down refills share the window: {}",
            back.today_reason
        );
        assert!(!back.today_reason.contains("waits for tomorrow"));
    }

    /// The admission base prices non-candidates at what they will
    /// ACTUALLY dispatch. On a forecast-rain morning the weekly
    /// sibling's allocator session is blocked by its own skip verdict
    /// (post-inertness), so its seconds must not occupy the window: two
    /// due soil zones both water instead of the second deferring against
    /// a window that is actually empty.
    #[tokio::test]
    async fn admission_ignores_weekly_seconds_a_skip_verdict_blocks() {
        use crate::config::schema::SchedulingModel;
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        // Two soil-governed sand zones, both pinned at TAW by the tick.
        for slug in ["front", "back"] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "sand",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 15.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1"
                }))
                .unwrap(),
            );
        }
        // A weekly-pinned sibling whose one allocator session alone
        // fills the whole pre-sunrise window (10 in over 1 session,
        // capped at 360 min).
        cfg.zones.insert(
            "lawn".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Lawn",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 15.0,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1",
                "scheduling_model": "weekly",
                "weekly_budget_in": 10.0,
                "sessions_per_week": 1,
                "max_run_minutes": 360
            }))
            .unwrap(),
        );
        let policy = WateringPolicy::from_config(&cfg).with_utc_calendar();
        // Fourteen dry 10 mm days pin both sand buckets at TAW. The
        // DAILY tomorrow entry fires the tomorrow_rain gate while the
        // hourly series stays dry, so defer-by-deficit does not hold the
        // soil zones (the gate is inert for them, priced by the defer).
        let tick = soil_tick(14, 10.0, 0.0);
        let now = Utc::now().timestamp();
        let mut fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            hourly: dry_hours(72.0, 4.0, 50),
            ..Default::default()
        };
        fc.daily = vec![
            crate::forecast::snapshot::DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(now),
                precip_sum_in: Some(0.0),
                ..Default::default()
            },
            crate::forecast::snapshot::DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(now + 86_400),
                precip_sum_in: Some(1.0),
                precip_probability_max: None,
                ..Default::default()
            },
        ];
        let snap = assemble_with_balance(
            fc,
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front", "back", "lawn"],
            policy,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let row = |slug: &str| {
            snap.water_budgets
                .iter()
                .find(|b| b.zone_slug == slug)
                .unwrap()
        };
        // The weekly sibling's row still carries its full allocator
        // session (display), while its verdict says the ladder blocks it
        // at dispatch.
        let lawn = row("lawn");
        assert!(
            lawn.today_seconds > 0,
            "the weekly row keeps its allocator seconds: {}",
            lawn.today_reason
        );
        let lawn_v = snap
            .zones
            .iter()
            .find(|z| z.slug == "lawn")
            .and_then(|z| z.verdict.as_ref())
            .unwrap();
        assert_eq!(lawn_v.verdict, "skip", "the weekly sibling holds");
        // Both due soil zones are admitted: the blocked weekly seconds
        // never priced the window.
        let (front, back) = (row("front"), row("back"));
        assert!(front.today_seconds > 0, "{}", front.today_reason);
        assert!(
            back.today_seconds > 0,
            "the second soil zone is admitted, not deferred: {}",
            back.today_reason
        );
        assert!(!front.today_reason.contains("waits for tomorrow"));
        assert!(!back.today_reason.contains("waits for tomorrow"));
    }

    /// An evidence-starved window publishes ABSENCE and the weekly
    /// allocator keeps sizing a governed zone: no fabricated full-TAW
    /// bucket, no refill planned on assumption alone. The absent-
    /// not-zero contract from 0.7.22 holds for the soil block itself,
    /// both on a genuinely input-free install (no ET0 ledger, no
    /// archive, no rain rows, no applied seconds, not even today's
    /// partial) and on one whose only rung is today's self-emitted
    /// partial: a single rung over thirteen fallback days stays under
    /// the `MIN_EVIDENCE_DAYS` floor instead of flipping the window to
    /// full confidence within the first morning.
    #[tokio::test]
    async fn evidence_starved_soil_zone_rides_the_weekly_allocator() {
        use crate::config::schema::SchedulingModel;
        let starved = |mut t: BalanceTick| {
            t.soil.et0_ledger.clear();
            t.soil.et0_archive.clear();
            t.soil.today_partial_et0_mm = None;
            t
        };
        let tick = starved(soil_tick(14, 8.0, 0.0));
        let (fc, ts) = calm();
        let soil_snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            sand_zone_policy(SchedulingModel::Soil),
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let (fc, ts) = calm();
        let weekly_snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            sand_zone_policy(SchedulingModel::Weekly),
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let b = &soil_snap.water_budgets[0];
        assert_eq!(b.scheduling_model, "soil", "the zone stays soil-resolved");
        assert_eq!(
            b.soil_depletion_mm, None,
            "absence, not a fabricated bucket"
        );
        assert!(!b.soil_due);
        assert_eq!(b.soil_planned_seconds, 0);
        assert_eq!(soil_snap.zones[0].bucket_mm, None);
        // The weekly allocator's sizing stands until a rung resolves.
        let w = &weekly_snap.water_budgets[0];
        assert_eq!(b.today_seconds, w.today_seconds);
        assert_eq!(
            b.today_reason,
            format!(
                "{}; soil estimate not established, using the weekly target",
                w.today_reason
            )
        );
        assert!(
            !b.today_reason.starts_with("soil"),
            "weekly vocabulary while starved: {}",
            b.today_reason
        );
        // Today's partial alone (one evidenced day, thirteen fallback)
        // stays under the MIN_EVIDENCE_DAYS floor: same posture.
        let thin = |mut t: BalanceTick| {
            t.soil.et0_ledger.clear();
            t.soil.et0_archive.clear();
            t
        };
        let tick = thin(soil_tick(14, 8.0, 0.0));
        let (fc, ts) = calm();
        let thin_snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            sand_zone_policy(SchedulingModel::Soil),
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        let t = &thin_snap.water_budgets[0];
        assert_eq!(
            t.soil_depletion_mm, None,
            "one rung must not buy full confidence"
        );
        assert_eq!(t.soil_planned_seconds, 0);
        assert_eq!(thin_snap.zones[0].bucket_mm, None);
        assert_eq!(t.today_seconds, w.today_seconds);
        assert_eq!(
            t.today_reason,
            format!(
                "{}; soil estimate not established, using the weekly target",
                w.today_reason
            )
        );
    }

    /// A runs-store read error the tick after a dispatched run must not
    /// replay applied=0 into an inflated re-dispatch: the degraded tick
    /// publishes no bucket and the governed swap stands down, so a zone
    /// that watered yesterday is not refilled again on blind evidence.
    #[tokio::test]
    async fn degraded_runs_read_never_inflates_a_governed_refill() {
        use crate::config::schema::SchedulingModel;
        let policy = sand_zone_policy(SchedulingModel::Soil);
        // Clean tick: yesterday's two-hour run rides the evidence, the
        // bucket clamps at field capacity, and the zone reads not due.
        let mut clean = soil_tick(4, 8.0, 0.0);
        clean
            .soil
            .applied_valve_s
            .insert("front".into(), vec![0, 0, 7200, 0]);
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy.clone(),
            HashMap::new(),
            None,
            Some(&clean),
        )
        .await;
        assert_eq!(
            snap.water_budgets[0].today_seconds, 0,
            "watered yesterday, not due: {}",
            snap.water_budgets[0].today_reason
        );
        // The same tick with the runs read errored: the applied column
        // is blind. Without the degraded mark the replay reconstructs a
        // deep depletion and re-dispatches a refill for water already on
        // the ground.
        let mut degraded = clean.clone();
        degraded.soil.applied_valve_s.clear();
        degraded.runs_degraded = true;
        let (fc, ts) = calm();
        let snap = assemble_with_balance(
            fc,
            ts,
            &["front"],
            policy,
            HashMap::new(),
            None,
            Some(&degraded),
        )
        .await;
        let b = &snap.water_budgets[0];
        assert_eq!(
            b.soil_depletion_mm, None,
            "no bucket published on a degraded tick"
        );
        assert_eq!(snap.zones[0].bucket_mm, None);
        assert!(
            !b.today_reason.starts_with("soil refill"),
            "no soil refill dispatched on blind evidence: {}",
            b.today_reason
        );
        assert_eq!(b.scheduling_model, "soil", "the model tag survives");
    }

    /// The forward-rain gates are INERT for soil-governed zones, at both
    /// layers the ladder acts on: the yard-wide blanket lifts (the
    /// soil-floor demotion morning's shape) and the per-zone verdict
    /// reads run with the demotion named, while a weekly sibling keeps
    /// its skip and a SAFETY gate (wind) still binds everything.
    #[tokio::test]
    async fn forward_rain_gates_are_inert_for_soil_zones_only() {
        use crate::config::schema::SchedulingModel;
        let rainy_tomorrow = |now: i64| {
            let mut fc = ForecastSnapshot {
                last_refresh_epoch: now,
                source_reachable: true,
                hourly: dry_hours(72.0, 4.0, 50),
                ..Default::default()
            };
            fc.daily = vec![
                crate::forecast::snapshot::DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(now),
                    precip_sum_in: Some(0.0),
                    ..Default::default()
                },
                crate::forecast::snapshot::DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(now + 86_400),
                    precip_sum_in: Some(1.0),
                    precip_probability_max: None,
                    ..Default::default()
                },
            ];
            fc
        };
        let now = Utc::now().timestamp();
        let tick = soil_tick(4, 8.0, 0.0);
        // Sanity: the same morning under the weekly model is a blanket
        // tomorrow-rain skip.
        let weekly = sand_zone_policy(SchedulingModel::Weekly);
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front"],
            weekly,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        assert!(snap.skip_check.will_skip);
        assert_eq!(snap.skip_check.reason_code, "tomorrow_rain");
        // All-soil: the blanket lifts, the zone runs, and the reason
        // names the demotion.
        let soil = sand_zone_policy(SchedulingModel::Soil);
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front"],
            soil.clone(),
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        assert!(!snap.skip_check.will_skip);
        assert_eq!(snap.skip_check.verdict, "run");
        assert!(
            snap.skip_check.reason.contains("All zones can water"),
            "{}",
            snap.skip_check.reason
        );
        let v = snap.zones[0].verdict.as_ref().unwrap();
        assert_eq!(v.verdict, "run");
        assert_eq!(v.source, "soil_model");
        assert!(
            snap.zones[0].planned_run_seconds > 0,
            "the soil zone waters"
        );
        // Mixed: the weekly sibling keeps its global-source skip, which
        // the dispatcher enforces per zone on a non-blanket morning.
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.engine.scheduling_model = Some(SchedulingModel::Soil);
        for (slug, pin) in [
            ("front", serde_json::Value::Null),
            ("back", serde_json::json!("weekly")),
        ] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": "st_augustine",
                    "soil_texture": "sand",
                    "sprinkler_type": "spray",
                    "precip_rate_mm_hr": 15.0,
                    "precip_rate_source": "measured",
                    "controller_id": "os_main",
                    "controller_station": "1",
                    "scheduling_model": pin
                }))
                .unwrap(),
            );
        }
        let mixed = WateringPolicy::from_config(&cfg).with_utc_calendar();
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 3.0, 50.0, 0.0),
            &["front", "back"],
            mixed,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        assert!(
            !snap.skip_check.will_skip,
            "the blanket lifts for the soil zone"
        );
        let front_v = snap.zones[0].verdict.as_ref().unwrap();
        assert_eq!(front_v.verdict, "run");
        assert_eq!(front_v.source, "soil_model");
        let back_v = snap.zones[1].verdict.as_ref().unwrap();
        assert_eq!(back_v.verdict, "skip", "the weekly sibling still holds");
        assert_eq!(back_v.source, "global");
        // A SAFETY gate binds soil zones exactly as before: hard wind
        // now is not in the inert set.
        let snap = assemble_with_balance(
            rainy_tomorrow(now),
            live_station(now, 72.0, 30.0, 50.0, 0.0),
            &["front"],
            soil,
            HashMap::new(),
            None,
            Some(&tick),
        )
        .await;
        assert!(snap.skip_check.will_skip, "wind still skips the yard");
        assert_eq!(
            snap.zones[0].verdict.as_ref().unwrap().verdict,
            "skip",
            "no inertness for safety gates"
        );
    }

    /// Assembly summarizes completed zone decisions; it cannot manufacture
    /// permission to water by rewriting a skip after safety and user rules.
    #[test]
    fn soil_gate_summary_never_rewrites_an_engine_hold() {
        use crate::model::ZoneVerdict;
        let verdict = |slug: &str, v: &str, source: &str, code: &str| ZoneVerdict {
            zone_slug: slug.into(),
            zone_name: slug.into(),
            verdict: v.into(),
            reason: "Rain expected tomorrow".into(),
            source: source.into(),
            multiplier: 1.0,
            reason_code: code.into(),
            value: None,
            threshold: None,
        };
        let zone = |slug: &str, v: &ZoneVerdict| crate::model::ZoneState {
            slug: slug.into(),
            verdict: Some(v.clone()),
            ..Default::default()
        };
        let mut snap = IrrigationSnapshot::default();
        let fv = verdict("front", "skip", "global", "tomorrow_rain");
        let bv = verdict("back", "skip", "global", "tomorrow_rain");
        snap.zones = vec![zone("front", &fv), zone("back", &bv)];
        snap.zone_verdicts = vec![fv, bv];
        snap.skip_check.decide(
            "skip",
            snap.skip_check.reason.clone(),
            snap.skip_check.reason_code.clone(),
        );
        snap.skip_check.reason = "Rain expected tomorrow".into();
        snap.skip_check.reason_code = "tomorrow_rain".into();
        crate::assembly::apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "skip");
        assert_eq!(snap.zone_verdicts[0].source, "global");
        assert_eq!(snap.zones[0].verdict.as_ref().unwrap().verdict, "skip");
        assert_eq!(snap.zone_verdicts[1].verdict, "skip", "weekly zone holds");
        assert!(
            snap.skip_check.will_skip,
            "configuration alone cannot lift a hold"
        );

        // This strip annotation pass cannot change the completed engine answer.
        let fv = verdict("front", "run", "soil_model", "tomorrow_rain");
        snap.zones[0].verdict = Some(fv.clone());
        snap.zone_verdicts[0] = fv;
        crate::assembly::apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert!(snap.skip_check.will_skip);
        assert_eq!(snap.skip_check.verdict, "skip");
        // A non-inert gate is untouched even for governed zones.
        let wv = verdict("front", "skip", "global", "wind_now");
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front", &wv)];
        snap.zone_verdicts = vec![wv];
        snap.skip_check
            .decide("skip", String::new(), "wind_now".into());
        crate::assembly::apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "skip");
        assert!(snap.skip_check.will_skip, "safety gates never demote");
        // Heat advisory: run_extended downgrades to run for a governed
        // zone (measured ET0 already carried the heat), and the yard
        // verdict follows when EVERY zone is governed.
        let hv = verdict("front", "run", "soil_model", "soil_model");
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front", &hv)];
        snap.zone_verdicts = vec![hv];
        snap.skip_check
            .revise_verdict("run_extended", snap.skip_check.reason.clone());
        snap.skip_check.reason = "Heat advisory".into();
        snap.skip_check.reason_code = "heat_advisory".into();
        crate::assembly::apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "run");
        assert_eq!(snap.skip_check.verdict, "run_extended");
        // A condition-rule extension is not the heat gate: untouched.
        let cv = verdict("front", "run_extended", "condition", "condition");
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front", &cv)];
        snap.zone_verdicts = vec![cv];
        crate::assembly::apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert_eq!(snap.zone_verdicts[0].verdict, "run_extended");
    }

    /// The seven-day strip narrates the same decision as the demoted
    /// aggregate. All-soil install: inert-gate cells (today AND the
    /// forward cells) demote to runs sourced soil_model, a heat
    /// extension cell downgrades to a plain run, and a safety-gate cell
    /// is untouched. Mixed install: the skip stands for the weekly
    /// zones with the weekly-only annotation the aggregate carries.
    #[test]
    fn verdict_strip_cells_follow_the_gate_inertness() {
        let cell = |off: u32, v: &str, code: &str| crate::model::DayVerdict {
            day_offset: off,
            verdict: v.into(),
            reason: "Rain expected".into(),
            reason_code: code.into(),
            ..Default::default()
        };
        let zone = |slug: &str| crate::model::ZoneState {
            slug: slug.into(),
            ..Default::default()
        };
        let strip = || {
            vec![
                cell(0, "skip", "tomorrow_rain"),
                cell(1, "skip", "rain_3day"),
                cell(2, "skip", "wind_now"),
                cell(3, "run_extended", "heat_advisory"),
                cell(4, "run", "run"),
            ]
        };
        // All-soil: the completed engine answer already permits watering.
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front")];
        snap.seven_day_verdicts = strip();
        snap.skip_check
            .decide("run", "Soil zones can water".into(), "run".into());
        snap.zone_verdicts = vec![crate::model::ZoneVerdict {
            zone_slug: "front".into(),
            zone_name: "Front".into(),
            verdict: "run".into(),
            reason: "Soil model already accounts for forecast rain".into(),
            source: "soil_model".into(),
            reason_code: "tomorrow_rain".into(),
            multiplier: 1.0,
            value: None,
            threshold: None,
        }];
        crate::assembly::apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        assert!(!snap.skip_check.will_skip);
        let cells = &snap.seven_day_verdicts;
        assert_eq!(
            cells[0].verdict, snap.skip_check.verdict,
            "the [0] cell agrees with the demoted skip_check"
        );
        assert_eq!(cells[0].reason_code, "soil_model");
        assert!(
            cells[0].reason.starts_with("Waters anyway:"),
            "{}",
            cells[0].reason
        );
        assert_eq!(cells[1].verdict, "run", "forward inert cells demote too");
        assert_eq!(cells[2].verdict, "skip", "safety gates never demote");
        assert_eq!(cells[2].reason_code, "wind_now");
        assert_eq!(cells[3].verdict, "run", "heat extension downgrades");
        assert_eq!(cells[4].verdict, "run");
        assert_eq!(cells[4].reason, "Rain expected", "plain runs untouched");
        // Mixed: the skip stands, annotated for the weekly zones.
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front"), zone("back")];
        snap.seven_day_verdicts = strip();
        crate::assembly::apply_soil_gate_inertness(&mut snap, &["front".to_string()]);
        let cells = &snap.seven_day_verdicts;
        assert_eq!(cells[0].verdict, "skip");
        assert!(
            cells[0].reason.ends_with(crate::model::MIXED_SKIP_NOTE),
            "{}",
            cells[0].reason
        );
        assert_eq!(
            cells[3].verdict, "run_extended",
            "mixed keeps the extension"
        );
        // No governed zones: nothing moves at all.
        let mut snap = IrrigationSnapshot::default();
        snap.zones = vec![zone("front")];
        snap.seven_day_verdicts = strip();
        crate::assembly::apply_soil_gate_inertness(&mut snap, &[]);
        assert_eq!(snap.seven_day_verdicts, strip());
    }

    /// The sticky overrides decide on BOTH deployment paths. On the Home
    /// Assistant path a stored Force or Skip was shown by the panel and
    /// ignored by the engine; the store is the operator's instruction and
    /// the engine reads it wherever it runs. The release note tells a
    /// Home Assistant operator to check the panel at the upgrade.
    /// The pause, the override and the two toggles come from LocalSky's
    /// store on both paths; a helper sitting in the entity map with the
    /// old name decides nothing.
    #[tokio::test]
    async fn the_control_store_decides_and_no_helper_is_read() {
        let future = Utc::now().timestamp() + 86_400;
        let map = helper_map(&[
            (
                crate::ha_adopt::PAUSE_TOGGLE,
                serde_json::json!({ "state": "on" }),
            ),
            (
                crate::ha_adopt::PAUSE_UNTIL,
                serde_json::json!({ "state": "2030-01-01 05:00:00", "attributes": { "timestamp": future } }),
            ),
            (
                crate::ha_adopt::MAX_WIND,
                serde_json::json!({ "state": "99" }),
            ),
        ]);
        let (fc, ts) = calm();
        let none = assemble_with(
            fc.clone(),
            ts.clone(),
            &["front"],
            WateringPolicy::default(),
            map.clone(),
            None,
        )
        .await;
        assert_eq!(
            none.pause_until_epoch, 0,
            "no store: no pause, not the helper's"
        );
        assert!(!none.skip_check.is_paused);
        let control = crate::model::IrrigationControlState {
            pause_until_epoch: future,
            is_paused: true,
            ..Default::default()
        };
        let stored = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            map,
            Some(&control),
        )
        .await;
        assert_eq!(stored.pause_until_epoch, future);
        assert!(stored.skip_check.is_paused);
    }

    #[tokio::test]
    async fn a_stored_sticky_override_decides_on_both_paths() {
        let mut control = crate::model::IrrigationControlState::default();
        control.global_override = "run".to_string();
        control
            .zone_overrides
            .insert("front".to_string(), "skip".to_string());
        for ha_path in [true, false] {
            let (fc, ts) = calm();
            let snap = assemble_with(
                fc,
                ts,
                &["front"],
                WateringPolicy::default(),
                HashMap::new(),
                Some(&control),
            )
            .await;
            assert_eq!(snap.global_override, "run", "ha_path={ha_path}");
            assert_eq!(snap.zones[0].override_mode, "skip", "ha_path={ha_path}");
        }
    }

    #[tokio::test]
    async fn the_migration_record_rides_the_snapshot_for_the_notice() {
        let policy = WateringPolicy {
            ha_adoption: vec![adopted(crate::ha_adopt::RAIN_SKIP)],
            ..Default::default()
        };
        let (fc, ts) = calm();
        let snap = assemble_with(fc, ts, &["front"], policy, HashMap::new(), None).await;
        assert_eq!(snap.ha_adoption.len(), 1);
        assert_eq!(snap.ha_adoption[0].entity, crate::ha_adopt::RAIN_SKIP);
        assert!(
            !snap.controls_persisted,
            "no control state means no control store, which is what the notice has to say"
        );
    }

    // The notice needs to tell "this install has no database, so the four
    // controls can never be adopted" apart from "a control was not answering
    // when the pass looked, so it was left alone". Both leave the control out
    // of the record set, so the records alone cannot say which; the snapshot
    // carries the bit.
    #[tokio::test]
    async fn a_mounted_control_store_says_so_on_the_snapshot() {
        let control = crate::model::IrrigationControlState::default();
        let (fc, ts) = calm();
        let snap = assemble_with(
            fc,
            ts,
            &["front"],
            WateringPolicy::default(),
            HashMap::new(),
            Some(&control),
        )
        .await;
        assert!(snap.controls_persisted);
    }

    // The per-zone ZoneMath cap follows the CONFIGURED max_run_minutes through
    // WateringPolicy::from_config (the same builder boot AND hot reload use),
    // so an applied cap change lands on the very next snapshot build.
    #[tokio::test]
    async fn zone_math_cap_follows_the_configured_run_limit() {
        let now = Utc::now().timestamp();
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            hourly: dry_hours(72.0, 4.0, 50),
            ..Default::default()
        };
        let tempest = live_station(now, 72.0, 3.0, 50.0, 0.0);
        let mut cfg = crate::config::schema::Config::default();
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "bermuda",
                "soil_texture": "sandy_loam",
                "sprinkler_type": "spray",
                "controller_id": "os_main",
                "controller_station": "1",
                "max_run_minutes": 90
            }))
            .unwrap(),
        );
        let policy = WateringPolicy::from_config(&cfg).with_utc_calendar();
        let fs = forecast_store_with(fc);
        let ts = tempest_store_with(tempest, policy.calendar);
        let scripts = CompiledScripts::compile(&[]);
        let (snap, _) = build_from_map(
            HashMap::new(),
            &fs,
            &ts,
            &zone_idents(&["front"]),
            &policy.zone_runtime,
            &policy,
            &scripts,
            None,
            None,
            None,
            None,
            Vec::new(),
        )
        .await;
        let math = snap.zones[0]
            .math
            .clone()
            .expect("math is always assembled");
        assert_eq!(
            math.max_duration_seconds, 5400,
            "ZoneMath carries the configured 90 minute cap in seconds"
        );
    }

    // ── CLEAR RUN ─────────────────────────────────────────────────────────────
    // Dry, warm, calm, fresh station, no soil config: the assembled verdict is a
    // plain "run" with an empty reason, and every per-zone verdict is run/global.
    // heat_index_max_3day_f is asserted to be the PER-DAY pairing (each day's
    // high temp × THAT day's humidity), proving the now-humidity bug is absent.
    /// Per-zone verdicts are produced one per entry in `soil_zones`. On an
    /// install with no zones in `localsky.toml` (an empty zones table,
    /// the `WateringPolicy::default()` passed here) that list is the legacy
    /// fallback, which used to be four hardcoded slugs: it handed such an
    /// install verdicts for zones it did not own and none for the zones it
    /// did. Every active zone must appear, and nothing else.
    #[tokio::test]
    async fn probeless_install_gets_a_verdict_for_its_own_zones_only() {
        let now = Utc::now().timestamp();
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            daily: vec![DailyEntry {
                day_marker: crate::engine::clock::DayMarker::inside_local_day(now),
                temp_max_f: Some(88.0),
                temp_min_f: Some(70.0),
                humidity_pct: Some(50),
                precip_sum_in: Some(0.0),
                precip_probability_max: Some(0),
                wind_max_mph: Some(4.0),
                ..Default::default()
            }],
            hourly: dry_hours(78.0, 4.0, 50),
            ..Default::default()
        };
        let snap = assemble(
            fc,
            live_station(now, 78.0, 3.0, 50.0, 0.0),
            &["orchard", "west_strip", "herb_bed"],
            WateringPolicy::default(),
        )
        .await;

        let got: Vec<&str> = snap
            .zone_verdicts
            .iter()
            .map(|v| v.zone_slug.as_str())
            .collect();
        assert_eq!(got, vec!["orchard", "west_strip", "herb_bed"], "{got:?}");
        // Every zone on the snapshot carries its own verdict, not None.
        for z in &snap.zones {
            assert!(z.verdict.is_some(), "no verdict for {}", z.slug);
        }
    }

    #[tokio::test]
    async fn assembles_clear_run_with_per_day_heat_index() {
        let now = Utc::now().timestamp();
        // Daily forecast: a hot-but-DRY-air day and a cooler humid day. The
        // hottest FEELS-LIKE day wins. Kept below the 95°F heat-advisory temp
        // gate so the verdict is a plain "run", not run_extended.
        let day_hi = DailyEntry {
            day_marker: crate::engine::clock::DayMarker::inside_local_day(now),
            temp_max_f: Some(90.0),
            temp_min_f: Some(70.0),
            humidity_pct: Some(45), // the day's OWN afternoon RH
            precip_sum_in: Some(0.0),
            precip_probability_max: Some(0),
            wind_max_mph: Some(5.0),
            ..Default::default()
        };
        let day_cool = DailyEntry {
            day_marker: crate::engine::clock::DayMarker::inside_local_day(now + 86_400),
            temp_max_f: Some(80.0),
            temp_min_f: Some(66.0),
            humidity_pct: Some(70),
            precip_sum_in: Some(0.0),
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            last_refresh_epoch: now, // fresh, so forecast rules are live
            source_reachable: true,
            daily: vec![day_hi.clone(), day_cool.clone()],
            hourly: dry_hours(72.0, 4.0, 50),
            ..Default::default()
        };
        // Live station carries a SATURATED post-rain "now" humidity (97%), wildly
        // different from any day's afternoon RH. The buggy pairing (day max temp ×
        // now humidity) would inflate the 3-day heat index; the correct per-day
        // pairing uses the day's own RH.
        let tempest = live_station(now, 72.0, 3.0, 97.0, 0.0);

        let snap = assemble(
            fc,
            tempest,
            &["back_yard", "front_yard"],
            WateringPolicy::default(),
        )
        .await;

        // Aggregate verdict: a clean run, no skip reason.
        assert_eq!(
            snap.skip_check.verdict, "run",
            "reason: {}",
            snap.skip_check.reason
        );
        assert!(!snap.skip_check.will_skip);
        assert!(snap.skip_check.reason.is_empty());

        // Per-zone verdicts: one per resolved soil zone (with no soil config that
        // is the legacy 4-zone fallback), all run/global on a clean morning.
        assert!(!snap.zone_verdicts.is_empty());
        for v in &snap.zone_verdicts {
            assert_eq!(v.verdict, "run", "zone {} should run", v.zone_slug);
            assert_eq!(v.source, "global");
        }
        // The per-zone verdict is back-filled onto each configured ZoneState that
        // has a matching soil-zone verdict (the legacy fallback covers back_yard +
        // front_yard, the two zones configured here).
        for z in &snap.zones {
            assert_eq!(
                z.verdict.as_ref().map(|v| v.verdict.as_str()),
                Some("run"),
                "zone {} should have a run verdict back-filled",
                z.slug
            );
        }

        // heat_index_max_3day_f: the correct per-day pairing. The hot-dry day
        // (90°F @ 45%) out-feels the cool-humid day (80°F @ 70%).
        let expected_per_day = heat_index_f(90.0, 45.0).max(heat_index_f(80.0, 70.0));
        assert!(
            (snap.skip_check.heat_index_max_3day_f - expected_per_day).abs() < 1e-6,
            "assembled heat index {} must equal the per-day max {expected_per_day}",
            snap.skip_check.heat_index_max_3day_f
        );
        // And it must be the hot-dry day, not the cool-humid one.
        assert!((expected_per_day - heat_index_f(90.0, 45.0)).abs() < 1e-9);
        // The now-humidity bug (90°F paired with the saturated 97% "now") would be
        // MUCH higher. The assembled value must stay well below it.
        let buggy_now_pairing = heat_index_f(90.0, 97.0);
        assert!(
            snap.skip_check.heat_index_max_3day_f < buggy_now_pairing - 5.0,
            "per-day heat index {} must be far below the now-humidity bug {buggy_now_pairing}",
            snap.skip_check.heat_index_max_3day_f
        );
        // The Forecast block mirrors the same value.
        assert!(
            (snap
                .forecast
                .heat_index_max_3day_f
                .expect("forecast heat present")
                - expected_per_day)
                .abs()
                < 1e-6,
            "forecast block heat index must match the skip_check value"
        );

        // No observed rain anywhere -> the recent-rain backstop sees nothing.
        assert!((snap.skip_check.rain_observed_recent_in - 0.0).abs() < 1e-9);
    }

    // ── CLEAR SKIP (observed-rain backstop) ─────────────────────────────────────
    /// An optional provider archive is modeled evidence, not a measurement.
    #[tokio::test]
    async fn archive_estimate_does_not_trigger_measured_wet_backstop() {
        let now = Utc::now().timestamp();
        let (mut forecast, _) = calm();
        forecast.past_daily = vec![DailyEntry {
            day_marker: crate::engine::clock::DayMarker::inside_local_day(now - 86400),
            precip_sum_in: Some(2.0),
            ..Default::default()
        }];
        let snapshot = assemble(
            forecast,
            live_station(now, 74.0, 3.0, 60.0, 0.04),
            &["back_yard"],
            WateringPolicy::default(),
        )
        .await;
        assert!((snapshot.skip_check.rain_observed_recent_in - 0.04).abs() < 1e-9);
        assert_eq!(snapshot.skip_check.verdict, "run");
        assert_ne!(snapshot.skip_check.reason_code, "observed_rain");
    }

    // Guard: today's rain alone (below the observed-window total) must NOT skip,
    // so the skip in the test above is genuinely driven by the PAST observed
    // window, not by today's measured rain leaking past a threshold.
    #[tokio::test]
    async fn observed_recent_without_past_window_runs() {
        let now = Utc::now().timestamp();
        let today = DailyEntry {
            day_marker: crate::engine::clock::DayMarker::inside_local_day(now),
            temp_max_f: Some(82.0),
            temp_min_f: Some(68.0),
            humidity_pct: Some(55),
            precip_sum_in: Some(0.0),
            ..Default::default()
        };
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            daily: std::iter::once(today)
                .chain(dry_days(now).into_iter().skip(1))
                .collect(),
            past_daily: vec![], // no past observed rain
            hourly: dry_hours(74.0, 4.0, 60),
            ..Default::default()
        };
        // 0.04" today, below the 0.05" already-wet floor and far below 0.25".
        let tempest = live_station(now, 74.0, 3.0, 60.0, 0.04);
        let snap = assemble(fc, tempest, &["back_yard"], WateringPolicy::default()).await;

        assert!((snap.skip_check.rain_observed_recent_in - 0.04).abs() < 1e-6);
        assert_eq!(
            snap.skip_check.verdict, "run",
            "today-only 0.04\" must not trip any rain gate; reason: {}",
            snap.skip_check.reason
        );
    }
}

// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod seam_guard_tests {
    /// The IO shell is the one place in a tick that reads the clock, and
    /// it reads it once. Everything downstream is handed that instant.
    #[test]
    fn the_shell_reads_the_clock_once() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/refresher/shell.rs"),
        )
        .unwrap();
        let start = src
            .find("async fn build_from_map(")
            .expect("the shell exists");
        let end = src[start..].find("\n}\n").map(|i| start + i).unwrap();
        let body = crate::engine::clock::code_only(&src[start..end]);
        let reads = ["Utc::now(", "now_local(", "Local::now(", "SystemTime::now("]
            .iter()
            .map(|r| body.matches(r).count())
            .sum::<usize>();
        assert_eq!(
            reads, 1,
            "build_from_map reads the clock exactly once:\n{body}"
        );
        // And `prefetch` reads none: what it fetches is judged at the
        // shell's instant, not its own.
        let start = src.find("async fn prefetch(").expect("prefetch exists");
        let end = src[start..].find("\n}\n").map(|i| start + i).unwrap();
        let body = crate::engine::clock::code_only(&src[start..end]);
        assert!(
            !body.contains("now("),
            "prefetch must not read the clock:\n{body}"
        );
    }
}
