// LlmProvider implementations.
//
//   ollama.rs        - native /api/chat with JSON mode via `format`
//   openai_compat.rs - /v1/chat/completions; covers OpenAI, Anthropic
//                      shims, vLLM, LM Studio, llama.cpp /v1, and any
//                      private gateway speaking the OpenAI shape
//   auto_detect.rs   - boot probe of localhost:11434, :8080, :1234
//
// The advisor in src/llm/advisor.rs accepts Arc<dyn LlmProvider>; prompts
// in src/llm/prompts.rs stay model-agnostic; cache in src/llm/cache.rs
// stays unchanged.

pub mod auto_detect;
pub mod ollama;
pub mod openai_compat;

pub use auto_detect::{default_probe_targets, detect, ProbeKind, ProbeTarget};
pub use ollama::OllamaProvider;
pub use openai_compat::OpenaiCompatProvider;
