// <FormField/> wrapper. Provides label + helptext + error slot around
// any form input. Used by the settings UI for consistent vertical
// rhythm + a11y-compliant error association: each field instance gets a
// stable error id, and a hydrate-only effect wires aria-describedby +
// aria-invalid onto the first input inside the slot whenever the error
// state flips. Post-hydration attribute changes are safe; SSR markup is
// untouched (error signals start None, so no SSR/hydrate id mismatch).

use std::sync::atomic::{AtomicU64, Ordering};

use leptos::prelude::*;

static FIELD_SEQ: AtomicU64 = AtomicU64::new(0);

#[component]
pub fn FormField(
    /// Label rendered above the field.
    #[prop(into)]
    label: String,
    /// Optional helptext rendered between label and field in --text-dim.
    #[prop(into, optional)]
    helptext: String,
    /// Optional error message; when Some, replaces helptext + adds a
    /// danger ring around the wrapped input.
    #[prop(into, optional)]
    error: Signal<Option<String>>,
    children: Children,
) -> impl IntoView {
    let label_owned = label.clone();
    let helptext_owned = helptext.clone();
    // Per-instance error id, scoped by a WASM-side counter. Only used
    // client-side (the error div renders only after hydration, and the
    // describedby wiring is a hydrate effect), so the SSR counter
    // diverging is irrelevant.
    let fid = FIELD_SEQ.fetch_add(1, Ordering::Relaxed);
    let label_id = format!("ui-ff-{fid}-label");
    let err_id = format!("ui-ff-{fid}-err");
    let err_id_for_div = err_id.clone();
    let root: NodeRef<leptos::html::Div> = NodeRef::new();

    #[cfg(feature = "hydrate")]
    {
        use wasm_bindgen::{closure::Closure, JsCast};
        // Read the NodeRef reactively: a field constructed inside Show can
        // run before its DOM node is attached. Current Zones effects are
        // verified to run in both standalone and settings-shell routes.
        Effect::new(move |_| {
            let invalid = error.get().is_some();
            let Some(div) = root.get() else { return };
            let field: &web_sys::Element = div.as_ref();
            super::field_labels::bind(field, &label_id, &err_id, invalid);
            let field = field.clone();
            let label_id = label_id.clone();
            let err_id = err_id.clone();
            let callback =
                Closure::<dyn FnMut(js_sys::Array, web_sys::MutationObserver)>::new(move |_, _| {
                    super::field_labels::bind(&field, &label_id, &err_id, invalid)
                });
            let Ok(observer) = web_sys::MutationObserver::new(callback.as_ref().unchecked_ref())
            else {
                return;
            };
            let options = web_sys::MutationObserverInit::new();
            options.set_child_list(true);
            options.set_subtree(true);
            let _ = observer.observe_with_options(div.as_ref(), &options);
            let resources = StoredValue::new_local(Some((observer, callback)));
            on_cleanup(move || {
                if let Some((observer, callback)) =
                    resources.try_update_value(Option::take).flatten()
                {
                    observer.disconnect();
                    drop(callback);
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = (label_id, err_id);

    view! {
        <div
            class="ui-form-field"
            class:ui-form-field--error=move || error.get().is_some()
            node_ref=root
        >
            <label class="ui-form-field__label">{label_owned.clone()}</label>
            {(!helptext.is_empty()).then(|| view! {
                <div class="ui-form-field__helptext">{helptext_owned.clone()}</div>
            })}
            <div class="ui-form-field__input">{children()}</div>
            {move || {
                let id = err_id_for_div.clone();
                error.get().map(|e| view! {
                    <div class="ui-form-field__error" id=id role="alert">{e}</div>
                })
            }}
        </div>
    }
}
