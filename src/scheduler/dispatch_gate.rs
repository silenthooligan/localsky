// Cross-task cancellation gate for in-flight irrigation dispatch.
//
// The smart-morning dispatcher runs a multi-zone sequence with long
// tokio sleeps between cycle-soak segments. A manual Stop / Stop All /
// vacation pause from the dashboard must interrupt that sequence, not
// just stop the currently-open valve: without a gate the loop happily
// dispatches the NEXT segment seconds after the operator hit Stop.
//
// The gate is a process-wide epoch of the most recent stop request.
// Writers (the POST /api/irrigation/action handler) call request_stop();
// the dispatcher snapshots the wall clock when a watering cycle begins
// and checks stop_requested_since(cycle_start) before every segment
// dispatch and inside every soak/run wait. Monotonic and race-tolerant:
// a stop that lands in the same second as the cycle start counts as a
// stop, which is the fail-safe direction for irrigation.

use std::sync::atomic::{AtomicI64, Ordering};

static LAST_STOP_EPOCH: AtomicI64 = AtomicI64::new(0);

/// Record a stop request at the current wall-clock epoch.
pub fn request_stop() {
    note_stop_at(chrono::Utc::now().timestamp());
}

/// Record a stop request at an explicit epoch. `fetch_max` keeps the
/// gate monotonic even if the wall clock steps backward between calls.
pub fn note_stop_at(epoch: i64) {
    LAST_STOP_EPOCH.fetch_max(epoch, Ordering::SeqCst);
}

/// True when a stop has been requested at or after `epoch`.
pub fn stop_requested_since(epoch: i64) -> bool {
    LAST_STOP_EPOCH.load(Ordering::SeqCst) >= epoch
}

#[cfg(test)]
mod tests {
    use super::*;

    // The gate is a process-wide static shared by every test in the
    // binary, so assertions are phrased relative to "now" rather than
    // absolute values: they hold no matter what other tests do.

    #[test]
    fn stop_visible_to_earlier_cycle_start() {
        let now = chrono::Utc::now().timestamp();
        request_stop();
        // A cycle that started a minute ago sees the stop.
        assert!(stop_requested_since(now - 60));
        // Same-second start also sees it (fail-safe direction).
        assert!(stop_requested_since(now));
    }

    #[test]
    fn future_cycle_start_does_not_see_past_stop() {
        request_stop();
        let now = chrono::Utc::now().timestamp();
        // A cycle starting well in the future is not cancelled by an
        // old stop request.
        assert!(!stop_requested_since(now + 10_000));
    }

    #[test]
    fn note_stop_is_monotonic() {
        let now = chrono::Utc::now().timestamp();
        note_stop_at(now + 100);
        // An older stop epoch cannot roll the gate backward.
        note_stop_at(now - 1_000_000);
        assert!(stop_requested_since(now + 100));
        assert!(!stop_requested_since(now + 101));
    }
}
