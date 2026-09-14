// Boot phase 7: the HTTP surface.
//
// Every router is built once from typed state and mounted at both
// `/api` (legacy) and `/api/v1` (canonical) by `mount_both`; the HTTP
// state each handler reads is what the earlier phases returned. The
// layers go on outermost-last: cache policy and security headers, then
// auth (which must see pages, APIs and the static fallback alike), then
// the demo read-only gate, then compression.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::http::{header, HeaderName, HeaderValue};
use axum::routing::get;
use axum::Router;
use leptos::prelude::*;
use leptos_axum::{generate_route_list, LeptosRoutes};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::api;
use crate::app::{shell, App};
use crate::config::wizard::WizardStore;

use super::config::BootConfig;
use super::control::Control;
use super::logging::Logging;
use super::sources::Sources;
use super::storage::Storage;
use super::stores::Stores;

/// The finished app and where it listens.
pub struct Served {
    pub app: Router,
    pub addr: SocketAddr,
    pub announcement: Option<crate::mdns::Announcement>,
}

/// Mount one router at both API prefixes. New clients (the HACS
/// integration, third-party automations) target `/api/v1`; the bare
/// `/api/*` aliases stay until a major release cuts them so the in-app
/// radar.js and the v0.1 push subscribers keep working across upgrade.
fn mount_both(app: Router, path: &str, router: Router) -> Router {
    app.nest(&format!("/api{path}"), router.clone())
        .nest(&format!("/api/v1{path}"), router)
}

pub fn build(
    storage: &Storage,
    config: &BootConfig,
    stores: &Stores,
    control: &Control,
    sources: &Sources,
    logging: Logging,
) -> anyhow::Result<Served> {
    use anyhow::Context;
    let conf = get_configuration(None).context(
        "read Leptos configuration (check Cargo.toml [package.metadata.leptos] and LEPTOS_* env vars)",
    )?;
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;
    // The static-site root the fallback resolves against; the bundled
    // docs live under <site_root>/docs.
    let site_root = leptos_options.site_root.to_string();
    let routes = generate_route_list(App);

    let cfg_store = config.store.clone();
    let history = storage.history_conn.clone();
    let cfg = config.cfg.as_ref();

    // Device topology, derived from the configured sources and
    // controllers; rebuilt on config hot-reload. HA's own devices are
    // imported on a background loop (a no-op when HA is not configured).
    let device_registry = crate::devices::DeviceRegistry::new();
    if let Some(cfg) = cfg {
        device_registry.set(crate::devices::build_devices(cfg));
    }
    if !storage.demo_mode {
        crate::devices::ha_import::spawn(device_registry.clone(), 120);
    }

    // Built-in auth. The identity store shares the history SQLite; the
    // policy is hot-read from the config file every 10s, and an existing
    // config without an [auth] block deserializes to mode=disabled.
    let auth_rt = history.clone().map(|hc| {
        let store = crate::auth::AuthStore::new(hc);
        let rt = Arc::new(crate::auth::AuthRuntime::new(store));
        rt.spawn_refresh(cfg_store.clone());
        rt
    });

    // Scheduled local backups: a no-op unless LOCALSKY_AUTO_BACKUP_HOURS
    // is set. The same bundle format as GET /api/backup, on an interval,
    // pruned, so a self-hoster always has a restorable artifact.
    crate::scheduler::backup::spawn(
        cfg_store.clone(),
        history.clone(),
        storage.history_path.clone(),
    );

    // Zone photos: uploads land here and are served back at /site/photos/*.
    let photos_dir = std::env::var("LOCALSKY_PHOTOS_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/data/site/photos"));
    if let Err(e) = std::fs::create_dir_all(&photos_dir) {
        tracing::warn!(
            ?e,
            "could not create photos dir {}; uploads will fail until the path exists",
            photos_dir.display()
        );
    }

    // Live runtime handles for config hot-reload: a PUT /api/config, a
    // wizard apply or a rollback re-applies the engine-tunable subset to
    // the RUNNING system with no restart.
    let runtime_handles = crate::runtime::RuntimeHandles {
        dispatch_context: control.registry.zone_locks(),
        tempest_store: stores.tempest.clone(),
        forecast_priority: config.forecast_priority.clone(),
        watering_policy: config.policy.clone(),
        manual_schedules: config.manual_schedules.clone(),
        source_reachable: sources.reachable.clone(),
        source_last_seen: Some(sources.last_seen.clone()),
        push: Some(stores.push.clone()),
    };
    // Built once and cloned into every route that answers health.
    let health = api::health::HealthState {
        started_at: std::time::Instant::now(),
        config_store: Some(cfg_store.clone()),
        sensor_history: sources.sensor_history.clone(),
        tempest_store: Some(stores.tempest.clone()),
        forecast_store: Some(stores.forecast.clone()),
        irrigation_store: Some(stores.irrigation.clone()),
        source_last_seen: Some(sources.last_seen.clone()),
        source_reachable: Some(sources.reachable.clone()),
        active_runs: control.active_runs.clone(),
    };
    // The zone dispatch and inventory plumbing are absent in demo mode,
    // where the zone actions answer 503 and the inventory is soil-only.
    let (dispatch, inventory) = if storage.demo_mode {
        (None, None)
    } else {
        (
            Some(api::irrigation::DispatchState {
                registry: control.registry.clone(),
                runs: control.runs.clone(),
                active_runs: control.active_runs.clone(),
                policy: config.policy.clone(),
            }),
            Some(api::sensors::InventoryState {
                cfg_store: cfg_store.clone(),
                controllers: control.registry.clone(),
            }),
        )
    };
    // The LLM advisor, from the configured [llm] block (falling back to
    // env). Lazy: never calls upstream until an /explanation or /anomalies
    // request hits. Resolved at boot, so a change flags a restart.
    let advisor = crate::llm::AdvisorState::from_config_or_env(cfg.and_then(|c| c.llm.as_ref()));
    let core = api::router(api::ApiState {
        tempest: stores.tempest.clone(),
        irrigation: stores.irrigation.clone(),
        forecast: stores.forecast.clone(),
        advisor,
        history: history.clone(),
        source: control.snapshot_source,
        devices: device_registry,
        sprinkler_prefix: cfg
            .map(|c| c.deployment.ha_sprinkler_prefix.clone())
            .unwrap_or_else(|| "opensprinkler".to_string()),
        cfg_store: cfg_store.clone(),
        watering_policy: config.policy.clone(),
        dispatch,
        inventory,
        tuning: control.tuning.clone(),
    })
    .layer(axum::middleware::from_fn(json_error_envelope));

    let updates = crate::updates::Updates::new();
    if cfg.map(|c| c.updates.check_enabled).unwrap_or(false) {
        crate::updates::spawn(&updates);
    }

    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                let tempest = stores.tempest.clone();
                let irrigation = stores.irrigation.clone();
                let forecast = stores.forecast.clone();
                let cfg = cfg_store.clone();
                move || {
                    provide_context(tempest.clone());
                    provide_context(irrigation.clone());
                    provide_context(forecast.clone());
                    provide_context(cfg.clone());
                }
            },
            {
                let opts = leptos_options.clone();
                move || shell(opts.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(shell))
        .with_state(leptos_options)
        // An unconfigured install opens on the wizard: every page 302s to
        // /setup until the config file exists. APIs, docs, assets and the
        // wizard itself pass (see setup_gate::EXEMPT_PREFIXES).
        .layer(axum::middleware::from_fn_with_state(
            cfg_store.clone(),
            crate::setup_gate::redirect_unconfigured,
        ))
        // The service worker lives at the origin root so its scope is the
        // whole app; the handler interpolates SW_VERSION at request time so
        // every deploy forces install -> waiting -> activate.
        .route("/sw.js", get(crate::sw::sw_js));

    let app = mount_both(app, "", core);
    let app = mount_both(
        app,
        "/push",
        crate::push::router(crate::push::api::PushState {
            history_conn: history.clone(),
            // Resolved on the first request, not here: the wizard writes
            // the keypair during this same boot.
            vapid_public_key: Arc::new(std::sync::OnceLock::new()),
        }),
    );
    let app = mount_both(app, "/location", api::location::router(cfg_store.clone()));
    let app = mount_both(app, "/system", api::system::router(history.clone()));
    let app = mount_both(
        app,
        "/config",
        api::config::router(api::config::ConfigApiState {
            store: cfg_store.clone(),
            runtime: Some(runtime_handles.clone()),
            tuning: control.tuning.clone(),
        }),
    );
    let app = mount_both(
        app,
        "/wizard",
        api::wizard::router(api::wizard::WizardApiState {
            draft_store: Arc::new(WizardStore::new(format!("{}.draft", config.path))),
            config_store: cfg_store.clone(),
            auth_rt: auth_rt.clone(),
            tempest_store: Some(stores.tempest.clone()),
            runtime: Some(runtime_handles.clone()),
            geocode_limiter: Default::default(),
        }),
    );
    let app = mount_both(
        app,
        "/health",
        Router::new()
            .route("/", get(api::health::health))
            .with_state(health.clone()),
    );
    let app = mount_both(
        app,
        "/diagnostics",
        api::diagnostics::router(api::diagnostics::DiagnosticsState {
            cfg_store: cfg_store.clone(),
            irrigation_store: Some(stores.irrigation.clone()),
            health,
            logs: logging.ring,
        }),
    );
    // The ingest receivers: /ingest (the address Ecowitt consoles and
    // webhook senders were given) and /api/v1/ingest.
    let ingest = api::ingest::router(api::ingest::IngestState {
        ecowitt: sources.ecowitt.clone(),
        webhooks: sources.webhooks.clone(),
        sensor_history: sources.sensor_history.clone(),
    });
    let app = app
        .nest("/ingest", ingest.clone())
        .nest("/api/v1/ingest", ingest);
    let app = mount_both(app, "/zones", api::photos::router(photos_dir.clone()));
    let app = app
        // Radar map data services, canonical-prefix only (both shipped
        // after the /api/v1 split, so there is no legacy alias to honor).
        .nest(
            "/api/v1/radar",
            api::windgrid::router(api::windgrid::WindGridState::new(Some(cfg_store.clone())))
                .merge(api::tropical::router(api::tropical::TropicalState::new()))
                .merge(api::precip::router(api::precip::PrecipState::new(Some(
                    cfg_store.clone(),
                )))),
        )
        .nest(
            "/api/v1/backup",
            api::backup::router(api::backup::BackupApiState {
                cfg_store: cfg_store.clone(),
                db: history.clone(),
                db_path: storage.history_path.clone(),
                runtime: Some(runtime_handles.clone()),
            }),
        )
        .nest(
            "/api/v1/updates",
            Router::new()
                .route("/", get(crate::updates::updates_handler))
                .with_state(updates),
        )
        .nest_service(
            "/site/photos",
            tower_http::services::ServeDir::new(&photos_dir),
        )
        // Bundled documentation, served same-origin so in-app help is
        // version-matched to the running build and works offline. The
        // Dockerfile runs `mdbook build docs` into <site_root>/docs; this
        // router resolves extensionless /docs/<slug> -> <slug>.html.
        // Mounted ahead of the SSR fallback so /docs/* never resolves to
        // the app shell.
        .nest("/docs", crate::docs_serve::router(&site_root))
        .layer(axum::middleware::from_fn(cache_policy))
        // App-baseline security headers, limited to what cannot break a
        // deploy. if_not_present, so an upstream proxy that already sets
        // a stricter value wins. DELIBERATELY NOT SET here: HSTS (breaks
        // LAN-HTTP self-hosters; belongs at the TLS edge), X-Frame-Options
        // and frame-ancestors (the HAOS ingress iframe embeds the addon
        // cross-origin), script/style CSP (risks Leptos hydration).
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("geolocation=(), camera=(), microphone=()"),
        ));

    // The auth API plus the enforcement middleware over the complete
    // router, so it sees pages, APIs and the static fallback. With no
    // history DB there is no identity store and login is unavailable,
    // but the STRUCTURAL protections (the Origin check and the privileged
    // route gate) still layer and FAIL CLOSED: only IP-vouched callers
    // reach privileged routes.
    let app = match auth_rt.clone() {
        Some(rt) => {
            let auth = api::auth::router(api::auth::AuthApiState {
                rt: rt.clone(),
                cfg_store: cfg_store.clone(),
            });
            mount_both(app, "/auth", auth).layer(axum::middleware::from_fn_with_state(
                rt,
                crate::auth::middleware::enforce,
            ))
        }
        None => {
            tracing::warn!(
                "no history DB; built-in login unavailable (mode stays disabled), but the Origin \
                 check + privileged-route gate still layer and FAIL CLOSED for anonymous callers"
            );
            let gate = Arc::new(crate::auth::NoStoreGate::new());
            gate.spawn_refresh(cfg_store.clone());
            app.layer(axum::middleware::from_fn_with_state(
                gate,
                crate::auth::middleware::enforce_no_store,
            ))
        }
    };

    // The public-demo read-only gate, outermost so it short-circuits
    // mutations and outbound probes before auth or any handler runs.
    let app = if storage.demo_mode {
        tracing::info!(
            "LOCALSKY_DEMO=1: demo read-only gate active (mutations + probes return 403)"
        );
        app.layer(axum::middleware::from_fn(
            crate::auth::demo_guard::block_when_demo,
        ))
    } else {
        app
    };

    // gzip/brotli. The hydrate wasm ships ~2.8 MB (brotli) instead of
    // ~24.5 MB, the single highest cold-load win on every RPi and
    // HA-OS-ingress deploy. DefaultPredicate skips text/event-stream (the
    // live /stream SSE), images and tiny bodies.
    let app = app.layer(tower_http::compression::CompressionLayer::new());

    // mDNS announce so the HACS zeroconf step and LAN clients find this
    // instance. Config-gated (network.mdns_enabled, default on); skipped
    // in demo mode. The returned announcement starts only after TCP binds.
    let mdns_enabled = cfg.map(|c| c.network.mdns_enabled).unwrap_or(true);
    let announcement =
        (mdns_enabled && !storage.demo_mode).then(|| crate::mdns::Announcement::new(auth_rt));

    Ok(Served {
        app,
        addr,
        announcement,
    })
}

/// Prometheus exposition. Public (auth-exempt in the middleware).
async fn metrics_handler() -> impl axum::response::IntoResponse {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        crate::metrics::render(),
    )
}

/// ONE error shape on the API surface: axum's built-in extractor
/// rejections (a typoed ?days=abc query, malformed JSON, a wrong
/// Content-Type) reply text/plain, while every handler-authored error is
/// the JSON {"error": ...} envelope integrators are told to parse. This
/// rewrites any text/plain ERROR response into the envelope so a client's
/// resp.json() error handler never throws on the transport layer's own
/// rejections. Only error statuses with a text/plain body are touched;
/// rejection bodies are tiny, the 64KB read cap is pure defense.
async fn json_error_envelope(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let res = next.run(req).await;
    let status = res.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return res;
    }
    let is_plain = res
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.starts_with("text/plain"))
        .unwrap_or(false);
    if !is_plain {
        return res;
    }
    let (parts, body) = res.into_parts();
    let bytes = axum::body::to_bytes(body, 64 * 1024)
        .await
        .unwrap_or_default();
    let msg = String::from_utf8_lossy(&bytes).trim().to_string();
    let mut out = axum::response::Json(serde_json::json!({ "error": msg })).into_response();
    *out.status_mut() = parts.status;
    out
}

/// Cache policy, path-aware. CONTENT-HASHED /pkg assets (the
/// LEPTOS_HASH_FILES output, a basename with 3+ dot segments) are
/// immutable BY NAME: a new deploy mints new URLs, so browsers may cache
/// them forever. EVERYTHING ELSE (the HTML shell, hashless assets, /docs)
/// stays no-cache, forcing revalidation on every request; without it
/// mobile Chrome applies heuristic caching and serves stale bytes from a
/// previous deploy.
async fn cache_policy(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let immutable = req
        .uri()
        .path()
        .strip_prefix("/pkg/")
        .map(|f| f.split('.').count() >= 3)
        .unwrap_or(false);
    let mut res = next.run(req).await;
    let v = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    res.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(v));
    res
}
