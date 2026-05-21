// <Slider/> range input with a value chip. Used in settings for
// numeric thresholds (max_wind_mph, min_temp_f, soil_target, etc).
// Native <input type="range"> styling lives in main.scss.

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
    /// Optional suffix rendered after the value (e.g. "mph", "°F", "%").
    #[prop(into, optional)]
    suffix: String,
    /// Number of digits past the decimal point in the displayed value.
    #[prop(default = 0)]
    precision: usize,
    /// Optional aria-label override (when no <label> wraps the slider).
    #[prop(into, optional)]
    aria_label: String,
) -> impl IntoView {
    let suffix_owned = suffix.clone();
    let aria_owned = aria_label.clone();
    view! {
        <div class="ui-slider">
            <input
                type="range"
                class="ui-slider__input"
                min=move || min.to_string()
                max=move || max.to_string()
                step=move || step.to_string()
                prop:value=move || value.get()
                aria-label=move || if aria_owned.is_empty() { String::new() } else { aria_owned.clone() }
                on:input=move |ev| {
                    if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                        value.set(v);
                    }
                }
            />
            <output class="ui-slider__value">
                {move || format!("{:.*}{}", precision, value.get(), suffix_owned)}
            </output>
        </div>
    }
}
