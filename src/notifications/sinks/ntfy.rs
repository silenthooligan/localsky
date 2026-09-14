// ntfy: one HTTP POST per notification to `<base_url>/<topic>`, the
// title in the Title header, an optional bearer token.

use async_trait::async_trait;

use crate::config::schema::NtfyConfig;
use crate::notifications::fanout_headline;
use crate::ports::notification_sink::{NotificationError, NotificationEvent, NotificationSink};

pub struct Ntfy {
    cfg: NtfyConfig,
    client: reqwest::Client,
}

impl Ntfy {
    pub fn new(cfg: NtfyConfig) -> Self {
        Self {
            cfg,
            // Shared outbound client: 10s timeout, derived per-install
            // User-Agent; `net::client` cannot fail (it falls back to reqwest
            // defaults), which is what `unwrap_or_default` used to cover.
            client: crate::net::client(std::time::Duration::from_secs(10)),
        }
    }

    fn url(&self) -> String {
        format!(
            "{}/{}",
            self.cfg.base_url.trim_end_matches('/'),
            self.cfg.topic.trim_matches('/')
        )
    }
}

#[async_trait]
impl NotificationSink for Ntfy {
    fn id(&self) -> &str {
        "ntfy"
    }
    fn handles(&self, _event: &NotificationEvent) -> bool {
        true
    }
    async fn emit(&self, event: &NotificationEvent) -> Result<(), NotificationError> {
        let (title, body) = fanout_headline(event);
        let mut req = self
            .client
            .post(self.url())
            .header("Title", title)
            .body(body);
        if let Some(t) = self.cfg.auth_token.as_ref().filter(|t| !t.is_empty()) {
            req = req.bearer_auth(t);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| NotificationError::Transport(e.to_string()))?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(NotificationError::Transport(format!(
                "ntfy answered {}",
                resp.status()
            )))
        }
    }
}
