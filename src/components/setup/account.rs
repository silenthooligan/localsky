// AccountStep. Create the owner account during first-run setup. The
// account posts to /api/auth/setup immediately (identity lives in
// SQLite, not the draft, so the password never touches the draft file);
// the session cookie comes back with the response, and the wizard apply
// then persists auth.mode = "required". Skipping leaves auth disabled,
// the right call behind a reverse proxy or on an isolated LAN.

use leptos::prelude::*;

use crate::components::setup::shell::{next_step_href, prev_step_href, SetupFooter};
use crate::components::ui::{Button, FormField, Panel, SecretInput};

#[component]
pub fn AccountStep() -> impl IntoView {
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let confirm = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let msg = RwSignal::new(String::new());
    let ok = RwSignal::new(false);
    // The deciding access-context question. The step used to be skippable with
    // no gate right after the user was told they may reach LocalSky off-LAN;
    // answering this once makes the no-auth outcome a conscious choice instead
    // of a silent default. "" = unanswered, "external" = reaches it off-LAN
    // (recommend a login), "lan" = LAN-only behind their own router/proxy
    // (no-auth confirmed). Local-only: it just steers the guidance below.
    let access = RwSignal::new(String::new());
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
            <h2 class="setup-step__title">"Protect this LocalSky "<span class="setup-step__optional">"optional"</span></h2>
            <p class="setup-step__body">
                "Protect this LocalSky with a login. The account guards the UI and API; "
                "integrations like Home Assistant authenticate with an API token you create "
                "in Settings afterward. Skipping is the right call for LAN-only use behind "
                "your own router, or when a reverse proxy already handles the login."
            </p>
            <p class="setup-step__body setup-step__nudge">
                "Create the login if you will reach LocalSky from outside your home network, "
                "for example a phone on cellular or a public URL. LocalSky logs a best-effort "
                "warning when it spots a request that looks internet-facing while no login is set, "
                "but your firewall or reverse proxy is the real boundary -- treat the login as the "
                "lock, not the warning."
            </p>

            // Deciding question, asked once. Until it's answered the step gives
            // no skippable "no login" outcome, so a user can't breeze past the
            // access decision they were just told matters.
            {move || (!already.get()).then(|| view! {
                <Panel title="How will you reach LocalSky?".to_string()>
                    <div class="setup-access-choice" role="radiogroup" aria-label="How will you reach LocalSky?">
                        <label style="display:flex; gap:0.5rem; align-items:flex-start; min-height:44px">
                            <input
                                type="radio"
                                name="access-context"
                                prop:checked=move || access.get() == "external"
                                on:input=move |_| access.set("external".into())
                            />
                            <span>
                                <strong>"From outside my home network"</strong>
                                <span class="setup-step__hint" style="display:block">
                                    "A phone on cellular, a public URL, or through a tunnel. A login is strongly recommended."
                                </span>
                            </span>
                        </label>
                        <label style="display:flex; gap:0.5rem; align-items:flex-start; min-height:44px">
                            <input
                                type="radio"
                                name="access-context"
                                prop:checked=move || access.get() == "lan"
                                on:input=move |_| access.set("lan".into())
                            />
                            <span>
                                <strong>"Only on my home network"</strong>
                                <span class="setup-step__hint" style="display:block">
                                    "LAN only, behind your own router (or a reverse proxy that already handles login). No login needed here."
                                </span>
                            </span>
                        </label>
                    </div>
                </Panel>
            })}

            // LAN-only answer: confirm the no-auth outcome explicitly, so
            // skipping is a stated decision, not an unmarked default.
            {move || (!already.get() && access.get() == "lan").then(|| view! {
                <p class="setup-test-result is-ok" style="padding-left:0">
                    "Got it: no login needed. Skip this step (use Next), or add one anyway below. "
                    "You can always set a login later in Settings if that changes."
                </p>
            })}

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
            } else if access.get().is_empty() {
                // Wait for the access answer before offering the form, so the
                // decision is made first (no unmarked skip).
                ().into_any()
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
                            <SecretInput
                                value=password
                                autocomplete="new-password"
                                on_input=Callback::new(move |v: String| password.set(v))
                            />
                        </FormField>
                        <FormField
                            label="Confirm password".to_string()
                            helptext="".to_string()
                            error=Signal::derive(|| None::<String>)
                        >
                            <SecretInput
                                value=confirm
                                autocomplete="new-password"
                                on_input=Callback::new(move |v: String| confirm.set(v))
                            />
                        </FormField>
                        <div class="settings-form-actions" style="justify-content:flex-start">
                            <Button
                                variant="primary"
                                disabled=Signal::derive(move || busy.get())
                                on_click=Callback::new(on_create)
                            >
                                {move || if busy.get() { "Creating…" } else { "Create account" }}
                            </Button>
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
