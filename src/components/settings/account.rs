// SettingsAccount. Owner account + API tokens.
//
//   - No account yet: explain + create-account form (POST /api/auth/setup).
//     Creating one flips auth.mode to required (the API persists it).
//   - Account exists: show who is signed in, sign-out button, and the
//     API token manager: list (name/created/last-used), create with a
//     show-once reveal, revoke. Tokens are what HACS and any automation
//     paste as their Bearer credential.

use leptos::prelude::*;

use crate::components::settings_ui::SettingsResult;
use crate::components::ui::{Button, FormField, Icon, Panel, SkeletonRows};

#[cfg(feature = "hydrate")]
async fn fetch_json(url: &str) -> Option<serde_json::Value> {
    let resp = gloo_net::http::Request::get(url).send().await.ok()?;
    if !resp.ok() {
        return None;
    }
    resp.json::<serde_json::Value>().await.ok()
}

#[component]
pub fn SettingsAccount() -> impl IntoView {
    // None = loading; Some(v) = /api/auth/status body.
    let status: RwSignal<Option<serde_json::Value>> = RwSignal::new(None);
    let session: RwSignal<serde_json::Value> = RwSignal::new(serde_json::Value::Null);
    let tokens: RwSignal<Vec<serde_json::Value>> = RwSignal::new(Vec::new());
    let reload = RwSignal::new(0u32);

    // Create-account form state.
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);

    // Token-create state. revealed = the show-once plaintext.
    let token_name = RwSignal::new(String::new());
    let revealed: RwSignal<Option<String>> = RwSignal::new(None);
    // reload only drives the hydrate fetch effect.
    #[cfg(not(feature = "hydrate"))]
    let _ = &reload;

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let _ = reload.get();
        leptos::task::spawn_local(async move {
            if let Some(v) = fetch_json("/api/auth/status").await {
                status.set(Some(v));
            }
            if let Some(v) = fetch_json("/api/auth/session").await {
                session.set(v);
            }
            if let Some(v) = fetch_json("/api/auth/tokens").await {
                tokens.set(
                    v.get("tokens")
                        .and_then(|t| t.as_array())
                        .cloned()
                        .unwrap_or_default(),
                );
            }
        });
    });

    let on_create_account = move |_| {
        if busy.get_untracked() {
            return;
        }
        let u = username.get_untracked().trim().to_string();
        let p = password.get_untracked();
        busy.set(true);
        result_msg.set(String::new());
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
                    crate::components::ui::use_toast()
                        .success("Account created. Login is now required.");
                    reload.update(|n| *n += 1);
                }
                Err(e) => {
                    result_ok.set(false);
                    result_msg.set(e);
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

    let on_logout = move |_| {
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let _ = gloo_net::http::Request::post("/api/auth/logout")
                .send()
                .await;
            if let Some(win) = web_sys::window() {
                let _ = win.location().set_href("/login");
            }
        });
    };

    let on_create_token = move |_| {
        let name = token_name.get_untracked().trim().to_string();
        if name.is_empty() {
            result_ok.set(false);
            result_msg.set("Give the token a name (e.g. home-assistant).".into());
            return;
        }
        #[cfg(feature = "hydrate")]
        leptos::task::spawn_local(async move {
            let result = async {
                let resp = gloo_net::http::Request::post("/api/auth/tokens")
                    .json(&serde_json::json!({ "name": name }))
                    .map_err(|e| e.to_string())?
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let v = resp
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| e.to_string())?;
                if resp.ok() {
                    Ok(v.get("token")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string())
                } else {
                    Err(v
                        .get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("token creation failed")
                        .to_string())
                }
            }
            .await;
            match result {
                Ok(tok) => {
                    revealed.set(Some(tok));
                    token_name.set(String::new());
                    reload.update(|n| *n += 1);
                }
                Err(e) => {
                    result_ok.set(false);
                    result_msg.set(e);
                }
            }
        });
        #[cfg(not(feature = "hydrate"))]
        let _ = name;
    };

    let copy_revealed = move |_| {
        #[cfg(feature = "hydrate")]
        if let Some(tok) = revealed.get_untracked() {
            if let Some(win) = web_sys::window() {
                let _ = win.navigator().clipboard().write_text(&tok);
                crate::components::ui::use_toast().success("Token copied to clipboard.");
            }
        }
    };

    let revoke = move |id: i64| {
        #[cfg(feature = "hydrate")]
        {
            // Revoking cuts off whatever is using the token immediately
            // and can't be undone, so gate it on an explicit confirm.
            let confirmed = web_sys::window()
                .map(|w| {
                    w.confirm_with_message(
                        "Revoke this token? Anything still using it (the Home \
                         Assistant add-on, scripts) loses access immediately.",
                    )
                    .unwrap_or(false)
                })
                .unwrap_or(false);
            if !confirmed {
                return;
            }
            leptos::task::spawn_local(async move {
                let result = async {
                    let resp = gloo_net::http::Request::delete(&format!("/api/auth/tokens/{id}"))
                        .send()
                        .await
                        .map_err(|e| e.to_string())?;
                    if resp.ok() {
                        Ok(())
                    } else {
                        Err(format!("HTTP {}", resp.status()))
                    }
                }
                .await;
                match result {
                    Ok(()) => {
                        crate::components::ui::use_toast().success("Token revoked.");
                        reload.update(|n| *n += 1);
                    }
                    Err(e) => {
                        crate::components::ui::use_toast().error(format!("Revoke failed: {e}"));
                    }
                }
            });
        }
        #[cfg(not(feature = "hydrate"))]
        let _ = id;
    };

    view! {
        <div class="settings-section">
            <header class="settings-section__head">
                <h2 class="settings-section__title">"Account and API tokens"</h2>
                <p class="settings-section__sub">
                    "The owner account protects the UI and API. Integrations (the Home "
                    "Assistant add-on, scripts) authenticate with API tokens sent as "
                    <code>"Authorization: Bearer"</code>"."
                </p>
            </header>

            {move || match status.get() {
                None => view! { <SkeletonRows count=3/> }.into_any(),
                Some(st) => {
                    let setup_complete = st.get("setup_complete").and_then(|b| b.as_bool()) == Some(true);
                    if !setup_complete {
                        view! {
                            <Panel title="Protect this LocalSky".to_string()>
                                <p class="sensors-section__hint">
                                    "No account exists yet, so the instance is open to anyone who can "
                                    "reach it. Create the owner account to require sign-in. (Skip this "
                                    "if a reverse proxy already guards access.)"
                                </p>
                                <FormField
                                    label="Username".to_string()
                                    helptext="".to_string()
                                    error=Signal::derive(|| None::<String>)
                                >
                                    <input type="text" class="ui-input" autocomplete="username"
                                        prop:value=move || username.get()
                                        on:input=move |ev| username.set(event_target_value(&ev))/>
                                </FormField>
                                <FormField
                                    label="Password".to_string()
                                    helptext="8+ characters.".to_string()
                                    error=Signal::derive(|| None::<String>)
                                >
                                    <input type="password" class="ui-input" autocomplete="new-password"
                                        prop:value=move || password.get()
                                        on:input=move |ev| password.set(event_target_value(&ev))/>
                                </FormField>
                                <div class="settings-form-actions" style="justify-content:flex-start">
                                    <Button
                                        variant="primary"
                                        loading=Signal::derive(move || busy.get())
                                        on_click=Callback::new(on_create_account)
                                    >"Create owner account"</Button>
                                </div>
                            </Panel>
                        }.into_any()
                    } else {
                        let who = session.get();
                        let username_label = who
                            .get("user")
                            .and_then(|u| u.get("username"))
                            .and_then(|u| u.as_str())
                            .map(|u| format!("Signed in as {u}"))
                            .unwrap_or_else(|| "Login required on this instance".to_string());
                        view! {
                            <Panel title="Owner".to_string()>
                                <div class="account-row">
                                    <span class="account-row__who">
                                        <Icon name="check" size=16/>
                                        {username_label}
                                    </span>
                                    <Button variant="ghost" on_click=Callback::new(on_logout)>"Sign out"</Button>
                                </div>
                            </Panel>
                        }.into_any()
                    }
                }
            }}

            <Panel title="API tokens".to_string()>
                {move || revealed.get().map(|tok| view! {
                    <div class="token-reveal" role="status">
                        <p class="token-reveal__note">
                            "Copy this token now; it is shown exactly once."
                        </p>
                        <code class="token-reveal__value">{tok}</code>
                        <Button variant="secondary" size="sm" on_click=Callback::new(copy_revealed)>"Copy"</Button>
                    </div>
                })}

                <div class="token-create">
                    <input
                        type="text"
                        class="ui-input"
                        placeholder="Token name (e.g. home-assistant)"
                        prop:value=move || token_name.get()
                        on:input=move |ev| token_name.set(event_target_value(&ev))
                    />
                    <Button variant="primary" on_click=Callback::new(on_create_token)>"Create token"</Button>
                </div>

                <ul class="token-list">
                    {move || {
                        let list = tokens.get();
                        if list.is_empty() {
                            return view! {
                                <li class="sensors-section__hint">"No tokens yet. Home Assistant needs one to connect when login is required."</li>
                            }.into_any();
                        }
                        list.into_iter().map(|t| {
                            let id = t.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
                            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string();
                            let last = t.get("last_used_at").and_then(|v| v.as_i64());
                            let last_label = match last {
                                Some(epoch) if epoch > 0 => {
                                    use chrono::TimeZone;
                                    chrono::Local
                                        .timestamp_opt(epoch, 0)
                                        .single()
                                        .map(|d| format!("last used {}", d.format("%b %-d, %-I:%M %p")))
                                        .unwrap_or_else(|| "used".into())
                                }
                                _ => "never used".into(),
                            };
                            view! {
                                <li class="token-list__row">
                                    <span class="token-list__name">{name}</span>
                                    <span class="token-list__meta">{last_label}</span>
                                    <button
                                        type="button"
                                        class="token-list__revoke"
                                        on:click=move |_| revoke(id)
                                    >"Revoke"</button>
                                </li>
                            }
                        }).collect_view().into_any()
                    }}
                </ul>
            </Panel>

            <SettingsResult result_msg result_ok/>
        </div>
    }
}
