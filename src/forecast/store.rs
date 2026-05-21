// Arc-swap store + watch channel. Identical shape to TempestStore /
// IrrigationStore so the SSE wiring is uniform.

use crate::forecast::snapshot::ForecastSnapshot;
use arc_swap::ArcSwap;
use std::sync::Arc;
use tokio::sync::watch;

pub struct ForecastStore {
    current: ArcSwap<ForecastSnapshot>,
    tx: watch::Sender<Arc<ForecastSnapshot>>,
    rx: watch::Receiver<Arc<ForecastSnapshot>>,
}

impl ForecastStore {
    pub fn new() -> Self {
        let initial = Arc::new(ForecastSnapshot::default());
        let (tx, rx) = watch::channel(initial.clone());
        Self {
            current: ArcSwap::from(initial),
            tx,
            rx,
        }
    }

    pub fn snapshot(&self) -> Arc<ForecastSnapshot> {
        self.current.load_full()
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<ForecastSnapshot>> {
        self.rx.clone()
    }

    pub fn store(&self, snap: ForecastSnapshot) {
        let new = Arc::new(snap);
        self.current.store(new.clone());
        let _ = self.tx.send(new);
    }
}

impl Default for ForecastStore {
    fn default() -> Self {
        Self::new()
    }
}
