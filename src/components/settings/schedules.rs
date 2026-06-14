// SettingsSchedules. Operator surface for `Config.manual_schedules`.
//
// The scheduler tick lives in src/scheduler/manual.rs; this page just
// builds the list. Save round-trips through GET/PUT /api/config and the
// dispatcher picks up new schedules at boot (a future iteration can
// hot-reload mid-run via a watch channel).
//
// Mirrors the editing-state pattern from settings/zones.rs +
// settings/restrictions.rs: `editing_id: Option<String>` switches the
// form panel between Add and Edit, the Save button label flips, and on
// submit the matching entry in the Vec is replaced in-place.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::settings_ui::{
    BadgeTone, SettingsBadge, SettingsCard, SettingsKv, SettingsResult,
};
use crate::components::ui::{FormField, HelpHint, Panel, SegmentedControl, Toggle};

/// Replace em-dashes, en-dashes, and the Latin-1-decoded UTF-8 mojibake
/// of either with a plain hyphen so old toml entries written before the
/// `feedback_no_em_dashes` rule still render legibly.
fn sanitize_name(raw: &str) -> String {
    raw.replace(['\u{2014}', '\u{2013}'], "-")
        .replace("\u{00e2}\u{0080}\u{0094}", "-")
        .replace("\u{00e2}\u{0080}\u{0093}", "-")
}

#[component]
pub fn SettingsSchedules() -> impl IntoView {
    let config_json = RwSignal::new(serde_json::Value::Null);

    let add_open = RwSignal::new(false);
    let editing_id: RwSignal<Option<String>> = RwSignal::new(None);
    let new_id = RwSignal::new(String::new());
    let new_name = RwSignal::new(String::new());
    let new_zone = RwSignal::new(String::new());
    let new_enabled = RwSignal::new(true);
    let new_weekdays: RwSignal<Vec<u8>> = RwSignal::new(Vec::new());
    let new_start_hour = RwSignal::new(5u32);
    let new_start_minute = RwSignal::new(0u32);
    let new_duration = RwSignal::new(30u32);
    let new_mode = RwSignal::new("override".to_string());

    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    // Pre-select the first zone in cfg.zones for the form's
                    // zone picker; falls back to empty if no zones yet.
                    if let Some(slug) = cfg
                        .get("zones")
                        .and_then(|z| z.as_object())
                        .and_then(|m| m.keys().next().cloned())
                    {
                        new_zone.set(slug);
                    }
                    config_json.set(cfg);
                }
            });
        });
        Effect::new(move |_| {
            let open = add_open.get();
            let _ = editing_id.get();
            if !open {
                return;
            }
            if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
                if let Some(elt) = doc.get_element_by_id("schedule-form-panel") {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                    opts.set_block(web_sys::ScrollLogicalPosition::Start);
                    elt.scroll_into_view_with_scroll_into_view_options(&opts);
                }
            }
        });
    }

    let schedules_view = move || {
        let cfg = config_json.get();
        let arr = cfg
            .get("manual_schedules")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            return view! {
                <li class="settings-list__item">
                    <span class="settings-list__icon" aria-hidden="true"><crate::components::ui::Icon name="calendar" size=18/></span>
                    <span class="settings-list__text">
                        <span class="settings-list__label">"No manual schedules configured"</span>
                        <span class="settings-list__helptext">
                            "Smart irrigation runs as soon as the engine's deficit math triggers it. "
                            "Add a manual schedule below to fire a zone at a fixed weekday + time instead."
                        </span>
                    </span>
                </li>
            }
            .into_any();
        }
        let items = arr
            .into_iter()
            .filter_map(|s| {
                let id = s.get("id").and_then(|v| v.as_str())?.to_string();
                Some(view! {
                    <ScheduleCard
                        id=id
                        schedule=s
                        config_json=config_json
                        new_id=new_id
                        new_name=new_name
                        new_zone=new_zone
                        new_enabled=new_enabled
                        new_weekdays=new_weekdays
                        new_start_hour=new_start_hour
                        new_start_minute=new_start_minute
                        new_duration=new_duration
                        new_mode=new_mode
                        editing_id=editing_id
                        add_open=add_open
                    />
                })
            })
            .collect_view();
        view! { <>{items}</> }.into_any()
    };

    let on_save = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let cfg = config_json.get();
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match save_config(cfg).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Scheduler picks up new schedules on next container restart.",
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
            let _ = cfg;
        }
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"Back to Settings"</a>
                <h1 class="settings-page__title">"Manual schedules"<HelpHint topic="schedules"/></h1>
                <p class="settings-page__subtitle">
                    "Fire a zone at a fixed weekday + time, on top of (or instead of) "
                    "the smart-irrigation auto-mode. Watering restrictions apply to "
                    "manual schedules just like they do to smart runs: a blocked "
                    "dispatch logs a skip row with the rule's reason."
                </p>
                <p class="settings-page__subtitle" style="margin-top: 0.5rem">
                    <strong>"Override"</strong>
                    " (default) replaces the smart engine for the zone; smart math still "
                    "computes for visibility but doesn't dispatch. "
                    <strong>"Floor"</strong>
                    " fires the manual run AND lets smart add additional runs if the "
                    "deficit math demands more."
                </p>
            </header>

            <Panel title="Configured schedules".to_string()>
                <ul class="settings-card-list">{schedules_view}</ul>
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    style="margin-top: 1rem"
                    on:click=move |_| {
                        let now_open = add_open.get();
                        add_open.set(!now_open);
                        if now_open {
                            reset_schedule_draft(
                                editing_id,
                                new_id,
                                new_name,
                                new_weekdays,
                                new_duration,
                                new_start_hour,
                                new_start_minute,
                                new_mode,
                            );
                        }
                    }
                >
                    {move || {
                        if add_open.get() {
                            if editing_id.get().is_some() {
                                "× Cancel edit"
                            } else {
                                "× Cancel add"
                            }
                        } else {
                            "+ Add schedule"
                        }
                    }}
                </button>
            </Panel>

            <Show when=move || add_open.get()>
                <ScheduleForm
                    config_json=config_json
                    new_id=new_id
                    new_name=new_name
                    new_zone=new_zone
                    new_enabled=new_enabled
                    new_weekdays=new_weekdays
                    new_start_hour=new_start_hour
                    new_start_minute=new_start_minute
                    new_duration=new_duration
                    new_mode=new_mode
                    editing_id=editing_id
                    add_open=add_open
                    result_msg=result_msg
                    result_ok=result_ok
                />
            </Show>

            <button
                type="button"
                class="setup-apply-btn"
                style="margin-top: 1.5rem"
                disabled=move || saving.get()
                on:click=on_save
            >
                {move || if saving.get() { "Saving…" } else { "Save all changes" }}
            </button>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
        </main>
    }
}

/// Add/edit form for a single manual schedule, extracted out of the page
/// component so the page is a thin shell (header + list + save bar) and
/// this whole `<Panel>` view tree compiles inside its own
/// monomorphization boundary instead of nesting into the page. Owns the
/// "add to in-memory config" handler and the zone-picker options derived
/// from config_json; the page still owns the load (Effect) and the
/// persist (Save all changes -> PUT).
#[component]
fn ScheduleForm(
    config_json: RwSignal<serde_json::Value>,
    new_id: RwSignal<String>,
    new_name: RwSignal<String>,
    new_zone: RwSignal<String>,
    new_enabled: RwSignal<bool>,
    new_weekdays: RwSignal<Vec<u8>>,
    new_start_hour: RwSignal<u32>,
    new_start_minute: RwSignal<u32>,
    new_duration: RwSignal<u32>,
    new_mode: RwSignal<String>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    result_msg: RwSignal<String>,
    result_ok: RwSignal<bool>,
) -> impl IntoView {
    let zone_options = move || {
        let cfg = config_json.get();
        let zones = cfg.get("zones").and_then(|v| v.as_object()).cloned();
        match zones {
            Some(m) => m
                .keys()
                .map(|k| (k.clone(), k.replace('_', " ")))
                .collect::<Vec<_>>(),
            None => Vec::new(),
        }
    };

    let on_add = move |_| {
        let id = new_id.get().trim().to_lowercase().replace(' ', "_");
        if id.is_empty() {
            result_ok.set(false);
            result_msg.set("ID is required (snake_case)".into());
            return;
        }
        if new_zone.get().is_empty() {
            result_ok.set(false);
            result_msg
                .set("Pick a zone (configure one under /settings/zones first if needed)".into());
            return;
        }
        if new_weekdays.get().is_empty() {
            result_ok.set(false);
            result_msg.set("Pick at least one weekday".into());
            return;
        }
        if new_duration.get() == 0 {
            result_ok.set(false);
            result_msg.set("Duration must be at least 1 minute".into());
            return;
        }
        let name = if new_name.get().is_empty() {
            id.clone()
        } else {
            new_name.get()
        };
        let entry = serde_json::json!({
            "id": id,
            "name": name,
            "zone_slug": new_zone.get(),
            "enabled": new_enabled.get(),
            "weekdays": new_weekdays.get(),
            "start_hour": new_start_hour.get(),
            "start_minute": new_start_minute.get(),
            "duration_minutes": new_duration.get(),
            "mode": new_mode.get(),
        });

        let was_edit = editing_id.get().is_some();
        config_json.update(|cfg| {
            let arr = cfg.as_object_mut().and_then(|o| {
                o.entry("manual_schedules")
                    .or_insert(serde_json::json!([]))
                    .as_array_mut()
            });
            if let Some(arr) = arr {
                if was_edit {
                    let target = editing_id.get().unwrap_or_default();
                    if let Some(idx) = arr
                        .iter()
                        .position(|s| s.get("id").and_then(|v| v.as_str()) == Some(target.as_str()))
                    {
                        arr[idx] = entry;
                    } else {
                        arr.push(entry);
                    }
                } else {
                    arr.push(entry);
                }
            }
        });

        reset_schedule_draft(
            editing_id,
            new_id,
            new_name,
            new_weekdays,
            new_duration,
            new_start_hour,
            new_start_minute,
            new_mode,
        );
        add_open.set(false);
        result_ok.set(true);
        result_msg.set(if was_edit {
            "Updated schedule. Click Save below to persist.".to_string()
        } else {
            "Added schedule. Click Save below to persist.".to_string()
        });
    };

    let on_cancel = move |_| {
        reset_schedule_draft(
            editing_id,
            new_id,
            new_name,
            new_weekdays,
            new_duration,
            new_start_hour,
            new_start_minute,
            new_mode,
        );
        add_open.set(false);
    };

    view! {
        <div id="schedule-form-panel"><Panel title="Schedule form".to_string()>
            <Show when=move || editing_id.get().is_some()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "Editing "
                    <code>{move || editing_id.get().unwrap_or_default()}</code>
                    ". Save below applies to this id; the id field is read-only."
                </p>
            </Show>

            <FormField
                label="ID".to_string()
                helptext="snake_case identifier (e.g. back_yard_morning, drip_xeri_wed). Read-only while editing.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="back_yard_morning"
                    prop:value=move || new_id.get()
                    prop:disabled=move || editing_id.get().is_some()
                    on:input=move |ev| new_id.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Display name".to_string()
                helptext="Human label for the dashboard's runs log. Defaults to the id.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="Back Yard 5am"
                    prop:value=move || new_name.get()
                    on:input=move |ev| new_name.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Zone".to_string()
                helptext="Which configured zone this schedule fires. Configure zones under /settings/zones first if the picker is empty.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl
                    value=new_zone
                    options=zone_options()
                    aria_label="Zone".to_string()
                />
            </FormField>

            <FormField
                label="Enabled".to_string()
                helptext="Disable to keep the entry but skip evaluation.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <Toggle
                    checked=new_enabled
                    label="Fire this schedule".to_string()
                    helptext="".to_string()
                />
            </FormField>

            <FormField
                label="Weekdays".to_string()
                helptext="Days this schedule runs. Empty = never.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {weekday_checkboxes(new_weekdays)}
            </FormField>

            <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem">
                <FormField
                    label="Start hour (0-23, local time)".to_string()
                    helptext="24-hour. 5 = 05:00. Watering restrictions can still block this hour; the dispatch logs a skip if so.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        min="0"
                        max="23"
                        class="ui-input"
                        prop:value=move || new_start_hour.get() as f64
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                if v < 24 {
                                    new_start_hour.set(v);
                                }
                            }
                        }
                    />
                </FormField>

                <FormField
                    label="Start minute (0-59)".to_string()
                    helptext="Resolution is 1 minute (the dispatcher ticks every 60s).".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        min="0"
                        max="59"
                        class="ui-input"
                        prop:value=move || new_start_minute.get() as f64
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                if v < 60 {
                                    new_start_minute.set(v);
                                }
                            }
                        }
                    />
                </FormField>
            </div>

            <FormField
                label="Duration (minutes)".to_string()
                helptext="How long the zone runs per fire. Tightened if a Phase C restriction caps run length.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    min="1"
                    class="ui-input"
                    prop:value=move || new_duration.get() as f64
                    on:input=move |ev| {
                        if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                            new_duration.set(v);
                        }
                    }
                />
            </FormField>

            <FormField
                label="Mode".to_string()
                helptext="Override replaces smart-irrigation for this zone (smart still computes for nerd visibility). Floor fires alongside smart.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl
                    value=new_mode
                    options=vec![
                        ("override".into(), "Override (default)".into()),
                        ("floor".into(), "Floor".into()),
                    ]
                    aria_label="Schedule mode".to_string()
                />
            </FormField>

            <div class="settings-form-actions">
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--ghost"
                    on:click=on_cancel
                >
                    "Cancel"
                </button>
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    on:click=on_add
                >
                    {move || if editing_id.get().is_some() {
                        "Save schedule changes"
                    } else {
                        "Add to list"
                    }}
                </button>
            </div>
        </Panel></div>
    }
}

/// Reset the schedule draft signals back to a blank "new schedule"
/// state. Shared by the page's Cancel toggle and the form's post-add
/// cleanup so the two stay in sync. Leaves `new_zone` and `new_enabled`
/// untouched, matching the original inline reset behavior.
fn reset_schedule_draft(
    editing_id: RwSignal<Option<String>>,
    new_id: RwSignal<String>,
    new_name: RwSignal<String>,
    new_weekdays: RwSignal<Vec<u8>>,
    new_duration: RwSignal<u32>,
    new_start_hour: RwSignal<u32>,
    new_start_minute: RwSignal<u32>,
    new_mode: RwSignal<String>,
) {
    editing_id.set(None);
    new_id.set(String::new());
    new_name.set(String::new());
    new_weekdays.set(Vec::new());
    new_duration.set(30);
    new_start_hour.set(5);
    new_start_minute.set(0);
    new_mode.set("override".to_string());
}

fn weekday_short(d: u8) -> &'static str {
    match d {
        0 => "Sun",
        1 => "Mon",
        2 => "Tue",
        3 => "Wed",
        4 => "Thu",
        5 => "Fri",
        6 => "Sat",
        _ => "?",
    }
}

fn weekday_checkboxes(value: RwSignal<Vec<u8>>) -> impl IntoView {
    let labels: [(u8, &'static str); 7] = [
        (0, "Sun"),
        (1, "Mon"),
        (2, "Tue"),
        (3, "Wed"),
        (4, "Thu"),
        (5, "Fri"),
        (6, "Sat"),
    ];
    view! {
        <div style="display: flex; flex-wrap: wrap; gap: 0.4rem">
            {labels
                .iter()
                .map(|(idx, label)| {
                    let idx = *idx;
                    let label = *label;
                    let checked = move || value.get().contains(&idx);
                    let class = move || {
                        if checked() {
                            "weekday-chip is-on"
                        } else {
                            "weekday-chip"
                        }
                    };
                    view! {
                        <button
                            type="button"
                            class=class
                            aria-pressed=checked
                            on:click=move |_| {
                                value.update(|v| {
                                    if let Some(pos) = v.iter().position(|x| *x == idx) {
                                        v.remove(pos);
                                    } else {
                                        v.push(idx);
                                        v.sort_unstable();
                                    }
                                });
                            }
                        >
                            {label}
                        </button>
                    }
                })
                .collect_view()}
        </div>
    }
}

#[cfg(feature = "hydrate")]
async fn fetch_config() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|e| e.to_string())
}

#[cfg(feature = "hydrate")]
async fn save_config(cfg: serde_json::Value) -> Result<(), String> {
    use gloo_net::http::Request;
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

/// Single manual-schedule row. Own component so its view tree is
/// contained inside one monomorphization boundary.
#[component]
fn ScheduleCard(
    id: String,
    schedule: serde_json::Value,
    config_json: RwSignal<serde_json::Value>,
    new_id: RwSignal<String>,
    new_name: RwSignal<String>,
    new_zone: RwSignal<String>,
    new_enabled: RwSignal<bool>,
    new_weekdays: RwSignal<Vec<u8>>,
    new_start_hour: RwSignal<u32>,
    new_start_minute: RwSignal<u32>,
    new_duration: RwSignal<u32>,
    new_mode: RwSignal<String>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
) -> impl IntoView {
    let raw_name = schedule
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(&id)
        .to_string();
    let name = sanitize_name(&raw_name);
    let zone = schedule
        .get("zone_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let enabled = schedule
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let h = schedule
        .get("start_hour")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let m = schedule
        .get("start_minute")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let dur = schedule
        .get("duration_minutes")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let mode = schedule
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("override")
        .to_string();
    let weekdays = schedule
        .get("weekdays")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64())
                .map(|x| weekday_short(x as u8))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let subtitle =
        format!("{zone} \u{00b7} {weekdays} @ {h:02}:{m:02} \u{00b7} {dur} min \u{00b7} {mode}");
    let zone_kv = zone.clone();
    let weekdays_kv = if weekdays.is_empty() {
        "(none)".to_string()
    } else {
        weekdays.clone()
    };
    let time_kv = format!("{h:02}:{m:02}");
    let dur_kv = format!("{dur} min");
    let mode_kv = mode.clone();
    let id_kv = id.clone();
    let id_for_edit = id.clone();
    let id_for_delete = id.clone();
    let id_for_edit_label = id.clone();
    let id_for_delete_label = id.clone();
    let s_for_edit = schedule.clone();

    let on_edit = move |_| {
        let s = &s_for_edit;
        new_id.set(id_for_edit.clone());
        new_name.set(
            s.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id_for_edit)
                .to_string(),
        );
        new_zone.set(
            s.get("zone_slug")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        );
        new_enabled.set(s.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
        new_weekdays.set(
            s.get("weekdays")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_u64())
                        .map(|x| x as u8)
                        .collect()
                })
                .unwrap_or_default(),
        );
        new_start_hour.set(s.get("start_hour").and_then(|v| v.as_u64()).unwrap_or(5) as u32);
        new_start_minute.set(s.get("start_minute").and_then(|v| v.as_u64()).unwrap_or(0) as u32);
        new_duration.set(
            s.get("duration_minutes")
                .and_then(|v| v.as_u64())
                .unwrap_or(30) as u32,
        );
        new_mode.set(
            s.get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("override")
                .to_string(),
        );
        editing_id.set(Some(id_for_edit.clone()));
        add_open.set(true);
    };
    let on_delete = move |_| {
        let target = id_for_delete.clone();
        config_json.update(|cfg| {
            if let Some(arr) = cfg
                .get_mut("manual_schedules")
                .and_then(|v| v.as_array_mut())
            {
                arr.retain(|s| s.get("id").and_then(|v| v.as_str()) != Some(&target));
            }
        });
    };

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon="\u{23f0}".into()
                title=name
                subtitle=subtitle
                badges=Box::new(move || view! {
                    {if enabled {
                        view! { <SettingsBadge label="Enabled".into() tone=BadgeTone::Good/> }.into_any()
                    } else {
                        view! { <SettingsBadge label="Disabled".into() tone=BadgeTone::Muted/> }.into_any()
                    }}
                }.into_any())
                details=Box::new(move || view! {
                    <SettingsKv label="ID" value=id_kv/>
                    <SettingsKv label="Zone" value=zone_kv/>
                    <SettingsKv label="Days" value=weekdays_kv/>
                    <SettingsKv label="Start time" value=time_kv/>
                    <SettingsKv label="Duration" value=dur_kv/>
                    <SettingsKv label="Mode" value=mode_kv/>
                }.into_any())
                actions=Box::new(move || view! {
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        aria-label=format!("Edit schedule {id_for_edit_label}")
                        on:click=on_edit
                    >
                        "Edit"
                    </button>
                    <button
                        class="setup-footer__btn setup-footer__btn--danger"
                        type="button"
                        aria-label=format!("Delete schedule {id_for_delete_label}")
                        on:click=on_delete
                    >
                        "Delete"
                    </button>
                }.into_any())
            />
        </li>
    }
}
