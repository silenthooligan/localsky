// Snapshot tests on /api/v1/* response shapes.
//
// These tests don't exercise the HTTP routing or live data; they
// serialize a default-state instance of each response type and lock the
// rendered JSON via `insta::assert_json_snapshot!`. The point is to
// catch silent breaking changes to the public API contract:
//
//   - field rename     -> snapshot diff
//   - field removed    -> snapshot diff
//   - field type change-> snapshot diff
//   - default value change -> snapshot diff
//
// On an intentional API change, run `cargo insta review` to accept the
// new shape and bump the `api_version` constant in src/api/info.rs.
//
// The /api/v1 prefix is documented as stable in docs/src/api.md:
//   MAJOR: breaking shape change (field removed, renamed, retyped)
//   MINOR: additive (new optional field, new endpoint)
//   PATCH: bug fix that does not alter the contract
//
// Snapshots live alongside the test in src/api/snapshots/.

#[cfg(test)]
mod tests {
    use crate::forecast::snapshot::ForecastSnapshot;
    use crate::ha::snapshot::IrrigationSnapshot;
    use crate::tempest::state::Snapshot as TempestSnapshot;
    use insta::assert_json_snapshot;
    use serde_json::json;

    /// `/api/v1/info` shape. Locked separately from the test in info.rs
    /// (which validates SemVer format) because that one doesn't catch
    /// added or renamed fields. Snapshots the REAL `info::Info` struct (not a
    /// hand-rolled subset), so a rename or removal of ANY field the HACS
    /// integration reads (uuid, auth_required, has_irrigation, ...) trips this
    /// gate. The volatile version + uuid fields are redacted so a crate/api
    /// version bump does not churn the snap.
    #[test]
    fn info_v1_shape() {
        // Fixed placeholder values for the version + uuid fields (insta is built
        // without the redactions feature), so a crate/api version bump does not
        // churn this SHAPE snapshot; the real values are validated by info.rs's
        // own SemVer test. A field rename/removal still trips this gate.
        let info = super::super::info::Info {
            service: "localsky",
            service_version: "0.0.0-test",
            api_version: "0.0.0-test",
            api_prefix: "/api/v1",
            license: "Apache-2.0",
            repository: "https://github.com/silenthooligan/localsky",
            dry_run: false,
            demo: false,
            auth_required: false,
            uuid: Some("00000000-0000-0000-0000-000000000000".into()),
            has_irrigation: false,
            nerd_mode_default: false,
        };
        assert_json_snapshot!("info_v1", info);
    }

    /// `/api/v1/snapshot` (Tempest weather). Default-state instance so
    /// every field renders with a deterministic value (numeric 0,
    /// empty string / vec, None as null).
    #[test]
    fn tempest_v1_shape() {
        assert_json_snapshot!("tempest_v1", TempestSnapshot::default());
    }

    /// `/api/v1/irrigation/snapshot`.
    #[test]
    fn irrigation_v1_shape() {
        assert_json_snapshot!("irrigation_v1", IrrigationSnapshot::default());
    }

    /// Flow surfacing: the snapshot must carry the controller's flow_meter
    /// capability flag and live flow_gpm reading. None (no meter) serializes
    /// as JSON null so non-flow setups render nothing; a real value (incl.
    /// 0.0 "meter present, zero flow") serializes as the number.
    #[test]
    fn snapshot_flow_serializes_present_and_none() {
        // Default: no meter, no reading.
        let none = IrrigationSnapshot::default();
        let v = serde_json::to_value(&none).unwrap();
        assert_eq!(v["flow_meter"], serde_json::json!(false));
        assert_eq!(v["flow_gpm"], serde_json::Value::Null);

        // Meter present, live reading.
        let mut present = IrrigationSnapshot::default();
        present.flow_meter = true;
        present.flow_gpm = Some(3.5);
        let v = serde_json::to_value(&present).unwrap();
        assert_eq!(v["flow_meter"], serde_json::json!(true));
        assert_eq!(v["flow_gpm"], serde_json::json!(3.5));

        // Meter present but zero flow is distinct from "no meter": Some(0.0)
        // serializes as 0.0, not null.
        let mut zero = IrrigationSnapshot::default();
        zero.flow_meter = true;
        zero.flow_gpm = Some(0.0);
        let v = serde_json::to_value(&zero).unwrap();
        assert_eq!(v["flow_gpm"], serde_json::json!(0.0));
    }

    /// Round-trip: a snapshot serialized without the flow fields (older
    /// producer) deserializes with flow_meter=false / flow_gpm=None thanks
    /// to `#[serde(default)]`, so the additive fields don't break the SSE
    /// contract the HA integration consumes.
    #[test]
    fn snapshot_flow_fields_default_when_absent() {
        // Start from a fully-populated default, drop the two flow keys to
        // simulate an older producer, and confirm it still deserializes.
        let mut v = serde_json::to_value(IrrigationSnapshot::default()).unwrap();
        v.as_object_mut().unwrap().remove("flow_meter");
        v.as_object_mut().unwrap().remove("flow_gpm");
        let snap: IrrigationSnapshot = serde_json::from_value(v).unwrap();
        assert!(!snap.flow_meter);
        assert_eq!(snap.flow_gpm, None);
    }

    /// Household units carry on the snapshot (display-plumbing). The default
    /// serializes as "imperial"; a metric deployment serializes as "metric";
    /// and a snapshot from an older producer (no `units` key) deserializes to
    /// Imperial via `#[serde(default)]`, so the additive field never breaks the
    /// SSE/HACS contract.
    #[test]
    fn snapshot_units_serializes_and_defaults() {
        use crate::config::schema::Units;

        let default = IrrigationSnapshot::default();
        let v = serde_json::to_value(&default).unwrap();
        assert_eq!(v["units"], serde_json::json!("imperial"));

        let mut metric = IrrigationSnapshot::default();
        metric.units = Units::Metric;
        let v = serde_json::to_value(&metric).unwrap();
        assert_eq!(v["units"], serde_json::json!("metric"));

        // Older producer (no units key) -> Imperial via serde default.
        let mut v = serde_json::to_value(IrrigationSnapshot::default()).unwrap();
        v.as_object_mut().unwrap().remove("units");
        let snap: IrrigationSnapshot = serde_json::from_value(v).unwrap();
        assert_eq!(snap.units, Units::Imperial);
    }

    /// `/api/v1/forecast/snapshot`.
    #[test]
    fn forecast_v1_shape() {
        assert_json_snapshot!("forecast_v1", ForecastSnapshot::default());
    }

    /// `/api/v1/sources/openmeteo/models`. The whole static catalog:
    /// locks ids, labels, agencies, and regions, so a model id rename
    /// (which would break saved configs) shows up as a snapshot diff.
    #[test]
    fn openmeteo_models_v1_shape() {
        assert_json_snapshot!(
            "openmeteo_models_v1",
            crate::forecast::model_catalog::models()
        );
    }

    /// `/api/v1/radar/windgrid` record shape. Locks the grib2json-style
    /// envelope leaflet-velocity parses (camelCase header keys, U then
    /// V, parameterCategory 2 / parameterNumber 2 and 3). Two-value
    /// data arrays keep the snapshot readable; the real handler always
    /// emits nx*ny values (asserted in api::windgrid's unit tests).
    #[test]
    fn radar_windgrid_v1_shape() {
        let records = super::super::windgrid::make_records(
            &super::super::windgrid::test_fixture_grid(),
            "2026-06-12T14:00:00Z",
            vec![1.25, -0.5],
            vec![0.0, 3.5],
        );
        assert_json_snapshot!("radar_windgrid_v1", records);
    }

    /// `/api/v1/radar/tropical` shape. Locks the normalized GeoJSON
    /// FeatureCollection contract radar.js renders: uniform per-storm
    /// property bag (kind/id/name/term/agency/basin/classification/
    /// intensity_kt/pressure_mb/movement/updated) over Point/
    /// LineString/Polygon geometry, plus the per-source health array.
    /// Built deterministically from the embedded recon fixtures so all
    /// three agency normalizers (NHC/CPHC, JMA, JTWC) are exercised.
    #[test]
    fn radar_tropical_v1_shape() {
        assert_json_snapshot!(
            "radar_tropical_v1",
            super::super::tropical::test_fixture_collection()
        );
    }

    /// Sanity-check the action POST envelope (the HACS integration's
    /// run_zone / stop_all services write JSON matching this shape).
    #[test]
    fn irrigation_action_envelope() {
        let envelope = json!({
            "kind": "run",
            "zone": "back_yard",
            "seconds": 600,
        });
        assert_json_snapshot!("irrigation_action_run", envelope);
    }

    /// `/api/v1/forecast/bias` shape. Locked at the identity model so
    /// the rendered JSON is deterministic (every month, multiplier 1.0,
    /// samples 0); the actual bias values are integration-side and
    /// vary per deployment.
    #[test]
    fn forecast_bias_v1_shape() {
        use crate::engine::forecast_bias::{BiasModel, DEFAULT_WINDOW_DAYS, MIN_OBSERVATIONS};
        let model = BiasModel::identity();
        let months: Vec<_> = (1..=12u32)
            .map(|m| {
                json!({
                    "month": m,
                    "multiplier": model.multiplier_for(m),
                    "samples": model.sample_count_for(m),
                    "description": model.describe_month(m),
                })
            })
            .collect();
        let body = json!({
            "current_month_multiplier": 1.0,
            "current_month": 1,
            "min_observations_required": MIN_OBSERVATIONS,
            "window_days": DEFAULT_WINDOW_DAYS,
            "months": months,
        });
        assert_json_snapshot!("forecast_bias_v1", body);
    }

    /// `/api/v1/health` response shape (declared contractual by the
    /// API_VERSION changelog: soil_probe_faults landed in 1.8.0). The
    /// handler assembles this from live state, so the snapshot locks a
    /// fully-populated instance with FIXED placeholder values for the
    /// volatile fields (version, uptime, epochs), the same way info_v1
    /// pins version/uuid. One deterministic entry per nested collection
    /// locks the per-entry shapes (SourceFreshness, ControllerSummary,
    /// SoilProbeFault, HaIntegration, ConditionsProvenance) that would
    /// otherwise vanish behind their skip_serializing_if attributes.
    #[test]
    fn health_v1_shape() {
        use super::super::health::{
            ConditionsProvenance, ControllerSummary, HaIntegration, HealthResponse,
            SourceFreshness, SubsystemReport,
        };
        let health = HealthResponse {
            status: "ok",
            config_present: true,
            version: "0.0.0-test",
            schema_version: Some(0),
            uptime_s: 0,
            subsystems: SubsystemReport {
                config_store: "ok",
                persistence: "ok",
            },
            sources: vec![SourceFreshness {
                id: "open_meteo".into(),
                kind: "open_meteo",
                enabled: true,
                last_seen_epoch: Some(0),
                stale_for_s: Some(0),
                status: "active",
            }],
            controllers: vec![ControllerSummary {
                id: "opensprinkler".into(),
                kind: "opensprinkler",
                default: true,
                enabled: true,
            }],
            soil_probe_faults: vec![crate::ha::snapshot::SoilProbeFault {
                zone_slug: "back_yard".into(),
                zone_name: "Back yard".into(),
                sensor_id: "source:ecowitt_gw:soilmoisture1".into(),
                since_epoch: Some(0),
            }],
            ha: Some(HaIntegration {
                env_configured: false,
                reachable: false,
                snapshot_source: "standalone",
                passthrough_sources: vec![("ha_weather".to_string(), 0)],
                service_call_controllers: vec!["ha_valves".to_string()],
                mqtt_discovery: false,
                hacs_last_seen_epoch: 0,
                hacs_streaming: false,
            }),
            conditions: vec![ConditionsProvenance {
                field: "Air temperature",
                source: "Open-Meteo".into(),
            }],
        };
        assert_json_snapshot!("health_v1", health);
    }

    /// The static core of every `/api/v1/config/source_catalog` entry:
    /// each cloud kind's CloudSourceMeta (kind, data_nature, rain_nature,
    /// the verbatim audit copy, key_tier, emits_current_rain,
    /// pop_is_synthetic, honesty_rank, irrigation_rank, upgrade_reason),
    /// which the handler serde-flattens to the top level of each
    /// CloudCatalogEntry. Catalog order (highest honesty first) is part
    /// of the lock, so a kind rename or reorder trips this gate.
    ///
    /// RESIDUAL GAP (documented): CloudCatalogEntry itself is private to
    /// api::config and adds nine live per-deployment fields
    /// (live_current_fields, field_natures, recommended_here,
    /// region_priority, region_appropriate, upgrade_available,
    /// already_configured, configured_present, status) that cannot be
    /// constructed here; those stay outside the shape gate.
    #[test]
    fn source_catalog_meta_v1_shape() {
        let metas: Vec<_> = crate::sources::cloud_catalog::cloud_kinds()
            .iter()
            .map(|k| {
                crate::sources::cloud_catalog::cloud_meta(k)
                    .expect("every catalog kind has cloud meta")
            })
            .collect();
        assert_json_snapshot!("source_catalog_meta_v1", metas);
    }

    /// `/api/v1/irrigation/tuning` (1.19.0). A fixed fixture instance so
    /// every field renders deterministically: one zone carrying a
    /// recommendation (with a companion field, the measured-rate pair),
    /// one zone in the ok state with informational lines, and a populated
    /// scorecard. Null stays the documented unknown value on the
    /// Option-typed counts, so the honest-unknowns register is part of
    /// the locked shape.
    #[test]
    fn irrigation_tuning_v1_shape() {
        use crate::history::types::{
            TuningCompanionField, TuningRecommendation, TuningReport, TuningScorecard, ZoneTuning,
        };
        let report = TuningReport {
            generated_epoch: 0,
            window_days: 14,
            zones: vec![
                ZoneTuning {
                    slug: "back_yard".into(),
                    display_name: "Back Yard".into(),
                    status: "recommendation".into(),
                    lines: vec!["Watered 5 time(s) in the last 14 days.".into()],
                    recommendation: Some(TuningRecommendation {
                        id: "0000000000000000".into(),
                        field: "precip_rate_mm_hr".into(),
                        current_value: serde_json::Value::Null,
                        suggested_value: json!(18.0),
                        companion_fields: vec![TuningCompanionField {
                            field: "precip_rate_source".into(),
                            value: json!("measured"),
                        }],
                        headline: "Set this zone's sprinkler rate to the measured 18.0 mm/hr; \
                                   runs are planned as if it were 38.0 mm/hr."
                            .into(),
                        evidence: vec!["Median rate backed out of 3 clean watering events: 18.0 \
                                        mm/hr vs the configured 38.0 mm/hr (53% apart)."
                            .into()],
                        confidence: "medium".into(),
                    }),
                    ..Default::default()
                },
                ZoneTuning {
                    slug: "front_yard".into(),
                    display_name: "Front Yard".into(),
                    status: "ok".into(),
                    lines: vec![
                        "Watered 4 time(s) in the last 14 days.".into(),
                        "A soil probe would unlock the drying-rate and sprinkler-rate checks \
                         for this zone."
                            .into(),
                    ],
                    recommendation: None,
                    ..Default::default()
                },
            ],
            scorecard: TuningScorecard {
                window_days: 30,
                scored_days: Some(4),
                confirmed_days: Some(3),
                min_scored_days: 3,
                line: "Skipped 4 days for forecast rain in the last 30; rain came 3 of 4.".into(),
                reactive_days: Some(2),
                reactive_line: "Skipped 2 day(s) for rain already falling or on the ground in \
                                the last 30."
                    .into(),
            },
        };
        assert_json_snapshot!("irrigation_tuning_v1", report);
    }
}
