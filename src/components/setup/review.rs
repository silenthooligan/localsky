// ReviewStep. Final page of the wizard. Renders a per-section summary of
// the draft (with Edit links back into each step) and a primary "Save and
// finish" button that POSTs /api/wizard/apply.
//
// The apply hot-reloads what it can and answers with the residue only a boot
// can wire. That residue is not academic here: the zone-to-station map is
// baked into each controller at construction, so rewiring a valve to another
// station or moving the controller to a new host is on disk and NOT in the
// running app. On a FIRST install there is no "what it can" at all, because
// the process booted with no config and therefore wired nothing (no
// controller registry, no source adapters), and the apply now reports that
// too, so this step's ordinary answer on a new install is the hold.
//
// A clean apply redirects to /. A restart-required apply must not: the step
// holds and raises the same RestartBanner the settings pages raise for the
// same edit, and keeps its own undismissable record of the hold beside it,
// because this is the screen someone walks away from believing the yard is
// done. On failure, surfaces the validation error.

use leptos::prelude::*;

use crate::components::config_client::outcome_from_body;
use crate::components::settings::RestartBanner;
use crate::components::setup::shell::{prev_step_href, SetupFooter};
use crate::components::sources_form::kind_pretty;
use crate::components::ui::Button;

/// The scheduling-model sentence for the final step. The apply stamps
/// the Soil model on a genuine first install (no config on disk yet)
/// and otherwise keeps whatever the config holds, so the biggest
/// behavioral decision of the install is stated to the person applying
/// it instead of being made silently. Derived from the draft: an
/// explicit value names itself; an absent key states the apply rule.
fn scheduling_model_sentence(draft: &serde_json::Value) -> String {
    let model = draft
        .get("config")
        .and_then(|c| c.get("engine"))
        .and_then(|e| e.get("scheduling_model"))
        .and_then(|v| v.as_str());
    match model {
        Some("soil") => "Watering runs on the Soil model: each zone waters when its own \
                         deficit crosses its trigger, assuming a dry yard at first. \
                         Change it under Settings, Engine."
            .to_string(),
        Some(_) => "Watering runs on the Weekly model: each zone waters toward its \
                    weekly target, split across sessions. Change it under \
                    Settings, Engine."
            .to_string(),
        None => "A new install starts on the Soil model: each zone waters when its \
                 own deficit crosses its trigger. A rerun keeps the model you \
                 have. Change it under Settings, Engine."
            .to_string(),
    }
}

/// One summary row: section label, computed value text, Edit link target.
fn summary_rows(draft: &serde_json::Value) -> Vec<(&'static str, String, &'static str)> {
    let cfg = draft.get("config").cloned().unwrap_or_default();

    let loc = cfg
        .get("deployment")
        .and_then(|d| d.get("location"))
        .cloned()
        .unwrap_or_default();
    let lat = loc.get("lat").and_then(|v| v.as_f64());
    let lon = loc.get("lon").and_then(|v| v.as_f64());
    let tz = cfg
        .get("deployment")
        .and_then(|d| d.get("timezone"))
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let location_text = match (lat, lon) {
        (Some(lat), Some(lon)) if lat != 0.0 || lon != 0.0 => {
            let tz_note = tz
                .map(|t| format!(", {t}"))
                .unwrap_or_else(|| ", timezone inferred at boot".into());
            format!("{lat:.4}, {lon:.4}{tz_note}")
        }
        _ => "Not set. LocalSky needs it for the forecast and for sunrise.".into(),
    };

    let sources = cfg
        .get("sources")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let sources_text = if sources.is_empty() {
        "None added. LocalSky uses Open-Meteo for your forecast and listens for a Tempest on your network, so you will see weather right away."
            .into()
    } else {
        let kinds: Vec<String> = sources
            .iter()
            .filter_map(|s| s.get("kind").and_then(|k| k.as_str()))
            .map(|k| kind_pretty(k).to_string())
            .collect();
        format!("{} ({})", sources.len(), kinds.join(", "))
    };

    let controllers = cfg
        .get("controllers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let controllers_text = if controllers.is_empty() {
        "None yet. Add one under Settings, or let Home Assistant run the valves.".into()
    } else {
        let names: Vec<String> = controllers
            .iter()
            .filter_map(|c| {
                let id = c.get("id")?.as_str()?;
                let default = c.get("default").and_then(|d| d.as_bool()) == Some(true);
                Some(if default {
                    format!("{id} (default)")
                } else {
                    id.to_string()
                })
            })
            .collect();
        names.join(", ")
    };

    let zones = cfg
        .get("zones")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let zones_text = if zones.is_empty() {
        "None yet (add in Settings -> Zones any time)".into()
    } else {
        let names: Vec<String> = zones
            .values()
            .filter_map(|z| z.get("display_name").and_then(|n| n.as_str()))
            .map(str::to_string)
            .collect();
        format!("{}: {}", zones.len(), names.join(", "))
    };

    // Soil-probe bindings: how many zones carry a non-null soil_sensor_id.
    let bound_count = zones
        .values()
        .filter(|z| {
            z.get("soil_sensor_id")
                .map(|v| v.is_string())
                .unwrap_or(false)
        })
        .count();
    let sensors_text = if bound_count == 0 {
        "None bound. Scheduling runs on weather evidence; add probes later under Settings, Sensors."
            .into()
    } else {
        format!(
            "{bound_count} zone{} bound to a soil probe",
            if bound_count == 1 { "" } else { "s" }
        )
    };

    let llm = cfg.get("llm").cloned().unwrap_or(serde_json::Value::Null);
    let llm_text = if llm.is_null() {
        "Disabled".into()
    } else {
        llm.get("provider")
            .and_then(|p| p.as_str())
            .map(|p| match p {
                "auto" => "Auto-detect on boot".to_string(),
                "ollama" => "Ollama".to_string(),
                "llamacpp" => "llama.cpp".to_string(),
                "openai_compat" => "OpenAI-compatible".to_string(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "Configured".into())
    };

    let notif = cfg
        .get("notifications")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let channels: Vec<&str> = [
        ("web_push", "Web Push"),
        ("mqtt", "MQTT"),
        ("ntfy", "ntfy"),
        ("slack", "Slack"),
        ("email", "Email"),
    ]
    .iter()
    .filter(|(key, _)| notif.get(*key).map(|v| !v.is_null()).unwrap_or(false))
    .map(|(_, label)| *label)
    .collect();
    let notif_text = if channels.is_empty() {
        "None (dashboard only)".into()
    } else {
        channels.join(", ")
    };

    vec![
        ("Location", location_text, "/setup/location"),
        ("Weather sources", sources_text, "/setup/sources"),
        ("Controllers", controllers_text, "/setup/controllers"),
        ("Zones", zones_text, "/setup/zones"),
        (
            "Watering rules",
            crate::components::setup::rules::rules_summary(&cfg),
            "/setup/rules",
        ),
        ("Soil probes", sensors_text, "/setup/sensors"),
        ("LLM advisor", llm_text, "/setup/llm"),
        ("Notifications", notif_text, "/setup/notifications"),
    ]
}

/// What the review step does with a successful apply. The apply response
/// carries the restart contract (`restart_required` + `restart_reasons`) and
/// this step used to discard it and redirect regardless, which told the owner
/// "configured" about a binding that would not actuate until the next boot.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
enum AfterApply {
    /// Everything took effect live: arm the "start here" card and go to the
    /// dashboard, the way finishing the wizard always has.
    Redirect,
    /// Boot-only residue, one server reason per line. Hold the step and show
    /// the reasons instead of claiming the setup is live.
    HoldForRestart(Vec<String>),
}

/// Read the apply body through the shared config outcome helper, so the wizard
/// and the settings pages agree on what "restart required" means for the same
/// edit. A missing or old field reads as "no restart", the helper's safe
/// default, so an apply that really did land whole still walks straight to the
/// dashboard. That is now the RE-RUN case rather than the first install: a
/// first apply reports the wiring its own process never did (see post_apply's
/// first_apply_boot_residue), so a new install normally holds here.
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
fn after_apply(body: &serde_json::Value) -> AfterApply {
    let outcome = outcome_from_body(body);
    if outcome.restart_required() {
        return AfterApply::HoldForRestart(outcome.restart_reasons);
    }
    // The server sets the flag FROM a non-empty reason list, so a flag with no
    // reasons should be unreachable. Hold anyway, and supply the line: holding
    // on an empty list would render an invisible banner and strand the owner on
    // a page that explains nothing, and redirecting would repeat exactly the
    // defect this function exists to close.
    if body.get("restart_required").and_then(|v| v.as_bool()) == Some(true) {
        let plain = "Part of this setup takes effect the next time LocalSky starts.";
        return AfterApply::HoldForRestart(vec![plain.to_string()]);
    }
    AfterApply::Redirect
}

#[component]
pub fn ReviewStep() -> impl IntoView {
    let applying = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    let draft = RwSignal::new(serde_json::Value::Null);
    // The shared restart banner's two signals, wired exactly as the settings
    // pages wire them: an empty `reasons` keeps it hidden, and `dismissed`
    // resets on each apply so a fresh restart-required answer re-shows it.
    let restart_reasons: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    let restart_dismissed = RwSignal::new(false);
    // Latched once the apply succeeds. The apply CONSUMES the draft, so a
    // second click can only fail; while the step is held for a restart the
    // button must not read as though there is more to save.
    let applied = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = crate::components::setup::draft::fetch().await {
                draft.set(d);
            }
        });
    });
    #[cfg(not(feature = "hydrate"))]
    let _ = draft;

    let on_apply = move |_| {
        applying.set(true);
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match call_apply().await {
                    Ok(body) => {
                        applied.set(true);
                        // Arm the one-time "start here" card on the Weather
                        // home (welcome_card.rs reads this) on both paths: the
                        // config IS written either way, and a held step still
                        // ends at the dashboard once the restart lands.
                        if let Some(win) = web_sys::window() {
                            if let Ok(Some(storage)) = win.local_storage() {
                                let _ = storage.set_item("first_run_done", "0");
                            }
                        }
                        match after_apply(&body) {
                            AfterApply::Redirect => {
                                // The green line is the redirect's alone: the
                                // hold is a saved config that is NOT in effect,
                                // and painting that the colour of success was
                                // the last thing left telling the owner the
                                // yard was done.
                                result_ok.set(true);
                                result_msg.set(
                                    "Configuration saved. Redirecting to dashboard…".to_string(),
                                );
                                if let Some(win) = web_sys::window() {
                                    // Replace, not assign: a back gesture after
                                    // setup must land on the dashboard, never
                                    // re-enter the completed wizard.
                                    let _ = win.location().replace(&crate::base::url("/"));
                                }
                            }
                            AfterApply::HoldForRestart(reasons) => {
                                // No redirect on purpose. The dashboard would
                                // show a yard that is configured on disk and
                                // still wired the old way in the running app
                                // (on a first install, not wired at all).
                                //
                                // result_msg stays EMPTY here: the held panel
                                // under the button carries the wording, so the
                                // hold never paints in the success colour, and
                                // only ONE polite live region (the banner's)
                                // updates in this frame instead of two, where
                                // a screen reader typically drops one.
                                restart_dismissed.set(false);
                                restart_reasons.set(reasons);
                            }
                        }
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(e);
                    }
                }
                applying.set(false);
            });
        }
        // SSR path: no-op; the button is only meaningfully interactive
        // after hydrate.
        #[cfg(not(feature = "hydrate"))]
        {
            applying.set(false);
        }
    };

    view! {
        <div class="setup-step">
            // Above the title, and sticky, on purpose: when the apply reports a
            // boot-only change this notice IS the answer to "did that work",
            // and the person reading it is already on their way out.
            <RestartBanner reasons=restart_reasons dismissed=restart_dismissed/>
            <h2 class="setup-step__title">"Everything look right?"</h2>
            <p class="setup-step__body">
                "When you click apply, your settings are saved. Anything LocalSky "
                "can only set up while it is starting keeps you on this page, with "
                "a button that starts it: on a new install that is most of it, "
                "because LocalSky came up before your yard existed. Otherwise the "
                "dashboard opens. If something does not check out, you get a "
                "specific message here and nothing changes."
            </p>

            {move || {
                let d = draft.get();
                if d.is_null() {
                    return ().into_any();
                }
                let rows = summary_rows(&d)
                    .into_iter()
                    .map(|(label, value, href)| view! {
                        <div class="review-row">
                            <span class="review-row__label">{label}</span>
                            <span class="review-row__value">{value}</span>
                            <a class="review-row__edit" href=href>"Edit"</a>
                        </div>
                    })
                    .collect_view();
                view! { <div class="review-table">{rows}</div> }.into_any()
            }}

            {move || {
                let d = draft.get();
                if d.is_null() {
                    return ().into_any();
                }
                view! {
                    <div class="review-summary">
                        <p class="review-summary__line">{scheduling_model_sentence(&d)}</p>
                    </div>
                }
                .into_any()
            }}

            <div class="review-summary">
                <p class="review-summary__line">
                    "Your settings are saved, and a copy of each version is kept "
                    "so you can roll back from Settings if you change your mind."
                </p>
                <p class="review-summary__line">
                    "Once applied, day-to-day edits live in /settings. If you "
                    "open the wizard again it offers a choice: modify the "
                    "current setup or start fresh."
                </p>
            </div>

            {move || {
                // Warn (not block) at apply when no zones are configured.
                let d = draft.get();
                let has_zones = d
                    .get("config")
                    .and_then(|c| c.get("zones"))
                    .and_then(|z| z.as_object())
                    .map(|m| !m.is_empty())
                    .unwrap_or(false);
                if d.is_null() || has_zones {
                    ().into_any()
                } else {
                    view! {
                        <p class="setup-zero-zone-warn">
                            "Heads up: no zones are configured yet, so irrigation is idle "
                            "and nothing will water. The weather home still works fully. "
                            "You can apply now and add zones any time under Settings -> "
                            "Zones, or go back to the Zones step to add your first one."
                        </p>
                    }
                    .into_any()
                }
            }}

            <crate::components::ui::Button
    variant="primary"
    size="md"
    disabled=Signal::derive(move || applying.get() || applied.get())
    on_click=Callback::new(on_apply)
    class="setup-apply-btn">
                {move || {
                    if applying.get() {
                        "Saving…"
                    } else if applied.get() {
                        "Saved"
                    } else {
                        "Save and finish"
                    }
                }}
            </crate::components::ui::Button>

            // The step's own record of the hold, and the part of it nobody
            // can dismiss. RestartBanner is dismiss-forever, and the reasons
            // live only in a client signal, so a Dismiss used to destroy the
            // one explanation of why the yard is not running yet and leave
            // the owner on a page that explains nothing. This survives it,
            // repeats the reasons once the banner is gone, and puts the
            // banner (with its Restart now button) back.
            //
            // NOT role="status": the banner is the live region for this
            // outcome, and two polite updates in one frame usually announce
            // as one. It borrows the step's existing warn box rather than
            // introducing a class, since the stylesheet is not this change's
            // to extend.
            {move || {
                if !applied.get() || restart_reasons.get().is_empty() {
                    return ().into_any();
                }
                let banner_gone = restart_dismissed.get();
                let reasons = banner_gone.then(|| {
                    view! {
                        <div class="review-summary">
                            {restart_reasons
                                .get()
                                .into_iter()
                                .map(|r| view! { <div>{r}</div> })
                                .collect_view()}
                        </div>
                    }
                });
                let reraise = banner_gone.then(|| {
                    view! {
                        <Button
                            variant="ghost"
                            size="sm"
                            on_click=Callback::new(move |_: leptos::ev::MouseEvent| {
                                restart_dismissed.set(false)
                            })
                        >
                            "Show the restart notice again"
                        </Button>
                    }
                });
                view! {
                    <div class="setup-zero-zone-warn">
                        <strong>"Saved, and not running yet."</strong>
                        " Your answers are written down, but the parts LocalSky can "
                        "only set up while it is starting are not running, so nothing "
                        "waters until you restart it. The Restart now button in the "
                        "notice at the top of this page does that. LocalSky comes back "
                        "to this page afterwards and offers to modify the setup or "
                        "start fresh: that screen means it worked, and Back to "
                        "Settings leaves it."
                        {reasons}
                        {reraise}
                    </div>
                }
                .into_any()
            }}

            <Show when=move || !result_msg.get().is_empty()>
                <p
                    class="setup-result"
                    class:setup-result--ok=move || result_ok.get()
                    class:setup-result--err=move || !result_ok.get()
                    role="status"
                >
                    {move || result_msg.get()}
                </p>
            </Show>

            // The footer goes with the spent step. The apply CONSUMED the
            // draft server-side, so Back would land on an earlier step that
            // fetches nothing, renders empty fields and has no way forward
            // (the apply button is latched); "Save and finish later" would
            // walk the owner to a dashboard that cannot water until the
            // restart, which is the exact walk-away this step exists to
            // prevent. On the redirect path none of this is ever seen.
            {move || (!applied.get()).then(|| view! {
                <SetupFooter prev=prev_step_href("review") next={None::<String>}/>
            })}
        </div>
    }
}

/// POST the apply and hand back its BODY. The response carries the restart
/// contract, so returning `()` here (which it used to) made it impossible for
/// the caller to know a boot was still owed.
#[cfg(feature = "hydrate")]
async fn call_apply() -> Result<serde_json::Value, String> {
    use gloo_net::http::Request;
    match Request::post("/api/wizard/apply").send().await {
        Ok(r) if r.ok() => {
            // A 2xx whose body will not parse reads as "nothing to restart",
            // the same safe default the shared config client takes.
            let body = r.json::<serde_json::Value>().await;
            Ok(body.unwrap_or(serde_json::Value::Null))
        }
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            Err(format!("Apply failed (HTTP {status}): {body}"))
        }
        Err(e) => Err(format!("Network error: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valve rewired to a different station (or a controller moved to a new
    /// host) is written to disk but NOT live: the zone-to-station map is baked
    /// into each controller at construction, so the apply answers
    /// restart_required with the reason. The wizard must hold the review step
    /// and carry those reasons to the banner. Against the old code this fails:
    /// the apply body was discarded and every success redirected to the
    /// dashboard, which told the owner the new binding was in effect.
    #[test]
    fn a_restart_required_apply_holds_the_step_and_keeps_the_reasons() {
        let body = serde_json::json!({
            "saved": 12,
            "restart_required": true,
            "restart_reasons": [
                "a zone moved to a different controller or station \
                 (zone-to-controller bindings are wired at boot)",
                "an irrigation controller was added, removed, enabled/disabled, \
                 or changed kind (controllers are wired at boot)",
            ],
        });
        match after_apply(&body) {
            AfterApply::HoldForRestart(reasons) => {
                assert_eq!(reasons.len(), 2, "{reasons:?}");
                assert!(reasons[0].contains("station"), "{reasons:?}");
                assert!(reasons[1].contains("controller"), "{reasons:?}");
            }
            other => panic!("a restart-required apply must not redirect: {other:?}"),
        }
    }

    /// The hold is for the residue only. An apply whose every part hot-reloaded
    /// still walks straight to the dashboard, so the fix does not put a restart
    /// prompt in front of an apply that does not need one (an unchanged wizard
    /// re-run over a configured install is the live example). An answer with no
    /// restart fields at all reads the same way, the safe default the shared
    /// config client already takes.
    #[test]
    fn an_apply_with_nothing_boot_only_still_redirects() {
        assert_eq!(
            after_apply(&serde_json::json!({
                "saved": 1, "restart_required": false, "restart_reasons": []
            })),
            AfterApply::Redirect
        );
        assert_eq!(
            after_apply(&serde_json::json!({ "saved": 1 })),
            AfterApply::Redirect
        );
    }

    /// Defensive. The server derives the flag from a non-empty reason list, so
    /// a flagged answer with no reasons should be unreachable; if it ever
    /// happens the step must still hold AND still say something, because
    /// holding on an empty list renders an invisible banner (the banner hides
    /// itself when reasons are empty) and strands the owner with no
    /// explanation and no button.
    #[test]
    fn a_flagged_apply_with_no_reasons_still_holds_and_still_speaks() {
        match after_apply(&serde_json::json!({ "restart_required": true })) {
            AfterApply::HoldForRestart(reasons) => {
                assert_eq!(reasons.len(), 1, "{reasons:?}");
                assert!(!reasons[0].trim().is_empty(), "{reasons:?}");
            }
            other => panic!("a flagged restart must not redirect: {other:?}"),
        }
    }

    /// The step consumes the decision and raises the SHARED settings banner
    /// (which carries "Restart now" and the in-progress-watering force path)
    /// rather than growing a second, weaker warning of its own. Guards the
    /// wiring the pure tests above cannot see: a correct `after_apply` that
    /// nothing calls is the exact shape of the defect being fixed.
    #[test]
    fn the_step_reads_the_outcome_and_mounts_the_shared_banner() {
        let src = include_str!("review.rs");
        let code = crate::engine::clock::code_only(src.split("#[cfg(test)]").next().unwrap());
        assert!(
            code.contains("match after_apply(&body)"),
            "the apply response must be read, not discarded"
        );
        assert!(
            code.contains("<RestartBanner"),
            "the review step must mount the shared settings restart banner"
        );
    }

    /// The redirect must stay CONDITIONAL, which the guard above cannot see.
    /// A HoldForRestart arm that ALSO navigated would contain both of that
    /// test's substrings, pass all three pure tests above, and reinstate the
    /// exact defect being fixed: the wizard walking on to a dashboard that
    /// reads "configured" about a yard the running process cannot water. So
    /// this pins the shape instead of the call: the file navigates exactly
    /// once, that navigation is inside the Redirect arm, and nothing from the
    /// hold arm onward navigates at all. It also pins the held panel, whose
    /// whole job is to outlive a Dismiss.
    #[test]
    fn only_the_clean_apply_navigates_and_the_hold_keeps_its_own_record() {
        let src = include_str!("review.rs");
        let code = crate::engine::clock::code_only(src.split("#[cfg(test)]").next().unwrap());
        assert_eq!(
            code.matches("location().replace").count(),
            1,
            "the step leaves the wizard exactly once, on the clean-apply path"
        );
        let hold = code
            .split("AfterApply::HoldForRestart(reasons) =>")
            .nth(1)
            .expect("the hold arm is a match arm in this file");
        for nav in ["location()", "replace(", "set_href(", "assign("] {
            assert!(
                !hold.contains(nav),
                "the hold arm, and everything the file does after it, must not \
                 navigate; found `{nav}`"
            );
        }
        let redirect = code
            .split("AfterApply::Redirect =>")
            .nth(1)
            .expect("the redirect arm is a match arm in this file")
            .split("AfterApply::HoldForRestart")
            .next()
            .unwrap();
        assert!(
            redirect.contains("location().replace"),
            "the ONE navigation belongs to the clean apply"
        );
        // The held panel: it reads the reasons itself (the arm only sets
        // them), so a dismissed banner does not take the explanation with it,
        // and it can put the banner back.
        assert!(
            hold.contains("restart_reasons.get()"),
            "the step must render its own copy of the hold, not lean entirely \
             on a dismissable banner"
        );
        assert!(
            code.matches("restart_dismissed.set(false)").count() >= 2,
            "one un-dismiss on apply is not enough: the held panel must be able \
             to put a dismissed banner back"
        );
    }

    /// The final step states the model that will govern watering, in one
    /// sentence, for all three draft shapes: explicit soil, explicit
    /// weekly, and the absent key whose fate the apply decides (Soil on
    /// a genuine first install, unchanged on a rerun).
    #[test]
    fn the_review_states_the_scheduling_model() {
        let soil = serde_json::json!({ "config": { "engine": { "scheduling_model": "soil" } } });
        let s = scheduling_model_sentence(&soil);
        assert!(s.starts_with("Watering runs on the Soil model"), "{s}");
        // The new operator is told why the first mornings may water more
        // than they expect.
        assert!(s.contains("dry yard"), "{s}");

        let weekly =
            serde_json::json!({ "config": { "engine": { "scheduling_model": "weekly" } } });
        let s = scheduling_model_sentence(&weekly);
        assert!(s.starts_with("Watering runs on the Weekly model"), "{s}");

        let absent = serde_json::json!({ "config": { "engine": {} } });
        let s = scheduling_model_sentence(&absent);
        assert!(
            s.starts_with("A new install starts on the Soil model"),
            "{s}"
        );
        // Someone re-running the wizard over a working install is told
        // their model is not about to change under them.
        assert!(s.contains("A rerun keeps the model you have"), "{s}");
    }
}
