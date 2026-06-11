// Push dispatcher. A small mpsc channel + background task that drains
// PushEvents emitted by the HA refresher and fans them out to all
// currently-subscribed browsers via Web Push (VAPID-signed).
//
// Failure mode: web-push errors with a 410 Gone (or 404) when a
// subscription has been revoked by the browser. We delete those rows
// immediately so the subscription list stays clean; other transient
// errors are logged and the row is preserved for the next attempt.
//
// VAPID config is loaded from env once at startup. Missing keys =
// dispatcher logs once and drops every event silently. That keeps the
// rest of the app running while the user generates keys.

use crate::push::store::{self, StoredSubscription};
use anyhow::Result;
use base64::Engine;
use rusqlite::Connection;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use web_push::{
    ContentEncoding, IsahcWebPushClient, SubscriptionInfo, SubscriptionKeys, VapidSignatureBuilder,
    WebPushClient, WebPushError, WebPushMessageBuilder,
};

#[derive(Debug, Clone)]
pub enum PushEvent {
    /// A zone just transitioned from idle to running. `name` is the
    /// human-friendly zone name (e.g. "Back yard"); `slug` matches the
    /// snapshot.
    ZoneStarted { name: String, slug: String },
    /// A zone just transitioned from running to idle. `duration_min` is
    /// the run length in minutes (rounded).
    ZoneStopped {
        name: String,
        slug: String,
        duration_min: u32,
    },
    /// Daily verdict computed (sent once per day on first verdict
    /// computation). `verdict` = "skip" | "run" | "run_extended".
    DailyVerdict { verdict: String, reason: String },
    /// A configured soil probe stopped producing valid readings (see
    /// the refresher's probe-fault detection). Sent once per probe per
    /// process lifetime. `since_epoch` is the last valid reading; None
    /// when the channel never produced one.
    SoilProbeFault {
        zone_name: String,
        zone_slug: String,
        since_epoch: Option<i64>,
    },
}

#[derive(Clone, Serialize)]
struct PushPayload {
    title: String,
    body: String,
    /// Notification grouping tag, same tag replaces previous notification
    /// instead of stacking.
    tag: String,
    /// URL the notification opens when tapped. Defaults to /irrigation.
    url: String,
}

#[derive(Clone)]
pub struct PushDispatcher {
    sender: mpsc::Sender<PushEvent>,
}

impl PushDispatcher {
    /// Best-effort emit. Drops the event if the channel is full or
    /// closed (push is fire-and-forget). Cheap; safe to call from the
    /// HA refresher's hot loop.
    pub fn emit(&self, ev: PushEvent) {
        if let Err(e) = self.sender.try_send(ev) {
            tracing::debug!("push channel full or closed, dropping event: {e}");
        }
    }
}

/// Spawn the background dispatcher task. Returns a sender handle the
/// HA refresher captures and emits into.
pub fn spawn_dispatcher(conn: Option<Arc<Mutex<Connection>>>) -> PushDispatcher {
    let (tx, mut rx) = mpsc::channel::<PushEvent>(64);
    let cfg = VapidConfig::from_env();

    tokio::spawn(async move {
        // Build the WebPush client once. isahc-client uses the same
        // rustls-tls toggle reqwest is built with.
        let client = match IsahcWebPushClient::new() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("push: failed to construct WebPushClient: {e}");
                return;
            }
        };

        if cfg.is_none() {
            tracing::warn!(
                "push: VAPID_* env vars missing, dispatcher running, but every event will be dropped silently. Generate keys + set env to enable."
            );
        }

        while let Some(ev) = rx.recv().await {
            let Some(conn) = conn.as_ref() else {
                tracing::debug!("push: history db not configured; dropping event");
                continue;
            };
            let Some(cfg) = cfg.as_ref() else {
                continue;
            };

            let payload = render_payload(&ev);
            let body = match serde_json::to_string(&payload) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("push: payload serialize failed: {e}");
                    continue;
                }
            };

            let subs = match store::list_all(conn.clone()).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("push: list subscriptions failed: {e}");
                    continue;
                }
            };

            for sub in subs {
                let result = send_one(&client, cfg, &sub, body.as_bytes()).await;
                match result {
                    Ok(()) => {
                        tracing::debug!(
                            "push: sent {} -> {}",
                            payload.tag,
                            mask_endpoint(&sub.endpoint)
                        );
                    }
                    Err(SendErr::Gone) | Err(SendErr::NotFound) => {
                        tracing::info!(
                            "push: pruning revoked endpoint {}",
                            mask_endpoint(&sub.endpoint)
                        );
                        let _ = store::delete_endpoint(conn.clone(), sub.endpoint).await;
                    }
                    Err(SendErr::Other(e)) => {
                        tracing::warn!(
                            "push: send to {} failed: {e}",
                            mask_endpoint(&sub.endpoint)
                        );
                    }
                }
            }
        }
        tracing::warn!("push: dispatcher channel closed");
    });

    PushDispatcher { sender: tx }
}

fn render_payload(ev: &PushEvent) -> PushPayload {
    match ev {
        PushEvent::ZoneStarted { name, slug } => PushPayload {
            title: format!("{name} started"),
            body: "Watering in progress.".to_string(),
            tag: format!("zone-{slug}"),
            url: format!("/irrigation/zone/{slug}"),
        },
        PushEvent::ZoneStopped {
            name,
            slug,
            duration_min,
        } => PushPayload {
            title: format!("{name} done"),
            body: format!("Ran for {duration_min} min."),
            tag: format!("zone-{slug}"),
            url: format!("/irrigation/zone/{slug}"),
        },
        PushEvent::DailyVerdict { verdict, reason } => {
            let title = match verdict.as_str() {
                "skip" => "Skipping today",
                "run_extended" => "Running extended today",
                _ => "Running today",
            }
            .to_string();
            let body = if reason.is_empty() {
                "Skip-check verdict ready.".to_string()
            } else {
                reason.clone()
            };
            PushPayload {
                title,
                body,
                tag: "daily-verdict".to_string(),
                url: "/irrigation".to_string(),
            }
        }
        PushEvent::SoilProbeFault {
            zone_name,
            zone_slug,
            since_epoch,
        } => {
            use chrono::TimeZone;
            let body = match since_epoch
                .and_then(|e| chrono::Local.timestamp_opt(e, 0).single())
            {
                Some(dt) => format!(
                    "{zone_name} has reported no valid reading since {}. The saturation gate is running without it.",
                    dt.format("%b %-d")
                ),
                None => format!(
                    "{zone_name} has never reported a valid reading. The saturation gate is running without it."
                ),
            };
            PushPayload {
                title: "Soil probe offline".to_string(),
                body,
                tag: format!("probe-fault-{zone_slug}"),
                url: "/sensors".to_string(),
            }
        }
    }
}

#[derive(Debug)]
enum SendErr {
    Gone,
    NotFound,
    Other(String),
}

async fn send_one(
    client: &IsahcWebPushClient,
    cfg: &VapidConfig,
    sub: &StoredSubscription,
    body: &[u8],
) -> Result<(), SendErr> {
    let info = SubscriptionInfo {
        endpoint: sub.endpoint.clone(),
        keys: SubscriptionKeys {
            p256dh: sub.p256dh.clone(),
            auth: sub.auth.clone(),
        },
    };

    // VapidSignatureBuilder::from_pem returns a PartialVapidSignatureBuilder
    // bound to this subscription. add_claim mutates in place and returns ().
    let mut sig_builder = match VapidSignatureBuilder::from_pem(cfg.private_pem.as_bytes(), &info) {
        Ok(b) => b,
        Err(e) => return Err(SendErr::Other(format!("vapid pem: {e}"))),
    };
    sig_builder.add_claim("sub", cfg.subject.clone());
    let sig = match sig_builder.build() {
        Ok(s) => s,
        Err(e) => return Err(SendErr::Other(format!("vapid sign: {e}"))),
    };

    let mut msg = WebPushMessageBuilder::new(&info);
    msg.set_payload(ContentEncoding::Aes128Gcm, body);
    msg.set_vapid_signature(sig);

    let built = match msg.build() {
        Ok(m) => m,
        Err(e) => return Err(SendErr::Other(format!("build: {e}"))),
    };

    match client.send(built).await {
        Ok(()) => Ok(()),
        Err(WebPushError::EndpointNotValid(_)) => Err(SendErr::Gone),
        Err(WebPushError::EndpointNotFound(_)) => Err(SendErr::NotFound),
        Err(e) => Err(SendErr::Other(format!("{e:?}"))),
    }
}

#[derive(Clone)]
struct VapidConfig {
    /// PEM-encoded EC P-256 private key.
    private_pem: String,
    /// "mailto:..." or HTTPS URL.
    subject: String,
    /// Base64url-encoded public key (raw or DER form, decided by the
    /// generator). Stored only for the /api/push/vapid-key endpoint.
    public_key_b64u: String,
}

impl VapidConfig {
    fn from_env() -> Option<Self> {
        let private_path = std::env::var("VAPID_PRIVATE_KEY_PATH").ok()?;
        let public_b64u = std::env::var("VAPID_PUBLIC_KEY").ok()?;
        // RFC 8292 allows an https: URL subject; the project URL is a
        // sane operator-neutral default when VAPID_SUBJECT is unset.
        let subject = std::env::var("VAPID_SUBJECT")
            .unwrap_or_else(|_| "https://github.com/silenthooligan/localsky".to_string());

        let private_pem = match std::fs::read_to_string(&private_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("push: VAPID private key at {private_path} unreadable: {e}");
                return None;
            }
        };

        // Sanity-check the public key is decodable. We don't need the
        // bytes here, just to surface a clearer log if it's malformed.
        if let Err(e) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&public_b64u) {
            tracing::warn!("push: VAPID_PUBLIC_KEY is not valid base64url: {e}");
            return None;
        }

        Some(Self {
            private_pem,
            subject,
            public_key_b64u: public_b64u,
        })
    }
}

/// Read-only public-key access for the API handler. Cached in a OnceLock
/// because env doesn't change at runtime.
pub fn vapid_public_key() -> Option<String> {
    use std::sync::OnceLock;
    static CELL: OnceLock<Option<String>> = OnceLock::new();
    CELL.get_or_init(|| VapidConfig::from_env().map(|c| c.public_key_b64u.clone()))
        .clone()
}

fn mask_endpoint(endpoint: &str) -> String {
    // Endpoints contain a unique device-bound id; logging the full URL
    // is a privacy footgun. Show only the host for debug.
    if let Some(rest) = endpoint.strip_prefix("https://") {
        if let Some(host) = rest.split('/').next() {
            return format!("https://{host}/...");
        }
    }
    endpoint.chars().take(40).collect::<String>() + "..."
}
