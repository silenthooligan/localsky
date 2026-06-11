// SettingsUnits. Imperial vs metric vs custom-per-field. Persists to
// localStorage so units are a per-device preference, not a per-
// deployment one. Read back by components::units_fmt::use_unit_prefs,
// which feeds the primary temperature and precipitation displays
// (weather hero, stat tiles, forecast hourly/daily). The wind and area
// keys are still persisted by the system presets but have no display
// consumers yet, so their per-field selectors are not rendered (no
// dead controls); restore the FormFields below when they get wired.

use leptos::prelude::*;

use crate::components::ui::{FormField, Panel, SegmentedControl};

#[component]
pub fn SettingsUnits() -> impl IntoView {
    let system = RwSignal::new("imperial".to_string());
    let temp_unit = RwSignal::new("f".to_string());
    let rain_unit = RwSignal::new("in".to_string());
    let wind_unit = RwSignal::new("mph".to_string());
    let area_unit = RwSignal::new("sqft".to_string());

    // Load saved values from localStorage on mount.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    if let Ok(Some(v)) = storage.get_item("units_system") {
                        system.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_temp") {
                        temp_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_rain") {
                        rain_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_wind") {
                        wind_unit.set(v);
                    }
                    if let Ok(Some(v)) = storage.get_item("units_area") {
                        area_unit.set(v);
                    }
                }
            }
        });
    }

    // Persist on change. Each Effect listens to a single signal so
    // reactive tracking is cheap.
    #[cfg(feature = "hydrate")]
    {
        let persist = |key: &'static str, sig: RwSignal<String>| {
            Effect::new(move |_| {
                let val = sig.get();
                if let Some(win) = web_sys::window() {
                    if let Ok(Some(storage)) = win.local_storage() {
                        let _ = storage.set_item(key, &val);
                    }
                }
            });
        };
        persist("units_system", system);
        persist("units_temp", temp_unit);
        persist("units_rain", rain_unit);
        persist("units_wind", wind_unit);
        persist("units_area", area_unit);
    }

    // When system switches, snap all per-field selectors to match.
    Effect::new(move |_| {
        let s = system.get();
        if s == "imperial" {
            temp_unit.set("f".into());
            rain_unit.set("in".into());
            wind_unit.set("mph".into());
            area_unit.set("sqft".into());
        } else if s == "metric" {
            temp_unit.set("c".into());
            rain_unit.set("mm".into());
            wind_unit.set("kph".into());
            area_unit.set("sqm".into());
        }
    });

    let show_custom = move || system.get() == "custom";

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Units"</h1>
                <p class="settings-page__subtitle">
                    "Per-device. Applies to display only; engine math runs in "
                    "metric internally and converts at the boundary. Today this "
                    "drives the temperature and rainfall readouts (hero, stat "
                    "tiles, forecast); wind and area conversions are coming."
                </p>
            </header>

            <Panel title="System".to_string()>
                <SegmentedControl
                    value=system
                    options=vec![
                        ("imperial".into(), "Imperial".into()),
                        ("metric".into(), "Metric".into()),
                        ("custom".into(), "Custom".into()),
                    ]
                    aria_label="Unit system".to_string()
                />
            </Panel>

            <Show when=show_custom>
                <Panel title="Per-field overrides".to_string()>
                    <FormField
                        label="Temperature".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=temp_unit
                            options=vec![
                                ("f".into(), "°F".into()),
                                ("c".into(), "°C".into()),
                            ]
                            aria_label="Temperature unit".to_string()
                        />
                    </FormField>

                    <FormField
                        label="Rainfall".to_string()
                        helptext="".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <SegmentedControl
                            value=rain_unit
                            options=vec![
                                ("in".into(), "inches".into()),
                                ("mm".into(), "mm".into()),
                            ]
                            aria_label="Rainfall unit".to_string()
                        />
                    </FormField>

                    // Wind speed and zone area selectors intentionally not
                    // rendered: nothing reads units_wind / units_area yet,
                    // and a control that does nothing reads as a bug. The
                    // system presets still persist both keys so existing
                    // choices survive until the displays are wired.
                </Panel>
            </Show>
        </main>
    }
}
