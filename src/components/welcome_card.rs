// WelcomeCard. One-time "start here" card shown on the Weather home right
// after the setup wizard applies. The wizard writes
// localStorage("first_run_done") = "0" on success; this card shows while
// the flag is "0" and dismiss writes "1". SSR and the first hydrate frame
// render nothing (the flag is read in a deferred effect), so the DOM
// matches and tachys stays happy.

use leptos::prelude::*;

use crate::components::ui::Icon;

// Only the hydrate build touches localStorage; SSR renders nothing.
#[cfg(feature = "hydrate")]
const FLAG_KEY: &str = "first_run_done";

#[component]
pub fn WelcomeCard() -> impl IntoView {
    let show = RwSignal::new(false);

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    if let Ok(Some(v)) = storage.get_item(FLAG_KEY) {
                        if v == "0" {
                            show.set(true);
                        }
                    }
                }
            }
        });
    });

    let dismiss = move |_| {
        show.set(false);
        #[cfg(feature = "hydrate")]
        if let Some(win) = web_sys::window() {
            if let Ok(Some(storage)) = win.local_storage() {
                let _ = storage.set_item(FLAG_KEY, "1");
            }
        }
    };

    move || {
        if !show.get() {
            return ().into_any();
        }
        view! {
            <section class="welcome-card" aria-label="Getting started">
                <div class="welcome-card__head">
                    <h2 class="welcome-card__title">"Welcome to LocalSky"</h2>
                    <button
                        type="button"
                        class="welcome-card__dismiss"
                        aria-label="Dismiss welcome card"
                        on:click=dismiss
                    >
                        <Icon name="x" size=16/>
                    </button>
                </div>
                <p class="welcome-card__body">
                    "Setup is done and the engine is live. Three good first stops:"
                </p>
                <div class="welcome-card__links">
                    <a class="welcome-card__link" href="/sensors">
                        <Icon name="activity" size=18/>
                        <span class="welcome-card__link-text">
                            <strong>"Sensors hub"</strong>
                            <span>"confirm your stations are reporting"</span>
                        </span>
                    </a>
                    <a class="welcome-card__link" href="/zones">
                        <Icon name="zones" size=18/>
                        <span class="welcome-card__link-text">
                            <strong>"Zone canvas"</strong>
                            <span>"see every zone's live status"</span>
                        </span>
                    </a>
                    <a class="welcome-card__link" href="/simulator">
                        <Icon name="simulator" size=18/>
                        <span class="welcome-card__link-text">
                            <strong>"Simulator"</strong>
                            <span>"play what-if against the skip rules"</span>
                        </span>
                    </a>
                </div>
            </section>
        }
        .into_any()
    }
}
