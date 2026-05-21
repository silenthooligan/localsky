// <Card/> claymorphic surface. Sits a half-step lifted off the panel
// background. Used inside .panel-grid layouts (zone cards, forecast
// tiles, etc).
//
// For clickable cards, wrap with a <button> or <a> in the caller; we
// don't take an on_click prop because Leptos's Send requirements
// complicate the closure type without much payoff.

use leptos::prelude::*;

#[component]
pub fn Card(
    /// Compact variant trims padding to ~60% of the default.
    #[prop(default = false)]
    compact: bool,
    children: Children,
) -> impl IntoView {
    let base_class = if compact { "card card--compact" } else { "card" };
    view! {
        <div class=base_class>
            {children()}
        </div>
    }
}
