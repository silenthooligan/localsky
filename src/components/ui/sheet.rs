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

/// How the sheet presents itself on a wide viewport. Both variants are
/// the same bottom sheet on a phone, because a side drawer on a 390px
/// screen is just a modal that wastes the edge.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SheetVariant {
    /// Centered dialog. For a decision: a confirm, a short form.
    #[default]
    Modal,
    /// Right-hand drawer. For editing something that lives on the page
    /// behind it, where keeping that context visible is the point.
    Drawer,
}

#[component]
pub fn Sheet(
    /// Drives the sheet's open/closed state. Setting false animates close.
    open: RwSignal<bool>,
    /// Header title rendered above the body. Reactive because an editor
    /// sheet names the thing being edited, which changes with the URL.
    #[prop(into)]
    title: Signal<String>,
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
    /// Modal (centered) or Drawer (side panel on desktop).
    #[prop(optional)]
    variant: SheetVariant,
    /// True while the sheet holds edits that would be lost on close.
    /// When this reads true, every dismissal route asks instead of
    /// closing, and it is `on_dismiss_request` that hears about it.
    #[prop(optional, into)]
    dirty: Option<Signal<bool>>,
    /// Called instead of closing while `dirty` is true. The caller
    /// raises its own confirm and closes the sheet if the answer is yes.
    /// Without this, a dirty sheet closes as it always did, so a caller
    /// that sets neither prop is unaffected.
    #[prop(optional)]
    on_dismiss_request: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    let aria = move || {
        if aria_label.is_empty() {
            title.get()
        } else {
            aria_label.clone()
        }
    };
    let panel_id = (!id.is_empty()).then_some(id);

    // Escape, the scrim and the X all come through here, so a dirty form
    // cannot be lost down one route while another one guards it.
    let request_close = Callback::new(move |()| {
        let is_dirty = dirty.map(|d| d.get_untracked()).unwrap_or(false);
        match dismissal(is_dirty, on_dismiss_request.is_some()) {
            Dismissal::Ask => {
                if let Some(ask) = on_dismiss_request {
                    ask.run(());
                }
            }
            Dismissal::Close => open.set(false),
        }
    });
    let close = move |_| request_close.run(());
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

    // Everything behind the sheet goes inert while it is open.
    //
    // The obvious move, marking <main class="page"> inert, does not work:
    // the sheet renders INSIDE that element, so it would go inert with
    // the page. This walks outward from the sheet to <body> and marks
    // each SIBLING on the way, leaving the ancestor chain alone, which is
    // what the platform does for a dialog in the top layer. Everything
    // marked is remembered so close restores exactly what was changed and
    // never clears an `inert` that was already someone else's.
    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::JsCast;
        // new_local: web_sys::Element is !Send, so the default SyncStorage
        // arena rejects it. Same reason as the keydown closure below.
        let marked: StoredValue<Vec<web_sys::Element>, LocalStorage> =
            StoredValue::new_local(Vec::new());

        let release = move || {
            if let Some(list) = marked.try_update_value(std::mem::take) {
                for el in list {
                    let _ = el.remove_attribute("inert");
                }
            }
        };

        Effect::new(move |_| {
            if !open.get() {
                release();
                return;
            }
            let Some(panel_el) = panel.get() else {
                return;
            };
            let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
                return;
            };
            let body: Option<web_sys::Element> = doc.body().map(|b| b.unchecked_into());
            // Start at the sheet wrapper, which is the panel's parent.
            let panel_ref: &web_sys::Element = panel_el.as_ref();
            let mut node: web_sys::Element = panel_ref.clone();
            if let Some(wrapper) = node.parent_element() {
                node = wrapper;
            }
            let mut hidden = Vec::new();
            while let Some(parent) = node.parent_element() {
                let kids = parent.children();
                for i in 0..kids.length() {
                    let Some(kid) = kids.item(i) else { continue };
                    if kid.is_same_node(Some(node.as_ref())) {
                        continue;
                    }
                    // Never re-mark, and never take ownership of an inert
                    // some other component put there.
                    if kid.has_attribute("inert") {
                        continue;
                    }
                    // Another sheet is not "the page behind": a confirm
                    // raised on top of this one is a sibling on this very
                    // path, and inerting it left a dialog that was visible
                    // and could not be clicked. Sheets under this one are
                    // already unreachable behind its scrim.
                    if kid.class_name().split_whitespace().any(|c| c == "sheet") {
                        continue;
                    }
                    if kid.set_attribute("inert", "").is_ok() {
                        hidden.push(kid);
                    }
                }
                if body
                    .as_ref()
                    .is_some_and(|b| b.is_same_node(Some(parent.as_ref())))
                {
                    break;
                }
                node = parent;
            }
            marked.set_value(hidden);
        });
        on_cleanup(release);
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
                sheet_keydown(&ev, open, panel, request_close);
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
            class:sheet--drawer=move || variant == SheetVariant::Drawer
            class:sheet--open=move || open.get()
            aria-hidden=move || (!open.get()).to_string()
        >
            <div
                class="sheet__backdrop"
                on:click=move |_| {
                    if dismiss_on_backdrop {
                        request_close.run(());
                    }
                }
            />
            <div
                class="sheet__panel"
                id=panel_id
                role="dialog"
                aria-modal="true"
                aria-label=aria
                node_ref=panel
            >
                <header class="sheet__header">
                    <div class="sheet__handle" aria-hidden="true"></div>
                    <h2 class="sheet__title">{move || title.get()}</h2>
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

/// What a dismissal request should do.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dismissal {
    /// Close now.
    Close,
    /// Hand it to the caller, which raises its own confirmation.
    Ask,
}

/// The dismissal rule, in one place because three routes share it:
/// Escape, a click on the scrim, and the close button.
///
/// A sheet only asks when it is BOTH dirty and given somewhere to ask.
/// A caller that sets `dirty` and forgets `on_dismiss_request` would
/// otherwise get a sheet that cannot be closed at all, which is a worse
/// failure than losing a draft.
pub fn dismissal(is_dirty: bool, has_handler: bool) -> Dismissal {
    if is_dirty && has_handler {
        Dismissal::Ask
    } else {
        Dismissal::Close
    }
}

#[cfg(test)]
mod tests {
    use super::{dismissal, Dismissal, SheetVariant};

    /// A sheet with nothing typed in it closes on every route, which is
    /// what every caller that predates the guard relies on.
    #[test]
    fn a_clean_sheet_closes_without_asking() {
        assert_eq!(dismissal(false, true), Dismissal::Close);
        assert_eq!(dismissal(false, false), Dismissal::Close);
    }

    /// The point of the phase: a half-typed form is not thrown away by
    /// the same keystroke that dismisses an empty one.
    #[test]
    fn a_dirty_sheet_asks_first() {
        assert_eq!(dismissal(true, true), Dismissal::Ask);
    }

    /// A caller that says it is dirty but gives nowhere to ask must not
    /// end up with a sheet that refuses to close. Losing a draft is bad;
    /// trapping someone in a dialog is worse.
    #[test]
    fn a_dirty_sheet_with_nowhere_to_ask_still_closes() {
        assert_eq!(dismissal(true, false), Dismissal::Close);
    }

    /// Modal is the default so that adding the prop changed no caller.
    #[test]
    fn the_default_variant_is_the_one_every_caller_already_had() {
        assert_eq!(SheetVariant::default(), SheetVariant::Modal);
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
    request_close: Callback<()>,
) {
    use wasm_bindgen::JsCast;

    if !open.get_untracked() {
        return;
    }
    let key = ev.key();
    if key == "Escape" {
        ev.prevent_default();
        request_close.run(());
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
