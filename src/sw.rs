// Service worker route handler. Serves /sw.js with the SW_VERSION constant
// interpolated at request time so every deploy ships a byte-different /sw.js,
// which triggers the browser's install -> activate lifecycle and picks up
// updated push logic immediately.
//
// NOTE: the SW is now push-only (see public/sw.template.js). Asset cache-
// busting is handled by content-hashed /pkg filenames (hash-files in Cargo.toml
// + LEPTOS_HASH_FILES), NOT by the SW, so the version no longer namespaces any
// cache. It still matters: a byte-different script is what makes the browser
// install the new push/click handlers and run the one-time old-cache cleanup in
// the activate handler.

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
