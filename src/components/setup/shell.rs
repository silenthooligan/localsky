// SetupShell. Top-level container for the first-run wizard. Mounted
// under /setup/* by app.rs. Renders the current step (picked by URL)
// inside a consistent header + progress strip + footer (back/next).
//
// Draft persistence flows through /api/wizard/draft: GET on mount,
// PUT on every field change (debounced by the caller; for now we PUT
// on each Next/Back transition).

use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_params_map};

use crate::components::ui::{Button, Panel};

/// (route id, human label, optional). The step picker names optional extras.
const STEPS: &[(&str, &str, bool)] = &[
    ("welcome", "Welcome", false),
    ("location", "Your location", false),
    ("sources", "Weather & Sensors", false),
    // Controller + Zones are OPTIONAL: a weather-station-only (or address +
    // forecast) deployment is first-class, so a no-hardware user must not be
    // told two sprinkler steps are mandatory. Matches the Welcome promise
    // ("Any hardware, or none") and the engine's weather-only support.
    ("controllers", "Controller", true),
    ("zones", "Zones", true),
    ("rules", "Watering rules", true),
    ("sensors", "Sensors", true),
    ("llm", "AI advisor", true),
    ("notifications", "Notifications", true),
    ("account", "Account", true),
    ("review", "Review & apply", false),
];

#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
enum EntryStage {
    Loading,
    Choices,
    Editing,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryAction {
    Current,
    Resume,
    Fresh,
}

#[cfg(feature = "hydrate")]
async fn entry_state() -> Result<(bool, bool, bool), ()> {
    #[derive(serde::Deserialize)]
    struct State {
        config_present: bool,
        draft_present: bool,
    }
    let response = gloo_net::http::Request::get("/api/wizard/state")
        .send()
        .await
        .map_err(|_| ())?;
    if !response.ok() {
        return Err(());
    }
    let state: State = response.json().await.map_err(|_| ())?;
    let demo = if state.config_present {
        let response = gloo_net::http::Request::get("/api/v1/info")
            .send()
            .await
            .map_err(|_| ())?;
        if !response.ok() {
            return Err(());
        }
        response
            .json::<serde_json::Value>()
            .await
            .map_err(|_| ())?
            .get("demo")
            .and_then(|v| v.as_bool())
            .ok_or(())?
    } else {
        false
    };
    Ok((state.config_present, state.draft_present, demo))
}

#[component]
pub fn SetupShell() -> impl IntoView {
    let params = use_params_map();
    let current_step = move || {
        params
            .read()
            .get("step")
            .unwrap_or_else(|| "welcome".to_string())
    };

    // Welcome saves license acceptance. Do not mount it before the state
    // check: that used to create a blank draft on configured instances.
    // A saved draft is a choice, not evidence that it matches live settings.
    let stage = RwSignal::new(EntryStage::Loading);
    let configured = RwSignal::new(false);
    let has_draft = RwSignal::new(false);
    let editing = RwSignal::new(EntryAction::Fresh);
    let busy = RwSignal::new(false);
    let error: RwSignal<Option<String>> = RwSignal::new(None);
    let retry = RwSignal::new(0_u32);
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        retry.get();
        stage.set(EntryStage::Loading);
        leptos::task::spawn_local(async move {
            match entry_state().await {
                Ok((config, draft, demo)) => {
                    configured.set(config && !demo);
                    has_draft.set(draft);
                    stage.set(if config && !demo {
                        EntryStage::Choices
                    } else {
                        EntryStage::Editing
                    });
                }
                Err(()) => stage.set(EntryStage::Failed),
            }
        });
    });

    let navigate = use_navigate();
    let open = Callback::new(move |action: EntryAction| {
        if busy.get_untracked() {
            return;
        }
        error.set(None);
        if action == EntryAction::Resume {
            editing.set(action);
            stage.set(EntryStage::Editing);
            if current_step() == "welcome" {
                navigate("/setup/location", Default::default());
            }
            return;
        }
        busy.set(true);
        #[cfg(feature = "hydrate")]
        {
            let navigate = navigate.clone();
            leptos::task::spawn_local(async move {
                let request = match action {
                    EntryAction::Current => {
                        gloo_net::http::Request::post("/api/wizard/seed_current")
                    }
                    EntryAction::Fresh => gloo_net::http::Request::delete("/api/wizard/draft"),
                    EntryAction::Resume => unreachable!(),
                };
                match request.send().await {
                    Ok(response) if response.ok() => {
                        editing.set(action);
                        stage.set(EntryStage::Editing);
                        let target = if action == EntryAction::Fresh {
                            "/setup/welcome"
                        } else {
                            "/setup/location"
                        };
                        navigate(target, Default::default());
                    }
                    Ok(response) => {
                        let code = response.status();
                        let body = response.text().await.unwrap_or_default();
                        error.set(Some(crate::components::settings_ui::save_error_message(
                            code, &body,
                        )));
                    }
                    Err(_) => error.set(Some("Could not open your setup. Try again.".into())),
                }
                busy.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        busy.set(false);
    });

    view! {
        <div class="setup-shell">
            <header class="setup-shell__header">
                <div class="setup-shell__top">
                    <a href="/" class="setup-brand" aria-label="LocalSky home">
                        <img src=crate::base::url("/brand-mark.svg") alt="" width="40" height="40"/>
                        <span>"LocalSky"<small>"Your yard, connected."</small></span>
                    </a>
                    <crate::components::settings::theme::AppearancePicker/>
                </div>
                <h1 class="setup-shell__title">{move || if configured.get() { "Edit LocalSky setup" } else { "Set up LocalSky" }}</h1>
                <p class="setup-shell__subtitle setup-live-intro">
                    {move || if configured.get() && stage.get() == EntryStage::Editing {
                        match editing.get() {
                            EntryAction::Current => "Your current settings are loaded. Review changes before applying.",
                            EntryAction::Resume => "Your saved draft is loaded. Review changes before applying.",
                            EntryAction::Fresh => "You're editing a blank draft. Your running setup is unchanged.",
                        }
                    } else if configured.get() {
                        "Choose where to begin. Your running setup changes only when you select Save and finish."
                    } else {
                        "Choose your weather sources and watering setup. Progress is saved as you go; review it before applying."
                    }}
                </p>
                <p class="setup-shell__subtitle setup-demo-intro" role="status">
                    "This demo is read-only. You can explore setup, but changes aren't saved and device probes are disabled."
                </p>
                <Show when=move || stage.get() == EntryStage::Editing><ProgressStrip current=current_step/></Show>
            </header>

            <Panel title="".to_string()>
                {move || match stage.get() {
                    EntryStage::Loading => view! {
                        <p role="status">"Loading your setup…"</p>
                    }.into_any(),
                    EntryStage::Failed => view! {
                        <div class="setup-step">
                            <p role="alert">"Could not load your current setup. Try again."</p>
                            <div class="setup-reentry">
                                <Button variant="primary" on_click=Callback::new(move |_| retry.update(|n| *n += 1))>"Try again"</Button>
                                <Button variant="ghost" href="/settings">"Back to Settings"</Button>
                            </div>
                        </div>
                    }.into_any(),
                    EntryStage::Choices => view! {
                        <div class="setup-step">
                            <h2 class="setup-step__title">"This LocalSky is already set up"</h2>
                            <div class="setup-entry-options">
                                <section>
                                    <h3>"Current setup"</h3>
                                    <p>"Load your running settings into the wizard."</p>
                                    <Show when=move || has_draft.get()><p class="sensors-section__hint">"This replaces the saved draft."</p></Show>
                                    <Button variant="primary" disabled=Signal::derive(move || busy.get())
                                        on_click=Callback::new(move |_| open.run(EntryAction::Current))>"Edit current setup"</Button>
                                </section>
                                <Show when=move || has_draft.get()>
                                    <section>
                                        <h3>"Saved draft"</h3>
                                        <p>"Continue your unfinished setup. It may differ from your running settings."</p>
                                        <Button variant="secondary" disabled=Signal::derive(move || busy.get())
                                            on_click=Callback::new(move |_| open.run(EntryAction::Resume))>"Resume saved draft"</Button>
                                    </section>
                                </Show>
                            </div>
                            <details class="setup-reference">
                                <summary>"Start from scratch"</summary>
                                <div class="setup-entry-fresh">
                                    <p>"Create a blank draft instead of using your current settings. This replaces any saved draft."</p>
                                    <Button variant="secondary" disabled=Signal::derive(move || busy.get())
                                        on_click=Callback::new(move |_| open.run(EntryAction::Fresh))>"Create blank draft"</Button>
                                </div>
                            </details>
                            {move || error.get().map(|message| view! { <p role="alert">{message}</p> })}
                            <Show when=move || busy.get()><p role="status">"Opening your setup…"</p></Show>
                            <Button variant="ghost" href="/settings">"Back to Settings"</Button>
                        </div>
                    }.into_any(),
                    EntryStage::Editing => render_step(&current_step()).into_any(),
                }}
            </Panel>
        </div>
    }
}

#[component]
fn ProgressStrip<F>(current: F) -> impl IntoView
where
    F: Fn() -> String + Copy + Send + Sync + 'static,
{
    let navigate = use_navigate();
    let save_status = crate::components::setup::draft::status();
    let idx = move || {
        STEPS
            .iter()
            .position(|(id, _, _)| *id == current())
            .unwrap_or(0)
    };
    view! {
        <div class="setup-progress" aria-label="Setup progress">
            <div class="setup-progress__meta">
                <div>
                <span class="setup-progress__count">
                    {move || format!("Step {} of {}", idx() + 1, STEPS.len())}
                </span>
                <span class="setup-progress__name">
                    {move || {
                        let i = idx();
                        let (_, label, optional) = STEPS[i];
                        if optional { format!("{label} (optional)") } else { label.to_string() }
                    }}
                </span>
                </div>
                <label class="setup-progress__jump">
                    <span>"Jump to step"</span>
                    // Wait for in-flight saves, but keep read-only demo steps explorable.
                    <select class="ui-input" prop:value=current
                        disabled=move || { save_status.get().pending > 0 }
                        on:change=move |ev| {
                            let id = event_target_value(&ev);
                            if STEPS.iter().any(|(step, _, _)| *step == id) {
                                navigate(&format!("/setup/{id}"), Default::default());
                            }
                        }>
                        {STEPS.iter().enumerate().map(|(i, (id, label, _))| view! {
                            <option value=*id>{format!("{}. {label}", i + 1)}</option>
                        }).collect_view()}
                    </select>
                </label>
            </div>
            <div class="setup-progress__track" role="progressbar"
                aria-label="Setup progress"
                aria-valuemin="1"
                aria-valuemax=STEPS.len().to_string()
                aria-valuenow=move || (idx() + 1).to_string()
            >
                <div
                    class="setup-progress__fill"
                    style:width=move || format!("{:.1}%", ((idx() + 1) as f64 / STEPS.len() as f64) * 100.0)
                ></div>
            </div>
        </div>
    }
}

fn render_step(step: &str) -> impl IntoView {
    use crate::components::setup::{
        AccountStep, ControllersStep, LlmStep, LocationStep, NotificationsStep, ReviewStep,
        RulesStep, SensorsStep, SourcesStep, WelcomeStep, ZonesStep,
    };
    match step {
        "welcome" => view! { <WelcomeStep/> }.into_any(),
        "location" => view! { <LocationStep/> }.into_any(),
        "sources" => view! { <SourcesStep/> }.into_any(),
        "controllers" => view! { <ControllersStep/> }.into_any(),
        "zones" => view! { <ZonesStep/> }.into_any(),
        "rules" => view! { <RulesStep/> }.into_any(),
        "sensors" => view! { <SensorsStep/> }.into_any(),
        "llm" => view! { <LlmStep/> }.into_any(),
        "notifications" => view! { <NotificationsStep/> }.into_any(),
        "account" => view! { <AccountStep/> }.into_any(),
        "review" => view! { <ReviewStep/> }.into_any(),
        other => view! { <StepPlaceholder step=other.to_string()/> }.into_any(),
    }
}

#[component]
fn StepPlaceholder(step: String) -> impl IntoView {
    let next_href = next_step_href(&step);
    let prev_href = prev_step_href(&step);
    let label = STEPS
        .iter()
        .find(|(id, _, _)| *id == step)
        .map(|(_, label, _)| label.to_string())
        .unwrap_or_else(|| step.clone());
    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">{label}</h2>
            <p class="setup-step__body">
                "This step is being built. The wizard scaffolding is in place; "
                "the editor for this section ships in a follow-up release. "
                "Skip ahead to keep moving and come back when it lands."
            </p>
            <SetupFooter prev=prev_href next=next_href/>
        </div>
    }
}

// Props are reactive (`#[prop(into)]` accepts both a plain
// Option<String> for ungated steps and a Signal::derive for gated ones)
// so a step whose gate opens after mount (license accepted, location
// picked) reveals Next without a remount. Reading the props once via
// get_untracked froze the gate at its mount-time value.
#[component]
pub fn SetupFooter(
    #[prop(into)] prev: Signal<Option<String>>,
    #[prop(into)] next: Signal<Option<String>>,
) -> impl IntoView {
    let save_status = crate::components::setup::draft::status();
    let can_leave = Signal::derive(move || {
        let status = save_status.get();
        status.pending == 0 && status.error.is_none()
    });
    let save_status = crate::components::setup::draft::status();
    let pending_status = save_status.clone();
    view! {
        <footer class="setup-footer">
            <div class="setup-footer__back">
            {move || prev.get().map(|href| view! {
                <Button variant="ghost" href=href>"Back"</Button>
            })}
            </div>
            <div class="setup-footer__later">
            {move || if can_leave.get() {
                view! { <Button variant="ghost" href="/".to_string()>"Save and finish later"</Button> }.into_any()
            } else {
                view! { <Button variant="ghost" disabled=true>"Save and finish later"</Button> }.into_any()
            }}
            </div>
            <div class="setup-footer__next">
            {move || next.get().filter(|_| can_leave.get()).map(|href| view! {
                <Button variant="primary" href=href>"Next"</Button>
            })}
            </div>
            {move || (pending_status.get().pending > 0).then(|| view! {
                <p role="status">"Saving your setup..."</p>
            })}
            {move || save_status.get().error.map(|error| view! {
                <p role="alert">{error}</p>
            })}
        </footer>
    }
}

pub fn next_step_href(current: &str) -> Option<String> {
    let idx = STEPS.iter().position(|(id, _, _)| *id == current)?;
    STEPS.get(idx + 1).map(|(id, _, _)| format!("/setup/{id}"))
}

pub fn prev_step_href(current: &str) -> Option<String> {
    let idx = STEPS.iter().position(|(id, _, _)| *id == current)?;
    if idx == 0 {
        None
    } else {
        STEPS.get(idx - 1).map(|(id, _, _)| format!("/setup/{id}"))
    }
}

#[cfg(test)]
mod vocabulary_tests {
    /// The wizard speaks to a homeowner. Engineering vocabulary in a
    /// string a person reads during setup is a bug: the reader did not
    /// choose the file format, the standards body or the environment.
    #[test]
    fn the_wizard_speaks_to_a_homeowner() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/components/setup");
        let banned = [
            "FAO-56",
            "IANA",
            "env var",
            ".toml",
            "/api/",
            "ET0",
            "null island",
        ];
        let mut hits = Vec::new();
        for path in crate::engine::clock::rust_sources(&root) {
            let src = std::fs::read_to_string(&path).unwrap();
            // Strings a person reads: comments, tests and request URLs
            // ("/api/wizard/draft" is code, not copy) may say what they like.
            let prose = src.split("#[cfg(test)]").next().unwrap_or("");
            for (n, line) in prose.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") || line.contains("\"/api/") {
                    continue;
                }
                for b in banned {
                    if line.contains(b) && line.contains('"') {
                        hits.push(format!("{}:{}: {b}", path.display(), n + 1));
                    }
                }
            }
        }
        assert!(
            hits.is_empty(),
            "wizard strings use engineering vocabulary:\n{}",
            hits.join("\n")
        );
    }

    #[test]
    fn the_steps_include_rules() {
        assert!(super::STEPS
            .iter()
            .any(|(id, label, optional)| *id == "rules"
                && *label == "Watering rules"
                && *optional));
    }
}
