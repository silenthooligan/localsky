// Push dispatcher. A small mpsc channel + background task that drains
// PushEvents emitted by the HA refresher and fans them out to all
// currently-subscribed browsers via Web Push (VAPID-signed).
//
// Failure mode: web-push errors with a 410 Gone (or 404) when a
// subscription has been revoked by the browser. We delete those rows
// immediately so the subscription list stays clean; other transient
// errors are logged and the row is preserved for the next attempt.
//
// VAPID config is resolved at delivery, CONFIG FIRST then env:
// `notifications.web_push` in /data/localsky.toml (what the wizard's Web
// Push toggle writes, with an auto-generated keypair) wins; a legacy
// VAPID_* env trio is the fallback for v0.1 continuity deployments. Missing
// both = browser delivery is unavailable; shared channels still work.

use crate::notification_preferences::EventKind;
use crate::push::store::{self, StoredSubscription};
use anyhow::Result;
use base64::Engine;
use chrono::Timelike;
use futures::FutureExt;
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
    ZoneStarted {
        name: String,
        slug: String,
        stop: Option<crate::controllers::notification_runs::NotificationRun>,
    },
    /// A zone just transitioned from running to idle. `duration_min` is
    /// the run length in minutes (rounded).
    ZoneStopped {
        name: String,
        slug: String,
        duration_min: u32,
    },
    /// Optional outlook admitted once per local day at the configured time.
    /// The delivery claim survives restarts. `verdict` is the current outlook.
    DailyVerdict { verdict: String, reason: String },
    /// A durable, once-per-local-day outlook for exactly one device.
    DeviceOutlook {
        endpoint: String,
        date_local: String,
        reason: String,
    },
    /// Measured weather crossing a threshold; never an official warning.
    Weather { kind: EventKind, body: String },
    /// A configured soil probe stopped producing valid readings (see
    /// the refresher's probe-fault detection). Sent once per probe per
    /// process lifetime. `since_epoch` is the last valid reading; None
    /// when the channel never produced one.
    SoilProbeFault {
        zone_name: String,
        zone_slug: String,
        since_epoch: Option<i64>,
    },
    /// A zone's soil probe was QUARANTINED: it reported a wild outlier vs
    /// its siblings (or went offline) so the engine inferred this zone's
    /// soil from the trustworthy neighbors instead of trusting the probe.
    /// Distinct from `SoilProbeFault` (which is a probe that stopped
    /// producing readings entirely): here the probe IS producing values,
    /// they're just untrustworthy. Sent once per zone per quarantine
    /// episode (see the refresher's edge-latch). `raw_pct` is the suspect
    /// reading the probe reported; `None` when the zone was offline-inferred.
    /// `yard_pct` is the trustworthy sibling median the engine used instead.
    SoilProbeSuspect {
        zone_name: String,
        zone_slug: String,
        raw_pct: Option<f64>,
        yard_pct: f64,
    },
    /// The weekly tuning report generated with at least one actionable
    /// recommendation. Emitted by scheduler::tuning_report at most once
    /// per 7 local days (persisted dedupe, M0014). Deep-links to the
    /// zones view where the recommendations and their Apply live; the
    /// service worker supports no action buttons, so the notification
    /// itself is link-only.
    TuningReportReady { recommendation_count: usize },
    /// A zone's per-run limit was raised past the 60 minute default by a
    /// config write (settings editor save or tuning Apply, both behind
    /// the UI confirmation). Informational safety notice: every
    /// subscribed device learns the line was crossed; nothing persists
    /// here (the config snapshot trail is the audit). Emitted by
    /// runtime::apply_runtime_config AFTER a successful save only.
    RunCapRaised {
        zone_name: String,
        zone_slug: String,
        minutes: u32,
    },
    /// A Home Assistant deployment planned a run for zones whose weekly
    /// target is inferred from the name rather than set. Sent once per
    /// process, the first tick that plans one: those installs dispatched
    /// nothing before 0.7.22, and an owner who never opens the UI has no
    /// other way to hear that the valves are about to open.
    InferredTargetsPlanned { zones: Vec<String> },
    /// A valve command was refused or failed at the controller.
    DispatchFailed {
        zone_name: String,
        zone_slug: String,
        controller_id: String,
        error: String,
    },
    /// A controller could not be reached when it was needed.
    ControllerOffline {
        controller_id: String,
        error: String,
    },
    /// A valve is past its shutoff deadline and LocalSky could not
    /// confirm it closed. The most dangerous state the product has.
    ValveUnclosed {
        zone_name: String,
        zone_slug: String,
        controller_id: String,
        overdue_s: i64,
    },
    /// The flow meter reads water moving while no zone is commanded on.
    FlowWithoutCommand { gpm: f64 },
    /// A configured weather source has stopped reporting.
    SourceOffline { source_id: String, silent_s: i64 },
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

fn allowed_for_device(
    ev: &PushEvent,
    sub: &StoredSubscription,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    let minute = now.hour() * 60 + now.minute();
    let kind = match ev {
        PushEvent::ZoneStarted { .. } => EventKind::WateringStarted,
        PushEvent::ZoneStopped { .. } => EventKind::WateringFinished,
        PushEvent::DispatchFailed { .. } | PushEvent::ControllerOffline { .. } => {
            EventKind::WateringProblem
        }
        PushEvent::ValveUnclosed { .. } | PushEvent::FlowWithoutCommand { .. } => {
            EventKind::UrgentEquipment
        }
        PushEvent::SoilProbeFault { .. } | PushEvent::SoilProbeSuspect { .. } => {
            EventKind::SoilSensor
        }
        PushEvent::SourceOffline { .. } => EventKind::WeatherSource,
        PushEvent::TuningReportReady { .. } => EventKind::Tuning,
        PushEvent::RunCapRaised { .. } | PushEvent::InferredTargetsPlanned { .. } => {
            EventKind::Configuration
        }
        PushEvent::Weather { kind, .. } => *kind,
        PushEvent::DailyVerdict { .. } => return false,
        PushEvent::DeviceOutlook {
            endpoint,
            date_local,
            ..
        } => {
            return endpoint == &sub.endpoint
                && date_local == &now.format("%Y-%m-%d").to_string()
                && sub.preferences.outlook_due(minute)
        }
    };
    sub.preferences.allows(kind, minute)
}

#[derive(Clone)]
pub struct PushDispatcher {
    sender: mpsc::Sender<PushEvent>,
}

impl PushDispatcher {
    /// A dispatcher whose events land in the returned receiver instead of
    /// a browser, for tests that need to see what was pushed.
    #[cfg(test)]
    pub fn capturing() -> (Self, mpsc::Receiver<PushEvent>) {
        let (sender, rx) = mpsc::channel(64);
        (Self { sender }, rx)
    }

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

        // Restart-on-panic supervisor. A panic while handling one event
        // (payload serialization, webpush internals) would otherwise kill the
        // dispatcher for the whole process lifetime, silently dropping every
        // later notification. catch_unwind turns a panic into a logged restart
        // of the recv loop; `rx` is borrowed (not moved) by the inner future,
        // so it survives the unwind. The channel-closed path breaks out cleanly.
        loop {
            let outcome = std::panic::AssertUnwindSafe(async {
                while let Some(ev) = rx.recv().await {
                    let preferences = sinks_config();
                    // Recheck at delivery: disabling summaries or leaving the
                    // scheduled window also suppresses anything in the queue.
                    if matches!(ev, PushEvent::DailyVerdict { .. }) {
                        use chrono::Timelike;
                        let now = crate::timeutil::now_local();
                        if !preferences
                            .daily_outlook
                            .due_at(now.hour() * 60 + now.minute())
                        {
                            continue;
                        }
                    }
                    // Every enabled sink first, read from the on-disk config
                    // at delivery so a saved ntfy topic or Slack webhook
                    // takes effect without a restart. Events are rare; the
                    // read is cheap.
                    if let Some(sink_event) =
                        crate::notifications::from_push_event(&ev, chrono::Utc::now().timestamp())
                    {
                        let fanout = crate::notifications::Fanout::from_config(&preferences);
                        if !fanout.is_empty() {
                            fanout.deliver(&sink_event).await;
                        }
                    }
                    let Some(conn) = conn.as_ref() else {
                        tracing::debug!("push: history db not configured; dropping event");
                        continue;
                    };
                    if !preferences.web_push_enabled || matches!(ev, PushEvent::DailyVerdict { .. })
                    {
                        continue;
                    }
                    let Some(cfg) = VapidConfig::from_config_or_env() else {
                        continue;
                    };

                    let payload = render_payload(&ev);
                    let mut message =
                        serde_json::to_value(&payload).expect("serializable push payload");
                    if let PushEvent::ZoneStarted {
                        stop: Some(stop), ..
                    } = &ev
                    {
                        message["stop"] = serde_json::json!(stop);
                    }
                    let body = match serde_json::to_string(&message) {
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
                        // Evaluate just before each send, including the date and
                        // summary window, so queued work cannot escape quiet hours.
                        if !allowed_for_device(&ev, &sub, crate::timeutil::now_local()) {
                            continue;
                        }
                        let result = send_one(&client, &cfg, &sub, body.as_bytes()).await;
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
            })
            .catch_unwind()
            .await;
            match outcome {
                Ok(()) => break,
                Err(_) => {
                    tracing::error!("push: dispatcher task panicked; restarting recv loop");
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        tracing::warn!("push: dispatcher channel closed");
    });

    PushDispatcher { sender: tx }
}

/// The notifications block of the on-disk config, the same way the VAPID
/// keypair is read: the dispatcher is spawned before the boot config is
/// loaded and a saved change should apply without a restart.
pub(crate) fn sinks_config() -> crate::config::schema::Notifications {
    let config_path =
        std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
    crate::config::loader::load_from_path(std::path::Path::new(&config_path))
        .map(|c| c.notifications)
        .unwrap_or_default()
}

fn render_payload(ev: &PushEvent) -> PushPayload {
    match ev {
        PushEvent::DeviceOutlook { reason, .. } => PushPayload {
            title: "Today's watering outlook".into(), body: reason.clone(),
            tag: "daily-verdict".into(), url: "/irrigation".into(),
        },
        PushEvent::Weather { kind, body } => PushPayload {
            title: kind.label().into(), body: body.clone(),
            tag: format!("weather-{kind:?}"), url: "/".into(),
        },
        PushEvent::DispatchFailed {
            zone_name,
            zone_slug,
            controller_id,
            error,
        } => PushPayload {
            title: format!("{zone_name} did not start"),
            body: format!("{controller_id} refused the command: {error}. Nothing watered."),
            tag: format!("dispatch-{zone_slug}"),
            url: format!("/zones/{zone_slug}"),
        },
        PushEvent::ControllerOffline {
            controller_id,
            error,
        } => PushPayload {
            title: format!("{controller_id} is not answering"),
            body: format!("{error}. Watering that needs this source cannot start until it responds."),
            tag: format!("controller-{controller_id}"),
            url: "/settings/devices".to_string(),
        },
        PushEvent::ValveUnclosed {
            zone_name,
            zone_slug,
            controller_id,
            overdue_s,
        } => PushPayload {
            title: format!("{zone_name} may still be open"),
            body: format!(
                "Its shutoff was due {} min ago and {controller_id} has not confirmed closing it. LocalSky keeps retrying; check the valve.",
                (overdue_s / 60).max(1)
            ),
            tag: format!("unclosed-{zone_slug}"),
            url: format!("/zones/{zone_slug}"),
        },
        PushEvent::SourceOffline { source_id, silent_s } => PushPayload {
            title: format!("{source_id} went quiet"),
            body: format!(
                "No observations for {} min. The engine falls back to the next source in priority until it returns.",
                silent_s / 60
            ),
            tag: format!("source-{source_id}"),
            url: "/settings/devices".to_string(),
        },
        PushEvent::FlowWithoutCommand { gpm } => PushPayload {
            title: "Water is flowing with nothing running".to_string(),
            body: format!(
                "The flow meter reads {gpm:.1} gal/min while no zone is commanded on. A stuck valve or a leak."
            ),
            tag: "flow-without-command".to_string(),
            url: "/".to_string(),
        },
        PushEvent::ZoneStarted { name, slug, .. } => PushPayload {
            title: format!("{name} started"),
            body: "Watering in progress.".to_string(),
            tag: format!("zone-{slug}"),
            url: "/irrigation".to_string(),
        },
        PushEvent::ZoneStopped {
            name,
            slug,
            duration_min,
        } => PushPayload {
            title: format!("{name} done"),
            body: crate::notifications::ran_for(*duration_min),
            tag: format!("zone-{slug}"),
            url: format!("/zones/{slug}"),
        },
        PushEvent::DailyVerdict { reason, .. } => {
            let title = "Today's watering outlook".to_string();
            let body = if reason.is_empty() {
                "See today's zone plans in LocalSky.".to_string()
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
            // #3 (TZ correctness): render the "since {date}" in the deployment's
            // CONFIGURED timezone, not chrono::Local (the container TZ, which may
            // be UTC and roll the calendar date over wrong). The configured-TZ
            // offset is read from timeutil (process-wide, set at boot from
            // cfg.deployment.timezone); applied to the epoch it gives the right
            // wall-clock calendar day. This row is date-only (no clock time), so
            // the offset for "now" is correct for the displayed date.
            let tz_offset = *crate::timeutil::now_local().offset();
            let body = match since_epoch
                .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
                .map(|dt| dt.with_timezone(&tz_offset))
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
        PushEvent::SoilProbeSuspect {
            zone_name,
            zone_slug,
            raw_pct,
            yard_pct,
        } => {
            let body = match raw_pct {
                Some(raw) => format!(
                    "{zone_name} reads {raw:.0}% vs the yard's {yard_pct:.0}% - watering decided from neighbors. Check the probe."
                ),
                None => format!(
                    "{zone_name}'s probe is offline; watering decided from the yard's {yard_pct:.0}% neighbors. Check the probe."
                ),
            };
            PushPayload {
                title: "Soil probe suspect".to_string(),
                body,
                tag: format!("probe-suspect-{zone_slug}"),
                url: "/sensors".to_string(),
            }
        }
        PushEvent::TuningReportReady {
            recommendation_count,
        } => {
            let body = if *recommendation_count == 1 {
                "1 zone has a tuning suggestion this week. Open it to review and apply.".to_string()
            } else {
                format!(
                    "{recommendation_count} zones have tuning suggestions this week. Open them \
                     to review and apply."
                )
            };
            PushPayload {
                title: "Tuning report ready".to_string(),
                body,
                tag: "tuning-report".to_string(),
                url: "/zones".to_string(),
            }
        }
        PushEvent::RunCapRaised {
            zone_name,
            zone_slug,
            minutes,
        } => PushPayload {
            title: format!("{zone_name} run limit raised to {minutes} minutes"),
            body: format!(
                "A single watering can now run up to {minutes} minutes. Cycle and soak \
                 still splits long runs to limit runoff."
            ),
            // One tag per zone: a repeat raise replaces the previous
            // notification instead of stacking.
            tag: format!("cap-raised-{zone_slug}"),
            url: format!("/zones/{zone_slug}"),
        },
        PushEvent::InferredTargetsPlanned { zones } => {
            let n = zones.len();
            let title = if n == 1 {
                "1 zone waters on an inferred target".to_string()
            } else {
                format!("{n} zones water on inferred targets")
            };
            PushPayload {
                title,
                body: format!(
                    "{}: the weekly target comes from the zone name because none is set. Set \
                     Weekly target and Sessions per week under Settings, then Zones.",
                    zones.join(", ")
                ),
                tag: "inferred-targets".to_string(),
                url: "/settings/zones".to_string(),
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
    // Never replay stale activity or a daytime alert in quiet hours when a
    // phone reconnects. The app retains current status and run history.
    msg.set_ttl(0);
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

/// Preserve a working legacy environment identity when the wizard adopts an
/// existing installation. Only the public key and private-file path enter the
/// draft; the private PEM is never returned to the browser.
pub(crate) fn environment_key_config() -> Option<crate::config::schema::WebPushConfig> {
    let existing = VapidConfig::from_env()?;
    Some(crate::config::schema::WebPushConfig {
        vapid_public: existing.public_key_b64u,
        vapid_private_path: std::env::var("VAPID_PRIVATE_KEY_PATH").ok()?,
        vapid_subject: existing.subject,
    })
}

impl VapidConfig {
    /// Resolve the active VAPID config CONFIG-FIRST: the wizard's Web Push
    /// toggle writes `notifications.web_push` (vapid_public + a path to the
    /// generated private PEM + subject) into /data/localsky.toml, which the
    /// runtime now actually reads. A legacy VAPID_* env trio is the fallback
    /// (v0.1 continuity); env never silently overrides a configured keypair.
    fn from_config_or_env() -> Option<Self> {
        Self::from_config().or_else(Self::from_env)
    }

    /// Build from `notifications.web_push` in the on-disk config. Reads the
    /// config off CONFIG_PATH directly (the dispatcher is spawned before the
    /// boot config snapshot is loaded in main.rs, and its signature is fixed),
    /// the same pattern other ssr sources use for their sidecars. None when no
    /// config exists, push is unconfigured, or the keypair fields are blank.
    fn from_config() -> Option<Self> {
        let config_path =
            std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
        let cfg = crate::config::loader::load_from_path(std::path::Path::new(&config_path)).ok()?;
        let wp = cfg.notifications.web_push.as_ref()?;
        if wp.vapid_public.trim().is_empty() || wp.vapid_private_path.trim().is_empty() {
            return None;
        }
        let private_pem = match std::fs::read_to_string(&wp.vapid_private_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "push: VAPID private key at {} (from config) unreadable: {e}",
                    wp.vapid_private_path
                );
                return None;
            }
        };
        if let Err(e) =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(wp.vapid_public.trim())
        {
            tracing::warn!("push: config web_push.vapid_public is not valid base64url: {e}");
            return None;
        }
        // RFC 8292 allows an https: subject; default when the operator left it
        // blank (the wizard fills a default, so this is belt-and-suspenders).
        let subject = if wp.vapid_subject.trim().is_empty() {
            "https://github.com/silenthooligan/localsky".to_string()
        } else {
            wp.vapid_subject.clone()
        };
        Some(Self {
            private_pem,
            subject,
            public_key_b64u: wp.vapid_public.trim().to_string(),
        })
    }

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

/// Generate a fresh VAPID keypair, write the PKCS#8 PEM private key to
/// `private_path` (0600, parent dirs created), and return the base64url
/// (URL-safe, no pad) encoding of the RAW uncompressed 65-byte P-256 public
/// point, which is exactly the `applicationServerKey` the browser's
/// PushManager.subscribe() expects and what `web-push`'s own
/// `PartialVapidSignatureBuilder::get_public_key()` produces for this key.
///
/// Pure RustCrypto (p256 + the rand OsRng already in the ssr tree), so it
/// adds no new dependency and pulls no C crypto backend. ssr-only; never
/// runs in the WASM bundle. Called by the wizard apply path when the
/// operator enables Web Push without supplying keys.
pub fn generate_vapid_keypair(
    private_path: &std::path::Path,
) -> std::result::Result<String, String> {
    use p256::elliptic_curve::sec1::ToSec1Point;
    use p256::elliptic_curve::Generate;
    use p256::pkcs8::{EncodePrivateKey, LineEnding};
    use p256::SecretKey;

    // try_generate draws directly from OS entropy (p256 getrandom feature);
    // fallible by design, so surface the error instead of unwrapping.
    let secret = SecretKey::try_generate().map_err(|e| format!("generate p256 key: {e}"))?;

    // Private key as PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`); web-push's
    // from_pem accepts exactly this (or SEC1).
    let pem = secret
        .to_pkcs8_pem(LineEnding::LF)
        .map_err(|e| format!("encode pkcs8 pem: {e}"))?;

    // Raw uncompressed public point (0x04 || X || Y), base64url no-pad.
    let public_point = secret.public_key().to_sec1_point(false);
    let public_b64u =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(public_point.as_bytes());

    if let Some(parent) = private_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create key dir: {e}"))?;
    }
    std::fs::write(private_path, pem.as_bytes()).map_err(|e| format!("write key: {e}"))?;
    // Best-effort tighten perms to owner-only on unix; the key is a secret.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(private_path, std::fs::Permissions::from_mode(0o600));
    }

    Ok(public_b64u)
}

/// The public key browsers subscribe against, resolved once at boot for
/// the push API's state (the keypair does not change at runtime).
pub fn vapid_public_key() -> Option<String> {
    VapidConfig::from_config_or_env().map(|c| c.public_key_b64u.clone())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_filters_target_device_date_quiet_hours_and_explicit_opt_out() {
        use chrono::TimeZone;
        let tz = chrono::FixedOffset::west_opt(4 * 3600).unwrap();
        let morning = tz.with_ymd_and_hms(2026, 10, 5, 9, 0, 0).unwrap();
        let midnight = tz.with_ymd_and_hms(2026, 10, 5, 0, 0, 0).unwrap();
        let mut sub = StoredSubscription {
            endpoint: "phone".into(),
            p256dh: "key".into(),
            auth: "auth".into(),
            preferences: Default::default(),
        };
        let event = PushEvent::DeviceOutlook {
            endpoint: "phone".into(),
            date_local: "2026-10-05".into(),
            reason: "plan".into(),
        };
        assert!(!allowed_for_device(&event, &sub, morning));
        sub.preferences.daily_outlook.enabled = true;
        assert!(allowed_for_device(&event, &sub, morning));
        assert!(!allowed_for_device(&event, &sub, midnight));
        assert!(!allowed_for_device(
            &event,
            &sub,
            morning + chrono::Duration::days(1)
        ));
        sub.endpoint = "tablet".into();
        assert!(!allowed_for_device(&event, &sub, morning));
        assert!(!allowed_for_device(
            &PushEvent::DailyVerdict {
                verdict: "run".into(),
                reason: "plan".into()
            },
            &sub,
            morning
        ));
        let urgent = PushEvent::FlowWithoutCommand { gpm: 1.0 };
        assert!(allowed_for_device(&urgent, &sub, midnight));
        sub.preferences.quiet_hours.allow_urgent = false;
        assert!(!allowed_for_device(&urgent, &sub, midnight));
        sub.preferences.enabled = false;
        assert!(!allowed_for_device(&urgent, &sub, morning));
        assert!(crate::notifications::from_push_event(&event, morning.timestamp()).is_none());
    }

    #[test]
    fn inferred_targets_payload_names_the_zones_and_the_fields() {
        let p = render_payload(&PushEvent::InferredTargetsPlanned {
            zones: vec!["Back Yard".into(), "Side Bed".into()],
        });
        assert_eq!(p.title, "2 zones water on inferred targets");
        assert!(p.body.starts_with("Back Yard, Side Bed:"), "{}", p.body);
        assert!(
            p.body.contains("Weekly target and Sessions per week"),
            "{}",
            p.body
        );
        assert_eq!(p.url, "/settings/zones");
        let one = render_payload(&PushEvent::InferredTargetsPlanned {
            zones: vec!["Back Yard".into()],
        });
        assert_eq!(one.title, "1 zone waters on an inferred target");
    }

    #[test]
    fn run_cap_raised_payload_names_the_zone_and_deep_links_it() {
        let p = render_payload(&PushEvent::RunCapRaised {
            zone_name: "Back Yard".into(),
            zone_slug: "back_yard".into(),
            minutes: 116,
        });
        assert_eq!(p.title, "Back Yard run limit raised to 116 minutes");
        assert!(p.body.contains("116 minutes"), "{}", p.body);
        assert_eq!(
            p.tag, "cap-raised-back_yard",
            "per-zone tag so repeats replace, never stack"
        );
        assert_eq!(p.url, "/zones/back_yard");
    }

    #[test]
    fn generate_vapid_keypair_writes_loadable_pem_and_browser_public_key() {
        let dir = std::env::temp_dir().join(format!(
            "ls-vapid-gen-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        let priv_path = dir.join("keys").join("vapid-private.pem");

        let public_b64u = generate_vapid_keypair(&priv_path).expect("keypair generation");

        // The PEM exists, is PKCS#8, and web-push can parse it (the exact call
        // send_one makes), proving the generated key is sign-ready.
        let pem = std::fs::read_to_string(&priv_path).expect("private pem written");
        assert!(
            pem.contains("BEGIN PRIVATE KEY"),
            "private key must be PKCS#8 PEM"
        );
        let info = SubscriptionInfo {
            endpoint: "https://example.com/ep".into(),
            keys: SubscriptionKeys {
                p256dh: "BPublicKeyNotChecked".into(),
                auth: "authNotChecked".into(),
            },
        };
        // web-push must accept the generated PKCS#8 PEM as a signing key.
        VapidSignatureBuilder::from_pem(pem.as_bytes(), &info)
            .expect("web-push must accept the generated PKCS#8 PEM");

        // The base64url we return (the applicationServerKey the browser
        // subscribes with) must be a valid raw uncompressed P-256 point.
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&public_b64u)
            .expect("returned public key must be valid base64url");
        assert_eq!(bytes.len(), 65, "uncompressed P-256 point is 65 bytes");
        assert_eq!(bytes[0], 0x04, "uncompressed point prefix");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finished_push_names_a_short_run_honestly() {
        let short = render_payload(&PushEvent::ZoneStopped {
            name: "Back Yard Shrubs".into(),
            slug: "back_yard_shrubs".into(),
            duration_min: 0,
        });
        assert_eq!(short.title, "Back Yard Shrubs done");
        assert_eq!(short.body, "Ran for less than a minute.");
    }
}
