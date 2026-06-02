// Native Ollama provider. Talks to /api/chat directly (rather than the
// /v1/chat/completions OpenAI-compat shim) so we can use the `format`
// field for JSON-mode anomaly detection without translation layers.
//
// Endpoint reference: https://github.com/ollama/ollama/blob/main/docs/api.md

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::ports::llm_provider::{ChatOpts, HealthReport, LlmError, LlmProvider};

pub struct OllamaProvider {
    id: String,
    base_url: String,
    model: String,
    client: Client,
}

impl OllamaProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            base_url: base_url.into(),
            model: model.into(),
            client,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<&'static str>,
    options: ChatOptions,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize, Default)]
struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    message: Option<RespMessage>,
}

#[derive(Debug, Deserialize)]
struct RespMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Debug, Deserialize)]
struct TagEntry {
    #[serde(default)]
    name: String,
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(&self, system: &str, user: &str, opts: ChatOpts) -> Result<String, LlmError> {
        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: user,
                },
            ],
            stream: false,
            format: opts.json_mode.then_some("json"),
            options: ChatOptions {
                temperature: opts.temperature,
                num_predict: opts.max_tokens,
            },
        };

        let mut req = self.client.post(self.url("/api/chat")).json(&body);
        if let Some(t) = opts.timeout_s {
            req = req.timeout(Duration::from_secs(t as u64));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Remote(format!("HTTP {status}: {body}")));
        }
        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Parse(e.to_string()))?;
        parsed
            .message
            .and_then(|m| m.content)
            .ok_or_else(|| LlmError::Remote("empty message content".into()))
    }

    async fn health(&self) -> Result<HealthReport, LlmError> {
        let resp = self
            .client
            .get(self.url("/api/tags"))
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        if !resp.status().is_success() {
            return Ok(HealthReport {
                reachable: false,
                model_loaded: None,
                provider_version: None,
                last_error: Some(format!("HTTP {}", resp.status())),
            });
        }
        let tags: TagsResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Parse(e.to_string()))?;
        let loaded = tags
            .models
            .iter()
            .find(|m| m.name == self.model || m.name.starts_with(&self.model))
            .map(|m| m.name.clone());
        Ok(HealthReport {
            reachable: true,
            model_loaded: loaded,
            provider_version: None,
            last_error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_returned() {
        let p = OllamaProvider::new("ollama", "http://x.invalid", "llama3");
        assert_eq!(p.id(), "ollama");
    }

    #[test]
    fn url_trim_handles_trailing_slash() {
        let p = OllamaProvider::new("p", "http://x.invalid/", "m");
        assert_eq!(p.url("/api/chat"), "http://x.invalid/api/chat");
    }
}
