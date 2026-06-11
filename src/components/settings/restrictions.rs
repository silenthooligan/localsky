// SettingsRestrictions. Operator surface for the watering-restriction
// system (engine layer in src/engine/restrictions.rs, schema in
// src/config/schema.rs). Round-trips through /api/config like every
// other settings page.
//
// Three surfaces on this page:
//   1. Address-parity radio (binds to deployment.address_parity). The
//      engine evaluator uses this to pick which weekday list of a
//      restriction applies to this household.
//   2. Starter-template panel: one click adds a common generic restriction
//      pattern (no-midday / two-days-a-week / odd-even) the user then edits.
//   3. List + add/edit form for engine.watering_restrictions.
//
// Mirrors the editing-state pattern from settings/zones.rs: an
// `editing_id: Option<String>` switches the form panel between Add and
// Edit, the Save button label flips accordingly, and on submit the
// matching entry in the Vec is replaced in-place.

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::settings_ui::{
    BadgeTone, SettingsBadge, SettingsCard, SettingsKv, SettingsResult,
};
use crate::components::ui::{FormField, Panel, SegmentedControl, Toggle};

/// Replace em-dashes, en-dashes, and the Latin-1-decoded UTF-8 mojibake
/// of either with a plain hyphen so old toml entries written before the
/// `feedback_no_em_dashes` rule still render legibly. Idempotent; safe
/// to call on already-clean strings.
fn sanitize_name(raw: &str) -> String {
    raw.replace('\u{2014}', "-") // U+2014 EM DASH
        .replace('\u{2013}', "-") // U+2013 EN DASH
        .replace("\u{00e2}\u{0080}\u{0094}", "-") // Latin-1-decoded UTF-8 of em-dash
        .replace("\u{00e2}\u{0080}\u{0093}", "-") // Latin-1-decoded UTF-8 of en-dash
}

/// Format a JSON weekday array (0=Sun, 6=Sat) as a comma-separated
/// short-name list. Returns "(any)" for None, "(none)" for an empty
/// array. Used by the read-only card view; the edit form still
/// renders the structured weekday picker.
fn format_weekdays(arr: Option<&Vec<serde_json::Value>>) -> String {
    let days = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    match arr {
        None => "(any)".to_string(),
        Some(a) if a.is_empty() => "(none)".to_string(),
        Some(a) => a
            .iter()
            .filter_map(|x| x.as_u64())
            .map(|x| days.get(x as usize).copied().unwrap_or("?"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

#[component]
pub fn SettingsRestrictions() -> impl IntoView {
    // Whole-config JSON. Loaded from /api/config on mount, mutated by
    // every Edit/Delete/Save action, persisted back on "Save all".
    let config_json = RwSignal::new(serde_json::Value::Null);

    // -- Form state, shared by Add and Edit --
    let add_open = RwSignal::new(false);
    let editing_id: RwSignal<Option<String>> = RwSignal::new(None);
    let new_id = RwSignal::new(String::new());
    let new_name = RwSignal::new(String::new());
    let new_enabled = RwSignal::new(true);
    let new_effective_kind = RwSignal::new("all_year".to_string());
    let new_date_start_month = RwSignal::new(3u32);
    let new_date_start_day = RwSignal::new(8u32);
    let new_date_end_month = RwSignal::new(11u32);
    let new_date_end_day = RwSignal::new(1u32);
    let new_weekdays_odd: RwSignal<Vec<u8>> = RwSignal::new(Vec::new());
    let new_weekdays_even: RwSignal<Vec<u8>> = RwSignal::new(Vec::new());
    let new_forbidden_hour_start = RwSignal::new(String::new());
    let new_forbidden_hour_end = RwSignal::new(String::new());
    let new_max_minutes = RwSignal::new(String::new());

    let parity = RwSignal::new("not_applicable".to_string());

    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(cfg) = fetch_config().await {
                    if let Some(p) = cfg
                        .get("deployment")
                        .and_then(|d| d.get("address_parity"))
                        .and_then(|v| v.as_str())
                    {
                        parity.set(p.to_string());
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
                if let Some(elt) = doc.get_element_by_id("restriction-form-panel") {
                    let opts = web_sys::ScrollIntoViewOptions::new();
                    opts.set_behavior(web_sys::ScrollBehavior::Smooth);
                    opts.set_block(web_sys::ScrollLogicalPosition::Start);
                    elt.scroll_into_view_with_scroll_into_view_options(&opts);
                }
            }
        });
    }

    let restrictions_view = move || {
        let cfg = config_json.get();
        let arr = cfg
            .get("engine")
            .and_then(|e| e.get("watering_restrictions"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            return view! {
                <li class="settings-list__item">
                    <span class="settings-list__icon" aria-hidden="true"><crate::components::ui::Icon name="rules" size=18/></span>
                    <span class="settings-list__text">
                        <span class="settings-list__label">"No restrictions configured"</span>
                        <span class="settings-list__helptext">
                            "Pick a starter template above, or +Add restriction below to enter your area\u{2019}s allowed days and hours."
                        </span>
                    </span>
                </li>
            }
            .into_any();
        }
        let items = arr
            .into_iter()
            .filter_map(|r| {
                let id = r.get("id").and_then(|v| v.as_str())?.to_string();
                Some(view! {
                    <RestrictionCard
                        id=id
                        restriction=r
                        config_json=config_json
                        new_id=new_id
                        new_name=new_name
                        new_enabled=new_enabled
                        new_effective_kind=new_effective_kind
                        new_date_start_month=new_date_start_month
                        new_date_start_day=new_date_start_day
                        new_date_end_month=new_date_end_month
                        new_date_end_day=new_date_end_day
                        new_weekdays_odd=new_weekdays_odd
                        new_weekdays_even=new_weekdays_even
                        new_forbidden_hour_start=new_forbidden_hour_start
                        new_forbidden_hour_end=new_forbidden_hour_end
                        new_max_minutes=new_max_minutes
                        editing_id=editing_id
                        add_open=add_open
                    />
                })
            })
            .collect_view();
        view! { <>{items}</> }.into_any()
    };

    // Add a generic starter restriction (the user then edits it for their
    // area). Re-adding the same template replaces it rather than duplicating.
    let add_starter = Callback::new(move |restriction: serde_json::Value| {
        let id = restriction
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        config_json.update(|cfg| {
            let engine = cfg.as_object_mut().and_then(|o| {
                o.entry("engine")
                    .or_insert(serde_json::json!({}))
                    .as_object_mut()
            });
            if let Some(eng) = engine {
                let arr = eng
                    .entry("watering_restrictions")
                    .or_insert(serde_json::json!([]))
                    .as_array_mut()
                    .unwrap();
                arr.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(id.as_str()));
                arr.push(restriction);
            }
        });
        result_ok.set(true);
        result_msg
            .set("Added a starter restriction. Edit it for your area, then Save below.".into());
    });

    let on_save = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let mut cfg = config_json.get();
        // Push the parity radio back into deployment.address_parity before
        // we persist.
        if let Some(dep) = cfg.get_mut("deployment").and_then(|d| d.as_object_mut()) {
            dep.insert("address_parity".into(), serde_json::json!(parity.get()));
        }
        // Sanitize the `name` field on every restriction so a save heals
        // any em-dash mojibake left over from pre-fix entries. Idempotent.
        if let Some(arr) = cfg
            .get_mut("engine")
            .and_then(|e| e.get_mut("watering_restrictions"))
            .and_then(|v| v.as_array_mut())
        {
            for r in arr.iter_mut() {
                if let Some(name_val) = r.get("name").cloned() {
                    if let Some(raw) = name_val.as_str() {
                        let cleaned = sanitize_name(raw);
                        if cleaned != raw {
                            r.as_object_mut()
                                .unwrap()
                                .insert("name".into(), serde_json::json!(cleaned));
                        }
                    }
                }
            }
        }
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match save_config(cfg).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Engine picks up restrictions on next tick.",
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

    // True when there's at least one enabled restriction with a non-empty
    // weekday list and the operator hasn't picked an address parity yet.
    // The engine's allowed_today() returns true (bypasses) on N/A parity,
    // which silently disables an odd/even weekday gate. Surface that loudly
    // here so the user knows why their restriction isn't blocking runs.
    let needs_parity = move || {
        if parity.get() != "not_applicable" {
            return false;
        }
        let cfg = config_json.get();
        cfg.get("engine")
            .and_then(|e| e.get("watering_restrictions"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter().any(|r| {
                    r.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)
                        && r.get("allowed_weekdays_odd")
                            .and_then(|v| v.as_array())
                            .map(|a| !a.is_empty())
                            .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"Back to Settings"</a>
                <h1 class="settings-page__title">"Watering restrictions"</h1>
                <p class="settings-page__subtitle">
                    "Honor watering rules from your local water authority, council, water management district, or HOA. "
                    "Restrictions can gate the live verdict (skip when not allowed), "
                    "cap the per-zone dispatch length, and follow odd/even address weekday rotation. "
                    "Stacks with all your skip-rule thresholds; the tightest rule wins."
                </p>
            </header>

            <Show when=needs_parity>
                <div class="setup-result setup-result--err" role="alert" style="margin-bottom: 1rem">
                    <strong>"Address parity is N/A "</strong>
                    "but at least one enabled restriction has odd/even weekday rules. "
                    "The engine treats N/A parity as 'no weekday gate' and silently ignores those rules, "
                    "so the dashboard will keep saying 'water tomorrow' even when the regulation forbids it. "
                    "Pick Odd or Even below and click "
                    <strong>"Save all changes"</strong>
                    " to enforce the schedule."
                </div>
            </Show>

            <Panel title="Address parity".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "Many jurisdictions split the watering schedule by house number. "
                    "Set yours here once; each restriction's odd/even weekday list is matched against it."
                </p>
                <SegmentedControl
                    value=parity
                    options=vec![
                        ("not_applicable".into(), "N/A".into()),
                        ("odd".into(), "Odd".into()),
                        ("even".into(), "Even".into()),
                    ]
                    aria_label="Address parity".to_string()
                />
            </Panel>

            <Panel title="Starter templates".to_string()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "Many areas limit watering to certain days and hours. Start from a "
                    "common pattern, then edit the days, hours, and dates to match your "
                    "local rules \u{2014} or build your own with +Add restriction below. "
                    "Check your water utility or municipality for the exact rules where you live."
                </p>
                <div style="display:flex; gap:0.5rem; flex-wrap:wrap">
                    <button type="button" class="setup-footer__btn setup-footer__btn--primary"
                        title="No watering during the hottest part of the day (any day)"
                        on:click=move |_| add_starter.run(serde_json::json!({
                            "id": "starter_no_midday", "name": "No midday watering", "enabled": true,
                            "effective": { "kind": "all_year" },
                            "forbidden_hour_start": 10, "forbidden_hour_end": 16,
                        }))>"No midday watering"</button>
                    <button type="button" class="setup-footer__btn setup-footer__btn--primary"
                        title="Water only two days a week (Wed & Sat), no midday"
                        on:click=move |_| add_starter.run(serde_json::json!({
                            "id": "starter_two_days", "name": "Two days a week", "enabled": true,
                            "effective": { "kind": "all_year" },
                            "allowed_weekdays_odd": [3, 6], "allowed_weekdays_even": [3, 6],
                            "forbidden_hour_start": 10, "forbidden_hour_end": 16,
                        }))>"Two days a week"</button>
                    <button type="button" class="setup-footer__btn setup-footer__btn--primary"
                        title="Odd house numbers water Wed/Sat, even Thu/Sun (common parity rule)"
                        on:click=move |_| add_starter.run(serde_json::json!({
                            "id": "starter_odd_even", "name": "Odd/even address days", "enabled": true,
                            "effective": { "kind": "all_year" },
                            "allowed_weekdays_odd": [3, 6], "allowed_weekdays_even": [4, 0],
                        }))>"Odd/even address days"</button>
                </div>
            </Panel>

            <Panel title="Configured restrictions".to_string()>
                <ul class="settings-card-list">{restrictions_view}</ul>
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    style="margin-top: 1rem"
                    on:click=move |_| {
                        let now_open = add_open.get();
                        add_open.set(!now_open);
                        if now_open {
                            reset_restriction_draft(
                                editing_id,
                                new_id,
                                new_name,
                                new_weekdays_odd,
                                new_weekdays_even,
                                new_forbidden_hour_start,
                                new_forbidden_hour_end,
                                new_max_minutes,
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
                            "+ Add restriction"
                        }
                    }}
                </button>
            </Panel>

            <Show when=move || add_open.get()>
                <RestrictionForm
                    config_json=config_json
                    new_id=new_id
                    new_name=new_name
                    new_enabled=new_enabled
                    new_effective_kind=new_effective_kind
                    new_date_start_month=new_date_start_month
                    new_date_start_day=new_date_start_day
                    new_date_end_month=new_date_end_month
                    new_date_end_day=new_date_end_day
                    new_weekdays_odd=new_weekdays_odd
                    new_weekdays_even=new_weekdays_even
                    new_forbidden_hour_start=new_forbidden_hour_start
                    new_forbidden_hour_end=new_forbidden_hour_end
                    new_max_minutes=new_max_minutes
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

/// Add/edit form for a single watering restriction, extracted out of the
/// page component so the page is a thin shell (header + parity/preset/
/// list panels + save bar) and this whole `<Panel>` view tree compiles
/// inside its own monomorphization boundary instead of nesting into the
/// page. Owns the "add to in-memory config" handler; the page still owns
/// the load (Effect) and the persist (Save all changes -> PUT).
#[component]
fn RestrictionForm(
    config_json: RwSignal<serde_json::Value>,
    new_id: RwSignal<String>,
    new_name: RwSignal<String>,
    new_enabled: RwSignal<bool>,
    new_effective_kind: RwSignal<String>,
    new_date_start_month: RwSignal<u32>,
    new_date_start_day: RwSignal<u32>,
    new_date_end_month: RwSignal<u32>,
    new_date_end_day: RwSignal<u32>,
    new_weekdays_odd: RwSignal<Vec<u8>>,
    new_weekdays_even: RwSignal<Vec<u8>>,
    new_forbidden_hour_start: RwSignal<String>,
    new_forbidden_hour_end: RwSignal<String>,
    new_max_minutes: RwSignal<String>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    result_msg: RwSignal<String>,
    result_ok: RwSignal<bool>,
) -> impl IntoView {
    let on_add = move |_| {
        let id = new_id.get().trim().to_lowercase().replace(' ', "_");
        if id.is_empty() {
            result_ok.set(false);
            result_msg.set("ID is required (snake_case, e.g. hoa_summer)".into());
            return;
        }
        let name = if new_name.get().is_empty() {
            id.clone()
        } else {
            new_name.get()
        };
        let effective = match new_effective_kind.get().as_str() {
            "dst_only" => serde_json::json!({ "kind": "dst_only" }),
            "standard_only" => serde_json::json!({ "kind": "standard_only" }),
            "date_range" => serde_json::json!({
                "kind": "date_range",
                "start_month": new_date_start_month.get(),
                "start_day": new_date_start_day.get(),
                "end_month": new_date_end_month.get(),
                "end_day": new_date_end_day.get(),
            }),
            _ => serde_json::json!({ "kind": "all_year" }),
        };
        let fhs = new_forbidden_hour_start
            .get()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|h| *h < 24);
        let fhe = new_forbidden_hour_end
            .get()
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|h| *h <= 24);
        let mmpz = new_max_minutes.get().trim().parse::<u32>().ok();
        let entry = serde_json::json!({
            "id": id,
            "name": name,
            "enabled": new_enabled.get(),
            "effective": effective,
            "allowed_weekdays_odd": new_weekdays_odd.get(),
            "allowed_weekdays_even": new_weekdays_even.get(),
            "forbidden_hour_start": fhs,
            "forbidden_hour_end": fhe,
            "max_minutes_per_zone": mmpz,
        });

        let was_edit = editing_id.get().is_some();
        config_json.update(|cfg| {
            let engine = cfg.as_object_mut().and_then(|o| {
                o.entry("engine")
                    .or_insert(serde_json::json!({}))
                    .as_object_mut()
            });
            if let Some(eng) = engine {
                let arr = eng
                    .entry("watering_restrictions")
                    .or_insert(serde_json::json!([]))
                    .as_array_mut()
                    .unwrap();
                if was_edit {
                    // Replace matching id in-place; preserve ordering.
                    let target = editing_id.get().unwrap_or_default();
                    if let Some(idx) = arr
                        .iter()
                        .position(|r| r.get("id").and_then(|v| v.as_str()) == Some(target.as_str()))
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

        // Reset form state.
        reset_restriction_draft(
            editing_id,
            new_id,
            new_name,
            new_weekdays_odd,
            new_weekdays_even,
            new_forbidden_hour_start,
            new_forbidden_hour_end,
            new_max_minutes,
        );
        new_enabled.set(true);
        new_effective_kind.set("all_year".to_string());
        add_open.set(false);
        result_ok.set(true);
        result_msg.set(if was_edit {
            "Updated restriction. Click Save below to persist.".to_string()
        } else {
            "Added restriction. Click Save below to persist.".to_string()
        });
    };

    let on_cancel = move |_| {
        reset_restriction_draft(
            editing_id,
            new_id,
            new_name,
            new_weekdays_odd,
            new_weekdays_even,
            new_forbidden_hour_start,
            new_forbidden_hour_end,
            new_max_minutes,
        );
        add_open.set(false);
    };

    view! {
        <div id="restriction-form-panel"><Panel title="Restriction form".to_string()>
            <Show when=move || editing_id.get().is_some()>
                <p class="settings-page__subtitle" style="margin: 0 0 0.75rem">
                    "Editing "
                    <code>{move || editing_id.get().unwrap_or_default()}</code>
                    ". Save below applies to this id; the id field is read-only."
                </p>
            </Show>

            <FormField
                label="ID".to_string()
                helptext="snake_case identifier (e.g. two_days, no_midday, hoa_summer). Read-only while editing.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="hoa_summer"
                    prop:value=move || new_id.get()
                    prop:disabled=move || editing_id.get().is_some()
                    on:input=move |ev| new_id.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Display name".to_string()
                helptext="Human label for the dashboard's verdict reason.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="text"
                    class="ui-input"
                    placeholder="HOA summer rules"
                    prop:value=move || new_name.get()
                    on:input=move |ev| new_name.set(event_target_value(&ev))
                />
            </FormField>

            <FormField
                label="Enabled".to_string()
                helptext="Disable to keep the entry but skip evaluation. Useful for season-bound rules you don't want to delete.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <Toggle
                    checked=new_enabled
                    label="Honor this restriction".to_string()
                    helptext="".to_string()
                />
            </FormField>

            <FormField
                label="Effective window".to_string()
                helptext="When this restriction is active. Most areas use All year. Summer/winter follow the US daylight-saving calendar; outside the US, use Custom range for seasonal rules.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <SegmentedControl
                    value=new_effective_kind
                    options=vec![
                        ("all_year".into(), "All year".into()),
                        ("dst_only".into(), "Summer (US DST)".into()),
                        ("standard_only".into(), "Winter (US standard)".into()),
                        ("date_range".into(), "Custom range".into()),
                    ]
                    aria_label="Effective window".to_string()
                />
            </FormField>

            <Show when=move || new_effective_kind.get() == "date_range">
                <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem">
                    <FormField
                        label="Start month".to_string()
                        helptext="1=Jan..12=Dec".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {month_input(new_date_start_month)}
                    </FormField>
                    <FormField
                        label="Start day".to_string()
                        helptext="1..31".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {day_input(new_date_start_day)}
                    </FormField>
                    <FormField
                        label="End month".to_string()
                        helptext="1=Jan..12=Dec".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {month_input(new_date_end_month)}
                    </FormField>
                    <FormField
                        label="End day".to_string()
                        helptext="1..31".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        {day_input(new_date_end_day)}
                    </FormField>
                </div>
            </Show>

            <FormField
                label="Allowed weekdays — odd-numbered addresses".to_string()
                helptext="Check the days odd addresses are allowed to water. Empty = no days.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {weekday_checkboxes(new_weekdays_odd)}
            </FormField>

            <FormField
                label="Allowed weekdays — even-numbered addresses".to_string()
                helptext="Same scheme. The engine picks the row that matches your address parity above.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {weekday_checkboxes(new_weekdays_even)}
            </FormField>

            <div style="display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem">
                <FormField
                    label="Forbidden hour — start".to_string()
                    helptext="0..23. Blank = no time gate. Example: 10 (forbids 10:00 onward).".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        min="0"
                        max="23"
                        class="ui-input"
                        placeholder="10"
                        prop:value=move || new_forbidden_hour_start.get()
                        on:input=move |ev| new_forbidden_hour_start.set(event_target_value(&ev))
                    />
                </FormField>

                <FormField
                    label="Forbidden hour — end".to_string()
                    helptext="0..24. Blank = no time gate. Example: 16 (re-allows watering at 16:00).".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        min="0"
                        max="24"
                        class="ui-input"
                        placeholder="16"
                        prop:value=move || new_forbidden_hour_end.get()
                        on:input=move |ev| new_forbidden_hour_end.set(event_target_value(&ev))
                    />
                </FormField>
            </div>

            <FormField
                label="Max minutes per zone (optional)".to_string()
                helptext="Caps the per-dispatch run length. The tightest cap across active restrictions wins, then is min()'d with the zone's own ceiling.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <input
                    type="number"
                    min="1"
                    class="ui-input"
                    placeholder="60"
                    prop:value=move || new_max_minutes.get()
                    on:input=move |ev| new_max_minutes.set(event_target_value(&ev))
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
                        "Save restriction changes"
                    } else {
                        "Add to list"
                    }}
                </button>
            </div>
        </Panel></div>
    }
}

/// Reset the restriction draft signals shared by the page's Cancel
/// toggle and the form's post-add cleanup. Covers the fields both reset
/// paths clear; the form additionally resets enabled + effective-kind
/// after a successful add.
fn reset_restriction_draft(
    editing_id: RwSignal<Option<String>>,
    new_id: RwSignal<String>,
    new_name: RwSignal<String>,
    new_weekdays_odd: RwSignal<Vec<u8>>,
    new_weekdays_even: RwSignal<Vec<u8>>,
    new_forbidden_hour_start: RwSignal<String>,
    new_forbidden_hour_end: RwSignal<String>,
    new_max_minutes: RwSignal<String>,
) {
    editing_id.set(None);
    new_id.set(String::new());
    new_name.set(String::new());
    new_weekdays_odd.set(Vec::new());
    new_weekdays_even.set(Vec::new());
    new_forbidden_hour_start.set(String::new());
    new_forbidden_hour_end.set(String::new());
    new_max_minutes.set(String::new());
}

fn weekday_checkboxes(value: RwSignal<Vec<u8>>) -> impl IntoView {
    // chrono::Weekday::num_days_from_sunday(): 0=Sun, 1=Mon, ..., 6=Sat
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

fn month_input(sig: RwSignal<u32>) -> impl IntoView {
    view! {
        <input
            type="number"
            min="1"
            max="12"
            class="ui-input"
            prop:value=move || sig.get() as f64
            on:input=move |ev| {
                if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                    if (1..=12).contains(&v) {
                        sig.set(v);
                    }
                }
            }
        />
    }
}

fn day_input(sig: RwSignal<u32>) -> impl IntoView {
    view! {
        <input
            type="number"
            min="1"
            max="31"
            class="ui-input"
            prop:value=move || sig.get() as f64
            on:input=move |ev| {
                if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                    if (1..=31).contains(&v) {
                        sig.set(v);
                    }
                }
            }
        />
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

/// Single watering-restriction row. Own component so its monomorphized
/// view tree (badges + 6 KV rows + the long edit-form-populate
/// closure) is contained inside one boundary instead of compounding
/// through the page.
#[component]
fn RestrictionCard(
    id: String,
    restriction: serde_json::Value,
    config_json: RwSignal<serde_json::Value>,
    new_id: RwSignal<String>,
    new_name: RwSignal<String>,
    new_enabled: RwSignal<bool>,
    new_effective_kind: RwSignal<String>,
    new_date_start_month: RwSignal<u32>,
    new_date_start_day: RwSignal<u32>,
    new_date_end_month: RwSignal<u32>,
    new_date_end_day: RwSignal<u32>,
    new_weekdays_odd: RwSignal<Vec<u8>>,
    new_weekdays_even: RwSignal<Vec<u8>>,
    new_forbidden_hour_start: RwSignal<String>,
    new_forbidden_hour_end: RwSignal<String>,
    new_max_minutes: RwSignal<String>,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
) -> impl IntoView {
    let raw_name = restriction
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(&id)
        .to_string();
    let name = sanitize_name(&raw_name);
    let enabled = restriction
        .get("enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let effective_label = match restriction
        .get("effective")
        .and_then(|v| v.get("kind"))
        .and_then(|v| v.as_str())
    {
        Some("dst_only") => "Summer (US DST)",
        Some("standard_only") => "Winter (US standard)",
        Some("date_range") => "Custom date range",
        _ => "All year",
    };
    let weekdays_odd_kv = format_weekdays(
        restriction
            .get("allowed_weekdays_odd")
            .and_then(|v| v.as_array()),
    );
    let weekdays_even_kv = format_weekdays(
        restriction
            .get("allowed_weekdays_even")
            .and_then(|v| v.as_array()),
    );
    let forbidden_kv = match (
        restriction
            .get("forbidden_hour_start")
            .and_then(|v| v.as_u64()),
        restriction
            .get("forbidden_hour_end")
            .and_then(|v| v.as_u64()),
    ) {
        (Some(s), Some(e)) => format!("{s:02}:00 - {e:02}:00"),
        _ => "(none)".to_string(),
    };
    let max_minutes_kv = restriction
        .get("max_minutes_per_zone")
        .and_then(|v| v.as_u64())
        .map(|n| format!("{n} min"))
        .unwrap_or_else(|| "(unlimited)".to_string());
    let subtitle = format!("{id} \u{00b7} {effective_label}");
    let id_kv = id.clone();
    let effective_kv = effective_label.to_string();
    let id_for_edit = id.clone();
    let id_for_delete = id.clone();
    let id_for_edit_label = id.clone();
    let id_for_delete_label = id.clone();
    let r_for_edit = restriction.clone();

    let on_edit = move |_| {
        let r = &r_for_edit;
        new_id.set(id_for_edit.clone());
        new_name.set(
            r.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id_for_edit)
                .to_string(),
        );
        new_enabled.set(r.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true));
        let eff = r.get("effective");
        let kind = eff
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("all_year")
            .to_string();
        new_effective_kind.set(kind);
        new_date_start_month.set(
            eff.and_then(|v| v.get("start_month"))
                .and_then(|v| v.as_u64())
                .unwrap_or(3) as u32,
        );
        new_date_start_day.set(
            eff.and_then(|v| v.get("start_day"))
                .and_then(|v| v.as_u64())
                .unwrap_or(8) as u32,
        );
        new_date_end_month.set(
            eff.and_then(|v| v.get("end_month"))
                .and_then(|v| v.as_u64())
                .unwrap_or(11) as u32,
        );
        new_date_end_day.set(
            eff.and_then(|v| v.get("end_day"))
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as u32,
        );
        new_weekdays_odd.set(
            r.get("allowed_weekdays_odd")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_u64())
                        .map(|x| x as u8)
                        .collect()
                })
                .unwrap_or_default(),
        );
        new_weekdays_even.set(
            r.get("allowed_weekdays_even")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_u64())
                        .map(|x| x as u8)
                        .collect()
                })
                .unwrap_or_default(),
        );
        new_forbidden_hour_start.set(
            r.get("forbidden_hour_start")
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_default(),
        );
        new_forbidden_hour_end.set(
            r.get("forbidden_hour_end")
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_default(),
        );
        new_max_minutes.set(
            r.get("max_minutes_per_zone")
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_default(),
        );
        editing_id.set(Some(id_for_edit.clone()));
        add_open.set(true);
    };
    let on_delete = move |_| {
        let target = id_for_delete.clone();
        config_json.update(|cfg| {
            if let Some(arr) = cfg
                .get_mut("engine")
                .and_then(|e| e.get_mut("watering_restrictions"))
                .and_then(|v| v.as_array_mut())
            {
                arr.retain(|r| r.get("id").and_then(|v| v.as_str()) != Some(&target));
            }
        });
    };

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon="\u{1f6b1}".into()
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
                    <SettingsKv label="Effective" value=effective_kv/>
                    <SettingsKv label="Allowed (odd address)" value=weekdays_odd_kv/>
                    <SettingsKv label="Allowed (even address)" value=weekdays_even_kv/>
                    <SettingsKv label="Forbidden hours" value=forbidden_kv/>
                    <SettingsKv label="Max per zone" value=max_minutes_kv/>
                }.into_any())
                actions=Box::new(move || view! {
                    <button
                        class="setup-footer__btn setup-footer__btn--ghost"
                        type="button"
                        aria-label=format!("Edit restriction {id_for_edit_label}")
                        on:click=on_edit
                    >
                        "Edit"
                    </button>
                    <button
                        class="setup-footer__btn setup-footer__btn--danger"
                        type="button"
                        aria-label=format!("Delete restriction {id_for_delete_label}")
                        on:click=on_delete
                    >
                        "Delete"
                    </button>
                }.into_any())
            />
        </li>
    }
}
