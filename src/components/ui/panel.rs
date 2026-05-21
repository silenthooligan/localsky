// <Panel/> glass-morphism surface. Wraps the existing `.panel` SCSS so
// every page renders with the same liquid-glass aesthetic without
// hand-coding markup.

use leptos::prelude::*;

#[component]
pub fn Panel(
    /// Optional header title rendered above the body.
    #[prop(into, optional)]
    title: String,
    /// Optional right-aligned badge slot (e.g. status pill).
    #[prop(optional)]
    badge: Option<View<()>>,
    /// True drops internal padding so callers can position content
    /// edge-to-edge (used by HistoryPanel for the Gantt strip).
    #[prop(default = false)]
    flush: bool,
    children: Children,
) -> impl IntoView {
    let class = if flush { "panel panel--flush" } else { "panel" };
    view! {
        <section class=class>
            {(!title.is_empty()).then(|| view! {
                <header class="panel__header">
                    <h2 class="panel__title">{title.clone()}</h2>
                    {badge}
                </header>
            })}
            <div class="panel__body">{children()}</div>
        </section>
    }
}
