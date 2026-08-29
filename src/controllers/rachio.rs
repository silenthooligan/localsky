// Rachio cloud controller, Gen 2 / Gen 3.
//
// Talks to the Rachio public v1 API (default https://api.rach.io/1/public)
// via Bearer-token auth. Each LocalSky zone slug maps to a Rachio zone UUID;
// the map is filled by the editor's "Scan zones" merge, by wizard-imported
// zone entries (overlaid in runtime::build_controllers), or by hand.
//
// Endpoints used (public v1):
//   PUT /zone/start                      body {id, duration}   run one zone
//   PUT /device/stop_water               body {id}             stop the DEVICE
//   GET /device/{deviceId}                                     device + zones
//   GET /device/{deviceId}/current_schedule                    live running state
//   GET /person/info -> GET /person/{id}                       device discovery
//
// Caveats:
//   - NO per-zone stop in the public API. The only stop operation is
//     device-wide `stop_water`, so `stop_zone` stops every running zone on
//     the device; caps advertise `per_zone_stop: false` and consumers
//     (reaper, manual Stop) say so.
//   - Rate limit: roughly 1700 requests/day per token. `status()` throttles
//     itself to `poll_interval_s` (default 120s, floor 60s) and serves the
//     cached snapshot inside that window, so the refresher's fast tick does
//     not translate 1:1 into cloud calls. Each live poll costs two calls
//     (device + current_schedule).
//   - A single zone start is capped at 3 hours (10800s); longer requests
//     are clamped with a warning.
//   - Run history via GET /device/{id}/event is NOT wired: the endpoint is
//     semi-deprecated and its shape is unverified against live hardware, so
//     `history_query` is false and `run_history` returns empty. Webhook
//     ingest (ZONE_STATUS events) is the documented future path for exact
//     run edges.
//   - Auth failures (401/403) map to ControllerError::AuthFailed; 429 maps
//     to RateLimited with no retry (the status throttle prevents a storm).
//   - Every parse here is fixture-driven and defensive: the shapes were
//     written against Rachio's documented v1 responses, not verified live.
//     A current_schedule shape surprise degrades to running-unknown
//     (`running_known: false`, previous running values carried forward),
//     never a panic and never a fabricated idle.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::config::schema::RachioConfig;
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerError, ControllerResult, ControllerStatus, IrrigationController,
    RunHandle, RunRecord, ZoneRuntimeStatus,
};

const DEFAULT_API_BASE: &str = "https://api.rach.io/1/public";

/// Default and floor for the status-poll throttle (seconds). 120s spends
/// about 1440 of the ~1700 daily requests on polling (two calls per poll),
/// leaving headroom for dispatch; 60s is the floor because anything faster
/// exhausts the budget before the day ends.
const DEFAULT_POLL_INTERVAL_S: u32 = 120;
const MIN_POLL_INTERVAL_S: u32 = 60;

/// Live running state parsed from GET /device/{id}/current_schedule.
#[derive(Debug, Clone, PartialEq)]
enum CurrentSchedule {
    /// Nothing running (empty body or empty object).
    Idle,
    /// A zone is running. `remaining_s` is start + duration - now when the
    /// response carried both; None when it ran a zone but the timing fields
    /// were absent.
    Running {
        zone_uuid: String,
        remaining_s: Option<u32>,
    },
    /// The response had content but not a shape we recognize. Callers must
    /// treat running state as UNKNOWN: carry the last known values forward
    /// with `running_known: false`, never fabricate idle.
    Unknown,
}

/// Parse the current_schedule response. `body` is None when the endpoint
/// answered with an empty body (Rachio's idle signal alongside `{}`).
fn parse_current_schedule(body: Option<&Value>, now_epoch: i64) -> CurrentSchedule {
    let Some(v) = body else {
        return CurrentSchedule::Idle;
    };
    match v {
        Value::Null => CurrentSchedule::Idle,
        Value::Object(m) if m.is_empty() => CurrentSchedule::Idle,
        Value::Object(m) => {
            let Some(zone_uuid) = m.get("zoneId").and_then(|z| z.as_str()) else {
                // Content without a zoneId is a shape we do not recognize.
                return CurrentSchedule::Unknown;
            };
            let start_ms = m
                .get("zoneStartDate")
                .and_then(|s| s.as_i64().or_else(|| s.as_f64().map(|f| f as i64)));
            let duration_s = m
                .get("zoneDuration")
                .and_then(|d| d.as_i64().or_else(|| d.as_f64().map(|f| f as i64)));
            let remaining_s = match (start_ms, duration_s) {
                (Some(start_ms), Some(dur_s)) => {
                    let end_epoch = start_ms / 1000 + dur_s;
                    Some((end_epoch - now_epoch).max(0) as u32)
                }
                _ => None,
            };
            CurrentSchedule::Running {
                zone_uuid: zone_uuid.to_string(),
                remaining_s,
            }
        }
        _ => CurrentSchedule::Unknown,
    }
}

/// Enabled zones from a GET /device/{id} response, in DiscoveredZone form
/// (uuid as station_id; name falls back to "Zone N" from zoneNumber).
fn discovered_from_device(
    device: &Value,
) -> Vec<crate::ports::irrigation_controller::DiscoveredZone> {
    use crate::ports::irrigation_controller::DiscoveredZone;
    let zones = device
        .get("zones")
        .and_then(|z| z.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for z in &zones {
        let Some(uuid) = z.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        // Skip disabled zones, they can't be watered.
        if z.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let name = z
            .get("name")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                let n = z.get("zoneNumber").and_then(|v| v.as_u64()).unwrap_or(0);
                format!("Zone {n}")
            });
        out.push(DiscoveredZone {
            station_id: uuid.to_string(),
            name,
        });
    }
    out
}

/// GET /person/info response: the authenticated account's person id.
#[derive(Debug, Deserialize)]
struct RachioPersonInfo {
    id: String,
}

/// Result of resolving a device from just the API token (the wizard's
/// device auto-discovery). `device_count` lets the caller say when the
/// account has more than one device and the first was picked.
#[derive(Debug, Clone)]
pub struct RachioDeviceSummary {
    pub device_id: String,
    pub name: Option<String>,
    pub device_count: usize,
}

pub struct Rachio {
    id: String,
    config: RachioConfig,
    client: Client,
    /// Reverse map (zone uuid -> slug) computed from config at construction
    /// for fast lookup during status(), Rachio returns zones by uuid.
    uuid_to_slug: HashMap<String, String>,
    /// Last successful status snapshot. Serves three jobs: the throttled
    /// answer inside the poll window, the stale fallback when the API is
    /// flaky (reachable=false), and the carry-forward source when running
    /// state is unknowable.
    last_status: Arc<Mutex<Option<ControllerStatus>>>,
    /// When the last LIVE fetch was attempted (success or failure). Gates
    /// re-fetches so a failing API does not turn the 10s refresher tick
    /// into a call storm against the daily budget.
    last_attempt: Arc<Mutex<Option<Instant>>>,
    /// Most recent X-RateLimit-Remaining header the cloud sent, surfaced in
    /// the controller test result. std Mutex: never held across an await.
    rate_limit_remaining: std::sync::Mutex<Option<String>>,
    /// Epoch since which the live running state has been UNKNOWABLE
    /// (current_schedule failing or answering an unrecognized shape).
    /// None while state reads normally. Bounds the carry-forward: see
    /// `carry_forward_expired`.
    unknown_since: Arc<Mutex<Option<i64>>>,
    /// Planned end (+ grace) of the most recent run_zone dispatch. Keeps
    /// the carry-forward alive across an outage for as long as a
    /// dispatched run could legitimately still be watering.
    last_dispatch_deadline: Arc<Mutex<Option<i64>>>,
}

/// Whether carried-forward running flags have outlived their credibility:
/// past max(unknown_since + 2 poll intervals, the last dispatched run's
/// planned end + grace) the adapter degrades carried running to
/// not-running (still running_known=false) so a permanent shape break
/// cannot pin zones "watering" forever in the UI and downstream state.
fn carry_forward_expired(
    now_epoch: i64,
    unknown_since_epoch: i64,
    poll_interval_s: i64,
    last_dispatch_deadline: Option<i64>,
) -> bool {
    let ttl_edge =
        (unknown_since_epoch + 2 * poll_interval_s).max(last_dispatch_deadline.unwrap_or(i64::MIN));
    now_epoch > ttl_edge
}

impl Rachio {
    pub fn new(id: impl Into<String>, config: RachioConfig) -> Result<Self, ControllerError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| ControllerError::Init(format!("reqwest client: {e}")))?;
        let uuid_to_slug = config
            .zone_uuid_map
            .iter()
            .map(|(slug, uuid)| (uuid.clone(), slug.clone()))
            .collect();
        Ok(Self {
            id: id.into(),
            config,
            client,
            uuid_to_slug,
            last_status: Arc::new(Mutex::new(None)),
            last_attempt: Arc::new(Mutex::new(None)),
            rate_limit_remaining: std::sync::Mutex::new(None),
            unknown_since: Arc::new(Mutex::new(None)),
            last_dispatch_deadline: Arc::new(Mutex::new(None)),
        })
    }

    fn api_base(&self) -> &str {
        self.config
            .base_url
            .as_deref()
            .map(|s| s.trim_end_matches('/'))
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_API_BASE)
    }

    /// Effective status-poll throttle: configured value clamped to the
    /// floor, default 120s.
    fn poll_interval(&self) -> Duration {
        let s = self
            .config
            .poll_interval_s
            .unwrap_or(DEFAULT_POLL_INTERVAL_S)
            .max(MIN_POLL_INTERVAL_S);
        Duration::from_secs(s as u64)
    }

    fn uuid_for(&self, slug: &str) -> Result<String, ControllerError> {
        self.config
            .zone_uuid_map
            .get(slug)
            .cloned()
            .ok_or_else(|| ControllerError::ZoneUnknown(slug.to_string()))
    }

    fn note_rate_limit(&self, resp: &reqwest::Response) {
        if let Some(v) = resp
            .headers()
            .get("x-ratelimit-remaining")
            .and_then(|h| h.to_str().ok())
        {
            if let Ok(mut slot) = self.rate_limit_remaining.lock() {
                *slot = Some(v.to_string());
            }
        }
    }

    /// The most recent X-RateLimit-Remaining header seen, if any. Read by
    /// the wizard's controller test so the operator can watch the daily
    /// budget.
    pub fn last_rate_limit_remaining(&self) -> Option<String> {
        self.rate_limit_remaining
            .lock()
            .ok()
            .and_then(|s| s.clone())
    }

    /// Map non-2xx to ControllerError; notes the rate-limit header first.
    fn check_response(&self, resp: &reqwest::Response) -> Result<(), ControllerError> {
        self.note_rate_limit(resp);
        let status = resp.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(ControllerError::AuthFailed);
        }
        if status.as_u16() == 429 {
            return Err(ControllerError::RateLimited);
        }
        Ok(())
    }

    async fn put_json(
        &self,
        path: &str,
        body: Value,
    ) -> Result<reqwest::Response, ControllerError> {
        let url = format!("{}{}", self.api_base(), path);
        let resp = self
            .client
            .put(&url)
            .bearer_auth(&self.config.api_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ControllerError::Transport(crate::net::reqwest_error_category(&e).to_string())
            })?;
        self.check_response(&resp)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ControllerError::Remote(format!("HTTP {status}: {body}")));
        }
        Ok(resp)
    }

    async fn get_json(&self, path: &str) -> Result<Value, ControllerError> {
        match self.get_json_opt(path).await? {
            Some(v) => Ok(v),
            None => Err(ControllerError::Remote("empty response body".into())),
        }
    }

    /// GET returning Ok(None) for an EMPTY body. current_schedule answers
    /// with nothing at all when the device is idle, which a plain .json()
    /// would reject as invalid JSON.
    async fn get_json_opt(&self, path: &str) -> Result<Option<Value>, ControllerError> {
        let url = format!("{}{}", self.api_base(), path);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.config.api_token)
            .send()
            .await
            .map_err(|e| {
                ControllerError::Transport(crate::net::reqwest_error_category(&e).to_string())
            })?;
        self.check_response(&resp)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ControllerError::Remote(format!("HTTP {status}: {body}")));
        }
        let text = resp
            .text()
            .await
            .map_err(|e| ControllerError::Remote(format!("read body: {e}")))?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| ControllerError::Remote(format!("invalid json: {e}")))
    }

    /// Resolve the account's device from just the api_token:
    /// GET /person/info -> person id, GET /person/{id} -> first device.
    /// Backs the wizard's device auto-discovery; the caller reports the
    /// result, it is never written into config silently.
    pub async fn resolve_device_id(&self) -> Result<RachioDeviceSummary, ControllerError> {
        let info = self.get_json("/person/info").await?;
        let person: RachioPersonInfo = serde_json::from_value(info)
            .map_err(|_| ControllerError::Remote("person/info response had no id".into()))?;
        let full = self.get_json(&format!("/person/{}", person.id)).await?;
        let devices = full
            .get("devices")
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        let Some(first) = devices.first() else {
            return Err(ControllerError::Remote("no devices on this account".into()));
        };
        let device_id = first
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ControllerError::Remote("device entry had no id".into()))?;
        Ok(RachioDeviceSummary {
            device_id: device_id.to_string(),
            name: first
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            device_count: devices.len(),
        })
    }

    /// Build zone_states from the device payload + the live schedule.
    /// `schedule = Unknown` carries the previous poll's running flags
    /// forward with running_known=false (never fabricated idle).
    /// `carry_expired = true` bounds that carry: the unknown state has
    /// outlived any run that could still be watering, so carried running
    /// degrades to not-running (still marked unknown).
    fn zone_states_from(
        &self,
        device: &Value,
        schedule: &CurrentSchedule,
        previous: Option<&ControllerStatus>,
        carry_expired: bool,
    ) -> Vec<ZoneRuntimeStatus> {
        let zones_arr = device
            .get("zones")
            .and_then(|z| z.as_array())
            .cloned()
            .unwrap_or_default();
        let mut zone_states = Vec::new();
        for z in &zones_arr {
            let uuid = match z.get("id").and_then(|v| v.as_str()) {
                Some(u) => u.to_string(),
                None => continue,
            };
            let slug = match self.uuid_to_slug.get(&uuid) {
                Some(s) => s.clone(),
                None => continue, // zone not mapped, ignore
            };
            let last_run_epoch = z
                .get("lastWateredDate")
                .and_then(|v| v.as_i64())
                // Rachio returns ms, convert.
                .map(|ms| ms / 1000);
            let (running, remaining_s, running_known) = match schedule {
                CurrentSchedule::Idle => (false, None, true),
                CurrentSchedule::Running {
                    zone_uuid,
                    remaining_s,
                } => {
                    let is_this = zone_uuid == &uuid;
                    (is_this, remaining_s.filter(|_| is_this), true)
                }
                CurrentSchedule::Unknown => {
                    // Carry the last KNOWN running value forward so a shape
                    // surprise never fabricates an idle edge for the run
                    // observer; mark it unknown. Once the carry has
                    // outlived any legitimately-still-watering run
                    // (carry_expired), degrade to not-running so a
                    // permanent break cannot show "watering" forever.
                    let prev_running = !carry_expired
                        && previous
                            .and_then(|p| p.zone_states.iter().find(|s| s.slug == slug))
                            .map(|s| s.running)
                            .unwrap_or(false);
                    (prev_running, None, false)
                }
            };
            zone_states.push(ZoneRuntimeStatus {
                slug,
                running,
                remaining_s,
                last_run_epoch,
                running_known,
            });
        }
        zone_states
    }
}

#[async_trait]
impl IrrigationController for Rachio {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            // The public v1 API exposes no live flow reading; advertising a
            // meter published a phantom capability. Same for the rain
            // sensor: its state only arrives via webhooks, which are not
            // wired.
            flow_meter: false,
            rain_sensor: false,
            master_valve: true,
            // Rachio waters one zone at a time.
            multi_zone_parallel: false,
            // run_history is not wired (see header); saying true while
            // returning empty violated the conformance contract.
            history_query: false,
            remote_program_upload: false,
            water_level: false,
            // The public API's only stop is device-wide stop_water.
            per_zone_stop: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        let zone_uuid = self.uuid_for(slug)?;
        // Rachio API caps a single zone start at 3 hours (10800s).
        let clamped = duration_s.clamp(1, 10_800);
        if clamped != duration_s {
            warn!(
                slug = %slug,
                requested = duration_s,
                clamped = clamped,
                "Rachio caps zone duration at 10800s; clamping"
            );
        }
        self.put_json(
            "/zone/start",
            json!({ "id": zone_uuid, "duration": clamped }),
        )
        .await?;
        // Remember how long a dispatched run could legitimately water, so
        // an unknown-state outage keeps carrying "running" at least that
        // long (see carry_forward_expired).
        *self.last_dispatch_deadline.lock().await = Some(
            chrono::Utc::now().timestamp()
                + clamped as i64
                + crate::controllers::reaper::CLOUD_DEVICE_STOP_GRACE_S,
        );
        // Reopen the throttle window: the next status() must poll LIVE so
        // the run-edge observer sees the rising edge within one refresher
        // tick instead of up to a full poll interval later (a short
        // cycle-soak segment could otherwise start and finish entirely
        // inside the cached window and never be recorded). One extra cloud
        // read per dispatch, negligible against the daily budget.
        *self.last_attempt.lock().await = None;
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: chrono::Utc::now().timestamp(),
            planned_duration_s: clamped,
            provider_ref: Some(zone_uuid),
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        // No per-zone stop in the public API (caps.per_zone_stop = false):
        // the only stop is device-wide stop_water, so THIS STOPS EVERY
        // RUNNING ZONE on the device. Consumers surface that scope.
        warn!(
            slug = %slug,
            "Rachio has no per-zone stop; issuing device-wide stop_water (all running zones on this device stop)"
        );
        // Validate slug is mapped, fail fast on typos rather than
        // surprise-stop the whole controller for an unknown zone.
        self.uuid_for(slug)?;
        self.put_json("/device/stop_water", json!({ "id": self.config.device_id }))
            .await?;
        // Reopen the throttle window so the next status() polls live and
        // the UI drops the running banner promptly instead of serving the
        // stale "running" snapshot for up to a poll interval.
        *self.last_attempt.lock().await = None;
        Ok(())
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        self.put_json("/device/stop_water", json!({ "id": self.config.device_id }))
            .await?;
        *self.last_attempt.lock().await = None;
        Ok(())
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let interval = self.poll_interval();

        // Throttle: inside the poll window, serve the cache (or admit we
        // have nothing yet) WITHOUT touching the cloud. The refresher ticks
        // every 10s; without this gate that is ~8640 device reads/day
        // against a ~1700/day budget.
        {
            let attempt = self.last_attempt.lock().await;
            if let Some(at) = *attempt {
                if at.elapsed() < interval {
                    let cache = self.last_status.lock().await;
                    if let Some(c) = cache.as_ref() {
                        return Ok(c.clone());
                    }
                    // Last attempt failed and nothing is cached; stay quiet
                    // until the window passes rather than hammering the API.
                    return Err(ControllerError::Offline);
                }
            }
        }
        *self.last_attempt.lock().await = Some(Instant::now());

        let path = format!("/device/{}", self.config.device_id);
        let device = match self.get_json(&path).await {
            Ok(v) => v,
            Err(e) => {
                // Fall back to last known-good snapshot if the API is
                // momentarily unreachable. Mark reachable=false so the
                // dashboard surfaces the degraded state.
                debug!("rachio status fetch failed, falling back to last_status: {e}");
                // The attempt gate above already covers the failure path
                // (each failed poll was still a cloud call), so serving the
                // stale snapshot needs no bookkeeping beyond the flag.
                let cache = self.last_status.lock().await;
                if let Some(c) = cache.as_ref() {
                    let mut stale = c.clone();
                    stale.reachable = false;
                    return Ok(stale);
                }
                return Err(e);
            }
        };

        // Live running state comes from current_schedule, not the device
        // payload (which carries no live running marker). A failure or an
        // unrecognized shape here must NOT read as idle: degrade to
        // running-unknown and carry the previous flags forward.
        let schedule = match self
            .get_json_opt(&format!(
                "/device/{}/current_schedule",
                self.config.device_id
            ))
            .await
        {
            Ok(body) => parse_current_schedule(body.as_ref(), chrono::Utc::now().timestamp()),
            Err(e) => {
                debug!("rachio current_schedule fetch failed; running state unknown: {e}");
                CurrentSchedule::Unknown
            }
        };
        // Carry-forward bookkeeping: track how long running state has been
        // unknowable and degrade the carried flags once that outlives any
        // run that could still be watering.
        let carry_expired = if schedule == CurrentSchedule::Unknown {
            let now_epoch = chrono::Utc::now().timestamp();
            let mut unknown_since = self.unknown_since.lock().await;
            let since = *unknown_since.get_or_insert(now_epoch);
            let expired = carry_forward_expired(
                now_epoch,
                since,
                self.poll_interval().as_secs() as i64,
                *self.last_dispatch_deadline.lock().await,
            );
            warn!(
                controller = %self.id,
                carry_expired = expired,
                "rachio running state unknowable; keeping last known running flags (running_known=false)"
            );
            expired
        } else {
            *self.unknown_since.lock().await = None;
            false
        };

        let previous = {
            let cache = self.last_status.lock().await;
            cache.clone()
        };
        let zone_states =
            self.zone_states_from(&device, &schedule, previous.as_ref(), carry_expired);

        let status = ControllerStatus {
            reachable: true,
            master_enabled: device
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s == "ONLINE"),
            water_level_pct: None, // Rachio doesn't expose this directly
            // Only webhooks carry rain-sensor state; polling has no live
            // source, so None (unknown), never a fabricated reading.
            rain_sensor_tripped: None,
            // The device payload names configured schedules, not what is
            // running now; naming one here misrepresented it as live state.
            current_program: None,
            zone_states,
            flow_gpm: None,
            flow_connected: false,
            firmware: device
                .get("firmwareVersion")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        let mut cache = self.last_status.lock().await;
        *cache = Some(status.clone());
        Ok(status)
    }

    async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        // Not wired: the /event endpoint is semi-deprecated and its shape is
        // unverified against live hardware, and no ingest path consumed the
        // records it would produce. history_query is false; webhook-based
        // event backfill is the documented deferral. LocalSky's own run-edge
        // observer records Rachio runs from live status instead.
        Ok(Vec::new())
    }

    async fn discover_zones(
        &self,
    ) -> ControllerResult<Vec<crate::ports::irrigation_controller::DiscoveredZone>> {
        let device = self
            .get_json(&format!("/device/{}", self.config.device_id))
            .await?;
        Ok(discovered_from_device(&device))
    }
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Example ids only; nothing here is a live credential or a real device.
    const TOKEN: &str = "example-api-token";
    const DEVICE: &str = "device-0001";
    const UUID_FRONT: &str = "1f00aa00-0000-4000-8000-000000000001";
    const UUID_BACK: &str = "1f00aa00-0000-4000-8000-000000000002";

    fn cfg() -> RachioConfig {
        RachioConfig {
            api_token: TOKEN.into(),
            device_id: DEVICE.into(),
            zone_uuid_map: {
                let mut m = BTreeMap::new();
                m.insert("front".into(), UUID_FRONT.into());
                m.insert("back".into(), UUID_BACK.into());
                m
            },
            poll_interval_s: None,
            base_url: None,
        }
    }

    fn cfg_at(base_url: &str) -> RachioConfig {
        RachioConfig {
            base_url: Some(base_url.to_string()),
            ..cfg()
        }
    }

    /// Recorded-fixture device payload (documented v1 shape; unverified
    /// against live hardware, see the module header).
    fn device_fixture() -> Value {
        json!({
            "id": DEVICE,
            "status": "ONLINE",
            "firmwareVersion": "iro3-firmware-example",
            "zones": [
                {
                    "id": UUID_FRONT,
                    "zoneNumber": 1,
                    "name": "Front",
                    "enabled": true,
                    "lastWateredDate": 1756300000000i64,
                },
                {
                    "id": UUID_BACK,
                    "zoneNumber": 2,
                    "name": "Back",
                    "enabled": true,
                },
                {
                    "id": "1f00aa00-0000-4000-8000-000000000003",
                    "zoneNumber": 3,
                    "name": "Disabled Bed",
                    "enabled": false,
                },
                {
                    "id": "1f00aa00-0000-4000-8000-000000000004",
                    "zoneNumber": 4,
                    "enabled": true,
                },
            ],
        })
    }

    // ---- pure parse tests (fixture-driven) ----

    #[test]
    fn current_schedule_running_shape() {
        let now = 1_756_300_600i64;
        let body = json!({
            "zoneId": UUID_FRONT,
            "zoneStartDate": 1_756_300_000_000i64, // ms
            "zoneDuration": 900,                    // seconds
        });
        let parsed = parse_current_schedule(Some(&body), now);
        assert_eq!(
            parsed,
            CurrentSchedule::Running {
                zone_uuid: UUID_FRONT.into(),
                // start 1_756_300_000 + 900 - now 1_756_300_600 = 300
                remaining_s: Some(300),
            }
        );
    }

    #[test]
    fn current_schedule_remaining_clamps_at_zero() {
        let body = json!({
            "zoneId": UUID_FRONT,
            "zoneStartDate": 1_756_300_000_000i64,
            "zoneDuration": 60,
        });
        match parse_current_schedule(Some(&body), 1_756_399_999) {
            CurrentSchedule::Running { remaining_s, .. } => assert_eq!(remaining_s, Some(0)),
            other => panic!("expected Running, got {other:?}"),
        }
    }

    #[test]
    fn current_schedule_idle_shapes() {
        // Empty body and empty object both mean idle.
        assert_eq!(parse_current_schedule(None, 0), CurrentSchedule::Idle);
        assert_eq!(
            parse_current_schedule(Some(&json!({})), 0),
            CurrentSchedule::Idle
        );
        assert_eq!(
            parse_current_schedule(Some(&Value::Null), 0),
            CurrentSchedule::Idle
        );
    }

    #[test]
    fn current_schedule_shape_surprise_is_unknown_not_idle() {
        // Content without a zoneId string is a shape we do not recognize;
        // fabricating idle here is exactly the defect this parse replaces.
        for surprise in [
            json!({ "scheduleId": "s-1", "status": "PROCESSING" }),
            json!([1, 2, 3]),
            json!("running"),
            json!({ "zoneId": 42 }),
        ] {
            assert_eq!(
                parse_current_schedule(Some(&surprise), 0),
                CurrentSchedule::Unknown,
                "shape {surprise} must be Unknown"
            );
        }
    }

    #[test]
    fn current_schedule_running_without_timing_has_no_remaining() {
        let body = json!({ "zoneId": UUID_BACK });
        assert_eq!(
            parse_current_schedule(Some(&body), 0),
            CurrentSchedule::Running {
                zone_uuid: UUID_BACK.into(),
                remaining_s: None,
            }
        );
    }

    #[test]
    fn discover_skips_disabled_zones_and_falls_back_to_zone_number_names() {
        let zones = discovered_from_device(&device_fixture());
        let names: Vec<&str> = zones.iter().map(|z| z.name.as_str()).collect();
        assert_eq!(names, vec!["Front", "Back", "Zone 4"]);
        assert_eq!(zones[0].station_id, UUID_FRONT);
        assert!(
            !zones.iter().any(|z| z.name == "Disabled Bed"),
            "disabled zones cannot be watered and must not be offered"
        );
    }

    // ---- construction / mapping ----

    #[test]
    fn uuid_for_known_and_unknown_slug() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        assert_eq!(r.uuid_for("front").unwrap(), UUID_FRONT);
        assert!(matches!(
            r.uuid_for("side"),
            Err(ControllerError::ZoneUnknown(_))
        ));
    }

    #[test]
    fn reverse_map_populated() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        assert_eq!(r.uuid_to_slug.get(UUID_FRONT), Some(&"front".to_string()));
        assert_eq!(r.uuid_to_slug.get(UUID_BACK), Some(&"back".to_string()));
    }

    #[test]
    fn caps_are_honest() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        let caps = r.supports();
        // No live flow, no polled rain sensor, no wired history: the caps
        // must not advertise what status()/run_history cannot deliver.
        assert!(!caps.flow_meter);
        assert!(!caps.rain_sensor);
        assert!(!caps.history_query);
        // The only stop is device-wide stop_water.
        assert!(!caps.per_zone_stop);
        // Rachio runs zones sequentially, not in parallel.
        assert!(!caps.multi_zone_parallel);
        assert!(caps.master_valve);
    }

    #[test]
    fn poll_interval_default_and_floor() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        assert_eq!(r.poll_interval(), Duration::from_secs(120));
        let r = Rachio::new(
            "rachio",
            RachioConfig {
                poll_interval_s: Some(10),
                ..cfg()
            },
        )
        .unwrap();
        assert_eq!(r.poll_interval(), Duration::from_secs(60), "floor is 60s");
        let r = Rachio::new(
            "rachio",
            RachioConfig {
                poll_interval_s: Some(300),
                ..cfg()
            },
        )
        .unwrap();
        assert_eq!(r.poll_interval(), Duration::from_secs(300));
    }

    #[tokio::test]
    async fn history_query_off_and_run_history_empty() {
        let r = Rachio::new("rachio", cfg()).unwrap();
        assert!(!r.supports().history_query);
        assert!(r.run_history(0).await.unwrap().is_empty());
    }

    // ---- wire tests (wiremock; native-only dev-dependency) ----

    async fn mock_device(server: &MockServer, expect: u64) {
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}")))
            .and(header("authorization", format!("Bearer {TOKEN}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_fixture()))
            .expect(expect)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_zone_puts_zone_start_with_uuid_and_clamped_duration() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/zone/start"))
            .and(body_partial_json(
                json!({ "id": UUID_FRONT, "duration": 10_800 }),
            ))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        // 4 hours clamps to the API's 3 hour cap.
        let h = r.run_zone("front", 14_400).await.unwrap();
        assert_eq!(h.planned_duration_s, 10_800);
        assert_eq!(h.provider_ref.as_deref(), Some(UUID_FRONT));
    }

    #[tokio::test]
    async fn run_zone_429_surfaces_rate_limited_without_retry() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/zone/start"))
            .respond_with(ResponseTemplate::new(429).insert_header("x-ratelimit-remaining", "0"))
            .expect(1) // exactly one attempt: no retry storm
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        assert!(matches!(
            r.run_zone("front", 300).await,
            Err(ControllerError::RateLimited)
        ));
        assert_eq!(r.last_rate_limit_remaining().as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn auth_failure_maps_to_auth_failed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}")))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        assert!(matches!(
            r.discover_zones().await,
            Err(ControllerError::AuthFailed)
        ));
    }

    #[tokio::test]
    async fn status_reads_running_state_from_current_schedule() {
        let server = MockServer::start().await;
        mock_device(&server, 1).await;
        let start_ms = (chrono::Utc::now().timestamp() - 60) * 1000;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "zoneId": UUID_FRONT,
                "zoneStartDate": start_ms,
                "zoneDuration": 600,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        let st = r.status().await.unwrap();
        assert!(st.reachable);
        let front = st.zone_states.iter().find(|z| z.slug == "front").unwrap();
        assert!(front.running, "current_schedule zone must report running");
        assert!(front.running_known);
        let rem = front
            .remaining_s
            .expect("remaining derives from start+duration");
        assert!(
            (500..=540).contains(&rem),
            "about 9 minutes left, got {rem}"
        );
        let back = st.zone_states.iter().find(|z| z.slug == "back").unwrap();
        assert!(!back.running);
        // lastWateredDate is ms on the wire, seconds internally.
        assert_eq!(front.last_run_epoch, Some(1_756_300_000));
        assert_eq!(st.firmware.as_deref(), Some("iro3-firmware-example"));
        assert_eq!(st.master_enabled, Some(true));
    }

    #[tokio::test]
    async fn status_empty_current_schedule_is_idle() {
        let server = MockServer::start().await;
        mock_device(&server, 1).await;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_string(""))
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        let st = r.status().await.unwrap();
        assert!(st.zone_states.iter().all(|z| !z.running && z.running_known));
    }

    #[tokio::test]
    async fn status_schedule_shape_surprise_degrades_to_running_unknown() {
        // Poll 1: front is running (recognized shape). Poll 2: the schedule
        // endpoint answers something unrecognized. The adapter must NOT
        // fabricate idle: it carries the running flag forward and marks
        // running_known=false.
        let server = MockServer::start().await;
        mock_device(&server, 2).await;
        let start_ms = (chrono::Utc::now().timestamp() - 60) * 1000;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "zoneId": UUID_FRONT,
                "zoneStartDate": start_ms,
                "zoneDuration": 600,
            })))
            .expect(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // A 1s poll floor cannot be configured (floor is 60s), so drive the
        // second live poll by clearing the attempt gate directly.
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        let st1 = r.status().await.unwrap();
        assert!(st1
            .zone_states
            .iter()
            .any(|z| z.slug == "front" && z.running));

        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "scheduleId": "s-1", "status": "PROCESSING" })),
            )
            .mount(&server)
            .await;
        *r.last_attempt.lock().await = None; // reopen the throttle window
        let st2 = r.status().await.unwrap();
        let front = st2.zone_states.iter().find(|z| z.slug == "front").unwrap();
        assert!(
            front.running,
            "shape surprise must carry the last known running state forward"
        );
        assert!(!front.running_known, "and mark it unknown");
    }

    #[tokio::test]
    async fn status_throttle_serves_cache_inside_the_poll_window() {
        let server = MockServer::start().await;
        // expect(1): the second status() inside the window must NOT hit the
        // cloud. This is the rate-limit compliance contract.
        mock_device(&server, 1).await;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        let st1 = r.status().await.unwrap();
        let st2 = r.status().await.unwrap();
        assert_eq!(st1.zone_states.len(), st2.zone_states.len());
        server.verify().await;
    }

    #[tokio::test]
    async fn stop_zone_issues_device_wide_stop_water() {
        let server = MockServer::start().await;
        Mock::given(method("PUT"))
            .and(path("/device/stop_water"))
            .and(body_partial_json(json!({ "id": DEVICE })))
            .respond_with(ResponseTemplate::new(204))
            .expect(2) // stop_zone + stop_all are the SAME device-wide call
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        r.stop_zone("front").await.unwrap();
        r.stop_all().await.unwrap();
        // An unmapped slug fails fast BEFORE any device-wide stop fires.
        assert!(matches!(
            r.stop_zone("ghost").await,
            Err(ControllerError::ZoneUnknown(_))
        ));
        server.verify().await;
    }

    #[tokio::test]
    async fn resolve_device_id_walks_person_info_then_person() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/person/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "person-0001" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/person/person-0001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "person-0001",
                "devices": [
                    { "id": DEVICE, "name": "Yard Controller" },
                    { "id": "device-0002", "name": "Second Controller" },
                ],
            })))
            .expect(1)
            .mount(&server)
            .await;
        let r = Rachio::new(
            "rachio",
            RachioConfig {
                device_id: String::new(),
                ..cfg_at(&server.uri())
            },
        )
        .unwrap();
        let found = r.resolve_device_id().await.unwrap();
        assert_eq!(found.device_id, DEVICE);
        assert_eq!(found.name.as_deref(), Some("Yard Controller"));
        assert_eq!(found.device_count, 2);
    }

    #[tokio::test]
    async fn resolve_device_id_with_no_devices_is_a_plain_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/person/info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "person-0001" })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/person/person-0001"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "id": "person-0001", "devices": [] })),
            )
            .mount(&server)
            .await;
        let r = Rachio::new(
            "rachio",
            RachioConfig {
                device_id: String::new(),
                ..cfg_at(&server.uri())
            },
        )
        .unwrap();
        assert!(matches!(
            r.resolve_device_id().await,
            Err(ControllerError::Remote(_))
        ));
    }

    // ---- throttle invalidation on dispatch (run edges must be seen) ----

    #[tokio::test]
    async fn run_zone_reopens_the_throttle_so_the_rising_edge_is_seen() {
        let server = MockServer::start().await;
        // TWO live polls expected: one before dispatch, one right after.
        // Without the invalidation the second status() would serve the
        // cached idle snapshot and a short segment could run unobserved.
        mock_device(&server, 2).await;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/zone/start"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        let st = r.status().await.unwrap();
        assert!(st.zone_states.iter().all(|z| !z.running), "idle before");

        r.run_zone("front", 600).await.unwrap();

        // The zone is now running; the immediate next status() must poll
        // LIVE and see it.
        let start_ms = chrono::Utc::now().timestamp() * 1000;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "zoneId": UUID_FRONT,
                "zoneStartDate": start_ms,
                "zoneDuration": 600,
            })))
            .expect(1)
            .mount(&server)
            .await;
        let st = r.status().await.unwrap();
        let front = st.zone_states.iter().find(|z| z.slug == "front").unwrap();
        assert!(
            front.running,
            "post-dispatch status must be a live poll showing the rising edge"
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn stop_reopens_the_throttle_so_the_banner_clears_promptly() {
        let server = MockServer::start().await;
        // status, stop, status: TWO live polls, not one poll + cache.
        mock_device(&server, 2).await;
        let start_ms = (chrono::Utc::now().timestamp() - 60) * 1000;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "zoneId": UUID_FRONT,
                "zoneStartDate": start_ms,
                "zoneDuration": 600,
            })))
            .expect(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/device/stop_water"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        let st = r.status().await.unwrap();
        assert!(st.zone_states.iter().any(|z| z.running), "running before");

        r.stop_zone("front").await.unwrap();

        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let st = r.status().await.unwrap();
        assert!(
            st.zone_states.iter().all(|z| !z.running),
            "post-stop status must be a live poll showing idle"
        );
        server.verify().await;
    }

    // ---- carry-forward TTL (the eternal-phantom-run bound) ----

    #[test]
    fn carry_forward_ttl_math() {
        // Within two poll intervals of the outage start: keep carrying.
        assert!(!carry_forward_expired(1000 + 239, 1000, 120, None));
        // Beyond it with no dispatched run: degrade.
        assert!(carry_forward_expired(1000 + 241, 1000, 120, None));
        // A dispatched run keeps the carry alive until its planned end +
        // grace, even past the poll-interval bound.
        assert!(!carry_forward_expired(
            1000 + 500,
            1000,
            120,
            Some(1000 + 700)
        ));
        // Past both bounds: degrade.
        assert!(carry_forward_expired(
            1000 + 701,
            1000,
            120,
            Some(1000 + 700)
        ));
    }

    #[tokio::test]
    async fn expired_carry_forward_degrades_to_not_running_still_unknown() {
        // Poll 1: front running (recognized). Then the schedule endpoint
        // breaks permanently; once the outage outlives the TTL the carried
        // "running" must degrade to not-running while staying UNKNOWN.
        let server = MockServer::start().await;
        mock_device(&server, 2).await;
        let start_ms = (chrono::Utc::now().timestamp() - 60) * 1000;
        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "zoneId": UUID_FRONT,
                "zoneStartDate": start_ms,
                "zoneDuration": 600,
            })))
            .expect(1)
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let r = Rachio::new("rachio", cfg_at(&server.uri())).unwrap();
        let st1 = r.status().await.unwrap();
        assert!(st1
            .zone_states
            .iter()
            .any(|z| z.slug == "front" && z.running));

        Mock::given(method("GET"))
            .and(path(format!("/device/{DEVICE}/current_schedule")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "scheduleId": "s-1", "status": "PROCESSING" })),
            )
            .mount(&server)
            .await;
        // Simulate an outage that started long ago (past 2x poll interval,
        // no dispatched run keeping it alive), then force a live poll.
        *r.unknown_since.lock().await = Some(chrono::Utc::now().timestamp() - 10_000);
        *r.last_attempt.lock().await = None;
        let st2 = r.status().await.unwrap();
        let front = st2.zone_states.iter().find(|z| z.slug == "front").unwrap();
        assert!(
            !front.running,
            "expired carry-forward must not keep reporting watering"
        );
        assert!(!front.running_known, "and the state stays marked unknown");
    }
}
