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
use crate::components::ui::{
    Button, FormField, HelpHint, Panel, SegmentedControl, Sheet, SheetVariant, Toggle,
};

/// The rule fields the engine learned in 0.9.0, as one bundle of signals
/// so the form, the card's edit path and the draft reset take one value
/// rather than six more props each.
///
/// Every field is optional on the wire and absent from a rule written
/// before it existed; `load` reads what is there and `write_into` writes
/// only what is set, so an older rule round-trips without growing keys
/// it never used.
#[derive(Clone, Copy)]
struct RestrictionExtras {
    /// "off" | "match_address" | "odd_dates" | "even_dates"
    date_parity: RwSignal<String>,
    skip_31st: RwSignal<bool>,
    max_days_per_week: RwSignal<String>,
    allowed_weekdays: RwSignal<Vec<u8>>,
    exempt_sprinklers: RwSignal<Vec<String>>,
    zones: RwSignal<Vec<String>>,
}

impl RestrictionExtras {
    fn new() -> Self {
        Self {
            date_parity: RwSignal::new("off".to_string()),
            skip_31st: RwSignal::new(false),
            max_days_per_week: RwSignal::new(String::new()),
            allowed_weekdays: RwSignal::new(Vec::new()),
            exempt_sprinklers: RwSignal::new(Vec::new()),
            zones: RwSignal::new(Vec::new()),
        }
    }

    fn reset(self) {
        self.date_parity.set("off".to_string());
        self.skip_31st.set(false);
        self.max_days_per_week.set(String::new());
        self.allowed_weekdays.set(Vec::new());
        self.exempt_sprinklers.set(Vec::new());
        self.zones.set(Vec::new());
    }

    fn load(self, r: &serde_json::Value) {
        let strings = |key: &str| -> Vec<String> {
            r.get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        self.date_parity.set(
            r.get("date_parity")
                .and_then(|v| v.as_str())
                .unwrap_or("off")
                .to_string(),
        );
        self.skip_31st.set(
            r.get("skip_31st")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        );
        self.max_days_per_week.set(
            r.get("max_days_per_week")
                .and_then(|v| v.as_u64())
                .map(|n| n.to_string())
                .unwrap_or_default(),
        );
        self.allowed_weekdays.set(
            r.get("allowed_weekdays")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_u64())
                        .map(|x| x as u8)
                        .collect()
                })
                .unwrap_or_default(),
        );
        self.exempt_sprinklers.set(strings("exempt_sprinklers"));
        self.zones.set(strings("zones"));
    }

    fn write_into(self, entry: &mut serde_json::Value) {
        let Some(obj) = entry.as_object_mut() else {
            return;
        };
        let parity = self.date_parity.get();
        if parity != "off" {
            obj.insert("date_parity".into(), serde_json::json!(parity));
        }
        if self.skip_31st.get() {
            obj.insert("skip_31st".into(), serde_json::json!(true));
        }
        if let Some(n) = self
            .max_days_per_week
            .get()
            .trim()
            .parse::<u8>()
            .ok()
            .filter(|n| (1..=7).contains(n))
        {
            obj.insert("max_days_per_week".into(), serde_json::json!(n));
        }
        let days = self.allowed_weekdays.get();
        if !days.is_empty() {
            obj.insert("allowed_weekdays".into(), serde_json::json!(days));
        }
        let heads = self.exempt_sprinklers.get();
        if !heads.is_empty() {
            obj.insert("exempt_sprinklers".into(), serde_json::json!(heads));
        }
        let zones = self.zones.get();
        if !zones.is_empty() {
            obj.insert("zones".into(), serde_json::json!(zones));
        }
    }
}

/// A rule depends on the address parity when its odd and even rows name
/// different days, or when it rotates dates by address.
fn rule_needs_parity(r: &serde_json::Value) -> bool {
    let days = |key: &str| -> Vec<u64> {
        let mut v: Vec<u64> = r
            .get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
            .unwrap_or_default();
        v.sort_unstable();
        v.dedup();
        v
    };
    let odd = days("allowed_weekdays_odd");
    let even = days("allowed_weekdays_even");
    let rows_differ = (!odd.is_empty() || !even.is_empty()) && odd != even;
    let by_address = r.get("date_parity").and_then(|v| v.as_str()) == Some("match_address");
    rows_differ || by_address
}

/// One line for the card: what the rule adds beyond weekdays and hours.
fn scope_summary(r: &serde_json::Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    match r.get("date_parity").and_then(|v| v.as_str()) {
        Some("match_address") => parts.push("odd/even dates by address".into()),
        Some("odd_dates") => parts.push("odd dates only".into()),
        Some("even_dates") => parts.push("even dates only".into()),
        _ => {}
    }
    if r.get("skip_31st").and_then(|v| v.as_bool()) == Some(true) {
        parts.push("never on the 31st".into());
    }
    if let Some(n) = r.get("max_days_per_week").and_then(|v| v.as_u64()) {
        parts.push(format!(
            "at most {n} day{} a week",
            if n == 1 { "" } else { "s" }
        ));
    }
    if let Some(days) = r.get("allowed_weekdays").and_then(|v| v.as_array()) {
        if !days.is_empty() {
            parts.push(format!("every address: {}", format_weekdays(Some(days))));
        }
    }
    if let Some(h) = r.get("exempt_sprinklers").and_then(|v| v.as_array()) {
        let names: Vec<&str> = h.iter().filter_map(|x| x.as_str()).collect();
        if !names.is_empty() {
            parts.push(format!("exempt: {}", names.join(", ").replace('_', " ")));
        }
    }
    if let Some(z) = r.get("zones").and_then(|v| v.as_array()) {
        let names: Vec<&str> = z.iter().filter_map(|x| x.as_str()).collect();
        if !names.is_empty() {
            parts.push(format!("only {}", names.join(", ")));
        }
    }
    if parts.is_empty() {
        "(every zone, every day the rows allow)".to_string()
    } else {
        parts.join("; ")
    }
}

/// Replace em-dashes, en-dashes, and the Latin-1-decoded UTF-8 mojibake
/// of either with a plain hyphen so old toml entries written before the
/// `feedback_no_em_dashes` rule still render legibly. Idempotent; safe
/// to call on already-clean strings.
fn sanitize_name(raw: &str) -> String {
    raw.replace(['\u{2014}', '\u{2013}'], "-") // U+2013 EN DASH
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

/// The effective window to save, given the picker's kind and the window
/// this rule was loaded with.
///
/// Split out of the save closure so it can be tested. It used to end in
/// `_ => all_year`, which meant a window the form has no control for was
/// rewritten to year-round the moment anyone touched the rule, even to
/// rename it or switch it off. That turns a seasonal legal restriction
/// into a permanent one, silently, and a compliance rule is the worst
/// possible thing to quietly change.
pub fn resolve_effective_window(
    kind: &str,
    loaded: Option<&serde_json::Value>,
    date_range: (u32, u32, u32, u32),
) -> serde_json::Value {
    let (start_month, start_day, end_month, end_day) = date_range;
    match kind {
        "dst_only" => serde_json::json!({ "kind": "dst_only" }),
        "standard_only" => serde_json::json!({ "kind": "standard_only" }),
        "date_range" => serde_json::json!({
            "kind": "date_range",
            "start_month": start_month,
            "start_day": start_day,
            "end_month": end_month,
            "end_day": end_day,
        }),
        "all_year" => serde_json::json!({ "kind": "all_year" }),
        // Anything else is a window this form cannot author, such as a
        // jurisdiction's own floating seasonal dates. Carry it through
        // byte for byte.
        other => loaded
            .filter(|o| o.get("kind").and_then(|k| k.as_str()) == Some(other))
            .cloned()
            .unwrap_or_else(|| serde_json::json!({ "kind": "all_year" })),
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
    // The effective window EXACTLY as it was loaded.
    //
    // The save path below ended in `_ => all_year`, so a window this form
    // has no control for was rewritten to year-round the moment anyone
    // touched the rule, even just to rename it or switch it off. A
    // seasonal legal rule silently became a permanent one, and nothing
    // told the operator. Holding the original lets an unrecognized window
    // pass through untouched.
    let loaded_effective: RwSignal<Option<serde_json::Value>> = RwSignal::new(None);
    let new_date_start_month = RwSignal::new(3u32);
    let new_date_start_day = RwSignal::new(8u32);
    let new_date_end_month = RwSignal::new(11u32);
    let new_date_end_day = RwSignal::new(1u32);
    let new_weekdays_odd: RwSignal<Vec<u8>> = RwSignal::new(Vec::new());
    let new_weekdays_even: RwSignal<Vec<u8>> = RwSignal::new(Vec::new());
    let new_forbidden_hour_start = RwSignal::new(String::new());
    let new_forbidden_hour_end = RwSignal::new(String::new());
    let new_max_minutes = RwSignal::new(String::new());
    let extras = RestrictionExtras::new();

    let parity = RwSignal::new("not_applicable".to_string());

    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    // Commit-immediately: every add / edit / delete persists on its own (no more
    // "Add to list -> Save all changes" two-step). Also pushes the parity radio
    // into deployment.address_parity and heals any em-dash mojibake on save.
    let persist = Callback::new(move |()| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let mut cfg = config_json.get();
        if let Some(dep) = cfg.get_mut("deployment").and_then(|d| d.as_object_mut()) {
            dep.insert("address_parity".into(), serde_json::json!(parity.get()));
        }
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
                // Restart reasons are not surfaced on this page: a
                // restriction change hot-reloads on the engine's next tick.
                match crate::components::config_client::put_config(&cfg).await {
                    Ok(_) => {
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
                        loaded_effective=loaded_effective
                        new_date_start_month=new_date_start_month
                        new_date_start_day=new_date_start_day
                        new_date_end_month=new_date_end_month
                        new_date_end_day=new_date_end_day
                        new_weekdays_odd=new_weekdays_odd
                        new_weekdays_even=new_weekdays_even
                        new_forbidden_hour_start=new_forbidden_hour_start
                        new_forbidden_hour_end=new_forbidden_hour_end
                        new_max_minutes=new_max_minutes
                        extras=extras
                        editing_id=editing_id
                        add_open=add_open
                        persist=persist
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
        // Commit the starter immediately; the user edits it in place from the list.
        persist.run(());
    });

    // True when an enabled restriction genuinely depends on the address
    // parity and the operator has not picked one: odd and even weekday
    // rows that DIFFER, or a date rotation keyed on the address. Rows
    // that agree bind on their own now, so they no longer need the alert.
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
                        && rule_needs_parity(r)
                })
            })
            .unwrap_or(false)
    };

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Watering restrictions"<HelpHint topic="restrictions"/></h1>
                <p class="settings-page__subtitle">
                    "Honor watering rules from your local water authority, council, water management district, or HOA. "
                    "Restrictions can gate the live verdict (skip when not allowed), "
                    "cap the per-zone dispatch length, and follow odd/even address weekday rotation. "
                    "Stacks with all your skip-rule thresholds; the tightest rule wins."
                </p>
            </header>

            <Show when=needs_parity>
                <div class="setup-result setup-result--err" role="alert" class:u-mb4-only=true>
                    <strong>"Address parity is N/A "</strong>
                    "but at least one enabled restriction has odd/even weekday rules. "
                    "N/A parity means no weekday gate and silently ignores those rules, "
                    "so the dashboard will keep saying 'water tomorrow' even when the regulation forbids it. "
                    "Pick Odd or Even below to save and apply the schedule."
                </div>
            </Show>

            <Panel title="Address parity".to_string()>
                <p class="settings-page__subtitle" class:u-mb3=true>
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
                    on_change=Callback::new(move |_| persist.run(()))
                />
            </Panel>

            <Panel title="Starter templates".to_string()>
                <p class="settings-page__subtitle" class:u-mb3=true>
                    "Many areas limit watering to certain days and hours. Start from a "
                    "common pattern, then edit the days, hours, and dates to match your "
                    "local rules, or build your own with +Add restriction below. "
                    "Check your water utility or municipality for the exact rules where you live."
                </p>
                <div class:u-wrap-row=true>
                    <crate::components::ui::Button
    variant="primary"
    size="md"
    title="No watering during the hottest part of the day (any day)"
    on_click=Callback::new(move |_| add_starter.run(serde_json::json!({
                            "id": "starter_no_midday", "name": "No midday watering", "enabled": true,
                            "effective": { "kind": "all_year" },
                            "forbidden_hour_start": 10, "forbidden_hour_end": 16,
                        })))
    class="setup-footer__btn setup-footer__btn--primary">"No midday watering"</crate::components::ui::Button>
                    <crate::components::ui::Button
    variant="primary"
    size="md"
    title="Water only two days a week (Wed & Sat), no midday"
    on_click=Callback::new(move |_| add_starter.run(serde_json::json!({
                            "id": "starter_two_days", "name": "Two days a week", "enabled": true,
                            "effective": { "kind": "all_year" },
                            "allowed_weekdays": [3, 6],
                            "forbidden_hour_start": 10, "forbidden_hour_end": 16,
                        })))
    class="setup-footer__btn setup-footer__btn--primary">"Two days a week"</crate::components::ui::Button>
                    <crate::components::ui::Button
    variant="primary"
    size="md"
    title="Odd house numbers water Wed/Sat, even Thu/Sun (common parity rule)"
    on_click=Callback::new(move |_| add_starter.run(serde_json::json!({
                            "id": "starter_odd_even", "name": "Odd/even address days", "enabled": true,
                            "effective": { "kind": "all_year" },
                            "allowed_weekdays_odd": [3, 6], "allowed_weekdays_even": [4, 0],
                        })))
    class="setup-footer__btn setup-footer__btn--primary">"Odd/even address days"</crate::components::ui::Button>
                </div>
            </Panel>

            <Panel title="Configured restrictions".to_string() help_topic="restrictions">
                <ul class="settings-card-list">{restrictions_view}</ul>
                <crate::components::ui::Button variant="primary" size="sm"

                    class="setup-footer__btn setup-footer__btn--primary u-mt4"

                    on_click=Callback::new(move |_| {
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
                                extras,
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
                            "+ Add restriction"
                        }
                    }}
                </crate::components::ui::Button>
            </Panel>

            <Sheet
                open=add_open
                title=Signal::derive(move || match editing_id.get() {
                    Some(id) => format!("Editing {id}"),
                    None => "Add a restriction".to_string(),
                })
                variant=SheetVariant::Drawer
            >
                <RestrictionForm
                    config_json=config_json
                    new_id=new_id
                    new_name=new_name
                    new_enabled=new_enabled
                    new_effective_kind=new_effective_kind
                        loaded_effective=loaded_effective
                    new_date_start_month=new_date_start_month
                    new_date_start_day=new_date_start_day
                    new_date_end_month=new_date_end_month
                    new_date_end_day=new_date_end_day
                    new_weekdays_odd=new_weekdays_odd
                    new_weekdays_even=new_weekdays_even
                    new_forbidden_hour_start=new_forbidden_hour_start
                    new_forbidden_hour_end=new_forbidden_hour_end
                    new_max_minutes=new_max_minutes
                    extras=extras
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
    /// The effective window exactly as loaded, so one this form cannot
    /// author survives an edit rather than being flattened to all_year.
    loaded_effective: RwSignal<Option<serde_json::Value>>,
    new_date_start_month: RwSignal<u32>,
    new_date_start_day: RwSignal<u32>,
    new_date_end_month: RwSignal<u32>,
    new_date_end_day: RwSignal<u32>,
    new_weekdays_odd: RwSignal<Vec<u8>>,
    new_weekdays_even: RwSignal<Vec<u8>>,
    new_forbidden_hour_start: RwSignal<String>,
    new_forbidden_hour_end: RwSignal<String>,
    new_max_minutes: RwSignal<String>,
    extras: RestrictionExtras,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    result_msg: RwSignal<String>,
    result_ok: RwSignal<bool>,
    persist: Callback<()>,
) -> impl IntoView {
    let on_add = move |_| {
        let id = crate::text::slugify(&new_id.get());
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
        let effective = resolve_effective_window(
            &new_effective_kind.get(),
            loaded_effective.get().as_ref(),
            (
                new_date_start_month.get(),
                new_date_start_day.get(),
                new_date_end_month.get(),
                new_date_end_day.get(),
            ),
        );
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
        let mut entry = serde_json::json!({
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
        extras.write_into(&mut entry);

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
            extras,
        );
        new_enabled.set(true);
        new_effective_kind.set("all_year".to_string());
        loaded_effective.set(None);
        add_open.set(false);
        // Commit immediately instead of staging for a separate "Save".
        persist.run(());
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
            extras,
        );
        add_open.set(false);
    };

    view! {
        <div id="restriction-form-panel"><Panel title="Restriction form".to_string()>
            <Show when=move || editing_id.get().is_some()>
                <p class="settings-page__subtitle" class:u-mb3=true>
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
                helptext="When this restriction applies. Summer and winter follow the US daylight-saving calendar; elsewhere use Custom range.".to_string()
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
                <div class:u-grid-two=true>
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
                label="Allowed weekdays, odd-numbered addresses".to_string()
                helptext="Check the days odd addresses are allowed to water. Empty = no days.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {weekday_checkboxes(new_weekdays_odd)}
            </FormField>

            <FormField
                label="Allowed weekdays, even-numbered addresses".to_string()
                helptext="Same scheme. The row matching your address parity applies above.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {weekday_checkboxes(new_weekdays_even)}
            </FormField>

            <FormField
                label="Allowed weekdays, every address".to_string()
                helptext="For a rule that ignores your house number. Empty means no such gate.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {weekday_checkboxes(extras.allowed_weekdays)}
            </FormField>

            <div class:u-grid-two=true>
                <FormField
                    label="Date rotation".to_string()
                    helptext="Some districts rotate by calendar date rather than weekday.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <select
                        class="ui-input"
                        prop:value=move || extras.date_parity.get()
                        on:change=move |ev| extras.date_parity.set(event_target_value(&ev))
                    >
                        <option value="off">"None"</option>
                        <option value="match_address">"Odd addresses on odd dates, even on even"</option>
                        <option value="odd_dates">"Everyone on odd dates"</option>
                        <option value="even_dates">"Everyone on even dates"</option>
                    </select>
                </FormField>
                <FormField
                    label="Days per week (optional)".to_string()
                    helptext="At most this many days with a run, Sunday to Saturday. Blank = no limit.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        min="1"
                        max="7"
                        class="ui-input"
                        placeholder="2"
                        prop:value=move || extras.max_days_per_week.get()
                        on:input=move |ev| extras.max_days_per_week.set(event_target_value(&ev))
                    />
                </FormField>
            </div>

            <FormField
                label="The 31st".to_string()
                helptext="Rotations usually skip the 31st, so the odd side does not get two days running.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                <label class="ui-check">
                    <input
                        type="checkbox"
                        prop:checked=move || extras.skip_31st.get()
                        on:change=move |ev| extras.skip_31st.set(event_target_checked(&ev))
                    />
                    " Nobody waters on the 31st"
                </label>
            </FormField>

            <FormField
                label="Exempt sprinkler types".to_string()
                helptext="Heads this rule spares. Many districts exempt drip. The zone's card says it is exempt, but a yard-wide hold still stops it.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {string_chips(extras.exempt_sprinklers, [
                    ("rotor", "Rotor"),
                    ("spray", "Spray"),
                    ("mp_rotator", "MP Rotator"),
                    ("drip", "Drip"),
                    ("bubbler", "Bubbler"),
                    ("other", "Other"),
                ].into_iter().map(|(k, l)| (k.to_string(), l.to_string())).collect())}
            </FormField>

            <FormField
                label="Only these zones (optional)".to_string()
                helptext="Leave every chip off to apply the rule to the whole yard.".to_string()
                error=Signal::derive(|| None::<String>)
            >
                {move || {
                    let slugs: Vec<(String, String)> = config_json
                        .get()
                        .get("zones")
                        .and_then(|z| z.as_object())
                        .map(|o| {
                            o.iter()
                                .map(|(slug, z)| {
                                    let name = z
                                        .get("display_name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or(slug)
                                        .to_string();
                                    (slug.clone(), name)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    if slugs.is_empty() {
                        view! { <p class="settings-page__subtitle">"No zones configured yet."</p> }.into_any()
                    } else {
                        string_chips(extras.zones, slugs).into_any()
                    }
                }}
            </FormField>

            <div class:u-grid-two=true>
                <FormField
                    label="Forbidden hour, start".to_string()
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
                    label="Forbidden hour, end".to_string()
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
                helptext="Caps a single run. The tightest active cap wins, and the zone's own ceiling still applies.".to_string()
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
                <Button variant="ghost" on_click=Callback::new(on_cancel)>
                    "Cancel"
                </Button>
                <Button variant="primary" on_click=Callback::new(on_add)>
                    {move || if editing_id.get().is_some() {
                        "Save restriction changes"
                    } else {
                        "Add restriction"
                    }}
                </Button>
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
    extras: RestrictionExtras,
) {
    editing_id.set(None);
    extras.reset();
    new_id.set(String::new());
    new_name.set(String::new());
    new_weekdays_odd.set(Vec::new());
    new_weekdays_even.set(Vec::new());
    new_forbidden_hour_start.set(String::new());
    new_forbidden_hour_end.set(String::new());
    new_max_minutes.set(String::new());
}

/// Toggle chips over a list of string values, the same shape as the
/// weekday chips.
fn string_chips(value: RwSignal<Vec<String>>, options: Vec<(String, String)>) -> impl IntoView {
    view! {
        <div class:u-wrap-row=true>
            {options
                .into_iter()
                .map(|(key, label)| {
                    let key_for_check = key.clone();
                    let checked = move || value.get().contains(&key_for_check);
                    // The closure owns a String, so it is Clone but not Copy;
                    // the class reader takes its own copy.
                    let is_on = checked.clone();
                    let class = move || {
                        if is_on() {
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
                                    if let Some(pos) = v.iter().position(|x| *x == key) {
                                        v.remove(pos);
                                    } else {
                                        v.push(key.clone());
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
    /// The effective window exactly as loaded, so one this form cannot
    /// author survives an edit rather than being flattened to all_year.
    loaded_effective: RwSignal<Option<serde_json::Value>>,
    new_date_start_month: RwSignal<u32>,
    new_date_start_day: RwSignal<u32>,
    new_date_end_month: RwSignal<u32>,
    new_date_end_day: RwSignal<u32>,
    new_weekdays_odd: RwSignal<Vec<u8>>,
    new_weekdays_even: RwSignal<Vec<u8>>,
    new_forbidden_hour_start: RwSignal<String>,
    new_forbidden_hour_end: RwSignal<String>,
    new_max_minutes: RwSignal<String>,
    extras: RestrictionExtras,
    editing_id: RwSignal<Option<String>>,
    add_open: RwSignal<bool>,
    persist: Callback<()>,
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
    let scope_kv = scope_summary(&restriction);
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
        loaded_effective.set(eff.cloned());
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
        extras.load(r);
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
        persist.run(());
    };

    view! {
        <li class="settings-card-list__item">
            <SettingsCard
                icon="ban".into()
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
                    <SettingsKv label="Also" value=scope_kv/>
                }.into_any())
                actions=Box::new(move || view! {
                    <Button
                        variant="ghost"
                        aria_label=format!("Edit restriction {id_for_edit_label}")
                        on_click=Callback::new(on_edit)
                    >
                        "Edit"
                    </Button>
                    <Button
                        variant="danger"
                        aria_label=format!("Delete restriction {id_for_delete_label}")
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
mod parity_alert_tests {
    use super::{rule_needs_parity, scope_summary};

    /// The "Two days a week" starter used to trip the alert on every
    /// default install, and the alert was right: the rule was inert. Now
    /// the rule binds on its own and the alert is for rules that cannot.
    #[test]
    fn rows_that_agree_do_not_need_a_parity() {
        let same = serde_json::json!({
            "allowed_weekdays_odd": [3, 6], "allowed_weekdays_even": [6, 3]
        });
        assert!(!rule_needs_parity(&same));
        let split = serde_json::json!({
            "allowed_weekdays_odd": [3, 6], "allowed_weekdays_even": [4, 0]
        });
        assert!(rule_needs_parity(&split));
        let by_date = serde_json::json!({ "date_parity": "match_address" });
        assert!(rule_needs_parity(&by_date));
        let everyone = serde_json::json!({ "allowed_weekdays": [3, 6] });
        assert!(!rule_needs_parity(&everyone));
    }

    #[test]
    fn the_scope_line_reads_as_a_sentence() {
        let r = serde_json::json!({
            "date_parity": "match_address", "skip_31st": true,
            "max_days_per_week": 2, "exempt_sprinklers": ["drip", "mp_rotator"],
            "zones": ["front"]
        });
        assert_eq!(
            scope_summary(&r),
            "odd/even dates by address; never on the 31st; at most 2 days a week; \
             exempt: drip, mp rotator; only front"
        );
        assert!(scope_summary(&serde_json::json!({})).starts_with("(every zone"));
    }
}

#[cfg(test)]
mod effective_window_tests {
    use super::resolve_effective_window;

    const DATES: (u32, u32, u32, u32) = (3, 1, 11, 1);

    /// The window this form cannot author must survive an edit.
    ///
    /// A southern-hemisphere district states its own seasonal dates. If
    /// the operator renames the rule, or switches it off and on, the
    /// window has to come back byte for byte. Flattening it to all_year
    /// converts a seasonal legal restriction into a permanent one, which
    /// is both wrong and invisible.
    #[test]
    fn a_window_the_form_cannot_author_survives_an_edit() {
        let sydney = serde_json::json!({
            "kind": "floating_range",
            "start": { "month": 10, "weekday": 0, "nth": "first" },
            "end": { "month": 4, "weekday": 0, "nth": "first" },
            "wraps_year": true
        });
        let saved = resolve_effective_window("floating_range", Some(&sydney), DATES);
        assert_eq!(
            saved, sydney,
            "the seasonal window must be preserved exactly"
        );
    }

    /// The windows the form DOES author are still rebuilt from the
    /// controls, so editing them actually works.
    #[test]
    fn the_authored_windows_are_rebuilt_from_the_form() {
        assert_eq!(
            resolve_effective_window("all_year", None, DATES),
            serde_json::json!({ "kind": "all_year" })
        );
        assert_eq!(
            resolve_effective_window("dst_only", None, DATES),
            serde_json::json!({ "kind": "dst_only" })
        );
        let dr = resolve_effective_window("date_range", None, (12, 1, 2, 28));
        assert_eq!(dr["kind"], "date_range");
        assert_eq!(dr["start_month"], 12);
        assert_eq!(dr["end_day"], 28);
    }

    /// Switching a preserved window TO one the form authors must take the
    /// form's answer, not the stale original.
    #[test]
    fn changing_the_kind_uses_the_form_not_the_original() {
        let sydney = serde_json::json!({ "kind": "floating_range", "wraps_year": true });
        let saved = resolve_effective_window("all_year", Some(&sydney), DATES);
        assert_eq!(saved, serde_json::json!({ "kind": "all_year" }));
    }

    /// A brand new rule with an unknown kind and nothing loaded falls
    /// back to something valid rather than writing a broken window.
    #[test]
    fn an_unknown_kind_with_no_original_is_still_valid() {
        assert_eq!(
            resolve_effective_window("something_new", None, DATES),
            serde_json::json!({ "kind": "all_year" })
        );
    }
}
