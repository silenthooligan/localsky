// HaPassthrough source, pulls any Home-Assistant sensor entity into
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
use futures::{stream::FuturesUnordered, StreamExt};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use tracing::{debug, info, warn};

use crate::config::schema::HaPassthroughConfig;
use crate::net::source_failure::{from_anyhow, from_reqwest, from_safe, response_format};
use crate::ports::source_error::{SourceErrorCode as Code, SourceFailure};
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::sources::poll::{run_polling, Poll};

const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Per-request budget for the HA REST poll. Matches the previous persistent
/// client's timeout; each fetch now builds an SSRF-hardened client.
const HA_TIMEOUT: Duration = Duration::from_secs(8);
const ENTITY_FETCH_CONCURRENCY: usize = 4;

pub struct HaPassthrough {
    id: String,
    config: HaPassthroughConfig,
    /// Pre-parsed field_map: WeatherField -> entity_id. Unknown keys
    /// are dropped at construction with a warn.
    mapping: Vec<(WeatherField, String)>,
}

#[derive(Debug)]
struct StateEntry {
    entity_id: String,
    state: String,
    /// HA `attributes.unit_of_measurement` (e.g. "°C", "km/h", "hPa", "mm").
    /// Used to normalize the reading to LocalSky's canonical imperial unit;
    /// None means "no declared unit, assume already canonical".
    unit: Option<String>,
    at_epoch: Option<i64>,
    rain_at_epoch: Option<i64>,
    restored: bool,
}

impl StateEntry {
    fn from_value(v: Value) -> Option<Self> {
        // last_changed is the time the VALUE changed, not its last report.
        // Modern HA reports unchanged readings through last_reported. Older
        // HA versions only expose last_updated; preserve that age rather than
        // inventing a fresh observation from a successful HTTP request.
        let at_epoch = v
            .get("last_reported")
            .or_else(|| v.get("last_updated"))
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp());
        Some(Self {
            entity_id: v.get("entity_id")?.as_str()?.into(),
            state: v.get("state")?.as_str()?.into(),
            unit: v
                .get("attributes")
                .and_then(|a| a.get("unit_of_measurement"))
                .and_then(Value::as_str)
                .map(str::to_string),
            at_epoch,
            // WeatherFlow identifies the physical observation period with
            // last_reset. HA republishing a state must not invent another minute.
            rain_at_epoch: match v.get("attributes").and_then(|a| a.get("last_reset")) {
                Some(value) => value
                    .as_str()
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|t| t.timestamp()),
                None => at_epoch,
            },
            restored: v
                .get("attributes")
                .and_then(|a| a.get("restored"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn reading(&self, now: i64) -> Option<(f64, i64)> {
        let epoch = self.at_epoch?;
        let value = self.state.parse::<f64>().ok()?;
        (!self.restored && epoch > 0 && epoch <= now && value.is_finite()).then_some((value, epoch))
    }
}

impl HaPassthrough {
    pub fn new(id: impl Into<String>, config: HaPassthroughConfig) -> Self {
        let id = id.into();
        let mapping = build_mapping(&id, &config.field_map);
        Self {
            id,
            config,
            mapping,
        }
    }

    async fn fetch_states(&self) -> anyhow::Result<Vec<StateEntry>> {
        // Bound the complete poll, including DNS and any mapped-entity fallback.
        // A broken bulk endpoint must not turn one eight-second request into
        // an unbounded sequence of eight-second requests.
        poll_with_deadline(self.fetch_states_inner()).await
    }

    async fn fetch_states_inner(&self) -> anyhow::Result<Vec<StateEntry>> {
        let url = format!("{}/api/states", self.config.base_url.trim_end_matches('/'));
        // SSRF-hardened client built per poll. The HA base_url is
        // config-supplied and this poller is always-on, so route outbound
        // through net::safe_fetch (defense in depth): forbidden-target filter,
        // resolved-IP pin (anti DNS-rebinding), no redirects. RFC1918/ULA stays
        // allowed (HA lives on the LAN), so legitimate polling is unaffected.
        let (client, safe_url) = crate::net::safe_fetch::build_safe_client(&url, HA_TIMEOUT)
            .await
            .map_err(|error| from_safe(&error, "HA resolve/build client"))?;
        self.fetch_states_with_client(&client, safe_url).await
    }

    async fn get_states_response(
        &self,
        client: &reqwest::Client,
        url: reqwest::Url,
        operation: &'static str,
    ) -> anyhow::Result<reqwest::Response> {
        client
            .get(url)
            .bearer_auth(&self.config.bearer_token)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| from_reqwest(&error, operation).into())
    }

    async fn fetch_states_with_client(
        &self,
        client: &reqwest::Client,
        url: reqwest::Url,
    ) -> anyhow::Result<Vec<StateEntry>> {
        let resp = self
            .get_states_response(client, url.clone(), "HA GET /api/states")
            .await?;
        if resp.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR {
            // HA serializes every visible entity for its bulk response. One
            // unrelated integration's invalid attribute can make it return 500
            // while our configured sensors still have valid individual states.
            // Only that status triggers fallback; auth/redirect failures must
            // remain failures. Never follow another host or copy error bodies.
            let bulk_error = ha_response_error(&resp, "HA GET /api/states");
            drop(resp);
            warn!(source_id = %self.id, error_code = bulk_error.code.as_str(),
                error = %bulk_error, "HA bulk read failed; attempting mapped entity recovery within the same poll deadline");
            let states = self.fetch_mapped_states(client, &url).await.map_err(|e| {
                let mut failure = SourceFailure::new(Code::HaFallback, "HA poll recovery");
                failure.causes = vec![bulk_error.clone(), from_anyhow(&e, "HA mapped entity read")];
                failure
            })?;
            warn!(source_id = %self.id, recovered_entities = states.len(),
                error_code = bulk_error.code.as_str(), error = %bulk_error, recovered = true,
                "HA bulk states returned HTTP 500; recovered through mapped entity endpoints. Check HA/proxy logs for the bulk error");
            return Ok(states);
        }
        if !resp.status().is_success() {
            return Err(ha_response_error(&resp, "HA GET /api/states").into());
        }
        let format = response_format(&resp);
        let arr: Vec<Value> = crate::net::safe_fetch::read_json_capped(resp)
            .await
            .map_err(|error| {
                let mut failure = from_safe(&error, "HA decode /api/states");
                failure.response_format = Some(format);
                failure
            })?;
        Ok(arr.into_iter().filter_map(StateEntry::from_value).collect())
    }

    async fn fetch_mapped_states(
        &self,
        client: &reqwest::Client,
        bulk_url: &reqwest::Url,
    ) -> anyhow::Result<Vec<StateEntry>> {
        let entities: std::collections::BTreeSet<&str> = self
            .mapping
            .iter()
            .map(|(_, entity)| entity.as_str())
            .chain(
                self.config
                    .soil_zone_map
                    .iter()
                    .filter(|(_, zone)| !zone.trim().is_empty())
                    .map(|(entity, _)| entity.as_str()),
            )
            .collect();
        if entities.is_empty() {
            return Err(
                SourceFailure::new(Code::HaNoMappings, "HA mapped entity selection").into(),
            );
        }
        let mut remaining = entities.into_iter();
        let mut pending = FuturesUnordered::new();
        for entity in remaining.by_ref().take(ENTITY_FETCH_CONCURRENCY) {
            pending.push(self.fetch_mapped_state(client, bulk_url, entity));
        }
        let mut states = Vec::new();
        while let Some(result) = pending.next().await {
            if let Some(state) = result? {
                states.push(state);
            }
            if let Some(entity) = remaining.next() {
                pending.push(self.fetch_mapped_state(client, bulk_url, entity));
            }
        }
        // Complete the whole read before emitting observations. A failing
        // required mapped endpoint cannot make a partial poll look successful.
        Ok(states)
    }

    async fn fetch_mapped_state(
        &self,
        client: &reqwest::Client,
        bulk_url: &reqwest::Url,
        entity: &str,
    ) -> anyhow::Result<Option<StateEntry>> {
        let mut url = bulk_url.clone();
        url.path_segments_mut()
            .map_err(|_| SourceFailure::new(Code::InvalidUrl, "HA mapped entity URL"))?
            .push(entity);
        let resp = self
            .get_states_response(client, url, "HA GET /api/states/{entity_id}")
            .await
            .map_err(|error| from_anyhow(&error, "HA mapped entity read").with_entity(entity))?;
        // A missing entity is also absent from a successful bulk read.
        // Preserve that absence; never substitute zero or refresh age.
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(ha_response_error(&resp, "HA GET /api/states/{entity_id}")
                .with_entity(entity)
                .into());
        }
        let format = response_format(&resp);
        let value: Value = crate::net::safe_fetch::read_json_capped(resp)
            .await
            .map_err(|error| {
                let mut failure = from_safe(&error, "HA decode mapped entity").with_entity(entity);
                failure.response_format = Some(format);
                failure
            })?;
        let state = StateEntry::from_value(value)
            .filter(|state| state.entity_id == entity)
            .ok_or_else(|| {
                SourceFailure::new(Code::HaEntityState, "HA validate mapped entity")
                    .with_entity(entity)
            })?;
        Ok(Some(state))
    }

    /// Transport reachability and measurement freshness are separate facts.
    /// Only the HTTP request happens now; each reading retains its HA age.
    async fn poll_once(self: Arc<Self>) -> anyhow::Result<Poll> {
        let states = self.fetch_states().await?;
        Ok(self.observations(&states, chrono::Utc::now().timestamp()))
    }

    fn observations(&self, states: &[StateEntry], now: i64) -> Poll {
        let by_id: std::collections::HashMap<_, _> =
            states.iter().map(|s| (s.entity_id.as_str(), s)).collect();
        // Batch only readings with the SAME report time. A fresh temperature
        // must not refresh an old rain total or a zone's stale soil channel.
        let mut batches: BTreeMap<i64, Vec<(WeatherField, f64)>> = BTreeMap::new();
        for (field, entity_id) in &self.mapping {
            let Some(state) = by_id.get(entity_id.as_str()) else {
                debug!(source_id = %self.id, entity_id, "ha_passthrough entity not present in /api/states");
                continue;
            };
            let Some((value, report_epoch)) = state.reading(now) else {
                debug!(source_id = %self.id, entity_id, "ha_passthrough entity has no valid current report");
                continue;
            };
            let epoch = if *field == WeatherField::RainLastMinIn {
                let Some(epoch) = state.rain_at_epoch.filter(|e| *e > 0 && *e <= report_epoch)
                else {
                    continue;
                };
                epoch
            } else {
                report_epoch
            };
            let value = crate::sources::units::to_canonical(*field, value, state.unit.as_deref());
            if value.is_finite() {
                batches.entry(epoch).or_default().push((*field, value));
            }
        }
        let mut poll = Poll::none();
        for (at_epoch, fields) in batches {
            poll.events
                .extend(Poll::observation(&self.id, fields, at_epoch).events);
        }
        for (entity_id, zone) in &self.config.soil_zone_map {
            let zone = zone.trim();
            if zone.is_empty() {
                continue;
            }
            let Some(state) = by_id.get(entity_id.as_str()) else {
                continue;
            };
            let Some((value, at_epoch)) = state.reading(now) else {
                continue;
            };
            poll = poll.with(SourceEvent::KeyedReading {
                source_id: self.id.clone(),
                key: crate::sources::bus_recorder::zone_soil_key(zone),
                value,
                at_epoch,
            });
        }
        poll
    }
}

async fn poll_with_deadline<T>(
    poll: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(HA_TIMEOUT, poll).await.map_err(|_| {
        let mut failure = SourceFailure::new(Code::Timeout, "HA complete state poll");
        failure.timeout_ms = Some(HA_TIMEOUT.as_millis() as u64);
        failure
    })?
}

/// Fixed diagnostic categories, never upstream bodies, redirect locations,
/// bearer tokens or configured URLs. A 500 does not identify which hop failed.
fn ha_response_error(resp: &reqwest::Response, operation: &'static str) -> SourceFailure {
    SourceFailure::http(
        resp.status().as_u16(),
        Some(response_format(resp)),
        operation,
    )
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
        "rainlastminin" | "raininlastmin" => WeatherField::RainLastMinIn,
        "rainintensityinhr" | "hourlyrainin" => WeatherField::RainIntensityInHr,
        "lightningcount" => WeatherField::LightningCount,
        "lightningdistancemi" => WeatherField::LightningDistanceMi,
        "et0today" => WeatherField::Et0Today,
        "flowgpm" | "flowrate" | "flowratepm" => WeatherField::FlowGpm,
        "flowtotalgaltoday" | "flowtotalgallons" | "flowtoday" => WeatherField::FlowTotalGalToday,
        "leafwetnesspct" | "leafwetness" | "wetness" => WeatherField::LeafWetness,
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
            if *f == WeatherField::RainLastMinIn {
                fields.insert(WeatherField::RainTodayIn);
            }
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
        if self.capabilities().fields.contains(&field) {
            30
        } else {
            i32::MIN
        }
    }

    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()> {
        // The shared loop logs the start; this line carries the mapping
        // size an operator needs when the source stays quiet.
        info!(
            source_id = %self.id,
            mapping_n = self.mapping.len(),
            soil_zones = self.config.soil_zone_map.len(),
            "HaPassthrough polling /api/states",
        );
        if self.mapping.is_empty() && self.config.soil_zone_map.is_empty() {
            warn!(source_id = %self.id, "HaPassthrough has empty field_map + soil_zone_map; idle");
        }
        let id = self.id.clone();
        run_polling(
            self,
            &id,
            "HaPassthrough",
            POLL_INTERVAL,
            bus,
            shutdown,
            Self::poll_once,
        )
        .await
    }
}

#[cfg(test)]
#[path = "ha_passthrough_http_tests.rs"]
mod http_tests;

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
            soil_zone_map: Default::default(),
        }
    }

    fn state(entity: &str, value: &str, epoch: i64) -> Value {
        serde_json::json!({"entity_id": entity, "state": value,
            "last_reported": chrono::DateTime::from_timestamp(epoch, 0).unwrap().to_rfc3339(),
            "attributes": {"unit_of_measurement": "°C"}})
    }

    #[test]
    fn weatherflow_minute_identity_survives_ha_republication_and_counts_equal_new_minutes() {
        let source = HaPassthrough::new("ha", cfg_with_map(&[("RainLastMinIn", "sensor.rain")]));
        assert!(source
            .capabilities()
            .fields
            .contains(&WeatherField::RainTodayIn));
        let now = 1_700_000_100;
        let store = crate::weather::LiveWeatherStore::new();
        for (report, period, amount, expected) in [
            (now, now, "2.54", 0.1),
            (now + 30, now, "2.54", 0.1),
            (now + 60, now + 60, "2.54", 0.2),
            (now + 120, now + 120, "0", 0.2),
        ] {
            let mut v = state("sensor.rain", amount, report);
            v["attributes"]["unit_of_measurement"] = serde_json::json!("mm");
            v["attributes"]["last_reset"] =
                serde_json::json!(chrono::DateTime::from_timestamp(period, 0)
                    .unwrap()
                    .to_rfc3339());
            let poll = source.observations(&[StateEntry::from_value(v).unwrap()], report);
            for event in poll.events {
                let SourceEvent::Observation {
                    source_id,
                    fields,
                    at_epoch,
                } = event
                else {
                    panic!("observation")
                };
                assert_eq!(at_epoch, period);
                store.apply_received_fields(&fields, at_epoch, report, true, &source_id);
            }
            assert!((store.snapshot().rain_in_today - expected).abs() < 1e-9);
        }
    }

    #[test]
    fn invalid_minute_period_cannot_become_new_rain() {
        let source = HaPassthrough::new("ha", cfg_with_map(&[("RainLastMinIn", "sensor.rain")]));
        let now = 1_700_000_100;
        for period in [
            serde_json::json!("bad"),
            serde_json::Value::Null,
            serde_json::json!(chrono::DateTime::from_timestamp(now + 1, 0)
                .unwrap()
                .to_rfc3339()),
        ] {
            let mut v = state("sensor.rain", "0.1", now);
            v["attributes"]["last_reset"] = period;
            assert!(source
                .observations(&[StateEntry::from_value(v).unwrap()], now)
                .events
                .is_empty());
        }
    }

    #[test]
    fn polling_a_frozen_ha_sensor_does_not_refresh_its_observation() {
        let source = HaPassthrough::new("ha", cfg_with_map(&[("AirTempF", "sensor.temp")]));
        let old = 1_700_000_000;
        let states = vec![StateEntry::from_value(state("sensor.temp", "25", old)).unwrap()];
        for now in [old + 86_400, old + 86_430] {
            let poll = source.observations(&states, now);
            assert_eq!(poll.events.len(), 1);
            let SourceEvent::Observation {
                fields, at_epoch, ..
            } = &poll.events[0]
            else {
                panic!("observation")
            };
            assert_eq!(*at_epoch, old);
            assert_eq!(fields, &vec![(WeatherField::AirTempF, 77.0)]);
        }
    }

    #[test]
    fn unchanged_values_use_the_new_report_not_the_old_value_change() {
        let now = 1_700_000_100;
        let mut v = state("sensor.temp", "25", now);
        v["last_updated"] = serde_json::json!("2000-01-01T00:00:00Z");
        v["last_changed"] = v["last_updated"].clone();
        let entry = StateEntry::from_value(v).unwrap();
        assert_eq!(entry.reading(now), Some((25.0, now)));
    }

    #[test]
    fn weather_and_soil_entities_keep_independent_report_times() {
        let now = 1_700_000_100;
        let mut cfg = cfg_with_map(&[("AirTempF", "sensor.temp"), ("RhPct", "sensor.rh")]);
        cfg.soil_zone_map
            .insert("sensor.soil".into(), "orchard".into());
        let source = HaPassthrough::new("ha", cfg);
        let states: Vec<_> = [
            state("sensor.temp", "25", now),
            state("sensor.rh", "50", now - 3600),
            state("sensor.soil", "30", now - 7200),
        ]
        .into_iter()
        .filter_map(StateEntry::from_value)
        .collect();
        let poll = source.observations(&states, now);
        assert_eq!(poll.events.len(), 3);
        let mut seen = BTreeMap::new();
        for event in poll.events {
            match event {
                SourceEvent::Observation {
                    fields, at_epoch, ..
                } => {
                    for (field, _) in fields {
                        seen.insert(format!("{field:?}"), at_epoch);
                    }
                }
                SourceEvent::KeyedReading { key, at_epoch, .. } => {
                    seen.insert(key, at_epoch);
                }
                _ => panic!("only observations"),
            }
        }
        assert_eq!(seen["AirTempF"], now);
        assert_eq!(seen["RhPct"], now - 3600);
        assert_eq!(seen["soilmoisture_orchard"], now - 7200);
    }

    #[test]
    fn older_ha_reports_use_last_updated_without_inventing_freshness() {
        let now = 1_700_000_100;
        let mut v = state("sensor.temp", "0", now - 600);
        let report = v.as_object_mut().unwrap().remove("last_reported").unwrap();
        v["last_updated"] = report;
        assert_eq!(
            StateEntry::from_value(v.clone()).unwrap().reading(now),
            Some((0.0, now - 600))
        );
        v["last_reported"] = serde_json::json!("malformed");
        assert_eq!(StateEntry::from_value(v).unwrap().reading(now), None);
    }

    #[test]
    fn absent_future_or_restored_reports_cannot_supply_current_weather() {
        let now = 1_700_000_100;
        let mut missing = state("sensor.temp", "25", now);
        missing.as_object_mut().unwrap().remove("last_reported");
        let mut restored = state("sensor.temp", "25", now);
        restored["attributes"]["restored"] = serde_json::json!(true);
        for v in [
            missing,
            restored,
            state("sensor.temp", "25", now + 1),
            state("sensor.temp", "25", 0),
        ] {
            assert_eq!(StateEntry::from_value(v).unwrap().reading(now), None);
        }
    }

    #[test]
    fn unavailable_and_nonfinite_ha_states_emit_neither_weather_nor_soil() {
        let now = 1_700_000_100;
        let mut cfg = cfg_with_map(&[("AirTempF", "sensor.temp")]);
        cfg.soil_zone_map
            .insert("sensor.temp".into(), "orchard".into());
        let source = HaPassthrough::new("ha", cfg);
        for value in ["unknown", "unavailable", "NaN", "inf", "-inf"] {
            let entries = vec![StateEntry::from_value(state("sensor.temp", value, now)).unwrap()];
            assert!(
                source.observations(&entries, now).events.is_empty(),
                "{value}"
            );
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
