// IrrigationStore. Same shape as TempestStore: arc-swap for
// snapshot reads + a watch channel SSE subscribers connect to.
// Refresher task is the only writer; everyone else only reads.

use crate::ha::snapshot::IrrigationSnapshot;
use arc_swap::ArcSwap;
use std::sync::Arc;
use tokio::sync::watch;

pub struct IrrigationStore {
    current: ArcSwap<IrrigationSnapshot>,
    tx: watch::Sender<Arc<IrrigationSnapshot>>,
    rx: watch::Receiver<Arc<IrrigationSnapshot>>,
}

impl IrrigationStore {
    pub fn new() -> Self {
        let initial = Arc::new(IrrigationSnapshot::default());
        let (tx, rx) = watch::channel(initial.clone());
        Self {
            current: ArcSwap::from(initial),
            tx,
            rx,
        }
    }

    pub fn snapshot(&self) -> Arc<IrrigationSnapshot> {
        self.current.load_full()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<IrrigationSnapshot>> {
        self.rx.clone()
    }

    /// Atomically swap the current snapshot and notify watchers, but only
    /// notify when the snapshot carries *new information* (P3-2). The refresher
    /// re-stamps the freshness timestamps every ~10s tick even when nothing
    /// material changed; without this gate a quiet-day phone (and the HA
    /// integration stream) receives ~6 full snapshots/min for no reason. The
    /// `current` slot is always updated so REST reads stay fresh; only the SSE
    /// `send` is gated. The stream's 15s keep-alive holds idle connections open.
    pub fn store(&self, snap: IrrigationSnapshot) {
        let changed = substantively_changed(&self.current.load(), &snap);
        let new = Arc::new(snap);
        self.current.store(new.clone());
        if changed {
            let _ = self.tx.send(new);
        }
    }
}

/// Whether `next` differs from `prev` in anything other than the per-tick
/// freshness timestamps (which advance every refresh regardless of content).
/// Robust against new fields by construction: it grafts `next`'s timestamps
/// onto a copy of `prev` and leans on the derived `PartialEq`, so a field added
/// later is compared automatically. `next_run_epoch` and `pause_until_epoch` are
/// deliberately NOT masked: those are real scheduling/control state.
fn substantively_changed(prev: &IrrigationSnapshot, next: &IrrigationSnapshot) -> bool {
    let mut masked = prev.clone();
    masked.last_refresh_epoch = next.last_refresh_epoch;
    masked.tempest_last_seen_epoch = next.tempest_last_seen_epoch;
    masked.forecast_last_seen_epoch = next.forecast_last_seen_epoch;
    masked != *next
}

impl Default for IrrigationStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_timestamp_tick_does_not_publish() {
        // A refresh that only advances the freshness stamps carries no new
        // information and must NOT wake every SSE subscriber.
        let base = IrrigationSnapshot::default();
        let mut tick = base.clone();
        tick.last_refresh_epoch = 1010;
        tick.tempest_last_seen_epoch = 1010;
        tick.forecast_last_seen_epoch = 1005;
        assert!(!substantively_changed(&base, &tick));
    }

    #[test]
    fn real_state_change_publishes() {
        let base = IrrigationSnapshot::default();
        // Scheduling + control state are real, never masked.
        let mut sched = base.clone();
        sched.next_run_epoch = 99_999;
        assert!(substantively_changed(&base, &sched));
        let mut pause = base.clone();
        pause.pause_until_epoch = 50_000;
        assert!(substantively_changed(&base, &pause));
        // A masked freshness stamp on its own still does not publish.
        let mut stamp = base.clone();
        stamp.last_refresh_epoch = 42;
        assert!(!substantively_changed(&base, &stamp));
    }
}
