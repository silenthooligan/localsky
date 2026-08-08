// SettingsEngine. The dispatch-shaping engine knobs that are not skip
// thresholds: the cycle-and-soak controls (soak length + cross-zone
// interleaving) and the seasonal water-budget dial. Split out of the Skip
// rules page (0.7.9) so that page holds only the skip-ladder thresholds.
// Reads + writes via /api/config; every knob here hot-reloads through the
// watering policy, so a save applies on the next scheduler tick.

use leptos::prelude::*;

use crate::components::settings_ui::{SettingsLoadError, SettingsResult};
use crate::components::ui::{Button, FormField, HelpHint, Panel, SkeletonRows, Toggle};

#[component]
pub fn SettingsEngine() -> impl IntoView {
    // -- engine.* fields this page owns (mirrors src/config/schema.rs) --
    let soak_minutes = RwSignal::new(30u32);
    let interleave_cycles = RwSignal::new(true);
    let seasonal_adjust_pct = RwSignal::new(100u32);

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
        let payload = EngineDraft {
            soak_minutes: soak_minutes.get().clamp(5, 120),
            interleave_cycles: interleave_cycles.get(),
            seasonal_adjust_pct: seasonal_adjust_pct.get(),
        };
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match patch_engine(payload).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Applies on the next scheduler tick.",
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

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Engine"<HelpHint topic="irrigation-engine"/></h1>
                <p class="settings-page__subtitle">
                    "How the engine shapes the runs it dispatches: cycle-and-soak "
                    "pacing and the seasonal water budget. Skip thresholds live on "
                    "the "
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

            <Panel title="Cycle and soak".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.85rem">
                    "When a zone's sprinklers apply water faster than the soil "
                    "absorbs it, the engine splits the run into short cycles "
                    "with soak pauses between them so the water sinks in "
                    "instead of running off."
                </p>
                <FormField
                    label="Soak time (minutes)".to_string()
                    helptext="Minimum pause between a zone's cycles so the water can infiltrate. 5-120 minutes; default 30. Longer soaks suit clay and slopes; applies on the next scheduler tick.".to_string()
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
                    helptext="Water other zones during a zone's soak pauses so the morning sequence finishes sooner. One valve still runs at a time, and every soak keeps at least its full length. Turn this off on a well or other low-recovery supply, where the idle soak gaps double as recovery time. Applies on the next scheduler tick.".to_string()
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
}

#[cfg(feature = "hydrate")]
async fn fetch_engine() -> Result<EngineDraft, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
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
