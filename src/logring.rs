// The last few hundred log lines, kept in memory for the diagnostics
// bundle. A tracing layer that formats each event the way the console
// does and keeps the newest `CAPACITY` lines; `Ring::recent(n)` hands
// them to GET /api/v1/diagnostics, which scrubs them before they leave.
// The ring is a handle the boot hands to the diagnostics router's state,
// not a process global.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

const CAPACITY: usize = 500;

/// The shared tail of the log. Cloning shares the buffer.
#[derive(Clone, Default)]
pub struct Ring {
    lines: Arc<Mutex<VecDeque<String>>>,
}

impl Ring {
    pub fn new() -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY))),
        }
    }

    /// Append one formatted line, dropping the oldest past capacity.
    pub fn push(&self, line: String) {
        if let Ok(mut r) = self.lines.lock() {
            if r.len() >= CAPACITY {
                r.pop_front();
            }
            r.push_back(line);
        }
    }

    /// The newest `n` lines, oldest first.
    pub fn recent(&self, n: usize) -> Vec<String> {
        self.lines
            .lock()
            .map(|r| r.iter().rev().take(n).cloned().collect::<Vec<_>>())
            .map(|mut v| {
                v.reverse();
                v
            })
            .unwrap_or_default()
    }
}

/// The layer to add to the subscriber at boot, and the ring it writes.
pub fn layer() -> (LogRing, Ring) {
    let ring = Ring::new();
    (LogRing { ring: ring.clone() }, ring)
}

pub struct LogRing {
    ring: Ring,
}

struct Line(String);

impl Visit for Line {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.0.push_str(&format!(" {value:?}"));
        } else {
            self.0.push_str(&format!(" {}={value:?}", field.name()));
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(&format!(" {value}"));
        } else {
            self.0.push_str(&format!(" {}={value}", field.name()));
        }
    }
}

impl<S: tracing::Subscriber> Layer<S> for LogRing {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut line = Line(format!(
            "{} {:>5} {}:",
            chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
            meta.level(),
            meta.target()
        ));
        event.record(&mut line);
        self.ring.push(line.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_keeps_the_newest_lines_in_order() {
        let ring = Ring::new();
        for i in 0..(CAPACITY + 10) {
            ring.push(format!("line {i}"));
        }
        let last = ring.recent(3);
        assert_eq!(last.len(), 3);
        assert!(last[2].ends_with(&format!("line {}", CAPACITY + 9)));
        assert!(last[0].ends_with(&format!("line {}", CAPACITY + 7)));
        assert!(ring.recent(10_000).len() <= CAPACITY);
    }
}
