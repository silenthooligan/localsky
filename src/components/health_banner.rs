// HealthBanner. Hydrate-only poll of /api/v1/health every 60s; when the
// overall status is degraded, a dismissable banner names the offline
// source(s) and links to the Sensors hub. Dismissing snoozes that exact
// source set for the session (a new failure re-raises the banner). SSR
// and the first hydrate frame render nothing, so the DOM matches.

use leptos::prelude::*;

use crate::components::ui::Icon;

#[component]
pub fn HealthBanner() -> impl IntoView {
    // The offline-source list driving the banner; empty = healthy.
    let offline: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    // Source set the user dismissed; compared as a joined key.
    let snoozed: RwSignal<String> = RwSignal::new(String::new());

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            loop {
                let next = async {
                    let resp = gloo_net::http::Request::get("/api/v1/health")
                        .send()
                        .await
                        .ok()?;
                    let v = resp.json::<serde_json::Value>().await.ok()?;
                    if v.get("status").and_then(|s| s.as_str()) != Some("degraded") {
                        return Some(Vec::new());
                    }
                    let list = v
                        .get("sources")
                        .and_then(|s| s.as_array())
                        .map(|arr| {
                            arr.iter()
                                .filter(|s| {
                                    s.get("enabled").and_then(|e| e.as_bool()) == Some(true)
                                        && s.get("status").and_then(|st| st.as_str())
                                            == Some("offline")
                                })
                                .filter_map(|s| s.get("id").and_then(|i| i.as_str()))
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    Some(list)
                }
                .await;
                if let Some(list) = next {
                    offline.set(list);
                }
                gloo_timers::future::TimeoutFuture::new(60_000).await;
            }
        });
    });

    let dismiss = move |_| {
        snoozed.set(offline.get_untracked().join(","));
    };

    move || {
        let list = offline.get();
        if list.is_empty() || snoozed.get() == list.join(",") {
            return ().into_any();
        }
        let label = if list.len() == 1 {
            format!("Weather source {} is offline", list[0])
        } else {
            format!(
                "{} weather sources are offline: {}",
                list.len(),
                list.join(", ")
            )
        };
        view! {
            <div class="health-banner" role="status">
                <span class="health-banner__icon" aria-hidden="true">
                    <Icon name="alert-triangle" size=16/>
                </span>
                <span class="health-banner__text">
                    {label}
                    ". The engine keeps deciding from the freshest data it has."
                </span>
                <a class="health-banner__link" href="/sensors">"Open Sensors hub"</a>
                <button
                    type="button"
                    class="health-banner__dismiss"
                    aria-label="Dismiss health warning"
                    on:click=dismiss
                >
                    <Icon name="x" size=14/>
                </button>
            </div>
        }
        .into_any()
    }
}
