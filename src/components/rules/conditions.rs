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

use crate::components::ui::{Button, ConfirmSheet};
use crate::model::IrrigationSnapshot;

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

/// Only represent expressions this editor can round-trip without dropping terms.
fn simple_conditions(
    condition: Option<&serde_json::Value>,
) -> Option<(&'static str, Vec<serde_json::Value>)> {
    let condition = condition?;
    let (mode, rows) = if condition.get("compare").is_some() {
        ("all", vec![condition.clone()])
    } else if let Some(rows) = condition.get("all").and_then(|v| v.as_array()) {
        ("all", rows.clone())
    } else {
        ("any", condition.get("any")?.as_array()?.clone())
    };
    if rows.is_empty()
        || rows.iter().any(|row| {
            let Some(c) = row.get("compare") else {
                return true;
            };
            !METRICS
                .iter()
                .any(|(metric, _, _)| Some(*metric) == c.get("metric").and_then(|v| v.as_str()))
                || !OPS
                    .iter()
                    .any(|(_, op)| Some(*op) == c.get("op").and_then(|v| v.as_str()))
                || c.get("value").and_then(|v| v.as_f64()).is_none()
        })
    {
        return None;
    }
    Some((mode, rows))
}

fn rule_list(config: &serde_json::Value) -> Vec<serde_json::Value> {
    config
        .pointer("/conditions/rules")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

#[cfg(feature = "hydrate")]
async fn persist_rules(
    before: Vec<serde_json::Value>,
    next: Vec<serde_json::Value>,
) -> Result<
    (
        serde_json::Value,
        crate::components::config_client::SaveOutcome,
    ),
    String,
> {
    let mut fresh = crate::components::config_client::get_config().await?;
    if rule_list(&fresh) != before {
        return Err("Rules changed elsewhere. Reload them before saving.".into());
    }
    fresh["conditions"]["rules"] = serde_json::json!(next);
    let result = crate::components::config_client::put_config(&fresh).await?;
    // Keep the server's normalized representation for the next conflict check
    // (including numeric/default-field normalization), not the submitted JSON.
    let saved = crate::components::config_client::get_config()
        .await
        .map_err(|e| format!("Current rules could not be reloaded after saving. {e}"))?;
    Ok((saved, result))
}

/// Human one-liner for a stored rule's condition + action (list view).
fn rule_summary(rule: &serde_json::Value) -> String {
    let Some((joiner, rows)) = simple_conditions(rule.get("condition")) else {
        return "Nested condition: edit in the configuration file".to_string();
    };
    let joiner = if joiner == "any" { " OR " } else { " AND " };
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
        desc: "Skip when today's rain exceeds 0.5 inches.",
        json: r#"{"id":"skip_heavy_rain","name":"Skip after heavy rain","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"rain_today_in","op":"gt","value":0.5}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Skip cold mornings",
        desc: "Skip when the current temperature is below 45 °F.",
        json: r#"{"id":"skip_cold_morning","name":"Skip cold mornings","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"temp_now_f","op":"lt","value":45.0}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Windy morning guard",
        desc: "Skip when current wind exceeds 12 mph.",
        json: r#"{"id":"skip_windy","name":"Windy morning guard","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"wind_now_mph","op":"gt","value":12.0}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Soil already comfortable",
        desc: "Skip when the zone's soil probe reads above 70%.",
        json: r#"{"id":"skip_soil_wet","name":"Soil already comfortable","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"zone_soil_pct","op":"gt","value":70.0}},"action":"skip"}"#,
    },
    RuleTemplate {
        name: "Heat wave boost",
        desc: "Add 25% when the three-day forecast high exceeds 95 °F.",
        json: r#"{"id":"heat_boost","name":"Heat wave boost","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"temp_max3day_f","op":"gt","value":95.0}},"action":{"adjust_multiplier":{"factor":1.25}}}"#,
    },
    RuleTemplate {
        name: "Dry spell extend",
        desc: "Extend watering after more than seven days without meaningful rain.",
        json: r#"{"id":"dry_spell","name":"Dry spell extend","enabled":true,"scope":"all_zones","condition":{"compare":{"metric":"days_since_rain","op":"gt","value":7.0}},"action":"extend"}"#,
    },
];

#[component]
pub fn ConditionsSection(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let config = RwSignal::new(serde_json::Value::Null);
    let loading = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let retry = RwSignal::new(0_u32);
    // None = list view; Some(idx) = editing rules[idx]; usize::MAX = new.
    let editing: RwSignal<Option<usize>> = RwSignal::new(None);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        retry.get();
        loading.set(true);
        error.set(None);
        leptos::task::spawn_local(async move {
            match crate::components::config_client::get_config().await {
                Ok(v) => config.set(v),
                Err(e) => error.set(Some(format!("Rules could not be loaded. {e}"))),
            }
            loading.set(false);
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = config;

    // One read-modify-write for every row mutation: apply `f` to the
    // stored rules array, then PUT the whole config. Lives on the page,
    // not the row, so the delete confirmation can reach it too.
    let mutate_save = move |f: &dyn Fn(&mut Vec<serde_json::Value>)| {
        if busy.get_untracked() || loading.get_untracked() || config.get_untracked().is_null() {
            return;
        }
        let before = rule_list(&config.get_untracked());
        let mut next = before.clone();
        f(&mut next);
        #[cfg(feature = "hydrate")]
        {
            busy.set(true);
            error.set(None);
            let toast = crate::components::ui::use_toast();
            leptos::task::spawn_local(async move {
                match persist_rules(before, next).await {
                    Ok((saved, outcome)) => {
                        config.set(saved);
                        toast.success(outcome.save_confirmation());
                    }
                    Err(e) => {
                        if let Ok(current) = crate::components::config_client::get_config().await {
                            config.set(current);
                        }
                        error.set(Some(format!("Could not confirm the rule change. {e}")));
                    }
                }
                busy.set(false);
            });
        }
    };

    // Delete asks through the shared ConfirmSheet instead of a native
    // confirm(): the row stages its index and opens the sheet, the sheet's
    // on_confirm removes the rule once the sheet has closed.
    let pending_delete: RwSignal<Option<usize>> = RwSignal::new(None);
    let delete_open = RwSignal::new(false);
    let do_delete = Callback::new(move |()| {
        if let Some(i) = pending_delete.get_untracked() {
            mutate_save(&|arr| {
                if i < arr.len() {
                    arr.remove(i);
                }
            });
        }
        pending_delete.set(None);
    });

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
                <li class="sensors-section__hint">"No custom rules yet. Add a rule or start with an example below."</li>
            }.into_any();
        }
        let total = rules.len();
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
                let editable = simple_conditions(r.get("condition")).is_some();
                let del = move |_| {
                    pending_delete.set(Some(idx));
                    delete_open.set(true);
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
                    <li class="cond-row cond-row--rule" class:cond-row--off=!enabled>
                        <div class="cond-row__order">
                            <button type="button" class="cond-row__arrow" aria-label="Move rule earlier" title="Evaluated sooner" on:click=up disabled=move || idx == 0 || busy.get() || editing.get().is_some()>{"\u{25B2}"}</button>
                            <button type="button" class="cond-row__arrow" aria-label="Move rule later" title="Evaluated later" on:click=down disabled=move || idx + 1 == total || busy.get() || editing.get().is_some()>{"\u{25BC}"}</button>
                        </div>
                        <span class="cond-row__dot" class:is-off=!enabled></span>
                        <div class="cond-row__text">
                            <span class="cond-row__name">{name.clone()}</span>
                            <span class="cond-row__sum">{summary}</span>
                        </div>
                        <div class="cond-row__actions">
                        <button
                            type="button"
                            class="toggle-pill"
                            role="switch"
                            aria-label=format!("{} rule", name)
                            disabled=move || busy.get() || editing.get().is_some()
                            aria-checked=enabled.to_string()
                            on:click=toggle
                        >
                            <span class="toggle-pill__opt toggle-pill__opt--on" class:is-active=enabled>"On"</span>
                            <span class="toggle-pill__opt toggle-pill__opt--off" class:is-active=!enabled>"Off"</span>
                        </button>
                        <Button variant="ghost" disabled=Signal::derive(move || busy.get() || editing.get().is_some() || !editable) on_click=Callback::new(move |_| editing.set(Some(idx)))>"Edit"</Button>
                        <Button variant="danger" disabled=Signal::derive(move || busy.get() || editing.get().is_some()) on_click=Callback::new(del)>"Delete"</Button>
                        </div>
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
                <Button variant="primary" disabled=Signal::derive(move || busy.get() || loading.get() || config.get().is_null() || editing.get().is_some())
                    on_click=Callback::new(move |_| editing.set(Some(usize::MAX)))>"+ New rule"</Button>
            </div>
            <p class="sensors-section__hint">
                "A rule can add a skip, or extend or scale a run. It can never overrule a safety gate. They run top to bottom and the first skip wins."
            </p>
            <Show when=move || loading.get()><p role="status">"Loading custom rules…"</p></Show>
            <Show when=move || busy.get()><p role="status">"Saving rule…"</p></Show>
            <Show when=move || error.get().is_some()>
                <div class="rulelab-load-error" role="alert"><p>{move || error.get().unwrap_or_default()}</p>
                    <button type="button" class="btn btn--ghost" disabled=move || busy.get() || editing.get().is_some() || loading.get()
                        on:click=move |_| retry.update(|n| *n += 1)>"Reload custom rules"</button>
                </div>
            </Show>
            <Show when=move || !loading.get() && !config.get().is_null()><ul class="cond-list">{rules_view}</ul></Show>

            <details class="rule-templates">
                <summary class="rule-templates__summary">"Example rules"</summary>
                <div class="rule-templates__grid">
                    {RULE_TEMPLATES.iter().map(|t| {
                        let tpl = *t;
                        let add = move |_| {
                            let rule: serde_json::Value = serde_json::from_str(tpl.json).expect("template json");
                            mutate_save(&|arr| {
                                let mut r = rule.clone();
                                let base = r.get("id").and_then(|v| v.as_str()).unwrap_or("rule");
                                let mut suffix = arr.len();
                                while arr.iter().any(|v| v.get("id").and_then(|v| v.as_str()) == Some(format!("{base}_{suffix}").as_str())) { suffix += 1; }
                                r["id"] = serde_json::json!(format!("{base}_{suffix}"));
                                arr.push(r);
                            });
                        };
                        view! {
                            <div class="rule-template">
                                <div class="rule-template__text">
                                    <span class="rule-template__name">{t.name}</span>
                                    <span class="rule-template__desc">{t.desc}</span>
                                </div>
                                <Button variant="primary" disabled=Signal::derive(move || busy.get() || loading.get() || config.get().is_null() || editing.get().is_some()) on_click=Callback::new(add)>"Add"</Button>
                            </div>
                        }
                    }).collect_view()}
                </div>
            </details>

            {move || editing.get().map(|idx| {
                let existing = if idx == usize::MAX {
                    None
                } else {
                    config.get_untracked().get("conditions").and_then(|c| c.get("rules"))
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

            // Deleting a rule is destructive, so it asks first. Mounted
            // unconditionally, outside the row loop: the sheet hides itself.
            <ConfirmSheet
                visible=delete_open
                title="Delete this rule?"
                body=Signal::derive(|| "This takes effect on the next decision.".to_string())
                confirm_label=Signal::derive(|| "Delete".to_string())
                danger=true
                on_confirm=do_delete
            />
        </section>
    }
}

#[component]
fn ConditionRuleEditor(
    snap: ReadSignal<IrrigationSnapshot>,
    config: RwSignal<serde_json::Value>,
    idx: usize,
    existing: Option<serde_json::Value>,
    on_done: Callback<()>,
) -> impl IntoView {
    // Compare against the list the editor actually opened, not a later render.
    let original_rules = StoredValue::new(rule_list(&config.get_untracked()));
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
    let (seed_mode, seed_rows) = simple_conditions(cond).unwrap_or(("all", Vec::new()));
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
    let saving = RwSignal::new(false);

    let on_save = move |_| {
        if saving.get_untracked() {
            return;
        }
        error.set(String::new());
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
        let before = original_rules.get_value();
        if before
            .iter()
            .enumerate()
            .any(|(i, r)| i != idx && r.get("id") == entry.get("id"))
        {
            error.set("A rule with this name already exists. Choose another name.".into());
            return;
        }
        let mut next = before.clone();
        if idx != usize::MAX && idx >= next.len() {
            error.set("This rule changed. Close the editor and reload rules.".into());
            return;
        }
        if idx == usize::MAX {
            next.push(entry);
        } else {
            next[idx] = entry;
        }
        #[cfg(feature = "hydrate")]
        {
            saving.set(true);
            let toast = crate::components::ui::use_toast();
            leptos::task::spawn_local(async move {
                match persist_rules(before, next).await {
                    Ok((saved, outcome)) => {
                        config.set(saved);
                        toast.success(outcome.save_confirmation());
                        on_done.run(());
                    }
                    Err(e) => {
                        error.set(format!("Rule was not confirmed saved. {e}"));
                        saving.set(false);
                    }
                }
            });
        }
    };

    // Live "would fire now?", answered by the ENGINE's own evaluator.
    //
    // This used to resolve each metric and apply each operator here, a
    // second implementation of `engine::conditions` that had already lost
    // its three-valued logic: it returned a number for the 24-hour low
    // whether or not the forecast reported one, so a yard with no
    // overnight low previewed a freeze rule as firing while the live
    // evaluation held it Unknown. The evaluator is shared now, so the
    // preview runs the same code the morning check runs, retaining Unknown.
    // A missing soil term stays in the expression; a known true ANY term or
    // known false ALL term can still decide the result.
    let would_fire = move || {
        use crate::engine::conditions::{preview_expr, ConditionCtx, ConditionExpr};
        let s = snap.get();
        let rs = rows.get();
        let terms: Vec<ConditionExpr> = rs
            .iter()
            .filter_map(|r| {
                Some(ConditionExpr::Compare {
                    metric: serde_json::from_value(serde_json::json!(r.metric)).ok()?,
                    op: serde_json::from_value(serde_json::json!(r.op)).ok()?,
                    value: r.value,
                })
            })
            .collect();
        if terms.is_empty() || terms.len() != rs.len() {
            return None;
        }
        let inputs = crate::engine::skip_rules::inputs_from_skipcheck(&s.skip_check);
        // There is no yard-wide probe. Keep that evidence unknown.
        let zone = crate::engine::skip_rules::ZoneSoil {
            slug: String::new(),
            name: String::new(),
            pct: None,
            saturation_pct: 0.0,
            target_min_pct: 0.0,
            probe_configured: false,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: Default::default(),
        };
        let ctx = ConditionCtx {
            i: &inputs,
            zone: &zone,
        };
        let expr = if mode.get() == "any" {
            ConditionExpr::Any(terms)
        } else {
            ConditionExpr::All(terms)
        };
        preview_expr(&expr, &ctx)
    };

    view! {
        <div class="cond-editor">
            <h3 class="source-editor__title">{if idx == usize::MAX { "New rule" } else { "Edit rule" }}</h3>
            <fieldset class="cond-editor__fields" disabled=move || saving.get()>
            <legend class="sr-only">"Rule settings"</legend>
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
                <select aria-label="Match conditions" class="ui-input ui-input--inline" on:change=move |ev| mode.set(event_target_value(&ev))>
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
                                <select aria-label="Measurement" class="ui-input ui-input--inline" on:change=set_metric>
                                    {METRICS.iter().map(|(val,label,_)| {
                                        let val = val.to_string(); let sel = val == m;
                                        view!{<option value=val.clone() selected=sel>{label.to_string()}</option>}
                                    }).collect_view()}
                                </select>
                                <select aria-label="Comparison" class="ui-input ui-input--inline cond-rows__op" on:change=set_op>
                                    {OPS.iter().map(|(sym,serde)| {
                                        let serde = serde.to_string(); let sel = serde == o;
                                        view!{<option value=serde.clone() selected=sel>{sym.to_string()}</option>}
                                    }).collect_view()}
                                </select>
                                <input aria-label="Threshold" type="number" class="ui-input ui-input--inline cond-rows__val" step="0.1"
                                    prop:value=move || v.to_string() on:input=set_val/>
                                <button type="button" class="cond-rows__del" on:click=remove aria-label="Remove condition">"×"</button>
                            </div>
                        }
                    }).collect_view()
                }}
                <crate::components::ui::Button
    variant="ghost"
    size="md"
    on_click=Callback::new(move |_| rows.update(|r| r.push(Row{metric:"rain_prob_tomorrow".into(), op:"gt".into(), value:60.0})))
    class="setup-footer__btn setup-footer__btn--ghost">
                    "+ Add condition"
                </crate::components::ui::Button>
            </div>

            <div class="cond-editor__match">
                <span>"Then"</span>
                <select aria-label="Watering action" class="ui-input ui-input--inline" on:change=move |ev| action.set(event_target_value(&ev))>
                    <option value="skip" selected=move || action.get() == "skip">"skip the zone"</option>
                    <option value="extend" selected=move || action.get() == "extend">"extend the run"</option>
                    <option value="adjust" selected=move || action.get() == "adjust">"scale the run"</option>
                </select>
                {move || (action.get() == "adjust").then(|| view! {
                    <input aria-label="Run multiplier" type="number" class="ui-input ui-input--inline cond-rows__val" min="0.5" max="1.5" step="0.05"
                        prop:value=move || factor.get().to_string()
                        on:input=move |ev| { if let Ok(v) = event_target_value(&ev).parse::<f64>() { factor.set(v); } }/>
                })}
            </div>

            <div class="cond-editor__match">
                <span>"Applies to"</span>
                <select aria-label="Rule scope" class="ui-input ui-input--inline" on:change=move |ev| scope_mode.set(event_target_value(&ev))>
                    <option value="all_zones" selected=move || scope_mode.get() == "all_zones">"all zones"</option>
                    <option value="zones" selected=move || scope_mode.get() == "zones">"specific zones"</option>
                </select>
                {move || (scope_mode.get() == "zones").then(|| view! {
                    <input aria-label="Zone IDs" type="text" class="ui-input ui-input--inline" placeholder="front_yard, side_yard"
                        prop:value=move || scope_zones.get() on:input=move |ev| scope_zones.set(event_target_value(&ev))/>
                })}
            </div>

            <div class="cond-editor__preview">
                {move || match would_fire() {
                    Some(true) => view! { <span class="cond-fire cond-fire--yes">"Would fire now"</span> }.into_any(),
                    Some(false) => view! { <span class="cond-fire cond-fire--no">"Would not fire now"</span> }.into_any(),
                    None => view! { <span class="cond-fire">"Needs a zone reading or missing weather data"</span> }.into_any(),
                }}
            </div>

            </fieldset>
            {move || { let e = error.get(); (!e.is_empty()).then(|| view! { <p class="source-editor__error" role="alert">{e}</p> }) }}

            <div class="settings-form-actions">
                <Button variant="ghost" disabled=Signal::derive(move || saving.get()) on_click=Callback::new(move |_| on_done.run(()))>"Cancel"</Button>
                <Button variant="primary" loading=Signal::derive(move || saving.get()) on_click=Callback::new(on_save)>"Save rule"</Button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod template_tests {
    use super::{simple_conditions, RULE_TEMPLATES};
    use crate::engine::conditions::ConditionRule;

    #[test]
    fn every_template_deserializes_into_a_real_rule() {
        for t in RULE_TEMPLATES {
            let r: ConditionRule = serde_json::from_str(t.json)
                .unwrap_or_else(|e| panic!("template '{}' invalid: {e}", t.name));
            assert!(r.enabled, "{} should instantiate enabled", t.name);
        }
    }

    #[test]
    fn every_template_opens_with_its_actual_conditions() {
        for template in RULE_TEMPLATES {
            let original: serde_json::Value = serde_json::from_str(template.json).unwrap();
            let (mode, rows) = simple_conditions(original.get("condition")).unwrap();
            assert_eq!(mode, "all");
            assert_eq!(rows, vec![original["condition"].clone()]);
        }
    }

    #[test]
    fn nested_or_unsupported_conditions_cannot_be_silently_flattened() {
        let compare =
            serde_json::json!({"compare":{"metric":"wind_now_mph","op":"gt","value":12.0}});
        assert!(simple_conditions(Some(
            &serde_json::json!({"all":[compare.clone(),{"any":[compare.clone()]}]})
        ))
        .is_none());
        assert!(simple_conditions(Some(&serde_json::json!({"not":compare.clone()}))).is_none());
        assert!(simple_conditions(Some(&serde_json::json!({"any":[]}))).is_none());
        assert_eq!(
            simple_conditions(Some(&serde_json::json!({"any":[compare.clone()]}))),
            Some(("any", vec![compare]))
        );
    }
}

#[cfg(all(test, feature = "ssr"))]
mod editor_vocabulary_tests {
    use super::{METRICS, OPS};
    use crate::engine::conditions::{CmpOp, Metric};

    /// Every metric and operator this editor offers has to deserialize
    /// into the ENGINE's own enum, because the "would fire now" preview
    /// builds a real `ConditionExpr` and hands it to the engine's
    /// evaluator. A slug the engine does not know is dropped silently
    /// from the expression, which would preview a rule against fewer
    /// terms than it contains, and would save a rule the engine cannot
    /// evaluate at all.
    #[test]
    fn every_offered_metric_and_operator_is_one_the_engine_knows() {
        for (slug, label, _unit) in METRICS {
            let parsed: Result<Metric, _> = serde_json::from_value(serde_json::json!(slug));
            assert!(
                parsed.is_ok(),
                "the editor offers {slug} ({label}) but the engine has no such metric"
            );
        }
        for (symbol, slug) in OPS {
            let parsed: Result<CmpOp, _> = serde_json::from_value(serde_json::json!(slug));
            assert!(
                parsed.is_ok(),
                "the editor offers {symbol} as {slug} but the engine has no such operator"
            );
        }
    }

    /// The reverse direction: a metric the engine can evaluate but the
    /// editor never offers is a rule nobody can build in the UI. Kept as
    /// an explicit list so adding one to the engine surfaces here.
    #[test]
    fn the_editor_offers_every_metric_the_engine_evaluates() {
        for m in [
            Metric::RainProbTomorrow,
            Metric::RainNext4hIn,
            Metric::RainTodayIn,
            Metric::Rain3dayWeightedIn,
            Metric::WindNowMph,
            Metric::WindMaxTodayMph,
            Metric::TempNowF,
            Metric::TempMin24hF,
            Metric::TempMax3dayF,
            Metric::HumidityNowPct,
            Metric::DaysSinceRain,
            Metric::ZoneSoilPct,
        ] {
            let slug = serde_json::to_value(m).unwrap();
            let slug = slug.as_str().unwrap();
            assert!(
                METRICS.iter().any(|(s, _, _)| *s == slug),
                "the engine evaluates {slug} but the rule editor offers no way to pick it"
            );
        }
    }
}
