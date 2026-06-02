// ReviewStep. Final page of the wizard. Renders a summary of the draft
// and a primary "Save and finish" button that POSTs /api/wizard/apply.
// On success, redirects to /. On failure, surfaces the validation error.

use leptos::prelude::*;

use crate::components::setup::shell::{prev_step_href, SetupFooter};

#[component]
pub fn ReviewStep() -> impl IntoView {
    let applying = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    let on_apply = move |_| {
        applying.set(true);
        result_msg.set(String::new());
        #[cfg(feature = "hydrate")]
        {
            wasm_bindgen_futures::spawn_local(async move {
                match call_apply().await {
                    Ok(()) => {
                        result_ok.set(true);
                        result_msg
                            .set("Configuration saved. Redirecting to dashboard…".to_string());
                        if let Some(win) = web_sys::window() {
                            let _ = win.location().set_href("/");
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
            <h2 class="setup-step__title">"Review and apply"</h2>
            <p class="setup-step__body">
                "When you click apply, your settings save to "
                <code>"/data/localsky.toml"</code>" and the dashboard mounts. "
                "If validation fails, you'll get a specific error here and "
                "nothing changes on disk."
            </p>

            <div class="review-summary">
                <p class="review-summary__line">
                    "Settings will be saved to "
                    <code>"/data/localsky.toml"</code>
                    " and a snapshot recorded in the config history table "
                    "(retained for 20 versions; rollback via /api/config/rollback)."
                </p>
                <p class="review-summary__line">
                    "Once applied, this wizard becomes unavailable until you "
                    "delete the file. The /settings page is the editor "
                    "from that point on."
                </p>
            </div>

            <button
                type="button"
                class="setup-apply-btn"
                disabled=move || applying.get()
                on:click=on_apply
            >
                {move || if applying.get() { "Saving…" } else { "Save and finish" }}
            </button>

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

            <SetupFooter prev=prev_step_href("review") next=None/>
        </div>
    }
}

#[cfg(feature = "hydrate")]
async fn call_apply() -> Result<(), String> {
    use gloo_net::http::Request;
    match Request::post("/api/wizard/apply").send().await {
        Ok(r) if r.ok() => Ok(()),
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            Err(format!("Apply failed (HTTP {status}): {body}"))
        }
        Err(e) => Err(format!("Network error: {e}")),
    }
}
