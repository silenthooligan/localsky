// Top nav with claymorphic pill tabs. Reads the current location from
// leptos_router and adds an `is-on` modifier to the active tab so CSS
// can light up the selected one.
//
// Each tab is a plain <a href> so default-action fallback works if
// the WASM intercept fails to attach, AND an inline on:click that
// calls leptos_router::use_navigate. The handler shape is the
// minimal pattern from the leptos book — a single closure per link
// capturing its own navigate clone directly, no helper indirection.
//
// Diagnostic step recorded into the global "navlog" debug signal
// (provide_context'd in app.rs and rendered in a fixed strip at
// the bottom of the page) so any failure point — handler firing,
// prevent_default, navigate call, route swap — is visible without
// needing browser dev tools, which are awkward to use on mobile.

use crate::nav_log::log_nav;
use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

#[component]
pub fn TopNav() -> impl IntoView {
    let loc = use_location();
    let weather_class = move || {
        if loc.pathname.get().starts_with("/irrigation") {
            "top-nav-tab".to_string()
        } else {
            "top-nav-tab is-on".to_string()
        }
    };
    let irrigation_class = move || {
        if loc.pathname.get().starts_with("/irrigation") {
            "top-nav-tab is-on".to_string()
        } else {
            "top-nav-tab".to_string()
        }
    };

    let navigate = use_navigate();

    let n1 = navigate.clone();
    let on_weather = move |ev: leptos::ev::MouseEvent| {
        log_nav("click weather");
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 {
            log_nav("modifier click, falling through to href");
            return;
        }
        ev.prevent_default();
        log_nav("calling navigate(/)");
        n1("/", NavigateOptions::default());
        log_nav("navigate(/) returned");
    };

    let n2 = navigate;
    let on_irrigation = move |ev: leptos::ev::MouseEvent| {
        log_nav("click irrigation");
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 {
            log_nav("modifier click, falling through to href");
            return;
        }
        ev.prevent_default();
        log_nav("calling navigate(/irrigation)");
        n2("/irrigation", NavigateOptions::default());
        log_nav("navigate(/irrigation) returned");
    };

    view! {
        <header class="header">
            <div class="header-brand">
                <span class="bolt">"⚡"</span>
                <span>"LocalSky"</span>
            </div>

            <nav class="top-nav" aria-label="Primary">
                <a href="/" class=weather_class on:click=on_weather>
                    <span aria-hidden="true">"☁︎"</span>
                    <span class="top-nav-label">"Weather"</span>
                </a>
                <a href="/irrigation" class=irrigation_class on:click=on_irrigation>
                    <span aria-hidden="true">"💧"</span>
                    <span class="top-nav-label">"Irrigation"</span>
                </a>
            </nav>
        </header>
    }
}
