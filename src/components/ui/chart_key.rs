use leptos::prelude::*;

/// A persistent series key outside the scrolling plot. Text and shape carry
/// meaning in every theme, including when the viewer cannot distinguish hues.
#[component]
pub fn ChartKey(
    #[prop(into)] label: String,
    #[prop(into)] color: String,
    #[prop(default = false)] bars: bool,
) -> impl IntoView {
    view! {
        <span class="chart-key__item">
            <span class="chart-key__mark" class:chart-key__mark--bar=bars
                style=format!("--series-color:{color}") aria-hidden="true"></span>
            {label}
        </span>
    }
}
