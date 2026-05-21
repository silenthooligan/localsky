// Helper re-exports from src/runtime.rs so main.rs can reach
// `build_receiver_sources` without needing the full Runtime type. Keeps
// main.rs short.

pub use crate::runtime::build_receiver_sources;
