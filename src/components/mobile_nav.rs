// Bottom-tab nav for the mobile shell. Always rendered; hidden at desktop
// widths via SCSS (.mobile-tab-bar { display:none } default, switched to
// flex at @media (max-width: 760px)).
//
// Four tabs:
//   Weather  -> /
//   Now      -> /irrigation                 (overview / next-run / skip verdict)
//   Zones    -> /irrigation?tab=zones       (per-zone list, tap into detail)
//   Schedule -> /irrigation?tab=schedule    (verdict strip, history, settings)
//
// Sub-tabs share the /irrigation route and dispatch via a `tab` query param
// rather than mirror routes. That preserves bookmarks across viewport changes
// and avoids splitting the route table.
//
// Per-tab closures follow the pattern in src/components/nav.rs: one
// classname closure + one click handler per tab, each with its own
// cloned use_navigate. nav_log entries on every step so the in-page
// debug strip shows exactly what happened.

use crate::nav_log::log_nav;
use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

fn current_tab(path: &str, tab_query: Option<String>) -> &'static str {
    if path == "/" {
        return "weather";
    }
    if path.starts_with("/irrigation") {
        match tab_query.as_deref() {
            Some("zones") => "zones",
            Some("schedule") => "schedule",
            _ => "now",
        }
    } else {
        "weather"
    }
}

#[component]
pub fn MobileNav() -> impl IntoView {
    let loc = use_location();

    let l1 = loc.clone();
    let cls_weather = move || {
        let t = current_tab(&l1.pathname.get(), l1.query.get().get("tab").map(|s| s.to_string()));
        if t == "weather" { "mobile-tab is-on" } else { "mobile-tab" }
    };
    let l2 = loc.clone();
    let cls_now = move || {
        let t = current_tab(&l2.pathname.get(), l2.query.get().get("tab").map(|s| s.to_string()));
        if t == "now" { "mobile-tab is-on" } else { "mobile-tab" }
    };
    let l3 = loc.clone();
    let cls_zones = move || {
        let t = current_tab(&l3.pathname.get(), l3.query.get().get("tab").map(|s| s.to_string()));
        if t == "zones" { "mobile-tab is-on" } else { "mobile-tab" }
    };
    let l4 = loc;
    let cls_schedule = move || {
        let t = current_tab(&l4.pathname.get(), l4.query.get().get("tab").map(|s| s.to_string()));
        if t == "schedule" { "mobile-tab is-on" } else { "mobile-tab" }
    };

    let navigate = use_navigate();

    let n1 = navigate.clone();
    let on_weather = move |ev: leptos::ev::MouseEvent| {
        log_nav("mobile-tab click: weather");
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 { return; }
        ev.prevent_default();
        n1("/", NavigateOptions::default());
    };

    let n2 = navigate.clone();
    let on_now = move |ev: leptos::ev::MouseEvent| {
        log_nav("mobile-tab click: now");
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 { return; }
        ev.prevent_default();
        n2("/irrigation", NavigateOptions::default());
    };

    let n3 = navigate.clone();
    let on_zones = move |ev: leptos::ev::MouseEvent| {
        log_nav("mobile-tab click: zones");
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 { return; }
        ev.prevent_default();
        n3("/irrigation?tab=zones", NavigateOptions::default());
    };

    let n4 = navigate;
    let on_schedule = move |ev: leptos::ev::MouseEvent| {
        log_nav("mobile-tab click: schedule");
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 { return; }
        ev.prevent_default();
        n4("/irrigation?tab=schedule", NavigateOptions::default());
    };

    view! {
        <nav class="mobile-tab-bar" aria-label="Primary mobile">
            <a href="/" class=cls_weather on:click=on_weather>
                <span class="mobile-tab-glyph" aria-hidden="true">"☁︎"</span>
                <span class="mobile-tab-label">"Weather"</span>
            </a>
            <a href="/irrigation" class=cls_now on:click=on_now>
                <span class="mobile-tab-glyph" aria-hidden="true">"💧"</span>
                <span class="mobile-tab-label">"Now"</span>
            </a>
            <a href="/irrigation?tab=zones" class=cls_zones on:click=on_zones>
                <span class="mobile-tab-glyph" aria-hidden="true">"▦"</span>
                <span class="mobile-tab-label">"Zones"</span>
            </a>
            <a href="/irrigation?tab=schedule" class=cls_schedule on:click=on_schedule>
                <span class="mobile-tab-glyph" aria-hidden="true">"⚙"</span>
                <span class="mobile-tab-label">"Schedule"</span>
            </a>
        </nav>
    }
}
