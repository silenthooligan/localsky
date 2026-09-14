// The one poll loop. A polling adapter (a cloud station API, a LAN
// board, a forecast provider) hands `run_polling` its interval and a
// `fetch` that returns the events one poll produced; the loop owns the
// tick, the missed-tick policy, the fetch metric, the shutdown, and the
// reachability edges. An adapter that used to carry its own copy of this
// loop drifted in exactly those places: one never sent the offline edge,
// one sent the online edge every poll, one ignored shutdown.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::ports::weather_source::{ShutdownSignal, SourceBus, SourceEvent, WeatherField};

/// Emits a Reachability event only on the transition, in both directions.
/// The first poll always reports (there is no prior state), so a source
/// that starts unreachable says so.
#[derive(Debug, Default)]
pub struct ReachabilityLatch {
    last: Option<bool>,
}

impl ReachabilityLatch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the outcome of one poll; true when it is an edge the bus
    /// should hear about.
    pub fn observe(&mut self, reachable: bool) -> bool {
        let edge = self.last != Some(reachable);
        self.last = Some(reachable);
        edge
    }

    /// Record and publish. Returns whether an edge was sent.
    pub fn report(&mut self, bus: &SourceBus, source_id: &str, reachable: bool) -> bool {
        if self.observe(reachable) {
            let _ = bus.send(SourceEvent::Reachability {
                source_id: source_id.to_string(),
                reachable,
            });
            true
        } else {
            false
        }
    }

    pub fn last(&self) -> Option<bool> {
        self.last
    }
}

/// What one poll produced.
#[derive(Debug, Default)]
pub struct Poll {
    /// Events to publish, in order. Empty is a legitimate poll (the
    /// upstream answered, nothing new).
    pub events: Vec<SourceEvent>,
    /// Override the reachability verdict a successful fetch implies
    /// (default true on Ok, false on Err). An adapter whose upstream
    /// answers 200 with "no station" sets `Some(false)`.
    pub reachable: Option<bool>,
}

impl Poll {
    pub fn none() -> Self {
        Self::default()
    }

    /// One observation event, or nothing when `fields` is empty.
    pub fn observation(source_id: &str, fields: Vec<(WeatherField, f64)>, at_epoch: i64) -> Self {
        let mut p = Self::default();
        if !fields.is_empty() {
            p.events.push(SourceEvent::Observation {
                source_id: source_id.to_string(),
                fields,
                at_epoch,
            });
        }
        p
    }

    pub fn with(mut self, ev: SourceEvent) -> Self {
        self.events.push(ev);
        self
    }

    pub fn unreachable(mut self) -> Self {
        self.reachable = Some(false);
        self
    }
}

/// Run `fetch` every `every`, publishing what it returns, until shutdown.
///
/// - The tick delays (never bursts) after a missed interval.
/// - The first poll runs immediately.
/// - `fetch` is wrapped in the source fetch metric.
/// - Ok publishes the events and reports reachable (unless the poll says
///   otherwise); Err logs at warn with the label and reports unreachable.
///   Both only on the edge.
/// - Shutdown returns Ok(()).
pub async fn run_polling<S, F, Fut>(
    source: Arc<S>,
    source_id: &str,
    label: &str,
    every: Duration,
    bus: SourceBus,
    mut shutdown: ShutdownSignal,
    mut fetch: F,
) -> anyhow::Result<()>
where
    S: Send + Sync + 'static,
    F: FnMut(Arc<S>) -> Fut,
    Fut: Future<Output = anyhow::Result<Poll>>,
{
    info!(source_id = %source_id, "{label} source started");
    let mut tick = interval(every);
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut latch = ReachabilityLatch::new();
    loop {
        tokio::select! {
            _ = tick.tick() => {
                match crate::metrics::observe_fetch(source_id, fetch(source.clone())).await {
                    Ok(poll) => {
                        let reachable = poll.reachable.unwrap_or(true);
                        latch.report(&bus, source_id, reachable);
                        let n = poll.events.len();
                        for ev in poll.events {
                            let _ = bus.send(ev);
                        }
                        if n > 0 {
                            debug!(source_id = %source_id, events = n, "{label} updated");
                        }
                    }
                    Err(e) => {
                        warn!(source_id = %source_id, error = %e, "{label} fetch failed");
                        latch.report(&bus, source_id, false);
                    }
                }
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    info!(source_id = %source_id, "{label} shutdown");
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn the_latch_reports_every_edge_and_nothing_else() {
        let mut l = ReachabilityLatch::new();
        assert!(l.observe(true), "the first poll always reports");
        assert!(!l.observe(true));
        assert!(l.observe(false));
        assert!(!l.observe(false));
        assert!(l.observe(true));
        let mut l = ReachabilityLatch::new();
        assert!(l.observe(false), "a source that starts unreachable says so");
    }

    struct Flaky {
        calls: AtomicUsize,
    }

    /// Ok, Err, Err, Ok: the bus hears online, offline, online (three
    /// edges out of four polls) and one observation per Ok.
    #[tokio::test(start_paused = true)]
    async fn the_loop_publishes_both_reachability_edges_once_each() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(64);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let src = Arc::new(Flaky {
            calls: AtomicUsize::new(0),
        });
        let task = tokio::spawn(run_polling(
            src,
            "flaky",
            "Flaky",
            Duration::from_secs(10),
            bus,
            stop_rx,
            |s: Arc<Flaky>| async move {
                let n = s.calls.fetch_add(1, Ordering::SeqCst);
                match n {
                    1 | 2 => Err(anyhow::anyhow!("down")),
                    _ => Ok(Poll::observation(
                        "flaky",
                        vec![(WeatherField::AirTempF, 70.0)],
                        1_000,
                    )),
                }
            },
        ));
        tokio::time::sleep(Duration::from_secs(31)).await;
        let _ = stop_tx.send(true);
        let _ = task.await;
        let mut edges = Vec::new();
        let mut observations = 0;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                SourceEvent::Reachability { reachable, .. } => edges.push(reachable),
                SourceEvent::Observation { .. } => observations += 1,
                _ => {}
            }
        }
        assert_eq!(edges, vec![true, false, true]);
        assert_eq!(observations, 2);
    }

    /// A poll that answers but says the station is gone reports offline
    /// without an error.
    #[tokio::test(start_paused = true)]
    async fn a_poll_can_overrule_the_reachability_verdict() {
        let (bus, mut rx) = tokio::sync::broadcast::channel(8);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(run_polling(
            Arc::new(()),
            "s",
            "S",
            Duration::from_secs(10),
            bus,
            stop_rx,
            |_: Arc<()>| async { Ok(Poll::none().unreachable()) },
        ));
        tokio::time::sleep(Duration::from_secs(1)).await;
        let _ = stop_tx.send(true);
        let _ = task.await;
        match rx.try_recv() {
            Ok(SourceEvent::Reachability { reachable, .. }) => assert!(!reachable),
            other => panic!("{other:?}"),
        }
    }
}
