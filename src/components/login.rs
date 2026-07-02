// /login. Centered sign-in card. The auth middleware 302s
// unauthenticated HTML requests here; on success we replace-navigate to /
// (location.replace, not assign) so every stream and fetch restarts with
// the session cookie attached AND the login card is dropped from the
// history stack, so a back gesture can never return to it.
// When no account exists yet (auth enabled by hand, or DB wiped), the
// card flips to first-account creation against /api/auth/setup.

use leptos::prelude::*;

use crate::components::ui::{Button, FormField, Icon, SecretInput};

#[component]
pub fn LoginPage() -> impl IntoView {
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let msg = RwSignal::new(String::new());
    // None = probing, Some(true) = login form, Some(false) = create-account.
    let has_account: RwSignal<Option<bool>> = RwSignal::new(None);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            let setup_complete = async {
                let resp = gloo_net::http::Request::get("/api/auth/status")
                    .send()
                    .await
                    .ok()?;
                let v = resp.json::<serde_json::Value>().await.ok()?;
                v.get("setup_complete").and_then(|b| b.as_bool())
            }
            .await
            .unwrap_or(true);
            has_account.set(Some(setup_complete));
        });
    });

    let submit = move |_| {
        if busy.get_untracked() {
            return;
        }
        let u = username.get_untracked().trim().to_string();
        let p = password.get_untracked();
        if u.is_empty() || p.is_empty() {
            msg.set("Username and password are required.".into());
            return;
        }
        let creating = has_account.get_untracked() == Some(false);
        busy.set(true);
        msg.set(String::new());
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let endpoint = if creating {
                "/api/auth/setup"
            } else {
                "/api/auth/login"
            };
            let result = async {
                let resp = gloo_net::http::Request::post(endpoint)
                    .json(&serde_json::json!({ "username": u, "password": p }))
                    .map_err(|e| e.to_string())?
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                if resp.ok() {
                    Ok(())
                } else if resp.status() == 401 {
                    Err("Invalid username or password.".into())
                } else if resp.status() == 429 {
                    Err("Too many attempts; wait a minute.".into())
                } else {
                    let v = resp
                        .json::<serde_json::Value>()
                        .await
                        .unwrap_or(serde_json::Value::Null);
                    Err(v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("sign-in failed")
                        .to_string())
                }
            }
            .await;
            match result {
                Ok(()) => {
                    if let Some(win) = web_sys::window() {
                        let _ = win.location().replace(&crate::base::url("/"));
                    }
                }
                Err(e) => {
                    msg.set(e);
                    busy.set(false);
                }
            }
        });
        #[cfg(not(feature = "hydrate"))]
        {
            let _ = (u, p, creating);
            busy.set(false);
        }
    };

    let on_keydown = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Enter" {
            submit(());
        }
    };

    view! {
        <div class="login-page">
            <div class="login-card" on:keydown=on_keydown>
                <div class="login-card__brand" aria-hidden="true">
                    <img src="/brand-mark.svg" alt="" width="44" height="44"/>
                    <span>
                        <span class="header-brand__local">"LOCAL"</span>
                        <span class="header-brand__sky">"SKY"</span>
                    </span>
                </div>
                {move || match has_account.get() {
                    None => view! { <crate::components::ui::SkeletonRows count=3/> }.into_any(),
                    Some(exists) => view! {
                        <h1 class="login-card__title">
                            {if exists { "Sign in" } else { "Create the owner account" }}
                        </h1>
                        <FormField
                            label="Username".to_string()
                            helptext="".to_string()
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
                            helptext="".to_string()
                            error=Signal::derive(|| None::<String>)
                        >
                            <SecretInput
                                value=password
                                autocomplete=if exists { "current-password" } else { "new-password" }
                                on_input=Callback::new(move |v: String| password.set(v))
                            />
                        </FormField>
                        <Button
                            variant="primary"
                            class="setup-apply-btn login-card__submit"
                            disabled=Signal::derive(move || busy.get())
                            loading=Signal::derive(move || busy.get())
                            on_click=Callback::new(move |_| submit(()))
                        >
                            {move || if busy.get() { "Signing in…" } else if exists { "Sign in" } else { "Create and sign in" }}
                        </Button>
                    }.into_any(),
                }}
                {move || {
                    let m = msg.get();
                    (!m.is_empty()).then(|| view! {
                        <p class="login-card__error" role="alert">
                            <Icon name="alert-triangle" size=14/>
                            {m}
                        </p>
                    })
                }}
            </div>
        </div>
    }
}
