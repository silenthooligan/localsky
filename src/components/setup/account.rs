// AccountStep. Create the owner account during first-run setup. The
// account posts to /api/auth/setup immediately (identity lives in
// SQLite, not the draft, so the password never touches the draft file);
// the session cookie comes back with the response, and the wizard apply
// then persists auth.mode = "required". Skipping leaves auth disabled,
// the right call behind a reverse proxy or on an isolated LAN.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{FormField, Panel};

#[component]
pub fn AccountStep() -> impl IntoView {
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let confirm = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let msg = RwSignal::new(String::new());
    let ok = RwSignal::new(false);
    // Whether an account already exists (created earlier this wizard run
    // or this is a re-run). Hydrate-only probe of /api/auth/status.
    let already = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Ok(resp) = gloo_net::http::Request::get("/api/auth/status")
                .send()
                .await
            {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    if v.get("setup_complete").and_then(|b| b.as_bool()) == Some(true) {
                        already.set(true);
                    }
                }
            }
        });
    });

    let pw_error = Signal::derive(move || {
        let p = password.get();
        let c = confirm.get();
        if !p.is_empty() && p.len() < 8 {
            Some("at least 8 characters".to_string())
        } else if !c.is_empty() && c != p {
            Some("passwords do not match".to_string())
        } else {
            None
        }
    });

    let on_create = move |_| {
        if busy.get_untracked() {
            return;
        }
        let u = username.get_untracked().trim().to_string();
        let p = password.get_untracked();
        if u.is_empty() || p.len() < 8 || p != confirm.get_untracked() {
            ok.set(false);
            msg.set("Pick a username and matching password (8+ characters).".into());
            return;
        }
        busy.set(true);
        msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let result = async {
                let resp = gloo_net::http::Request::post("/api/auth/setup")
                    .json(&serde_json::json!({ "username": u, "password": p }))
                    .map_err(|e| e.to_string())?
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if resp.ok() {
                    Ok(())
                } else {
                    let v = resp
                        .json::<serde_json::Value>()
                        .await
                        .unwrap_or(serde_json::Value::Null);
                    Err(v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("account creation failed")
                        .to_string())
                }
            }
            .await;
            match result {
                Ok(()) => {
                    ok.set(true);
                    already.set(true);
                    msg.set("Account created and you are signed in on this browser.".into());
                }
                Err(e) => {
                    ok.set(false);
                    msg.set(e);
                }
            }
            busy.set(false);
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = (u, p);
            busy.set(false);
        }
    };

    view! {
        <div class="setup-step">
            <h2 class="setup-step__title">"Owner account"</h2>
            <p class="setup-step__body">
                "Protect this LocalSky with a login. The account guards the UI and API; "
                "integrations like Home Assistant authenticate with an API token you create "
                "in Settings afterward. Skipping leaves the instance open, which is fine "
                "behind your own reverse-proxy login or on a trusted, isolated network."
            </p>

            {move || if already.get() {
                view! {
                    <Panel title="Account ready".to_string()>
                        <p class="setup-step__body" style="margin-bottom:0">
                            "An owner account exists and login will be required once setup "
                            "finishes. Manage it (and create API tokens for Home Assistant) "
                            "under Settings after the wizard."
                        </p>
                    </Panel>
                }.into_any()
            } else {
                view! {
                    <Panel title="Create the owner account".to_string()>
                        <FormField
                            label="Username".to_string()
                            helptext="Lowercased; this is the only account for now.".to_string()
                            error=Signal::derive(|| None::<String>)
                        >
                            <input
                                type="text"
                                class="ui-input"
                                autocomplete="username"
                                prop:value=move || username.get()
                                on:input=move |ev| username.set(event_target_value(&ev))
                            />
                        </FormField>
                        <FormField
                            label="Password".to_string()
                            helptext="8+ characters. Stored as an argon2id hash, never plaintext.".to_string()
                            error=pw_error
                        >
                            <input
                                type="password"
                                class="ui-input"
                                autocomplete="new-password"
                                prop:value=move || password.get()
                                on:input=move |ev| password.set(event_target_value(&ev))
                            />
                        </FormField>
                        <FormField
                            label="Confirm password".to_string()
                            helptext="".to_string()
                            error=Signal::derive(|| None::<String>)
                        >
                            <input
                                type="password"
                                class="ui-input"
                                autocomplete="new-password"
                                prop:value=move || confirm.get()
                                on:input=move |ev| confirm.set(event_target_value(&ev))
                            />
                        </FormField>
                        <div class="settings-form-actions" style="justify-content:flex-start">
                            <button
                                type="button"
                                class="setup-footer__btn setup-footer__btn--primary"
                                prop:disabled=move || busy.get()
                                on:click=on_create
                            >
                                {move || if busy.get() { "Creating…" } else { "Create account" }}
                            </button>
                        </div>
                    </Panel>
                }.into_any()
            }}

            {move || {
                let m = msg.get();
                (!m.is_empty()).then(|| {
                    let cls = if ok.get() { "setup-test-result is-ok" } else { "setup-test-result is-err" };
                    view! { <p class=cls style="padding-left:0">{m}</p> }
                })
            }}

            <SetupFooter
                prev=prev_step_href("account")
                next=next_step_href("account")
            />
        </div>
    }
}
