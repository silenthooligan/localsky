// LocationStep. Lat / lon / elevation / timezone. Map picker + Nominatim
// proxy hook up in a follow-up; for now the form uses raw inputs so the
// wizard can complete a basic install without Leaflet.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::FormField;

#[component]
pub fn LocationStep() -> impl IntoView {
    let lat = RwSignal::new(0.0f64);
    let lon = RwSignal::new(0.0f64);
    let elevation = RwSignal::new(0.0f64);
    let tz = RwSignal::new(String::new());

    let lat_err: Signal<Option<String>> = Signal::derive(move || {
        let v = lat.get();
        if !(-90.0..=90.0).contains(&v) {
            Some(format!("Latitude must be between -90 and 90 (got {v:.4})"))
        } else if v == 0.0 && lon.get() == 0.0 {
            Some("0,0 is the null island default; set your actual location".to_string())
        } else {
            None
        }
    });
    let lon_err: Signal<Option<String>> = Signal::derive(move || {
        let v = lon.get();
        if !(-180.0..=180.0).contains(&v) {
            Some(format!(
                "Longitude must be between -180 and 180 (got {v:.4})"
            ))
        } else {
            None
        }
    });

    let can_advance = move || lat_err.get().is_none() && lon_err.get().is_none();

    let next_href = move || {
        if can_advance() {
            next_step_href("location")
        } else {
            None
        }
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Where are you?"</h2>
            <p class="setup-step__body">
                "LocalSky uses latitude and longitude for the radar center, the "
                "Open-Meteo forecast, sunrise/sunset, and the FAO-56 ET0 "
                "calculation. Browser geolocation and an address lookup land "
                "in a follow-up release."
            </p>

            <FormField
                label="Latitude".to_string()
                helptext="Decimal degrees (positive north).".to_string()
                error=lat_err
            >
                <input
                    type="number"
                    step="0.0001"
                    class="ui-input"
                    prop:value=move || lat.get()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            lat.set(v);
                        }
                    }
                />
            </FormField>

            <FormField
                label="Longitude".to_string()
                helptext="Decimal degrees (positive east).".to_string()
                error=lon_err
            >
                <input
                    type="number"
                    step="0.0001"
                    class="ui-input"
                    prop:value=move || lon.get()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            lon.set(v);
                        }
                    }
                />
            </FormField>

            <FormField
                label="Elevation (m)".to_string()
                helptext="Optional. Used by FAO-56 net-radiation. Leave at 0 for sea level.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    step="1"
                    class="ui-input"
                    prop:value=move || elevation.get()
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                            elevation.set(v);
                        }
                    }
                />
            </FormField>

            <FormField
                label="Timezone".to_string()
                helptext="IANA name (e.g. America/New_York). Leave blank to derive from lat/lon at boot.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="America/New_York"
                    prop:value=move || tz.get()
                    on:input=move |ev| tz.set(event_target_value(&ev))
                />
            </FormField>

            <SetupFooter
                prev=prev_step_href("location")
                next=Signal::derive(next_href).get_untracked()
            />
        </div>
    }
}
