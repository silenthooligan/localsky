// Phase E soil sensor visibility, per-zone tile showing current
// calibrated moisture, a 7-day projection sparkline, and a colored
// status pill. The projection comes from
// `snap.soil_forecasts[N].predicted_pct` (computed server-side in
// refresher.rs `compute_soil_forecasts`); the line is overlaid on a
// target band drawn from `target_min_pct`..`target_max_pct` so the
// operator can see at a glance whether each zone stays in its healthy
// range on rain + ET alone.
//
// No interaction, these tiles are observational. The user tunes the
// target band via the input_numbers exposed in the HA UI (or, longer
// term, an inline slider on this tile). When a probe is offline
// (current_pct is None) the tile collapses to a "(probe offline)"
// state so the rest of the dashboard isn't dragged into a dead chart.

use crate::components::ui::HelpHint;
use crate::ha::snapshot::{IrrigationSnapshot, SoilForecast};
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn SoilSensors(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // One Signal per zone so a single zone refresh doesn't re-render
    // the others. Matches the ZoneGrid pattern in zones.rs.
    let zone0 = Signal::derive(move || soil_for(&snap.get(), 0));
    let zone1 = Signal::derive(move || soil_for(&snap.get(), 1));
    let zone2 = Signal::derive(move || soil_for(&snap.get(), 2));
    let zone3 = Signal::derive(move || soil_for(&snap.get(), 3));

    view! {
        <section class="soil-grid">
            <h2 class="soil-grid-title">
                "Soil Sensors · 7-day projection"
                <HelpHint topic="soil-sensors"/>
            </h2>
            <p class="soil-grid-sub">
                "If no irrigation runs this week, here\u{2019}s where each zone lands. \
                 Green band is the healthy range (target min \u{2192} saturation). \
                 Rain + ET only; the live skip-check still gates real runs."
            </p>
            <div class="soil-tile-grid">
                {view! { <SoilTile zone=zone0/> }.into_any()}
                {view! { <SoilTile zone=zone1/> }.into_any()}
                {view! { <SoilTile zone=zone2/> }.into_any()}
                {view! { <SoilTile zone=zone3/> }.into_any()}
            </div>
        </section>
    }
}

fn soil_for(snap: &IrrigationSnapshot, idx: usize) -> SoilForecast {
    snap.soil_forecasts.get(idx).cloned().unwrap_or_default()
}

#[component]
fn SoilTile(zone: Signal<SoilForecast>) -> impl IntoView {
    let tile_class = move || {
        let status = zone.get().status;
        match status.as_str() {
            "dry" => "soil-tile soil-tile-dry",
            "wet" => "soil-tile soil-tile-wet",
            "ok" => "soil-tile soil-tile-ok",
            _ => "soil-tile soil-tile-no-data",
        }
    };
    let badge = move || {
        let status = zone.get().status;
        match status.as_str() {
            "dry" => "DRY",
            "wet" => "WET",
            "ok" => "OK",
            _ => "OFFLINE",
        }
    };
    let current_display = move || match zone.get().current_pct {
        Some(p) => format!("{:.1}%", p),
        None => "-".to_string(),
    };
    let band_display = move || {
        let z = zone.get();
        format!("{:.0}-{:.0}%", z.target_min_pct, z.target_max_pct)
    };
    let trend_display = move || {
        let z = zone.get();
        let p = &z.predicted_pct;
        if p.len() < 2 {
            return "-".to_string();
        }
        let delta = p[p.len() - 1] - p[0];
        // Right-arrow + signed delta. > 1 pct = "up/down", otherwise "flat".
        if delta > 1.0 {
            format!("\u{2197} +{:.1} pts in 7d", delta)
        } else if delta < -1.0 {
            format!("\u{2198} {:.1} pts in 7d", delta)
        } else {
            "\u{2192} flat".to_string()
        }
    };
    let min_display = move || format!("min {:.0}%", zone.get().min_predicted_pct);
    let days_dry_display = move || {
        let n = zone.get().days_below_target;
        if n == 0 {
            "stays in band".to_string()
        } else if n == 1 {
            "1 day below target".to_string()
        } else {
            format!("{n} days below target")
        }
    };

    view! {
        <article class=tile_class>
            <header class="soil-tile-head">
                <h3 class="soil-tile-name">{move || zone.get().zone_name}</h3>
                <span class="soil-tile-badge">{badge}</span>
            </header>
            <div class="soil-tile-current">
                <span class="soil-tile-current-value">{current_display}</span>
                <span class="soil-tile-current-band">"target " {band_display}</span>
            </div>
            {view! { <SoilSparkline zone=zone/> }.into_any()}
            <footer class="soil-tile-foot">
                <span class="soil-tile-trend">{trend_display}</span>
                <span class="soil-tile-min">{min_display}</span>
                <span class="soil-tile-dry-days">{days_dry_display}</span>
            </footer>
        </article>
    }
}

#[component]
fn SoilSparkline(zone: Signal<SoilForecast>) -> impl IntoView {
    // SVG viewBox is 200×60; we fit the series + the target band into
    // that space. The y-axis is fixed 0..100 % so the band overlay
    // stays comparable between tiles regardless of each zone's series
    // range. Matches the pressure sparkline (src/components/pressure.rs)
    // for visual consistency.
    let path = Memo::new(move |_| {
        let z = zone.get();
        if z.predicted_pct.is_empty() || z.current_pct.is_none() {
            return String::new();
        }
        let n = z.predicted_pct.len();
        let dx = if n > 1 { 200.0 / (n as f64 - 1.0) } else { 0.0 };
        let mut d = String::new();
        for (i, v) in z.predicted_pct.iter().enumerate() {
            let x = i as f64 * dx;
            // y inverted: high % at top.
            let y = 60.0 - (v / 100.0) * 60.0;
            if i == 0 {
                d.push_str(&format!("M {:.1} {:.1}", x, y));
            } else {
                d.push_str(&format!(" L {:.1} {:.1}", x, y));
            }
        }
        d
    });
    let band_y = Memo::new(move |_| {
        let z = zone.get();
        let top = 60.0 - (z.target_max_pct / 100.0) * 60.0;
        let bottom = 60.0 - (z.target_min_pct / 100.0) * 60.0;
        (top, bottom - top)
    });
    let has_data = move || zone.get().current_pct.is_some();

    view! {
        <svg
            class="soil-sparkline"
            viewBox="0 0 200 60"
            preserveAspectRatio="none"
            role="img"
            aria-label="7-day moisture projection"
        >
            // Target band (drawn first so the line sits on top of it)
            <rect
                class="soil-sparkline-band"
                x="0"
                width="200"
                y=move || format!("{:.1}", band_y.get().0)
                height=move || format!("{:.1}", band_y.get().1)
            />
            // Today marker, vertical tick at x=0
            <line
                class="soil-sparkline-today"
                x1="0" x2="0" y1="0" y2="60"
            />
            <Show when=has_data>
                <path class="soil-sparkline-path" d=move || path.get()/>
            </Show>
        </svg>
    }
}
