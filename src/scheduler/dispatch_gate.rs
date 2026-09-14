// The stop gate between the API and a smart-morning sequence in flight.
//
// A Stop or Stop All tapped while the dispatcher is walking a sequence
// has to abandon the rest of it. The gate is a monotonic generation: a
// cycle snapshots the generation when it starts, every stop request
// bumps it, and the cycle checks between steps whether it moved. Order
// is the only thing that matters, and a counter cannot be wrong about
// order.
//
// It used to be a wall-clock epoch kept with fetch_max, compared
// against the epoch the cycle started at. On a host without a real-time
// clock the wall clock steps: it boots in 1970 or in the far future and
// lands on the right time once NTP answers. A Stop tapped while the
// clock read the future stamped a future epoch, and every cycle that
// started afterwards, at the correct time, read that stamp as "a stop
// after my start" and abandoned itself as "Stopped manually". A counter
// does not know what time it is and cannot be fooled by it.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// A monotonic stop counter.
pub struct StopGate(AtomicU64);

impl StopGate {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }
}

impl Default for StopGate {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide gate the API bumps and the dispatcher reads.
static GLOBAL: StopGate = StopGate::new();

tokio::task_local! {
    /// A private gate for one task tree, so tests that dispatch in
    /// parallel inside one process cannot stop each other's mornings.
    static SCOPED: Arc<StopGate>;
}

fn with_gate<R>(f: impl FnOnce(&StopGate) -> R) -> R {
    // Resolve the gate first, then call once: the closure is FnOnce.
    let scoped: Option<Arc<StopGate>> = SCOPED.try_with(|g| g.clone()).ok();
    match scoped {
        Some(g) => f(&g),
        None => f(&GLOBAL),
    }
}

/// A Stop or Stop All was requested. Every cycle that started before
/// this call sees it; a cycle that starts after it does not.
pub fn request_stop() {
    with_gate(|g| {
        g.0.fetch_add(1, Ordering::SeqCst);
    });
}

/// The generation a cycle snapshots when it starts.
pub fn generation() -> u64 {
    with_gate(|g| g.0.load(Ordering::SeqCst))
}

/// Whether a stop has been requested since a cycle that snapshotted
/// `at_start` began.
pub fn stop_requested_since(at_start: u64) -> bool {
    generation() > at_start
}

/// Compatibility for the callers that stamped wall-clock epochs. The
/// epoch is not consulted: a stop is a request, not a time, and the
/// generation decides what it applies to.
pub fn note_stop_at(_epoch: i64) {
    request_stop();
}

/// Run `fut` against a gate private to its task tree. Nested calls
/// share the outer gate, so a test can wrap a dispatch and a stopper
/// together and have them see each other.
pub async fn isolated<F: Future>(fut: F) -> F::Output {
    if SCOPED.try_with(|_| ()).is_ok() {
        return fut.await;
    }
    SCOPED.scope(Arc::new(StopGate::new()), fut).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stop after a cycle started is visible to it; a stop before the
    /// cycle started is not, whatever the wall clock said at either
    /// moment. The generation is the whole story.
    #[test]
    fn a_stop_is_visible_only_to_cycles_already_running() {
        let earlier = generation();
        request_stop();
        assert!(stop_requested_since(earlier));
        let later = generation();
        assert!(!stop_requested_since(later));
    }

    /// The case that used to abandon every later morning: a stop stamped
    /// while the host's clock read the future, followed by a cycle that
    /// starts at the correct, earlier time. The cycle proceeds.
    #[test]
    fn a_future_epoch_stop_does_not_abandon_a_later_cycle() {
        note_stop_at(4_102_444_800); // 2100-01-01, a clock that has not synced
        let cycle = generation();
        assert!(!stop_requested_since(cycle));
        // And a real stop during that cycle still lands.
        request_stop();
        assert!(stop_requested_since(cycle));
    }
}
