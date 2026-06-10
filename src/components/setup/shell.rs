// SetupShell. Top-level container for the first-run wizard. Mounted
// under /setup/* by app.rs. Renders the current step (picked by URL)
// inside a consistent header + progress strip + footer (back/next).
//
// Draft persistence flows through /api/wizard/draft: GET on mount,
// PUT on every field change (debounced by the caller; for now we PUT
// on each Next/Back transition).

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::components::ui::Panel;

const STEPS: &[(&str, &str)] = &[
    ("welcome", "Welcome"),
    ("location", "Location"),
    ("sources", "Sources"),
    ("controllers", "Controllers"),
    ("zones", "Zones"),
    ("llm", "LLM"),
    ("notifications", "Notifications"),
    ("account", "Account"),
    ("review", "Review"),
];

#[component]
pub fn SetupShell() -> impl IntoView {
    let params = use_params_map();
    let current_step = move || {
        params
            .read()
            .get("step")
            .unwrap_or_else(|| "welcome".to_string())
    };

    view! {
        <main id="main-content" class="setup-shell">
            <header class="setup-shell__header">
                <h1 class="setup-shell__title">"LocalSky setup"</h1>
                <p class="setup-shell__subtitle">
                    "First-run wizard. You can save and resume at any time; "
                    "your progress is stored on the server until you finalize."
                </p>
                <ProgressStrip current=current_step/>
            </header>

            <Panel title="".to_string()>
                {move || render_step(&current_step())}
            </Panel>
        </main>
    }
}

#[component]
fn ProgressStrip<F>(current: F) -> impl IntoView
where
    F: Fn() -> String + Copy + Send + Sync + 'static,
{
    view! {
        <ol class="setup-progress" aria-label="Wizard progress">
            {STEPS.iter().enumerate().map(|(i, (id, label))| {
                let id_owned = id.to_string();
                let label_owned = label.to_string();
                let id_a = id_owned.clone();
                let id_b = id_owned.clone();
                let n = i + 1;
                view! {
                    <li
                        class="setup-progress__step"
                        class:setup-progress__step--current=move || current() == id_a
                        aria-current=move || if current() == id_b { "step" } else { "false" }
                    >
                        <span class="setup-progress__num" aria-hidden="true">{n}</span>
                        <span class="setup-progress__label">{label_owned}</span>
                    </li>
                }
            }).collect_view()}
        </ol>
    }
}

fn render_step(step: &str) -> impl IntoView {
    use crate::components::setup::{
        AccountStep, ControllersStep, LlmStep, LocationStep, NotificationsStep, ReviewStep,
        SourcesStep, WelcomeStep, ZonesStep,
    };
    match step {
        "welcome" => view! { <WelcomeStep/> }.into_any(),
        "location" => view! { <LocationStep/> }.into_any(),
        "sources" => view! { <SourcesStep/> }.into_any(),
        "controllers" => view! { <ControllersStep/> }.into_any(),
        "zones" => view! { <ZonesStep/> }.into_any(),
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
        .find(|(id, _)| *id == step)
        .map(|(_, label)| label.to_string())
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

#[component]
pub fn SetupFooter(prev: Option<String>, next: Option<String>) -> impl IntoView {
    view! {
        <footer class="setup-footer">
            {prev.map(|href| view! {
                <a class="setup-footer__btn setup-footer__btn--ghost" href=href>"Back"</a>
            })}
            <a class="setup-footer__btn setup-footer__btn--ghost" href="/">
                "Save and finish later"
            </a>
            {next.map(|href| view! {
                <a class="setup-footer__btn setup-footer__btn--primary" href=href>"Next"</a>
            })}
        </footer>
    }
}

pub fn next_step_href(current: &str) -> Option<String> {
    let idx = STEPS.iter().position(|(id, _)| *id == current)?;
    STEPS.get(idx + 1).map(|(id, _)| format!("/setup/{id}"))
}

pub fn prev_step_href(current: &str) -> Option<String> {
    let idx = STEPS.iter().position(|(id, _)| *id == current)?;
    if idx == 0 {
        None
    } else {
        STEPS.get(idx - 1).map(|(id, _)| format!("/setup/{id}"))
    }
}
