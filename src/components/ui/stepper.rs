// <Stepper/> — compact +/- integer-ish spinner bound to an f64 signal.
// Used for zone durations, budget days, etc. where a slider is overkill
// and a bare number input is fiddly on touch.

use leptos::prelude::*;

use crate::components::ui::Icon;

#[component]
pub fn Stepper(
    value: RwSignal<f64>,
    #[prop(default = 0.0)] min: f64,
    #[prop(default = f64::INFINITY)] max: f64,
    #[prop(default = 1.0)] step: f64,
    /// Digits past the decimal in the readout. Default 0.
    #[prop(default = 0)]
    precision: usize,
    #[prop(into, optional)] suffix: String,
    #[prop(into, optional)] aria_label: String,
) -> impl IntoView {
    let dec = move |_| value.update(|v| *v = (*v - step).max(min));
    let inc = move |_| value.update(|v| *v = (*v + step).min(max));
    let suffix_owned = suffix.clone();
    let has_suffix = !suffix.is_empty();
    view! {
        <div class="ui-stepper" role="group" aria-label=aria_label>
            <button type="button" class="ui-stepper__btn" aria-label="Decrease" on:click=dec>
                <Icon name="minus" size=16/>
            </button>
            <span class="ui-stepper__value">
                {move || format!("{:.*}", precision, value.get())}
                {has_suffix.then(|| view! { <span class="ui-stepper__suffix">{suffix_owned.clone()}</span> })}
            </span>
            <button type="button" class="ui-stepper__btn" aria-label="Increase" on:click=inc>
                <Icon name="plus" size=16/>
            </button>
        </div>
    }
}
