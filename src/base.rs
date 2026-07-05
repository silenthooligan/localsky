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
//   - The hydrated client reads the meta tag for two distinct jobs, and the
//     distinction matters (see issue #3):
//       * The Router `base` prop, so leptos_router strips the prefix before
//         matching AND re-applies it inside navigate()/<A>/<Redirect>. Those
//         router navigations therefore take a PLAIN route and must NOT be
//         wrapped in `url()` below, or the prefix is applied twice.
//       * `url()`, for the URLs the client hands straight to the browser with
//         no router in between: window.location/set_href targets, the plain
//         <a href> the shell click-shim rewrites, and the shell's asset links.
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
/// prefix is active. Use ONLY for URLs handed straight to the browser:
/// window.location/set_href targets, plain `<a href>` values, and the
/// shell's asset links. Do NOT use it for leptos_router navigate()/<A>/
/// <Redirect> targets (the Router base already prefixes those, so url()
/// would double-prefix under ingress: issue #3), and not for plain
/// fetch/EventSource calls (the shell shim translates those; they stay
/// root-relative).
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

    // Regression guard for issue #3 (HA ingress double-prefix). leptos_router
    // resolves a navigate()/<A>/client-side-<Redirect> target against the
    // Router base itself, so handing any of them a path that base::url() has
    // already prefixed produces "/api/hassio_ingress/<t>/api/hassio_ingress/
    // <t>/..." and 404s under ingress. Raw browser sinks (window.location,
    // set_href, asset links, plain <a href> resolved by the shell click shim)
    // are the ONLY correct callers of base::url(). This scans the component
    // tree for a navigate call whose argument is a base::url expression, so
    // that specific mistake cannot return unnoticed.
    #[test]
    fn navigate_targets_are_never_pre_prefixed_with_base_url() {
        use std::path::Path;
        // Assembled at runtime so this guard file never matches itself.
        let needle: String = ["navigate(&crate::base::", "url("].concat();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders = Vec::new();
        scan_for(&root, &needle, &mut offenders);
        assert!(
            offenders.is_empty(),
            "navigate() targets must be plain routes (the Router base is \
             applied by navigate itself); base::url() double-prefixes under \
             HA ingress. Offending files: {offenders:?}"
        );
    }

    fn scan_for(dir: &std::path::Path, needle: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                scan_for(&path, needle, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                let src = std::fs::read_to_string(&path).expect("read rs file");
                let squeezed: String = src.chars().filter(|c| !c.is_whitespace()).collect();
                if squeezed.contains(needle) {
                    out.push(path.display().to_string());
                }
            }
        }
    }
}
