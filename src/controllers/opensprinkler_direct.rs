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
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            config,
            client,
            zone_to_station: Arc::new(zone_to_station),
        }
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
        let mut url = format!("{}{}?pw={}", self.base_url(), path, self.config.password_md5);
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
        serde_json::from_str(&body)
            .map_err(|e| ControllerError::Remote(format!("json parse: {e}; body={body}")))
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
        let _: CmResponse = self
            .get_json(
                "/cm",
                &[
                    ("sid", zero_indexed.to_string()),
                    ("en", "0".to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        let _: CmResponse = self
            .get_json("/cv", &[("rsn", "1".to_string())])
            .await?;
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
        Ok(ControllerStatus {
            reachable: true,
            master_enabled: Some(r.en == 1),
            water_level_pct: Some(r.wl as f64),
            rain_sensor_tripped: Some(r.rs == 1),
            current_program: None,
            zone_states,
            flow_gpm: None,
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
        );
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
        );
        let err = c.station_for("nope").unwrap_err();
        assert!(matches!(err, ControllerError::ZoneUnknown(_)));
    }

    #[test]
    fn base_url_format() {
        let c = OpenSprinklerDirect::new(
            "os1",
            OpenSprinklerDirectConfig {
                host: "opensprinkler.local".into(),
                port: 8080,
                password_md5: "abc".into(),
                poll_interval_s: 10,
            },
            std::collections::HashMap::new(),
        );
        assert_eq!(c.base_url(), "http://opensprinkler.local:8080");
    }
}
