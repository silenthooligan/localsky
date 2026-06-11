// Rule Lab, the skip-ladder provenance view (marquee feature 2, first
// cut). Reads the structured DecisionTrace the refresher now attaches to
// the live IrrigationSnapshot and renders the ladder top-to-bottom: every
// rule, the data values it saw, and which one fired. The deciding rule is
// highlighted; rules after it are shown as "not reached" (first-match
// wins, exactly mirroring the engine).
//
// A "recent decisions" rail lets you click any past day to load the trace
// that was captured at decision time (persisted via M0007); "Today (live)"
// shows the running trace off the snapshot. Editable thresholds are the
// remaining follow-up.

pub mod conditions;

use chrono::{Local, TimeZone};
use leptos::prelude::*;

use crate::components::rules::conditions::ConditionsSection;
use crate::components::verdict::{verdict_label, verdict_token};
use crate::ha::snapshot::{DecisionTrace, IrrigationSnapshot, RuleEval};
use crate::history::types::DecisionRecord;

fn fmt_day(epoch: i64) -> String {
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|dt| dt.format("%a %b %-d").to_string())
        .unwrap_or_else(|| "-".into())
}

fn day_key(epoch: i64) -> String {
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// Collapse the raw decision log (the engine re-evaluates many times a day,
/// so a single day can hold dozens of identical entries) to one row per
/// calendar day: the latest decision that day, plus how many evaluations it
/// represents. Input is newest-first; output preserves that order.
fn group_by_day(decisions: Vec<DecisionRecord>) -> Vec<(DecisionRecord, usize)> {
    use std::collections::HashMap;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for d in &decisions {
        *counts.entry(day_key(d.epoch)).or_insert(0) += 1;
    }
    let mut seen: HashMap<String, ()> = HashMap::new();
    let mut out = Vec::new();
    for d in decisions {
        let k = day_key(d.epoch);
        if seen.insert(k.clone(), ()).is_none() {
            let n = *counts.get(&k).unwrap_or(&1);
            out.push((d, n));
        }
    }
    out
}

#[component]
pub fn RuleLabPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // Past decisions (newest first). None selected = show today's live trace.
    let decisions = RwSignal::new(Vec::<DecisionRecord>::new());
    let selected: RwSignal<Option<i64>> = RwSignal::new(None);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            leptos::task::spawn_local(async move {
                if let Ok(resp) =
                    gloo_net::http::Request::get("/api/v1/irrigation/decisions?days=30")
                        .send()
                        .await
                {
                    if let Ok(w) = resp.json::<crate::history::types::DecisionWindow>().await {
                        let mut d = w.decisions;
                        d.reverse(); // newest first
                        decisions.set(d);
                    }
                }
            });
        });
    }

    // Two tabs: Rules (configure, front and center) and Decisions (audit).
    let tab = RwSignal::new("rules");

    view! {
        <div class="rulelab-page">
            <header class="rulelab-page__header">
                <p class="rulelab-page__eyebrow">"Irrigation logic"</p>
                <h1 class="rulelab-page__title">"Rule Lab"</h1>
                <p class="rulelab-page__sub">
                    "Configure your watering rules, and see exactly why each day was decided."
                </p>
            </header>

            <div class="rulelab-tabs" role="tablist">
                <button type="button" class="rulelab-tab" class:is-active=move || tab.get() == "rules"
                    role="tab" on:click=move |_| tab.set("rules")>"Rules"</button>
                <button type="button" class="rulelab-tab" class:is-active=move || tab.get() == "decisions"
                    role="tab" on:click=move |_| tab.set("decisions")>"Decisions"</button>
            </div>

            {move || if tab.get() == "decisions" {
                view! {
                    <div class="rulelab-layout">
                        <aside class="rulelab-history" aria-label="Recent decisions">
                            <button
                                type="button"
                                class="rulelab-history__item"
                                class:is-active=move || selected.get().is_none()
                                on:click=move |_| selected.set(None)
                            >
                                <span class="rulelab-history__day">"Today"</span>
                                <span class="rulelab-history__reason">"Live decision"</span>
                            </button>
                            {move || {
                                group_by_day(decisions.get()).into_iter().map(|(d, n)| {
                                    let ep = d.epoch;
                                    let tok = verdict_token(&d.verdict);
                                    let lab = verdict_label(&d.verdict);
                                    let day = fmt_day(d.epoch);
                                    let reason = if d.reason.is_empty() { "All clear".to_string() } else { d.reason.clone() };
                                    view! {
                                        <button
                                            type="button"
                                            class="rulelab-history__item"
                                            class:is-active=move || selected.get() == Some(ep)
                                            on:click=move |_| selected.set(Some(ep))
                                        >
                                            <span class="rulelab-history__day">
                                                {day}
                                                {(n > 1).then(|| view! {
                                                    <span class="rulelab-history__count" title="evaluations that day">{n}" evals"</span>
                                                })}
                                            </span>
                                            <span class="rulelab-history__pill" style=format!("--v:{tok}")>{lab}</span>
                                            <span class="rulelab-history__reason">{reason}</span>
                                        </button>
                                    }
                                }).collect_view()
                            }}
                        </aside>

                        <div class="rulelab-main">
                            {move || {
                                match selected.get() {
                                    None => match snap.get().decision_trace {
                                        Some(trace) => view! { <TraceView trace/> }.into_any(),
                                        None => view! { <div class="rulelab-empty">"Waiting for the first decision of the day…"</div> }.into_any(),
                                    },
                                    Some(ep) => {
                                        let rec = decisions.get().into_iter().find(|d| d.epoch == ep);
                                        match rec.and_then(|d| d.trace) {
                                            Some(trace) => view! { <TraceView trace/> }.into_any(),
                                            None => view! { <div class="rulelab-empty">"No stored trace for this decision (recorded before trace capture)."</div> }.into_any(),
                                        }
                                    }
                                }
                            }}
                        </div>
                    </div>
                }.into_any()
            } else {
                view! {
                    <ConditionsSection snap=snap/>
                    <SafetyGates/>
                }.into_any()
            }}
        </div>
    }
}

/// The built-in gate ladder, shown under the custom-rule editor so users
/// understand what their rules layer on top of. Weather gates are operator
/// togglable via BuiltinGateManager; control and legal gates stay locked.
#[component]
fn SafetyGates() -> impl IntoView {
    view! {
        <details class="rulelab-gates" open>
            <summary>"Built-in skip rules, run before your rules"</summary>
            <div class="rulelab-gates__body">
                <p class="sensors-section__hint">
                    "These deterministic gates decide first, in this order. Each weather gate can be disabled if you know what you are doing; control and legal gates (override, pause, restrictions) are always on. Disabling is config, not code: a snapshot is kept and one click re-enables."
                </p>
                <a class="setup-footer__btn setup-footer__btn--primary rulelab-gates__cta" href="/settings/skip-rules">
                    "Configure thresholds (rain inches, wind mph, freeze temperature)"
                </a>
                <BuiltinGateManager/>
            </div>
        </details>
    }
}

#[component]
fn TraceView(trace: DecisionTrace) -> impl IntoView {
    let vtoken = verdict_token(&trace.verdict);
    let vlabel = verdict_label(&trace.verdict);
    let reason = if trace.reason.is_empty() {
        "All clear, no skip rule fired.".to_string()
    } else {
        trace.reason.clone()
    };
    view! {
        <div class="rulelab">
            <div class="rulelab-verdict" style=format!("--v:{vtoken}")>
                <span class="rulelab-verdict__pill">{vlabel}</span>
                <span class="rulelab-verdict__reason">{reason}</span>
            </div>
            <ol class="rulelab-ladder">
                {trace.rules.into_iter().map(|r| view! { <RuleRow r/> }).collect_view()}
            </ol>
        </div>
    }
}

#[component]
fn RuleRow(r: RuleEval) -> impl IntoView {
    let (badge_label, badge_class, accent) = match r.outcome.as_str() {
        "fired" => {
            let v = r.verdict.clone().unwrap_or_default();
            (
                verdict_label(&v).to_string(),
                "rule-row__badge rule-row__badge--fired",
                verdict_token(&v),
            )
        }
        "passed" => (
            "PASS".to_string(),
            "rule-row__badge rule-row__badge--passed",
            "var(--accent-good)",
        ),
        "skipped" => (
            "N/A".to_string(),
            "rule-row__badge rule-row__badge--skipped",
            "var(--text-faint)",
        ),
        _ => (
            "-".to_string(),
            "rule-row__badge rule-row__badge--skipped",
            "var(--text-faint)",
        ),
    };
    let row_class = if r.outcome == "fired" {
        "rule-row is-fired"
    } else if r.outcome == "not_reached" {
        "rule-row is-muted"
    } else {
        "rule-row"
    };
    let cat_attr = r.category.clone();
    view! {
        <li class=row_class style=format!("--accent-row:{accent}")>
            <span class="rule-row__cat" data-cat=cat_attr>{r.category}</span>
            <div class="rule-row__body">
                <span class="rule-row__label">{r.label}</span>
                <span class="rule-row__detail">{r.detail}</span>
            </div>
            <span class=badge_class>{badge_label}</span>
        </li>
    }
}

/// Operator control over the built-in ladder. Catalog comes from the
/// engine (id, label, what-disabling-means, protected); the disable set
/// lives at engine.skip_rules.disabled_rules. Disabling demands an
/// explicit acknowledgement that names the consequence.
#[component]
fn BuiltinGateManager() -> impl IntoView {
    let config = RwSignal::new(serde_json::Value::Null);
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

    let disabled_now = move || -> Vec<String> {
        config
            .get()
            .pointer("/engine/skip_rules/disabled_rules")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };

    let set_disabled = move |id: String, disable: bool, meaning: &'static str| {
        #[cfg(feature = "hydrate")]
        {
            if disable {
                let msg = format!(
                    "Disable the built-in '{id}' gate?\n\nWhat this means: {meaning}\n\nThe engine will no longer protect against this on its own. You can re-enable it here at any time."
                );
                let ok = web_sys::window()
                    .and_then(|w| w.confirm_with_message(&msg).ok())
                    .unwrap_or(false);
                if !ok {
                    return;
                }
            }
            config.update(|cfg| {
                let Some(sr) = cfg.pointer_mut("/engine/skip_rules") else {
                    return;
                };
                let arr = sr
                    .as_object_mut()
                    .map(|o| o.entry("disabled_rules").or_insert(serde_json::json!([])));
                if let Some(serde_json::Value::Array(arr)) = arr {
                    arr.retain(|x| x.as_str() != Some(id.as_str()));
                    if disable {
                        arr.push(serde_json::Value::String(id.clone()));
                    }
                }
            });
            let candidate = config.get_untracked();
            leptos::task::spawn_local(async move {
                match crate::components::rules::conditions::save_config(candidate).await {
                    Ok(()) => crate::components::ui::use_toast().success(if disable {
                        "Gate disabled. The trace will show it as disabled by operator."
                    } else {
                        "Gate re-enabled."
                    }),
                    Err(e) => crate::components::ui::use_toast().error(format!("Save failed: {e}")),
                }
            });
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = (id, disable, meaning);
    };

    view! {
        <div class="gate-list">
            {crate::gates_catalog::builtin_rule_catalog().iter().map(|(id, label, meaning, protected)| {
                let id_s = id.to_string();
                let on_click = {
                    let id_c = id_s.clone();
                    move |_| {
                        let currently_disabled = disabled_now().contains(&id_c);
                        set_disabled(id_c.clone(), !currently_disabled, meaning);
                    }
                };
                let id_chk = id_s.clone();
                view! {
                    <div class="gate-row" class:gate-row--off=move || disabled_now().contains(&id_chk)>
                        <div class="gate-row__text">
                            <span class="gate-row__label">{label.to_string()}</span>
                            <span class="gate-row__meaning">{meaning.to_string()}</span>
                        </div>
                        {if *protected {
                            view! { <span class="gate-row__lock" title="Control and legal gates stay on">"always on"</span> }.into_any()
                        } else {
                            let id_sw = id_s.clone();
                            let id_on = id_s.clone();
                            let id_off = id_s.clone();
                            view! {
                                <button
                                    type="button"
                                    class="toggle-pill"
                                    role="switch"
                                    aria-checked=move || (!disabled_now().contains(&id_sw)).to_string()
                                    on:click=on_click
                                >
                                    <span class="toggle-pill__opt toggle-pill__opt--on" class:is-active=move || !disabled_now().contains(&id_on)>"On"</span>
                                    <span class="toggle-pill__opt toggle-pill__opt--off" class:is-active=move || disabled_now().contains(&id_off)>"Off"</span>
                                </button>
                            }.into_any()
                        }}
                    </div>
                }
            }).collect_view()}
        </div>
    }
}
