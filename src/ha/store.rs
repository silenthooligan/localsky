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

    /// Atomically swap the current snapshot and notify watchers.
    pub fn store(&self, snap: IrrigationSnapshot) {
        let new = Arc::new(snap);
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }
}

impl Default for IrrigationStore {
    fn default() -> Self {
        Self::new()
    }
}
