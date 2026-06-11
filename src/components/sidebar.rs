// Primary nav for LocalSky. Left sidebar with grouped sections; on
// mobile (≤ 760px) the sidebar collapses to a hamburger that opens a
// slide-in drawer. Icons are inline SVGs picked from the registry in
// sidebar_icon.rs so they inherit the link's text color.
//
// All links are plain <a href> so default browser navigation still
// works if the WASM intercept fails, with an on:click handler that
// hands off to leptos_router::use_navigate when it does. Active state
// is computed from use_location.

use crate::components::ui::Icon;
use crate::nav_log::log_nav;
use leptos::prelude::*;
use leptos_router::hooks::{use_location, use_navigate};
use leptos_router::NavigateOptions;

#[component]
pub fn Sidebar() -> impl IntoView {
    // Drawer-open state for mobile. Desktop ignores it (sidebar is
    // always visible above 760px via SCSS).
    let drawer_open = RwSignal::new(false);

    let toggle_drawer = move |_| {
        drawer_open.set(!drawer_open.get_untracked());
    };

    let backdrop_class = move || {
        if drawer_open.get() {
            "sidebar-backdrop is-visible"
        } else {
            "sidebar-backdrop"
        }
    };
    let sidebar_class = move || {
        if drawer_open.get() {
            "sidebar is-open"
        } else {
            "sidebar"
        }
    };

    view! {
        <div class="mobile-app-bar">
            <button
                class="sidebar-hamburger"
                aria-label="Open navigation"
                on:click=toggle_drawer
            >
                <span class="hamburger-bar"></span>
                <span class="hamburger-bar"></span>
                <span class="hamburger-bar"></span>
            </button>
            <a href="/" class="header-brand" aria-label="LocalSky home">
                <span class="header-brand__mark" aria-hidden="true">
                    <img src="/brand-mark.svg" alt="" width="20" height="20"/>
                </span>
                <span>
                    <span class="header-brand__local">"LOCAL"</span><span class="header-brand__sky">"SKY"</span>
                </span>
            </a>
        </div>

        <div class=backdrop_class on:click=toggle_drawer></div>

        <aside class=sidebar_class aria-label="Primary navigation">
            <a href="/" class="sidebar-brand" aria-label="LocalSky home">
                <span class="sidebar-brand-mark" aria-hidden="true">
                    // The brand mark lives in public/brand-mark.svg —
                    // a background-stripped variant of favicon.svg so
                    // the same artwork shows in the sidebar pill, the
                    // mobile app bar, and the browser tab. The old
                    // sidebar_icon "brand" entry was a placeholder
                    // Lucide-style outline that diverged from the
                    // real logomark; using the real SVG here keeps
                    // them in sync.
                    <img src="/brand-mark.svg" alt="" width="32" height="32"/>
                </span>
                <span class="sidebar-brand-name">
                    <span class="header-brand__local">"LOCAL"</span><span class="header-brand__sky">"SKY"</span>
                </span>
            </a>

            // ───────────────────────────────────────────────────────
            // Three groups. The two live products (Weather + Irrigation)
            // and the spatial Zones view sit up top as peers. The three
            // cross-cutting analysis tools (Simulator / Rule Lab /
            // History) get their own ANALYZE group — they reason ABOUT
            // the live products rather than being sub-views of "today".
            // Settings collapses to a single quiet entry whose tabs live
            // inside the page.
            // ───────────────────────────────────────────────────────

            // PRIMARY — the live products + the garden map.
            <NavSection title="">
                <NavLink href="/" icon="weather" label="Weather" drawer=drawer_open/>
                <NavLink href="/irrigation" icon="droplet" label="Irrigation" drawer=drawer_open/>
                <NavLink href="/zones" icon="zones" label="Zones" drawer=drawer_open/>
                <NavLink href="/sensors" icon="activity" label="Sensors" drawer=drawer_open/>
            </NavSection>

            // ANALYZE — the marquee reasoning tools.
            <NavSection title="Analyze">
                <NavLink href="/simulator" icon="simulator" label="Simulator" drawer=drawer_open/>
                <NavLink href="/rules" icon="rule-lab" label="Rule Lab" drawer=drawer_open/>
                <NavLink href="/history" icon="history" label="History" drawer=drawer_open/>
            </NavSection>

            // SETTINGS — set-once configuration, demoted to one entry.
            // The 12 sections live as tabs inside the settings page.
            <NavSection title="" compact=true>
                <NavLink href="/settings" icon="settings" label="Settings" drawer=drawer_open/>
            </NavSection>

            // ───────────────────────────────────────────────────────
            // Footer — low-traffic actions + external links. Re-run
            // wizard moves down here from System because it's
            // configuration-recovery, not day-to-day.
            // ───────────────────────────────────────────────────────
            <div class="sidebar-footer">
                <a class="sidebar-footer-link" href="/setup" title="Setup wizard">
                    <span class="sidebar-footer-label">"Setup wizard"</span>
                    <span class="sidebar-footer-glyph"><Icon name="wizard" size=14u32/></span>
                </a>
                <a class="sidebar-footer-link" href="/about" title="About">
                    <span class="sidebar-footer-label">"About"</span>
                    <span class="sidebar-footer-glyph"><Icon name="info" size=14u32/></span>
                </a>
                <a class="sidebar-footer-link" href="https://github.com/silenthooligan/localsky" target="_blank" rel="noopener" title="GitHub">
                    <span class="sidebar-footer-label">"GitHub"</span>
                    <span class="sidebar-footer-glyph"><Icon name="external" size=14u32/></span>
                </a>
            </div>
        </aside>
    }
}

#[component]
fn NavSection(
    title: &'static str,
    children: Children,
    /// Render the section in compact mode (smaller text, tighter
    /// padding). Used by Settings to demote 12 items below the two
    /// live products without hiding them behind a click.
    #[prop(optional)]
    compact: bool,
) -> impl IntoView {
    let section_class = if compact {
        "sidebar-section sidebar-section--compact"
    } else {
        "sidebar-section"
    };
    let has_title = !title.is_empty();
    view! {
        <div class=section_class>
            {has_title.then(|| view! { <div class="sidebar-section-title">{title}</div> })}
            <ul class="sidebar-nav">
                {children()}
            </ul>
        </div>
    }
}

#[component]
fn NavLink(
    href: &'static str,
    icon: &'static str,
    label: &'static str,
    drawer: RwSignal<bool>,
    #[prop(optional)] sub: bool,
) -> impl IntoView {
    let loc = use_location();
    let active_class = move || {
        let path = loc.pathname.get();
        // Exact match for the root route to keep Weather from
        // light-housing on /irrigation/zone/* and /settings/*.
        let is_active = if href == "/" {
            path == "/"
        } else {
            path == href || path.starts_with(&format!("{href}/"))
        };
        let base = if sub {
            "sidebar-link sidebar-link--sub"
        } else {
            "sidebar-link"
        };
        if is_active {
            format!("{base} is-active")
        } else {
            base.to_string()
        }
    };

    let navigate = use_navigate();
    let on_click = move |ev: leptos::ev::MouseEvent| {
        log_nav(format!("sidebar click {href}"));
        if ev.ctrl_key() || ev.meta_key() || ev.shift_key() || ev.button() != 0 {
            return;
        }
        ev.prevent_default();
        drawer.set(false);
        navigate(href, NavigateOptions::default());
    };

    view! {
        <li>
            // title carries the label for the tablet icon-rail, where the
            // text label is hidden and hover/long-press needs a tooltip.
            <a class=active_class href=href on:click=on_click title=label>
                <span class="sidebar-link-icon"><Icon name=icon/></span>
                <span class="sidebar-link-label">{label}</span>
            </a>
        </li>
    }
}
