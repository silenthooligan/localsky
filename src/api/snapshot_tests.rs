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
    use serde::Serialize;
    use serde_json::json;

    /// `/api/v1/info` shape. Locked separately from the test in info.rs
    /// (which validates SemVer format) because that one doesn't catch
    /// added or renamed fields.
    #[derive(Serialize)]
    struct InfoFixture {
        service: &'static str,
        service_version: &'static str,
        api_version: &'static str,
    }

    #[test]
    fn info_v1_shape() {
        let fixture = InfoFixture {
            service: "localsky",
            service_version: "0.2.0-beta.1",
            api_version: super::super::info::API_VERSION,
        };
        assert_json_snapshot!("info_v1", fixture);
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
}
