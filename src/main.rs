// SSR binary entry — boots the Tempest UDP listener, then runs the Axum
// HTTP server. The shared TempestStore is created here, populated by the
// listener, and read by the API endpoints + the SSR pass for the Leptos
// app (via `provide_context`).

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() {
    use axum::Router;
    use leptos::prelude::*;
    use leptos_axum::{generate_route_list, LeptosRoutes};
    use std::sync::Arc;
    use tracing_subscriber::EnvFilter;
    use axum::http::{header, HeaderValue};
    use tower_http::set_header::SetResponseHeaderLayer;
    use localsky::{
        api,
        app::{shell, App},
        config::{wizard::WizardStore, FileConfigStore},
        forecast::{spawn_forecast_refresher, ForecastStore},
        ha::{spawn_refresher, IrrigationStore},
        history::HistoryDb,
        llm::AdvisorState,
        ports::config_store::ConfigStore,
        push, runtime_helpers,
        sw,
        tempest::{listener::spawn_listener, state::TempestStore},
    };
    use axum::routing::get;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let tempest_store = Arc::new(TempestStore::new());

    // Demo mode short-circuits the live data path. When set, the demo
    // feeder writes synthetic weather + irrigation + forecast snapshots
    // into the stores; the legacy listener + refreshers are not spawned.
    // Useful for screenshots, public demos, and CI smoke tests.
    let demo_mode = std::env::var("LOCALSKY_DEMO").ok().as_deref() == Some("1");
    if !demo_mode {
        spawn_listener(tempest_store.clone());
    }

    // SQLite-backed run history. Optional — if /data isn't mounted
    // or the file can't be opened, we log and run without history
    // (the rest of the irrigation page works fine).
    let history_path = std::env::var("HISTORY_DB_PATH")
        .unwrap_or_else(|_| "/data/irrigation.db".to_string());
    let history_db = match HistoryDb::open(history_path.clone().into()) {
        Ok(db) => Some(db),
        Err(e) => {
            tracing::warn!(
                "history db open failed at {history_path:?}: {e:#}; running without persistence"
            );
            None
        }
    };
    let history_conn = history_db.as_ref().map(|db| db.handle());

    let forecast_store = Arc::new(ForecastStore::new());
    if !demo_mode {
        spawn_forecast_refresher(forecast_store.clone());
    }

    // Push dispatcher. Background task that drains PushEvents from the
    // HA refresher and fans them out to subscribed PWAs via VAPID.
    // Non-fatal if VAPID env is missing — the dispatcher logs once and
    // drops every event, so the rest of the app keeps running.
    let push_dispatcher = push::spawn_dispatcher(history_conn.clone());

    let irrigation_store = Arc::new(IrrigationStore::new());
    if !demo_mode {
        spawn_refresher(
            irrigation_store.clone(),
            forecast_store.clone(),
            tempest_store.clone(),
            history_conn.clone(),
            push_dispatcher.clone(),
        );
    } else {
        localsky::demo_data::spawn(
            tempest_store.clone(),
            irrigation_store.clone(),
            forecast_store.clone(),
        );
        tracing::info!("LOCALSKY_DEMO=1: live data paths disabled; demo feeder active");
    }

    let conf = get_configuration(None).unwrap();
    let leptos_options = conf.leptos_options;
    let addr = leptos_options.site_addr;
    let routes = generate_route_list(App);

    // LLM advisor: wraps an OpenAI-compatible client + a TTL cache
    // for explanations + anomalies. Lazy: never calls upstream until
    // an /explanation or /anomalies request hits.
    // Set LLM_ADVISOR_DISABLED=1 to short-circuit (tile reads "disabled").
    let advisor = AdvisorState::from_env();

    let api_router = api::router(
        tempest_store.clone(),
        irrigation_store.clone(),
        forecast_store.clone(),
        advisor,
        history_conn.clone(),
    );

    // v2 opt-in. Set LOCALSKY_V2=1 (or place a /data/localsky.toml file)
    // to mount /api/config + /api/wizard + /api/health + /ingest/*
    // alongside the v0.1 routes. These endpoints power the settings UI,
    // first-run wizard, health probes, and the HTTP-receiver sensor
    // sources (Ecowitt local + generic webhook). They never interact
    // with the legacy irrigation refresher, so toggling them on is safe
    // for the existing deployment.
    let config_path = std::env::var("CONFIG_PATH")
        .unwrap_or_else(|_| "/data/localsky.toml".to_string());
    let v2_enabled = std::env::var("LOCALSKY_V2").ok().as_deref() == Some("1")
        || std::path::Path::new(&config_path).exists();
    let (v2_config_router, v2_wizard_router, v2_health_router, v2_ingest_router) = if v2_enabled {
        let cfg_store = Arc::new(FileConfigStore::new(&config_path));
        let draft_path = format!("{config_path}.draft");
        let draft_store = Arc::new(WizardStore::new(&draft_path));

        // Construct receiver sources from the loaded Config so their
        // POST handlers have somewhere to emit observations.
        let (receiver_bus_tx, _rx) = tokio::sync::broadcast::channel(256);
        let (ecowitt_sources, webhook_sources) = match cfg_store.load().await {
            Ok(cfg) => runtime_helpers::build_receiver_sources(&cfg, receiver_bus_tx.clone()),
            Err(e) => {
                tracing::warn!(
                    config = %config_path,
                    error = %e,
                    "could not load config for receiver sources; /ingest/* will return 503"
                );
                (Vec::new(), Vec::new())
            }
        };

        tracing::info!(
            config = %config_path,
            ecowitt_receivers = ecowitt_sources.len(),
            webhook_receivers = webhook_sources.len(),
            "v2 endpoints enabled: /api/config + /api/wizard + /api/health + /ingest/* mounted"
        );

        // Per-source freshness needs the SensorHistoryStore. Open the
        // SQLite at HISTORY_DB_PATH (defaults to /data/irrigation.db,
        // same as the legacy refresher's path); if it doesn't open
        // cleanly, health still works but the sources[] field stays
        // empty.
        let sensor_history = match rusqlite::Connection::open(&history_path) {
            Ok(c) => {
                Some(localsky::persistence::SensorHistoryStore::new(Arc::new(
                    tokio::sync::Mutex::new(c),
                )))
            }
            Err(e) => {
                tracing::warn!(
                    history = %history_path,
                    error = %e,
                    "could not open sensor history for /api/health; freshness unavailable"
                );
                None
            }
        };

        let health_router = axum::Router::new()
            .route("/", axum::routing::get(api::health::health))
            .with_state(api::health::HealthState {
                config_store: Some(cfg_store.clone()),
                sensor_history,
            });
        let ingest_router = api::ingest::router(api::ingest::IngestState {
            ecowitt: ecowitt_sources,
            webhooks: webhook_sources,
        });

        (
            Some(api::config::router(cfg_store.clone())),
            Some(api::wizard::router(api::wizard::WizardApiState {
                draft_store,
                config_store: cfg_store,
            })),
            Some(health_router),
            Some(ingest_router),
        )
    } else {
        (None, None, None, None)
    };

    let app = Router::new()
        .leptos_routes_with_context(
            &leptos_options,
            routes,
            {
                let tempest = tempest_store.clone();
                let irrigation = irrigation_store.clone();
                let forecast = forecast_store.clone();
                move || {
                    provide_context(tempest.clone());
                    provide_context(irrigation.clone());
                    provide_context(forecast.clone());
                }
            },
            {
                let opts = leptos_options.clone();
                move || shell(opts.clone())
            },
        )
        .fallback(leptos_axum::file_and_error_handler(shell))
        .with_state(leptos_options)
        // Service worker. Lives at the origin root so its scope is the entire
        // app — registering /sw.js scopes to /, which is what we want. The
        // handler interpolates SW_VERSION at request time so every deploy
        // forces install -> waiting -> activate and old caches get nuked.
        .route("/sw.js", get(sw::sw_js))
        // Mount the snapshot + SSE endpoints at /api/*. `merge` would put
        // them at the root, where Leptos's fallback would never be reached
        // anyway — but the radar.js client expects `/api/snapshot` and
        // `/api/stream` explicitly. Irrigation lives under /api/irrigation.
        .nest("/api", api_router)
        // Web Push subscribe/unsubscribe + vapid-key. Lives under /api/push.
        .nest(
            "/api/push",
            push::router(push::api::PushState {
                history_conn: history_conn.clone(),
            }),
        );

    // v2 endpoints. Mount only when LOCALSKY_V2=1 or /data/localsky.toml
    // exists. Either gives the operator a way to opt into the new
    // settings/wizard surface without restarting on the legacy path.
    let app = match (v2_config_router, v2_wizard_router, v2_health_router, v2_ingest_router) {
        (Some(c), Some(w), Some(h), Some(i)) => app
            .nest("/api/config", c)
            .nest("/api/wizard", w)
            .nest("/api/health", h)
            .nest("/ingest", i),
        _ => app,
    }
        // Force revalidation on every request. Without this, browsers
        // (notably mobile Chrome) apply heuristic caching to /pkg/*.css
        // and serve a stale stylesheet from a previous deploy. With
        // no-cache + the existing Last-Modified header, the browser
        // sends If-Modified-Since each visit; the server replies 304
        // when unchanged so the bytes are still cached, just always
        // verified fresh.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ));

    tracing::info!("localsky listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();
}

#[cfg(not(feature = "ssr"))]
pub fn main() {
    // The WASM client is built via `lib.rs::hydrate`; this stub is here so
    // the same binary target compiles cleanly with the `hydrate` feature.
}
