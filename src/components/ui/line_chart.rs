// <LineChart/>, multi-series SVG line chart. SVG draws the paths +
// gridlines; HTML draws the legend and axis labels (the pattern the
// history panel already documents: vector data in SVG, text in HTML so
// it inherits fonts/tokens and stays crisp). Pure render, every series
// is pre-computed (x, y) data, auto-scaled across all series so they
// share one coordinate space. Reused by History (YoY, correlation),
// Simulator (baseline vs hypothetical), and zone soil-moisture history.

use leptos::prelude::*;

/// One plotted line. `dashed` dims + dashes it (e.g. prior-year overlay).
#[derive(Clone)]
pub struct Series {
    pub label: String,
    /// CSS color token, e.g. "var(--accent)".
    pub color: String,
    pub points: Vec<(f64, f64)>,
    pub dashed: bool,
}

impl Series {
    pub fn new(
        label: impl Into<String>,
        color: impl Into<String>,
        points: Vec<(f64, f64)>,
    ) -> Self {
        Self {
            label: label.into(),
            color: color.into(),
            points,
            dashed: false,
        }
    }
    pub fn dashed(mut self) -> Self {
        self.dashed = true;
        self
    }
}

#[component]
pub fn LineChart(
    series: Vec<Series>,
    /// Logical height in px (width is fluid via viewBox). Default 220.
    #[prop(default = 220u32)]
    height: u32,
    /// Show the legend row.
    #[prop(default = true)]
    legend: bool,
    /// Optional unit suffix for the y-axis max/min labels.
    #[prop(into, optional)]
    y_unit: String,
    /// Optional x-axis display labels, indexed by point position of the
    /// first series (all current callers share one x domain). Shown as
    /// the tooltip header while scrubbing.
    #[prop(optional)]
    x_labels: Vec<String>,
) -> impl IntoView {
    const W: f64 = 600.0;
    let h = height as f64;
    let pad_l = 4.0;
    let pad_t = 8.0;
    let pad_b = 8.0;
    let plot_h = h - pad_t - pad_b;

    // Bounds across all series.
    let all: Vec<(f64, f64)> = series
        .iter()
        .flat_map(|s| s.points.iter().cloned())
        .collect();
    let (xmin, xmax, ymin, ymax) = bounds(&all);
    let xspan = (xmax - xmin).max(f64::EPSILON);
    let yspan = (ymax - ymin).max(f64::EPSILON);

    let project = move |(x, y): (f64, f64)| {
        let px = pad_l + ((x - xmin) / xspan) * (W - 2.0 * pad_l);
        let py = pad_t + (1.0 - (y - ymin) / yspan) * plot_h;
        (px, py)
    };

    let paths: Vec<_> = series
        .iter()
        .map(|s| {
            let mut d = String::new();
            for (i, p) in s.points.iter().enumerate() {
                let (px, py) = project(*p);
                if i == 0 {
                    d.push_str(&format!("M {px:.2} {py:.2}"));
                } else {
                    d.push_str(&format!(" L {px:.2} {py:.2}"));
                }
            }
            (d, s.color.clone(), s.dashed)
        })
        .collect();

    // Three horizontal gridlines (0/50/100% of plot).
    let grid: Vec<f64> = [0.0_f64, 0.5, 1.0]
        .iter()
        .map(|f| pad_t + f * plot_h)
        .collect();

    let legend_items: Vec<_> = series
        .iter()
        .map(|s| (s.label.clone(), s.color.clone(), s.dashed))
        .collect();
    let ymax_label = format!("{:.0}{}", ymax, y_unit);
    let ymin_label = format!("{:.0}{}", ymin, y_unit);
    let empty = all.is_empty();

    // Scrub state: pointer x as a 0..1 fraction of the plot width. None
    // when the pointer is outside. Hover only ever changes client-side,
    // so SSR and the first hydrate frame render no crosshair (DOM match).
    let hover: RwSignal<Option<f64>> = RwSignal::new(None);
    // Up to five evenly spaced x labels rendered as a real axis row
    // (the full label set still drives the scrub tooltip).
    let x_ticks: Vec<String> = if x_labels.len() >= 2 {
        let n = x_labels.len();
        let count = 5.min(n);
        (0..count)
            .map(|i| x_labels[(i * (n - 1)) / (count - 1)].clone())
            .collect()
    } else {
        Vec::new()
    };
    let lookup = StoredValue::new((series.clone(), x_labels, y_unit.clone()));

    let on_move = move |ev: leptos::ev::PointerEvent| {
        #[cfg(feature = "hydrate")]
        {
            use wasm_bindgen::JsCast;
            if let Some(t) = ev.current_target() {
                if let Ok(el) = t.dyn_into::<web_sys::Element>() {
                    let r = el.get_bounding_client_rect();
                    if r.width() > 0.0 {
                        let fx = ((ev.client_x() as f64 - r.left()) / r.width()).clamp(0.0, 1.0);
                        hover.set(Some(fx));
                    }
                }
            }
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = &ev;
    };
    let on_leave = move |_| hover.set(None);

    let scrub = move || {
        let fx = hover.get()?;
        if empty {
            return None;
        }
        let xv = xmin + fx * xspan;
        let (rows, header) = lookup.with_value(|(series, x_labels, y_unit)| {
            let rows: Vec<(String, String, String)> = series
                .iter()
                .filter_map(|s| {
                    let (idx, &(_, y)) = s.points.iter().enumerate().min_by(|(_, a), (_, b)| {
                        (a.0 - xv).abs().partial_cmp(&(b.0 - xv).abs()).unwrap()
                    })?;
                    let _ = idx;
                    Some((s.label.clone(), s.color.clone(), format!("{y:.1}{y_unit}")))
                })
                .collect();
            let header = series.first().and_then(|s| {
                let (idx, _) = s.points.iter().enumerate().min_by(|(_, a), (_, b)| {
                    (a.0 - xv).abs().partial_cmp(&(b.0 - xv).abs()).unwrap()
                })?;
                x_labels.get(idx).cloned()
            });
            (rows, header)
        });
        if rows.is_empty() {
            return None;
        }
        // Flip the tooltip to the left of the crosshair past 60% so it
        // never clips the right edge.
        let pct = fx * 100.0;
        let tip_style = if fx > 0.6 {
            format!("right:{:.1}%;", 100.0 - pct + 1.5)
        } else {
            format!("left:{:.1}%;", pct + 1.5)
        };
        Some(view! {
            <div class="ui-line-chart__cross" style=format!("left:{pct:.2}%")></div>
            <div class="ui-line-chart__tip" style=tip_style>
                {header.map(|h| view! { <div class="ui-line-chart__tip-head">{h}</div> })}
                {rows.into_iter().map(|(label, color, val)| view! {
                    <div class="ui-line-chart__tip-row">
                        <span class="ui-line-chart__swatch" style=format!("background:{color}")></span>
                        <span class="ui-line-chart__tip-label">{label}</span>
                        <span class="ui-line-chart__tip-val">{val}</span>
                    </div>
                }).collect_view()}
            </div>
        })
    };

    view! {
        <div class="ui-line-chart">
            <div class="ui-line-chart__plot"
                on:pointermove=on_move
                on:pointerleave=on_leave
            >
                <svg
                    class="ui-line-chart__svg"
                    viewBox=format!("0 0 {W} {h}")
                    preserveAspectRatio="none"
                    aria-hidden="true"
                >
                    {grid.into_iter().map(|gy| view! {
                        <line class="ui-line-chart__grid" x1="0" x2=W.to_string() y1=gy.to_string() y2=gy.to_string()/>
                    }).collect_view()}
                    {paths.into_iter().map(|(d, color, dashed)| view! {
                        <path
                            class="ui-line-chart__line"
                            class:ui-line-chart__line--dashed=dashed
                            d=d
                            style=format!("stroke:{color}")
                        />
                    }).collect_view()}
                </svg>
                {(!empty).then(|| view! {
                    <div class="ui-line-chart__yaxis">
                        <span>{ymax_label}</span>
                        <span>{ymin_label}</span>
                    </div>
                })}
                {scrub}
            </div>
            {(!empty && !x_ticks.is_empty()).then(|| view! {
                <div class="ui-line-chart__xaxis">
                    {x_ticks.into_iter().map(|t| view! { <span>{t}</span> }).collect_view()}
                </div>
            })}
            {(legend && !legend_items.is_empty()).then(|| view! {
                <div class="ui-line-chart__legend">
                    {legend_items.into_iter().map(|(label, color, dashed)| view! {
                        <span class="ui-line-chart__legend-item">
                            <span
                                class="ui-line-chart__swatch"
                                class:ui-line-chart__swatch--dashed=dashed
                                style=format!("background:{color}")
                            ></span>
                            {label}
                        </span>
                    }).collect_view()}
                </div>
            })}
        </div>
    }
}

fn bounds(pts: &[(f64, f64)]) -> (f64, f64, f64, f64) {
    if pts.is_empty() {
        return (0.0, 1.0, 0.0, 1.0);
    }
    let mut xmin = f64::INFINITY;
    let mut xmax = f64::NEG_INFINITY;
    let mut ymin = f64::INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    for &(x, y) in pts {
        xmin = xmin.min(x);
        xmax = xmax.max(x);
        ymin = ymin.min(y);
        ymax = ymax.max(y);
    }
    // Pad the y-range a touch so peaks don't clip the top edge; never
    // pad below zero for non-negative data (a "-11 min" axis label is
    // nonsense).
    let pad = (ymax - ymin) * 0.08;
    let padded_min = if ymin >= 0.0 {
        (ymin - pad).max(0.0)
    } else {
        ymin - pad
    };
    (xmin, xmax, padded_min, ymax + pad)
}
