// Bottom-tab nav for the mobile shell. Always rendered; hidden at desktop
// widths via SCSS. Five real-route tabs (no more `?tab=` query dispatch):
//
//   Weather    -> /
//   Irrigation -> /irrigation
//   Zones      -> /zones
//   History    -> /history
//   More       -> opens a sheet (Simulator / Rule Lab / Settings / About)
//
// The Analyze tools + Settings live behind "More", reachable on mobile
// but not bottom-tab-worthy. Glyphs use the shared <Icon/> registry so
// they tint with the active state across themes (emoji could not).

use crate::components::ui::{Icon, Sheet};
use crate::nav_log::log_nav;
use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

fn active_tab(path: &str) -> &'static str {
    if path == "/" {
        "weather"
    } else if path.starts_with("/irrigation") {
        "irrigation"
    } else if path.starts_with("/zones") {
        "zones"
    } else if path.starts_with("/history") {
        "history"
    } else {
        // simulator / rules / settings / about all surface via More
        "more"
    }
}

#[component]
pub fn MobileNav() -> impl IntoView {
    let more_open = RwSignal::new(false);

    view! {
        <nav class="mobile-tab-bar" aria-label="Primary mobile">
            <Tab tab="weather" href="/" icon="weather" label="Weather"/>
            <Tab tab="irrigation" href="/irrigation" icon="droplet" label="Irrigation"/>
            <Tab tab="zones" href="/zones" icon="zones" label="Zones"/>
            <Tab tab="history" href="/history" icon="history" label="History"/>
            <MoreTab more_open=more_open/>
        </nav>
        <Sheet open=more_open title="More".to_string() aria_label="More destinations".to_string() id="mobile-more-menu".to_string()>
            <div class="mobile-more">
                <MoreLink href="/week" icon="calendar" label="Week" open=more_open/>
                <MoreLink href="/sensors" icon="activity" label="Sensors" open=more_open/>
                <MoreLink href="/simulator" icon="simulator" label="Simulator" open=more_open/>
                <MoreLink href="/rules" icon="rule-lab" label="Rule Lab" open=more_open/>
                <MoreLink href="/settings" icon="settings" label="Settings" open=more_open/>
                <MoreLink href="/about" icon="info" label="About" open=more_open/>
            </div>
        </Sheet>
    }
}

#[component]
fn Tab(
    tab: &'static str,
    href: &'static str,
    icon: &'static str,
    label: &'static str,
) -> impl IntoView {
    let loc = use_location();
    let cls = move || {
        if active_tab(&crate::base::route_path(&loc.pathname.get())) == tab {
            "mobile-tab is-on"
        } else {
            "mobile-tab"
        }
    };
    let navigate = use_navigate();
    let on_click = move |ev: leptos::ev::MouseEvent| {
        log_nav(format!("mobile-tab click: {tab}"));
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 {
            return;
        }
        ev.prevent_default();
        navigate(&crate::base::url(href), NavigateOptions::default());
    };
    view! {
        <a href=href class=cls on:click=on_click>
            <span class="mobile-tab-glyph" aria-hidden="true"><Icon name=icon size=22/></span>
            <span class="mobile-tab-label">{label}</span>
        </a>
    }
}

#[component]
fn MoreTab(more_open: RwSignal<bool>) -> impl IntoView {
    let loc = use_location();
    let cls = move || {
        if active_tab(&crate::base::route_path(&loc.pathname.get())) == "more" {
            "mobile-tab is-on"
        } else {
            "mobile-tab"
        }
    };
    view! {
        <button
            type="button"
            class=cls
            aria-label="More"
            aria-haspopup="menu"
            aria-controls="mobile-more-menu"
            aria-expanded=move || more_open.get().to_string()
            on:click=move |_| more_open.set(true)
        >
            <span class="mobile-tab-glyph" aria-hidden="true"><Icon name="more" size=22/></span>
            <span class="mobile-tab-label">"More"</span>
        </button>
    }
}

#[component]
fn MoreLink(
    href: &'static str,
    icon: &'static str,
    label: &'static str,
    open: RwSignal<bool>,
) -> impl IntoView {
    let navigate = use_navigate();
    let on_click = move |ev: leptos::ev::MouseEvent| {
        log_nav(format!("mobile-more click: {href}"));
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 {
            return;
        }
        ev.prevent_default();
        open.set(false);
        navigate(&crate::base::url(href), NavigateOptions::default());
    };
    view! {
        <a href=href class="mobile-more__link" on:click=on_click>
            <span class="mobile-more__icon"><Icon name=icon size=20/></span>
            <span class="mobile-more__label">{label}</span>
            <Icon name="chevron-right" size=18/>
        </a>
    }
}
