// Boot-time LLM auto-detection. Probe a list of well-known local
// endpoints and return the first one that responds. Lets users drop
// LocalSky onto a workstation with Ollama / llama.cpp / LM Studio
// already running and get advisor coverage without writing any
// configuration.
//
// Probe order (default):
//   1. Ollama          http://localhost:11434/api/tags
//   2. llama.cpp       http://localhost:8080/v1/models
//   3. LM Studio       http://localhost:1234/v1/models
//
// Returns a Box<dyn LlmProvider> ready to dispatch. Caller is expected
// to pass the model name; auto-detect doesn't pick one.

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::llm::providers::{ollama::OllamaProvider, openai_compat::OpenaiCompatProvider};
use crate::ports::llm_provider::LlmProvider;

#[derive(Debug, Clone)]
pub struct ProbeTarget {
    pub kind: ProbeKind,
    pub base_url: String,
}

#[derive(Debug, Clone, Copy)]
pub enum ProbeKind {
    Ollama,
    OpenaiCompat,
}

pub fn default_probe_targets() -> Vec<ProbeTarget> {
    vec![
        ProbeTarget {
            kind: ProbeKind::Ollama,
            base_url: "http://localhost:11434".into(),
        },
        ProbeTarget {
            kind: ProbeKind::OpenaiCompat,
            base_url: "http://localhost:8080".into(),
        },
        ProbeTarget {
            kind: ProbeKind::OpenaiCompat,
            base_url: "http://localhost:1234".into(),
        },
    ]
}

/// Probe each target and return a provider for the first reachable one.
/// `model` is the model name to use with whichever provider wins. The
/// detected provider gets `id = "auto:<kind>:<base_url>"` so logs are
/// traceable to the actual endpoint.
pub async fn detect(targets: Vec<ProbeTarget>, model: String) -> Option<Arc<dyn LlmProvider>> {
    for target in targets {
        let probe_path = match target.kind {
            ProbeKind::Ollama => "/api/tags",
            ProbeKind::OpenaiCompat => "/v1/models",
        };
        let url = format!("{}{}", target.base_url.trim_end_matches('/'), probe_path);
        debug!("auto-detect probing {url}");
        // SSRF-hardened, IP-pinned probe: the probe_order is operator-
        // supplied and reachable from the wizard's test_llm endpoint, so
        // each candidate goes through the same forbidden-target +
        // anti-rebinding + no-redirect client the real providers use. The
        // built-in localhost defaults are loopback and so are rejected here
        // (consistent with the OpenAI-compat provider's loopback policy); in
        // a containerized deployment localhost is the container itself, not a
        // real LLM host, so the operator points probe_order at the LAN/host.
        let (client, safe_url) =
            match crate::net::safe_fetch::build_safe_client(&url, Duration::from_secs(3)).await {
                Ok(pair) => pair,
                Err(e) => {
                    debug!("auto-detect: {url} not a permitted target: {e}");
                    continue;
                }
            };
        match client.get(safe_url).send().await {
            Ok(r) if r.status().is_success() => {
                info!("auto-detect: {url} responded; using {:?}", target.kind);
                let provider: Arc<dyn LlmProvider> = match target.kind {
                    ProbeKind::Ollama => Arc::new(OllamaProvider::new(
                        format!("auto:ollama:{}", target.base_url),
                        target.base_url,
                        model.clone(),
                    )),
                    ProbeKind::OpenaiCompat => Arc::new(OpenaiCompatProvider::new(
                        format!("auto:openai_compat:{}", target.base_url),
                        target.base_url,
                        model.clone(),
                        None,
                    )),
                };
                return Some(provider);
            }
            Ok(r) => debug!("auto-detect: {url} returned {}", r.status()),
            Err(e) => debug!("auto-detect: {url} unreachable: {e}"),
        }
    }
    warn!("auto-detect: no local LLM provider responded");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_probe_targets_has_three() {
        let targets = default_probe_targets();
        assert_eq!(targets.len(), 3);
        assert!(targets.iter().any(|t| t.base_url.contains("11434"))); // Ollama
        assert!(targets.iter().any(|t| t.base_url.contains("8080"))); // llama.cpp
        assert!(targets.iter().any(|t| t.base_url.contains("1234"))); // LM Studio
    }

    #[tokio::test]
    async fn detect_with_unreachable_targets_returns_none() {
        // All targets are bogus ports on localhost; nothing should respond.
        let targets = vec![ProbeTarget {
            kind: ProbeKind::Ollama,
            base_url: "http://127.0.0.1:1".into(),
        }];
        let p = detect(targets, "model".into()).await;
        assert!(p.is_none());
    }
}
