// Tab nav for the /irrigation sub-routes. Renders four chips that link
// to /irrigation, /irrigation/zones, /irrigation/budget, and
// /irrigation/history. The chip whose route matches the current
// location lights up via leptos_router::use_location, so no JS-side
// active-state tracking is needed.
//
// Lives at the top of each sub-page (below RunningBanner) so the user
// can switch between Today / Zones / Budget / History without going
// back to the sidebar. Renders on both desktop + mobile so dense
// sub-pages on phones still have one-tap nav between them.

use crate::nav_log::log_nav;
use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

#[component]
pub fn IrrigationTabNav() -> impl IntoView {
    view! {
        <nav class="irrigation-toolbar" aria-label="Irrigation sub-section nav">
            <TabChip href="/irrigation" label="Today"/>
            <TabChip href="/irrigation/zones" label="Zones"/>
            <TabChip href="/irrigation/budget" label="Water budget"/>
            <TabChip href="/irrigation/history" label="History"/>
        </nav>
    }
}

#[component]
fn TabChip(href: &'static str, label: &'static str) -> impl IntoView {
    let loc = use_location();
    let class = move || {
        let path = crate::base::route_path(&loc.pathname.get());
        // Exact match for /irrigation so it doesn't stay lit on
        // /irrigation/zones etc.; everything else uses prefix match.
        let is_active = if href == "/irrigation" {
            path == "/irrigation"
        } else {
            path == href || path.starts_with(&format!("{href}/"))
        };
        if is_active {
            "irrigation-toolbar-chip is-active"
        } else {
            "irrigation-toolbar-chip"
        }
    };
    let navigate = use_navigate();
    let on_click = move |ev: leptos::ev::MouseEvent| {
        log_nav(format!("irrigation tab click {href}"));
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 {
            return;
        }
        ev.prevent_default();
        navigate(&crate::base::url(href), NavigateOptions::default());
    };
    view! {
        <a class=class href=href on:click=on_click>{label}</a>
    }
}
