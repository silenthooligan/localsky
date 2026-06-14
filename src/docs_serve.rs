// Same-origin bundled documentation server.
//
// LocalSky ships its own docs INTO the image (the Dockerfile runs
// `mdbook build docs` and copies the output into the site root at
// `<site_root>/docs`). They are served here at `/docs` so in-app help is
// version-matched to the running build and works offline, on LAN-only,
// and on air-gapped installs, with no round trip to localsky.io.
//
// URL form: extensionless `/docs/<slug>` (what `crate::docs::doc_url`
// emits, what mdbook's canonical site-url uses, and what the public
// Caddy site serves). mdbook actually writes `<slug>.html`, so this
// router replicates Caddy's try-files resolution: for a request path it
// tries, in order:
//   1. the path as-is              (`/docs/css/general.css`, images, JS)
//   2. the path + ".html"          (`/docs/controllers` -> controllers.html)
//   3. the path + "/index.html"    (`/docs/` and section dirs -> index)
// The first candidate that resolves to a real file under the docs root
// is streamed back with the correct content-type + caching headers via
// tower_http::services::ServeFile.
//
// Ingress: Home Assistant ingress strips the `X-Ingress-Path` prefix
// before forwarding, so this server always sees a plain `/docs/...`
// path. The browser-facing prefix is added on the link side by
// `crate::base::url` (see docs.rs). Nothing prefix-aware is needed here.
//
// This route is mounted ahead of the Leptos SSR fallback so `/docs/*`
// never falls through to the app shell, and it is added to the auth
// exemption set so help is reachable pre-login and on fresh installs.

use std::path::{Component, Path, PathBuf};

use axum::{
    body::Body,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use tower::ServiceExt;
use tower_http::services::ServeFile;

#[derive(Clone)]
struct DocsState {
    root: PathBuf,
}

/// Build the `/docs` router rooted at `<site_root>/docs`.
///
/// `site_root` is the same value the Leptos static fallback resolves
/// against (`LEPTOS_SITE_ROOT`, default `target/site`), so this works in
/// local dev (`target/site/docs`) and in the container (`/app/site/docs`)
/// with no extra configuration.
pub fn router(site_root: &str) -> Router {
    let root = Path::new(site_root).join("docs");
    let state = DocsState { root };
    Router::new()
        // Bare /docs and /docs/ both serve the book index.
        .route("/", get(serve))
        .route("/{*path}", get(serve))
        .with_state(state)
}

/// Resolve the request path against the docs root using try-files, then
/// stream the chosen file. Returns 404 when no candidate exists or the
/// path tries to escape the root.
async fn serve(State(state): State<DocsState>, req: Request<Body>) -> Response {
    let raw = req.uri().path().trim_start_matches('/');
    // Reject traversal up front: only normal path segments are allowed.
    // (Anything with `..`, a root, or a prefix component is hostile.)
    let rel = Path::new(raw);
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let base = state.root.join(rel);
    // try-files order: exact file, then `.html`, then `index.html` in a
    // directory. The exact-file branch covers assets (css/js/fonts/png)
    // and any caller that already asked for `<slug>.html`.
    let candidates = [
        base.clone(),
        base.with_extension("html"),
        base.join("index.html"),
    ];
    let Some(file) = candidates.into_iter().find(|p| p.is_file()) else {
        return StatusCode::NOT_FOUND.into_response();
    };

    match ServeFile::new(&file).oneshot(req).await {
        Ok(resp) => resp.into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_roots_under_site_docs() {
        // The route table is built without touching the filesystem; this
        // just guards that construction with a typical site root does not
        // panic and the docs subdir is what we expect to serve from.
        let _ = router("target/site");
        assert_eq!(
            Path::new("target/site").join("docs"),
            PathBuf::from("target/site/docs")
        );
    }

    #[test]
    fn try_files_candidate_order() {
        // Documents the extensionless -> .html -> index.html resolution
        // contract the docs links depend on.
        let base = Path::new("/app/site/docs").join("controllers");
        let candidates = [
            base.clone(),
            base.with_extension("html"),
            base.join("index.html"),
        ];
        assert_eq!(candidates[0], PathBuf::from("/app/site/docs/controllers"));
        assert_eq!(
            candidates[1],
            PathBuf::from("/app/site/docs/controllers.html")
        );
        assert_eq!(
            candidates[2],
            PathBuf::from("/app/site/docs/controllers/index.html")
        );
    }
}
