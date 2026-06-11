// /api/health endpoint. Liveness + readiness probe with structured
// per-subsystem and per-source status. The health endpoint is always
// reachable, even when the engine is degraded; orchestrators (Docker
// healthcheck, Kubernetes probes, uptime-kuma) hit it to decide
// restart policy.

use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::{extract::State, response::Json};
use serde::Serialize;

use crate::config::schema::SourceKind;
use crate::config::FileConfigStore;
use crate::forecast::ForecastStore;
use crate::ha::IrrigationStore;
use crate::persistence::SensorHistoryStore;
use crate::ports::config_store::ConfigStore;
use crate::tempest::state::TempestStore;

static STARTED_AT: OnceLock<Instant> = OnceLock::new();

fn started_at() -> Instant {
    *STARTED_AT.get_or_init(Instant::now)
}

#[derive(Clone)]
pub struct HealthState {
    pub config_store: Option<Arc<FileConfigStore>>,
    /// When set, /api/health enumerates sources from the loaded config
    /// and reports per-source freshness (seconds since last observation).
    /// Used as a fallback for kinds without an in-memory store (MQTT,
    /// HTTP webhook, Ecowitt local POST receiver).
    pub sensor_history: Option<SensorHistoryStore>,
    /// Live freshness sources for the two legacy v0.1 paths that do not
    /// publish on the source bus: TempestUdp feeds TempestStore via the
    /// UDP listener and OpenMeteo feeds ForecastStore via the refresher.
    /// Every other kind reports freshness from data it actually produced
    /// (the bus recorder's last-seen map + sensor_history rows).
    pub tempest_store: Option<Arc<TempestStore>>,
    pub forecast_store: Option<Arc<ForecastStore>>,
    pub irrigation_store: Option<Arc<IrrigationStore>>,
    /// In-memory per-source last-observation map fed by the bus
    /// recorder (this boot only; sensor_history covers across restarts).
    pub source_last_seen: Option<crate::sources::SourceLastSeen>,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub config_present: bool,
    pub version: &'static str,
    pub schema_version: Option<u32>,
    pub uptime_s: u64,
    pub subsystems: SubsystemReport,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceFreshness>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub controllers: Vec<ControllerSummary>,
    /// The Home Assistant relationship, both directions. None for
    /// anonymous callers on auth-required instances (same trimming as
    /// sources/controllers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ha: Option<HaIntegration>,
}

#[derive(Debug, Serialize)]
pub struct HaIntegration {
    /// HA_URL env present (the inbound snapshot bridge is configured).
    pub env_configured: bool,
    /// Last poll of HA succeeded (from the irrigation snapshot).
    pub reachable: bool,
    /// Where the irrigation snapshot comes from: "home_assistant" |
    /// "standalone" (deployment.mode resolved against the env).
    pub snapshot_source: &'static str,
    /// ha_passthrough sources: (id, mapped-field count). A zero count
    /// means the source feeds nothing.
    pub passthrough_sources: Vec<(String, usize)>,
    /// Controllers actuated through HA service calls.
    pub service_call_controllers: Vec<String>,
    /// Outbound: MQTT discovery publishing enabled.
    pub mqtt_discovery: bool,
    /// Outbound: epoch of the HA integration's last contact (manifest
    /// fetch at load, or live-stream connect). 0 = never this boot.
    pub hacs_last_seen_epoch: i64,
    /// Outbound: the integration is holding a live SSE stream right now.
    pub hacs_streaming: bool,
}

#[derive(Debug, Serialize)]
pub struct SubsystemReport {
    pub config_store: &'static str,
    pub persistence: &'static str,
}

#[derive(Debug, Serialize)]
pub struct SourceFreshness {
    pub id: String,
    pub kind: &'static str,
    pub enabled: bool,
    pub last_seen_epoch: Option<i64>,
    pub stale_for_s: Option<i64>,
    /// "fresh" (<5 min), "stale" (5 min .. 1 hr), "offline" (>1 hr or never).
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct ControllerSummary {
    pub id: String,
    pub kind: &'static str,
    pub default: bool,
    pub enabled: bool,
}

pub async fn health(
    State(state): State<HealthState>,
    req: axum::http::Request<axum::body::Body>,
) -> Json<HealthResponse> {
    // Anonymous callers on an auth-required instance get liveness only:
    // status/version/uptime, no per-source detail (Docker healthchecks
    // and uptime monitors keep working without leaking topology).
    let full_detail = {
        use crate::auth::middleware::{AuthRequired, RequestIdentity};
        let required = req
            .extensions()
            .get::<AuthRequired>()
            .map(|a| a.0)
            .unwrap_or(false);
        let identified = matches!(
            req.extensions().get::<RequestIdentity>(),
            Some(RequestIdentity::User(_)) | Some(RequestIdentity::TrustedNetwork)
        );
        !required || identified
    };
    let uptime_s = started_at().elapsed().as_secs();
    let mut config_present = false;
    let mut schema_version = None;
    let mut config_status = "missing";
    let mut sources_freshness: Vec<SourceFreshness> = Vec::new();
    let mut controller_summaries: Vec<ControllerSummary> = Vec::new();

    if let Some(store) = &state.config_store {
        if store.is_initialized() {
            config_present = true;
            match store.load().await {
                Ok(cfg) => {
                    schema_version = Some(cfg.schema_version);
                    config_status = "ok";

                    // Per-source freshness comes from data the source
                    // actually produced: TempestUdp from the UDP
                    // listener's store, OpenMeteo from the forecast
                    // refresher's store (the two legacy v0.1 paths), and
                    // every bus-publishing kind from the recorder's
                    // last-seen map plus its sensor_history rows
                    // (history survives restarts; the map is this boot).
                    let source_ids: Vec<String> =
                        cfg.sources.iter().map(|s| s.id.clone()).collect();
                    let last_seen = if let Some(hist) = &state.sensor_history {
                        hist.last_seen_per_source(source_ids.clone())
                            .await
                            .unwrap_or_default()
                    } else {
                        std::collections::HashMap::new()
                    };
                    let tempest_last = state
                        .tempest_store
                        .as_ref()
                        .map(|s| s.snapshot().last_packet_epoch)
                        .filter(|e| *e > 0);
                    let forecast_last = state
                        .forecast_store
                        .as_ref()
                        .map(|s| s.snapshot().last_refresh_epoch)
                        .filter(|e| *e > 0);
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    for entry in &cfg.sources {
                        let last_seen_epoch = match &entry.source {
                            SourceKind::TempestUdp(_) => tempest_last,
                            SourceKind::OpenMeteo(_) => forecast_last,
                            _ => {
                                let bus = state
                                    .source_last_seen
                                    .as_ref()
                                    .and_then(|m| m.get(&entry.id));
                                let hist = last_seen.get(&entry.id).copied();
                                match (bus, hist) {
                                    (Some(a), Some(b)) => Some(a.max(b)),
                                    (a, b) => a.or(b),
                                }
                            }
                        };
                        let stale_for_s = last_seen_epoch.map(|e| (now - e).max(0));
                        // Freshness windows are kind-aware: polled forecast
                        // models refresh on a 10-30 min cadence, so the
                        // generic 5-minute window false-flagged them "stale"
                        // between perfectly healthy polls. LaCrosse polls
                        // every 5 min, so it gets a mid window.
                        let (fresh_s, offline_s) = match &entry.source {
                            SourceKind::OpenMeteo(_)
                            | SourceKind::Nws(_)
                            | SourceKind::OpenWeather(_)
                            | SourceKind::PirateWeather(_)
                            | SourceKind::MetNorway(_)
                            | SourceKind::Netatmo(_) => (3900, 10800),
                            SourceKind::Lacrosse(_) => (900, 3600),
                            _ => (300, 3600),
                        };
                        let status = match stale_for_s {
                            None => "offline",
                            Some(s) if s < fresh_s => "fresh",
                            Some(s) if s < offline_s => "stale",
                            _ => "offline",
                        };
                        // Operator-configured max_age_s caps the status:
                        // an observation older than the cap can never
                        // report "fresh", even inside the kind window.
                        let status = match (entry.max_age_s, stale_for_s) {
                            (Some(max), Some(s)) if s > max as i64 && status == "fresh" => "stale",
                            _ => status,
                        };
                        sources_freshness.push(SourceFreshness {
                            id: entry.id.clone(),
                            kind: source_kind_label(&entry.source),
                            enabled: entry.enabled,
                            last_seen_epoch,
                            stale_for_s,
                            status,
                        });
                    }

                    for entry in &cfg.controllers {
                        controller_summaries.push(ControllerSummary {
                            id: entry.id.clone(),
                            kind: controller_kind_label(&entry.controller),
                            default: entry.default,
                            enabled: entry.enabled,
                        });
                    }
                }
                Err(_) => {
                    config_status = "error";
                }
            }
        }
    }

    // Home Assistant relationship summary (both directions).
    let ha = {
        let env_configured = std::env::var("HA_URL").is_ok();
        let reachable = state
            .irrigation_store
            .as_ref()
            .map(|s| s.snapshot().ha_reachable)
            .unwrap_or(false);
        let mut passthrough_sources = Vec::new();
        let mut service_call_controllers = Vec::new();
        let mut mqtt_discovery = false;
        let mut mode_standalone = false;
        if let Some(store) = &state.config_store {
            if let Ok(cfg) = store.load().await {
                for e in &cfg.sources {
                    if let SourceKind::HaPassthrough(c) = &e.source {
                        passthrough_sources.push((e.id.clone(), c.field_map.len()));
                    }
                }
                for c in &cfg.controllers {
                    if matches!(
                        c.controller,
                        crate::config::schema::ControllerKind::HaServiceCall(_)
                    ) {
                        service_call_controllers.push(c.id.clone());
                    }
                }
                mqtt_discovery = cfg
                    .notifications
                    .mqtt
                    .as_ref()
                    .map(|m| m.publish_enabled)
                    .unwrap_or(false);
                mode_standalone = matches!(
                    cfg.deployment.mode,
                    crate::config::schema::DeploymentMode::Standalone
                ) || (matches!(
                    cfg.deployment.mode,
                    crate::config::schema::DeploymentMode::Auto
                ) && !env_configured);
            }
        }
        Some(HaIntegration {
            env_configured,
            reachable,
            snapshot_source: if mode_standalone {
                "standalone"
            } else {
                "home_assistant"
            },
            passthrough_sources,
            service_call_controllers,
            mqtt_discovery,
            hacs_last_seen_epoch: crate::api::manifest::LAST_MANIFEST_FETCH_EPOCH
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(
                    crate::api::irrigation::LAST_INTEGRATION_STREAM_EPOCH
                        .load(std::sync::atomic::Ordering::Relaxed),
                ),
            hacs_streaming: crate::api::irrigation::INTEGRATION_STREAMS
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0,
        })
    };

    let status = match (config_present, config_status) {
        (true, "ok") => {
            let any_offline = sources_freshness
                .iter()
                .any(|s| s.enabled && s.status == "offline");
            if any_offline {
                "degraded"
            } else {
                "ok"
            }
        }
        (false, _) => "wizard",
        (_, _) => "degraded",
    };

    let mut ha = ha;
    if !full_detail {
        sources_freshness.clear();
        controller_summaries.clear();
        schema_version = None;
        ha = None;
    }

    Json(HealthResponse {
        status,
        config_present,
        version: env!("CARGO_PKG_VERSION"),
        schema_version,
        uptime_s,
        subsystems: SubsystemReport {
            config_store: config_status,
            persistence: "ok",
        },
        sources: sources_freshness,
        controllers: controller_summaries,
        ha,
    })
}

fn source_kind_label(kind: &crate::config::schema::SourceKind) -> &'static str {
    use crate::config::schema::SourceKind::*;
    match kind {
        TempestUdp(_) => "tempest_udp",
        TempestWs(_) => "tempest_ws",
        OpenMeteo(_) => "open_meteo",
        EcowittLocal(_) => "ecowitt_local",
        EcowittGwPoll(_) => "ecowitt_gw_poll",
        Nws(_) => "nws",
        OpenWeather(_) => "openweather",
        PirateWeather(_) => "pirate_weather",
        MetNorway(_) => "met_norway",
        AmbientWeather(_) => "ambient_weather",
        Netatmo(_) => "netatmo",
        Yolink(_) => "yolink",
        Lacrosse(_) => "lacrosse",
        TuyaCloud(_) => "tuya_cloud",
        DavisWll(_) => "davis_wll",
        HaPassthrough(_) => "ha_passthrough",
        Mqtt(_) => "mqtt",
        HttpWebhook(_) => "http_webhook",
        DemoReplay(_) => "demo_replay",
    }
}

fn controller_kind_label(kind: &crate::config::schema::ControllerKind) -> &'static str {
    use crate::config::schema::ControllerKind::*;
    match kind {
        OpensprinklerDirect(_) => "opensprinkler_direct",
        HaServiceCall(_) => "ha_service_call",
        EsphomeNative(_) => "esphome_native",
        Rachio(_) => "rachio",
        Hydrawise(_) => "hydrawise",
        Bhyve(_) => "bhyve",
        Rainbird(_) => "rainbird",
        MqttCommand(_) => "mqtt_command",
        DryRun(_) => "dry_run",
    }
}
