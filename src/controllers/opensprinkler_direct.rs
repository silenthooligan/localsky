// OpenSprinkler HTTP-API controller. Talks directly to an OS controller
// on the LAN (firmware 2.1.9+). No HA dependency.
//
// Protocol reference: https://openthings.freshdesk.com/support/solutions/articles/5000716363
//
//   GET /jc?pw=<md5>             controller status (zone states, water_level, rain_sensor)
//   GET /jn?pw=<md5>             station names + attributes
//   GET /cm?pw=<md5>&sid=N&en=1&t=S  start station N for S seconds
//   GET /cm?pw=<md5>&sid=N&en=0      stop station N
//   GET /cv?pw=<md5>&rsn=1            stop all
//   GET /jl?pw=<md5>&hist=N           log history (last N entries)
//
// Password is md5(plaintext) lowercased and passed as `pw=` query.
// Zone -> station mapping comes from config.zones[*].controller_station
// (we accept any string for portability; OS uses 1-based integers).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::config::schema::OpenSprinklerDirectConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

pub struct OpenSprinklerDirect {
    id: String,
    config: OpenSprinklerDirectConfig,
    client: Client,
    /// Map of zone_slug -> station number (1-based). Populated from
    /// config.zones[*].controller_station before adapter construction.
    zone_to_station: Arc<std::collections::HashMap<String, u32>>,
}

impl OpenSprinklerDirect {
    pub fn new(
        id: impl Into<String>,
        config: OpenSprinklerDirectConfig,
        zone_to_station: std::collections::HashMap<String, u32>,
    ) -> Result<Self, ControllerError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ControllerError::Init(format!("reqwest client: {e}")))?;
        Ok(Self {
            id: id.into(),
            config,
            client,
            zone_to_station: Arc::new(zone_to_station),
        })
    }

    fn base_url(&self) -> String {
        format!("http://{}:{}", self.config.host, self.config.port)
    }

    fn station_for(&self, slug: &str) -> Result<u32, ControllerError> {
        self.zone_to_station
            .get(slug)
            .copied()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        extra_query: &[(&str, String)],
    ) -> Result<T, ControllerError> {
        // Build the URL manually so we don't depend on reqwest's query()
        // helper being available across feature flag combinations.
        let mut url = format!(
            "{}{}?pw={}",
            self.base_url(),
            path,
            self.config.password_md5
        );
        for (k, v) in extra_query {
            url.push('&');
            url.push_str(k);
            url.push('=');
            url.push_str(v);
        }
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ControllerError::Transport(e.to_string()))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ControllerError::Transport(e.to_string()))?;
        if !status.is_success() {
            return Err(ControllerError::Remote(format!("HTTP {status}: {body}")));
        }
        let value: serde_json::Value = serde_json::from_str(&body)
            .map_err(|e| ControllerError::Remote(format!("json parse: {e}; body={body}")))?;
        // OS replies HTTP 200 even on errors and signals them via a
        // {"result":N} envelope (firmware always uses 200 + result
        // codes). Without this check a wrong password "passes" the /jc
        // probe (the wizard's controller test) because {"result":2}
        // deserializes into an all-default struct, and stop commands
        // are never actually verified.
        check_result_envelope(&value)?;
        serde_json::from_value(value)
            .map_err(|e| ControllerError::Remote(format!("json parse: {e}; body={body}")))
    }
}

/// Inspect an OpenSprinkler response for the {"result":N} error
/// envelope. Status endpoints (/jc, /jn, /jl) return their data object
/// directly on success and never include "result"; command endpoints
/// (/cm, /cv) return {"result":1} on success. Anything else is an
/// error: 2 = unauthorized (wrong password), the rest per the firmware
/// API reference.
fn check_result_envelope(v: &serde_json::Value) -> Result<(), ControllerError> {
    let Some(code) = v.get("result").and_then(|r| r.as_i64()) else {
        return Ok(());
    };
    match code {
        1 => Ok(()),
        2 => Err(ControllerError::AuthFailed),
        other => {
            let label = match other {
                3 => "mismatch",
                16 => "data missing",
                17 => "out of range",
                18 => "data format error",
                32 => "page not found",
                48 => "not permitted",
                _ => "unknown error",
            };
            Err(ControllerError::Remote(format!(
                "OpenSprinkler error result={other} ({label})"
            )))
        }
    }
}

#[derive(Debug, Deserialize)]
struct JcResponse {
    /// Master enable bit (0/1).
    #[serde(default)]
    en: u8,
    /// Water level percent (0..=250).
    #[serde(default)]
    wl: u32,
    /// Rain sensor tripped (0/1). Field name varies; OS may use "rs".
    #[serde(default)]
    rs: u8,
    /// Per-station status: array of [run_state, ...] entries.
    #[serde(default)]
    ps: Vec<Vec<i64>>,
    /// Firmware version string.
    #[serde(default)]
    fwv: Option<u32>,
    /// Live flow click rate (clicks/min) from a flow sensor wired to the
    /// FLOW input. Convert to GPM via the configured K-factor (fpr0/fpr1
    /// from /jo). Present only when flow sensor is enabled (sn1t=2).
    #[serde(default)]
    flcrt: Option<u64>,
    /// Flow pulse rate, gallons-per-click (fpr0 in units of 0.01).
    /// OS firmware 2.1.9+ surfaces this on /jc when flow sensing is on.
    #[serde(default)]
    fpr0: Option<u64>,
    /// Same pulse rate, integer-divided part (fpr1).
    #[serde(default)]
    fpr1: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CmResponse {
    /// OS returns "result":1 on success.
    #[serde(default)]
    result: i32,
}

#[async_trait]
impl IrrigationController for OpenSprinklerDirect {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            flow_meter: true,
            rain_sensor: true,
            master_valve: true,
            multi_zone_parallel: false,
            history_query: true,
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let sid = self.station_for(slug)?;
        // OS stations are 0-indexed in /cm despite UI numbering from 1.
        let zero_indexed = sid.saturating_sub(1);
        let r: CmResponse = self
            .get_json(
                "/cm",
                &[
                    ("sid", zero_indexed.to_string()),
                    ("en", "1".to_string()),
                    ("t", duration_s.to_string()),
                ],
            )
            .await?;
        if r.result != 1 {
            return Err(ControllerError::Remote(format!(
                "OS rejected manual start: result={}",
                r.result
            )));
        }
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: now_epoch(),
            planned_duration_s: duration_s,
            provider_ref: Some(sid.to_string()),
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        let sid = self.station_for(slug)?;
        let zero_indexed = sid.saturating_sub(1);
        let r: CmResponse = self
            .get_json(
                "/cm",
                &[("sid", zero_indexed.to_string()), ("en", "0".to_string())],
            )
            .await?;
        // get_json already rejects non-1 result envelopes; this guards
        // the (unexpected) case of a 200 body with no result at all so
        // a stop is never silently assumed to have worked.
        if r.result != 1 {
            return Err(ControllerError::Remote(format!(
                "OS rejected station stop: result={}",
                r.result
            )));
        }
        Ok(())
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        let r: CmResponse = self.get_json("/cv", &[("rsn", "1".to_string())]).await?;
        if r.result != 1 {
            return Err(ControllerError::Remote(format!(
                "OS rejected stop-all: result={}",
                r.result
            )));
        }
        Ok(())
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let r: JcResponse = self.get_json("/jc", &[]).await?;
        // Map per-station program state into ZoneRuntimeStatus. ps[i] =
        // [pid, rem, sst] where rem > 0 means actively running.
        let mut zone_states = Vec::new();
        for (i, ps) in r.ps.iter().enumerate() {
            let station = (i + 1) as u32;
            let slug = self
                .zone_to_station
                .iter()
                .find_map(|(k, v)| (*v == station).then(|| k.clone()))
                .unwrap_or_else(|| format!("station_{station}"));
            let remaining = ps.get(1).copied().unwrap_or(0);
            zone_states.push(ZoneRuntimeStatus {
                slug,
                running: remaining > 0,
                remaining_s: if remaining > 0 {
                    Some(remaining as u32)
                } else {
                    None
                },
                last_run_epoch: None,
            });
        }
        // Flow: OS reports click rate in clicks/minute; convert to GPM
        // via the K-factor stored as gallons-per-click. The K-factor on
        // /jc is encoded as (fpr1 * 100 + fpr0) hundredths of a gallon.
        // E.g. fpr1=0, fpr0=50 -> 0.50 gal/click. With flcrt clicks/min,
        // gpm = flcrt * k_gal_per_click. flcrt=0 (no flow) -> Some(0.0)
        // rather than None so the engine can distinguish "meter present,
        // zero flow" from "no meter".
        let flow_gpm = match (r.flcrt, r.fpr0, r.fpr1) {
            (Some(rate), Some(p0), Some(p1)) => {
                let k = (p1 as f64 * 100.0 + p0 as f64) / 100.0;
                Some(rate as f64 * k)
            }
            _ => None,
        };
        Ok(ControllerStatus {
            reachable: true,
            master_enabled: Some(r.en == 1),
            water_level_pct: Some(r.wl as f64),
            rain_sensor_tripped: Some(r.rs == 1),
            current_program: None,
            zone_states,
            flow_gpm,
            firmware: r.fwv.map(|v| v.to_string()),
        })
    }

    async fn run_history(&self, since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        // OS /jl returns a JSON object with "log":[[pid,sid,dur,start], ...]
        // Each entry is one zone run. We filter by start >= since_epoch.
        #[derive(Deserialize)]
        struct Jl {
            #[serde(default)]
            log: Vec<Vec<i64>>,
        }
        let r: Jl = self.get_json("/jl", &[("hist", "0".to_string())]).await?;
        let mut out = Vec::new();
        for entry in r.log {
            if entry.len() < 4 {
                continue;
            }
            let (_pid, sid, dur, start) = (entry[0], entry[1] as u32, entry[2] as u32, entry[3]);
            if start < since_epoch {
                continue;
            }
            let slug = self
                .zone_to_station
                .iter()
                .find_map(|(k, v)| (*v == sid).then(|| k.clone()))
                .unwrap_or_else(|| format!("station_{sid}"));
            out.push(RunRecord {
                zone_slug: slug,
                start_epoch: start,
                end_epoch: Some(start + dur as i64),
                duration_s: Some(dur),
                source: "controller_external".to_string(),
            });
        }
        Ok(out)
    }

    async fn discover_zones(
        &self,
    ) -> ControllerResult<Vec<crate::ports::irrigation_controller::DiscoveredZone>> {
        use crate::ports::irrigation_controller::DiscoveredZone;
        // snames is REQUIRED: a wrong password is rejected upstream by
        // the result-envelope check (AuthFailed), and any other body
        // missing snames is a protocol error. Either way the scan
        // surfaces the failure instead of reporting "0 zones found".
        #[derive(Deserialize)]
        struct JnResponse {
            snames: Vec<String>,
        }
        let r: JnResponse = self.get_json("/jn", &[]).await?;
        let out = r
            .snames
            .into_iter()
            .enumerate()
            // Skip OpenSprinkler's default "Disabled" name for unused
            // stations so onboarding only offers real zones.
            .filter(|(_, name)| !name.trim().eq_ignore_ascii_case("disabled"))
            .map(|(i, name)| {
                let station = i + 1; // OpenSprinkler stations are 1-based.
                DiscoveredZone {
                    station_id: station.to_string(),
                    name: if name.trim().is_empty() {
                        format!("Station {station}")
                    } else {
                        name
                    },
                }
            })
            .collect();
        Ok(out)
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn station_resolution_succeeds_for_mapped_zones() {
        let mut map = std::collections::HashMap::new();
        map.insert("back_yard".to_string(), 1);
        map.insert("front_yard".to_string(), 2);
        let c = OpenSprinklerDirect::new(
            "os1",
            OpenSprinklerDirectConfig {
                host: "127.0.0.1".into(),
                port: 80,
                password_md5: "abc".into(),
                poll_interval_s: 10,
            },
            map,
        )
        .unwrap();
        assert_eq!(c.station_for("back_yard").unwrap(), 1);
        assert_eq!(c.station_for("front_yard").unwrap(), 2);
    }

    #[test]
    fn station_resolution_fails_for_unknown_zones() {
        let c = OpenSprinklerDirect::new(
            "os1",
            OpenSprinklerDirectConfig {
                host: "127.0.0.1".into(),
                port: 80,
                password_md5: "abc".into(),
                poll_interval_s: 10,
            },
            std::collections::HashMap::new(),
        )
        .unwrap();
        let err = c.station_for("nope").unwrap_err();
        assert!(matches!(err, ControllerError::ZoneUnknown(_)));
    }

    #[test]
    fn result_envelope_accepts_success_and_data_bodies() {
        // Command success: {"result":1}.
        assert!(check_result_envelope(&serde_json::json!({"result": 1})).is_ok());
        // Status bodies (/jc, /jn) have no result key on success.
        assert!(check_result_envelope(&serde_json::json!({"en": 1, "wl": 100})).is_ok());
        assert!(check_result_envelope(&serde_json::json!({"snames": ["Front", "Back"]})).is_ok());
    }

    #[test]
    fn result_envelope_maps_unauthorized_to_auth_failed() {
        // Wrong password: OS replies HTTP 200 with {"result":2}. This
        // must NOT pass the wizard's controller test.
        let err = check_result_envelope(&serde_json::json!({"result": 2})).unwrap_err();
        assert!(matches!(err, ControllerError::AuthFailed));
    }

    #[test]
    fn result_envelope_surfaces_other_error_codes() {
        for code in [3, 16, 17, 18, 32, 48, 99] {
            let err = check_result_envelope(&serde_json::json!({"result": code})).unwrap_err();
            match err {
                ControllerError::Remote(msg) => {
                    assert!(msg.contains(&format!("result={code}")), "msg: {msg}")
                }
                other => panic!("expected Remote, got {other:?}"),
            }
        }
    }

    #[test]
    fn base_url_format() {
        let c = OpenSprinklerDirect::new(
            "os1",
            OpenSprinklerDirectConfig {
                host: "192.0.2.5".into(),
                port: 8080,
                password_md5: "abc".into(),
                poll_interval_s: 10,
            },
            std::collections::HashMap::new(),
        )
        .unwrap();
        assert_eq!(c.base_url(), "http://192.0.2.5:8080");
    }
}
