// LLM advisory layer. Talks to whatever provider the operator configured
// (Ollama, llama.cpp, OpenAI, any OpenAI-compatible gateway). Never gates
// safety decisions: the deterministic skip-check engine in
// `engine::skip_rules` runs first and owns every irrigation action.
// Advisor output is surface content only -- explanations on the
// dashboard, anomaly banners, weekly summaries (deferred), natural-
// language threshold edits (deferred).
//
// Layering:
//   providers/  -- LlmProvider implementations (Ollama, OpenAI-compat,
//                  auto-detect probe)
//   client.rs   -- legacy single-endpoint wrapper retained for v0.1
//                  callers; v2 callers prefer the LlmProvider trait
//   cache.rs    -- TTL cache so we don't re-call for the same snapshot
//   prompts.rs  -- versioned system prompts
//   advisor.rs  -- explain_today, detect_anomalies entry points

#[cfg(feature = "ssr")]
pub mod advisor;
#[cfg(feature = "ssr")]
pub mod cache;
#[cfg(feature = "ssr")]
pub mod client;
#[cfg(feature = "ssr")]
pub mod prompts;
#[cfg(feature = "ssr")]
pub mod providers;

#[cfg(feature = "ssr")]
pub use advisor::{AdvisorError, AdvisorState, Anomaly};
