// <SegmentedControl/> horizontal pill picker. Used for enum settings
// (grass species, soil texture, sprinkler type, theme). One option
// active at a time. Keeps radiogroup semantics and implements the
// WAI-ARIA radio-group pattern for real: a single roving tab stop (the
// checked option, or the first option when nothing matches), Arrow
// Left/Up and Right/Down select and focus the previous/next option
// (wrapping), Home/End jump to the ends. Enter and Space activate
// natively because the options are real <button>s. Radio semantics are
// the right claim here: these are genuine single-select enums, and the
// single tab stop is what makes the 12-item species picker passable
// without twelve Tab presses.

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
    /// Fired when the USER moves the selection (click or arrow key), with
    /// the newly-selected value. Deliberately not an Effect on `value`: a
    /// caller that must react to a genuine change needs to distinguish it
    /// from the form seeding `value` while opening, and an Effect cannot.
    /// Never fires when the value is set programmatically.
    #[prop(optional)]
    on_change: Option<Callback<String>>,
) -> impl IntoView {
    let aria = aria_label.clone();
    let group: NodeRef<leptos::html::Div> = NodeRef::new();
    let count = options.len();
    // Value order snapshot: drives arrow-key targets + the roving stop.
    let values = StoredValue::new(options.iter().map(|(v, _)| v.clone()).collect::<Vec<_>>());

    // The single tab stop: the checked option, falling back to the first
    // option when the current value matches none, so the group never
    // drops out of the tab order entirely.
    let tab_stop = move || {
        let v = value.get();
        values.with_value(|vals| {
            if vals.contains(&v) {
                v
            } else {
                vals.first().cloned().unwrap_or_default()
            }
        })
    };

    view! {
        <div
            class="ui-segmented"
            role="radiogroup"
            aria-label=aria.clone()
            node_ref=group
        >
            {options
                .into_iter()
                .enumerate()
                .map(|(i, (val, label))| {
                    let val_for_click = val.clone();
                    let val_for_check = val.clone();
                    let val_for_tab = val.clone();
                    let on_key = move |ev: leptos::ev::KeyboardEvent| {
                        let target = match ev.key().as_str() {
                            "ArrowRight" | "ArrowDown" => Some((i + 1) % count),
                            "ArrowLeft" | "ArrowUp" => Some((i + count - 1) % count),
                            "Home" => Some(0),
                            "End" => Some(count.saturating_sub(1)),
                            _ => None,
                        };
                        let Some(j) = target else { return };
                        ev.prevent_default();
                        // Radios select on arrow movement (APG): update the
                        // value, then move focus to the newly-checked option.
                        if let Some(next) = values.try_with_value(|v| v.get(j).cloned()).flatten() {
                            let moved = value.get_untracked() != next;
                            value.set(next.clone());
                            if moved {
                                if let Some(cb) = on_change {
                                    cb.run(next);
                                }
                            }
                        }
                        #[cfg(feature = "hydrate")]
                        {
                            use wasm_bindgen::JsCast;
                            if let Some(group_el) = group.get_untracked() {
                                let el: &web_sys::Element = group_el.as_ref();
                                if let Ok(opts) = el.query_selector_all(".ui-segmented__option") {
                                    if let Some(btn) = opts
                                        .item(j as u32)
                                        .and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok())
                                    {
                                        let _ = btn.focus();
                                    }
                                }
                            }
                        }
                    };
                    view! {
                        <button
                            class="ui-segmented__option"
                            class:ui-segmented__option--active=move || value.get() == val_for_check
                            role="radio"
                            aria-checked=move || (value.get() == val).to_string()
                            tabindex=move || if tab_stop() == val_for_tab { "0" } else { "-1" }
                            type="button"
                            on:click=move |_| {
                                let next = val_for_click.clone();
                                let moved = value.get_untracked() != next;
                                value.set(next.clone());
                                if moved {
                                    if let Some(cb) = on_change {
                                        cb.run(next);
                                    }
                                }
                            }
                            on:keydown=on_key
                        >
                            {label}
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}
