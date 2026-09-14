//! Shared interpretation of controller meter evidence for every read surface.

use crate::ports::irrigation_controller::ControllerStatus;

pub fn current_flow(
    status: &ControllerStatus,
    now: i64,
    poll_interval_s: Option<u32>,
) -> (bool, Option<f64>) {
    // A live on-demand read has no stored epoch. Explicit epochs belong to
    // caches and must fit the controller's advertised polling horizon.
    let fresh = status.observed_epoch.is_none_or(|at| {
        at > 0
            && at <= now
            && now.saturating_sub(at)
                <= i64::from(poll_interval_s.unwrap_or(super::guard::CLOUD_STATUS_POLL_S))
    });
    let connected = status.reachable && fresh && status.flow_connected;
    let rate = if connected {
        status.flow_gpm.filter(|v| v.is_finite() && *v >= 0.0)
    } else {
        None
    };
    (connected, rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_cache_rejects_future_stale_disconnected_and_invalid_readings() {
        let mut status = ControllerStatus {
            observed_epoch: Some(1_000),
            reachable: true,
            master_enabled: None,
            water_level_pct: None,
            rain_sensor_tripped: None,
            current_program: None,
            zone_states: Vec::new(),
            flow_gpm: Some(0.0),
            flow_connected: true,
            firmware: None,
        };
        assert_eq!(current_flow(&status, 1_000, Some(60)), (true, Some(0.0)));
        assert_eq!(current_flow(&status, 999, Some(60)), (false, None));
        assert_eq!(current_flow(&status, 1_061, Some(60)), (false, None));
        status.reachable = false;
        assert_eq!(current_flow(&status, 1_010, Some(60)), (false, None));
        status.reachable = true;
        status.flow_connected = false;
        status.flow_gpm = Some(8.0);
        assert_eq!(current_flow(&status, 1_010, Some(60)), (false, None));
        status.flow_connected = true;
        for invalid in [f64::NAN, f64::INFINITY, -1.0] {
            status.flow_gpm = Some(invalid);
            assert_eq!(current_flow(&status, 1_010, Some(60)), (true, None));
        }
    }
}
