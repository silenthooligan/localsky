// Condition builder, the no-code half of Rule Lab. Lets you compose your
// own watering triggers ("if rain_prob > 60 AND soil > 65 -> skip this
// zone") with dropdowns instead of Rhai. Reads/writes config.conditions
// .rules via the same read-modify-write PUT every settings surface uses.
//
// The backend (engine/conditions.rs) supports an arbitrarily nested
// AND/OR/NOT tree; this editor emits the common shape, match ALL or ANY
// of a flat list of metric comparisons, which covers the great majority
// of real rules. (Hand-authored nested trees still load + run; the editor
// shows them read-only if it can't represent them.)

use leptos::prelude::*;

use crate::components::ui::Button;
use crate::ha::snapshot::{IrrigationSnapshot, SkipCheck};

/// (value, label, unit) for every metric a comparison can read. `value`
/// must match the backend `Metric` serde (snake_case) exactly.
const METRICS: &[(&str, &str, &str)] = &[
    ("zone_soil_pct", "This zone's soil moisture", "%"),
    ("rain_prob_tomorrow", "Rain probability tomorrow", "%"),
    ("rain_next4h_in", "Rain next 4h", "in"),
    ("rain_today_in", "Rain today", "in"),
    ("rain3day_weighted_in", "Rain 3-day (weighted)", "in"),
    ("wind_now_mph", "Wind now", "mph"),
    ("wind_max_today_mph", "Wind max today", "mph"),
    ("temp_now_f", "Temperature now", "°F"),
    ("temp_min24h_f", "Temp min next 24h", "°F"),
    ("temp_max3day_f", "Temp max 3-day", "°F"),
    ("humidity_now_pct", "Humidity now", "%"),
    ("days_since_rain", "Days since rain", "d"),
];

const OPS: &[(&str, &str)] = &[(">", "gt"), ("≥", "gte"), ("<", "lt"), ("≤", "lte")];

fn metric_label(value: &str) -> &'static str {
    METRICS
        .iter()
        .find(|(v, _, _)| *v == value)
        .map(|(_, l, _)| *l)
        .unwrap_or("?")
}
fn op_symbol(serde: &str) -> &'static str {
    OPS.iter()
        .find(|(_, s)| *s == serde)
        .map(|(sym, _)| *sym)
        .unwrap_or("?")
}

/// One comparison row in the editor.
#[derive(Clone, Debug, PartialEq)]
struct Row {
    metric: String,
    op: String,
    value: f64,
}

/// Live value of a metric from the snapshot's skip_check, for the preview.
/// `zone_soil_pct` is per-zone (no single value) so it's not previewable.
fn metric_live(m: &str, s: &SkipCheck) -> Option<f64> {
    Some(match m {
        "rain_prob_tomorrow" => s.rain_tomorrow_prob_pct as f64,
        "rain_next4h_in" => s.rain_next_4h_in,
        "rain_today_in" => s.rain_today_in,
        "rain3day_weighted_in" => s.rain_3day_weighted_in,
        "wind_now_mph" => s.wind_now_mph,
        "wind_max_today_mph" => s.wind_max_today_mph,
        "temp_now_f" => s.temp_now_f,
        "temp_min24h_f" => s.temp_min_24h_f,
        "temp_max3day_f" => s.temp_max_3day_f,
        "humidity_now_pct" => s.humidity_now_pct,
        "days_since_rain" => s.days_since_significant_rain as f64,
        _ => return None,
    })
}

fn op_apply(op: &str, a: f64, b: f64) -> bool {
    match op {
        "gt" => a > b,
        "gte" => a >= b,
        "lt" => a < b,
        "lte" => a <= b,
        _ => false,
    }
}

/// Human one-liner for a stored rule's condition + action (list view).
fn rule_summary(rule: &serde_json::Value) -> String {
    let cond = rule.get("condition");
    let (joiner, rows) =
        if let Some(all) = cond.and_then(|c| c.get("all")).and_then(|v| v.as_array()) {
            (" AND ", all.clone())
        } else if let Some(any) = cond.and_then(|c| c.get("any")).and_then(|v| v.as_array()) {
            (" OR ", any.clone())
        } else {
            return "custom condition".to_string();
        };
    let parts: Vec<String> = rows
        .iter()
        .filter_map(|r| {
            let c = r.get("compare")?;
            Some(format!(
                "{} {} {}",
                metric_label(c.get("metric")?.as_str()?),
                op_symbol(c.get("op")?.as_str()?),
                c.get("value")?.as_f64()?
            ))
        })
        .collect();
    let action = match rule.get("action") {
        Some(serde_json::Value::String(s)) if s == "skip" => "skip".to_string(),
        Some(serde_json::Value::String(s)) if s == "extend" => "extend".to_string(),
        Some(v) if v.get("adjust_multiplier").is_some() => {
            let f = v
                .get("adjust_multiplier")
                .and_then(|a| a.get("factor"))
                .and_then(|x| x.as_f64())
                .unwrap_or(1.0);
            format!("×{f:.2}")
        }
        _ => "?".to_string(),
    };
    format!("if {} → {}", parts.join(joiner), action)
}

/// Curated starting points. Each instantiates as a normal editable rule;
/// values are sensible defaults, not gospel.
#[derive(Clone, Copy)]
struct RuleTemplate {
    name: &'static str,
    desc: &'static str,
    json: &'static str,
}

const RULE_TEMPLATES: &[RuleTemplate] = &[
    RuleTemplate {
        name: "Skip after heavy rain",
        desc: "More than half an inch already today: the yard has had its drink.",
        json: r#"{"id":"skip_heavy_rain","name":"Skip after heavy rain","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"rain_today_in","op":"gt","value":0.5}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Skip cold mornings",
        desc: "Below 45 F at decision time: cold water on cold turf does nothing good.",
        json: r#"{"id":"skip_cold_morning","name":"Skip cold mornings","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"temp_now_f","op":"lt","value":45.0}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Windy morning guard",
        desc: "Wind above 12 mph: spray drifts instead of landing.",
        json: r#"{"id":"skip_windy","name":"Windy morning guard","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"wind_now_mph","op":"gt","value":12.0}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Soil already comfortable",
        desc: "Zone probe above 70 percent: let the model coast.",
        json: r#"{"id":"skip_soil_wet","name":"Soil already comfortable","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"zone_soil_pct","op":"gt","value":70.0}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Heat wave boost",
        desc: "Three-day forecast high above 95 F: stretch runs by a quarter.",
        json: r#"{"id":"heat_boost","name":"Heat wave boost","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"temp_max3day_f","op":"gt","value":95.0}},"action":{"adjust_multiplier":{"factor":1.25}}}"#,
    },
    RuleTemplate {
        name: "Dry spell extend",
        desc: "No meaningful rain for a week: lean a little harder.",
        json: r#"{"id":"dry_spell","name":"Dry spell extend","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"days_since_rain","op":"gt","value":7.0}},"action":"extend"}"#,
    },
];

#[component]
pub fn ConditionsSection(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let config = RwSignal::new(serde_json::Value::Null);
    // None = list view; Some(idx) = editing rules[idx]; usize::MAX = new.
    let editing: RwSignal<Option<usize>> = RwSignal::new(None);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/config").send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    config.set(v);
                }
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = config;

    let rules_view = move || {
        let cfg = config.get();
        let rules = cfg
            .get("conditions")
            .and_then(|c| c.get("rules"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if rules.is_empty() {
            return view! {
                <p class="sensors-section__hint">"No custom rules yet. Add one to skip / extend / scale watering on conditions you choose."</p>
            }.into_any();
        }
        rules
            .into_iter()
            .enumerate()
            .map(|(idx, r)| {
                let name = r.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .or_else(|| r.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                    .unwrap_or_else(|| "rule".to_string());
                let enabled = r.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                let summary = rule_summary(&r);
                let mutate_save = move |f: &dyn Fn(&mut Vec<serde_json::Value>)| {
                    config.update(|cfg| {
                        if let Some(arr) = cfg.get_mut("conditions").and_then(|c| c.get_mut("rules")).and_then(|v| v.as_array_mut()) {
                            f(arr);
                        }
                    });
                    let candidate = config.get_untracked();
                    #[cfg(feature = "hydrate")]
                    leptos::task::spawn_local(async move {
                        if let Err(e) = save_config(candidate).await {
                            crate::components::ui::use_toast().error(format!("Rule save failed: {e}"));
                        }
                    });
                    #[cfg(not(feature = "hydrate"))]
                    let _ = candidate;
                };
                let del = move |_| {
                    #[cfg(feature = "hydrate")]
                    {
                        let ok = web_sys::window()
                            .and_then(|w| w.confirm_with_message("Delete this rule? This takes effect on the next decision.").ok())
                            .unwrap_or(false);
                        if !ok { return; }
                    }
                    mutate_save(&|arr| { if idx < arr.len() { arr.remove(idx); } });
                };
                let toggle = move |_| {
                    mutate_save(&|arr| {
                        if let Some(r) = arr.get_mut(idx) {
                            let cur = r.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                            r["enabled"] = serde_json::Value::Bool(!cur);
                        }
                    });
                };
                let up = move |_| { mutate_save(&|arr| { if idx > 0 && idx < arr.len() { arr.swap(idx, idx - 1); } }); };
                let down = move |_| { mutate_save(&|arr| { if idx + 1 < arr.len() { arr.swap(idx, idx + 1); } }); };
                view! {
                    <li class="cond-row" class:cond-row--off=!enabled>
                        <div class="cond-row__order">
                            <button type="button" class="cond-row__arrow" aria-label="Move rule earlier" title="Evaluated sooner" on:click=up disabled=move || idx == 0>{"\u{25B2}"}</button>
                            <button type="button" class="cond-row__arrow" aria-label="Move rule later" title="Evaluated later" on:click=down>{"\u{25BC}"}</button>
                        </div>
                        <span class="cond-row__dot" class:is-off=!enabled></span>
                        <div class="cond-row__text">
                            <span class="cond-row__name">{name}</span>
                            <span class="cond-row__sum">{summary}</span>
                        </div>
                        <button
                            type="button"
                            class="toggle-pill"
                            role="switch"
                            aria-checked=enabled.to_string()
                            on:click=toggle
                        >
                            <span class="toggle-pill__opt toggle-pill__opt--on" class:is-active=enabled>"On"</span>
                            <span class="toggle-pill__opt toggle-pill__opt--off" class:is-active=!enabled>"Off"</span>
                        </button>
                        <Button variant="ghost" on_click=Callback::new(move |_| editing.set(Some(idx)))>"Edit"</Button>
                        <Button variant="danger" on_click=Callback::new(del)>"Delete"</Button>
                    </li>
                }
            })
            .collect_view()
            .into_any()
    };

    view! {
        <section class="rulelab-conditions">
            <div class="rulelab-conditions__head">
                <h2 class="rulelab__section-title">"Your watering rules"</h2>
                <Button variant="primary"
                    on_click=Callback::new(move |_| editing.set(Some(usize::MAX)))>"+ New rule"</Button>
            </div>
            <p class="sensors-section__hint">
                "Structured triggers, augment-only: a rule can add a skip, extend, or scale a zone's run; it can never override a safety gate (freeze, wind, restriction, rain). Rules run top to bottom and the first skip wins, so order them by priority with the arrows."
            </p>
            <ul class="cond-list">{rules_view}</ul>

            <details class="rule-templates">
                <summary class="rule-templates__summary">"Template farm: proven rules, one click to make live"</summary>
                <div class="rule-templates__grid">
                    {RULE_TEMPLATES.iter().map(|t| {
                        let tpl = *t;
                        let add = move |_| {
                            let rule: serde_json::Value = serde_json::from_str(tpl.json).expect("template json");
                            config.update(|cfg| {
                                let conditions = cfg
                                    .as_object_mut()
                                    .map(|o| o.entry("conditions").or_insert(serde_json::json!({"rules": []})));
                                if let Some(c) = conditions {
                                    let arr = c
                                        .as_object_mut()
                                        .map(|o| o.entry("rules").or_insert(serde_json::json!([])));
                                    if let Some(serde_json::Value::Array(arr)) = arr {
                                        let mut r = rule.clone();
                                        // Unique id per instantiation.
                                        let base = r.get("id").and_then(|v| v.as_str()).unwrap_or("rule").to_string();
                                        let n = arr.len();
                                        r["id"] = serde_json::Value::String(format!("{base}_{n}"));
                                        arr.push(r);
                                    }
                                }
                            });
                            let candidate = config.get_untracked();
                            #[cfg(feature = "hydrate")]
                            leptos::task::spawn_local(async move {
                                match save_config(candidate).await {
                                    Ok(()) => crate::components::ui::use_toast().success("Rule added and live. Tune it with Edit."),
                                    Err(e) => crate::components::ui::use_toast().error(format!("Add failed: {e}")),
                                }
                            });
                            #[cfg(not(feature = "hydrate"))]
                            let _ = candidate;
                        };
                        view! {
                            <div class="rule-template">
                                <div class="rule-template__text">
                                    <span class="rule-template__name">{t.name}</span>
                                    <span class="rule-template__desc">{t.desc}</span>
                                </div>
                                <Button variant="primary" on_click=Callback::new(add)>"Add"</Button>
                            </div>
                        }
                    }).collect_view()}
                </div>
            </details>

            {move || editing.get().map(|idx| {
                let existing = if idx == usize::MAX {
                    None
                } else {
                    config.get().get("conditions").and_then(|c| c.get("rules"))
                        .and_then(|v| v.as_array()).and_then(|a| a.get(idx).cloned())
                };
                view! {
                    <ConditionRuleEditor
                        snap=snap
                        config=config
                        idx=idx
                        existing=existing
                        on_done=Callback::new(move |()| editing.set(None))
                    />
                }
            })}
        </section>
    }
}

#[cfg(feature = "hydrate")]
pub(crate) async fn save_config(cfg: serde_json::Value) -> Result<(), String> {
    use gloo_net::http::Request;
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

#[component]
fn ConditionRuleEditor(
    snap: ReadSignal<IrrigationSnapshot>,
    config: RwSignal<serde_json::Value>,
    idx: usize,
    existing: Option<serde_json::Value>,
    on_done: Callback<()>,
) -> impl IntoView {
    // Seed from existing or sensible defaults.
    let seed_id = existing
        .as_ref()
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let seed_name = existing
        .as_ref()
        .and_then(|r| r.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let seed_enabled = existing
        .as_ref()
        .and_then(|r| r.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // Match mode + rows.
    let cond = existing.as_ref().and_then(|r| r.get("condition"));
    let (seed_mode, seed_rows) =
        if let Some(arr) = cond.and_then(|c| c.get("any")).and_then(|v| v.as_array()) {
            ("any", arr.clone())
        } else {
            (
                "all",
                cond.and_then(|c| c.get("all"))
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default(),
            )
        };
    let rows_seed: Vec<Row> = seed_rows
        .iter()
        .filter_map(|r| {
            let c = r.get("compare")?;
            Some(Row {
                metric: c.get("metric")?.as_str()?.to_string(),
                op: c.get("op")?.as_str()?.to_string(),
                value: c.get("value")?.as_f64()?,
            })
        })
        .collect();
    let rows_seed = if rows_seed.is_empty() {
        vec![Row {
            metric: "zone_soil_pct".into(),
            op: "gte".into(),
            value: 65.0,
        }]
    } else {
        rows_seed
    };

    // Action seed.
    let (seed_action, seed_factor) = match existing.as_ref().map(|r| r.get("action")) {
        Some(Some(serde_json::Value::String(s))) if s == "extend" => ("extend", 1.0),
        Some(Some(v)) if v.get("adjust_multiplier").is_some() => (
            "adjust",
            v.get("adjust_multiplier")
                .and_then(|a| a.get("factor"))
                .and_then(|x| x.as_f64())
                .unwrap_or(0.8),
        ),
        _ => ("skip", 1.0),
    };
    // Scope seed.
    let scope = existing.as_ref().and_then(|r| r.get("scope"));
    let (seed_scope, seed_zones) = match scope {
        Some(v) if v.get("zones").is_some() => (
            "zones",
            v.get("zones")
                .and_then(|z| z.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default(),
        ),
        _ => ("all_zones", String::new()),
    };

    let id = RwSignal::new(seed_id);
    let name = RwSignal::new(seed_name);
    let enabled = RwSignal::new(seed_enabled);
    let mode = RwSignal::new(seed_mode.to_string());
    let rows = RwSignal::new(rows_seed);
    let action = RwSignal::new(seed_action.to_string());
    let factor = RwSignal::new(seed_factor);
    let scope_mode = RwSignal::new(seed_scope.to_string());
    let scope_zones = RwSignal::new(seed_zones);
    let error = RwSignal::new(String::new());

    let on_save = move |_| {
        let mut rid = id.get().trim().to_string();
        if rid.is_empty() {
            // Derive a slug from the name for new rules.
            rid = name
                .get()
                .trim()
                .to_lowercase()
                .replace(|c: char| !c.is_alphanumeric(), "_");
            if rid.is_empty() {
                error.set("Give the rule a name.".into());
                return;
            }
        }
        let compares: Vec<serde_json::Value> = rows
            .get()
            .iter()
            .map(|r| {
                serde_json::json!({"compare": {"metric": r.metric, "op": r.op, "value": r.value}})
            })
            .collect();
        let condition = if mode.get() == "any" {
            serde_json::json!({ "any": compares })
        } else {
            serde_json::json!({ "all": compares })
        };
        let action_json = match action.get().as_str() {
            "extend" => serde_json::json!("extend"),
            "adjust" => {
                serde_json::json!({ "adjust_multiplier": { "factor": factor.get() } })
            }
            _ => serde_json::json!("skip"),
        };
        let scope_json = if scope_mode.get() == "zones" {
            let zs: Vec<String> = scope_zones
                .get()
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            serde_json::json!({ "zones": zs })
        } else {
            serde_json::json!("all_zones")
        };
        let entry = serde_json::json!({
            "id": rid,
            "name": name.get(),
            "enabled": enabled.get(),
            "scope": scope_json,
            "condition": condition,
            "action": action_json,
        });
        config.update(|cfg| {
            if !cfg.is_object() {
                *cfg = serde_json::json!({});
            }
            // Ensure conditions.rules exists.
            let obj = cfg.as_object_mut().unwrap();
            let conditions = obj
                .entry("conditions")
                .or_insert(serde_json::json!({"rules": []}));
            if conditions.get("rules").is_none() {
                conditions
                    .as_object_mut()
                    .unwrap()
                    .insert("rules".into(), serde_json::json!([]));
            }
            if let Some(arr) = conditions.get_mut("rules").and_then(|v| v.as_array_mut()) {
                if idx == usize::MAX || idx >= arr.len() {
                    arr.push(entry);
                } else {
                    arr[idx] = entry;
                }
            }
        });
        let candidate = config.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_config(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
        on_done.run(());
    };

    // Live "would fire now?", evaluate the flat rows against skip_check.
    let would_fire = move || {
        let s = snap.get();
        let sc = &s.skip_check;
        let rs = rows.get();
        let mut evaluable = false;
        let results: Vec<bool> = rs
            .iter()
            .filter_map(|r| {
                metric_live(&r.metric, sc).map(|v| {
                    evaluable = true;
                    op_apply(&r.op, v, r.value)
                })
            })
            .collect();
        if !evaluable {
            return None; // only zone_soil_pct rows -> per-zone, not previewable
        }
        Some(if mode.get_untracked() == "any" {
            results.iter().any(|b| *b)
        } else {
            results.iter().all(|b| *b)
        })
    };

    view! {
        <div class="cond-editor">
            <h3 class="source-editor__title">{if idx == usize::MAX { "New rule" } else { "Edit rule" }}</h3>
            <label class="cond-editor__field">
                <span>"Name"</span>
                <input type="text" class="ui-input" placeholder="e.g. Skip soggy front yard"
                    prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev))/>
            </label>
            <label class="cond-editor__check">
                <input type="checkbox" prop:checked=move || enabled.get() on:input=move |ev| enabled.set(event_target_checked(&ev))/>
                "Enabled"
            </label>

            <div class="cond-editor__match">
                <span>"Match"</span>
                <select class="ui-input ui-input--inline" on:change=move |ev| mode.set(event_target_value(&ev))>
                    <option value="all" selected=move || mode.get() == "all">"ALL of"</option>
                    <option value="any" selected=move || mode.get() == "any">"ANY of"</option>
                </select>
                <span>"these conditions:"</span>
            </div>

            <div class="cond-rows">
                {move || {
                    let rs = rows.get();
                    rs.into_iter().enumerate().map(|(i, row)| {
                        let m = row.metric.clone();
                        let o = row.op.clone();
                        let v = row.value;
                        let set_metric = move |ev: leptos::ev::Event| { let nv = event_target_value(&ev); rows.update(|r| if i < r.len() { r[i].metric = nv.clone(); }); };
                        let set_op = move |ev: leptos::ev::Event| { let nv = event_target_value(&ev); rows.update(|r| if i < r.len() { r[i].op = nv.clone(); }); };
                        let set_val = move |ev: leptos::ev::Event| { if let Ok(nv) = event_target_value(&ev).parse::<f64>() { rows.update(|r| if i < r.len() { r[i].value = nv; }); } };
                        let remove = move |_| { rows.update(|r| if r.len() > 1 && i < r.len() { r.remove(i); }); };
                        view! {
                            <div class="cond-rows__row">
                                <select class="ui-input ui-input--inline" on:change=set_metric>
                                    {METRICS.iter().map(|(val,label,_)| {
                                        let val = val.to_string(); let sel = val == m;
                                        view!{<option value=val.clone() selected=sel>{label.to_string()}</option>}
                                    }).collect_view()}
                                </select>
                                <select class="ui-input ui-input--inline cond-rows__op" on:change=set_op>
                                    {OPS.iter().map(|(sym,serde)| {
                                        let serde = serde.to_string(); let sel = serde == o;
                                        view!{<option value=serde.clone() selected=sel>{sym.to_string()}</option>}
                                    }).collect_view()}
                                </select>
                                <input type="number" class="ui-input ui-input--inline cond-rows__val" step="0.1"
                                    prop:value=move || v.to_string() on:input=set_val/>
                                <button type="button" class="cond-rows__del" on:click=remove aria-label="Remove condition">"×"</button>
                            </div>
                        }
                    }).collect_view()
                }}
                <button type="button" class="setup-footer__btn setup-footer__btn--ghost"
                    on:click=move |_| rows.update(|r| r.push(Row{metric:"rain_prob_tomorrow".into(), op:"gt".into(), value:60.0}))>
                    "+ Add condition"
                </button>
            </div>

            <div class="cond-editor__match">
                <span>"Then"</span>
                <select class="ui-input ui-input--inline" on:change=move |ev| action.set(event_target_value(&ev))>
                    <option value="skip" selected=move || action.get() == "skip">"skip the zone"</option>
                    <option value="extend" selected=move || action.get() == "extend">"extend the run"</option>
                    <option value="adjust" selected=move || action.get() == "adjust">"scale the run"</option>
                </select>
                {move || (action.get() == "adjust").then(|| view! {
                    <input type="number" class="ui-input ui-input--inline cond-rows__val" min="0.5" max="1.5" step="0.05"
                        prop:value=move || factor.get().to_string()
                        on:input=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<f64>() { factor.set(v); } }/>
                })}
            </div>

            <div class="cond-editor__match">
                <span>"Applies to"</span>
                <select class="ui-input ui-input--inline" on:change=move |ev| scope_mode.set(event_target_value(&ev))>
                    <option value="all_zones" selected=move || scope_mode.get() == "all_zones">"all zones"</option>
                    <option value="zones" selected=move || scope_mode.get() == "zones">"specific zones"</option>
                </select>
                {move || (scope_mode.get() == "zones").then(|| view! {
                    <input type="text" class="ui-input ui-input--inline" placeholder="front_yard, side_yard"
                        prop:value=move || scope_zones.get() on:input=move |ev| scope_zones.set(event_target_value(&ev))/>
                })}
            </div>

            <div class="cond-editor__preview">
                {move || match would_fire() {
                    Some(true) => view! { <span class="cond-fire cond-fire--yes">"Would fire now"</span> }.into_any(),
                    Some(false) => view! { <span class="cond-fire cond-fire--no">"Would not fire now"</span> }.into_any(),
                    None => view! { <span class="cond-fire">"Per-zone, evaluated live per zone"</span> }.into_any(),
                }}
            </div>

            {move || { let e = error.get(); (!e.is_empty()).then(|| view! { <p class="source-editor__error">{e}</p> }) }}

            <div class="settings-form-actions">
                <Button variant="ghost" on_click=Callback::new(move |_| on_done.run(()))>"Cancel"</Button>
                <Button variant="primary" on_click=Callback::new(on_save)>"Save rule"</Button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod template_tests {
    use super::RULE_TEMPLATES;
    use crate::engine::conditions::ConditionRule;

    #[test]
    fn every_template_deserializes_into_a_real_rule() {
        for t in RULE_TEMPLATES {
            let r: ConditionRule = serde_json::from_str(t.json)
                .unwrap_or_else(|e| panic!("template '{}' invalid: {e}", t.name));
            assert!(r.enabled, "{} should instantiate enabled", t.name);
        }
    }
}
