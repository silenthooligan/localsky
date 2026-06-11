// Beta feedback affordance. A fixed pill (desktop bottom-right; above
// the tab bar on phones) that opens a small composer: pick Bug or Idea,
// describe it, and "Open on GitHub" builds a prefilled issue with the
// instance's diagnostic context (version, API, mode, route, viewport)
// appended. Nothing leaves the browser until the user submits the issue
// on GitHub themselves, which keeps the no-telemetry promise intact.

use leptos::prelude::*;

use crate::components::ui::Icon;
#[cfg(feature = "hydrate")]
use crate::docs::REPO_URL;

#[component]
pub fn BetaFeedback() -> impl IntoView {
    let open = RwSignal::new(false);
    let kind = RwSignal::new("bug".to_string());
    let text = RwSignal::new(String::new());
    // Diagnostic context, fetched once when the sheet first opens. Read
    // and written only inside hydrate-gated code; unused in SSR builds.
    #[allow(unused_variables)]
    let ctx = RwSignal::new(String::new());

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        if !open.get() || !ctx.get_untracked().is_empty() {
            return;
        }
        leptos::task::spawn_local(async move {
            let mut lines: Vec<String> = Vec::new();
            if let Ok(resp) = gloo_net::http::Request::get("/api/v1/info").send().await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    let g = |k: &str| {
                        v.get(k)
                            .and_then(|x| x.as_str().map(str::to_string).or(Some(x.to_string())))
                            .unwrap_or_default()
                    };
                    lines.push(format!("- LocalSky: {}", g("service_version")));
                    lines.push(format!("- API: {}", g("api_version")));
                    lines.push(format!("- Demo mode: {}", g("demo")));
                }
            }
            if let Some(win) = web_sys::window() {
                if let Ok(path) = win.location().pathname() {
                    lines.push(format!("- Page: {path}"));
                }
                lines.push(format!(
                    "- Viewport: {}x{}",
                    win.inner_width()
                        .ok()
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0) as i32,
                    win.inner_height()
                        .ok()
                        .and_then(|v| v.as_f64())
                        .unwrap_or(0.0) as i32,
                ));
                lines.push(format!(
                    "- Browser: {}",
                    win.navigator().user_agent().unwrap_or_default()
                ));
            }
            ctx.set(lines.join("\n"));
        });
    });

    let submit = move |_| {
        #[cfg(feature = "hydrate")]
        {
            let body_text = text.get_untracked();
            let is_bug = kind.get_untracked() == "bug";
            let title_seed = body_text
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(70)
                .collect::<String>();
            let title = if title_seed.is_empty() {
                if is_bug {
                    "Beta feedback: bug".to_string()
                } else {
                    "Beta feedback: idea".to_string()
                }
            } else {
                title_seed
            };
            let label = if is_bug { "bug" } else { "enhancement" };
            let body = format!(
                "{}\n\n---\n**Environment** (auto-filled by the in-app beta feedback button)\n{}\n",
                body_text,
                ctx.get_untracked()
            );
            let url = format!(
                "{REPO_URL}/issues/new?title={}&body={}&labels={label},beta-feedback",
                js_sys::encode_uri_component(&title),
                js_sys::encode_uri_component(&body),
            );
            if let Some(win) = web_sys::window() {
                let _ = win.open_with_url_and_target(&url, "_blank");
            }
            open.set(false);
            text.set(String::new());
        }
    };

    view! {
        <div class="beta-fb">
            <button
                type="button"
                class="beta-fb__pill"
                on:click=move |_| open.update(|o| *o = !*o)
                aria-expanded=move || open.get().to_string()
                aria-controls="beta-fb-sheet"
            >
                <Icon name="zap" size=14/>
                "Beta feedback"
            </button>

            {move || open.get().then(|| view! {
                <div class="beta-fb__sheet" id="beta-fb-sheet" role="dialog" aria-label="Send beta feedback">
                    <p class="beta-fb__title">"Help shape LocalSky"</p>
                    <p class="beta-fb__sub">
                        "This is a beta: rough edges are findable, and reports like "
                        "yours are how they get fixed."
                    </p>
                    <div class="beta-fb__kind" role="radiogroup" aria-label="Feedback type">
                        <button
                            type="button"
                            class="beta-fb__kind-btn"
                            class:is-active=move || kind.get() == "bug"
                            on:click=move |_| kind.set("bug".into())
                        >"Something broke"</button>
                        <button
                            type="button"
                            class="beta-fb__kind-btn"
                            class:is-active=move || kind.get() == "idea"
                            on:click=move |_| kind.set("idea".into())
                        >"I have an idea"</button>
                    </div>
                    <textarea
                        class="beta-fb__text"
                        rows="4"
                        placeholder="What happened, or what would make this better? First line becomes the title."
                        prop:value=move || text.get()
                        on:input=move |ev| text.set(event_target_value(&ev))
                    ></textarea>
                    <button type="button" class="beta-fb__send" on:click=submit>
                        <Icon name="external" size=14/>
                        "Open on GitHub"
                    </button>
                    <p class="beta-fb__privacy">
                        "Opens a prefilled GitHub issue with your version and page attached. "
                        "Nothing is sent until you submit it there."
                    </p>
                </div>
            })}
        </div>
    }
}
