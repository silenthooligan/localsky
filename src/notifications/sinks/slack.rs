// Slack: one incoming-webhook POST per notification, `{"text": ...}`.

use async_trait::async_trait;

use crate::config::schema::SlackConfig;
use crate::notifications::fanout_headline;
use crate::ports::notification_sink::{NotificationError, NotificationEvent, NotificationSink};

pub struct Slack {
    cfg: SlackConfig,
    client: reqwest::Client,
}

impl Slack {
    pub fn new(cfg: SlackConfig) -> Self {
        Self {
            cfg,
            // Timeout + derived per-install User-Agent; `net::client` cannot
            // fail (it falls back to reqwest defaults), so no unwrap here.
            client: crate::net::client(std::time::Duration::from_secs(10)),
        }
    }
}

#[async_trait]
impl NotificationSink for Slack {
    fn id(&self) -> &str {
        "slack"
    }
    fn handles(&self, _event: &NotificationEvent) -> bool {
        true
    }
    async fn emit(&self, event: &NotificationEvent) -> Result<(), NotificationError> {
        let (title, body) = fanout_headline(event);
        let payload = serde_json::json!({ "text": format!("*{title}*\n{body}") });
        let resp = self
            .client
            .post(&self.cfg.webhook_url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| NotificationError::Transport(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(NotificationError::Transport(format!(
                "Slack answered {}",
                resp.status()
            )))
        }
    }
}
