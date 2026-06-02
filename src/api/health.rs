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
    /// Live freshness sources for the kinds that don't go through
    /// sensor_history. The /api/health logic dispatches on SourceKind
    /// to pick the matching store rather than universally querying
    /// sensor_history (which only has receiver-POST data).
    pub tempest_store: Option<Arc<TempestStore>>,
    pub forecast_store: Option<Arc<ForecastStore>>,
    pub irrigation_store: Option<Arc<IrrigationStore>>,
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

pub async fn health(State(state): State<HealthState>) -> Json<HealthResponse> {
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

                    // sensor_history only carries rows from receiver-POST
                    // sources (MQTT subscribe, Ecowitt local, HTTP
                    // webhook). For TempestUdp / TempestWs / OpenMeteo /
                    // HaPassthrough we read freshness from the matching
                    // in-memory store, which is updated every refresh
                    // cycle regardless of whether sensor_history sees it.
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
                    let irrigation_last = state
                        .irrigation_store
                        .as_ref()
                        .map(|s| s.snapshot().last_refresh_epoch)
                        .filter(|e| *e > 0);
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    for entry in &cfg.sources {
                        let last_seen_epoch = match &entry.source {
                            SourceKind::TempestUdp(_)
                            | SourceKind::TempestWs(_)
                            | SourceKind::DavisWll(_)
                            | SourceKind::Yolink(_)
                            | SourceKind::Lacrosse(_)
                            | SourceKind::TuyaCloud(_) => tempest_last,
                            SourceKind::OpenMeteo(_)
                            | SourceKind::Nws(_)
                            | SourceKind::OpenWeather(_)
                            | SourceKind::PirateWeather(_)
                            | SourceKind::MetNorway(_)
                            | SourceKind::Netatmo(_) => forecast_last,
                            SourceKind::HaPassthrough(_) => irrigation_last,
                            // Receiver-POST kinds + everything else falls
                            // back to sensor_history, which holds their rows.
                            _ => last_seen.get(&entry.id).copied(),
                        };
                        let stale_for_s = last_seen_epoch.map(|e| (now - e).max(0));
                        let status = match stale_for_s {
                            None => "offline",
                            Some(s) if s < 300 => "fresh",
                            Some(s) if s < 3600 => "stale",
                            _ => "offline",
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
