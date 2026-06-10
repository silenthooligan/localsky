// SettingsSkipRules. Editor for the 17-rule skip ladder's tunable
// thresholds (engine::SkipRuleParams). Reads + writes via /api/config;
// the engine picks up new values on the next tick. A snapshot of the
// previous config is recorded for one-PUT rollback before the write.
//
// This page is what answers the "can the user override the smart logic?"
// question yes - every threshold the ladder evaluates is editable here
// with the defaults inline as helptext.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{FormField, HelpHint, Panel, Slider};

#[component]
pub fn SettingsSkipRules() -> impl IntoView {
    // -- Engine SkipRuleParams fields (mirrors src/config/schema.rs) --
    let already_wet_in = RwSignal::new(0.05f64);
    let rain_now_in_hr = RwSignal::new(0.01f64);
    let rain_next_4h_skip_in = RwSignal::new(0.10f64);
    let rain_3day_factor = RwSignal::new(1.5f64);
    let heat_advisory_temp_f = RwSignal::new(95.0f64);
    let heat_advisory_humidity_pct = RwSignal::new(60.0f64);
    let heat_advisory_dry_days = RwSignal::new(2u32);
    let wind_forecast_slack_mph = RwSignal::new(5.0f64);
    let max_wind_mph = RwSignal::new(15.0f64);
    let min_temp_f = RwSignal::new(40.0f64);
    let rain_skip_in = RwSignal::new(0.25f64);
    let frost_skip_soil_f = RwSignal::new(38.0f64);

    let loaded = RwSignal::new(false);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(d) = fetch_skip_rules().await {
                    already_wet_in.set(d.already_wet_in);
                    rain_now_in_hr.set(d.rain_now_in_hr);
                    rain_next_4h_skip_in.set(d.rain_next_4h_skip_in);
                    rain_3day_factor.set(d.rain_3day_factor);
                    heat_advisory_temp_f.set(d.heat_advisory_temp_f);
                    heat_advisory_humidity_pct.set(d.heat_advisory_humidity_pct);
                    heat_advisory_dry_days.set(d.heat_advisory_dry_days);
                    wind_forecast_slack_mph.set(d.wind_forecast_slack_mph);
                    max_wind_mph.set(d.max_wind_mph);
                    min_temp_f.set(d.min_temp_f);
                    rain_skip_in.set(d.rain_skip_in);
                    frost_skip_soil_f.set(d.frost_skip_soil_f);
                    loaded.set(true);
                }
            });
        });
    }

    let on_save = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let payload = SkipRulesDraft {
            already_wet_in: already_wet_in.get(),
            rain_now_in_hr: rain_now_in_hr.get(),
            rain_next_4h_skip_in: rain_next_4h_skip_in.get(),
            rain_3day_factor: rain_3day_factor.get(),
            heat_advisory_temp_f: heat_advisory_temp_f.get(),
            heat_advisory_humidity_pct: heat_advisory_humidity_pct.get(),
            heat_advisory_dry_days: heat_advisory_dry_days.get(),
            wind_forecast_slack_mph: wind_forecast_slack_mph.get(),
            max_wind_mph: max_wind_mph.get(),
            min_temp_f: min_temp_f.get(),
            rain_skip_in: rain_skip_in.get(),
            frost_skip_soil_f: frost_skip_soil_f.get(),
        };
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match patch_skip_rules(payload).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Engine picks up on next tick.",
                        );
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(e);
                    }
                }
                saving.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        {
            saving.set(false);
            let _ = payload;
        }
    };

    let on_reset = move |_| {
        already_wet_in.set(0.05);
        rain_now_in_hr.set(0.01);
        rain_next_4h_skip_in.set(0.10);
        rain_3day_factor.set(1.5);
        heat_advisory_temp_f.set(95.0);
        heat_advisory_humidity_pct.set(60.0);
        heat_advisory_dry_days.set(2);
        wind_forecast_slack_mph.set(5.0);
        max_wind_mph.set(15.0);
        min_temp_f.set(40.0);
        rain_skip_in.set(0.25);
        frost_skip_soil_f.set(38.0);
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Skip rules"</h1>
                <p class="settings-page__subtitle">
                    "Override the 17-rule skip ladder's thresholds. The engine "
                    "evaluates these every tick to decide run / skip / extended "
                    "for tonight and the next 6 days. Defaults shown match the "
                    "values the engine uses with no override. See these rules "
                    "decide a real day, and layer your own on top, in "
                    <a href="/rules" style="color: var(--accent)">"Rule Lab"</a>"."
                </p>
            </header>

            <Panel title="Rain skips".to_string()>
                <HelpHint topic="skip-breakdown"/>
                <div class="grid settings-field-grid">
                    <FormField
                        label="Already-wet threshold (in)".to_string()
                        helptext="If rain_today is at or above this, skip tonight - the soil is presumed saturated. Default: 0.05".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {numeric_input("already_wet_in", already_wet_in, 0.01)}
                    </FormField>
                    <FormField
                        label="Rain-now intensity (in/hr)".to_string()
                        helptext="Skip if the live rain rate exceeds this when the evening verdict is computed. Default: 0.01".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {numeric_input("rain_now_in_hr", rain_now_in_hr, 0.001)}
                    </FormField>
                    <FormField
                        label="Rain in next 4 h (in)".to_string()
                        helptext="Forecasted accumulation in the next 4 hours. At or above means skip. Default: 0.10".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {numeric_input("rain_next_4h_skip_in", rain_next_4h_skip_in, 0.01)}
                    </FormField>
                    <FormField
                        label="3-day rollup factor".to_string()
                        helptext="Multiplier applied to your rain_skip_in for the probability-weighted 3-day total. Default: 1.5".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {numeric_input("rain_3day_factor", rain_3day_factor, 0.1)}
                    </FormField>
                    <FormField
                        label="Rain skip threshold (in)".to_string()
                        helptext="Your personal 'how much rain is enough' threshold. Used in the 3-day rollup and the morning override. Default: 0.25".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {numeric_input("rain_skip_in", rain_skip_in, 0.05)}
                    </FormField>
                </div>
            </Panel>

            <Panel title="Wind + temperature".to_string()>
                <HelpHint topic="skip-breakdown"/>
                <div class="grid settings-field-grid">
                    <FormField
                        label="Max wind (mph)".to_string()
                        helptext="Skip when sustained wind is at or above this. Default: 15".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <Slider value=max_wind_mph min=0.0 max=40.0 step=1.0 suffix=" mph".to_string()/>
                    </FormField>
                    <FormField
                        label="Wind forecast slack (mph)".to_string()
                        helptext="Added to max_wind_mph when evaluating forecast wind so a brief gust doesn't shut down the night. Default: 5".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <Slider value=wind_forecast_slack_mph min=0.0 max=20.0 step=1.0 suffix=" mph".to_string()/>
                    </FormField>
                    <FormField
                        label="Min temperature (\u{00b0}F)".to_string()
                        helptext="Skip when air temp drops below this overnight. Default: 40".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <Slider value=min_temp_f min=20.0 max=70.0 step=1.0 suffix=" \u{00b0}F".to_string()/>
                    </FormField>
                    <FormField
                        label="Soil frost threshold (\u{00b0}F)".to_string()
                        helptext="Skip when yard-wide soil temp drops below this. Requires a soil sensor source. Default: 38".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <Slider value=frost_skip_soil_f min=28.0 max=50.0 step=1.0 suffix=" \u{00b0}F".to_string()/>
                    </FormField>
                </div>
            </Panel>

            <Panel title="Heat advisory (run-extended trigger)".to_string()>
                <HelpHint topic="skip-breakdown"/>
                <p class="settings-page__subtitle" style="margin: 0 0 0.6rem">
                    "When ALL three conditions are met, the verdict flips to "
                    "run-extended and the engine applies the Steadman heat "
                    "multiplier to the watering duration."
                </p>
                <div class="grid settings-field-grid">
                    <FormField
                        label="Heat advisory temp (\u{00b0}F)".to_string()
                        helptext="Daily high at or above this. Default: 95".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <Slider value=heat_advisory_temp_f min=85.0 max=120.0 step=1.0 suffix=" \u{00b0}F".to_string()/>
                    </FormField>
                    <FormField
                        label="Heat advisory humidity (%)".to_string()
                        helptext="Afternoon RH at or above this. Default: 60".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <Slider value=heat_advisory_humidity_pct min=0.0 max=100.0 step=5.0 suffix="%".to_string()/>
                    </FormField>
                    <FormField
                        label="Heat advisory dry days".to_string()
                        helptext="Consecutive dry days required before heat advisory triggers. Default: 2".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <input
                            type="number"
                            step="1"
                            min="0"
                            class="ui-input"
                            prop:value=move || heat_advisory_dry_days.get() as f64
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                    heat_advisory_dry_days.set(v);
                                }
                            }
                        />
                    </FormField>
                </div>
            </Panel>

            <div class="settings-actions">
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    disabled=move || saving.get()
                    on:click=on_save
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </button>
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--ghost"
                    on:click=on_reset
                >
                    "Reset to defaults"
                </button>
            </div>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>

            <Show when=move || !loaded.get()>
                <p class="settings-page__subtitle" style="margin-top: 1rem">
                    "Loading current values from /api/config..."
                </p>
            </Show>
        </main>
    }
}

fn numeric_input(_id: &'static str, sig: RwSignal<f64>, step: f64) -> impl IntoView {
    view! {
        <input
            type="number"
            step={format!("{step}")}
            class="ui-input"
            prop:value=move || sig.get()
            on:input=move |ev| {
                if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                    sig.set(v);
                }
            }
        />
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct SkipRulesDraft {
    already_wet_in: f64,
    rain_now_in_hr: f64,
    rain_next_4h_skip_in: f64,
    rain_3day_factor: f64,
    heat_advisory_temp_f: f64,
    heat_advisory_humidity_pct: f64,
    heat_advisory_dry_days: u32,
    wind_forecast_slack_mph: f64,
    max_wind_mph: f64,
    min_temp_f: f64,
    rain_skip_in: f64,
    frost_skip_soil_f: f64,
}

#[cfg(feature = "hydrate")]
async fn fetch_skip_rules() -> Result<SkipRulesDraft, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let sr = val.get("engine").and_then(|e| e.get("skip_rules"));
    let f64_at = |key: &str, default: f64| -> f64 {
        sr.and_then(|v| v.get(key))
            .and_then(|v| v.as_f64())
            .unwrap_or(default)
    };
    let u32_at = |key: &str, default: u32| -> u32 {
        sr.and_then(|v| v.get(key))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(default)
    };
    Ok(SkipRulesDraft {
        already_wet_in: f64_at("already_wet_in", 0.05),
        rain_now_in_hr: f64_at("rain_now_in_hr", 0.01),
        rain_next_4h_skip_in: f64_at("rain_next_4h_skip_in", 0.10),
        rain_3day_factor: f64_at("rain_3day_factor", 1.5),
        heat_advisory_temp_f: f64_at("heat_advisory_temp_f", 95.0),
        heat_advisory_humidity_pct: f64_at("heat_advisory_humidity_pct", 60.0),
        heat_advisory_dry_days: u32_at("heat_advisory_dry_days", 2),
        wind_forecast_slack_mph: f64_at("wind_forecast_slack_mph", 5.0),
        max_wind_mph: f64_at("max_wind_mph", 15.0),
        min_temp_f: f64_at("min_temp_f", 40.0),
        rain_skip_in: f64_at("rain_skip_in", 0.25),
        frost_skip_soil_f: f64_at("frost_skip_soil_f", 38.0),
    })
}

#[cfg(feature = "hydrate")]
async fn patch_skip_rules(d: SkipRulesDraft) -> Result<(), String> {
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;
    let engine = cfg
        .as_object_mut()
        .and_then(|c| c.get_mut("engine"))
        .ok_or_else(|| "config missing 'engine' table".to_string())?;
    let engine_obj = engine
        .as_object_mut()
        .ok_or_else(|| "engine is not a table".to_string())?;
    engine_obj.insert(
        "skip_rules".into(),
        serde_json::json!({
            "already_wet_in": d.already_wet_in,
            "rain_now_in_hr": d.rain_now_in_hr,
            "rain_next_4h_skip_in": d.rain_next_4h_skip_in,
            "rain_3day_factor": d.rain_3day_factor,
            "heat_advisory_temp_f": d.heat_advisory_temp_f,
            "heat_advisory_humidity_pct": d.heat_advisory_humidity_pct,
            "heat_advisory_dry_days": d.heat_advisory_dry_days,
            "wind_forecast_slack_mph": d.wind_forecast_slack_mph,
            "max_wind_mph": d.max_wind_mph,
            "min_temp_f": d.min_temp_f,
            "rain_skip_in": d.rain_skip_in,
            "frost_skip_soil_f": d.frost_skip_soil_f,
        }),
    );
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {body}", resp.status()));
    }
    Ok(())
}
