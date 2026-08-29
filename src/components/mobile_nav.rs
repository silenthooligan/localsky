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
            // Zones mirrors the desktop sidebar's suggestions count, fed
            // by the same app-level tuning summary (one shared fetch).
            <Tab
                tab="zones"
                href="/zones"
                icon="zones"
                label="Zones"
                badge=crate::components::zones::tuning::use_suggestion_count()
            />
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
    /// Optional count badge riding the glyph's corner (--attention tint,
    /// entity-badge geometry). Renders NOTHING at zero, matching the
    /// desktop sidebar's rule, so SSR and hydrate's first frame agree.
    #[prop(optional)]
    badge: Option<Signal<usize>>,
) -> impl IntoView {
    let pathname = use_location().pathname;
    // One shared active predicate feeds both the class and aria-current
    // so visual state and the SR-exposed state cannot drift.
    let is_on = move || active_tab(&crate::base::route_path(&pathname.get())) == tab;
    let cls = move || {
        if is_on() {
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
        // Plain route, not base::url(href): the Router base is applied by
        // navigate() itself; pre-prefixing double-prefixes under HA ingress
        // (issue #3). Real browser navs are prefixed by the shell click shim.
        navigate(href, NavigateOptions::default());
    };
    view! {
        <a
            href=href
            class=cls
            aria-current=move || is_on().then_some("page")
            on:click=on_click
        >
            <span class="mobile-tab-glyph" aria-hidden="true"><Icon name=icon size=22/></span>
            <span class="mobile-tab-label">{label}</span>
            // Badge element is conditional (never an empty pill at zero).
            // The visible number is aria-hidden; the sr-only suffix gives
            // SR users the same fact in words.
            {badge.map(|b| move || {
                let n = b.get();
                (n > 0).then(|| view! {
                    <span class="mobile-tab-badge">
                        <span aria-hidden="true">{n.to_string()}</span>
                        <span class="sr-only">
                            {format!("{n} suggestion{}", if n == 1 { "" } else { "s" })}
                        </span>
                    </span>
                })
            })}
        </a>
    }
}

#[component]
fn MoreTab(more_open: RwSignal<bool>) -> impl IntoView {
    // No aria-current here: More is a sheet trigger, not a page link;
    // its expanded state is what aria-expanded below already conveys.
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
        // Plain route (see Tab above): navigate() adds the Router base, so
        // base::url() here would double-prefix under HA ingress (issue #3).
        navigate(href, NavigateOptions::default());
    };
    view! {
        <a href=href class="mobile-more__link" on:click=on_click>
            <span class="mobile-more__icon"><Icon name=icon size=20/></span>
            <span class="mobile-more__label">{label}</span>
            <Icon name="chevron-right" size=18/>
        </a>
    }
}
