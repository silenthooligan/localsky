// Base-path support for serving the app under a rewritten URL prefix.
//
// Home Assistant ingress (and any prefix-stripping reverse proxy that sets
// the `X-Ingress-Path` header) serves the UI at a path like
// `/api/hassio_ingress/<token>/...` on its own origin while forwarding
// stripped paths to us. Server-side routing is therefore unaffected; what
// breaks is every root-relative URL the BROWSER resolves (assets, links,
// fetches). The strategy:
//
//   - SSR reads the header per request (so direct-port access and ingress
//     access work simultaneously from the same process) and emits the
//     prefix into the shell: asset links, a `<meta name="localsky-base">`
//     tag, and a small fetch/EventSource shim (see app.rs::shell) that
//     translates the WASM app's root-relative network calls at the
//     boundary. The Rust client code keeps thinking it lives at `/`.
//   - The hydrated client reads the meta tag for the few places that must
//     emit prefixed URLs themselves: router base, anchor hrefs, navigate()
//     targets.
//
// The prefix is sanitized to a conservative charset before use: it is
// attacker-supplied (any LAN client can send the header) and gets embedded
// into HTML and inline JS. An invalid prefix degrades to "" (no prefix).

/// Validate an ingress prefix: absolute, no traversal, conservative
/// charset, no trailing slash. Anything else collapses to "".
fn sanitize(raw: &str) -> String {
    let trimmed = raw.trim_end_matches('/');
    let ok = trimmed.starts_with('/')
        && !trimmed.contains("..")
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'));
    if ok {
        trimmed.to_string()
    } else {
        String::new()
    }
}

/// Resolve the prefix from raw request headers. For server code that has
/// the request in hand (middleware) rather than leptos context.
#[cfg(feature = "ssr")]
pub fn from_headers(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-ingress-path")
        .and_then(|v| v.to_str().ok())
        .map(sanitize)
        .unwrap_or_default()
}

/// The active URL prefix for browser-resolved paths. "" when the request
/// came in directly (no proxy header) or the header failed validation.
#[cfg(feature = "ssr")]
pub fn base_path() -> String {
    use leptos::prelude::use_context;
    // axum::http is the `http` crate re-exported; this is the same Parts
    // type leptos_axum provides into context per request.
    use_context::<axum::http::request::Parts>()
        .map(|parts| from_headers(&parts.headers))
        .unwrap_or_default()
}

/// Hydrate side: the SSR shell stamps the prefix into
/// `<meta name="localsky-base">`; read it once and cache.
#[cfg(all(feature = "hydrate", not(feature = "ssr")))]
pub fn base_path() -> String {
    use std::cell::OnceCell;
    thread_local! {
        static BASE: OnceCell<String> = const { OnceCell::new() };
    }
    BASE.with(|cell| {
        cell.get_or_init(|| {
            web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| {
                    d.query_selector("meta[name='localsky-base']")
                        .ok()
                        .flatten()
                })
                .and_then(|m| m.get_attribute("content"))
                .map(|raw| sanitize(&raw))
                .unwrap_or_default()
        })
        .clone()
    })
}

/// Feature-less builds (plain `cargo check`) have no request or document
/// to consult; behave as unprefixed.
#[cfg(not(any(feature = "ssr", feature = "hydrate")))]
pub fn base_path() -> String {
    String::new()
}

/// Prefix a root-relative path with the active base. Identity when no
/// prefix is active. Use for anchor hrefs, navigate() targets, and the
/// shell's asset links; plain fetch/EventSource calls are translated by
/// the shell shim instead and should stay root-relative.
pub fn url(path: &str) -> String {
    let base = base_path();
    if base.is_empty() {
        path.to_string()
    } else {
        format!("{base}{path}")
    }
}

/// Map a browser pathname back into app route space by stripping the
/// active base. Use when comparing `use_location().pathname` against route
/// literals (active-link highlighting); the router strips the base before
/// matching, but the location signal carries the full browser path.
pub fn route_path(pathname: &str) -> String {
    let base = base_path();
    if !base.is_empty() {
        if let Some(stripped) = pathname.strip_prefix(base.as_str()) {
            return if stripped.is_empty() {
                "/".to_string()
            } else {
                stripped.to_string()
            };
        }
    }
    pathname.to_string()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn sanitize_accepts_ingress_shape() {
        assert_eq!(
            sanitize("/api/hassio_ingress/AbC123-_token"),
            "/api/hassio_ingress/AbC123-_token"
        );
    }

    #[test]
    fn sanitize_strips_trailing_slash() {
        assert_eq!(sanitize("/prefix/"), "/prefix");
    }

    #[test]
    fn sanitize_rejects_garbage() {
        assert_eq!(sanitize("not-absolute"), "");
        assert_eq!(sanitize("/has space"), "");
        assert_eq!(sanitize("/dot/../dot"), "");
        assert_eq!(sanitize("/quote'inject"), "");
        assert_eq!(sanitize("/<script>"), "");
        assert_eq!(sanitize(""), "");
    }
}
