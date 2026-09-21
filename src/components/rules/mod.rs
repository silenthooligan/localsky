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
use leptos_router::hooks::{use_location, use_navigate};

use crate::components::rules::conditions::ConditionsSection;
use crate::components::ui::{Button, ConfirmSheet, HelpHint};
use crate::components::units_fmt::{use_unit_prefs, UnitPrefs};
use crate::components::verdict::{verdict_label, verdict_token};
use crate::history::types::DecisionRecord;
use crate::model::{DecisionTrace, IrrigationSnapshot, RuleEval};
use crate::reason_render::{render_rule_detail, render_rule_margin, render_trace_reason};

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
    // Per-device unit preference; the ladder's per-rule detail + margin re-render
    // unit-aware from the structured RuleEval (P2 units architecture).
    let prefs = use_unit_prefs();

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

    // Two tabs: Rules (configure, front and center) and Decisions (audit). The
    // active tab is URL state (?tab=decisions) not a bare signal, so the phone
    // back gesture switches tabs / leaves Rule Lab one step at a time instead of
    // jumping straight out, and a tab deep-links.
    let loc = use_location();
    let tab = Signal::derive(move || {
        if loc.search.get().contains("tab=decisions") {
            "decisions"
        } else {
            "rules"
        }
    });
    let nav_rules = use_navigate();
    let go_rules = move |_| nav_rules("/rules", Default::default());
    let nav_dec = use_navigate();
    let go_dec = move |_| nav_dec("/rules?tab=decisions", Default::default());

    view! {
        <div class="rulelab-page">
            <header class="page-head">
                <p class="page-eyebrow">"Irrigation logic"</p>
                <h1 class="page-title">"Rule Lab"<HelpHint topic="skip-rules"/></h1>
                <p class="rulelab-page__sub">
                    "Configure your watering rules, and see exactly why each day was decided."
                </p>
            </header>

            <div class="rulelab-tabs" role="tablist">
                <button type="button" class="rulelab-tab" class:is-active=move || tab.get() == "rules"
                    role="tab" on:click=go_rules>"Rules"</button>
                <button type="button" class="rulelab-tab" class:is-active=move || tab.get() == "decisions"
                    role="tab" on:click=go_dec>"Decisions"</button>
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
                                        Some(trace) => view! { <TraceView trace prefs/> }.into_any(),
                                        None => view! { <div class="rulelab-empty">"Waiting for the first decision of the day…"</div> }.into_any(),
                                    },
                                    Some(ep) => {
                                        let rec = decisions.get().into_iter().find(|d| d.epoch == ep);
                                        match rec.and_then(|d| d.trace) {
                                            Some(trace) => view! { <TraceView trace prefs/> }.into_any(),
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
                    "These gates decide first, in this order. A weather gate can be turned off; control and legal gates cannot. Turning one off is reversible in a click."
                </p>
                <Button variant="primary" href="/settings/skip-rules" class="rulelab-gates__cta">
                    "Configure thresholds (rain inches, wind mph, freeze temperature)"
                </Button>
                <BuiltinGateManager/>
            </div>
        </details>
    }
}

#[component]
fn TraceView(trace: DecisionTrace, prefs: Signal<UnitPrefs>) -> impl IntoView {
    let vtoken = verdict_token(&trace.verdict);
    let vlabel = verdict_label(&trace.verdict);
    // P2 units architecture: re-render the trace's top-level reason unit-aware
    // from the deciding rule's structured operands; fall back to the baked reason
    // for codes whose operands aren't carried (control gates, etc.).
    let reason = if trace.reason.is_empty() {
        "All clear, no skip rule fired.".to_string()
    } else {
        render_trace_reason(&trace, prefs.get_untracked())
    };
    view! {
        <div class="rulelab">
            <div class="rulelab-verdict" style=format!("--v:{vtoken}")>
                <span class="rulelab-verdict__eyebrow">"Today's decision"</span>
                <div class="rulelab-verdict__row">
                    <span class="rulelab-verdict__pill">{vlabel}</span>
                    {trace.degraded.then(|| view! {
                        <span class="ha-chip ha-chip--warn">
                            <span class="ha-chip__dot" aria-hidden="true"></span>
                            "ran on backup readings"
                        </span>
                    })}
                </div>
                <span class="rulelab-verdict__reason">{reason}</span>
            </div>
            <ol class="rulelab-ladder">
                {trace.rules.into_iter().map(|r| view! { <RuleRow r prefs/> }).collect_view()}
            </ol>
        </div>
    }
}

#[component]
fn RuleRow(r: RuleEval, prefs: Signal<UnitPrefs>) -> impl IntoView {
    // An overridden row keeps outcome "fired" -- the gate really did trip --
    // so it must be matched BEFORE the plain "fired" arm, or it renders as
    // the deciding rule it is not.
    let (badge_label, badge_class, accent) = if r.overridden() {
        (
            "OVERRIDDEN".to_string(),
            "rule-row__badge rule-row__badge--overridden",
            "var(--accent-warn)",
        )
    } else {
        match r.outcome.as_str() {
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
        }
    };
    let row_class = if r.overridden() {
        "rule-row is-overridden"
    } else if r.outcome == "fired" {
        "rule-row is-fired"
    } else if r.outcome == "not_reached" {
        "rule-row is-muted"
    } else {
        "rule-row"
    };
    let cat_attr = r.category.clone();
    // P2 units architecture: re-render the per-rule detail + margin unit-aware
    // from the structured RuleEval operands. Both read prefs.get() inside the
    // closures so a units toggle re-renders; threshold gates with no operands
    // fall back to the baked detail / margin_label inside the renderer.
    let has_margin = r.margin_label.is_some() || (r.value.is_some() && r.threshold.is_some());
    let r_detail = r.clone();
    let r_margin = r.clone();
    // Name what set this gate aside, so the row explains itself without the
    // reader having to compare value against threshold by hand.
    let overridden_note = r.overridden_detail.clone().or_else(|| {
        r.overridden_by
            .clone()
            .map(|by| format!("set aside by {by}"))
    });
    view! {
        <li class=row_class style=format!("--accent-row:{accent}")>
            <span class="rule-row__cat" data-cat=cat_attr>{r.category}</span>
            <div class="rule-row__body">
                <span class="rule-row__label">{r.label}</span>
                <span class="rule-row__detail">
                    {move || render_rule_detail(&r_detail, prefs.get())}
                </span>
                {has_margin.then(|| view! {
                    <span class="rule-row__margin" aria-label="margin" title="how close this gate was to flipping">
                        {move || render_rule_margin(&r_margin, prefs.get()).unwrap_or_default()}
                    </span>
                })}
                {overridden_note.map(|note| view! {
                    <span class="rule-row__overridden" aria-label="overridden">
                        {"This gate tripped and was set aside: "}{note}
                    </span>
                })}
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
            if let Ok(v) = crate::components::config_client::get_config().await {
                config.set(v);
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

    // Turning a gate off needs an explicit acknowledgement, so it stages
    // behind the shared ConfirmSheet instead of a native confirm(): the
    // row asks, the sheet's on_confirm writes. Re-enabling restores the
    // safe default and applies straight away. The gate the sheet is
    // about is parked here as (id, what-disabling-means); None = nothing
    // pending.
    let pending_gate: RwSignal<Option<(String, String)>> = RwSignal::new(None);
    let confirm_open = RwSignal::new(false);

    // The write itself, with no confirmation in it: the direct re-enable
    // and the sheet's on_confirm both land here.
    let set_disabled = move |id: String, disable: bool| {
        #[cfg(feature = "hydrate")]
        {
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
                match crate::components::config_client::put_config(&candidate).await {
                    Ok(_) => crate::components::ui::use_toast().success(if disable {
                        "Gate disabled. The trace will show it as disabled by operator."
                    } else {
                        "Gate re-enabled."
                    }),
                    Err(e) => crate::components::ui::use_toast().error(format!("Save failed: {e}")),
                }
            });
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = (id, disable);
    };

    let do_disable = Callback::new(move |()| {
        if let Some((id, _)) = pending_gate.get_untracked() {
            set_disabled(id, true);
        }
        pending_gate.set(None);
    });

    view! {
        <div class="gate-list">
            {crate::gates_catalog::builtin_rule_catalog().iter().map(|(id, label, meaning, protected)| {
                let id_s = id.to_string();
                let on_click = {
                    let id_c = id_s.clone();
                    let meaning_c = meaning.to_string();
                    move |_| {
                        let currently_disabled = disabled_now().contains(&id_c);
                        if currently_disabled {
                            // Re-enabling restores the safe default: no confirm.
                            set_disabled(id_c.clone(), false);
                        } else {
                            // Ask first; the sheet's on_confirm does the write.
                            pending_gate.set(Some((id_c.clone(), meaning_c.clone())));
                            confirm_open.set(true);
                        }
                    }
                };
                let id_chk = id_s.clone();
                view! {
                    <div
                        class="gate-row"
                        class:gate-row--protected=*protected
                        class:gate-row--off=move || disabled_now().contains(&id_chk)
                    >
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

            // Always mounted, outside the row map: a row's Off opens it,
            // it hides itself, and the disable writes from its on_confirm.
            <ConfirmSheet
                visible=confirm_open
                title=Signal::derive(move || match pending_gate.get() {
                    Some((id, _)) => format!("Disable the {id} gate?"),
                    None => "Disable this gate?".to_string(),
                })
                body=Signal::derive(move || {
                    let Some((_, meaning)) = pending_gate.get() else {
                        return String::new();
                    };
                    format!(
                        "{meaning} Watering will no longer be held for this on its \
                         own. You can re-enable it here at any time."
                    )
                })
                confirm_label=Signal::derive(|| "Disable gate".to_string())
                on_confirm=do_disable
            />
        </div>
    }
}
