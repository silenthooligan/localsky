// <Sparkline/>, inline single-series SVG trend line. Pure render: takes
// a pre-computed Vec<f64> and draws a normalized polyline that fills its
// box (preserveAspectRatio=none so it stretches to any width). No axes,
// no labels, it's a glance-distance shape, not a chart. The dashboard
// workhorse; promoted from the `.sparkline` CSS class.
//
// SSR-safe: the path string is computed from owned data at render time,
// so the SSR frame and the hydrate frame produce identical markup.

use leptos::prelude::*;

#[component]
pub fn Sparkline(
    /// Y values, left to right. Fewer than 2 points renders an empty box.
    points: Vec<f64>,
    /// Accent CSS color (token like "var(--accent)"). Default --accent.
    #[prop(into, default = "var(--accent)".to_string())]
    accent: String,
    /// Render a soft area fill under the line.
    #[prop(default = true)]
    fill: bool,
    /// Logical height in px (width is fluid). Default 36.
    #[prop(default = 36u32)]
    height: u32,
) -> impl IntoView {
    const W: f64 = 100.0;
    let h = height as f64;
    let line = build_path(&points, W, h);
    let area = if fill && line.is_some() {
        line.as_ref().map(|p| format!("{p} L {W} {h} L 0 {h} Z"))
    } else {
        None
    };
    let style = format!("--spark-accent:{accent};height:{height}px;");
    view! {
        <svg
            class="ui-sparkline"
            viewBox=format!("0 0 {W} {h}")
            preserveAspectRatio="none"
            style=style
            aria-hidden="true"
        >
            {area.map(|d| view! { <path class="ui-sparkline__area" d=d/> })}
            {line.map(|d| view! { <path class="ui-sparkline__line" d=d/> })}
        </svg>
    }
}

/// Normalize points into an SVG path string spanning [0,w] × [0,h]
/// (y inverted so higher values sit toward the top). None if < 2 points.
fn build_path(points: &[f64], w: f64, h: f64) -> Option<String> {
    if points.len() < 2 {
        return None;
    }
    let min = points.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = points.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let span = (max - min).max(f64::EPSILON);
    let pad = h * 0.12;
    let n = (points.len() - 1) as f64;
    let mut d = String::with_capacity(points.len() * 12);
    for (i, v) in points.iter().enumerate() {
        let x = (i as f64 / n) * w;
        let y = h - pad - ((v - min) / span) * (h - 2.0 * pad);
        if i == 0 {
            d.push_str(&format!("M {x:.2} {y:.2}"));
        } else {
            d.push_str(&format!(" L {x:.2} {y:.2}"));
        }
    }
    Some(d)
}
