// <Slider/> range input paired with an editable number field. Used in
// settings for numeric thresholds (max_wind_mph, min_temp_f, etc). Drag
// the slider for a quick set, or type an exact value in the number box;
// both drive the same signal, and the number entry is clamped to
// [min, max] so the two controls stay in agreement. Native styling
// lives in main.scss.

use leptos::prelude::*;

#[component]
pub fn Slider(
    value: RwSignal<f64>,
    /// Inclusive min.
    min: f64,
    /// Inclusive max.
    max: f64,
    /// Step size. Default 1.
    #[prop(default = 1.0)]
    step: f64,
    /// Optional suffix rendered after the value (e.g. " mph", " °F", "%").
    /// Encode your own leading space — it is rendered verbatim next to
    /// the number box.
    #[prop(into, optional)]
    suffix: String,
    /// Number of digits past the decimal point in the number box.
    #[prop(default = 0)]
    precision: usize,
    /// Optional aria-label override (when no <label> wraps the slider).
    #[prop(into, optional)]
    aria_label: String,
) -> impl IntoView {
    let suffix_owned = suffix.clone();
    let aria_range = aria_label.clone();
    let aria_num = aria_label.clone();
    let has_suffix = !suffix.is_empty();
    view! {
        <div class="ui-slider">
            <input
                type="range"
                class="ui-slider__input"
                min=move || min.to_string()
                max=move || max.to_string()
                step=move || step.to_string()
                prop:value=move || value.get()
                aria-label=move || if aria_range.is_empty() { String::new() } else { aria_range.clone() }
                on:input=move |ev| {
                    if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                        value.set(v.clamp(min, max));
                    }
                }
            />
            <div class="ui-slider__entry">
                <input
                    type="number"
                    class="ui-slider__num"
                    min=move || min.to_string()
                    max=move || max.to_string()
                    step=move || step.to_string()
                    prop:value=move || format!("{:.*}", precision, value.get())
                    aria-label=move || if aria_num.is_empty() { String::new() } else { aria_num.clone() }
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            value.set(v.clamp(min, max));
                        }
                    }
                />
                {has_suffix.then(|| view! {
                    <span class="ui-slider__suffix">{suffix_owned}</span>
                })}
            </div>
        </div>
    }
}
