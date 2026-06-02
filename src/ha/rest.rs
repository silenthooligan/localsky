// Thin HA REST client. Two operations for now: bulk-fetch states and
// call a service. Authentication is a Bearer token from $HA_TOKEN.
// The HA URL is read from $HA_URL; required for any HA integration.

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone)]
pub struct HaClient {
    client: Client,
    base_url: String,
    token: String,
}

impl HaClient {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("HA_TOKEN")
            .or_else(|_| std::env::var("HA_LONG_LIVED_TOKEN"))
            .context(
                "HA_TOKEN (or HA_LONG_LIVED_TOKEN) env var is required for the irrigation backend",
            )?;
        let base_url =
            std::env::var("HA_URL").context("HA_URL env var is required when HA is configured")?;
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .user_agent("localsky/irrigation")
            .build()?;
        Ok(Self {
            client,
            base_url,
            token,
        })
    }

    /// Bulk read of every entity. Returns the raw JSON array. Parsed
    /// into a HashMap<entity_id, Value> by callers so each component
    /// reads only what it needs.
    pub async fn states(&self) -> Result<Vec<Value>> {
        let url = format!("{}/api/states", self.base_url);
        let resp = self
            .client
            .get(&url)
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        if !resp.status().is_success() {
            return Err(anyhow!("GET /api/states returned {}", resp.status()));
        }
        let arr: Vec<Value> = resp.json().await.context("decode /api/states JSON")?;
        Ok(arr)
    }

    /// Call `domain.service` with a JSON body. Used for zone runs,
    /// stops, threshold updates, etc. `data` should be something like
    /// `serde_json::json!({ "entity_id": "..." })`.
    pub async fn call_service<T: Serialize>(
        &self,
        domain: &str,
        service: &str,
        data: &T,
    ) -> Result<()> {
        let url = format!("{}/api/services/{}/{}", self.base_url, domain, service);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.token)
            .json(data)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "POST /api/services/{domain}/{service} returned {status}: {body}"
            ));
        }
        Ok(())
    }
}
