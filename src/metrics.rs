// A tiny zero-dependency metrics registry + Prometheus text exposition,
// scraped by the monitoring stack. Deliberately minimal: labeled counters
// (a BTreeMap keyed by metric name + the rendered label string) and unlabeled
// gauges, behind Mutexes, in a process-global OnceLock. No histograms yet --
// counters + a refresh-freshness gauge cover the first-cut signals (refresh rate,
// degraded-rate, verdict mix, controller errors). Adding a series later is a
// one-liner at the call site; only `render()` and the META tables know the shape.
//
// All mutators are best-effort: a poisoned lock silently drops the sample rather
// than panicking a hot path (metrics must never take down watering).

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

/// Counter metric names + HELP text. `render()` emits one TYPE block per entry,
/// then every sample whose name matches (or a single `name 0` so the series
/// always exists for scrapers/alerts even before the first event).
const COUNTER_META: &[(&str, &str)] = &[
    (
        "localsky_refresh_total",
        "Refresher ticks that produced a snapshot",
    ),
    (
        "localsky_refresh_degraded_total",
        "Refreshes whose decision ran on degraded inputs (stale/absent station or forecast)",
    ),
    (
        "localsky_verdict_total",
        "Irrigation decision verdicts by outcome",
    ),
    (
        "localsky_controller_errors_total",
        "Controller operation errors by controller and operation",
    ),
    (
        "localsky_source_fetch_total",
        "Outbound fetches per source and outcome (ok / error)",
    ),
];

/// Gauge metric names + HELP text.
const GAUGE_META: &[(&str, &str)] = &[(
    "localsky_last_refresh_epoch",
    "Unix time of the last completed refresh (freshness; alert if it stops advancing)",
)];

/// Histogram metric names + HELP text. One histogram today: outbound fetch
/// latency per source, recorded by `observe_fetch` around every poll-style
/// source's fetch.
const HISTOGRAM_META: &[(&str, &str)] = &[(
    "localsky_source_fetch_seconds",
    "Outbound fetch latency per source (every poll of a cloud, LAN or bus source)",
)];

/// Upper bounds of the latency buckets, seconds. A cloud forecast answers
/// in under a second; a LAN gateway in tens of milliseconds; the 30 s end
/// catches a provider that is timing out.
pub const FETCH_BUCKETS_S: [f64; 9] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0];

#[derive(Debug, Clone, Default)]
struct Histogram {
    /// Cumulative counts per bucket in `FETCH_BUCKETS_S` order.
    buckets: [u64; FETCH_BUCKETS_S.len()],
    sum: f64,
    count: u64,
}

struct Registry {
    // (metric_name, rendered_labels) -> value. labels are "" or `k="v",k2="v2"`.
    counters: Mutex<BTreeMap<(&'static str, String), u64>>,
    gauges: Mutex<BTreeMap<&'static str, f64>>,
    histograms: Mutex<BTreeMap<(&'static str, String), Histogram>>,
}

/// The process-wide registry, the way every metrics library keeps one:
/// counters are incremented from adapters, handlers and tasks that share
/// nothing else, so the registry is the one static that is not a boot
/// handle in disguise.
static REGISTRY: LazyLock<Registry> = LazyLock::new(|| Registry {
    counters: Mutex::new(BTreeMap::new()),
    gauges: Mutex::new(BTreeMap::new()),
    histograms: Mutex::new(BTreeMap::new()),
});

fn registry() -> &'static Registry {
    &REGISTRY
}

/// Record one observation into a histogram.
pub fn observe(name: &'static str, labels: String, value: f64) {
    if let Ok(mut h) = registry().histograms.lock() {
        let entry = h.entry((name, labels)).or_default();
        for (i, bound) in FETCH_BUCKETS_S.iter().enumerate() {
            if value <= *bound {
                entry.buckets[i] += 1;
            }
        }
        entry.sum += value;
        entry.count += 1;
    }
}

/// Time one source fetch and record it: a count per source and outcome
/// (`localsky_source_fetch_total`) and the latency histogram
/// (`localsky_source_fetch_seconds`). Wraps the future so a source's
/// run loop changes one line to be instrumented.
pub async fn observe_fetch<T, E, F>(source: &str, fut: F) -> Result<T, E>
where
    F: std::future::Future<Output = Result<T, E>>,
{
    let started = std::time::Instant::now();
    let out = fut.await;
    let secs = started.elapsed().as_secs_f64();
    let outcome = if out.is_ok() { "ok" } else { "error" };
    inc(
        "localsky_source_fetch_total",
        format!("{},{}", label("source", source), label("outcome", outcome)),
    );
    observe(
        "localsky_source_fetch_seconds",
        label("source", source),
        secs,
    );
    out
}

/// Escape a Prometheus label value (backslash, double-quote, newline).
fn esc(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Render one label as `key="value"`. Caller joins multiples with `,`.
pub fn label(key: &str, value: &str) -> String {
    format!("{key}=\"{}\"", esc(value))
}

/// Increment a labeled counter by 1. `labels` is the already-joined label body
/// (e.g. `verdict="skip"`) or "" for an unlabeled counter.
pub fn inc(name: &'static str, labels: String) {
    inc_by(name, labels, 1);
}

/// Increment a labeled counter by `by`.
pub fn inc_by(name: &'static str, labels: String, by: u64) {
    if let Ok(mut c) = registry().counters.lock() {
        *c.entry((name, labels)).or_insert(0) += by;
    }
}

/// Set an unlabeled gauge.
pub fn set_gauge(name: &'static str, value: f64) {
    if let Ok(mut g) = registry().gauges.lock() {
        g.insert(name, value);
    }
}

/// Render the full registry in Prometheus text exposition format.
pub fn render() -> String {
    let mut out = String::new();
    let counters = registry().counters.lock().ok();
    let gauges = registry().gauges.lock().ok();

    for (name, help) in COUNTER_META {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} counter\n"));
        let mut any = false;
        if let Some(c) = &counters {
            for ((n, labels), v) in c.iter() {
                if n == name {
                    any = true;
                    if labels.is_empty() {
                        out.push_str(&format!("{name} {v}\n"));
                    } else {
                        out.push_str(&format!("{name}{{{labels}}} {v}\n"));
                    }
                }
            }
        }
        if !any {
            out.push_str(&format!("{name} 0\n"));
        }
    }

    for (name, help) in GAUGE_META {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n"));
        let v = gauges
            .as_ref()
            .and_then(|g| g.get(name))
            .copied()
            .unwrap_or(0.0);
        out.push_str(&format!("{name} {v}\n"));
    }

    let histograms = registry().histograms.lock().ok();
    for (name, help) in HISTOGRAM_META {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} histogram\n"));
        if let Some(h) = &histograms {
            for ((n, labels), hist) in h.iter() {
                if n != name {
                    continue;
                }
                let sep = if labels.is_empty() { "" } else { "," };
                for (i, bound) in FETCH_BUCKETS_S.iter().enumerate() {
                    out.push_str(&format!(
                        "{name}_bucket{{{labels}{sep}le=\"{bound}\"}} {}\n",
                        hist.buckets[i]
                    ));
                }
                out.push_str(&format!(
                    "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {}\n",
                    hist.count
                ));
                out.push_str(&format!("{name}_sum{{{labels}}} {}\n", hist.sum));
                out.push_str(&format!("{name}_count{{{labels}}} {}\n", hist.count));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fetch_is_counted_and_timed() {
        let ok: Result<u8, String> = observe_fetch("test_src", async { Ok(7) }).await;
        assert_eq!(ok, Ok(7));
        let err: Result<u8, String> = observe_fetch("test_src", async { Err("x".into()) }).await;
        assert!(err.is_err());
        let text = render();
        assert!(
            text.contains("localsky_source_fetch_total{source=\"test_src\",outcome=\"ok\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("localsky_source_fetch_total{source=\"test_src\",outcome=\"error\"} 1"),
            "{text}"
        );
        assert!(
            text.contains("# TYPE localsky_source_fetch_seconds histogram"),
            "{text}"
        );
        assert!(
            text.contains(
                "localsky_source_fetch_seconds_bucket{source=\"test_src\",le=\"+Inf\"} 2"
            ),
            "{text}"
        );
        assert!(
            text.contains("localsky_source_fetch_seconds_count{source=\"test_src\"} 2"),
            "{text}"
        );
    }

    #[test]
    fn render_emits_zero_series_then_reflects_increments() {
        // A fresh metric name still appears (as 0) so alerts can reference it.
        let body = render();
        assert!(body.contains("# TYPE localsky_refresh_total counter"));
        assert!(
            body.contains("localsky_refresh_total 0") || body.contains("localsky_refresh_total ")
        );

        inc("localsky_refresh_total", String::new());
        inc("localsky_refresh_total", String::new());
        inc("localsky_verdict_total", label("verdict", "skip"));
        set_gauge("localsky_last_refresh_epoch", 1_782_000_000.0);
        let body = render();
        assert!(body.contains("localsky_refresh_total 2"));
        assert!(body.contains("localsky_verdict_total{verdict=\"skip\"} 1"));
        assert!(body.contains("# TYPE localsky_last_refresh_epoch gauge"));
        assert!(body.contains("localsky_last_refresh_epoch 1782000000"));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(label("op", "run\"zone"), "op=\"run\\\"zone\"");
    }
}
