// <Toggle/> iOS-style switch. Two states; clickable label region;
// keyboard-accessible via Space/Enter. Used in settings for boolean
// preferences.

use leptos::prelude::*;

#[component]
pub fn Toggle(
    checked: RwSignal<bool>,
    /// Visible label rendered to the left of the switch.
    #[prop(into)]
    label: String,
    /// Optional helptext rendered under the label in --text-dim.
    #[prop(into, optional)]
    helptext: String,
    /// Disabled state. Defaults false.
    #[prop(default = false)]
    disabled: bool,
) -> impl IntoView {
    let id = format!("toggle-{}", uuid_like());
    let label_owned = label;
    let help_owned = helptext.clone();
    view! {
        <div class="ui-toggle" class:ui-toggle--disabled=move || disabled>
            <label for=id.clone() class="ui-toggle__label-block">
                <div class="ui-toggle__label-text">
                    <span class="ui-toggle__label">{label_owned.clone()}</span>
                    {(!helptext.is_empty()).then(|| view! {
                        <span class="ui-toggle__helptext">{help_owned.clone()}</span>
                    })}
                </div>
            </label>
            <button
                id=id.clone()
                type="button"
                role="switch"
                aria-checked=move || if checked.get() { "true" } else { "false" }
                class="ui-toggle__switch"
                class:ui-toggle__switch--on=move || checked.get()
                disabled=disabled
                on:click=move |_| {
                    if !disabled {
                        checked.update(|v| *v = !*v);
                    }
                }
            >
                <span class="ui-toggle__thumb"></span>
            </button>
        </div>
    }
}

/// Pseudo-unique id for label htmlFor binding. Browser doesn't require
/// global uniqueness within a page for `for=` -> `id=` pairs we control,
/// but distinct ids help screen readers in repeated lists.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    format!("{n:x}")
}
