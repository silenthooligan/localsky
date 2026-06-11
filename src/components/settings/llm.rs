// SettingsLlm. Edit cfg.llm. Loads + saves via /api/config.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{FormField, Panel, SegmentedControl};

#[component]
pub fn SettingsLlm() -> impl IntoView {
    let provider = RwSignal::new("auto".to_string());
    let base_url = RwSignal::new(String::new());
    let model = RwSignal::new(String::new());
    let api_key = RwSignal::new(String::new());
    let timeout_s = RwSignal::new(20u32);

    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(d) = fetch_llm().await {
                    provider.set(d.provider);
                    base_url.set(d.base_url);
                    model.set(d.model);
                    api_key.set(d.api_key);
                    timeout_s.set(d.timeout_s);
                }
            });
        });
    }

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

    let on_save = move |_| {
        if saving.get() {
            return;
        }
        saving.set(true);
        result_msg.set(String::new());
        let payload = LlmDraft {
            provider: provider.get(),
            base_url: base_url.get(),
            model: model.get(),
            api_key: api_key.get(),
            timeout_s: timeout_s.get(),
        };
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match save_llm(payload).await {
                    Ok(()) => {
                        crate::components::settings_ui::toast_saved(
                            result_msg,
                            result_ok,
                            "Saved. Advisor reconnects on next call.",
                        );
                    }
                    Err(e) => {
                        result_ok.set(false);
                        result_msg.set(e);
                    }
                }
                saving.set(false);
            });
        }
        #[cfg(not(feature = "hydrate"))]
        {
            saving.set(false);
            let _ = payload;
        }
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"LLM advisor"</h1>
                <p class="settings-page__subtitle">
                    "The advisor produces plain-English explanations of "
                    "today's verdict. The deterministic skip-rule engine "
                    "owns every irrigation decision; the LLM is surface "
                    "content only and never gates safety."
                </p>
            </header>

            <Panel title="Provider".to_string() help_topic="llm">
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
                <Panel title="Endpoint".to_string() help_topic="llm">
                    <div class="grid settings-field-grid">
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

                        <Show when=show_model>
                            <FormField
                                label="Model".to_string()
                                helptext="e.g. llama3.2:3b-instruct, gpt-4o-mini".to_string()
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
                                helptext="Required by OpenAI; leave blank for local providers.".to_string()
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
                    </div>
                </Panel>
            </Show>

            <div class="settings-actions">
                <button
                    type="button"
                    class="setup-footer__btn setup-footer__btn--primary"
                    disabled=move || saving.get()
                    on:click=on_save
                >
                    {move || if saving.get() { "Saving…" } else { "Save changes" }}
                </button>
            </div>

            <SettingsResult result_msg=result_msg result_ok=result_ok/>
        </main>
    }
}

#[derive(Clone, Default)]
#[allow(dead_code)]
struct LlmDraft {
    provider: String,
    base_url: String,
    model: String,
    api_key: String,
    timeout_s: u32,
}

#[cfg(feature = "hydrate")]
async fn fetch_llm() -> Result<LlmDraft, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let val: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let llm = val.get("llm").cloned().unwrap_or(serde_json::Value::Null);
    let provider = llm
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("auto")
        .to_string();
    let config = llm
        .get("config")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Ok(LlmDraft {
        provider,
        base_url: config
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        model: config
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        api_key: config
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        timeout_s: llm.get("timeout_s").and_then(|v| v.as_u64()).unwrap_or(20) as u32,
    })
}

#[cfg(feature = "hydrate")]
async fn save_llm(d: LlmDraft) -> Result<(), String> {
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;
    let config_block = match d.provider.as_str() {
        "auto" => serde_json::json!({ "probe_order": [] }),
        "ollama" => serde_json::json!({
            "base_url": d.base_url,
            "model": d.model,
        }),
        "llamacpp" => serde_json::json!({
            "base_url": d.base_url,
            "model": if d.model.is_empty() { serde_json::Value::Null } else { serde_json::json!(d.model) },
        }),
        "openai_compat" => serde_json::json!({
            "base_url": d.base_url,
            "model": d.model,
            "api_key": if d.api_key.is_empty() { serde_json::Value::Null } else { serde_json::json!(d.api_key) },
        }),
        _ => serde_json::Value::Null,
    };
    if d.provider == "none" {
        cfg["llm"] = serde_json::Value::Null;
    } else {
        cfg["llm"] = serde_json::json!({
            "provider": d.provider,
            "config": config_block,
            "timeout_s": d.timeout_s,
            "explanation_ttl_s": 300,
            "anomaly_ttl_s": 3600,
        });
    }
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {body}", resp.status()));
    }
    Ok(())
}
