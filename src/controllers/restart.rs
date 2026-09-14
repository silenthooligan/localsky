//! Process-lifetime hold after changing configuration that only boot can wire.
//! The registry, runtime apply path and dispatcher share this one latch. There
//! is deliberately no clear operation: only a new process can rebuild all of
//! the affected adapters, zone bindings and calendar coherently.

use std::sync::{Arc, Mutex};

pub use crate::gates_catalog::RESTART_REQUIRED_REASON as WATERING_HOLD_REASON;

#[derive(Clone, Default)]
pub struct RestartHold {
    reasons: Arc<Mutex<Vec<String>>>,
}

impl RestartHold {
    pub fn latch(&self, reasons: impl IntoIterator<Item = String>) {
        let mut held = self.reasons.lock().unwrap();
        for reason in reasons {
            if !reason.is_empty() && !held.contains(&reason) {
                held.push(reason);
            }
        }
    }

    pub fn reasons(&self) -> Vec<String> {
        self.reasons.lock().unwrap().clone()
    }

    pub fn is_pending(&self) -> bool {
        !self.reasons.lock().unwrap().is_empty()
    }
}
