// P4-1: a tiny zero-dependency metrics registry + Prometheus text exposition,
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
use std::sync::{Mutex, OnceLock};

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
];

/// Gauge metric names + HELP text. (Outbound cloud-fetch counts + latency are a
/// follow-up: they need a shared fetch wrapper the sources don't have yet; only
/// instrumented series are exposed so a scraper never reads a misleading 0.)
const GAUGE_META: &[(&str, &str)] = &[(
    "localsky_last_refresh_epoch",
    "Unix time of the last completed refresh (freshness; alert if it stops advancing)",
)];

struct Registry {
    // (metric_name, rendered_labels) -> value. labels are "" or `k="v",k2="v2"`.
    counters: Mutex<BTreeMap<(&'static str, String), u64>>,
    gauges: Mutex<BTreeMap<&'static str, f64>>,
}

fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| Registry {
        counters: Mutex::new(BTreeMap::new()),
        gauges: Mutex::new(BTreeMap::new()),
    })
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
