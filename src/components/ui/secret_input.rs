// <SecretInput/>, the one masked-input-with-reveal primitive. Renders a
// `<input>` that starts masked (type=password) plus a real `<button>`
// eye/eye-off toggle that flips the input between password and text. The
// toggle is keyboard operable (a native button), carries an aria-label
// that tracks state ("Show secret" / "Hide secret"), and exposes
// aria-pressed so assistive tech announces the reveal state. The input
// keeps whatever id/autocomplete wiring the caller passes so FormField's
// aria-describedby effect and the browser's autofill still target it.
//
// Binding is callback-shaped (not a hard RwSignal) so every existing
// caller fits: a settings field passes its RwSignal read + a setter
// closure; the source-config form passes the serde-backed read + a
// cfg.update closure. Default state is masked.

use leptos::ev;
use leptos::prelude::*;

use crate::components::ui::Icon;

#[component]
pub fn SecretInput(
    /// Current value (read side). Drives `prop:value`.
    #[prop(into)]
    value: Signal<String>,
    /// Fired on every keystroke with the new value.
    #[prop(into, optional)]
    on_input: Option<Callback<String>>,
    /// Fired on the native change event (blur/commit) with the value.
    #[prop(into, optional)]
    on_change: Option<Callback<String>>,
    /// Placeholder shown while empty.
    #[prop(into, optional)]
    placeholder: String,
    /// Input id (used by `<label for>` / aria-describedby wiring).
    #[prop(into, optional)]
    id: String,
    /// autocomplete hint (e.g. "current-password", "new-password", "off").
    #[prop(into, default = "off".to_string())]
    autocomplete: String,
    /// Disabled state. Defaults false.
    #[prop(into, optional)]
    disabled: Signal<bool>,
) -> impl IntoView {
    let revealed = RwSignal::new(false);

    let input_id = if id.is_empty() { None } else { Some(id) };
    let placeholder = if placeholder.is_empty() {
        None
    } else {
        Some(placeholder)
    };

    let on_input_cb = on_input;
    let on_change_cb = on_change;

    let input_handler = move |ev: ev::Event| {
        if let Some(cb) = on_input_cb {
            cb.run(event_target_value(&ev));
        }
    };
    let change_handler = move |ev: ev::Event| {
        if let Some(cb) = on_change_cb {
            cb.run(event_target_value(&ev));
        }
    };

    view! {
        <div class="ui-secret-input">
            <input
                id=input_id
                type=move || if revealed.get() { "text" } else { "password" }
                class="ui-input ui-secret-input__field"
                autocomplete=autocomplete
                placeholder=placeholder
                prop:disabled=move || disabled.get()
                prop:value=move || value.get()
                on:input=input_handler
                on:change=change_handler
            />
            <button
                type="button"
                class="ui-secret-input__toggle"
                aria-label=move || if revealed.get() { "Hide secret" } else { "Show secret" }
                aria-pressed=move || if revealed.get() { "true" } else { "false" }
                prop:disabled=move || disabled.get()
                tabindex="0"
                on:click=move |_| {
                    if !disabled.get() {
                        revealed.update(|r| *r = !*r);
                    }
                }
            >
                {move || {
                    let glyph = if revealed.get() { "eye-off" } else { "eye" };
                    view! { <Icon name=glyph size=18/> }
                }}
            </button>
        </div>
    }
}
