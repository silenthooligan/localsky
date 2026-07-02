// LlmStep. Provider picker for the optional LLM advisor. None is a
// valid choice; the rest of LocalSky runs without an LLM. Choices are
// persisted into the wizard draft (config.llm) and can be probed live
// via POST /api/wizard/test_llm before finishing setup.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{Button, FormField, HelpHint, Panel, SecretInput, SegmentedControl};

#[cfg(feature = "hydrate")]
async fn fetch_draft() -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get("/api/wizard/draft")
        .send()
        .await
        .ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

#[cfg(feature = "hydrate")]
async fn save_draft(draft: serde_json::Value) -> Result<(), String> {
    let resp = gloo_net::http::Request::put("/api/wizard/draft")
        .json(&draft)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    Ok(())
}

/// Assemble the draft's `config.llm` value from the picker state.
/// `None` means "no LLM" (config.llm = null).
fn llm_json(
    provider: &str,
    base_url: &str,
    model: &str,
    api_key: &str,
) -> Option<serde_json::Value> {
    let base_url = base_url.trim();
    let model = model.trim();
    let api_key = api_key.trim();
    match provider {
        "none" => None,
        "auto" => Some(serde_json::json!({ "provider": "auto", "config": {} })),
        "ollama" => Some(serde_json::json!({
            "provider": "ollama",
            "config": {
                "base_url": if base_url.is_empty() { "http://localhost:11434" } else { base_url },
                "model": if model.is_empty() { "llama3.2:3b-instruct" } else { model },
            }
        })),
        "llamacpp" => Some(serde_json::json!({
            "provider": "llamacpp",
            "config": {
                "base_url": if base_url.is_empty() { "http://localhost:8080" } else { base_url },
                "model": if model.is_empty() { serde_json::Value::Null } else { model.into() },
            }
        })),
        "openai_compat" => Some(serde_json::json!({
            "provider": "openai_compat",
            "config": {
                "base_url": base_url,
                "model": model,
                "api_key": if api_key.is_empty() { serde_json::Value::Null } else { api_key.into() },
            }
        })),
        _ => None,
    }
}

#[component]
pub fn LlmStep() -> impl IntoView {
    // Default to "none": a user who clicks straight through the wizard must NOT
    // ship a live advisor. "auto" persists config.llm = { provider: "auto" },
    // which makes the running app probe localhost:11434/8080/1234 every tick and
    // never connect on a no-LLM box. The user opts in by picking a provider.
    let provider = RwSignal::new("none".to_string());
    let base_url = RwSignal::new(String::new());
    let model = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());

    let draft = RwSignal::new(serde_json::Value::Null);
    let loaded = RwSignal::new(false);
    let testing = RwSignal::new(false);
    let test_msg = RwSignal::new(String::new());
    let test_ok = RwSignal::new(false);

    // Seed the picker from the draft (config.llm), then mark loaded so the
    // persist effect can start writing changes back.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(d) = fetch_draft().await {
                let llm = d.get("config").and_then(|c| c.get("llm")).cloned();
                if let Some(llm) = llm.filter(|v| !v.is_null()) {
                    let p = llm
                        .get("provider")
                        .and_then(|v| v.as_str())
                        .unwrap_or("none")
                        .to_string();
                    let cfg = llm.get("config").cloned().unwrap_or_default();
                    base_url.set(
                        cfg.get("base_url")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    model.set(
                        cfg.get("model")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    api_key.set(
                        cfg.get("api_key")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    provider.set(p);
                }
                draft.set(d);
                loaded.set(true);
            }
        });
    });

    // Persist the current picker state into the draft. Runs on provider
    // switches (Effect below) and on text-field commit (on:change).
    let persist_now = move || {
        if !loaded.get_untracked() {
            return;
        }
        let llm = llm_json(
            &provider.get_untracked(),
            &base_url.get_untracked(),
            &model.get_untracked(),
            &api_key.get_untracked(),
        );
        let mut changed = false;
        draft.update(|d| {
            if let Some(cfg) = d.get_mut("config").and_then(|c| c.as_object_mut()) {
                let next = llm.clone().unwrap_or(serde_json::Value::Null);
                if cfg.get("llm") != Some(&next) {
                    cfg.insert("llm".into(), next);
                    changed = true;
                }
            }
        });
        if !changed {
            return;
        }
        let candidate = draft.get_untracked();
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = save_draft(candidate).await;
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = candidate;
    };

    // Provider switches persist immediately (segmented control).
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let _ = provider.get();
        persist_now();
    });

    let on_test = move |_| {
        if testing.get_untracked() {
            return;
        }
        persist_now();
        let llm = llm_json(
            &provider.get_untracked(),
            &base_url.get_untracked(),
            &model.get_untracked(),
            &api_key.get_untracked(),
        );
        let Some(llm) = llm else {
            test_ok.set(true);
            test_msg.set("No LLM selected; nothing to test.".into());
            return;
        };
        testing.set(true);
        test_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let result = async {
                let resp = gloo_net::http::Request::post("/api/wizard/test_llm")
                    .json(&serde_json::json!({ "llm": llm }))
                    .map_err(|e| e.to_string())?
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let v = resp
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| e.to_string())?;
                if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
                    let provider_id = v
                        .get("provider")
                        .and_then(|p| p.as_str())
                        .unwrap_or("provider")
                        .to_string();
                    let model_note = v
                        .get("model_loaded")
                        .and_then(|m| m.as_str())
                        .map(|m| format!(", model {m}"))
                        .unwrap_or_default();
                    Ok(format!("Connected to {provider_id}{model_note}"))
                } else {
                    Err(v
                        .get("detail")
                        .and_then(|d| d.as_str())
                        .unwrap_or("provider unreachable")
                        .to_string())
                }
            }
            .await;
            match result {
                Ok(msg) => {
                    test_ok.set(true);
                    test_msg.set(msg);
                }
                Err(e) => {
                    test_ok.set(false);
                    test_msg.set(format!("Test failed: {e}"));
                }
            }
            testing.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = llm;
    };

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
    let show_test = move || provider.get() != "none";

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"AI advisor "<span class="setup-step__optional">"optional"</span><HelpHint topic="llm"/></h2>
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
                        on:change=move |_| persist_now()
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
                        on:change=move |_| persist_now()
                    />
                </FormField>
            </Show>

            <Show when=show_key>
                <FormField
                    label="API key".to_string()
                    helptext="Required by OpenAI; leave blank for local Ollama / LM Studio.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <SecretInput
                        value=api_key
                        on_input=Callback::new(move |v: String| api_key.set(v))
                        on_change=Callback::new(move |_: String| persist_now())
                    />
                </FormField>
            </Show>

            <Show when=show_test>
                <div class="settings-form-actions" style="justify-content:flex-start">
                    <Button
                        variant="ghost"
                        disabled=Signal::derive(move || testing.get())
                        on_click=Callback::new(on_test)
                    >
                        {move || if testing.get() { "Testing…" } else { "Test provider" }}
                    </Button>
                </div>
            </Show>
            {move || {
                let m = test_msg.get();
                (!m.is_empty()).then(|| {
                    let cls = if test_ok.get() { "setup-test-result is-ok" } else { "setup-test-result is-err" };
                    view! { <p class=cls style="padding-left:0">{m}</p> }
                })
            }}

            <SetupFooter
                prev=prev_step_href("llm")
                next=next_step_href("llm")
            />
        </div>
    }
}
