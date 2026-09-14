// SetupShell. Top-level container for the first-run wizard. Mounted
// under /setup/* by app.rs. Renders the current step (picked by URL)
// inside a consistent header + progress strip + footer (back/next).
//
// Draft persistence flows through /api/wizard/draft: GET on mount,
// PUT on every field change (debounced by the caller; for now we PUT
// on each Next/Back transition).

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;

use crate::components::ui::{Button, Panel};

/// (route id, human label, optional). Optional steps are skippable
/// extras; the progress UI renders them as hollow dots.
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

#[component]
pub fn SetupShell() -> impl IntoView {
    let params = use_params_map();
    let current_step = move || {
        params
            .read()
            .get("step")
            .unwrap_or_else(|| "welcome".to_string())
    };

    // Re-entry gate. On an already-configured instance with no draft in
    // progress, the wizard opens with a choice (modify vs start fresh)
    // instead of silently walking toward a config wipe. SSR + the first
    // hydrate frame render the normal step (gate=false), then the state
    // probe flips the gate client-side only when it applies.
    let gate: RwSignal<bool> = RwSignal::new(false);
    let gate_busy = RwSignal::new(false);
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            let Ok(resp) = gloo_net::http::Request::get("/api/wizard/state")
                .send()
                .await
            else {
                return;
            };
            let Ok(v) = resp.json::<serde_json::Value>().await else {
                return;
            };
            let config = v.get("config_present").and_then(|b| b.as_bool()) == Some(true);
            let draft = v.get("draft_present").and_then(|b| b.as_bool()) == Some(true);
            if config && !draft {
                gate.set(true);
            }
        });
    });

    let modify = move |_| {
        if gate_busy.get_untracked() {
            return;
        }
        gate_busy.set(true);
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let ok = gloo_net::http::Request::post("/api/wizard/seed_current")
                .send()
                .await
                .map(|r| r.ok())
                .unwrap_or(false);
            if ok {
                if let Some(win) = web_sys::window() {
                    let _ = win
                        .location()
                        .set_href(&crate::base::url("/setup/location"));
                }
                gate.set(false);
            }
            gate_busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        gate_busy.set(false);
    };
    let fresh = move |_| {
        // Just proceed: the default (absent) draft is the blank slate;
        // nothing on disk changes until Save and finish.
        gate.set(false);
    };

    view! {
        <div class="setup-shell">
            <header class="setup-shell__header">
                <h1 class="setup-shell__title">"Set up LocalSky"</h1>
                <p class="setup-shell__subtitle">
                    "About five minutes. Leave any time; your progress is saved "
                    "on this device until you apply it at the end."
                </p>
                <ProgressStrip current=current_step/>
            </header>

            <Panel title="".to_string()>
                {move || if gate.get() {
                    view! {
                        <div class="setup-step">
                            <h2 class="setup-step__title">"This LocalSky is already set up"</h2>
                            <p class="setup-step__body">
                                "Walk the wizard again as an editor over your current "
                                "configuration, or start from a clean slate. Nothing is "
                                "written to disk until you finish on the Review step."
                            </p>
                            <div class="setup-reentry">
                                <Button
                                    variant="primary"
                                    disabled=Signal::derive(move || gate_busy.get())
                                    on_click=Callback::new(modify)
                                >
                                    {move || if gate_busy.get() { "Loading current setup…" } else { "Modify current setup" }}
                                </Button>
                                <Button
                                    variant="ghost"
                                    on_click=Callback::new(fresh)
                                >"Start fresh"</Button>
                                <Button variant="ghost" href="/settings">
                                    "Back to Settings"
                                </Button>
                            </div>
                            <p class="sensors-section__hint">
                                "Modify pre-fills every step from the live config (sources, "
                                "controllers, zones, the lot) so you can adjust one thing and "
                                "re-apply. Start fresh ignores the current config; applying at "
                                "the end replaces it (a snapshot of the old version is kept "
                                "for rollback)."
                            </p>
                        </div>
                    }.into_any()
                } else {
                    render_step(&current_step()).into_any()
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
    let idx = move || {
        STEPS
            .iter()
            .position(|(id, _, _)| *id == current())
            .unwrap_or(0)
    };
    view! {
        <div class="setup-progress" aria-label="Setup progress">
            <div class="setup-progress__meta">
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
            <ol class="setup-progress__dots">
                {STEPS.iter().enumerate().map(|(i, (id, label, optional))| {
                    let href = format!("/setup/{id}");
                    let opt = *optional;
                    view! {
                        <li>
                            <a
                                class="setup-progress__dot"
                                class:setup-progress__dot--optional=opt
                                class:setup-progress__dot--done=move || i < idx()
                                class:setup-progress__dot--current=move || i == idx()
                                href=href
                                title=*label
                                aria-label=format!("Step {}: {label}", i + 1)
                            ></a>
                        </li>
                    }
                }).collect_view()}
            </ol>
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
            {move || prev.get().map(|href| view! {
                <Button variant="ghost" href=href>"Back"</Button>
            })}
            {move || if can_leave.get() {
                view! { <Button variant="ghost" href="/".to_string()>"Save and finish later"</Button> }.into_any()
            } else {
                view! { <Button variant="ghost" disabled=true>"Save and finish later"</Button> }.into_any()
            }}
            {move || next.get().filter(|_| can_leave.get()).map(|href| view! {
                <Button variant="primary" href=href>"Next"</Button>
            })}
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
