// OpenAI-compatible chat provider. Covers OpenAI, Anthropic-compat
// shims, vLLM, LM Studio, llama.cpp's /v1 endpoint, and any third-party
// gateway that speaks /v1/chat/completions.
//
// Implements the LlmProvider port; the advisor talks via this trait so
// switching backends is a config edit, not a code change.

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::ports::llm_provider::{ChatOpts, HealthReport, LlmError, LlmProvider};

pub struct OpenaiCompatProvider {
    id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: Client,
}

impl OpenaiCompatProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client construction");
        Self {
            id: id.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<RespMessage>,
}

#[derive(Debug, Deserialize)]
struct RespMessage {
    #[serde(default)]
    content: Option<String>,
}

#[async_trait]
impl LlmProvider for OpenaiCompatProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(
        &self,
        system: &str,
        user: &str,
        opts: ChatOpts,
    ) -> Result<String, LlmError> {
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
            temperature: opts.temperature,
            max_tokens: opts.max_tokens,
            response_format: opts.json_mode.then_some(ResponseFormat { kind: "json_object" }),
            stream: false,
        };

        let mut req = self
            .client
            .post(self.url("/v1/chat/completions"))
            .json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        if let Some(t) = opts.timeout_s {
            req = req.timeout(Duration::from_secs(t as u64));
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(LlmError::AuthFailed);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(LlmError::RateLimited);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LlmError::Remote(format!("HTTP {status}: {body}")));
        }
        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| LlmError::Parse(e.to_string()))?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.and_then(|m| m.content))
            .ok_or_else(|| LlmError::Remote("empty choices/content".into()))
    }

    async fn health(&self) -> Result<HealthReport, LlmError> {
        // /v1/models for OpenAI; many compatible servers also expose it.
        let mut req = self.client.get(self.url("/v1/models"));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| LlmError::Transport(e.to_string()))?;
        let reachable = resp.status().is_success();
        Ok(HealthReport {
            reachable,
            model_loaded: Some(self.model.clone()),
            provider_version: None,
            last_error: if reachable {
                None
            } else {
                Some(format!("HTTP {}", resp.status()))
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_trim_handles_trailing_slash() {
        let p = OpenaiCompatProvider::new("p", "http://x.invalid/", "m", None);
        assert_eq!(p.url("/v1/foo"), "http://x.invalid/v1/foo");
    }

    #[test]
    fn id_returned() {
        let p = OpenaiCompatProvider::new("p1", "http://x.invalid", "m", None);
        assert_eq!(p.id(), "p1");
    }
}
