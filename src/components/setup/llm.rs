// LlmStep. Provider picker for the optional LLM advisor. None is a
// valid choice; the rest of LocalSky runs without an LLM.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{FormField, Panel, SegmentedControl};

#[component]
pub fn LlmStep() -> impl IntoView {
    let provider = RwSignal::new("auto".to_string());
    let base_url = RwSignal::new(String::new());
    let model = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());

    let show_url = move || {
        matches!(
            provider.get().as_str(),
            "openai_compat" | "ollama" | "llamacpp"
        )
    };
    let show_model = move || {
        matches!(
            provider.get().as_str(),
            "ollama" | "openai_compat" | "llamacpp"
        )
    };
    let show_key = move || provider.get() == "openai_compat";

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"LLM advisor (optional)"</h2>
            <p class="setup-step__body">
                "LocalSky can call an LLM to explain today's verdict in plain "
                "English and flag anomalies in the snapshot. The deterministic "
                "skip-rule engine owns every irrigation decision; the LLM is "
                "surface content only and never gates safety. Pick "
                <strong>"Auto"</strong>
                " to have LocalSky probe localhost on boot for Ollama / "
                "llama.cpp / LM Studio."
            </p>

            <Panel title="Provider".to_string()>
                <SegmentedControl
                    value=provider
                    options=vec![
                        ("auto".into(), "Auto".into()),
                        ("ollama".into(), "Ollama".into()),
                        ("llamacpp".into(), "llama.cpp".into()),
                        ("openai_compat".into(), "OpenAI-compatible".into()),
                        ("none".into(), "None".into()),
                    ]
                    aria_label="LLM provider".to_string()
                />
            </Panel>

            <Show when=show_url>
                <FormField
                    label="Base URL".to_string()
                    helptext="e.g. http://localhost:11434 (Ollama), http://localhost:8080 (llama.cpp), https://api.openai.com".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="url"
                        class="ui-input"
                        prop:value=move || base_url.get()
                        on:input=move |ev| base_url.set(event_target_value(&ev))
                    />
                </FormField>
            </Show>

            <Show when=show_model>
                <FormField
                    label="Model".to_string()
                    helptext="e.g. llama3.2:3b-instruct (Ollama), gpt-4o-mini (OpenAI)".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="text"
                        class="ui-input"
                        prop:value=move || model.get()
                        on:input=move |ev| model.set(event_target_value(&ev))
                    />
                </FormField>
            </Show>

            <Show when=show_key>
                <FormField
                    label="API key".to_string()
                    helptext="Required by OpenAI; leave blank for local Ollama / LM Studio.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="password"
                        class="ui-input"
                        prop:value=move || api_key.get()
                        on:input=move |ev| api_key.set(event_target_value(&ev))
                    />
                </FormField>
            </Show>

            <SetupFooter
                prev=prev_step_href("llm")
                next=next_step_href("llm")
            />
        </div>
    }
}
