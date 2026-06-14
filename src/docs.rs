// Documentation URL contract. Every "?" hint, every "see the docs"
// link, every error message that points to a guide builds its URL
// from these constants so the entire app moves in lockstep when the
// docs site evolves.
//
// The docs are bundled INTO the app: the same mdbook output that ships
// to localsky.io is built into the image and served same-origin at
// /docs (see main.rs's docs ServeDir + the Dockerfile `mdbook build`).
// That means help is version-matched to the running build and works
// offline, on LAN-only, and on air-gapped installs, no internet round
// trip to localsky.io. doc_url() therefore returns an app-local,
// ingress-aware path, NOT an absolute localsky.io URL.
//
// SITE_BASE / REPO_URL / ISSUES_URL stay absolute: they point at the
// public marketing site root and the GitHub repo, which are genuinely
// external and live there permanently.

/// Public site root (marketing / landing). Genuinely external.
pub const SITE_BASE: &str = "https://localsky.io";

/// Public docs site root. Retained for any reference that explicitly
/// wants the hosted copy; in-app help links use `doc_url` instead so
/// they resolve against the bundled, same-origin docs.
pub const DOCS_BASE: &str = "https://localsky.io/docs";

/// Public GitHub repo (issues, releases, source).
pub const REPO_URL: &str = "https://github.com/silenthooligan/localsky";

/// Issues tracker for "report a problem" links.
pub const ISSUES_URL: &str = "https://github.com/silenthooligan/localsky/issues";

/// Build an app-local documentation page URL from a slug. Slugs use
/// kebab-case and match the keys in `ui::help_hint::help_topic` and the
/// page file names under `docs/src/<slug>.md`.
///
/// The returned path is same-origin (`/docs/<slug>`) and ingress-aware:
/// `crate::base::url` prefixes it with the active `X-Ingress-Path` base
/// on SSR (per-request header) and with the cached `<meta
/// name="localsky-base">` value on hydrate, exactly like every other
/// in-app link/asset URL. Home Assistant ingress strips the prefix
/// before forwarding, so the SERVER route stays plain `/docs/<slug>`
/// while the BROWSER href carries the prefix.
///
/// Extensionless by design: the bundled docs ServeDir in main.rs
/// resolves `/docs/<slug>` to `<slug>.html` (try-files), matching the
/// public Caddy site, mdbook's canonical site-url, and a plain
/// `/docs/<slug>.html` request alike. Example:
/// `doc_url("controllers")` -> `/docs/controllers` (or
/// `/api/hassio_ingress/<token>/docs/controllers` under ingress).
pub fn doc_url(slug: &str) -> String {
    crate::base::url(&format!("/docs/{slug}"))
}

#[cfg(test)]
mod tests {
    use super::doc_url;

    #[test]
    fn doc_url_is_app_local_and_extensionless() {
        // No ingress prefix in a feature-less/test context, so the path
        // is the bare same-origin route the docs ServeDir mounts at.
        assert_eq!(doc_url("controllers"), "/docs/controllers");
        assert_eq!(doc_url("getting-started"), "/docs/getting-started");
        // Never an absolute localsky.io URL anymore.
        assert!(!doc_url("faq").starts_with("http"));
    }
}
