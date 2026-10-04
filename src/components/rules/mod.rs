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
use crate::reason_render::{plain_watering_reason, render_rule_detail, render_trace_reason};

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
    let history_error = RwSignal::new(false);
    let history_loading = RwSignal::new(true);
    let history_retry = RwSignal::new(0_u32);
    // Per-device unit preference; the ladder's per-rule detail + margin re-render
    // unit-aware from the structured RuleEval (P2 units architecture).
    let prefs = use_unit_prefs();

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            history_retry.get();
            history_loading.set(true);
            history_error.set(false);
            leptos::task::spawn_local(async move {
                let loaded = async {
                    let resp = gloo_net::http::Request::get(&crate::base::url(
                        "/api/v1/irrigation/decisions?days=30",
                    ))
                    .send()
                    .await
                    .ok()?;
                    if !resp.ok() {
                        return None;
                    }
                    resp.json::<crate::history::types::DecisionWindow>()
                        .await
                        .ok()
                }
                .await;
                match loaded {
                    Some(w) => {
                        let mut d = w.decisions;
                        d.reverse(); // newest first
                        decisions.set(d);
                    }
                    None => history_error.set(true),
                }
                history_loading.set(false);
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
                    "Set watering rules and review how a decision was made."
                </p>
            </header>

            <div class="rulelab-tabs" role="group" aria-label="Rule Lab view">
                <button type="button" class="rulelab-tab" class:is-active=move || tab.get() == "rules"
                    aria-pressed=move || (tab.get() == "rules").to_string() on:click=go_rules>"Rules"</button>
                <button type="button" class="rulelab-tab" class:is-active=move || tab.get() == "decisions"
                    aria-pressed=move || (tab.get() == "decisions").to_string() on:click=go_dec>"Decisions"</button>
            </div>

            {move || if tab.get() == "decisions" {
                view! {
                    <div class="rulelab-layout">
                        <aside class="rulelab-history" aria-label="Recent decisions">
                            <button
                                type="button"
                                class="rulelab-history__item"
                                class:is-active=move || selected.get().is_none()
                                aria-pressed=move || selected.get().is_none().to_string()
                                on:click=move |_| selected.set(None)
                            >
                                <span class="rulelab-history__day">"Live decision"</span>
                                <span class="rulelab-history__reason">"Current conditions"</span>
                            </button>
                            <Show when=move || history_loading.get()><p role="status">"Loading recorded decisions…"</p></Show>
                            <Show when=move || history_error.get()>
                                <div class="rulelab-load-error" role="alert">
                                    <p>"Recorded decisions could not be loaded."</p>
                                    <button type="button" class="btn btn--ghost" on:click=move |_| history_retry.update(|n| *n += 1)>"Retry decisions"</button>
                                </div>
                            </Show>
                            <Show when=move || !history_loading.get() && !history_error.get() && decisions.get().is_empty()>
                                <p>"No recorded decisions in the last 30 days."</p>
                            </Show>
                            {move || {
                                group_by_day(decisions.get()).into_iter().map(|(d, n)| {
                                    let ep = d.epoch;
                                    let tok = verdict_token(&d.verdict);
                                    let lab = verdict_label(&d.verdict);
                                    let day = fmt_day(d.epoch);
                                    let reason = if d.reason.is_empty() { "No reason recorded".to_string() } else { plain_watering_reason(&d.reason) };
                                    view! {
                                        <button
                                            type="button"
                                            class="rulelab-history__item"
                                            class:is-active=move || selected.get() == Some(ep)
                                            aria-pressed=move || (selected.get() == Some(ep)).to_string()
                                            on:click=move |_| selected.set(Some(ep))
                                        >
                                            <span class="rulelab-history__day">
                                                {day}
                                                {(n > 1).then(|| view! {
                                                    <span class="rulelab-history__count" title="Decisions recorded that day">{n}" checks"</span>
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
                                        Some(trace) => view! { <TraceView trace prefs heading="Live decision".to_string()/> }.into_any(),
                                        None => view! { <div class="rulelab-empty">"No live decision is available yet."</div> }.into_any(),
                                    },
                                    Some(ep) => {
                                        let rec = decisions.get().into_iter().find(|d| d.epoch == ep);
                                        match rec.and_then(|d| d.trace) {
                                            Some(trace) => view! { <TraceView trace prefs heading=format!("Recorded decision · {}", fmt_day(ep))/> }.into_any(),
                                            None => view! { <div class="rulelab-empty">"Rule details were not recorded for this decision."</div> }.into_any(),
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
            <summary>"Built-in watering rules"</summary>
            <div class="rulelab-gates__body">
                <p class="sensors-section__hint">
                    "Protected rules stay on. Open Decisions to see which checks affected watering."
                </p>
                <Button variant="primary" href="/settings/skip-rules" class="rulelab-gates__cta">
                    "Edit weather thresholds"
                </Button>
                <BuiltinGateManager/>
            </div>
        </details>
    }
}

#[component]
fn TraceView(trace: DecisionTrace, prefs: Signal<UnitPrefs>, heading: String) -> impl IntoView {
    let vtoken = verdict_token(&trace.verdict);
    let vlabel = match trace.verdict.as_str() {
        "run" => "Watering allowed",
        "run_extended" => "Extended watering allowed",
        "skip" => "Watering skipped",
        _ => "Decision unavailable",
    };
    let deciding = trace
        .rules
        .iter()
        .enumerate()
        .find(|(_, r)| r.decided())
        .map(|(i, r)| {
            format!(
                "Check {} · {}",
                i + 1,
                r.label.replace("Hold all watering", "Skip all watering")
            )
        });
    let (checked, unchecked): (Vec<_>, Vec<_>) = trace
        .rules
        .iter()
        .cloned()
        .enumerate()
        .partition(|(_, r)| r.outcome != "not_reached");
    let unchecked_count = unchecked.len();
    // P2 units architecture: re-render the trace's top-level reason unit-aware
    // from the deciding rule's structured operands; fall back to the baked reason
    // for codes whose operands aren't carried (control gates, etc.).
    let reason_trace = trace.clone();
    view! {
        <div class="rulelab">
            <div class="rulelab-verdict" style=format!("--v:{vtoken}")>
                <span class="rulelab-verdict__eyebrow">{heading}</span>
                <div class="rulelab-verdict__row">
                    <span class="rulelab-verdict__pill">{vlabel}</span>
                    {trace.degraded.then(|| view! {
                        <span class="ha-chip ha-chip--warn">
                            <span class="ha-chip__dot" aria-hidden="true"></span>
                            "Limited weather data"
                        </span>
                    })}
                </div>
                <span class="rulelab-verdict__reason">{move || if reason_trace.reason.is_empty() {
                    if matches!(reason_trace.verdict.as_str(), "run" | "run_extended") {
                        "No rule blocks watering.".to_string()
                    } else { "No reason was recorded.".to_string() }
                } else { plain_watering_reason(&render_trace_reason(&reason_trace, prefs.get())) }}</span>
                {deciding.map(|label| view! { <span class="rulelab-verdict__source">{label}</span> })}
            </div>
            <section class="rulelab-path" aria-label="Decision path">
                <header class="rulelab-path__head">
                    <h2>"Decision path"</h2>
                    <p>"Checks in order. Green passed; the highlighted rule decided."</p>
                </header>
                <ol class="rulelab-ladder">
                    {checked.into_iter().map(|(i, r)| view! { <RuleRow r prefs step={i+1}/> }).collect_view()}
                </ol>
                { (unchecked_count > 0).then(|| view! {
                    <details class="rulelab-unchecked">
                        <summary>{format!("Not evaluated · {unchecked_count} {}", if unchecked_count == 1 { "check" } else { "checks" })}</summary>
                        <p>"These checks did not run. They did not affect this decision."</p>
                        <ol class="rulelab-ladder">
                            {unchecked.into_iter().map(|(i, r)| view! { <RuleRow r prefs step={i+1}/> }).collect_view()}
                        </ol>
                    </details>
                })}
            </section>
        </div>
    }
}

#[component]
fn RuleRow(r: RuleEval, prefs: Signal<UnitPrefs>, step: usize) -> impl IntoView {
    // An overridden row keeps outcome "fired" -- the gate really did trip --
    // so it must be matched BEFORE the plain "fired" arm, or it renders as
    // the deciding rule it is not.
    let (badge_label, badge_class, accent) = if r.overridden() {
        (
            "Overridden".to_string(),
            "rule-row__badge rule-row__badge--overridden",
            "var(--accent-warn)",
        )
    } else {
        match r.outcome.as_str() {
            "fired" => {
                let v = r.verdict.clone().unwrap_or_default();
                (
                    "Deciding rule".to_string(),
                    "rule-row__badge rule-row__badge--fired",
                    verdict_token(&v),
                )
            }
            "passed" => (
                "Passed".to_string(),
                "rule-row__badge rule-row__badge--passed",
                "var(--accent-good)",
            ),
            "skipped" => (
                if r.detail == "disabled by operator" {
                    "Off"
                } else {
                    "Not applicable"
                }
                .to_string(),
                "rule-row__badge rule-row__badge--skipped",
                "var(--text-faint)",
            ),
            "not_reached" => (
                "Not evaluated".to_string(),
                "rule-row__badge rule-row__badge--skipped",
                "var(--text-faint)",
            ),
            _ => (
                "Unknown".to_string(),
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
    // Keep numeric evidence unit-aware and clarify older control-rule wording.
    let show_detail =
        r.outcome != "not_reached" && !r.detail.is_empty() && r.detail != "disabled by operator";
    let r_detail = r.clone();
    // Name what set this gate aside, so the row explains itself without the
    // reader having to compare value against threshold by hand.
    let overridden_note = r.overridden_detail.clone().or_else(|| {
        r.overridden_by
            .clone()
            .map(|by| format!("set aside by {by}"))
    });
    view! {
        <li class=row_class value=step style=format!("--accent-row:{accent}")>
            <span class="rule-row__step" aria-label=format!("Check {step}")>{step}</span>
            <div class="rule-row__body">
                <div class="rule-row__head">
                    <span class="rule-row__label">{r.label.replace("Hold all watering", "Skip all watering")}</span>
                    <span class=badge_class>{badge_label}</span>
                </div>
                {show_detail.then(|| view! {
                    <span class="rule-row__detail">
                        {move || readable_rule_detail(&r_detail, prefs.get())}
                    </span>
                })}
                {overridden_note.map(|note| view! {
                    <span class="rule-row__overridden" aria-label="overridden">
                        {"Did not decide: "}{plain_watering_reason(&note)}
                    </span>
                })}
            </div>
        </li>
    }
}

fn readable_rule_detail(rule: &RuleEval, prefs: UnitPrefs) -> String {
    let detail = render_rule_detail(rule, prefs);
    match (rule.id.as_str(), detail.as_str()) {
        ("paused", "paused = false") => "Vacation pause is off".into(),
        ("paused", "paused = true") => "Vacation pause is on".into(),
        ("pause_until", "no timed pause set") => "No timed pause is set".into(),
        ("restart_required", "startup configuration is active") => {
            "Current settings are active".into()
        }
        ("override", "no global override; tomorrow override only applies to the tomorrow cell") => {
            "No manual override applies to this decision".into()
        }
        _ => plain_watering_reason(&detail),
    }
}

/// Operator control over the built-in ladder. Catalog comes from the
/// engine (id, label, what-disabling-means, protected); the disable set
/// lives at engine.skip_rules.disabled_rules. Disabling demands an
/// explicit acknowledgement that names the consequence.
#[component]
fn BuiltinGateManager() -> impl IntoView {
    let config = RwSignal::new(serde_json::Value::Null);
    let loading = RwSignal::new(true);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let retry = RwSignal::new(0_u32);
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
            if busy.get_untracked() || loading.get_untracked() || config.get_untracked().is_null() {
                return;
            }
            busy.set(true);
            error.set(None);
            let toast = crate::components::ui::use_toast();
            leptos::task::spawn_local(async move {
                // Read current configuration before changing this one field. Do
                // not display success or flip the switch before the save replies.
                let result = async {
                    let mut candidate = crate::components::config_client::get_config().await?;
                    let sr = candidate
                        .pointer_mut("/engine/skip_rules")
                        .and_then(|v| v.as_object_mut())
                        .ok_or_else(|| {
                            "Watering rules are unavailable in this configuration.".to_string()
                        })?;
                    let arr = sr
                        .entry("disabled_rules")
                        .or_insert(serde_json::json!([]))
                        .as_array_mut()
                        .ok_or_else(|| "The disabled-rule list could not be read.".to_string())?;
                    arr.retain(|x| x.as_str() != Some(id.as_str()));
                    if disable {
                        arr.push(serde_json::Value::String(id));
                    }
                    let outcome = crate::components::config_client::put_config(&candidate).await?;
                    config.set(candidate);
                    Ok::<_, String>(outcome)
                }
                .await;
                match result {
                    Ok(outcome) => toast.success(outcome.save_confirmation()),
                    Err(e) => {
                        // A response can fail after the server saved; reconcile
                        // with the server rather than inventing a rollback.
                        if let Ok(current) = crate::components::config_client::get_config().await {
                            config.set(current);
                        }
                        error.set(Some(format!("Could not confirm the rule change. {e}")));
                    }
                }
                busy.set(false);
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
            <Show when=move || loading.get()><p role="status">"Loading watering rules…"</p></Show>
            <Show when=move || busy.get()><p role="status">"Saving rule…"</p></Show>
            <Show when=move || error.get().is_some()>
                <div class="rulelab-load-error" role="alert">
                    <p>{move || error.get().unwrap_or_default()}</p>
                    <button type="button" class="btn btn--ghost" disabled=move || loading.get() || busy.get()
                        on:click=move |_| retry.update(|n| *n += 1)>"Reload rules"</button>
                </div>
            </Show>
            <Show when=move || !loading.get() && !config.get().is_null()>
            {[("Safety and controls", true), ("Weather and soil", false)].into_iter().map(|(title, protected_group)| view! {
            <section class="gate-group">
            <h3>{title}</h3>
            {crate::gates_catalog::builtin_rule_catalog().iter().filter(|g| g.3 == protected_group).map(|(id, label, meaning, protected)| {
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
                            <details class="gate-row__details">
                                <summary>{if *protected { "Why it stays on" } else { "What turning it off allows" }}</summary>
                                <span class="gate-row__meaning">{meaning.to_string()}</span>
                            </details>
                        </div>
                        {if *protected {
                            view! { <span class="gate-row__lock">"Always on"</span> }.into_any()
                        } else {
                            let id_sw = id_s.clone();
                            let id_on = id_s.clone();
                            let id_off = id_s.clone();
                            view! {
                                <button
                                    type="button"
                                    class="toggle-pill"
                                    role="switch"
                                    aria-label=format!("{label} rule")
                                    aria-checked=move || (!disabled_now().contains(&id_sw)).to_string()
                                    disabled=move || busy.get()
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
            </section>
            }).collect_view()}
            </Show>

            // Always mounted, outside the row map: a row's Off opens it,
            // it hides itself, and the disable writes from its on_confirm.
            <ConfirmSheet
                visible=confirm_open
                title=Signal::derive(move || match pending_gate.get() {
                    Some((id, _)) => {
                        let label = crate::gates_catalog::builtin_rule_catalog().iter().find(|(key, _, _, _)| *key == id).map(|(_, label, _, _)| *label).unwrap_or("watering");
                        format!("Turn off {label}?")
                    },
                    None => "Turn off this rule?".to_string(),
                })
                body=Signal::derive(move || {
                    let Some((_, meaning)) = pending_gate.get() else {
                        return String::new();
                    };
                    format!(
                        "{meaning} You can turn this rule back on here."
                    )
                })
                confirm_label=Signal::derive(|| "Turn off rule".to_string())
                on_confirm=do_disable
            />
        </div>
    }
}
