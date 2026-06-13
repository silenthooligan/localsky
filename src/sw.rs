// Service worker route handler. Serves /sw.js with the SW_VERSION constant
// interpolated at request time so every deploy invalidates the SW (and
// therefore every namespaced cache) on next page load.
//
// Why interpolate at request time rather than at build time:
// - cargo-leptos doesn't fingerprint /pkg/* filenames in this project, so we
//   can't rely on a hashed URL change to bust the SW.
// - Embedding the version as a constant in JS gives `caches.delete()` on
//   activate something concrete to compare against.
// - Putting the version into the served JS rather than into a query string
//   means browsers see a *byte-different* sw.js on each deploy, which is what
//   triggers the install -> waiting -> activate lifecycle.

use axum::{
    http::header::{self, HeaderMap, HeaderValue},
    response::IntoResponse,
};

const SW_TEMPLATE: &str = include_str!("../public/sw.template.js");

fn sw_version() -> String {
    let pkg = env!("CARGO_PKG_VERSION");
    let sha = option_env!("GIT_SHA").unwrap_or("dev");
    let short = sha.chars().take(8).collect::<String>();
    format!("{pkg}-{short}")
}

pub async fn sw_js() -> impl IntoResponse {
    let body = SW_TEMPLATE.replace("__SW_VERSION__", &sw_version());

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/javascript; charset=utf-8"),
    );
    // Browsers always re-check the SW script (every ~24h or on registration),
    // but explicit no-cache keeps proxies and the existing main.rs Cache-Control
    // layer aligned. The SW_VERSION namespacing is what actually gates caches.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-store, must-revalidate"),
    );
    // Service-Worker-Allowed lets the registration claim the entire origin
    // even though the script lives at /sw.js (which would otherwise scope it
    // to the root only by default, that happens to match what we want, but
    // being explicit avoids surprises if we ever move the file).
    headers.insert("Service-Worker-Allowed", HeaderValue::from_static("/"));

    (headers, body)
}
