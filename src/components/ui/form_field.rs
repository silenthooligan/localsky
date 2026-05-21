// <FormField/> wrapper. Provides label + helptext + error slot around
// any form input. Used by the settings UI for consistent vertical
// rhythm + a11y-compliant error association.

use leptos::prelude::*;

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
    view! {
        <div class="ui-form-field" class:ui-form-field--error=move || error.get().is_some()>
            <label class="ui-form-field__label">{label_owned.clone()}</label>
            {(!helptext.is_empty()).then(|| view! {
                <div class="ui-form-field__helptext">{helptext_owned.clone()}</div>
            })}
            <div class="ui-form-field__input">{children()}</div>
            {move || error.get().map(|e| view! {
                <div class="ui-form-field__error" role="alert">{e}</div>
            })}
        </div>
    }
}
