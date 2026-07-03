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
    let err_id = format!("ui-ff-{fid}-err");
    let err_id_for_div = err_id.clone();
    let input_id = format!("ui-ff-{fid}-input");
    let root: NodeRef<leptos::html::Div> = NodeRef::new();

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let has_error = error.get().is_some();
        let Some(div) = root.get() else {
            return;
        };
        let el: &web_sys::Element = div.as_ref();
        let Ok(Some(input)) = el.query_selector("input, select, textarea") else {
            return;
        };
        if has_error {
            let _ = input.set_attribute("aria-describedby", &err_id);
            let _ = input.set_attribute("aria-invalid", "true");
        } else {
            let _ = input.remove_attribute("aria-describedby");
            let _ = input.remove_attribute("aria-invalid");
        }
    });
    // Associate the label with the wrapped control so the field has an
    // accessible NAME (screen readers announce it; clicking the label focuses
    // the control). Hydrate-only + idempotent to match the error wiring above
    // (SSR markup untouched, no SSR/hydrate id mismatch). Skips a control that
    // already carries its own name (id / non-empty aria-label / aria-labelledby)
    // so a caller-provided one always wins.
    #[cfg(feature = "hydrate")]
    {
        let input_id = input_id.clone();
        Effect::new(move |_| {
            let Some(div) = root.get() else {
                return;
            };
            let el: &web_sys::Element = div.as_ref();
            let Ok(Some(input)) = el.query_selector("input, select, textarea") else {
                return;
            };
            let has_name = input.get_attribute("id").is_some()
                || input
                    .get_attribute("aria-label")
                    .is_some_and(|s| !s.is_empty())
                || input.get_attribute("aria-labelledby").is_some();
            if has_name {
                return;
            }
            let _ = input.set_attribute("id", &input_id);
            if let Ok(Some(label)) = el.query_selector("label.ui-form-field__label") {
                let _ = label.set_attribute("for", &input_id);
            }
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = (&err_id, &input_id);

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
