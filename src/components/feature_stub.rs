// Placeholder shell for marquee screens that are routed + reachable but
// not yet built out. Gives each new top-level destination an intentional
// header (eyebrow + title + blurb) and an icon, so the nav never points
// at a 404 during the screen-by-screen migration. Each real screen
// replaces the body that renders this.

use leptos::prelude::*;

use crate::components::ui::Icon;

#[component]
pub fn FeatureStub(
    #[prop(into)] eyebrow: String,
    #[prop(into)] title: String,
    #[prop(into)] blurb: String,
    #[prop(into)] icon: &'static str,
) -> impl IntoView {
    view! {
        <div class="feature-stub">
            <div class="feature-stub__badge"><Icon name=icon size=28/></div>
            <p class="feature-stub__eyebrow">{eyebrow}</p>
            <h1 class="feature-stub__title">{title}</h1>
            <p class="feature-stub__blurb">{blurb}</p>
            <p class="feature-stub__note">"Coming online — wiring up live data."</p>
        </div>
    }
}
