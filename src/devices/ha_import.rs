// Background HA device import (Phase F1b). Periodically pulls Home
// Assistant's device + entity registries over the WS API and merges the
// resulting origin=HomeAssistant devices into the DeviceRegistry, so HA's
// hardware shows up in LocalSky's Devices view next to the native devices.
//
// No-op when HA isn't configured (HA_URL/HA_TOKEN absent), the import is
// the LocalSky analogue of Music Assistant's "Home Assistant provider".

use std::time::Duration;

use crate::devices::DeviceRegistry;
use crate::ha::ws::{fetch_ha_devices, HaWsConfig};

/// Spawn the import loop. Refreshes every `interval_s` (floored at 30s).
/// Returns immediately; does nothing if HA env isn't configured.
pub fn spawn(registry: DeviceRegistry, interval_s: u64) {
    let cfg = match HaWsConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::info!(reason = %e, "HA device import disabled (no HA configured)");
            return;
        }
    };
    let period = Duration::from_secs(interval_s.max(30));
    tokio::spawn(async move {
        tracing::info!(interval_s = period.as_secs(), "HA device import started");
        let mut tick = tokio::time::interval(period);
        loop {
            tick.tick().await;
            match fetch_ha_devices(&cfg).await {
                Ok(devices) => {
                    let n = devices.len();
                    registry.set_ha(devices);
                    tracing::debug!(ha_devices = n, "HA device import refreshed");
                }
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "HA device import failed; keeping prior set");
                }
            }
        }
    });
}
