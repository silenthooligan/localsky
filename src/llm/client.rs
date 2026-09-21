// Thin OpenAI-compatible chat-completions client. Legacy v0.1 single-
// endpoint shape; v2 callers prefer the LlmProvider trait + the
// providers in src/llm/providers/. All errors are caught and returned
// as `ClientError` so the advisor layer can degrade gracefully:
// never panics, never blocks irrigation.

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("LLM disabled by configuration")]
    Disabled,
    #[error("{0}")]
    Failed(#[source] Box<crate::failure::Failure>),
}
impl ClientError {
    pub fn diagnostic(&self) -> crate::failure::Failure {
        match self {
            Self::Failed(failure) => (**failure).clone(),
            Self::Disabled => crate::failure::Failure::new(
                crate::failure::FailureCode::ProviderOffline,
                "LLM advisor disabled",
            ),
        }
    }
}

/// Minimal OpenAI-compatible request body. We only need messages +
/// max_tokens + temperature; the rest of the OpenAI spec stays at the
/// upstream's default.
#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageOwned,
}

#[derive(Debug, Deserialize)]
struct ChatMessageOwned {
    content: String,
}

/// Wraps an HTTP client + base URL + model name. Cheap to clone.
#[derive(Clone)]
pub struct LlmClient {
    http: Client,
    base_url: String,
    model: String,
    /// Optional bearer token for an authenticated OpenAI-compatible endpoint
    /// (e.g. OpenAI proper, or a keyed gateway). Sent as `Authorization:
    /// Bearer <key>`; None for a keyless local endpoint (Ollama, llama.cpp).
    api_key: Option<String>,
    disabled: bool,
}

impl LlmClient {
    /// The advisor keeps its own purpose label as User-Agent rather than the
    /// derived per-install identity; `net::client_with` cannot fail (it falls
    /// back to reqwest defaults), so the `Result` on `from_env` / `from_config`
    /// stays only because `llm::advisor` matches on it.
    fn build_http() -> Client {
        crate::net::client_with(Duration::from_secs(20), "localsky/advisor")
    }

    /// Construct from env. LLM_BASE_URL points at any OpenAI-compatible
    /// /v1/chat/completions endpoint; LLM_ADVISOR_DISABLED=1 short-
    /// circuits every call; LLM_MODEL (or legacy LLM_ADVISOR_MODEL)
    /// names the model; LLM_API_KEY (optional) authenticates.
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("LLM_BASE_URL").unwrap_or_default();
        let model = std::env::var("LLM_MODEL")
            .or_else(|_| std::env::var("LLM_ADVISOR_MODEL"))
            .unwrap_or_default();
        let api_key = std::env::var("LLM_API_KEY")
            .ok()
            .filter(|s| !s.trim().is_empty());
        let disabled = matches!(
            std::env::var("LLM_ADVISOR_DISABLED").ok().as_deref(),
            Some("1") | Some("true") | Some("True")
        );
        Ok(Self {
            http: Self::build_http(),
            base_url,
            model,
            api_key,
            disabled,
        })
    }

    /// Construct from the UI/wizard-configured `[llm]` block, so a provider set
    /// in Settings actually drives the live advisor (the env path used to be the
    /// ONLY one, so a UI-configured Ollama/OpenAI-compat endpoint was silently
    /// ignored and the advisor stayed offline despite a passing Test). Handles
    /// the single-endpoint providers (Ollama, OpenAI-compat, llama.cpp); returns
    /// None for `Auto` (whose runtime probing is the LlmProvider path) and for an
    /// empty base_url, so the caller falls back to env.
    pub fn from_config(cfg: &crate::config::schema::LlmConfig) -> Option<Result<Self>> {
        use crate::config::schema::LlmProviderKind as K;
        let (base_url, model, api_key) = match &cfg.provider {
            K::Ollama(c) => (c.base_url.clone(), c.model.clone(), None),
            K::OpenaiCompat(c) => (
                c.base_url.clone(),
                c.model.clone(),
                c.api_key.clone().filter(|s| !s.trim().is_empty()),
            ),
            K::Llamacpp(c) => (
                c.base_url.clone(),
                c.model.clone().unwrap_or_default(),
                None,
            ),
            K::Auto(_) => return None,
        };
        if base_url.trim().is_empty() {
            return None;
        }
        Some(Ok(Self {
            http: Self::build_http(),
            base_url,
            model,
            api_key,
            disabled: false,
        }))
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    /// One-shot chat completion. Returns the assistant's content
    /// string on success, ClientError on every failure mode.
    pub async fn chat(
        &self,
        system: &str,
        user: &str,
        max_tokens: Option<u32>,
        temperature: Option<f32>,
    ) -> Result<String, ClientError> {
        if self.disabled {
            return Err(ClientError::Disabled);
        }
        let url = format!(
            "{}/v1/chat/completions",
            self.base_url.trim_end_matches('/')
        );
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
            max_tokens,
            temperature,
        };
        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await.map_err(|e| {
            ClientError::Failed(Box::new(crate::diagnostics::from_error(
                &e,
                "LLM advisor chat request",
            )))
        })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(ClientError::Failed(Box::new(
                crate::failure::Failure::http(
                    status.as_u16(),
                    Some(crate::net::source_failure::response_format(&resp)),
                    "LLM advisor chat response",
                ),
            )));
        }
        let parsed: ChatResponse = resp.json().await.map_err(|e| {
            ClientError::Failed(Box::new(crate::diagnostics::from_error(
                &e,
                "LLM advisor chat decode",
            )))
        })?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or(ClientError::Failed(Box::new(
                crate::failure::Failure::new(
                    crate::failure::FailureCode::MissingField,
                    "LLM advisor chat response",
                )
                .with_field("choices.message.content"),
            )))?
            .message
            .content
            .trim()
            .to_string();
        if content.is_empty() {
            return Err(ClientError::Failed(Box::new(
                crate::failure::Failure::new(
                    crate::failure::FailureCode::MissingField,
                    "LLM advisor chat response",
                )
                .with_field("choices.message.content"),
            )));
        }
        Ok(content)
    }
}

/// Retain the typed client error through callers using anyhow.
pub fn map_err(e: ClientError) -> anyhow::Error {
    e.into()
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn advisor_rejection_preserves_status_without_response_secret() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(429).set_body_string("private upstream token"),
            )
            .mount(&server)
            .await;
        let client = super::LlmClient {
            http: reqwest::Client::new(),
            base_url: server.uri(),
            model: "test".into(),
            api_key: Some("private credential".into()),
            disabled: false,
        };
        let error = client.chat("system", "user", None, None).await.unwrap_err();
        let failure = error.diagnostic();
        assert_eq!(failure.http_status, Some(429));
        assert_eq!(failure.operation, "LLM advisor chat response");
        assert!(!error.to_string().contains("private"));
    }
}
