// <EmptyState/> for post-wizard pages with no data yet. Always
// actionable: icon + title + 1-line body + a primary CTA.

use leptos::prelude::*;

use crate::components::ui::Icon;

#[component]
pub fn EmptyState(
    /// One-line title.
    #[prop(into)]
    title: String,
    /// One-line explanation.
    #[prop(into)]
    body: String,
    /// CTA label + href. Renders a primary button-styled link.
    #[prop(into)]
    cta_label: String,
    #[prop(into)] cta_href: String,
    /// Optional Icon registry name shown above the title. Decorative.
    #[prop(optional)]
    icon: Option<&'static str>,
) -> impl IntoView {
    view! {
        <div class="ui-empty">
            {icon.map(|name| view! {
                <div class="ui-empty__icon" aria-hidden="true"><Icon name=name size=40/></div>
            })}
            <h3 class="ui-empty__title">{title}</h3>
            <p class="ui-empty__body">{body}</p>
            <a class="ui-empty__cta" href=cta_href>{cta_label}</a>
        </div>
    }
}
