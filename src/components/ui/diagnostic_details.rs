//! Compact, copyable server diagnostics. The API owns redaction and wording.
use leptos::prelude::*;

#[component]
pub fn DiagnosticDetails(record: serde_json::Value) -> impl IntoView {
    let code = record
        .pointer("/failure/code")
        .and_then(|v| v.as_str())
        .unwrap_or("Diagnostic")
        .to_owned();
    let text = serde_json::to_string_pretty(&record).unwrap_or_default();
    let clipboard_text = text.clone();
    let copy_label = RwSignal::new("Copy details");
    let copy = Callback::new(move |_| {
        #[cfg(feature = "hydrate")]
        {
            let text = clipboard_text.clone();
            wasm_bindgen_futures::spawn_local(async move {
                if let Some(window) = web_sys::window() {
                    if !window.is_secure_context() {
                        let _ = copy_label.try_set("Select and copy below");
                        return;
                    }
                    let result = wasm_bindgen_futures::JsFuture::from(
                        window.navigator().clipboard().write_text(&text),
                    )
                    .await;
                    let _ = copy_label.try_set(if result.is_ok() {
                        "Copied"
                    } else {
                        "Select and copy below"
                    });
                }
            });
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = &clipboard_text;
    });
    view! {
        <details class="diagnostic-details">
            <summary>{format!("Technical details · {code}")}</summary>
            <super::Button variant="secondary" size="sm" on_click=copy>{move || copy_label.get()}</super::Button>
            <pre tabindex="0">{text}</pre>
        </details>
    }
}
