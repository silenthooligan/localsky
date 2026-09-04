// SettingsEngine. The dispatch-shaping engine knobs that are not skip
// thresholds: the scheduling-model selector (weekly | soil), the
// cycle-and-soak controls (soak length + cross-zone interleaving) and the
// seasonal water-budget dial. Split out of the Skip rules page (0.7.9) so
// that page holds only the skip-ladder thresholds.
// Reads + writes via /api/config; every knob here hot-reloads through the
// watering policy, so a save applies on the next evaluation, usually within a minute.

use leptos::prelude::*;

use crate::components::settings_ui::{SettingsLoadError, SettingsResult};
use crate::components::ui::{
    Button, ConfirmSheet, FormField, HelpHint, Panel, SegmentedControl, SkeletonRows, Toggle,
};

/// The model-flip confirmation body: a plain summary of what changes at
/// the next evaluation, per direction. The switch to Soil also names
/// that the soil-vs-weekly comparison line retires, because the owner
/// who used it to decide loses it on opt-in. Pure so the copy is
/// pinned.
fn model_flip_summary(to_soil: bool) -> &'static str {
    if to_soil {
        "Watering switches to the Soil model on the next evaluation, usually within a \
         minute. Each zone waters when its own soil deficit crosses its trigger and \
         refills it, in place of the fixed weekly split; zones pinned to a model in the \
         zone editor keep their pin. The soil-vs-weekly comparison line in each zone's \
         Tuning panel retires; the soil plan becomes the plan."
    } else {
        "Watering switches to the Weekly model on the next evaluation, usually within a \
         minute. Each zone waters toward its weekly target split across sessions; zones \
         pinned to a model in the zone editor keep their pin. The soil-vs-weekly \
         comparison line returns to each zone's Tuning panel."
    }
}

#[component]
pub fn SettingsEngine() -> impl IntoView {
    // Seeded from the ENGINE's own defaults rather than from numbers
    // retyped here, so this page cannot show a starting value the engine
    // does not use. The live config loads over these a moment later.
    let seed = crate::config::schema::EngineParams::default();
    let soak_minutes = RwSignal::new(seed.soak_minutes);
    let interleave_cycles = RwSignal::new(seed.interleave_cycles);
    let seasonal_adjust_pct = RwSignal::new(seed.seasonal_adjust_pct);
    // engine.scheduling_model: "weekly" (the shipped default) | "soil".
    let scheduling_model = RwSignal::new("weekly".to_string());
    // What the last load showed, for change detection: the field rides a
    // save ONLY when the operator changed it on this page. An unset
    // config (absent key = follow the shipped default) must stay unset
    // across saves of unrelated knobs, or every install would stamp an
    // explicit "weekly" indistinguishable from a deliberate choice (the
    // 0.7.9 GET-PUT round-trip lesson).
    let scheduling_model_loaded = RwSignal::new("weekly".to_string());

    let loaded = RwSignal::new(false);
    // Initial-load state: Some(err) when the config GET failed. The editor body
    // is replaced by a Retry banner in that case; `load_retry` bumps to re-run
    // the load effect.
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
                match fetch_engine().await {
                    Ok(d) => {
                        soak_minutes.set(d.soak_minutes);
                        interleave_cycles.set(d.interleave_cycles);
                        seasonal_adjust_pct.set(d.seasonal_adjust_pct);
                        scheduling_model.set(d.scheduling_model.clone());
                        scheduling_model_loaded.set(d.scheduling_model);
                        loaded.set(true);
                        load_error.set(None);
                    }
                    Err(e) => load_error.set(Some(e)),
                }
            });
        });
    }

    // The model-flip confirmation. Switching the scheduling model is the
    // largest behavior change this page can make, and it used to confirm
    // with the same generic toast as a soak-minutes tweak; the shared
    // ConfirmSheet now states plainly what changes at the next
    // evaluation before anything writes. An unchanged model saves
    // directly, exactly as before.
    let confirm_open = RwSignal::new(false);
    let do_save = Callback::new(move |()| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let payload = EngineDraft {
            soak_minutes: soak_minutes.get().clamp(5, 120),
            interleave_cycles: interleave_cycles.get(),
            seasonal_adjust_pct: seasonal_adjust_pct.get(),
            scheduling_model: scheduling_model.get(),
            // Only a change the operator made on this page writes the
            // key; saving soak minutes alone must not stamp the model.
            scheduling_model_changed: scheduling_model.get() != scheduling_model_loaded.get(),
        };
        #[cfg(feature = "hydrate")]
        {
            let saved_model = payload.scheduling_model.clone();
            wasm_bindgen_futures::spawn_local(async move {
                match patch_engine(payload).await {
                    Ok(()) => {
                        scheduling_model_loaded.set(saved_model);
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Applies on the next evaluation, usually within a minute.",
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
    });
    let on_save = move |_| {
        if saving.get() {
            return;
        }
        if scheduling_model.get() != scheduling_model_loaded.get() {
            confirm_open.set(true);
            return;
        }
        do_save.run(());
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Engine"<HelpHint topic="irrigation-engine"/></h1>
                <p class="settings-page__subtitle">
                    "How the engine sizes and shapes the runs it dispatches: the "
                    "scheduling model, cycle-and-soak pacing and the seasonal "
                    "water budget. Skip thresholds live on the "
                    <a href="/settings/skip-rules" style="color: var(--accent)">"Skip rules"</a>
                    " page."
                </p>
            </header>

            // A failed initial GET replaces the whole editor with a Retry banner
            // rather than a form pre-filled with compile-time defaults (a Save
            // from which would overwrite the live values).
            <Show
                when=move || load_error.get().is_none()
                fallback=move || view! { <SettingsLoadError error=load_error retry=load_retry/> }
            >

            <Panel title="Scheduling model".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.85rem">
                    "Which model sizes and schedules smart-morning runs. Weekly "
                    "splits each zone's weekly target into sessions spaced "
                    "across the week. Soil waters each zone when its own soil "
                    "deficit crosses the trigger and refills it, so cadence "
                    "follows soil texture and roots instead of a session count."
                </p>
                <FormField
                    label="Model".to_string()
                    helptext="Applies to every zone without a per-zone pin (the zone editor's Scheduling model field). Under Soil, a set weekly target acts as a delivery ceiling and Sessions per week has no effect; both keep their meaning for zones pinned to Weekly. Applies on the next evaluation, usually within a minute.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <SegmentedControl
                        value=scheduling_model
                        options=vec![
                            ("weekly".into(), "Weekly".into()),
                            ("soil".into(), "Soil".into()),
                        ]
                        aria_label="Scheduling model".to_string()
                    />
                    <p class="form-effect">
                        {move || {
                            if scheduling_model.get() == "soil" {
                                "Every zone waters by its own soil deficit unless it pins the \
                                 weekly model."
                            } else {
                                "Every zone waters toward its weekly target unless it pins the \
                                 soil model."
                            }
                        }}
                    </p>
                </FormField>
            </Panel>

            <Panel title="Cycle and soak".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.85rem">
                    "When a zone's sprinklers apply water faster than the soil "
                    "absorbs it, the engine splits the run into short cycles "
                    "with soak pauses between them so the water sinks in "
                    "instead of running off."
                </p>
                <FormField
                    label="Soak time (minutes)".to_string()
                    helptext="Minimum pause between a zone's cycles so the water can infiltrate. 5-120 minutes; default 30. Longer soaks suit clay and slopes; applies on the next evaluation, usually within a minute.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <div class="seasonal-dial">
                        <input
                            type="range"
                            class="slider-clay"
                            min="5"
                            max="120"
                            step="5"
                            prop:value=move || soak_minutes.get().to_string()
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                    soak_minutes.set(v.clamp(5, 120));
                                }
                            }
                        />
                        <span class="seasonal-dial__value">
                            {move || format!("{} min", soak_minutes.get())}
                        </span>
                    </div>
                </FormField>
                <Toggle
                    checked=interleave_cycles
                    label="Interleave cycles across zones".to_string()
                    helptext="Water other zones during a zone's soak pauses so the morning sequence finishes sooner. One valve still runs at a time, and every soak keeps at least its full length. Turn this off on a well or other low-recovery supply, where the idle soak gaps double as recovery time. Applies on the next evaluation, usually within a minute.".to_string()
                />
            </Panel>

            // P2-6: the seasonal trust dial (moved here from Skip rules; it is
            // a run-shaping control, not a skip threshold).
            <Panel title="Water budget".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.85rem">
                    "The trust dial. Scales every zone's run depth up or down without "
                    "touching the per-zone math, like the seasonal-adjust on a commercial "
                    "controller. 100% is the engine's computed amount; dial down in a wet, "
                    "cool stretch and up in a heat wave."
                </p>
                <FormField
                    label="Seasonal adjustment".to_string()
                    helptext="Percent of the engine-computed run depth, 50-150%. Applied before the per-zone safety cap, so tonight's planned minutes already reflect it.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <div class="seasonal-dial">
                        <input
                            type="range"
                            class="slider-clay"
                            min="50"
                            max="150"
                            step="5"
                            prop:value=move || seasonal_adjust_pct.get().to_string()
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                    seasonal_adjust_pct.set(v);
                                }
                            }
                        />
                        <span class="seasonal-dial__value">
                            {move || format!("{}%", seasonal_adjust_pct.get())}
                        </span>
                    </div>
                    <p class="seasonal-dial__effect">
                        {move || {
                            let p = seasonal_adjust_pct.get();
                            if p == 100 {
                                "Every run waters the engine's computed depth.".to_string()
                            } else if p < 100 {
                                format!("Every run waters {p}% of the computed depth (drier).")
                            } else {
                                format!("Every run waters {p}% of the computed depth (wetter).")
                            }
                        }}
                    </p>
                </FormField>
            </Panel>

            <div class="settings-actions">
                <Button
                    variant="primary"
                    // Gate Save on a successful load: the fields init to
                    // compile-time defaults, so saving before the GET resolves
                    // (or after it errored) would overwrite the live values.
                    disabled=Signal::derive(move || saving.get() || !loaded.get())
                    on_click=Callback::new(on_save)
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </Button>
            </div>

            <ConfirmSheet
                visible=confirm_open
                title="Switch the scheduling model?"
                body=Signal::derive(move || {
                    model_flip_summary(scheduling_model.get() == "soil").to_string()
                })
                confirm_label=Signal::derive(move || {
                    if scheduling_model.get() == "soil" {
                        "Switch to Soil".to_string()
                    } else {
                        "Switch to Weekly".to_string()
                    }
                })
                on_confirm=do_save
            />
            </Show>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>

            <Show when=move || !loaded.get() && load_error.get().is_none()>
                <SkeletonRows count=2/>
            </Show>
        </div>
    }
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
struct EngineDraft {
    /// Lives on `engine` (engine.soak_minutes).
    soak_minutes: u32,
    /// Lives on `engine`; default on, off for low-recovery supplies.
    interleave_cycles: bool,
    /// Lives on `engine`, beside the two above.
    seasonal_adjust_pct: u32,
    /// Lives on `engine`: "weekly" (default) | "soil". Written only when
    /// `scheduling_model_changed`; an absent key on disk means "follow
    /// the shipped default" and must survive unrelated saves.
    scheduling_model: String,
    /// The operator changed the model on this page this session.
    scheduling_model_changed: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The model flip confirms with a plain statement of what changes at
    /// the next evaluation, and the switch to Soil names that the
    /// soil-vs-weekly comparison line retires (the owner who used it to
    /// decide loses it on opt-in); the switch back names its return.
    #[test]
    fn the_model_flip_summary_states_the_change_and_the_comparison_line() {
        let to_soil = model_flip_summary(true);
        assert!(
            to_soil.starts_with("Watering switches to the Soil model"),
            "{to_soil}"
        );
        assert!(to_soil.contains("usually within a minute"), "{to_soil}");
        assert!(to_soil.contains("keep their pin"), "{to_soil}");
        assert!(
            to_soil.contains("comparison line in each zone's Tuning panel retires"),
            "{to_soil}"
        );
        let to_weekly = model_flip_summary(false);
        assert!(
            to_weekly.starts_with("Watering switches to the Weekly model"),
            "{to_weekly}"
        );
        assert!(to_weekly.contains("comparison line returns"), "{to_weekly}");
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_engine() -> Result<EngineDraft, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::load_error_message(
            resp.status(),
            &body,
        ));
    }
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let engine = val.get("engine");
    Ok(EngineDraft {
        soak_minutes: engine
            .and_then(|e| e.get("soak_minutes"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(30),
        // Absent = the shipped default (on).
        interleave_cycles: engine
            .and_then(|e| e.get("interleave_cycles"))
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        seasonal_adjust_pct: engine
            .and_then(|e| e.get("seasonal_adjust_pct"))
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .unwrap_or(100),
        // Absent = the shipped default (weekly).
        scheduling_model: engine
            .and_then(|e| e.get("scheduling_model"))
            .and_then(|v| v.as_str())
            .unwrap_or("weekly")
            .to_string(),
        // Freshly loaded: nothing changed yet.
        scheduling_model_changed: false,
    })
}

#[cfg(feature = "hydrate")]
async fn patch_engine(d: EngineDraft) -> Result<(), String> {
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
    // Merge ONLY the three keys this page edits into the fetched engine
    // object (never replace it wholesale): skip_rules, restrictions, and any
    // key this page does not know about must survive a save (same round-trip
    // discipline as patch_skip_rules).
    engine_obj.insert("soak_minutes".into(), serde_json::json!(d.soak_minutes));
    engine_obj.insert(
        "interleave_cycles".into(),
        serde_json::json!(d.interleave_cycles),
    );
    engine_obj.insert(
        "seasonal_adjust_pct".into(),
        serde_json::json!(d.seasonal_adjust_pct),
    );
    // The scheduling model writes ONLY when the operator changed it on
    // this page: an untouched install keeps its absent key (follow the
    // shipped default) instead of getting the default stamped in as an
    // explicit choice on every unrelated save. A previously explicit
    // value round-trips through the fetched config untouched.
    if d.scheduling_model_changed {
        engine_obj.insert(
            "scheduling_model".into(),
            serde_json::json!(d.scheduling_model),
        );
    }
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(crate::components::settings_ui::save_error_message(
            resp.status(),
            &body,
        ));
    }
    Ok(())
}
