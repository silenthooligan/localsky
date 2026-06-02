// LlmProvider port. Every LLM backend (Ollama, llama.cpp, any OpenAI-
// compatible endpoint) implements this. The advisor in src/llm/advisor.rs
// uses Arc<dyn LlmProvider>; prompts and the TTL cache are provider-
// agnostic.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("provider offline")]
    Offline,
    #[error("auth failed")]
    AuthFailed,
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("rate limited")]
    RateLimited,
    #[error("provider error: {0}")]
    Remote(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("response parse error: {0}")]
    Parse(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatOpts {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub timeout_s: Option<u32>,
    /// Force JSON response (anomaly detector path). Provider-specific:
    /// Ollama -> format: "json", OpenAI -> response_format: json_object.
    pub json_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub reachable: bool,
    pub model_loaded: Option<String>,
    pub provider_version: Option<String>,
    pub last_error: Option<String>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    async fn chat(&self, system: &str, user: &str, opts: ChatOpts) -> Result<String, LlmError>;
    async fn health(&self) -> Result<HealthReport, LlmError>;
}
