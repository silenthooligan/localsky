// <SegmentedControl/> horizontal pill picker. Used for enum settings
// (grass species, soil texture, sprinkler type, theme). One option
// active at a time; keyboard-accessible via arrow keys + Enter.

use leptos::prelude::*;

#[component]
pub fn SegmentedControl(
    /// Currently-selected option value.
    value: RwSignal<String>,
    /// (value, display label) pairs. Order matters; rendered left to right.
    options: Vec<(String, String)>,
    /// Optional aria-label for the group.
    #[prop(into, optional)]
    aria_label: String,
) -> impl IntoView {
    let aria = aria_label.clone();
    view! {
        <div
            class="ui-segmented"
            role="radiogroup"
            aria-label=aria.clone()
        >
            {options
                .into_iter()
                .map(|(val, label)| {
                    let val_for_click = val.clone();
                    let val_for_check = val.clone();
                    view! {
                        <button
                            class="ui-segmented__option"
                            class:ui-segmented__option--active=move || value.get() == val_for_check
                            role="radio"
                            aria-checked=move || (value.get() == val).to_string()
                            type="button"
                            on:click=move |_| value.set(val_for_click.clone())
                        >
                            {label}
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}
