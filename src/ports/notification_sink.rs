// NotificationSink port. Outbound delivery channels (Web Push, MQTT, ntfy,
// Slack, email, log). The dispatcher fan-outs PushEvent to every enabled
// sink; failures in one sink don't block others.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NotificationError {
    #[error("sink not configured")]
    NotConfigured,
    #[error("sink offline")]
    Offline,
    #[error("transport error: {0}")]
    Transport(String),
    #[error("permanent: {0}")]
    Permanent(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationEvent {
    ZoneStarted {
        zone_slug: String,
        controller_id: String,
        planned_duration_s: u32,
        at_epoch: i64,
    },
    ZoneStopped {
        zone_slug: String,
        controller_id: String,
        actual_duration_s: u32,
        at_epoch: i64,
    },
    DailyVerdict {
        date_local: String,
        verdict: String,
        reason: String,
        at_epoch: i64,
    },
    SkipExplained {
        date_local: String,
        reason: String,
        next_attempt_epoch: Option<i64>,
        at_epoch: i64,
    },
    SourceOffline {
        source_id: String,
        at_epoch: i64,
    },
    ControllerOffline {
        controller_id: String,
        at_epoch: i64,
    },
    AnomalyDetected {
        severity: String,
        description: String,
        at_epoch: i64,
    },
}

#[async_trait]
pub trait NotificationSink: Send + Sync {
    fn id(&self) -> &str;
    /// True when this sink is willing to handle a particular event class.
    /// Lets users gate verbosity per channel (push: zone events only,
    /// slack: verdicts + anomalies, log: everything).
    fn handles(&self, event: &NotificationEvent) -> bool;
    async fn emit(&self, event: &NotificationEvent) -> Result<(), NotificationError>;
}
