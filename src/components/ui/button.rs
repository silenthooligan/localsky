// <Button/> — the one button primitive. Variants unify the old ad-hoc
// `.btn-clay*` family and bespoke range buttons. Sizes sm/md/lg. A
// `loading` flag swaps the label for a spinner and disables the button.
//
// Click handling uses an optional Callback so the primitive owns the
// disabled/loading guard (a caller can't fire a handler on a disabled
// button). For navigation, pass `href` and it renders an <a>.

use leptos::ev::MouseEvent;
use leptos::prelude::*;

#[component]
pub fn Button(
    /// primary | secondary | ghost | danger
    #[prop(into, default = "primary".to_string())]
    variant: String,
    /// sm | md | lg
    #[prop(into, default = "md".to_string())]
    size: String,
    /// Optional leading icon name (see ui::Icon).
    #[prop(into, optional)]
    icon: Option<&'static str>,
    /// Disabled state.
    #[prop(into, optional)]
    disabled: Signal<bool>,
    /// Loading state — shows a spinner and blocks clicks.
    #[prop(into, optional)]
    loading: Signal<bool>,
    /// Full-width block button.
    #[prop(default = false)]
    block: bool,
    /// If set, renders an <a href> instead of a <button>.
    #[prop(into, optional)]
    href: Option<String>,
    /// Click handler (ignored while disabled/loading).
    #[prop(into, optional)]
    on_click: Option<Callback<MouseEvent>>,
    /// aria-label when the button has no readable text (icon-only).
    #[prop(into, optional)]
    aria_label: String,
    children: Children,
) -> impl IntoView {
    let class = move || {
        let mut c = format!("btn btn--{variant} btn--{size}");
        if block {
            c.push_str(" btn--block");
        }
        c
    };
    let icon_view = icon.map(
        |n| view! { <span class="btn__icon"><crate::components::ui::Icon name=n size=16/></span> },
    );

    if let Some(href) = href {
        let cls = class();
        view! {
            <a class=cls href=href aria-label=aria_label.clone()>
                {icon_view}
                <span class="btn__label">{children()}</span>
            </a>
        }
        .into_any()
    } else {
        let cls = class();
        let handler = on_click;
        view! {
            <button
                type="button"
                class=cls
                class:btn--loading=move || loading.get()
                aria-label=aria_label.clone()
                disabled=move || disabled.get() || loading.get()
                on:click=move |ev| {
                    if disabled.get() || loading.get() { return; }
                    if let Some(cb) = handler { cb.run(ev); }
                }
            >
                <span class="btn__spinner" aria-hidden="true"></span>
                {icon_view}
                <span class="btn__label">{children()}</span>
            </button>
        }
        .into_any()
    }
}
