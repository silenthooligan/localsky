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
    #[error("{0}")]
    AuthFailed(#[source] Box<crate::failure::Failure>),
    #[error("model unavailable: {0}")]
    ModelUnavailable(String),
    #[error("{0}")]
    RateLimited(#[source] Box<crate::failure::Failure>),
    #[error("provider error: {0}")]
    Remote(#[source] Box<crate::failure::Failure>),
    #[error("transport error: {0}")]
    Transport(#[source] Box<crate::failure::Failure>),
    #[error("response parse error: {0}")]
    Parse(#[source] Box<crate::failure::Failure>),
}

impl LlmError {
    pub fn transport(f: crate::failure::Failure) -> Self {
        Self::Transport(Box::new(f))
    }
    pub fn remote(f: crate::failure::Failure) -> Self {
        Self::Remote(Box::new(f))
    }
    pub fn parse(f: crate::failure::Failure) -> Self {
        Self::Parse(Box::new(f))
    }
    pub fn http(status: u16, format: Option<&'static str>, operation: &'static str) -> Self {
        let f = Box::new(crate::failure::Failure::http(status, format, operation));
        match status {
            401 | 403 => Self::AuthFailed(f),
            429 => Self::RateLimited(f),
            _ => Self::Remote(f),
        }
    }
    pub fn diagnostic(&self) -> crate::failure::Failure {
        use crate::failure::{Failure, FailureCode as Code};
        match self {
            Self::Remote(f)
            | Self::Transport(f)
            | Self::Parse(f)
            | Self::AuthFailed(f)
            | Self::RateLimited(f) => (**f).clone(),
            Self::Offline => Failure::new(Code::ProviderOffline, "LLM provider health"),
            Self::ModelUnavailable(model) => {
                Failure::new(Code::LlmModel, "LLM model lookup").with_resource(model)
            }
        }
    }
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

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub reachable: bool,
    pub model_loaded: Option<String>,
    pub provider_version: Option<String>,
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<crate::failure::FailureRecord>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    async fn chat(&self, system: &str, user: &str, opts: ChatOpts) -> Result<String, LlmError>;
    async fn health(&self) -> Result<HealthReport, LlmError>;
}
