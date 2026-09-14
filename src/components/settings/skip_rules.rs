// SettingsSkipRules. Editor for the skip ladder's tunable
// thresholds (engine::SkipRuleParams). Reads + writes via /api/config;
// the engine picks up new values on the next tick. A snapshot of the
// previous config is recorded for one-PUT rollback before the write.
//
// This page is what answers the "can the user override the smart logic?"
// question yes - every threshold the ladder evaluates is editable here
// with the defaults inline as helptext.

use leptos::prelude::*;

use crate::components::settings_ui::{SettingsLoadError, SettingsResult};
use crate::components::ui::{Button, FormField, HelpHint, Panel, SkeletonRows, Slider};
use crate::components::units_fmt::use_unit_prefs;

#[component]
pub fn SettingsSkipRules() -> impl IntoView {
    // Every field below is seeded from the ENGINE's own defaults, not from
    // numbers retyped here. The form loads the live config over these a
    // moment later; this is only what shows before that lands, and what
    // Reset restores.
    let seed = crate::config::schema::SkipRuleParams::default();
    let prefs = use_unit_prefs();
    let already_wet_in = RwSignal::new(seed.already_wet_in);
    let rain_now_in_hr = RwSignal::new(seed.rain_now_in_hr);
    let rain_next_4h_skip_in = RwSignal::new(seed.rain_next_4h_skip_in);
    let rain_3day_factor = RwSignal::new(seed.rain_3day_factor);
    let heat_advisory_temp_f = RwSignal::new(seed.heat_advisory_temp_f);
    let heat_advisory_humidity_pct = RwSignal::new(seed.heat_advisory_humidity_pct);
    let heat_advisory_dry_days = RwSignal::new(seed.heat_advisory_dry_days);
    let wind_forecast_slack_mph = RwSignal::new(seed.wind_forecast_slack_mph);
    let max_wind_mph = RwSignal::new(seed.max_wind_mph);
    let min_temp_f = RwSignal::new(seed.min_temp_f);
    let rain_skip_in = RwSignal::new(seed.rain_skip_in);
    let frost_skip_soil_f = RwSignal::new(seed.frost_skip_soil_f);

    let loaded = RwSignal::new(false);
    // Initial-load state: Some(err) when the config GET failed. The editor body is
    // replaced by a Retry banner in that case; `load_retry` bumps to re-run the
    // load effect.
    let load_error: RwSignal<Option<String>> = RwSignal::new(None);
    let load_retry = RwSignal::new(0u32);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let _ = load_retry.get();
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_skip_rules().await {
                    Ok(d) => {
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
                        load_error.set(None);
                    }
                    Err(e) => load_error.set(Some(e)),
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
                            crate::voice::SAVED_LIVE,
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

    // Reset writes the ENGINE's defaults, taken from the schema's own
    // Default impl rather than retyped here. Three of the numbers this
    // used to set (wind 15 against the engine's 10, min temp 40 against
    // 38, soil frost 38 against 35) were values the engine never chooses,
    // so Reset silently moved a yard onto thresholds no default install
    // has ever run.
    let on_reset = move |_| {
        let d = crate::config::schema::SkipRuleParams::default();
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
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Skip rules"<HelpHint topic="skip-rules"/></h1>
                <p class="settings-page__subtitle">
                    {format!("Override the {}-rule skip ladder's thresholds. They are ", crate::gates_catalog::builtin_rule_catalog().len())}
                    "checked every pass to decide run / skip / extended "
                    "for the next run and the 6 days after it. Defaults shown match the "
                    "values used when nothing is overridden. See these rules "
                    "decide a real day, and layer your own on top, in "
                    <a href="/rules" class:u-accent=true>"Rule Lab"</a>
                    ". Cycle-and-soak pacing and the seasonal water budget "
                    "moved to the "
                    <a href="/settings/engine" class:u-accent=true>"Engine"</a>
                    " page."
                </p>
            </header>

            // A failed initial GET replaces the whole editor with a Retry banner
            // rather than a form pre-filled with compile-time defaults (a Save from
            // which would overwrite every live threshold).
            <Show
                when=move || load_error.get().is_none()
                fallback=move || view! { <SettingsLoadError error=load_error retry=load_retry/> }
            >

            <Panel title="Rain skips".to_string() help_topic="skip-breakdown">
                <div class="grid settings-field-grid">
                    {unit_field(FieldCopy::already_wet(), already_wet_in, prefs)}
                    {unit_field(FieldCopy::rain_now(), rain_now_in_hr, prefs)}
                    {unit_field(FieldCopy::rain_next_4h(), rain_next_4h_skip_in, prefs)}
                    <FormField
                        label="3-day rollup factor".to_string()
                        helptext=format!("Multiplier applied to your rain skip threshold for the probability-weighted 3-day total. Default: {}", seed.rain_3day_factor)
                        error=Signal::derive(|| None::<String>)
                    >
                        {numeric_input("rain_3day_factor", rain_3day_factor, 0.1)}
                    </FormField>
                    {unit_field(FieldCopy::rain_skip(), rain_skip_in, prefs)}
                </div>
            </Panel>
            <Panel title="Wind + temperature".to_string() help_topic="skip-breakdown">
                <div class="grid settings-field-grid">
                    {unit_field(FieldCopy::max_wind(), max_wind_mph, prefs)}
                    {unit_field(FieldCopy::wind_slack(), wind_forecast_slack_mph, prefs)}
                    {unit_field(FieldCopy::min_temp(), min_temp_f, prefs)}
                    {unit_field(FieldCopy::frost(), frost_skip_soil_f, prefs)}
                </div>
            </Panel>
            <Panel title="Heat advisory (run-extended trigger)".to_string() help_topic="skip-breakdown">
                <p class="settings-page__subtitle" style="margin: 0 0 0.6rem">
                    "When ALL three conditions are met, the verdict flips to "
                    "run-extended, and the Steadman heat "
                    "multiplier to the watering duration."
                </p>
                <div class="grid settings-field-grid">
                    {unit_field(FieldCopy::heat_temp(), heat_advisory_temp_f, prefs)}
                    <FormField
                        label="Heat advisory humidity (%)".to_string()
                        helptext=format!("Afternoon RH at or above this. Default: {:.0}", seed.heat_advisory_humidity_pct)
                        error=Signal::derive(|| None::<String>)
                    >
                        <Slider value=heat_advisory_humidity_pct min=0.0 max=100.0 step=5.0 suffix="%".to_string()/>
                    </FormField>
                    <FormField
                        label="Heat advisory dry days".to_string()
                        helptext=format!("Consecutive dry days required before heat advisory triggers. Default: {}", seed.heat_advisory_dry_days)
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
                <Button
                    variant="primary"
                    // Gate Save on a successful load: the form fields init to
                    // compile-time defaults, so saving before the GET resolves
                    // (or after it errored) would silently overwrite every live
                    // watering threshold with defaults.
                    disabled=Signal::derive(move || saving.get() || !loaded.get())
                    on_click=Callback::new(on_save)
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </Button>
                <Button variant="ghost" on_click=Callback::new(on_reset)>
                    "Reset to defaults"
                </Button>
            </div>
            </Show>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>

            <Show when=move || !loaded.get() && load_error.get().is_none()>
                <SkeletonRows count=4/>
            </Show>
        </div>
    }
}

/// One unit-bearing threshold: its label, its sentence, its dimension,
/// the engine's default for it and the range the form allows, all in the
/// engine's own (imperial) units. The view converts at its edge.
#[derive(Debug, Clone)]
pub struct FieldCopy {
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub dim: crate::components::units_fmt::Dimension,
    /// The engine's default, for "Default: N" in the helptext.
    pub default: f64,
    /// A slider's bounds and step, stored units; None for a free field.
    pub range: Option<(f64, f64, f64)>,
    /// A free field's step, stored units.
    pub step: f64,
}

impl FieldCopy {
    fn seed() -> crate::config::schema::SkipRuleParams {
        crate::config::schema::SkipRuleParams::default()
    }
    pub fn already_wet() -> Self {
        Self {
            key: "already_wet_in",
            label: "Already-wet threshold",
            help: "If today's rain is at or above this, skip: the soil is presumed saturated.",
            dim: crate::components::units_fmt::Dimension::Depth,
            default: Self::seed().already_wet_in,
            range: None,
            step: 0.01,
        }
    }
    pub fn rain_now() -> Self {
        Self {
            key: "rain_now_in_hr",
            label: "Rain-now intensity",
            help: "Skip if the live rain rate is above this when the next run is decided.",
            dim: crate::components::units_fmt::Dimension::Rate,
            default: Self::seed().rain_now_in_hr,
            range: None,
            step: 0.001,
        }
    }
    pub fn rain_next_4h() -> Self {
        Self {
            key: "rain_next_4h_skip_in",
            label: "Rain in next 4 h",
            help: "Forecast accumulation in the next 4 hours. At or above means skip.",
            dim: crate::components::units_fmt::Dimension::Depth,
            default: Self::seed().rain_next_4h_skip_in,
            range: None,
            step: 0.01,
        }
    }
    pub fn rain_skip() -> Self {
        Self { key: "rain_skip_in", label: "Rain skip threshold", help: "Your own 'how much rain is enough'. Used in the 3-day rollup and the morning check.", dim: crate::components::units_fmt::Dimension::Depth, default: Self::seed().rain_skip_in, range: Some((0.0, 10.0, 0.05)), step: 0.05 }
    }
    pub fn max_wind() -> Self {
        Self {
            key: "max_wind_mph",
            label: "Max wind",
            help: "Skip when sustained wind is at or above this.",
            dim: crate::components::units_fmt::Dimension::Wind,
            default: Self::seed().max_wind_mph,
            range: Some((0.0, 50.0, 1.0)),
            step: 1.0,
        }
    }
    pub fn wind_slack() -> Self {
        Self { key: "wind_forecast_slack_mph", label: "Wind forecast slack", help: "Added to max wind when judging forecast wind, so a brief gust does not cancel the run.", dim: crate::components::units_fmt::Dimension::Wind, default: Self::seed().wind_forecast_slack_mph, range: Some((0.0, 20.0, 1.0)), step: 1.0 }
    }
    pub fn min_temp() -> Self {
        Self {
            key: "min_temp_f",
            label: "Min temperature",
            help: "Skip when the air is below this at the run, or is forecast to be overnight.",
            dim: crate::components::units_fmt::Dimension::Temp,
            default: Self::seed().min_temp_f,
            range: Some((20.0, 70.0, 1.0)),
            step: 1.0,
        }
    }
    pub fn frost() -> Self {
        Self {
            key: "frost_skip_soil_f",
            label: "Soil frost threshold",
            help: "Skip when the yard-wide soil temperature is below this. Needs a soil sensor.",
            dim: crate::components::units_fmt::Dimension::Temp,
            default: Self::seed().frost_skip_soil_f,
            range: Some((28.0, 50.0, 1.0)),
            step: 1.0,
        }
    }
    pub fn heat_temp() -> Self {
        Self {
            key: "heat_advisory_temp_f",
            label: "Heat advisory temp",
            help: "Daily high at or above this.",
            dim: crate::components::units_fmt::Dimension::Temp,
            default: Self::seed().heat_advisory_temp_f,
            range: Some((85.0, 120.0, 1.0)),
            step: 1.0,
        }
    }
    /// Every unit-bearing field, for the test that checks each default.
    pub fn all() -> Vec<Self> {
        vec![
            Self::already_wet(),
            Self::rain_now(),
            Self::rain_next_4h(),
            Self::rain_skip(),
            Self::max_wind(),
            Self::wind_slack(),
            Self::min_temp(),
            Self::frost(),
            Self::heat_temp(),
        ]
    }
    /// The helptext as rendered: the sentence, then the engine's default
    /// in the viewer's units.
    pub fn helptext(&self, p: crate::components::units_fmt::UnitPrefs) -> String {
        let prec = self.dim.precision(p);
        format!(
            "{} Default: {:.*} {}",
            self.help,
            prec,
            self.dim.to_display(self.default, p),
            self.dim.unit(p)
        )
    }
    pub fn label(&self, p: crate::components::units_fmt::UnitPrefs) -> String {
        format!("{} ({})", self.label, self.dim.unit(p))
    }
}

/// A threshold field in the viewer's units. The signal holds the engine's
/// number (imperial); what is shown and typed is converted at this edge,
/// so a metric viewer sees and types millimeters, Celsius or km/h and
/// the engine reads the same inches, Fahrenheit and mph it always did.
fn unit_field(
    copy: FieldCopy,
    sig: RwSignal<f64>,
    prefs: Signal<crate::components::units_fmt::UnitPrefs>,
) -> impl IntoView {
    move || {
        let p = prefs.get();
        let copy = copy.clone();
        let dim = copy.dim;
        let prec = dim.precision(p);
        let shown = move || format!("{:.*}", prec, dim.to_display(sig.get(), p));
        let (lo, hi, step) = match copy.range {
            Some((lo, hi, step)) => (
                Some(dim.to_display(lo, p)),
                Some(dim.to_display(hi, p)),
                dim.to_display(step, p)
                    .max(if prec == 0 { 1.0 } else { 0.001 }),
            ),
            None => (None, None, dim.to_display(copy.step, p)),
        };
        let stored_range = copy.range.map(|(lo, hi, _)| (lo, hi));
        let input = view! {
            <input
                type=if copy.range.is_some() { "range" } else { "number" }
                class=if copy.range.is_some() { "slider-clay" } else { "ui-input" }
                step=format!("{step}")
                min=lo.map(|v| format!("{v:.*}", prec))
                max=hi.map(|v| format!("{v:.*}", prec))
                prop:value=shown
                on:input=move |ev| {
                    if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                        let stored = dim.to_stored(v, p);
                        sig.set(match stored_range {
                            Some((lo, hi)) => stored.clamp(lo, hi),
                            None => stored,
                        });
                    }
                }
            />
        };
        let readout = copy.range.map(|_| view! { <span class="seasonal-dial__value">{move || format!("{} {}", shown(), dim.unit(p))}</span> });
        view! {
            <FormField
                label=copy.label(p)
                helptext=copy.helptext(p)
                error=Signal::derive(|| None::<String>)
            >
                <div class="seasonal-dial">{input}{readout}</div>
            </FormField>
        }
    }
}

fn numeric_input(_id: &'static str, sig: RwSignal<f64>, step: f64) -> impl IntoView {
    bounded_numeric_input(_id, sig, step, None)
}

/// A numeric field carrying the server's own accepted range.
///
/// The three adopted thresholds are range-checked by `POST /action
/// set_threshold` and published with min/max/step on their `number` entity, so
/// Settings must not be able to save a value the dashboard slider and the Home
/// Assistant entity would both refuse. `rain_skip_in` is the one of the three
/// that is a free numeric field rather than a slider; max wind and min temp
/// are sliders already bounded to the same ranges.
fn bounded_numeric_input(
    _id: &'static str,
    sig: RwSignal<f64>,
    step: f64,
    range: Option<(f64, f64)>,
) -> impl IntoView {
    let lo = range.map(|r| r.0.to_string());
    let hi = range.map(|r| r.1.to_string());
    view! {
        <input
            type="number"
            step={format!("{step}")}
            min=lo
            max=hi
            class="ui-input"
            prop:value=move || sig.get()
            on:input=move |ev| {
                if let Ok(v) = event_target_value(&ev).parse::<f64>() {
                    sig.set(match range {
                        Some((lo, hi)) => v.clamp(lo, hi),
                        None => v,
                    });
                }
            }
        />
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
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
    let val = crate::components::config_client::get_config().await?;
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
    // A key missing from the config falls back to the ENGINE's default for
    // it, the number the engine uses with no override. The fallbacks used
    // to be retyped here and three of them were wrong (15 / 40 / 38
    // against the engine's 10 / 38 / 35), so a config missing a key
    // loaded a number the engine never used and saved it as a choice.
    let d = crate::config::schema::SkipRuleParams::default();
    Ok(SkipRulesDraft {
        already_wet_in: f64_at("already_wet_in", d.already_wet_in),
        rain_now_in_hr: f64_at("rain_now_in_hr", d.rain_now_in_hr),
        rain_next_4h_skip_in: f64_at("rain_next_4h_skip_in", d.rain_next_4h_skip_in),
        rain_3day_factor: f64_at("rain_3day_factor", d.rain_3day_factor),
        heat_advisory_temp_f: f64_at("heat_advisory_temp_f", d.heat_advisory_temp_f),
        heat_advisory_humidity_pct: f64_at(
            "heat_advisory_humidity_pct",
            d.heat_advisory_humidity_pct,
        ),
        heat_advisory_dry_days: u32_at("heat_advisory_dry_days", d.heat_advisory_dry_days),
        wind_forecast_slack_mph: f64_at("wind_forecast_slack_mph", d.wind_forecast_slack_mph),
        max_wind_mph: f64_at("max_wind_mph", d.max_wind_mph),
        min_temp_f: f64_at("min_temp_f", d.min_temp_f),
        rain_skip_in: f64_at("rain_skip_in", d.rain_skip_in),
        frost_skip_soil_f: f64_at("frost_skip_soil_f", d.frost_skip_soil_f),
    })
}

#[cfg(feature = "hydrate")]
async fn patch_skip_rules(d: SkipRulesDraft) -> Result<(), String> {
    // Read-merge-write: the merge below must start from the live document,
    // so a failed GET is a failed save, never a PUT built on garbage.
    let mut cfg = crate::components::config_client::get_config().await?;
    let engine = cfg
        .as_object_mut()
        .and_then(|c| c.get_mut("engine"))
        .ok_or_else(|| "config missing 'engine' table".to_string())?;
    let engine_obj = engine
        .as_object_mut()
        .ok_or_else(|| "engine is not a table".to_string())?;
    // Merge the edited keys INTO the fetched skip_rules object rather than
    // replacing it wholesale: fields this page does not edit (disabled_rules,
    // soil-quarantine tuning, rain_observed_window_days) must survive a save.
    if !engine_obj.contains_key("skip_rules") {
        engine_obj.insert("skip_rules".into(), serde_json::json!({}));
    }
    let sr = engine_obj
        .get_mut("skip_rules")
        .and_then(|v| v.as_object_mut())
        .ok_or_else(|| "engine.skip_rules is not a table".to_string())?;
    for (k, v) in [
        ("already_wet_in", serde_json::json!(d.already_wet_in)),
        ("rain_now_in_hr", serde_json::json!(d.rain_now_in_hr)),
        (
            "rain_next_4h_skip_in",
            serde_json::json!(d.rain_next_4h_skip_in),
        ),
        ("rain_3day_factor", serde_json::json!(d.rain_3day_factor)),
        (
            "heat_advisory_temp_f",
            serde_json::json!(d.heat_advisory_temp_f),
        ),
        (
            "heat_advisory_humidity_pct",
            serde_json::json!(d.heat_advisory_humidity_pct),
        ),
        (
            "heat_advisory_dry_days",
            serde_json::json!(d.heat_advisory_dry_days),
        ),
        (
            "wind_forecast_slack_mph",
            serde_json::json!(d.wind_forecast_slack_mph),
        ),
        ("max_wind_mph", serde_json::json!(d.max_wind_mph)),
        ("min_temp_f", serde_json::json!(d.min_temp_f)),
        ("rain_skip_in", serde_json::json!(d.rain_skip_in)),
        ("frost_skip_soil_f", serde_json::json!(d.frost_skip_soil_f)),
    ] {
        sr.insert(k.into(), v);
    }
    // The seasonal dial + cycle/soak knobs sit on `engine` beside
    // `skip_rules` and are owned by the Engine settings page now; this page
    // merges only skip_rules keys so a save here never touches them.
    // Skip-rule thresholds hot-reload (the engine reads them on its next
    // tick), so the PUT's restart reasons are not surfaced here.
    crate::components::config_client::put_config(&cfg)
        .await
        .map(|_| ())
}

#[cfg(test)]
mod default_copy_tests {
    use super::*;
    use crate::components::units_fmt::{Dimension, UnitPrefs, METRIC};

    /// Every helptext ends with the engine's own default for its field:
    /// the number after "Default:" equals SkipRuleParams::default(), in
    /// the viewer's units. The copy used to say 15 / 40 / 38 where the
    /// engine used 10 / 38 / 35.
    #[test]
    fn every_default_in_the_copy_is_the_engines() {
        let seed = serde_json::to_value(crate::config::schema::SkipRuleParams::default()).unwrap();
        for f in FieldCopy::all() {
            let engine = seed[f.key]
                .as_f64()
                .unwrap_or_else(|| panic!("{} is numeric", f.key));
            let help = f.helptext(UnitPrefs::default());
            let after = help.split("Default: ").nth(1).expect("a Default: clause");
            let number: f64 = after.split_whitespace().next().unwrap().parse().unwrap();
            assert!(
                (number - engine).abs() < 0.005,
                "{}: copy says {number}, engine {engine}",
                f.key
            );
            assert!(help.ends_with(f.dim.unit(UnitPrefs::default())), "{help}");
        }
    }

    /// A metric preference renders millimeters, Celsius and km/h on every
    /// label, and the number beside "Default:" is converted with it.
    #[test]
    fn a_metric_preference_renders_metric_on_every_label() {
        for f in FieldCopy::all() {
            let label = f.label(METRIC);
            let unit = f.dim.unit(METRIC);
            assert!(label.ends_with(&format!("({unit})")), "{label}");
            assert!(matches!(unit, "mm" | "mm/hr" | "°C" | "km/h"), "{unit}");
        }
        let wet = FieldCopy::already_wet();
        assert_eq!(
            wet.helptext(METRIC).split("Default: ").nth(1).unwrap(),
            "1.3 mm"
        );
        // And the edge converts both ways.
        let p = METRIC;
        assert!(
            (Dimension::Depth.to_stored(Dimension::Depth.to_display(0.25, p), p) - 0.25).abs()
                < 1e-9
        );
        assert!(
            (Dimension::Temp.to_stored(Dimension::Temp.to_display(38.0, p), p) - 38.0).abs() < 1e-9
        );
        assert!(
            (Dimension::Wind.to_stored(Dimension::Wind.to_display(10.0, p), p) - 10.0).abs() < 1e-9
        );
    }
}
