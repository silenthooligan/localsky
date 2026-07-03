// Thin OpenAI-compatible chat-completions client. Legacy v0.1 single-
// endpoint shape; v2 callers prefer the LlmProvider trait + the
// providers in src/llm/providers/. All errors are caught and returned
// as `ClientError` so the advisor layer can degrade gracefully:
// never panics, never blocks irrigation.

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug)]
pub enum ClientError {
    Disabled,
    Unreachable(String),
    BadStatus(String),
    Decode(String),
    Empty,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(f, "LLM disabled by env var"),
            Self::Unreachable(s) => write!(f, "LLM upstream unreachable: {s}"),
            Self::BadStatus(s) => write!(f, "LLM returned non-2xx: {s}"),
            Self::Decode(s) => write!(f, "LLM response decode failed: {s}"),
            Self::Empty => write!(f, "LLM returned empty content"),
        }
    }
}

impl std::error::Error for ClientError {}

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
    fn build_http() -> Result<Client> {
        Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("localsky/advisor")
            .build()
            .context("build http client")
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
            http: Self::build_http()?,
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
        Some(Self::build_http().map(|http| Self {
            http,
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
        let resp = req
            .send()
            .await
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(ClientError::BadStatus(format!(
                "{}, {}",
                status,
                truncate(&text, 240)
            )));
        }
        let parsed: ChatResponse = resp
            .json()
            .await
            .map_err(|e| ClientError::Decode(e.to_string()))?;
        let content = parsed
            .choices
            .into_iter()
            .next()
            .ok_or(ClientError::Empty)?
            .message
            .content
            .trim()
            .to_string();
        if content.is_empty() {
            return Err(ClientError::Empty);
        }
        Ok(content)
    }
}

/// Byte-budgeted truncation that never splits a UTF-8 codepoint: when `max`
/// lands mid-character, walk back to the nearest char boundary before slicing.
/// The upstream error body this trims is arbitrary bytes-of-text (a proxy page,
/// a JSON error with curly quotes), and `&s[..max]` on a non-boundary index
/// panics inside the request task. Log-trimming only, so losing a few bytes to
/// the boundary walk is fine. Mirrors `llm::advisor::truncate`.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &s[..cut])
}

/// Convenience: log a warning + map to anyhow for callers that prefer
/// the unified anyhow error type. Used by advisor.rs.
pub fn map_err(e: ClientError) -> anyhow::Error {
    anyhow!("llm client: {e}")
}

#[cfg(test)]
mod tests {
    use super::truncate;

    #[test]
    fn truncate_never_splits_a_multibyte_codepoint() {
        // A degree sign straddling the byte budget must not panic the slice:
        // "88°F and rising" cut at byte 3 lands mid-"°" and walks back.
        let s = "88°F and rising";
        assert_eq!(truncate(s, 3), "88…");
        // On-boundary cut keeps the full prefix; short input is untouched.
        assert_eq!(truncate(s, 4), "88°…");
        assert_eq!(truncate(s, s.len()), s);
        assert_eq!(truncate("abcdef", 3), "abc…");
    }
}
