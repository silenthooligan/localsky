// /api/wizard router. Drives the first-run setup flow.
//
// Endpoints:
//   GET    /api/wizard/draft           -> current draft (or default)
//   PUT    /api/wizard/draft           -> save draft
//   DELETE /api/wizard/draft           -> clear draft (cancel + restart)
//   POST   /api/wizard/apply           -> validate + write /data/localsky.toml
//   POST   /api/wizard/test_source     -> dispatch through source adapter (Phase 6)
//   POST   /api/wizard/test_controller -> dispatch through controller adapter (Phase 5)
//   POST   /api/wizard/scan_zones      -> mDNS + controller probe (Phase 5)
//   GET    /api/wizard/geocode?q=...   -> server-side Nominatim proxy
//
// Wizard endpoints are only mounted when /data/localsky.toml does not yet
// exist; the setup-gate middleware redirects normal routes to /setup.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{delete, get, post},
    Router,
};
use serde::{Deserialize, Serialize};

use crate::config::wizard::{WizardDraft, WizardError, WizardStore};
use crate::config::FileConfigStore;
use crate::ports::config_store::ConfigStore;

#[derive(Clone)]
pub struct WizardApiState {
    pub draft_store: Arc<WizardStore>,
    pub config_store: Arc<FileConfigStore>,
    /// Present when the identity store booted; lets apply persist
    /// auth.mode = required when the wizard created an owner account.
    pub auth_rt: Option<Arc<crate::auth::AuthRuntime>>,
    /// Live Tempest store: passive discovery (a broadcasting hub shows
    /// up here without any probe).
    pub tempest_store: Option<Arc<crate::tempest::state::TempestStore>>,
}

pub fn router(state: WizardApiState) -> Router {
    Router::new()
        .route("/draft", get(get_draft).put(put_draft))
        .route("/draft", delete(delete_draft))
        .route("/apply", post(post_apply))
        .route("/test_source", post(post_test_source))
        .route("/test_controller", post(post_test_controller))
        .route("/test_llm", post(post_test_llm))
        .route("/scan_zones", post(post_scan_zones))
        .route("/probe_soil", post(post_probe_soil))
        .route("/discover", get(get_discover))
        .route("/state", get(get_state))
        .route("/seed_current", post(post_seed_current))
        .route("/geocode", get(get_geocode))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
    detail: Option<String>,
}

fn wizard_err(e: WizardError) -> (StatusCode, Json<ApiError>) {
    let code = match &e {
        WizardError::NotPresent => StatusCode::NOT_FOUND,
        WizardError::LicenseNotAccepted | WizardError::Validation(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        code,
        Json(ApiError {
            error: "wizard_error".into(),
            detail: Some(e.to_string()),
        }),
    )
}

async fn get_draft(State(s): State<WizardApiState>) -> impl IntoResponse {
    let store = s.draft_store.clone();
    let res = tokio::task::spawn_blocking(move || store.load()).await;
    match res {
        Ok(Ok(d)) => Json(d).into_response(),
        Ok(Err(WizardError::NotPresent)) => Json(WizardDraft::default()).into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

async fn put_draft(
    State(s): State<WizardApiState>,
    Json(draft): Json<WizardDraft>,
) -> impl IntoResponse {
    let store = s.draft_store.clone();
    let res = tokio::task::spawn_blocking(move || store.save(&draft)).await;
    match res {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

async fn delete_draft(State(s): State<WizardApiState>) -> impl IntoResponse {
    let store = s.draft_store.clone();
    let res = tokio::task::spawn_blocking(move || store.clear()).await;
    match res {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

async fn post_apply(State(s): State<WizardApiState>) -> impl IntoResponse {
    let draft_store = s.draft_store.clone();
    let load_res = tokio::task::spawn_blocking(move || draft_store.load()).await;
    let mut draft = match load_res {
        Ok(Ok(d)) => d,
        Ok(Err(e)) => return wizard_err(e).into_response(),
        Err(e) => return wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    };
    // Pre-apply checks, then fill the defaults the wizard promises for
    // skipped steps (sources) before writing.
    if let Err(e) = s.draft_store.validate_for_apply(&draft) {
        return wizard_err(e).into_response();
    }
    WizardStore::finalize_for_apply(&mut draft);
    // The Account step creates the owner directly in SQLite; reflect it
    // in the written policy so login is required from first boot.
    if let Some(rt) = &s.auth_rt {
        if rt.setup_complete.load(std::sync::atomic::Ordering::Relaxed) {
            draft.config.auth.mode = crate::config::schema::AuthMode::Required;
        }
    }
    // Write the config atomically.
    match s.config_store.save(&draft.config).await {
        Ok(v) => {
            // Drop the draft. Best-effort; if cleanup fails the next boot
            // can still resume from the draft, which is harmless.
            let ds = s.draft_store.clone();
            let _ = tokio::task::spawn_blocking(move || ds.clear()).await;
            Json(v).into_response()
        }
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "config_save_failed".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

// ---- Adapter test endpoints. Real impls land alongside Phase 5/6. ----

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TestSourceBody {
    pub source: crate::config::schema::SourceEntry,
}

async fn post_test_source(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestSourceBody>,
) -> impl IntoResponse {
    // The config deserialized into a typed SourceEntry, so it's structurally
    // valid. Receiver sources (Ecowitt LAN, webhook) confirm by live readings
    // on the Sensors hub once the device posts; polled sources confirm within
    // one cycle after apply. A live probe per kind is a follow-up.
    serde_json::json!({
        "ok": true,
        "id": body.source.id,
        "note": "config valid; confirm live readings on the Sensors hub after applying",
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
struct TestControllerBody {
    pub controller: crate::config::schema::ControllerEntry,
}

async fn post_test_controller(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestControllerBody>,
) -> impl IntoResponse {
    match crate::runtime::build_test_controller(&body.controller) {
        Ok(c) => match c.status().await {
            Ok(st) => Json(serde_json::json!({
                "ok": true,
                "reachable": st.reachable,
                "master_enabled": st.master_enabled,
                "water_level_pct": st.water_level_pct,
                "zone_count": st.zone_states.len(),
                "firmware": st.firmware,
            }))
            .into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "controller_unreachable".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "controller_unsupported".into(),
                detail: Some(e),
            }),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct TestLlmBody {
    pub llm: crate::config::schema::LlmConfig,
}

async fn post_test_llm(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestLlmBody>,
) -> impl IntoResponse {
    let provider = match crate::runtime::build_llm_from(&body.llm).await {
        Some(p) => p,
        None => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "llm_unreachable".into(),
                    detail: Some(
                        "no provider responded; for Auto, make sure Ollama / llama.cpp / LM Studio is running on this host".into(),
                    ),
                }),
            )
                .into_response()
        }
    };
    match provider.health().await {
        Ok(h) => Json(serde_json::json!({
            "ok": h.reachable,
            "provider": provider.id(),
            "model_loaded": h.model_loaded,
            "provider_version": h.provider_version,
            "detail": h.last_error,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "llm_unreachable".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

async fn post_scan_zones(
    State(_s): State<WizardApiState>,
    Json(body): Json<TestControllerBody>,
) -> impl IntoResponse {
    match crate::runtime::build_test_controller(&body.controller) {
        Ok(c) => match c.discover_zones().await {
            Ok(zones) => Json(serde_json::json!({ "zones": zones })).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "zone_scan_failed".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "controller_unsupported".into(),
                detail: Some(e),
            }),
        )
            .into_response(),
    }
}

// ---- Live soil enumeration for the Sensors step. ----
//
// The wizard draft is NOT applied while the wizard runs, so no
// `ecowitt_gw_poll` source is actually polling and `/api/v1/sensors/inventory`
// lists nothing for a gateway the user just added in the Weather step. To
// still show real probes, this endpoint queries the gateway's local API
// directly over the LAN (`GET http://<host>/get_livedata_info`) and reuses the
// exact same parser the live poller uses, so the channel ids it returns
// (`source:<src>:soilmoisture<N>`) are byte-identical to what the engine
// resolves post-apply.

#[derive(Debug, Deserialize)]
struct ProbeSoilBody {
    /// Gateway IP or hostname on the LAN (from the draft source's config.host).
    host: String,
    /// Draft source id, used to build canonical `source:<id>:soilmoisture<N>`
    /// channel ids that match the eventual live binding. Defaults to
    /// "ecowitt_gw" (the id the discovery "Add" button uses).
    #[serde(default = "default_probe_source_id")]
    source_id: String,
}

fn default_probe_source_id() -> String {
    "ecowitt_gw".to_string()
}

#[derive(Debug, Serialize)]
struct ProbeChannel {
    /// Channel number as the gateway reports it ("1".."N").
    channel: String,
    /// Canonical engine address, identical to a live `soil_sensor_id`:
    /// `source:<source_id>:soilmoisture<channel>`.
    id: String,
    /// Live moisture %, when the gateway reported it.
    moisture_pct: Option<f64>,
    /// Soil battery %, when present (already scaled 0..100 by the parser).
    battery_pct: Option<f64>,
    /// Soil temperature F, when present.
    temp_f: Option<f64>,
    /// Soil EC, when present.
    ec: Option<f64>,
}

/// POST /api/wizard/probe_soil { host, source_id? } -> the gateway's current
/// soil channels read live off its local API. User-initiated (the Sensors
/// step calls it per configured gateway). A few seconds at worst.
async fn post_probe_soil(
    State(_s): State<WizardApiState>,
    Json(body): Json<ProbeSoilBody>,
) -> impl IntoResponse {
    let host = body.host.trim();
    if host.is_empty() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "missing_host".into(),
                detail: Some("gateway host is required".into()),
            }),
        )
            .into_response();
    }
    let url = format!("http://{host}/get_livedata_info");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ApiError {
                    error: "client_build_failed".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response()
        }
    };
    let body_json = match client
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.json::<serde_json::Value>().await {
            Ok(v) => v,
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ApiError {
                        error: "gateway_parse_error".into(),
                        detail: Some(e.to_string()),
                    }),
                )
                    .into_response()
            }
        },
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "gateway_unreachable".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response()
        }
    };

    let channels = soil_channels_from_livedata(&body_json, &body.source_id);
    Json(serde_json::json!({ "ok": true, "channels": channels })).into_response()
}

/// Reduce a `/get_livedata_info` body to one ProbeChannel per soil-moisture
/// channel, gathering the battery/temp/EC siblings the parser emits under
/// `soil{batt,temp,ec}<N>`. Reuses `ecowitt_gw_poll::parse_livedata` so the
/// keys (and therefore the channel ids) match the live poller exactly.
fn soil_channels_from_livedata(body: &serde_json::Value, source_id: &str) -> Vec<ProbeChannel> {
    use std::collections::BTreeMap;
    // epoch is irrelevant here (we only read keys/values), pass 0.
    let readings = crate::sources::ecowitt_gw_poll::parse_livedata(body, source_id, 0);

    // Collect by channel number parsed off the key suffix.
    let mut by_ch: BTreeMap<String, ProbeChannel> = BTreeMap::new();
    let ensure = |map: &mut BTreeMap<String, ProbeChannel>, ch: &str| {
        map.entry(ch.to_string()).or_insert_with(|| ProbeChannel {
            channel: ch.to_string(),
            id: format!("source:{source_id}:soilmoisture{ch}"),
            moisture_pct: None,
            battery_pct: None,
            temp_f: None,
            ec: None,
        });
    };

    for r in &readings {
        if let Some(ch) = r.key.strip_prefix("soilmoisture") {
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().moisture_pct = Some(r.value);
        } else if let Some(ch) = r.key.strip_prefix("soilbatt") {
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().battery_pct = Some(r.value);
        } else if let Some(rest) = r.key.strip_prefix("soiltemp") {
            // key shape is soiltemp<N>f.
            let ch = rest.trim_end_matches('f');
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().temp_f = Some(r.value);
        } else if let Some(ch) = r.key.strip_prefix("soilec") {
            ensure(&mut by_ch, ch);
            by_ch.get_mut(ch).unwrap().ec = Some(r.value);
        }
    }

    // Only surface channels that actually reported a moisture reading (a bare
    // battery/temp sibling without moisture is not a usable probe to bind).
    by_ch
        .into_values()
        .filter(|c| c.moisture_pct.is_some())
        .collect()
}

// ---- Re-entry support: state probe + seed-from-current. ----

/// GET /api/wizard/state -> { config_present, draft_present }. The setup
/// shell uses it to offer "modify current setup" vs "start fresh" when
/// the wizard is re-entered on an already-configured instance.
async fn get_state(State(s): State<WizardApiState>) -> impl IntoResponse {
    let draft_present = s.draft_store.exists();
    let config_present = s.config_store.is_initialized();
    Json(serde_json::json!({
        "config_present": config_present,
        "draft_present": draft_present,
    }))
}

/// POST /api/wizard/seed_current -> start a draft pre-filled from the
/// live config (license already accepted on the original run), so the
/// wizard becomes an editor over the existing setup instead of a wipe.
async fn post_seed_current(State(s): State<WizardApiState>) -> impl IntoResponse {
    let cfg = match s.config_store.load().await {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::CONFLICT,
                Json(ApiError {
                    error: "no_config".into(),
                    detail: Some(format!("nothing to modify: {e}")),
                }),
            )
                .into_response()
        }
    };
    let draft = WizardDraft {
        current_step: crate::config::wizard::WizardStep::Location,
        config: cfg,
        license_accepted: true,
        telemetry_choice: Some(false),
        last_updated_epoch: chrono::Utc::now().timestamp(),
    };
    let store = s.draft_store.clone();
    match tokio::task::spawn_blocking(move || store.save(&draft)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => wizard_err(e).into_response(),
        Err(e) => wizard_err(WizardError::Io(format!("join: {e}"))).into_response(),
    }
}

// ---- Network discovery. One aggregated sweep for the wizard. ----

/// GET /api/wizard/discover -> everything findable on the LAN right now:
/// passive Tempest (any hub already broadcasting on UDP 50222), Ecowitt
/// gateways (UDP broadcast probe), OpenSprinkler controllers (HTTP /24
/// sweep). User-initiated only; total wall time a few seconds.
async fn get_discover(State(s): State<WizardApiState>) -> impl IntoResponse {
    let tempest = s.tempest_store.as_ref().map(|store| {
        let snap = store.snapshot();
        let now = chrono::Utc::now().timestamp();
        let fresh = snap.last_packet_epoch > 0 && now - snap.last_packet_epoch < 300;
        serde_json::json!({
            "detected": fresh,
            "hub_serial": if snap.hub_serial.is_empty() { serde_json::Value::Null } else { snap.hub_serial.clone().into() },
            "last_seen_epoch": snap.last_packet_epoch,
        })
    });

    let (ecowitt, opensprinkler) = tokio::join!(
        crate::discovery::ecowitt::discover_ecowitt(std::time::Duration::from_secs(3)),
        crate::discovery::opensprinkler::discover_opensprinkler(std::time::Duration::from_millis(
            1500
        )),
    );

    Json(serde_json::json!({
        "tempest": tempest,
        "ecowitt": ecowitt,
        "opensprinkler": opensprinkler,
    }))
}

// ---- Geocode proxy. Lets the location step do address -> lat/lon. ----

#[derive(Debug, Deserialize)]
struct GeocodeQuery {
    q: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeocodeResult {
    display_name: String,
    lat: String,
    lon: String,
}

async fn get_geocode(Query(q): Query<GeocodeQuery>) -> impl IntoResponse {
    let url = format!(
        "https://nominatim.openstreetmap.org/search?q={}&format=json&limit=5",
        urlencode(&q.q)
    );
    let client = reqwest::Client::new();
    let res = client
        .get(&url)
        // Nominatim ToS requires a meaningful User-Agent identifying the
        // operator. The deployment name + URL is reasonable; users can
        // override via wizard customization.
        .header("User-Agent", "LocalSky setup wizard")
        .send()
        .await;
    match res {
        Ok(r) => match r.json::<Vec<GeocodeResult>>().await {
            Ok(results) => Json(results).into_response(),
            Err(e) => (
                StatusCode::BAD_GATEWAY,
                Json(ApiError {
                    error: "geocode_parse_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(ApiError {
                error: "geocode_transport_error".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

fn urlencode(s: &str) -> String {
    // Lightweight encoder for query string values. Nominatim accepts most
    // punctuation as-is so we only escape the obvious offenders.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b' ' => out.push('+'),
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod probe_soil_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reduces_livedata_to_bindable_channels() {
        // A real EC-soil gateway shape: two probes with moisture + siblings.
        let body = json!({
            "ch_ec": [
                {"channel": "1", "name": "Back Yard", "battery": "5", "humidity": "32%", "temp": "79.2", "unit": "F", "ec": "40 uS/cm"},
                {"channel": "3", "name": "Side Yard", "battery": "4", "humidity": "65%", "temp": "82.0", "unit": "F", "ec": "170 uS/cm"}
            ]
        });
        let mut chans = soil_channels_from_livedata(&body, "ecowitt_gw");
        chans.sort_by(|a, b| a.channel.cmp(&b.channel));
        assert_eq!(chans.len(), 2);

        let c1 = &chans[0];
        assert_eq!(c1.channel, "1");
        // The id must be byte-identical to a live `soil_sensor_id` binding.
        assert_eq!(c1.id, "source:ecowitt_gw:soilmoisture1");
        assert_eq!(c1.moisture_pct, Some(32.0));
        // Ecowitt battery level 5 -> 100% (the parser scales x20, clamped).
        assert_eq!(c1.battery_pct, Some(100.0));
        assert_eq!(c1.temp_f, Some(79.2));
        assert_eq!(c1.ec, Some(40.0));

        let c3 = &chans[1];
        assert_eq!(c3.channel, "3");
        assert_eq!(c3.id, "source:ecowitt_gw:soilmoisture3");
        assert_eq!(c3.moisture_pct, Some(65.0));
    }

    #[test]
    fn ignores_non_soil_and_uses_source_id() {
        // Classic WH51 ch_soil plus weather noise that must not surface.
        let body = json!({
            "common_list": [{"id": "0x02", "val": "71.6"}],
            "ch_soil": [{"channel": "2", "name": "Front", "battery": "3", "humidity": "45%"}]
        });
        let chans = soil_channels_from_livedata(&body, "my_gw");
        assert_eq!(chans.len(), 1, "only the soil channel is bindable");
        assert_eq!(chans[0].id, "source:my_gw:soilmoisture2");
        assert_eq!(chans[0].moisture_pct, Some(45.0));
        // ch_soil carries no temp/EC.
        assert_eq!(chans[0].temp_f, None);
        assert_eq!(chans[0].ec, None);
    }

    #[test]
    fn empty_body_has_no_channels() {
        assert!(soil_channels_from_livedata(&json!({}), "gw").is_empty());
    }
}
