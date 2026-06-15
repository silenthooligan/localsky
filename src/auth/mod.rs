// Built-in authentication. Identity lives in SQLite (M0009: users,
// auth_sessions, api_tokens), never in the TOML config (which is
// round-tripped through GET/PUT /api/config and snapshotted). The TOML
// carries only policy: [auth] mode/session_ttl_days/trusted_networks,
// all serde-defaulted so existing configs deserialize to mode=disabled
// and nothing changes for upgrades until the owner opts in (the wizard
// writes mode="required" for new installs that create an account).
//
//   hash.rs       - argon2id password hashing + token generation
//   store.rs      - all SQLite access (spawn_blocking discipline)
//   middleware.rs - the request gate (exemption table, Bearer/cookie/
//                   query-token acceptance, html-vs-api responses)
//
// Token formats:
//   session cookie  localsky_session=lss_<43 chars base64url>
//   API token       lsk_<43 chars base64url>
// Both are stored as sha256 hex; plaintext is shown exactly once.

pub mod demo_guard;
pub mod hash;
pub mod middleware;
pub mod store;

pub use middleware::{AuthRuntime, RequestIdentity};
pub use store::AuthStore;

/// Cookie name for browser sessions.
pub const SESSION_COOKIE: &str = "localsky_session";
/// Prefix on session tokens (cookie value).
pub const SESSION_PREFIX: &str = "lss_";
/// Prefix on long-lived API tokens (HACS, automations).
pub const API_TOKEN_PREFIX: &str = "lsk_";
