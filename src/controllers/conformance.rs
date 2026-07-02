//! P1-3 controller conformance harness.
//!
//! Offline-only by necessity: `safe_fetch::build_safe_client` forbids loopback,
//! so the HTTP adapters (OpenSprinkler direct / Rachio / RainBird / B-hyve /
//! Hydrawise / http_generic / ha_service_call) cannot be pointed at a local mock
//! server. Their wire methods stay covered by the per-adapter response-parsing
//! unit tests, not here. This harness pins the `IrrigationController` trait
//! CONTRACT end-to-end for adapters that operate with no network -- today that is
//! `DryRunController`.
//!
//! To make the HTTP adapters conformance-testable, the codebase needs a transport
//! seam: an injectable `reqwest::Client` / request trait, or a test-only loopback
//! exemption on `safe_fetch::build_safe_client`. Recorded here so the gap is
//! visible rather than silently uncovered.

use crate::ports::irrigation_controller::{ControllerError, IrrigationController};

/// Drive the offline trait contract against an operable controller.
/// `live_zone` is a slug the controller accepts. When `expects_zone_unknown` is
/// true, `bad_zone` must be one it rejects with `ZoneUnknown` (adapters that hold
/// a zone map); adapters that accept any slug pass `false`.
async fn assert_conformant(
    c: &dyn IrrigationController,
    live_zone: &str,
    expects_zone_unknown: bool,
    bad_zone: &str,
) {
    // id(): non-empty and stable across calls.
    let id = c.id().to_string();
    assert!(!id.is_empty(), "id() empty");
    assert_eq!(c.id(), id, "id() not stable across calls");

    // caps consistency: no history_query => run_history must be empty.
    let caps = c.supports();
    if !caps.history_query {
        assert!(
            c.run_history(0).await.unwrap().is_empty(),
            "history_query=false must yield an empty run_history"
        );
    }

    // run_zone(): the RunHandle echoes the inputs and stamps a start.
    let h = c.run_zone(live_zone, 120).await.expect("run_zone Ok");
    assert_eq!(h.controller_id, id, "RunHandle.controller_id mismatch");
    assert_eq!(h.zone_slug, live_zone, "RunHandle.zone_slug mismatch");
    assert_eq!(
        h.planned_duration_s, 120,
        "RunHandle.planned_duration_s mismatch"
    );
    assert!(h.started_epoch > 0, "RunHandle.started_epoch must be set");

    // status(): flow_connected must never be true when the flow_meter cap is off.
    let st = c.status().await.expect("status Ok");
    if !caps.flow_meter {
        assert!(
            !st.flow_connected,
            "flow_connected true but the flow_meter cap is false"
        );
    }

    // stop contract: stop_zone / stop_all are Ok and stop_all is idempotent.
    c.stop_zone(live_zone).await.expect("stop_zone Ok");
    c.stop_all().await.expect("stop_all Ok");
    c.stop_all().await.expect("stop_all must be idempotent");

    // ZoneUnknown for an unmapped slug (adapters that hold a zone map only).
    if expects_zone_unknown {
        match c.run_zone(bad_zone, 60).await {
            Err(ControllerError::ZoneUnknown(_)) => {}
            other => panic!("expected ZoneUnknown for {bad_zone}, got {other:?}"),
        }
    }

    // discover_zones(): either Ok with valid stations, or Unsupported.
    match c.discover_zones().await {
        Ok(zs) => {
            for z in &zs {
                assert!(!z.station_id.is_empty(), "discovered station_id empty");
                assert!(!z.name.is_empty(), "discovered name empty");
            }
        }
        Err(ControllerError::Unsupported(_)) => {}
        Err(e) => panic!("discover_zones must be Ok or Unsupported, got {e:?}"),
    }
}

#[tokio::test]
async fn dry_run_is_conformant() {
    use crate::config::schema::DryRunConfig;
    use crate::controllers::dry_run::DryRunController;
    // DryRunController accepts any slug (no zone map) -> no ZoneUnknown path, so
    // expects_zone_unknown = false. discover_zones returns sample stations (Ok).
    let c = DryRunController::new(
        "dry_run_conf",
        DryRunConfig {
            simulate_runs: false,
        },
        None,
    );
    assert_conformant(&c, "front_yard", false, "ghost_zone_xyz").await;
}
