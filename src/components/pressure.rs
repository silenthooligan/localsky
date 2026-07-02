// Pressure panel: current value + a 6-hour SVG sparkline pulled from the
// rolling buffer the listener maintains. Up arrow / flat / down arrow
// derived from the slope across the last hour.

use crate::components::units_fmt::{pressure_unit, pressure_value, use_unit_prefs, UnitPrefs};
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn PressurePanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let trend_arrow = move || {
        let s = snap.get();
        let pts = &s.pressure_trend_inhg;
        if pts.len() < 2 {
            return ("→", "flat");
        }
        // Compare the most recent sample to the one closest to ~1h before it.
        let last = pts.last().copied().unwrap();
        let one_hour_ago = last.0 - 3600;
        let earlier = pts
            .iter()
            .rev()
            .find(|(t, _)| *t <= one_hour_ago)
            .copied()
            .unwrap_or_else(|| pts.first().copied().unwrap());
        let delta = last.1 - earlier.1;
        if delta > 0.02 {
            ("↑", "rising")
        } else if delta < -0.02 {
            ("↓", "falling")
        } else {
            ("→", "steady")
        }
    };

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
            {move || view! { <Sparkline snap prefs=prefs.get()/> }}
        </section>
    }
}

#[component]
fn Sparkline(snap: ReadSignal<Snapshot>, prefs: UnitPrefs) -> impl IntoView {
    let path = Memo::new(move |_| {
        let s = snap.get();
        let pts = &s.pressure_trend_inhg;
        if pts.len() < 2 {
            return (String::new(), 0.0_f64, 0.0_f64);
        }
        let (min_t, max_t) = (pts.first().unwrap().0, pts.last().unwrap().0);
        let mut min_v = f64::MAX;
        let mut max_v = f64::MIN;
        for (_, v) in pts {
            min_v = min_v.min(*v);
            max_v = max_v.max(*v);
        }
        // Pad the y range a hair so flat lines don't render as a divide-by-zero.
        if (max_v - min_v).abs() < 0.005 {
            min_v -= 0.05;
            max_v += 0.05;
        }
        let dx = (max_t - min_t).max(1) as f64;
        let dy = max_v - min_v;
        let mut d = String::new();
        for (i, (t, v)) in pts.iter().enumerate() {
            let x = ((t - min_t) as f64 / dx) * 200.0;
            let y = 60.0 - ((v - min_v) / dy) * 60.0;
            if i == 0 {
                d.push_str(&format!("M {:.2} {:.2}", x, y));
            } else {
                d.push_str(&format!(" L {:.2} {:.2}", x, y));
            }
        }
        (d, min_v, max_v)
    });

    view! {
        <svg class="sparkline" viewBox="0 0 200 60" preserveAspectRatio="none">
            <path d=move || path.get().0 class="sparkline-path"/>
        </svg>
        <div class="sparkline-axis">
            <span>{move || pressure_value(path.get().1, prefs)}</span>
            <span>{move || pressure_value(path.get().2, prefs)}</span>
        </div>
    }
}
