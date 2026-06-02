// <StatTile/> — label + big number + unit, with an optional delta arrow
// and an optional inline sparkline. The unit of the new dense dashboards
// (weather telemetry + irrigation KPIs). Pure render.

use leptos::prelude::*;

use crate::components::ui::Sparkline;

/// Whether an upward delta is good (green) or bad (red). Weather deltas
/// are usually neutral; budget/savings deltas carry sentiment.
#[derive(Clone, Copy, PartialEq)]
pub enum DeltaSense {
    Neutral,
    UpGood,
    UpBad,
}

#[component]
pub fn StatTile(
    /// Eyebrow label (e.g. "HUMIDITY").
    #[prop(into)]
    label: String,
    /// The primary value already formatted (e.g. "72").
    #[prop(into)]
    value: String,
    /// Trailing unit (e.g. "%", "°F", "mph"). Optional.
    #[prop(into, optional)]
    unit: String,
    /// Optional leading icon name.
    #[prop(into, optional)]
    icon: Option<&'static str>,
    /// Optional signed delta value already formatted (e.g. "+3.1").
    #[prop(into, optional)]
    delta: Option<String>,
    /// Sentiment for the delta coloring.
    #[prop(default = DeltaSense::Neutral)]
    delta_sense: DeltaSense,
    /// True if the delta is negative (drives arrow direction + sign color).
    #[prop(default = false)]
    delta_down: bool,
    /// Optional sparkline series.
    #[prop(optional)]
    spark: Option<Vec<f64>>,
    /// Sparkline / icon accent token. Default --accent.
    #[prop(into, default = "var(--accent)".to_string())]
    accent: String,
) -> impl IntoView {
    let delta_class = match (delta_sense, delta_down) {
        (DeltaSense::Neutral, _) => "stat-tile__delta",
        (DeltaSense::UpGood, false) | (DeltaSense::UpBad, true) => {
            "stat-tile__delta stat-tile__delta--good"
        }
        (DeltaSense::UpGood, true) | (DeltaSense::UpBad, false) => {
            "stat-tile__delta stat-tile__delta--bad"
        }
    };
    let arrow = if delta_down { "arrow-down" } else { "arrow-up" };
    let has_unit = !unit.is_empty();
    let icon_accent = accent.clone();
    view! {
        <div class="stat-tile">
            <div class="stat-tile__head">
                {icon.map(|n| view! {
                    <span class="stat-tile__icon" style=format!("color:{icon_accent}")>
                        <crate::components::ui::Icon name=n size=15/>
                    </span>
                })}
                <span class="stat-tile__label">{label}</span>
            </div>
            <div class="stat-tile__value-row">
                <span class="stat-tile__value">{value}</span>
                {has_unit.then(|| view! { <span class="stat-tile__unit">{unit}</span> })}
                {delta.map(|d| view! {
                    <span class=delta_class>
                        <crate::components::ui::Icon name=arrow size=12/>
                        {d}
                    </span>
                })}
            </div>
            {spark.map(|pts| view! {
                <div class="stat-tile__spark">
                    <Sparkline points=pts accent=accent.clone() height=30/>
                </div>
            })}
        </div>
    }
}
