// Boot phase 1: the tracing subscriber.
//
// `LOCALSKY_LOG_FORMAT=json` emits one JSON object per line for a log
// shipper; the default is the console format. Either way the last few
// hundred lines are kept in memory for GET /api/v1/diagnostics, and the
// handle to that ring is the phase's output: the diagnostics router
// reads it from its state, not from a process global.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::logring;

/// What logging hands the later phases.
pub struct Logging {
    /// The in-memory tail of the log, for the diagnostics bundle.
    pub ring: logring::Ring,
}

pub fn init() -> Logging {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    let json_logs = std::env::var("LOCALSKY_LOG_FORMAT").ok().as_deref() == Some("json");
    let (layer, ring) = logring::layer();
    if json_logs {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().json())
            .with(layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer())
            .with(layer)
            .init();
    }
    Logging { ring }
}
