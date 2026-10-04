//! One temperature anatomy for current readings and forecast callouts.
use crate::components::units_fmt::{temp_value, use_unit_prefs};
use leptos::prelude::*;

#[component]
pub fn TemperatureValue(#[prop(into)] value: Signal<f64>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let number = move || {
        let v = value.get();
        if v.is_finite() {
            temp_value(v, prefs.get())
        } else {
            "—".into()
        }
    };
    view! {
        <span class="temperature-value">
            <span class="sr-only">{move || if value.get().is_finite() {
                format!("{} degrees {}", number(), if prefs.get().temp_c { "Celsius" } else { "Fahrenheit" })
            } else { "Temperature unavailable".into() }}</span>
            <span class="temperature-value__number" aria-hidden="true">{number}</span>
            <span class="temperature-value__degree" aria-hidden="true">"°"</span>
            <span class="temperature-value__scale" aria-hidden="true">{move || if prefs.get().temp_c { "C" } else { "F" }}</span>
        </span>
    }
}
