// Pressure panel: current value + a 6-hour SVG sparkline pulled from the
// rolling buffer the listener maintains. Up arrow / flat / down arrow
// derived from the slope across the last hour.

use crate::components::units_fmt::{pressure_unit, pressure_value, use_unit_prefs, UnitPrefs};
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

// Source refreshes can arrive in a different order from their observation times.
// Normalize only the presentation; keep the source buffer and arbitration intact.
fn chronological_readings(raw: &[(i64, f64)]) -> Vec<(i64, f64)> {
    raw.iter()
        .copied()
        .filter(|(t, v)| *t > 0 && v.is_finite() && *v > 0.0)
        .collect::<std::collections::BTreeMap<_, _>>()
        .into_iter()
        .collect()
}

fn trend(pts: &[(i64, f64)]) -> (&'static str, &'static str) {
    if pts.len() < 2 {
        return ("—", "collecting");
    }
    let last = pts.last().copied().unwrap();
    let earlier = pts
        .iter()
        .rev()
        .find(|(t, _)| *t <= last.0 - 3600)
        .copied()
        .unwrap_or(pts[0]);
    match last.1 - earlier.1 {
        delta if delta > 0.02 => ("↑", "rising"),
        delta if delta < -0.02 => ("↓", "falling"),
        _ => ("→", "steady"),
    }
}

fn pressure_path(pts: &[(i64, f64)]) -> (String, f64, f64) {
    if pts.len() < 2 {
        return (String::new(), 0.0, 0.0);
    }
    let (min_t, max_t) = (pts[0].0, pts.last().unwrap().0);
    let observed_min = pts.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min);
    let observed_max = pts
        .iter()
        .map(|(_, v)| *v)
        .fold(f64::NEG_INFINITY, f64::max);
    let pad = if observed_max - observed_min < 0.005 {
        0.05
    } else {
        0.0
    };
    let min_v = observed_min - pad;
    let dy = observed_max + pad - min_v;
    let dx = (max_t - min_t).max(1) as f64;
    let d = pts
        .iter()
        .enumerate()
        .map(|(i, (t, v))| {
            // Inset the stroke so an extreme or flat reading never clips at the edge.
            let x = 2.0 + (*t - min_t) as f64 / dx * 196.0;
            let y = 58.0 - (*v - min_v) / dy * 56.0;
            format!("{} {x:.2} {y:.2}", if i == 0 { "M" } else { "L" })
        })
        .collect::<Vec<_>>()
        .join(" ");
    (d, observed_min, observed_max)
}

#[component]
pub fn PressurePanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let readings = Memo::new(move |_| chronological_readings(&snap.get().pressure_trend_inhg));
    let trend_arrow = move || readings.with(|pts| trend(pts));

    view! {
        <section class="panel pressure">
            <h2 class="panel-title">"Pressure"</h2>
            <div class="pressure-row">
                <div class="big-number">
                    {move || pressure_value(snap.get().pressure_inhg, prefs.get())}
                    <span class="big-unit">{move || format!(" {}", pressure_unit(prefs.get()))}</span>
                </div>
                <div class={move || format!("trend trend-{}", trend_arrow().1)}>
                    <span class="trend-arrow">{move || trend_arrow().0}</span>
                    <span class="trend-label">{move || trend_arrow().1}</span>
                </div>
            </div>
            {move || view! { <Sparkline readings prefs=prefs.get()/> }}
        </section>
    }
}

#[component]
fn Sparkline(readings: Memo<Vec<(i64, f64)>>, prefs: UnitPrefs) -> impl IntoView {
    let path = Memo::new(move |_| readings.with(|pts| pressure_path(pts)));

    view! {
        <Show when=move || { readings.with(|pts| pts.len() >= 2) } fallback=|| view! { <p class="chart-note">"Collecting pressure readings for the trend."</p> }>
        <div class="chart-key"><crate::components::ui::ChartKey label="Pressure · recent 6 hours" color="var(--chart-pressure)"/></div>
        <svg aria-hidden="true" class="sparkline" viewBox="0 0 200 60" preserveAspectRatio="none">
            <path d=move || path.get().0 class="sparkline-path"/>
        </svg>
        <div class="sparkline-axis">
            <span>{move || format!("Low {} {}", pressure_value(path.get().1, prefs), pressure_unit(prefs))}</span>
            <span>{move || format!("High {} {}", pressure_value(path.get().2, prefs), pressure_unit(prefs))}</span>
        </div>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_source_readings_stay_visible_and_use_observation_time_for_trend() {
        let pts =
            chronological_readings(&[(7200, 30.02), (3600, 29.97), (7199, 29.96), (7260, 30.03)]);
        assert_eq!(trend(&pts).1, "rising");
        let (path, low, high) = pressure_path(&pts);
        assert_eq!((low, high), (29.96, 30.03));
        let coordinates: Vec<f64> = path
            .split_whitespace()
            .filter_map(|s| s.parse().ok())
            .collect();
        let mut previous_x = 0.0;
        for point in coordinates.chunks_exact(2) {
            assert!((2.0..=198.0).contains(&point[0]));
            assert!((2.0..=58.0).contains(&point[1]));
            assert!(point[0] >= previous_x);
            previous_x = point[0];
        }
        assert_eq!(previous_x, 198.0);
    }

    #[test]
    fn duplicate_and_invalid_samples_do_not_invent_a_trend() {
        let pts = chronological_readings(&[
            (1, 30.0),
            (1, 30.1),
            (0, 30.0),
            (2, f64::NAN),
            (3, f64::INFINITY),
        ]);
        assert_eq!(pts, vec![(1, 30.1)]);
        assert_eq!(trend(&pts).1, "collecting");
        assert!(pressure_path(&pts).0.is_empty());
        assert_eq!(
            pressure_path(&[(1, 30.0), (2, 30.0)]).0,
            "M 2.00 30.00 L 198.00 30.00"
        );
    }
}
