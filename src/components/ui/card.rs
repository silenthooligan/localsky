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
    /// Compact variant trims padding.
    #[prop(default = false)]
    compact: bool,
    /// Interactive: hover lift + pointer affordance, for clickable cards.
    #[prop(default = false)]
    interactive: bool,
    /// Accent: a grad-flow identity stripe along the top edge, for hero /
    /// featured cards (the signature blue->teal treatment from the logomark).
    #[prop(default = false)]
    accent: bool,
    children: Children,
) -> impl IntoView {
    let mut class = String::from("card");
    if compact {
        class.push_str(" card--compact");
    }
    if interactive {
        class.push_str(" card--interactive");
    }
    if accent {
        class.push_str(" card--accent");
    }
    view! {
        <div class=class>
            {children()}
        </div>
    }
}
