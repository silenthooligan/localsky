// HaPassthrough source — pulls any Home-Assistant sensor entity into
// LocalSky as a WeatherField. This is the meta-adapter that makes the
// "LocalSky owns everything" architecture self-extending: if HA already
// has a working integration for some weather hardware we don't natively
// support (Davis VP2 via weewx, Netatmo, Pirate Weather via the legacy
// HA integration, an Aqara LYWSD03MMC zigbee humidity sensor, anything),
// the user can map its entity_id onto a WeatherField and it joins the
// merge engine like a first-class source.
//
// Config:
//   base_url      = "http://192.0.2.79:8123"
//   bearer_token  = "eyJ..."   (HA Long-Lived Access Token)
//   field_map     = { "AirTempF": "sensor.tempest_outdoor_temp", ... }
//
// Endpoint:
//   GET {base_url}/api/states
//     → array of {entity_id, state, attributes, ...}; we look up each
//     mapped entity_id, parse its `state` as f64, emit one tuple per
//     successful parse.
//
// Polling cadence: 30s. HA's REST API is light, this runs on-LAN, and
// users expect HA state changes to surface in LocalSky promptly.
//
// Field-name keys: the user writes the WeatherField *variant name*
// (case-insensitive) as the map key. parse_weather_field() handles the
// conversion. Unknown keys are warned once at startup and ignored.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::schema::HaPassthroughConfig;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};

const POLL_INTERVAL: Duration = Duration::from_secs(30);

pub struct HaPassthrough {
    id: String,
    config: HaPassthroughConfig,
    client: Client,
    /// Pre-parsed field_map: WeatherField -> entity_id. Unknown keys
    /// are dropped at construction with a warn.
    mapping: Vec<(WeatherField, String)>,
}

#[derive(Debug, Deserialize)]
struct StateEntry {
    entity_id: String,
    state: String,
}

impl HaPassthrough {
    pub fn new(id: impl Into<String>, config: HaPassthroughConfig) -> Self {
        let id = id.into();
        let mapping = build_mapping(&id, &config.field_map);
        let client = Client::builder()
            .timeout(Duration::from_secs(8))
            .user_agent("localsky/ha-passthrough")
            .build()
            .expect("reqwest client construction");
        Self {
            id,
            config,
            client,
            mapping,
        }
    }

    async fn fetch_states(&self) -> anyhow::Result<Vec<StateEntry>> {
        let url = format!("{}/api/states", self.config.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.config.bearer_token)
            .send()
            .await?
            .error_for_status()?;
        // HA returns the full attribute blob per entity but we only need
        // entity_id + state. Deserialize::deny_unknown_fields is off so
        // the extra fields just get ignored.
        let arr: Vec<Value> = resp.json().await?;
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            if let (Some(entity_id), Some(state)) = (
                v.get("entity_id").and_then(|x| x.as_str()),
                v.get("state").and_then(|x| x.as_str()),
            ) {
                out.push(StateEntry {
                    entity_id: entity_id.to_string(),
                    state: state.to_string(),
                });
            }
        }
        Ok(out)
    }
}

fn build_mapping(
    source_id: &str,
    field_map: &BTreeMap<String, String>,
) -> Vec<(WeatherField, String)> {
    let mut out = Vec::new();
    for (k, v) in field_map {
        match parse_weather_field(k) {
            Some(f) => out.push((f, v.clone())),
            None => {
                warn!(source_id, key = %k, "ha_passthrough field_map key does not match a known WeatherField; ignoring");
            }
        }
    }
    out
}

fn parse_weather_field(name: &str) -> Option<WeatherField> {
    // Case-insensitive match against variant names. snake_case is also
    // accepted because the wizard surfaces both forms.
    let n = name.replace('_', "").to_ascii_lowercase();
    Some(match n.as_str() {
        "airtempf" | "tempf" | "temperaturef" => WeatherField::AirTempF,
        "dewpointf" => WeatherField::DewPointF,
        "rhpct" | "humidity" | "humidityrh" => WeatherField::RhPct,
        "windmph" | "windspeedmph" => WeatherField::WindMph,
        "windgustmph" => WeatherField::WindGustMph,
        "windbearingdeg" | "winddir" | "winddirdeg" => WeatherField::WindBearingDeg,
        "solarwm2" | "solarradiation" => WeatherField::SolarWm2,
        "uvindex" | "uv" => WeatherField::UvIndex,
        "illuminance" | "illuminancelx" => WeatherField::Illuminance,
        "pressureinhg" | "barometricinhg" => WeatherField::PressureInHg,
        "raintodayin" | "dailyrainin" => WeatherField::RainTodayIn,
        "rainintensityinhr" | "hourlyrainin" => WeatherField::RainIntensityInHr,
        "lightningcount" => WeatherField::LightningCount,
        "lightningdistancemi" => WeatherField::LightningDistanceMi,
        "et0today" => WeatherField::Et0Today,
        "flowgpm" | "flowrate" | "flowratepm" => WeatherField::FlowGpm,
        "flowtotalgaltoday" | "flowtotalgallons" | "flowtoday" => WeatherField::FlowTotalGalToday,
        _ => return None,
    })
}

#[async_trait]
impl WeatherSource for HaPassthrough {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        let mut fields = HashSet::new();
        for (f, _) in &self.mapping {
            fields.insert(*f);
        }
        SourceCaps {
            // Live values forwarded from whatever the HA entity reports.
            live_current: !self.mapping.is_empty(),
            hourly_forecast_hours: 0,
            daily_forecast_days: 0,
            radar_tiles: false,
            et0_native: false,
            fields,
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        // HA passthrough is by definition a routed-through copy of some
        // OTHER source's data. Priority 30: above raw forecast (25), well
        // below any direct adapter (60+). Users who want HA to win should
        // remove the conflicting native adapter from cfg.sources.
        if self.mapping.iter().any(|(f, _)| *f == field) {
            30
        } else {
            i32::MIN
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        info!(
            source_id = %self.id,
            mapping_n = self.mapping.len(),
            "HaPassthrough source started",
        );
        if self.mapping.is_empty() {
            warn!(source_id = %self.id, "HaPassthrough has empty field_map; idle");
        }
        let mut tick = interval(POLL_INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_reachable: Option<bool> = None;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    match self.fetch_states().await {
                        Ok(states) => {
                            if last_reachable != Some(true) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: true,
                                });
                                last_reachable = Some(true);
                            }
                            // O(states * mapping) on every tick — both
                            // are small (HA: a few hundred entities;
                            // mapping: handful of fields). Building a
                            // HashMap pays for itself once mapping > 1.
                            let mut by_id: std::collections::HashMap<&str, &str> =
                                std::collections::HashMap::with_capacity(states.len());
                            for s in &states {
                                by_id.insert(s.entity_id.as_str(), s.state.as_str());
                            }
                            let mut fields = Vec::new();
                            for (field, entity_id) in &self.mapping {
                                let Some(raw) = by_id.get(entity_id.as_str()) else {
                                    debug!(source_id = %self.id, entity_id, "ha_passthrough entity not present in /api/states");
                                    continue;
                                };
                                // HA encodes unavailable / unknown as
                                // string literals; only parse if numeric.
                                let Ok(v) = raw.parse::<f64>() else {
                                    debug!(source_id = %self.id, entity_id, value = %raw, "ha_passthrough entity state not numeric");
                                    continue;
                                };
                                fields.push((*field, v));
                            }
                            if !fields.is_empty() {
                                debug!(source_id = %self.id, fields_n = fields.len(), "HaPassthrough updated");
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: self.id.clone(),
                                    fields,
                                    at_epoch: chrono::Utc::now().timestamp(),
                                });
                            }
                        }
                        Err(e) => {
                            warn!(source_id = %self.id, error = %e, "HaPassthrough fetch failed");
                            if last_reachable != Some(false) {
                                let _ = bus.send(SourceEvent::Reachability {
                                    source_id: self.id.clone(),
                                    reachable: false,
                                });
                                last_reachable = Some(false);
                            }
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        info!(source_id = %self.id, "HaPassthrough shutdown");
                        return Ok(());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with_map(map: &[(&str, &str)]) -> HaPassthroughConfig {
        let mut fm = BTreeMap::new();
        for (k, v) in map {
            fm.insert((*k).to_string(), (*v).to_string());
        }
        HaPassthroughConfig {
            base_url: "http://example.invalid".into(),
            bearer_token: "t".into(),
            field_map: fm,
        }
    }

    #[test]
    fn parses_known_field_names() {
        assert_eq!(
            parse_weather_field("AirTempF"),
            Some(WeatherField::AirTempF)
        );
        assert_eq!(
            parse_weather_field("air_temp_f"),
            Some(WeatherField::AirTempF)
        );
        assert_eq!(parse_weather_field("humidity"), Some(WeatherField::RhPct));
        assert_eq!(
            parse_weather_field("windDir"),
            Some(WeatherField::WindBearingDeg)
        );
        assert_eq!(parse_weather_field("garbage"), None);
    }

    #[test]
    fn caps_reflect_mapping() {
        let s = HaPassthrough::new(
            "ha",
            cfg_with_map(&[("AirTempF", "sensor.tempf"), ("WindMph", "sensor.windmph")]),
        );
        let caps = s.capabilities();
        assert!(caps.live_current);
        assert!(caps.fields.contains(&WeatherField::AirTempF));
        assert!(caps.fields.contains(&WeatherField::WindMph));
        assert!(!caps.fields.contains(&WeatherField::UvIndex));
    }

    #[test]
    fn priority_only_for_mapped_fields() {
        let s = HaPassthrough::new("ha", cfg_with_map(&[("AirTempF", "sensor.x")]));
        assert_eq!(s.priority(WeatherField::AirTempF), 30);
        assert_eq!(s.priority(WeatherField::WindMph), i32::MIN);
    }
}
