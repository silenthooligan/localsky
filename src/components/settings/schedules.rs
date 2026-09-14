// SettingsSchedules. Operator surface for `Config.manual_schedules`.
//
// The scheduler tick lives in src/scheduler/manual.rs; this page just
// builds the list. Save round-trips through GET/PUT /api/config, which swaps
// the schedule set into the dispatcher's live handle (W1.5 ArcSwap); the
// dispatcher loads that handle at the top of every tick and is spawned
// unconditionally at boot, so an added (including the FIRST) or edited
// schedule takes effect on the next tick without a container restart.
//
// Mirrors the editing-state pattern from settings/zones.rs +
// settings/restrictions.rs: `editing_id: Option<String>` switches the
// form panel between Add and Edit, the Save button label flips, and on
// submit the matching entry in the Vec is replaced in-place.
//
// This page also owns the UI half of the per-schedule weather waiver
// (`ignore_weather_safety`): the checkbox that arms it, the ConfirmSheet
// that gates arming it, and the danger badge that keeps an armed
// schedule visible in the list without opening its editor. The other
// half, letting a waived schedule dispatch past the freeze, wind,
// rain-now and live-data holds, is the scheduler's
// (src/scheduler/manual.rs); this file only states the choice and shows
// it. A schedule the scheduler does not understand simply stays gated.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::settings_ui::{
    BadgeTone, SettingsBadge, SettingsCard, SettingsKv, SettingsResult,
};
use crate::components::ui::{
    Button, ConfirmSheet, FormField, HelpHint, Panel, SegmentedControl, Sheet, SheetVariant, Toggle,
};

/// Replace em-dashes, en-dashes, and the Latin-1-decoded UTF-8 mojibake
/// of either with a plain hyphen so old toml entries written before the
/// `feedback_no_em_dashes` rule still render legibly.
fn sanitize_name(raw: &str) -> String {
    raw.replace(['\u{2014}', '\u{2013}'], "-")
        .replace("\u{00e2}\u{0080}\u{0094}", "-")
        .replace("\u{00e2}\u{0080}\u{0093}", "-")
}

/// Config key for the per-schedule weather waiver. Named once so the
/// checkbox that writes it and the list badge that reads it cannot drift
/// apart, and so a rename is one edit.
const WAIVER_KEY: &str = "ignore_weather_safety";

/// The badge an armed schedule wears in the list.
const WAIVER_BADGE: &str = "Ignores weather";

/// Is this saved schedule armed to water through the weather holds?
///
/// Only a literal `true` counts. A missing key (every schedule written
/// before the waiver existed), a null, a string, a number: all read as
/// OFF. A config shape this page cannot make sense of must never be the
/// reason a valve opens into a freeze, so every ambiguity fails toward
/// NOT watering.
fn schedule_waives_weather(schedule: &serde_json::Value) -> bool {
    schedule
        .get(WAIVER_KEY)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The list marker for an armed schedule: `Some(label)` when the waiver
/// is on, `None` otherwise. The marker is the whole point of the badge.
/// Someone scanning this list six months from now has to see which
/// schedule ignores a freeze without opening each editor.
fn waiver_badge_label(schedule: &serde_json::Value) -> Option<&'static str> {
    if schedule_waives_weather(schedule) {
        Some(WAIVER_BADGE)
    } else {
        None
    }
}

/// Does moving the waiver checkbox from `current` to `requested` need
/// the confirmation sheet?
///
/// Only arming it does. Turning it back off makes the schedule safer,
/// and a confirmation in front of the safer choice is just practice at
/// clicking through confirmations.
fn waiver_needs_confirm(current: bool, requested: bool) -> bool {
    requested && !current
}

/// Write the waiver onto a schedule entry about to be saved. Always
/// writes the key, armed or not, so a saved schedule states its weather
/// stance outright instead of leaving it to be inferred from an absent
/// key.
fn stamp_waiver(entry: &mut serde_json::Value, waived: bool) {
    entry[WAIVER_KEY] = serde_json::Value::Bool(waived);
}

/// The "Weather gates" details row for a schedule card.
fn waiver_effect_line(waived: bool) -> &'static str {
    if waived {
        "WAIVED - fires in freeze, wind, rain, and with no live weather data"
    } else {
        "Freeze, wind, rain and missing-data holds all stop this schedule"
    }
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
    // The weather waiver starts OFF on every draft. Arming it is always
    // a deliberate act taken inside the editor, never a default and
    // never inherited from the schedule edited before this one.
    let new_ignore_weather = RwSignal::new(false);
    // A waiver must never survive a drawer the operator walked away from.
    // The "+ Add schedule" toggle resets the draft, but the drawer's own
    // three exits (its X, Escape, the scrim) do not, so an armed checkbox
    // that was confirmed and then abandoned came back armed on the next
    // Add. Disarm on the closing edge, whichever exit produced it. An
    // edit rehydrates the real value when it opens, so this cannot erase
    // a saved waiver.
    //
    // In the component body, NOT inside view!: an Effect written as a
    // bare block there renders as a node on one side of hydration and
    // panics the renderer.
    Effect::new(move |was_open: Option<bool>| {
        let open = add_open.get();
        if was_open == Some(true) && !open {
            new_ignore_weather.set(false);
        }
        open
    });

    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    // Commit-immediately: every add / edit / delete persists on its own (no more
    // "Add to list -> Save all changes" two-step that lost work on navigation).
    let persist = Callback::new(move |()| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let cfg = config_json.get();
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match crate::components::config_client::put_config(&cfg)
                    .await
                    .map(|_| ())
                {
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
            let _ = cfg;
        }
    });

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = crate::components::config_client::get_config().await {
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
        // The scroll-into-view that used to live here is gone with the
        // panel it chased: the form opened below the list, so it could
        // open somewhere off screen. A drawer opens where it opens.
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
                            "Smart irrigation runs on the mornings the weekly water balance says a zone still needs water. "
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
                        new_ignore_weather=new_ignore_weather
                        editing_id=editing_id
                        add_open=add_open
                        persist=persist
                    />
                })
            })
            .collect_view();
        view! { <>{items}</> }.into_any()
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Manual schedules"<HelpHint topic="schedules"/></h1>
                <p class="settings-page__subtitle">
                    "Fire a zone at a fixed weekday + time, on top of (or instead of) "
                    "the smart-irrigation auto-mode. Watering restrictions apply to "
                    "manual schedules just like they do to smart runs: a blocked "
                    "dispatch logs a skip row with the rule's reason."
                </p>
                <p class="settings-page__subtitle" class:u-mt2=true>
                    <strong>"Override"</strong>
                    " (default) replaces the smart engine for the zone: on the days this "
                    "schedule covers, smart watering will not run for that zone at all. "
                    "The zone card says so too. "
                    <strong>"Floor"</strong>
                    " fires the manual run AND lets smart add runs on top when the "
                    "weekly budget calls for more."
                </p>
            </header>

            <Panel title="Configured schedules".to_string()>
                <ul class="settings-card-list">{schedules_view}</ul>
                <crate::components::ui::Button variant="primary" size="sm"

                    class="setup-footer__btn setup-footer__btn--primary u-mt4"

                    on_click=Callback::new(move |_| {
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
                                new_ignore_weather,
                            );
                        }
                    })>
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
                </crate::components::ui::Button>
            </Panel>

            <Sheet
                open=add_open
                title=Signal::derive(move || match editing_id.get() {
                    Some(id) => format!("Editing {id}"),
                    None => "Add a schedule".to_string(),
                })
                variant=SheetVariant::Drawer
            >
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
                    new_ignore_weather=new_ignore_weather
                    editing_id=editing_id
                    add_open=add_open
                    result_msg=result_msg
                    result_ok=result_ok
                    persist=persist
                />
            </Sheet>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
        </div>
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
    new_ignore_weather: RwSignal<bool>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    result_msg: RwSignal<String>,
    result_ok: RwSignal<bool>,
    persist: Callback<()>,
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

    // The waiver's confirmation, on the shared ConfirmSheet idiom: the
    // sheet is always mounted at the bottom of this form and its
    // visibility is owned here. The checkbox only opens it; `arm_waiver`
    // is the single thing that turns the draft flag on, and it runs from
    // the sheet's Confirm.
    let waiver_confirm = RwSignal::new(false);
    let arm_waiver = Callback::new(move |()| new_ignore_weather.set(true));

    let on_add = move |_| {
        let id = crate::text::slugify(&new_id.get());
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
        let mut entry = serde_json::json!({
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
        // Written on every save, armed or not: an edit that clears the
        // box has to persist as a stated "no", not as a missing key.
        stamp_waiver(&mut entry, new_ignore_weather.get());

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
            new_ignore_weather,
        );
        add_open.set(false);
        // Commit immediately instead of staging for a separate "Save".
        persist.run(());
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
            new_ignore_weather,
        );
        add_open.set(false);
    };

    view! {
        <div id="schedule-form-panel"><Panel title="Schedule form".to_string()>
            <Show when=move || editing_id.get().is_some()>
                <p class="settings-page__subtitle" class:u-mb3=true>
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

            <div class:u-grid-two=true>
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
                helptext="Override replaces smart watering on these days. Floor runs alongside it.".to_string()
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

            // The weather waiver. Last field in the form, and the only
            // control on this page that can open a valve into a freeze,
            // so it is the only one with a confirmation in front of it.
            <FormField
                label="Weather holds".to_string()
                helptext="Weather normally stops this schedule. Check the box only if it must run regardless.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label class:u-touch-row=true>
                    <input
                        type="checkbox"
                        prop:checked=move || {
                            // Reading the sheet's signal here is what snaps
                            // the box back off when a turn-on is cancelled:
                            // the click checked the DOM element without
                            // changing the draft, so this property has to be
                            // rewritten every time the sheet opens or closes.
                            // The box therefore reads OFF while the question
                            // is on screen, which is the truth: nothing has
                            // been armed yet.
                            let _asking = waiver_confirm.get();
                            new_ignore_weather.get()
                        }
                        on:change=move |ev| {
                            let requested = event_target_checked(&ev);
                            if waiver_needs_confirm(new_ignore_weather.get_untracked(), requested) {
                                // The box asks; the sheet decides. Nothing
                                // is armed on this path.
                                waiver_confirm.set(true);
                            } else {
                                // Clearing it, or a no-op. Making a schedule
                                // safer is never worth an interruption.
                                new_ignore_weather.set(requested);
                            }
                        }
                    />
                    "Water even in freezing or windy weather"
                </label>
            </FormField>

            <div class="settings-form-actions">
                <Button
                    variant="ghost"
                    on_click=Callback::new(on_cancel)
                >
                    "Cancel"
                </Button>
                <Button
                    variant="primary"
                    on_click=Callback::new(on_add)
                >
                    {move || if editing_id.get().is_some() {
                        "Save schedule changes"
                    } else {
                        "Add schedule"
                    }}
                </Button>
            </div>

            // Arming the waiver is a two-step. Cancel leaves the draft
            // (and the box) off; only Confirm arms it. Clearing the
            // waiver never opens this sheet.
            <ConfirmSheet
                visible=waiver_confirm
                title="Water even in freezing or windy weather?"
                body=Signal::derive(|| {
                    "Watering in a freeze can damage plants and burst pipes. \
                     While this is on, this schedule fires anyway: in a freeze \
                     or on frozen ground, in high wind, in rain, and when live \
                     weather data is missing."
                        .to_string()
                })
                confirm_label=Signal::derive(|| "Yes, water anyway".to_string())
                danger=true
                on_confirm=arm_waiver
            />
        </Panel></div>
    }
}

/// Reset the schedule draft signals back to a blank "new schedule"
/// state. Shared by the page's Cancel toggle and the form's post-add
/// cleanup so the two stay in sync. Leaves `new_zone` and `new_enabled`
/// untouched, matching the original inline reset behavior.
///
/// The weather waiver IS cleared here, unlike those two. A draft that
/// kept the last edited schedule's waiver would arm the next schedule to
/// water through a freeze without anyone having chosen that, which is
/// the one mistake the confirmation exists to make impossible.
fn reset_schedule_draft(
    editing_id: RwSignal<Option<String>>,
    new_id: RwSignal<String>,
    new_name: RwSignal<String>,
    new_weekdays: RwSignal<Vec<u8>>,
    new_duration: RwSignal<u32>,
    new_start_hour: RwSignal<u32>,
    new_start_minute: RwSignal<u32>,
    new_mode: RwSignal<String>,
    new_ignore_weather: RwSignal<bool>,
) {
    editing_id.set(None);
    new_id.set(String::new());
    new_name.set(String::new());
    new_weekdays.set(Vec::new());
    new_duration.set(30);
    new_start_hour.set(5);
    new_start_minute.set(0);
    new_mode.set("override".to_string());
    new_ignore_weather.set(false);
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
        <div class:u-wrap-row=true>
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
    new_ignore_weather: RwSignal<bool>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    persist: Callback<()>,
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
    // Both Copy, so the badge closure reads them without cloning the
    // whole schedule into itself.
    let waives_weather = schedule_waives_weather(&schedule);
    let waiver_badge = waiver_badge_label(&schedule);
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
    let waiver_kv = waiver_effect_line(waives_weather).to_string();
    // An Override schedule suppresses smart watering for its zone on its
    // days. That was the unlabelled default and nothing said it out loud,
    // so a schedule added as a workaround silently locked smart out.
    let suppression_kv = if enabled && mode == "override" && !weekdays.is_empty() {
        format!("Smart watering is off for {zone} on {weekdays}")
    } else if enabled && mode == "floor" {
        // A Floor run is watering evidence like any other: it feeds the
        // weekly balance AND resets the session-spacing anchor. The old
        // flat "Smart watering still runs" was false for a zone whose
        // schedule fires as often as its own session cadence, which is
        // every 1-session-a-week bed on a weekly schedule.
        format!(
            "Smart watering may add runs for {zone}, but not until the zone's session spacing              has passed since this run"
        )
    } else {
        "Nothing suppressed (schedule disabled)".to_string()
    };
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
        // Loaded through the same reader the badge uses, so an editor
        // opened on an armed schedule shows the box already checked and
        // saving it again does not silently disarm it.
        new_ignore_weather.set(schedule_waives_weather(s));
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
        persist.run(());
    };

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon="calendar".into()
                title=name
                subtitle=subtitle
                badges=Box::new(move || view! {
                    {if enabled {
                        view! { <SettingsBadge label="Enabled".into() tone=BadgeTone::Good/> }.into_any()
                    } else {
                        view! { <SettingsBadge label="Disabled".into() tone=BadgeTone::Muted/> }.into_any()
                    }}
                    // The waiver's persistent marker. It rides the card
                    // itself, not the editor, so a schedule armed to
                    // ignore a freeze is legible while scanning the list.
                    {waiver_badge.map(|label| view! {
                        <SettingsBadge label=label.to_string() tone=BadgeTone::Danger/>
                    })}
                }.into_any())
                details=Box::new(move || view! {
                    <SettingsKv label="ID" value=id_kv/>
                    <SettingsKv label="Zone" value=zone_kv/>
                    <SettingsKv label="Days" value=weekdays_kv/>
                    <SettingsKv label="Start time" value=time_kv/>
                    <SettingsKv label="Duration" value=dur_kv/>
                    <SettingsKv label="Mode" value=mode_kv/>
                    <SettingsKv label="Weather gates" value=waiver_kv/>
                    <SettingsKv label="Effect on smart" value=suppression_kv/>
                }.into_any())
                actions=Box::new(move || view! {
                    <Button
                        variant="ghost"
                        aria_label=format!("Edit schedule {id_for_edit_label}")
                        on_click=Callback::new(on_edit)
                    >
                        "Edit"
                    </Button>
                    <Button
                        variant="danger"
                        aria_label=format!("Delete schedule {id_for_delete_label}")
                        on_click=Callback::new(on_delete)
                    >
                        "Delete"
                    </Button>
                }.into_any())
            />
        </li>
    }
}

#[cfg(test)]
mod waiver_tests {
    use super::{
        schedule_waives_weather, stamp_waiver, waiver_badge_label, waiver_effect_line,
        waiver_needs_confirm, WAIVER_BADGE,
    };

    /// Only a literal `true` arms the waiver.
    ///
    /// Everything else reads as OFF: a schedule written before the
    /// waiver existed, a null, the STRING "true", a 1. This is the
    /// direction the ambiguity has to fail in, and it is the reason the
    /// page never tests the key inline: one `!= false` written somewhere
    /// in a view would arm every legacy schedule at once.
    #[test]
    fn only_a_literal_true_arms_the_waiver() {
        let armed = serde_json::json!({ "id": "back_yard", "ignore_weather_safety": true });
        assert!(schedule_waives_weather(&armed));

        for ambiguous in [
            serde_json::json!({ "id": "back_yard" }),
            serde_json::json!({ "ignore_weather_safety": null }),
            serde_json::json!({ "ignore_weather_safety": "true" }),
            serde_json::json!({ "ignore_weather_safety": 1 }),
            serde_json::json!({ "ignore_weather_safety": false }),
        ] {
            assert!(
                !schedule_waives_weather(&ambiguous),
                "{ambiguous} must not arm the waiver"
            );
        }
    }

    /// An armed schedule is marked in the LIST, not only in its editor.
    ///
    /// This is the assertion that fails against the page as it was: the
    /// card rendered exactly two badges, Enabled and Disabled, so a
    /// schedule set to water through a freeze looked identical to one
    /// that would hold, and the only way to find it was to open every
    /// editor in turn.
    #[test]
    fn an_armed_schedule_is_marked_in_the_list() {
        let armed = serde_json::json!({ "id": "back_yard", "ignore_weather_safety": true });
        assert_eq!(waiver_badge_label(&armed), Some(WAIVER_BADGE));
        assert!(waiver_effect_line(true).starts_with("WAIVED"));

        let normal = serde_json::json!({ "id": "back_yard" });
        assert_eq!(waiver_badge_label(&normal), None);
        assert!(!waiver_effect_line(false).contains("WAIVED"));
    }

    /// Arming asks; disarming does not.
    ///
    /// Fails against the shape every other boolean in this form uses:
    /// Enabled is a plain Toggle bound straight to its draft signal, and
    /// a waiver wired that way would arm on the click itself, with no
    /// question in front of it and nothing to cancel.
    #[test]
    fn arming_asks_and_disarming_does_not() {
        assert!(waiver_needs_confirm(false, true), "arming must ask");
        assert!(
            !waiver_needs_confirm(true, false),
            "disarming must not interrupt"
        );
        assert!(!waiver_needs_confirm(true, true));
        assert!(!waiver_needs_confirm(false, false));
    }

    /// A save states the waiver either way, so clearing the box persists
    /// as a stated "no" rather than as an absent key.
    #[test]
    fn a_save_states_the_waiver_either_way() {
        let mut entry = serde_json::json!({ "id": "back_yard", "mode": "override" });

        stamp_waiver(&mut entry, true);
        assert_eq!(entry["ignore_weather_safety"], serde_json::json!(true));
        assert!(schedule_waives_weather(&entry));

        stamp_waiver(&mut entry, false);
        assert_eq!(entry["ignore_weather_safety"], serde_json::json!(false));
        assert!(!schedule_waives_weather(&entry));
    }
}
