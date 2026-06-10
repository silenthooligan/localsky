// Rule Lab — the skip-ladder provenance view (marquee feature 2, first
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
        .unwrap_or_else(|| "—".into())
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

    // Two tabs: Rules (configure — front and center) and Decisions (audit).
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
                    <SafetyGates snap=snap/>
                }.into_any()
            }}
        </div>
    }
}

/// Read-only reference of the built-in safety + weather gates (always on,
/// not removable). Shown under the custom-rule editor so users understand
/// what their rules layer on top of. Derived from the live decision trace.
#[component]
fn SafetyGates(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <details class="rulelab-gates" open>
            <summary>"Built-in skip rules — always on, run before your rules"</summary>
            <div class="rulelab-gates__body">
                <p class="sensors-section__hint">
                    "These deterministic gates (freeze, wind, watering restriction, rain, soil saturation) decide first and can't be disabled. Your custom rules above only run when these leave a zone watering — they can add a skip or scale a run, never override a gate."
                </p>
                <a class="setup-footer__btn setup-footer__btn--primary rulelab-gates__cta" href="/settings/skip-rules">
                    "Configure thresholds (rain inches, wind mph, freeze °F…)"
                </a>
                {move || {
                    match snap.get().decision_trace {
                        Some(t) => t.rules.into_iter()
                            .filter(|r| r.category != "condition" && r.category != "script")
                            .map(|r| view! {
                                <div class="rulelab-gates__row">
                                    <span class=format!("rulelab-gates__cat rulelab-gates__cat--{}", r.category)>{r.category.clone()}</span>
                                    <span class="rulelab-gates__label">{r.label.clone()}</span>
                                </div>
                            }).collect_view().into_any(),
                        None => view! { <p class="sensors-section__hint">"The ladder appears after the first decision of the day."</p> }.into_any(),
                    }
                }}
            </div>
        </details>
    }
}

#[component]
fn TraceView(trace: DecisionTrace) -> impl IntoView {
    let vtoken = verdict_token(&trace.verdict);
    let vlabel = verdict_label(&trace.verdict);
    let reason = if trace.reason.is_empty() {
        "All clear — no skip rule fired.".to_string()
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
            "—".to_string(),
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
