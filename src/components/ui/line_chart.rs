// <LineChart/> — multi-series SVG line chart. SVG draws the paths +
// gridlines; HTML draws the legend and axis labels (the pattern the
// history panel already documents: vector data in SVG, text in HTML so
// it inherits fonts/tokens and stays crisp). Pure render — every series
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

    view! {
        <div class="ui-line-chart">
            <div class="ui-line-chart__plot">
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
            </div>
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
    // Pad the y-range a touch so peaks don't clip the top edge.
    let pad = (ymax - ymin) * 0.08;
    (xmin, xmax, ymin - pad, ymax + pad)
}
