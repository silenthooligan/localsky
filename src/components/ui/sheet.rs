// <Sheet/> viewport-aware bottom-sheet / centered-modal. On mobile
// (width <= 760px) slides up from the bottom edge; on desktop renders
// as a centered modal. Both share the same prop surface so callers
// don't branch on form factor.
//
// Render strategy: emit the markup unconditionally with class:hidden
// gating visibility. Avoids the FnOnce vs Fn issue that <Show> hits
// when children() is consumed inside its body.
//
// A11y: on open, focus moves to the close button and the previously
// focused element is remembered; Escape closes; Tab cycles within the
// panel (first/last wrap trap, pulling focus back in if it escaped to
// the body); on close, focus is restored. The keydown handling lives on
// a DOCUMENT-level listener registered only while the sheet is open: a
// listener on the sheet wrapper stops hearing keys as soon as focus
// lands outside the panel (e.g. a tap on non-focusable sheet text moves
// focus to <body>), which used to strand Escape and let Tab walk the
// aria-hidden page behind the modal. All of it is hydrate-only DOM
// work, so SSR markup is unchanged.

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
    /// Optional DOM id for the panel, so an external toggle can point
    /// `aria-controls` at it. Omitted attribute when empty.
    #[prop(into, optional)]
    id: String,
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
    let panel_id = (!id.is_empty()).then_some(id);
    let close = move |_| open.set(false);
    let title_owned = title.clone();
    let panel: NodeRef<leptos::html::Div> = NodeRef::new();

    // Focus management: remember the opener, focus the panel's close
    // button on open, restore on close.
    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::JsCast;
        let prev_focus: StoredValue<Option<web_sys::HtmlElement>> = StoredValue::new(None);
        Effect::new(move |_| {
            let is_open = open.get();
            let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
                return;
            };
            if is_open {
                prev_focus.set_value(
                    doc.active_element()
                        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok()),
                );
                if let Some(panel_el) = panel.get() {
                    let el: &web_sys::Element = panel_el.as_ref();
                    if let Ok(Some(btn)) = el.query_selector(".sheet__close") {
                        if let Ok(btn) = btn.dyn_into::<web_sys::HtmlElement>() {
                            let _ = btn.focus();
                        }
                    }
                }
            } else if let Some(prev) = prev_focus.with_value(|p| p.clone()) {
                let _ = prev.focus();
                prev_focus.set_value(None);
            }
        });
    }

    // Escape closes; Tab wraps within the panel's focusable elements.
    // Registered on document while open (see header comment) so the trap
    // holds even after focus leaves the panel; detached on close and on
    // component cleanup.
    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        type KeyClosure = Closure<dyn FnMut(leptos::ev::KeyboardEvent)>;
        // new_local: wasm Closures are !Send, so the default SyncStorage
        // arena rejects them; the listener only ever lives on this thread.
        let key_handler = StoredValue::new_local(None::<KeyClosure>);

        let detach = move || {
            if let Some(cb) = key_handler.try_update_value(|h| h.take()).flatten() {
                if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                    let _ = doc.remove_event_listener_with_callback(
                        "keydown",
                        cb.as_ref().unchecked_ref(),
                    );
                }
            }
        };

        Effect::new(move |_| {
            if !open.get() {
                detach();
                return;
            }
            // Already attached (defensive: also treats a disposed
            // StoredValue as attached so nothing new leaks).
            if key_handler.try_with_value(|h| h.is_some()).unwrap_or(true) {
                return;
            }
            let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
                return;
            };
            let cb: KeyClosure = Closure::new(move |ev: leptos::ev::KeyboardEvent| {
                sheet_keydown(&ev, open, panel);
            });
            if doc
                .add_event_listener_with_callback("keydown", cb.as_ref().unchecked_ref())
                .is_ok()
            {
                key_handler.set_value(Some(cb));
            }
        });
        on_cleanup(detach);
    }

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
                id=panel_id
                role="dialog"
                aria-modal="true"
                aria-label=aria.clone()
                node_ref=panel
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

/// Document-level keydown handling for an open sheet: Escape closes,
/// Tab is trapped inside the panel. Runs for keydowns anywhere in the
/// document, so it keeps working after focus has moved to <body> (the
/// failure mode of a wrapper-scoped listener).
#[cfg(feature = "hydrate")]
fn sheet_keydown(
    ev: &leptos::ev::KeyboardEvent,
    open: RwSignal<bool>,
    panel: NodeRef<leptos::html::Div>,
) {
    use wasm_bindgen::JsCast;

    if !open.get_untracked() {
        return;
    }
    let key = ev.key();
    if key == "Escape" {
        ev.prevent_default();
        open.set(false);
        return;
    }
    if key != "Tab" {
        return;
    }
    let Some(panel_el) = panel.get_untracked() else {
        return;
    };
    let el: &web_sys::Element = panel_el.as_ref();
    // :not([disabled]) keeps the wrap boundary on elements that can
    // actually hold focus; a disabled first/last control used to break
    // the trap edges.
    let Ok(focusables) = el.query_selector_all(
        "button:not([disabled]), [href], input:not([disabled]), select:not([disabled]), \
         textarea:not([disabled]), [tabindex]:not([tabindex='-1'])",
    ) else {
        return;
    };
    if focusables.length() == 0 {
        return;
    }
    let first = focusables
        .item(0)
        .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok());
    let last = focusables
        .item(focusables.length() - 1)
        .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok());
    let (Some(first), Some(last)) = (first, last) else {
        return;
    };
    let active = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.active_element());
    // Focus escaped the dialog (body, or something behind the modal):
    // pull it back to the first focusable instead of letting Tab walk
    // the aria-hidden background.
    let inside = active
        .as_ref()
        .map(|a| el.contains(Some(a.as_ref())))
        .unwrap_or(false);
    if !inside {
        ev.prevent_default();
        let _ = first.focus();
        return;
    }
    let Some(active) = active else {
        return;
    };
    if ev.shift_key() {
        if active.is_same_node(Some(first.as_ref())) {
            ev.prevent_default();
            let _ = last.focus();
        }
    } else if active.is_same_node(Some(last.as_ref())) {
        ev.prevent_default();
        let _ = first.focus();
    }
}
