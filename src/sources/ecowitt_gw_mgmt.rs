//! Ecowitt gateway MANAGEMENT writes (Tier 1 device removal).
//!
//! `ecowitt_gw_poll` only READS the gateway (get_livedata_info, get_cli_soilad,
//! both unauthenticated on the LAN). This is the write counterpart: it
//! unregisters a sensor on the gateway so an operator can remove a device from
//! LocalSky and have it cleared from the gateway in ONE action, instead of
//! hand-deleting it in the Ecowitt UI (the gateway keeps a dead sensor's
//! registration + last RSSI/battery basically forever; pulling the battery does
//! NOT clear it).
//!
//! Protocol (reverse-engineered from the gateway's own sensorsID.html + axjs.js,
//! 2026-07-04):
//!   POST http://<host>/set_sensors_info      (Basic auth, application/json)
//!   {"type": <channel type code>, "id": "0xFFFFFFFE"}
//! `type` is the per-channel code from get_sensors_info ("Soil moisture CH<n>").
//! id `0xFFFFFFFE` = DISABLE the slot: the gateway then refuses to auto-register
//! a sensor to it, even a live one, so this is a true remove-and-stays-removed
//! (id `0xFFFFFFFF` would re-enable auto-learn, `0x<hex>` binds a specific id).
//!
//! All calls go through `net::safe_fetch` (IP-pin, no redirects, forbidden-target
//! reject, capped body): `host` is operator-configured, so it is treated like
//! every other operator-supplied fetch target in the codebase.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::time::Duration;

const MGMT_TIMEOUT: Duration = Duration::from_secs(8);
/// The "disable this slot" sensor id: the gateway won't auto-register a sensor
/// to a disabled slot, even a live one.
const DISABLE_ID: &str = "0xFFFFFFFE";
/// Pages of get_sensors_info to walk (the gateway paginates ~16 sensors/page;
/// soil lives on a later page than the weather sensors).
const MAX_PAGES: u32 = 6;

#[derive(Deserialize)]
struct RawSensor {
    name: Option<String>,
    /// Echoed verbatim into the set body, so string ("14") or number both work.
    #[serde(rename = "type")]
    type_code: Option<serde_json::Value>,
    /// "1"/1 = registered, "0"/0 = empty slot.
    idst: Option<serde_json::Value>,
}

/// Outcome of an unregister request, so the caller and UI stay honest about
/// what actually happened at the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnregisterOutcome {
    /// The channel was registered and is now disabled on the gateway.
    Unregistered,
    /// The channel was not registered (already gone / never present): no write.
    NotRegistered,
}

/// Registered when idst is anything other than "0"/0.
fn is_registered(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::String(s)) => s.trim() != "0",
        Some(serde_json::Value::Number(n)) => n.as_i64() != Some(0),
        _ => false,
    }
}

/// Find the `type` code + registration state for "Soil moisture CH<channel>" by
/// walking the PAGINATED get_sensors_info. Each page is a SEPARATE JSON array;
/// concatenating pages before parsing yields invalid JSON (a real footgun that
/// silently returns nothing), so we fetch + parse one page at a time.
async fn resolve_soil_channel(
    host: &str,
    user: &str,
    pass: &str,
    channel: u32,
) -> Result<Option<(serde_json::Value, bool)>> {
    let want = format!("Soil moisture CH{channel}");
    for page in 1..=MAX_PAGES {
        let url = format!("http://{host}/get_sensors_info?page={page}");
        let (client, safe_url) = crate::net::safe_fetch::build_safe_client(&url, MGMT_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("gateway unreachable ({host}): {e}"))?;
        let resp = client
            .get(safe_url)
            .basic_auth(user, Some(pass))
            .send()
            .await
            .with_context(|| format!("GET get_sensors_info p{page} ({host})"))?
            .error_for_status()
            .with_context(|| format!("get_sensors_info p{page} non-2xx ({host})"))?;
        let bytes = crate::net::safe_fetch::read_body_capped(resp)
            .await
            .map_err(|e| anyhow::anyhow!("read get_sensors_info p{page}: {e}"))?;
        let sensors: Vec<RawSensor> = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            // A page we cannot parse: skip it, keep walking (never abort on one).
            Err(_) => continue,
        };
        if sensors.is_empty() {
            break; // walked past the last populated page
        }
        if let Some(s) = sensors
            .iter()
            .find(|s| s.name.as_deref() == Some(want.as_str()))
        {
            let ty = s.type_code.clone().unwrap_or(serde_json::Value::Null);
            return Ok(Some((ty, is_registered(s.idst.as_ref()))));
        }
    }
    Ok(None)
}

/// Unregister (disable) an Ecowitt soil-moisture channel on the gateway so it
/// stops showing as a registered sensor. Idempotent: a channel that isn't
/// registered returns `NotRegistered` without writing.
pub async fn unregister_soil_channel(
    host: &str,
    username: &str,
    password: &str,
    channel: u32,
) -> Result<UnregisterOutcome> {
    let Some((type_code, registered)) =
        resolve_soil_channel(host, username, password, channel).await?
    else {
        return Ok(UnregisterOutcome::NotRegistered);
    };
    if !registered {
        return Ok(UnregisterOutcome::NotRegistered);
    }
    let url = format!("http://{host}/set_sensors_info");
    let (client, safe_url) = crate::net::safe_fetch::build_safe_client(&url, MGMT_TIMEOUT)
        .await
        .map_err(|e| anyhow::anyhow!("gateway unreachable ({host}): {e}"))?;
    let body = serde_json::json!({ "type": type_code, "id": DISABLE_ID });
    client
        .post(safe_url)
        .basic_auth(username, Some(password))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST set_sensors_info ({host})"))?
        .error_for_status()
        .with_context(|| format!("set_sensors_info non-2xx ({host})"))?;
    Ok(UnregisterOutcome::Unregistered)
}

/// Parse the channel number out of a soil binding spec
/// `source:<id>:soilmoisture<N>` -> N. None for any other shape (an `ha:`
/// entity, a non-soil channel, a malformed spec).
pub fn soil_channel_of(spec: &str) -> Option<u32> {
    let (_, key) = spec.strip_prefix("source:")?.split_once(':')?;
    key.strip_prefix("soilmoisture")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registered_reads_both_string_and_number_idst() {
        assert!(is_registered(Some(&serde_json::json!("1"))));
        assert!(is_registered(Some(&serde_json::json!(1))));
        assert!(!is_registered(Some(&serde_json::json!("0"))));
        assert!(!is_registered(Some(&serde_json::json!(0))));
        assert!(!is_registered(None));
    }

    #[test]
    fn soil_channel_parsed_only_from_ecowitt_soil_spec() {
        assert_eq!(soil_channel_of("source:ecowitt_gw:soilmoisture2"), Some(2));
        assert_eq!(soil_channel_of("source:gw:soilmoisture10"), Some(10));
        assert_eq!(soil_channel_of("source:gw:temperature"), None);
        assert_eq!(soil_channel_of("ha:sensor.back_yard_moisture"), None);
        assert_eq!(soil_channel_of("soilmoisture2"), None);
        assert_eq!(soil_channel_of(""), None);
    }
}
