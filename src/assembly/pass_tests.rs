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
mod engine_params_tests {
    use super::*;

    fn base_inputs() -> Inputs {
        Inputs {
            forecast_in: Some(0.0),
            rain_today_forecast_in: Some(0.0),
            rain_intensity_now_in_hr: Some(0.0),
            rain_next_4h_in: Some(0.0),
            rain_3day_weighted_in: Some(0.0),
            rain_7day_weighted_in: Some(0.0),
            temp_now_f: 70.0,
            wind_now_mph: 3.0,
            wind_max_today_mph: 6.0,
            temp_min_24h_f: Some(60.0),
            temp_max_3day_f: 80.0,
            humidity_now_pct: 55.0,
            days_since_significant_rain: 1,
            max_wind_mph: 10.0,
            min_temp_f: 38.0,
            rain_skip_in: 0.25,
            frost_skip_soil_f: 35.0,
            when: crate::engine::clock::DecisionTime::at(
                crate::engine::calendar::Calendar::utc(),
                1_700_000_000,
            ),
            ..Default::default()
        }
    }

    /// Regression for the params-threading fix: a non-default
    /// `already_wet_in` must reach the live decision. Before the fix,
    /// apply_engine constructed SkipRuleParams::default() locally, so
    /// the operator's config value never changed any verdict.
    #[test]
    fn user_already_wet_threshold_flips_verdict() {
        let scripts = CompiledScripts::compile(&[]);
        let mut inputs = base_inputs();
        inputs.rain_today_in = 0.07;

        // Default threshold (0.05"): 0.07" today is "already wet" -> skip.
        let mut snap = IrrigationSnapshot::default();
        let defaults = crate::config::schema::SkipRuleParams::default();
        apply_engine(&mut snap, &inputs, &scripts, &[], &defaults);
        assert_eq!(snap.skip_check.verdict, "skip");
        assert!(snap.skip_check.reason.starts_with("Already wet"));

        // Operator raises the floor to 0.10": the same inputs must run.
        let mut tuned = crate::config::schema::SkipRuleParams::default();
        tuned.already_wet_in = 0.10;
        let mut snap2 = IrrigationSnapshot::default();
        apply_engine(&mut snap2, &inputs, &scripts, &[], &tuned);
        assert_eq!(snap2.skip_check.verdict, "run");
        // The trace must agree (same params reach decide_traced).
        assert_eq!(snap2.decision_trace.as_ref().unwrap().verdict, "run");
    }

    fn zone(slug: &str, pct: Option<f64>, governed: bool) -> ZoneSoil {
        ZoneSoil {
            slug: slug.into(),
            name: slug.into(),
            pct,
            saturation_pct: 70.0,
            target_min_pct: 30.0,
            governed_by_soil_model: governed,
            ..Default::default()
        }
    }

    fn snapshot_for(inputs: &Inputs, scripts: &CompiledScripts) -> IrrigationSnapshot {
        let mut snapshot = IrrigationSnapshot {
            zones: inputs
                .soil_zones
                .iter()
                .map(|zone| crate::model::ZoneState {
                    slug: zone.slug.clone(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        apply_engine(
            &mut snapshot,
            inputs,
            scripts,
            &[],
            &crate::config::schema::SkipRuleParams::default(),
        );
        snapshot
    }

    fn user_hold() -> CompiledScripts {
        CompiledScripts::compile(&[crate::config::schema::ScriptRule {
            id: "owner_hold".into(),
            name: "Do not water during the garden party".into(),
            enabled: true,
            script: "true".into(),
        }])
    }

    #[test]
    fn scripts_hold_normal_heat_and_soil_model_runs_on_both_zone_surfaces() {
        let scripts = user_hold();
        for kind in ["normal", "heat", "soil_model", "soil_floor"] {
            let mut inputs = base_inputs();
            inputs.soil_zones = vec![zone("front", Some(40.0), kind == "soil_model")];
            if kind == "heat" {
                inputs.temp_max_3day_f = 110.0;
                inputs.days_since_significant_rain = 10;
            } else if kind == "soil_model" || kind == "soil_floor" {
                inputs.rain_next_4h_in = Some(1.0);
                if kind == "soil_floor" {
                    inputs.soil_zones[0].pct = Some(20.0);
                }
            }
            let mut snapshot = snapshot_for(&inputs, &scripts);
            snapshot.seven_day_verdicts = vec![
                DayVerdict {
                    time_epoch: 1_700_000_000,
                    verdict: "skip".into(),
                    reason_code: "tomorrow_rain".into(),
                    ..Default::default()
                },
                DayVerdict {
                    day_offset: 1,
                    time_epoch: 1_700_086_400,
                    verdict: "skip".into(),
                    reason_code: "wind_forecast".into(),
                    ..Default::default()
                },
            ];
            Finalize {
                watered: vec![],
                forecast: Arc::new(ForecastSnapshot::default()),
                now_epoch: 1_700_000_000,
            }
            .apply(&mut snapshot, &WateringPolicy::default());
            assert!(snapshot.skip_check.will_skip, "{kind}: aggregate must hold");
            for verdict in [
                &snapshot.zone_verdicts[0],
                snapshot.zones[0].verdict.as_ref().unwrap(),
            ] {
                assert_eq!(verdict.verdict, "skip", "{kind}");
                assert_eq!(verdict.reason_code, "owner_hold", "{kind}");
                assert_eq!(verdict.source, "script", "{kind}");
            }
            assert_eq!(snapshot.decision_trace.as_ref().unwrap().verdict, "skip");
            assert_eq!(
                snapshot.decision_trace.as_ref().unwrap().reason_code,
                "owner_hold"
            );
            // One WINNER. The heat rung the script overrode keeps outcome
            // "fired" plus `overridden_by`; it is no longer rewritten to
            // "passed", which used to erase the fact that it tripped.
            assert_eq!(
                snapshot
                    .decision_trace
                    .as_ref()
                    .unwrap()
                    .rules
                    .iter()
                    .filter(|r| r.decided())
                    .count(),
                1
            );
            // Whatever set a gate aside is, by construction, the decision that
            // went on to win the trace.
            let reason_code = snapshot
                .decision_trace
                .as_ref()
                .unwrap()
                .reason_code
                .clone();
            assert!(
                snapshot
                    .decision_trace
                    .as_ref()
                    .unwrap()
                    .rules
                    .iter()
                    .filter(|r| r.overridden())
                    .all(|r| r.overridden_by.as_deref() == Some(reason_code.as_str())),
                "a set-aside gate must name what set it aside; got {:?}",
                snapshot
                    .decision_trace
                    .as_ref()
                    .unwrap()
                    .rules
                    .iter()
                    .filter(|r| r.overridden())
                    .map(|r| (r.id.clone(), r.overridden_by.clone()))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                snapshot
                    .decision_trace
                    .as_ref()
                    .unwrap()
                    .rules
                    .iter()
                    .filter(|r| r.id == "owner_hold")
                    .count(),
                1
            );
            assert_eq!(
                snapshot.seven_day_verdicts[0].verdict, "skip",
                "{kind}: today's tile must retain the script hold"
            );
            assert_eq!(
                snapshot.seven_day_verdicts[0].reason_code, "owner_hold",
                "{kind}"
            );
            assert_eq!(
                snapshot.seven_day_verdicts[1].reason_code, "wind_forecast",
                "future weather stays a projection"
            );
        }
    }

    #[test]
    fn script_holds_remain_typed_and_traced_when_a_weather_gate_wins() {
        for script in ["true", "bad syntax (", "missing_function()", "42"] {
            let scripts = CompiledScripts::compile(&[crate::config::schema::ScriptRule {
                id: "owner_hold".into(),
                name: "Owner rule".into(),
                enabled: true,
                script: script.into(),
            }]);
            let mut inputs = base_inputs();
            inputs.temp_now_f = 20.0;
            inputs.soil_zones = vec![zone("front", None, false)];
            let snapshot = snapshot_for(&inputs, &scripts);
            assert_eq!(snapshot.skip_check.reason_code, "freeze_now");
            assert_eq!(snapshot.zone_verdicts[0].reason_code, "freeze_now");
            let hold = snapshot.skip_check.script_hold.as_ref().unwrap();
            assert_eq!(hold.id, "owner_hold");
            let trace = snapshot.decision_trace.as_ref().unwrap();
            assert_eq!(trace.reason_code, "freeze_now");
            let row = trace.rules.iter().find(|r| r.id == hold.id).unwrap();
            assert_eq!(row.category, "script");
            assert!(row.detail.contains(&hold.reason));
            assert_eq!(
                trace.rules.iter().filter(|r| r.outcome == "fired").count(),
                1
            );
            let json = serde_json::to_value(&snapshot).unwrap();
            assert_eq!(json["skip_check"]["script_hold"]["id"], "owner_hold");
            let restored: IrrigationSnapshot = serde_json::from_value(json).unwrap();
            assert_eq!(
                restored.skip_check.script_hold,
                snapshot.skip_check.script_hold
            );
        }
    }

    #[test]
    fn valid_false_and_empty_scripts_add_no_hold_under_weather_or_clear_skies() {
        for script in ["false", "\"\""] {
            let scripts = CompiledScripts::compile(&[crate::config::schema::ScriptRule {
                id: "noop".into(),
                name: "No hold".into(),
                enabled: true,
                script: script.into(),
            }]);
            for freeze in [false, true] {
                let mut inputs = base_inputs();
                inputs.soil_zones = vec![zone("front", None, false)];
                if freeze {
                    inputs.temp_now_f = 20.0;
                }
                let snapshot = snapshot_for(&inputs, &scripts);
                assert!(snapshot.skip_check.script_hold.is_none());
                assert_eq!(snapshot.skip_check.will_skip, freeze);
                assert!(!snapshot
                    .decision_trace
                    .unwrap()
                    .rules
                    .iter()
                    .any(|r| r.category == "script"));
            }
        }
    }

    #[test]
    fn all_saturated_soil_model_zones_remain_held_with_forecast_rain() {
        let mut inputs = base_inputs();
        inputs.rain_next_4h_in = Some(1.0);
        inputs.soil_zones = vec![
            zone("front", Some(80.0), true),
            zone("back", Some(85.0), true),
        ];
        let snapshot = snapshot_for(&inputs, &CompiledScripts::default());
        assert!(snapshot.skip_check.will_skip);
        assert_eq!(snapshot.skip_check.reason_code, "soil_saturation");
        assert!(snapshot
            .zone_verdicts
            .iter()
            .all(|zone| zone.verdict == "skip"));
    }

    #[test]
    fn scripts_reach_restriction_exempt_zones_even_while_the_aggregate_holds() {
        use crate::config::schema::{SprinklerType, WateringRestriction};
        let mut inputs = base_inputs();
        inputs.soil_zones = vec![zone("lawn", None, false), zone("beds", None, false)];
        inputs.soil_zones[1].sprinkler_type = SprinklerType::Drip;
        inputs.watering_restrictions = vec![WateringRestriction {
            exempt_sprinklers: vec![SprinklerType::Drip],
            allowed_weekdays: vec![],
            ..Default::default()
        }];
        // An empty allowed-weekday set is not a restriction. Use a day other
        // than the known Tuesday decision instant from base_inputs.
        inputs.watering_restrictions[0].allowed_weekdays = vec![0];
        let snapshot = snapshot_for(&inputs, &user_hold());
        assert_eq!(snapshot.skip_check.reason_code, "restrictions");
        assert_eq!(snapshot.zone_verdicts[1].verdict, "skip");
        assert_eq!(snapshot.zone_verdicts[1].reason_code, "owner_hold");
    }

    #[test]
    fn completed_zone_decisions_drive_the_yard_and_trace_headlines() {
        use crate::config::schema::{SprinklerType, WateringRestriction};
        let mut inputs = base_inputs();
        inputs.soil_zones = vec![zone("lawn", None, false), zone("beds", None, false)];
        inputs.soil_zones[1].sprinkler_type = SprinklerType::Drip;
        inputs.watering_restrictions = vec![WateringRestriction {
            exempt_sprinklers: vec![SprinklerType::Drip],
            allowed_weekdays: vec![0],
            ..Default::default()
        }];
        let mut snapshot = snapshot_for(&inputs, &CompiledScripts::default());
        assert!(!snapshot.skip_check.will_skip);
        assert!(snapshot.skip_check.reason.contains("1 of 2 zones"));
        assert_eq!(snapshot.zone_verdicts[0].verdict, "skip");
        assert_eq!(snapshot.zone_verdicts[1].verdict, "run");
        assert_eq!(
            snapshot.decision_trace.as_ref().unwrap().reason,
            snapshot.skip_check.reason
        );
        snapshot.seven_day_verdicts = vec![DayVerdict {
            time_epoch: 1_700_000_000,
            verdict: "skip".into(),
            reason_code: "restrictions".into(),
            ..Default::default()
        }];
        Finalize {
            watered: vec![],
            forecast: Arc::new(ForecastSnapshot::default()),
            now_epoch: 1_700_000_000,
        }
        .apply(&mut snapshot, &WateringPolicy::default());
        assert!(snapshot.seven_day_verdicts[0].mixed_hold);
        assert_eq!(snapshot.seven_day_verdicts[0].verdict, "run");

        inputs.watering_restrictions.clear();
        inputs.rain_next_4h_in = Some(1.0);
        inputs.soil_zones[1].governed_by_soil_model = true;
        let snapshot = snapshot_for(&inputs, &CompiledScripts::default());
        assert!(!snapshot.skip_check.will_skip);
        assert_eq!(snapshot.zone_verdicts[0].verdict, "skip");
        assert_eq!(snapshot.zone_verdicts[1].verdict, "run");
        assert_eq!(snapshot.decision_trace.as_ref().unwrap().verdict, "run");

        inputs.temp_now_f = 20.0;
        let frozen = snapshot_for(&inputs, &CompiledScripts::default());
        assert!(frozen.skip_check.will_skip);
        assert!(frozen
            .zone_verdicts
            .iter()
            .all(|zone| zone.verdict == "skip"));
    }
}

#[cfg(test)]
mod budget_default_tests {
    use super::{agronomic_budget_default, kc_depth_for, ZoneAgronomyCfg};
    use crate::config::schema::{GrassSpecies, SoilTexture, SprinklerType};
    use std::collections::HashMap;

    fn agronomy_for(
        species: GrassSpecies,
        root_override: Option<f64>,
    ) -> HashMap<String, ZoneAgronomyCfg> {
        let mut m = HashMap::new();
        m.insert(
            "back_yard".to_string(),
            ZoneAgronomyCfg {
                sprinkler_type: SprinklerType::Spray,
                precip_rate_mm_hr: None,
                soil_texture: SoilTexture::SandyLoam,
                slope_pct: 0.0,
                species,
                root_depth_mm: root_override,
                mad_pct_override: None,
                scheduling_model: None,
            },
        );
        m
    }

    /// The moisture projection reads the zone's own species curve, not a
    /// constant: a dormant midwinter lawn dries at its winter Kc, and the
    /// same day in the southern hemisphere reads as summer instead.
    /// Before this, every turf zone projected at a flat 1.08 whatever the
    /// species, the season or the hemisphere.
    #[test]
    fn the_projection_reads_species_season_and_hemisphere() {
        let ag = agronomy_for(GrassSpecies::Bermuda, None);
        // Jan 15 (doy 15) and Jul 15 (doy 196), north and south.
        let (kc_north_jan, _) = kc_depth_for("back_yard", &ag, 15, 28.5);
        let (kc_north_jul, _) = kc_depth_for("back_yard", &ag, 196, 28.5);
        let (kc_south_jan, _) = kc_depth_for("back_yard", &ag, 15, -33.9);
        let (kc_south_jul, _) = kc_depth_for("back_yard", &ag, 196, -33.9);
        assert!(
            kc_north_jan < kc_north_jul,
            "north: January is dormant, July is peak ({kc_north_jan} vs {kc_north_jul})"
        );
        assert!(
            kc_south_jan > kc_south_jul,
            "south: the seasons invert ({kc_south_jan} vs {kc_south_jul})"
        );
        // The hemispheres mirror: the north's January is the south's July.
        assert!((kc_north_jan - kc_south_jul).abs() < 1e-9);
        // Nothing here is the old flat constant.
        for kc in [kc_north_jan, kc_north_jul, kc_south_jan, kc_south_jul] {
            assert!((kc - 1.08).abs() > 1e-9, "the 1.08 constant is gone");
        }
    }

    /// Root depth follows the species profile, and a per-zone override
    /// wins, the same precedence the soil model's bucket uses.
    #[test]
    fn the_projection_depth_follows_species_then_override() {
        let (_, bermuda) = kc_depth_for(
            "back_yard",
            &agronomy_for(GrassSpecies::Bermuda, None),
            196,
            28.5,
        );
        let (_, centipede) = kc_depth_for(
            "back_yard",
            &agronomy_for(GrassSpecies::Centipede, None),
            196,
            28.5,
        );
        assert_eq!(bermuda, 200.0, "Bermuda roots deeper than the old constant");
        assert_eq!(centipede, 100.0, "Centipede is shallower");
        let (_, overridden) = kc_depth_for(
            "back_yard",
            &agronomy_for(GrassSpecies::Bermuda, Some(275.0)),
            196,
            28.5,
        );
        assert_eq!(overridden, 275.0);
    }

    /// The starting weekly target scales with the species' own peak crop
    /// coefficient against reference turf, so a planting that transpires
    /// harder starts on more water. Reference turf keeps the inch a week
    /// every extension guide recommends.
    #[test]
    fn the_starting_target_follows_the_species_curve() {
        use crate::agronomy::default_weekly_target_in;
        // Both warm-season, so both sit on the table's 0.85 Kc_mid and
        // therefore on the same starting target. They differed before
        // only because the catalog had invented a spread the cited table
        // does not describe.
        assert_eq!(default_weekly_target_in("st_augustine"), (0.85, 2));
        assert_eq!(default_weekly_target_in("bermuda"), (0.85, 2));
        // Vegetables transpire HARDER than turf. The old name-based guess
        // gave anything containing "garden" half an inch, so a vegetable
        // bed started on well under half the water it wants.
        assert_eq!(default_weekly_target_in("vegetable_garden"), (1.15, 2));
        // Established plantings watered deeply and infrequently.
        assert_eq!(default_weekly_target_in("ornamental_shrubs"), (0.55, 1));
        assert_eq!(default_weekly_target_in("drip_xeriscape"), (0.35, 1));
        // An unknown species takes the generic profile, not turf.
        assert_eq!(default_weekly_target_in("mystery"), (0.70, 2));
    }

    /// The zone's NAME no longer decides its water. A lawn a previous
    /// owner named for a flower bed is watered as the lawn the operator
    /// declared it to be, and a bed named for its corner of the yard is
    /// watered as a bed.
    #[test]
    fn the_starting_target_ignores_what_the_zone_is_called() {
        use crate::config::schema::{Config, GrassSpecies};
        use crate::refresher::WateringPolicy;
        let mut cfg = Config::default();
        for (slug, species) in [
            ("back_yard_shrubs", GrassSpecies::Bermuda),
            ("north_corner", GrassSpecies::OrnamentalShrubs),
        ] {
            cfg.zones.insert(
                slug.into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": crate::engine::species_slug(species),
                    "soil_texture": "sandy_loam",
                    "sprinkler_type": "spray",
                    "controller_id": "os_main",
                    "controller_station": "1"
                }))
                .unwrap(),
            );
        }
        let policy = WateringPolicy::from_config(&cfg);
        let row = |slug: &str| {
            policy
                .budget_zones
                .iter()
                .find(|b| b.slug == slug)
                .unwrap_or_else(|| panic!("no row for {slug}"))
        };
        // Named like a bed, planted as bermuda: waters as bermuda.
        assert_eq!(row("back_yard_shrubs").default_budget_in, 0.85);
        assert_eq!(row("back_yard_shrubs").default_sessions, 2);
        // Named like a lawn, planted as shrubs: waters as shrubs.
        assert_eq!(row("north_corner").default_budget_in, 0.55);
        assert_eq!(row("north_corner").default_sessions, 1);
    }

    /// A zone with no agronomy config (an install that never ran the
    /// wizard) takes the neutral coefficient the rest of the assembly
    /// falls back to, never a slug guess.
    #[test]
    fn an_unconfigured_zone_takes_the_neutral_coefficient() {
        let empty = HashMap::new();
        for slug in [
            "back_yard",
            "back_yard_shrubs",
            "front_garden",
            "flower_bed",
        ] {
            let (kc, depth) = kc_depth_for(slug, &empty, 196, 28.5);
            assert_eq!(kc, 1.0, "{slug}");
            assert_eq!(depth, 150.0, "{slug}");
        }
    }

    #[test]
    fn turf_slugs_get_legacy_one_inch_two_sessions() {
        for slug in ["back_yard", "front_yard", "side_yard", "lawn"] {
            assert_eq!(
                agronomic_budget_default(slug),
                (1.00, 2),
                "turf slug {slug} must reproduce the legacy 1.0\"/2 default"
            );
        }
    }

    #[test]
    fn bed_slugs_get_legacy_half_inch_one_session() {
        for slug in ["back_yard_shrubs", "front_garden", "flower_bed"] {
            assert_eq!(
                agronomic_budget_default(slug),
                (0.50, 1),
                "shrub/garden/bed slug {slug} must reproduce the legacy 0.5\"/1 default"
            );
        }
    }

    /// The zone editor shows the inferred default as the placeholder of the
    /// two budget fields. It compiles for the browser, where this module does
    /// not, so it carries its own copy of the rule; the two must never drift,
    /// or the box would promise a number the yard is not watering on.
    #[test]
    fn the_zone_editor_placeholder_matches_the_engine_default() {
        use crate::components::settings::zones::inferred_weekly_target;
        use crate::config::schema::{Config, GrassSpecies};
        use crate::refresher::WateringPolicy;
        for species in [
            GrassSpecies::StAugustine,
            GrassSpecies::Bermuda,
            GrassSpecies::TallFescue,
            GrassSpecies::OrnamentalShrubs,
            GrassSpecies::VegetableGarden,
            GrassSpecies::DripXeriscape,
        ] {
            let slug = crate::engine::species_slug(species);
            let mut cfg = Config::default();
            cfg.zones.insert(
                "z".into(),
                serde_json::from_value(serde_json::json!({
                    "display_name": "Z",
                    "area_sqft": 1000.0,
                    "species": slug,
                    "soil_texture": "sandy_loam",
                    "sprinkler_type": "spray",
                    "controller_id": "os_main",
                    "controller_station": "1"
                }))
                .unwrap(),
            );
            let row = WateringPolicy::from_config(&cfg).budget_zones.remove(0);
            assert_eq!(
                inferred_weekly_target(slug),
                (row.default_budget_in, row.default_sessions),
                "{slug}: the editor's placeholder must be the target the \
                 engine actually waters on"
            );
        }
    }

    /// The zone editor derives the rain-cap placeholder client-side (the
    /// engine's soil catalog compiles only server-side), so it carries
    /// its own copy of each texture's FC-WP spread. Pinned against
    /// `soil_catalog::taw_mm` for every texture at several root depths,
    /// or the box would promise a cap the balance is not clipping at.
    #[test]
    fn the_zone_editor_rain_cap_matches_the_soil_catalog() {
        use crate::components::settings::zones::derived_rain_cap_in;
        use crate::config::schema::SoilTexture;
        let textures = [
            ("sand", SoilTexture::Sand),
            ("loamy_sand", SoilTexture::LoamySand),
            ("sandy_loam", SoilTexture::SandyLoam),
            ("loam", SoilTexture::Loam),
            ("silt_loam", SoilTexture::SiltLoam),
            ("clay_loam", SoilTexture::ClayLoam),
            ("clay", SoilTexture::Clay),
        ];
        for (slug, texture) in textures {
            for root_mm in [100.0, 150.0, 200.0, 250.0, 300.0, 400.0] {
                let editor_mm = derived_rain_cap_in(slug, root_mm) * 25.4;
                let engine_mm = crate::engine::taw_mm(texture, root_mm);
                assert!(
                    (editor_mm - engine_mm).abs() < 1e-9,
                    "{slug} at {root_mm} mm roots: editor {editor_mm}, engine {engine_mm}"
                );
            }
        }
        // An unknown texture slug takes the sandy_loam spread, the same
        // default the form loads for an unset texture.
        assert_eq!(
            derived_rain_cap_in("mystery", 150.0),
            derived_rain_cap_in("sandy_loam", 150.0)
        );
    }
}
