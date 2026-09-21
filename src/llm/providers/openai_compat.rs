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

/// Default per-request budget. Matches the old persistent client; chat()
/// can still raise it per call via ChatOpts.timeout_s.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct OpenaiCompatProvider {
    id: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
}

impl OpenaiCompatProvider {
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    /// SSRF-hardened, IP-pinned client for a given endpoint URL. The LLM
    /// base_url is operator-supplied (and can be a private vLLM/Ollama on
    /// the LAN, which stays allowed); loopback/metadata/link-local/
    /// multicast are rejected, the resolved IP is pinned (anti DNS-
    /// rebinding) and redirects are disabled.
    async fn safe_client(&self, url: &str) -> Result<(Client, reqwest::Url), LlmError> {
        // Loopback-permitting probe client: a self-hosted Ollama / OpenAI-compatible
        // endpoint on 127.0.0.1 is the expected local-AI case, and the strict
        // safe_fetch client blocks loopback (so Auto could detect but never
        // reach a local provider). Same anti-SSRF hardening otherwise.
        crate::net::build_llm_probe_client(url, DEFAULT_TIMEOUT)
            .await
            .map_err(|e| {
                LlmError::transport(crate::net::source_failure::from_safe(
                    &e,
                    "OpenAI-compatible provider initialize request",
                ))
            })
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
            temperature: opts.temperature,
            max_tokens: opts.max_tokens,
            response_format: opts.json_mode.then_some(ResponseFormat {
                kind: "json_object",
            }),
            stream: false,
        };

        let endpoint = self.url("/v1/chat/completions");
        let (client, safe_url) = self.safe_client(&endpoint).await?;
        let mut req = client.post(safe_url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        if let Some(t) = opts.timeout_s {
            req = req.timeout(Duration::from_secs(t as u64));
        }
        let resp = req.send().await.map_err(|e| {
            LlmError::transport(crate::net::source_failure::from_reqwest(
                &e,
                "OpenAI-compatible provider chat",
            ))
        })?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(LlmError::http(
                status.as_u16(),
                Some(crate::net::source_failure::response_format(&resp)),
                "OpenAI-compatible provider chat",
            ));
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(LlmError::http(
                429,
                Some(crate::net::source_failure::response_format(&resp)),
                "OpenAI-compatible provider chat",
            ));
        }
        if !status.is_success() {
            // Status only: don't reflect the upstream body (SSRF exfil
            // channel when the URL was steered to an unintended target).
            return Err(LlmError::http(
                status.as_u16(),
                Some(crate::net::source_failure::response_format(&resp)),
                "OpenAI-compatible provider chat",
            ));
        }
        let parsed: ChatResponse = resp.json().await.map_err(|e| {
            LlmError::parse(crate::net::source_failure::from_reqwest(
                &e,
                "OpenAI-compatible provider chat",
            ))
        })?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.and_then(|m| m.content))
            .ok_or_else(|| {
                LlmError::remote(
                    crate::failure::Failure::new(
                        crate::failure::FailureCode::MissingField,
                        "OpenAI-compatible provider chat response",
                    )
                    .with_field("message.content"),
                )
            })
    }

    async fn health(&self) -> Result<HealthReport, LlmError> {
        // /v1/models for OpenAI; many compatible servers also expose it.
        let endpoint = self.url("/v1/models");
        let (client, safe_url) = self.safe_client(&endpoint).await?;
        let mut req = client.get(safe_url);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            LlmError::transport(crate::net::source_failure::from_reqwest(
                &e,
                "OpenAI-compatible provider health",
            ))
        })?;
        let reachable = resp.status().is_success();
        let diagnostic = (!reachable).then(|| {
            crate::failure::FailureRecord::now(crate::failure::Failure::http(
                resp.status().as_u16(),
                Some(crate::net::source_failure::response_format(&resp)),
                "OpenAI-compatible provider health",
            ))
        });
        Ok(HealthReport {
            reachable,
            model_loaded: reachable.then(|| self.model.clone()),
            provider_version: None,
            last_error: diagnostic.as_ref().map(|d| d.failure.to_string()),
            diagnostic,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn provider_http_failure_keeps_status_and_does_not_expose_credentials() {
        use wiremock::{matchers::any, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(403).set_body_string("secret echoed credential"))
            .mount(&server)
            .await;
        let provider = OpenaiCompatProvider::new(
            "test",
            server.uri(),
            "model",
            Some("secret request credential".into()),
        );
        let error = provider
            .chat("system", "user", ChatOpts::default())
            .await
            .unwrap_err();
        assert_eq!(error.diagnostic().http_status, Some(403));
        assert!(!error.to_string().contains("secret"));
        let health = provider.health().await.unwrap();
        assert!(!health.reachable);
        assert!(health.model_loaded.is_none());
        assert_eq!(health.diagnostic.unwrap().failure.http_status, Some(403));
    }

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
