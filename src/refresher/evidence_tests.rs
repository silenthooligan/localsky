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

/// End-to-end binding: a zone-bound MQTT soil subscription lands in
/// sensor_history under the canonical `soilmoisture_<zone_slug>` key (the
/// bus recorder does this from a KeyedReading event), and a zone whose
/// `soil_sensor_id` points at `source:<mqtt_src>:soilmoisture_<zone_slug>`
/// resolves it through the SAME `resolve_soil_pct` path native channels use.
/// This is the engine half of the MQTT-soil fix; mqtt_subscribe.rs covers
/// the parse->emit half.
#[cfg(test)]
mod mqtt_soil_binding_tests {
    use super::resolve_soil_pct;
    use crate::persistence::runner;
    use crate::persistence::SensorHistoryStore;
    use crate::sources::bus_recorder::zone_soil_key;
    use rusqlite::Connection;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn fresh_store() -> SensorHistoryStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        SensorHistoryStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn zone_bound_mqtt_soil_resolves_to_zone_reading() {
        let store = fresh_store().await;
        // Simulate the bus recorder persisting a KeyedReading from a
        // zone-bound MQTT soil subscription on source "garden_mqtt".
        let key = zone_soil_key("back_yard");
        assert_eq!(key, "soilmoisture_back_yard");
        store
            .insert(crate::persistence::sensor_history::Reading {
                epoch: 1_700_000_000,
                source_id: "garden_mqtt".into(),
                key: key.clone(),
                value: 37.0,
            })
            .await
            .unwrap();

        // The zone binds the canonical channel id and resolves it exactly
        // like a native `source:` channel.
        let spec = format!("source:garden_mqtt:{key}");
        let map: HashMap<String, serde_json::Value> = HashMap::new();
        let pct = resolve_soil_pct(Some(&spec), &map, Some(&store)).await;
        assert_eq!(pct, Some(37.0));
    }

    #[tokio::test]
    async fn zone_bound_mqtt_soil_is_discoverable_as_soil_channel() {
        let store = fresh_store().await;
        store
            .insert(crate::persistence::sensor_history::Reading {
                epoch: 1_700_000_100,
                source_id: "garden_mqtt".into(),
                key: zone_soil_key("front_yard"),
                value: 52.0,
            })
            .await
            .unwrap();
        // The soil-channel discovery (LIKE 'soilmoisture%') must surface it,
        // so it shows up in /sensors/soil + the inventory + the picker.
        let chans = store.soil_channels().await.unwrap();
        let found = chans
            .iter()
            .find(|r| r.source_id == "garden_mqtt" && r.key == "soilmoisture_front_yard")
            .expect("zone-bound MQTT soil channel is discoverable");
        assert_eq!(found.value, 52.0);
    }
}

#[tokio::test]
async fn configured_missing_probe_is_distinct_from_unbound_zone() {
    let cfg = [Some("source:missing:soilmoisture1"), None]
        .into_iter()
        .enumerate()
        .map(|(index, id)| ZoneSoilCfg {
            slug: format!("zone_{index}"),
            name: format!("Zone {index}"),
            soil_sensor_id: id.map(str::to_string),
            soil_probes: Default::default(),
            soil_aggregation: Default::default(),
            saturation_pct: 70.0,
            target_min_pct: 30.0,
            sprinkler_type: Default::default(),
        })
        .collect::<Vec<_>>();
    let resolved = resolve_soil_zones(&cfg, &HashMap::new(), None).await;
    assert!(resolved[0].probe_configured);
    assert!(!resolved[1].probe_configured);
    assert!(resolved.iter().all(|z| z.pct.is_none()));
}

#[cfg(test)]
mod multiple_soil_probes {
    use super::*;
    use crate::config::{SoilAggregation, SoilProbeConfig};
    use crate::persistence::{runner, SensorHistoryStore};

    async fn store() -> SensorHistoryStore {
        let mut conn = Connection::open_in_memory().unwrap();
        runner::run(&mut conn).unwrap();
        SensorHistoryStore::new(Arc::new(Mutex::new(conn)))
    }
    fn zone() -> ZoneSoilCfg {
        ZoneSoilCfg {
            slug: "bed".into(),
            name: "Bed".into(),
            soil_sensor_id: None,
            soil_probes: (1..=4)
                .map(|n| {
                    let mut p = SoilProbeConfig::new(format!("source:mqtt:soilmoisture{n}"));
                    p.stale_after_s = 900;
                    p
                })
                .collect(),
            soil_aggregation: SoilAggregation::Median,
            saturation_pct: 70.0,
            target_min_pct: 30.0,
            sprinkler_type: Default::default(),
        }
    }
    async fn sample(store: &SensorHistoryStore, n: usize, value: f64, epoch: i64) {
        store
            .insert(crate::persistence::sensor_history::Reading {
                source_id: "mqtt".into(),
                key: format!("soilmoisture{n}"),
                value,
                epoch,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn arrival_order_cannot_select_the_saturated_probe() {
        for order in [[1, 2, 3, 4], [4, 3, 2, 1]] {
            let store = store().await;
            let now = Utc::now().timestamp();
            let values = [74.0, 40.0, 36.0, 29.0];
            for n in order {
                sample(&store, n, values[n - 1], now).await;
            }
            let zones = resolve_soil_zones(&[zone()], &HashMap::new(), Some(&store)).await;
            assert_eq!(zones[0].pct, Some(38.0));
            assert!(zones[0].pct.unwrap() < zones[0].saturation_pct);
        }
    }

    #[tokio::test]
    async fn live_peers_do_not_hide_a_stale_probe() {
        let store = store().await;
        let now = Utc::now().timestamp();
        sample(&store, 1, 74.0, now - 901).await;
        for n in 2..=4 {
            sample(&store, n, 29.0, now).await;
        }
        let cfg = [zone()];
        let zones = resolve_soil_zones(&cfg, &HashMap::new(), Some(&store)).await;
        assert_eq!(zones[0].pct, Some(29.0));
        let faults = detect_soil_probe_faults(&cfg, &HashMap::new(), Some(&store)).await;
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].sensor_id, "source:mqtt:soilmoisture1");
    }

    #[tokio::test]
    async fn all_stale_missing_invalid_or_excluded_keep_the_probe_hold() {
        let store = store().await;
        let now = Utc::now().timestamp();
        sample(&store, 1, 74.0, now - 901).await;
        sample(&store, 2, 101.0, now).await;
        sample(&store, 3, 0.0, now).await;
        let mut cfg = zone();
        let zones = resolve_soil_zones(&[cfg.clone()], &HashMap::new(), Some(&store)).await;
        assert!(zones[0].pct.is_none());
        assert!(zones[0].probe_configured);
        sample(&store, 4, 29.0, now).await;
        cfg.soil_probes[3].excluded_from_decisions = true;
        let zones = resolve_soil_zones(&[cfg], &HashMap::new(), Some(&store)).await;
        assert!(zones[0].pct.is_none());
        assert!(zones[0].probe_configured);
    }

    #[tokio::test]
    async fn one_fresh_included_probe_is_enough_and_future_data_is_rejected() {
        let store = store().await;
        let now = Utc::now().timestamp();
        sample(&store, 1, 74.0, now + 3600).await;
        sample(&store, 4, 29.0, now).await;
        let mut cfg = zone();
        cfg.soil_probes[1].excluded_from_decisions = true;
        let zones = resolve_soil_zones(&[cfg], &HashMap::new(), Some(&store)).await;
        assert_eq!(zones[0].pct, Some(29.0));
    }

    #[tokio::test]
    async fn ha_measurement_timestamp_controls_freshness() {
        let now = Utc::now().timestamp();
        let mut cfg = zone();
        cfg.soil_probes = vec![SoilProbeConfig {
            sensor_id: "ha:sensor.soil".into(),
            stale_after_s: 900,
            excluded_from_decisions: false,
        }];
        let mut map = HashMap::from([(
            "sensor.soil".into(),
            serde_json::json!({"state": "29", "last_updated": chrono::DateTime::from_timestamp(now - 901, 0).unwrap().to_rfc3339()}),
        )]);
        assert!(resolve_soil_zones(&[cfg.clone()], &map, None).await[0]
            .pct
            .is_none());
        map.get_mut("sensor.soil").unwrap()["last_reported"] =
            chrono::DateTime::from_timestamp(now, 0)
                .unwrap()
                .to_rfc3339()
                .into();
        assert_eq!(
            resolve_soil_zones(&[cfg], &map, None).await[0].pct,
            Some(29.0)
        );
    }
}
