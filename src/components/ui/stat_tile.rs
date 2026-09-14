// <StatTile/>, label + big number + unit, with an optional inline
// sparkline. The unit of the new dense dashboards
// (weather telemetry + irrigation KPIs). Pure render.

use leptos::prelude::*;

use crate::components::ui::Sparkline;

#[component]
pub fn StatTile(
    /// Eyebrow label (e.g. "HUMIDITY").
    #[prop(into)]
    label: String,
    /// The primary value already formatted (e.g. "72").
    #[prop(into)]
    value: Signal<String>,
    /// Trailing unit (e.g. "%", "°F", "mph"). Optional.
    #[prop(into, optional)]
    unit: Signal<String>,
    /// card | compact | inline | hero; one metric anatomy across surfaces.
    #[prop(default = "card")]
    layout: &'static str,
    #[prop(into, optional)] detail: String,
    #[prop(into, optional)] detail_class: String,
    #[prop(into, optional)] value_class: String,
    #[prop(into, optional)] role: Option<&'static str>,
    /// Optional leading icon name.
    #[prop(into, optional)]
    icon: Option<&'static str>,
    /// Optional sparkline series.
    #[prop(optional)]
    spark: Option<Vec<f64>>,
    /// Sparkline / icon accent token. Default --accent.
    #[prop(into, default = "var(--accent)".to_string())]
    accent: String,
) -> impl IntoView {
    let class = match layout {
        "compact" => "stat-tile stat-tile--compact",
        "inline" => "stat-tile stat-tile--inline",
        "hero" => "stat-tile stat-tile--hero",
        _ => "stat-tile",
    };
    let detail_class = format!("stat-tile__detail {detail_class}");
    let value_class = format!("stat-tile__value {value_class}");
    let icon_accent = accent.clone();
    view! {
        <div class=class role=role style=format!("--tile-accent:{accent}")>
            <div class="stat-tile__head">
                {icon.map(|n| view! {
                    <span class="stat-tile__icon" style=format!("color:{icon_accent}")>
                        <crate::components::ui::Icon name=n size=15/>
                    </span>
                })}
                <span class="stat-tile__label">{label}</span>
            </div>
            <div class="stat-tile__value-row">
                <span class=value_class>{move || value.get()}</span>
                <Show when=move || !unit.get().is_empty()><span class="stat-tile__unit">{move || unit.get()}</span></Show>
            </div>
            {(!detail.is_empty()).then(|| view! { <span class=detail_class>{detail}</span> })}
            {spark.map(|pts| view! {
                <div class="stat-tile__spark">
                    <Sparkline points=pts accent=accent.clone() height=30/>
                </div>
            })}
        </div>
    }
}
