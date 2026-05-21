// <Sheet/> viewport-aware bottom-sheet / centered-modal. On mobile
// (width <= 760px) slides up from the bottom edge; on desktop renders
// as a centered modal. Both share the same prop surface so callers
// don't branch on form factor.
//
// Render strategy: emit the markup unconditionally with class:hidden
// gating visibility. Avoids the FnOnce vs Fn issue that <Show> hits
// when children() is consumed inside its body.

use leptos::prelude::*;

#[component]
pub fn Sheet(
    /// Drives the sheet's open/closed state. Setting false animates close.
    open: RwSignal<bool>,
    /// Header title rendered above the body.
    #[prop(into)]
    title: String,
    /// Optional aria-label for the modal region. Defaults to title.
    #[prop(into, optional)]
    aria_label: String,
    /// Click-outside-to-dismiss. Defaults true.
    #[prop(default = true)]
    dismiss_on_backdrop: bool,
    children: Children,
) -> impl IntoView {
    let aria = if aria_label.is_empty() {
        title.clone()
    } else {
        aria_label
    };
    let close = move |_| open.set(false);
    let title_owned = title.clone();
    view! {
        <div
            class="sheet"
            class:sheet--hidden=move || !open.get()
            aria-hidden=move || (!open.get()).to_string()
        >
            <div
                class="sheet__backdrop"
                on:click=move |_| {
                    if dismiss_on_backdrop {
                        open.set(false);
                    }
                }
            />
            <div
                class="sheet__panel"
                role="dialog"
                aria-modal="true"
                aria-label=aria.clone()
            >
                <header class="sheet__header">
                    <div class="sheet__handle" aria-hidden="true"></div>
                    <h2 class="sheet__title">{title_owned.clone()}</h2>
                    <button
                        class="sheet__close"
                        type="button"
                        aria-label="Close"
                        on:click=close
                    >
                        "×"
                    </button>
                </header>
                <div class="sheet__body">{children()}</div>
            </div>
        </div>
    }
}
