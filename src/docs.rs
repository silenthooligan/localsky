// Documentation URL contract. Every "?" hint, every "see the docs"
// link, every error message that points to a guide builds its URL
// from these constants so the entire app moves in lockstep when the
// docs site evolves.
//
// localsky.io is the planned public docs site; pages don't exist
// yet — these links are intentional placeholders so the URL contract
// is wired across the UI before content lands. When a page ships at
// localsky.io/docs/<slug>, every reference to that slug from this
// app starts resolving with no code change.
//
// Repository / issue tracker links keep pointing at the public
// GitHub repo because those are dev-facing and live there
// permanently.

/// Public docs site root.
pub const SITE_BASE: &str = "https://localsky.io";

/// Docs section root. Sub-pages live at `{DOCS_BASE}/<slug>`.
pub const DOCS_BASE: &str = "https://localsky.io/docs";

/// Public GitHub repo (issues, releases, source).
pub const REPO_URL: &str = "https://github.com/silenthooligan/localsky";

/// Issues tracker for "report a problem" links.
pub const ISSUES_URL: &str = "https://github.com/silenthooligan/localsky/issues";

/// Build a documentation page URL from a slug. Slugs use kebab-case
/// and match the keys in `ui::help_hint::help_topic`. Example:
/// `doc_url("controllers")` -> `https://localsky.io/docs/controllers`.
pub fn doc_url(slug: &str) -> String {
    format!("{DOCS_BASE}/{slug}")
}
