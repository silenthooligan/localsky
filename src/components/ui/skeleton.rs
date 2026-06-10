// <Skeleton/> loading placeholders. Shimmering ghost shapes shown while
// a page waits for its first data, replacing bare "Loading…" text. The
// shimmer is pure CSS (gated off under prefers-reduced-motion and solid
// in high-contrast), so SSR and hydrate render identical DOM.

use leptos::prelude::*;

#[component]
pub fn Skeleton(
    /// line | block | tile | chart | row
    #[prop(into, default = "line".to_string())]
    variant: String,
    /// Optional inline width override, e.g. "12rem" or "40%".
    #[prop(into, optional)]
    width: String,
) -> impl IntoView {
    let class = format!("ui-skel ui-skel--{variant}");
    let style = (!width.is_empty()).then(|| format!("width:{width}"));
    view! { <div class=class style=style aria-hidden="true"></div> }
}

/// A stack of skeleton rows, the common "list is loading" shape.
#[component]
pub fn SkeletonRows(#[prop(default = 3)] count: usize) -> impl IntoView {
    view! {
        <div class="ui-skel-rows">
            {(0..count).map(|_| view! { <Skeleton variant="row"/> }).collect_view()}
        </div>
    }
}
