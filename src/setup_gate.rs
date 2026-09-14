// The front door of an unconfigured install is the setup wizard.
//
// Until /data/localsky.toml exists, every page request is answered with
// a redirect to /setup. Before this, a fresh container served the full
// weather dashboard for nowhere in particular and the getting-started
// guide had to say "go to /setup yourself". The APIs, the docs, the
// wizard, the login page and every static asset are exempt, so the
// wizard itself, health checks and the service worker keep working.

use std::sync::Arc;

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::config::FileConfigStore;

/// Where an unconfigured install is sent.
pub const SETUP_PATH: &str = "/setup";

/// Paths never redirected. Prefix match on a path segment boundary.
pub const EXEMPT_PREFIXES: &[&str] = &[
    "/setup", "/login", "/api", "/docs", "/pkg", "/metrics", "/sw.js", "/ingest",
];

/// Whether this request is a page view that belongs at the wizard while
/// nothing is configured. Only GET/HEAD; only paths without a file
/// extension (assets keep loading); never the exempt prefixes.
pub fn should_redirect(method: &Method, path: &str, config_present: bool) -> bool {
    if config_present || !(method == Method::GET || method == Method::HEAD) {
        return false;
    }
    if EXEMPT_PREFIXES.iter().any(|p| {
        path == *p
            || path
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('/'))
    }) {
        return false;
    }
    let last = path.rsplit('/').next().unwrap_or("");
    !last.contains('.')
}

/// axum middleware: 302 every page of an unconfigured install to /setup.
pub async fn redirect_unconfigured(
    State(store): State<Arc<FileConfigStore>>,
    req: Request,
    next: Next,
) -> Response {
    if should_redirect(req.method(), req.uri().path(), store.is_initialized()) {
        let mut res = Response::new(axum::body::Body::empty());
        *res.status_mut() = StatusCode::FOUND;
        res.headers_mut()
            .insert(header::LOCATION, HeaderValue::from_static(SETUP_PATH));
        return res;
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use tower::ServiceExt;

    fn app(store: Arc<FileConfigStore>) -> Router {
        Router::new()
            .route("/", get(|| async { "home" }))
            .route("/setup", get(|| async { "wizard" }))
            .route("/api/v1/health", get(|| async { "{}" }))
            .fallback(|| async { "page" })
            .layer(axum::middleware::from_fn_with_state(
                store,
                redirect_unconfigured,
            ))
    }

    async fn status_of(app: &Router, path: &str) -> (StatusCode, Option<String>) {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let loc = res
            .headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        (res.status(), loc)
    }

    /// The e2e shape: a container on an empty volume. Every page goes to
    /// the wizard, the wizard and the APIs answer, and the moment the
    /// config file exists the pages come back.
    #[tokio::test]
    async fn an_empty_volume_sends_every_page_to_setup() {
        let dir = std::env::temp_dir().join(format!("localsky-setup-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localsky.toml");
        let store = Arc::new(FileConfigStore::new(&path));
        let app = app(store);

        for page in ["/", "/irrigation", "/zones/front", "/settings"] {
            let (status, loc) = status_of(&app, page).await;
            assert_eq!(status, StatusCode::FOUND, "{page}");
            assert_eq!(loc.as_deref(), Some("/setup"), "{page}");
        }
        for open in [
            "/setup",
            "/setup/location",
            "/api/v1/health",
            "/pkg/localsky.js",
            "/favicon.ico",
            "/sw.js",
            "/docs/index.html",
        ] {
            let (status, _) = status_of(&app, open).await;
            assert_eq!(status, StatusCode::OK, "{open} stays reachable");
        }

        std::fs::write(&path, "schema_version = 1\n").unwrap();
        let (status, _) = status_of(&app, "/").await;
        assert_eq!(status, StatusCode::OK, "configured: the dashboard serves");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_page_views_redirect() {
        assert!(should_redirect(&Method::GET, "/", false));
        assert!(should_redirect(&Method::HEAD, "/history", false));
        assert!(!should_redirect(&Method::POST, "/", false));
        assert!(!should_redirect(&Method::GET, "/", true));
        assert!(!should_redirect(&Method::GET, "/setup", false));
        assert!(
            should_redirect(&Method::GET, "/setupx", false),
            "a sibling path is not the wizard"
        );
        assert!(!should_redirect(&Method::GET, "/api/v1/info", false));
        assert!(!should_redirect(
            &Method::GET,
            "/manifest.webmanifest",
            false
        ));
        assert!(!should_redirect(&Method::GET, "/login", false));
    }
}
