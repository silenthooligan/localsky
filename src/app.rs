// Top-level Leptos shell. The SSR pass reads the current Tempest +
// Irrigation snapshots out of context (the axum side `provide_context`s
// both Arc<TempestStore> and Arc<IrrigationStore>) so the first render
// is fully hydrated with live values, no spinner, no flash. After
// hydration, the browser subscribes to the matching SSE streams and
// replaces each signal on every server-pushed snapshot.

use crate::components::{
    footer::Footer,
    forecast::{DailyForecast, HourlyForecast},
    hero::Hero,
    humidity::HumidityPanel,
    install_prompt::InstallPrompt,
    irrigation::IrrigationPage,
    lightning::LightningPanel,
    mobile_nav::MobileNav,
    page_header::PageHeader,
    pressure::PressurePanel,
    radar::RadarPanel,
    rain::RainPanel,
    sidebar::Sidebar,
    solar::SolarPanel,
    wind::WindPanel,
};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::ha::snapshot::IrrigationSnapshot;
use crate::tempest::state::Snapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
use leptos_meta::*;
use leptos_router::{
    components::{Route, Router, Routes},
    path,
};

// (forecast/irrigation/tempest store imports removed, the SSR initial
// snapshot helpers don't read from the stores anymore; see comment on
// initial_*_ssr below for the rationale.)

// Why every initial_*_ssr returns a default value (even on the SSR
// build): the WASM hydrate-side signals init to ::default(), and any
// view tree whose CHILD COUNT depends on Vec length (the 7-day
// forecast row, the 48-hour hourly chart, the 7-day verdict strip,
// etc.) will produce different numbers of DOM children on SSR vs on
// hydrate's first render, which crashes tachys's hydration walker
// at `tachys-0.2.15/src/hydration.rs:227` with `internal error:
// entered unreachable code`. The fix is to make the *initial* render
// identical between SSR and hydrate; the SSE streams (/api/stream,
// /api/irrigation/stream, /api/forecast/stream) push the real values
// within ~10 s of hydration, so the only cost is a brief
// "Loading forecast…" placeholder on first paint.
fn initial_tempest_ssr() -> Snapshot {
    Snapshot::default()
}

fn initial_irrigation_ssr() -> IrrigationSnapshot {
    IrrigationSnapshot::default()
}

fn initial_forecast_ssr() -> ForecastSnapshot {
    ForecastSnapshot::default()
}

/// Global "show raw engine math everywhere" toggle. Provided as context
/// so the Advanced settings toggle and every consumer (skip breakdown,
/// forecast intelligence sub-blocks, zone math, etc.) share one signal.
/// Newtype'd so use_context lookup doesn't collide with other
/// RwSignal<bool> values like is_mobile.
#[derive(Clone, Copy)]
pub struct NerdMode(pub RwSignal<bool>);

/// Household display-unit default, derived once at the app root from the
/// irrigation snapshot's `units` field (which the refresher copies from
/// `cfg.deployment.units`). Provided as context so `use_unit_prefs` can
/// resolve a device's display units against the household baseline WITHOUT a
/// per-component `/api/config` fetch. Newtype'd so the `use_context` lookup is
/// unambiguous.
///
/// SSR-safety: the irrigation signal starts at `IrrigationSnapshot::default()`
/// on both SSR and hydrate's first frame, whose `units` is `Units::Imperial`.
/// So this signal reads Imperial on the SSR-matching first frame and only
/// changes once the SSE pushes the real snapshot (client-side), which keeps the
/// SSR/hydrate DOM trees identical. `use_unit_prefs` reads this only inside its
/// hydrate Effect, never at SSR.
#[derive(Clone, Copy)]
pub struct HouseholdUnits(pub Signal<crate::ha::snapshot::Units>);

/// Whether this deployment has any irrigation hardware configured (at least one
/// controller OR zone), read once at app root from `GET /api/v1/info`'s
/// `has_irrigation`. Provided as context so any component can branch the
/// information architecture on it. The shell uses it to gate the irrigation
/// routes and to set `data-no-irrigation` on `<html>` (the stylesheet hides the
/// irrigation-only nav items from there, matching the `data-nerd`/`data-dry-run`
/// mechanism). Newtype'd so the `use_context` lookup is unambiguous.
///
/// SSR-safety: starts at `true` on both SSR and hydrate's first frame, then the
/// deferred `/api/v1/info` fetch flips it to the real value client-side. Routes
/// and nav entries that depend on it therefore render identically on the
/// SSR-matching first frame (everything present), and only the no-irrigation
/// install collapses them after hydration. Starting at `true` (not `false`)
/// keeps a full-irrigation install from flashing a stripped nav before the
/// fetch resolves.
#[derive(Clone, Copy)]
pub struct HasIrrigation(pub RwSignal<bool>);

#[component]
pub fn App() -> impl IntoView {
    provide_meta_context();

    // `set_*` is only invoked inside the hydrate-feature SSE effect; SSR
    // builds never write the signals, so suppress the unused warnings.
    #[allow(unused_variables)]
    let (tempest, set_tempest) = signal(initial_tempest_ssr());
    #[allow(unused_variables)]
    let (irrigation, set_irrigation) = signal(initial_irrigation_ssr());
    #[allow(unused_variables)]
    let (forecast, set_forecast) = signal(initial_forecast_ssr());

    // Nav debug ring buffer is preserved as a developer affordance, log_nav()
    // calls scattered through the app no-op when the sink isn't installed, so
    // we can re-enable the in-page strip by reinstalling install_sink() and
    // re-rendering <NavLogStrip/> in the view tree below if we ever need it.
    // The visible strip was a debug build artifact; intentionally not rendered
    // in prod.
    let (nav_debug, _set_nav_debug) = signal::<Vec<String>>(Vec::new());
    provide_context(nav_debug);

    // Household display-unit default, derived from the irrigation snapshot and
    // shared via context so `use_unit_prefs` resolves household-vs-device units
    // without its own fetch. Reads Imperial on SSR + hydrate's first frame
    // (default snapshot), then tracks the SSE-pushed value client-side. See the
    // `HouseholdUnits` doc comment for the SSR-match rationale.
    provide_context(HouseholdUnits(Signal::derive(move || {
        irrigation.get().units
    })));

    // Viewport flag for layout decisions. SSR + hydrate's first frame both
    // see `false` (desktop), so the initial DOM tree matches and tachys
    // hydrates cleanly. Post-hydrate we read window.matchMedia and flip the
    // signal, descendants reading via use_context::<RwSignal<bool>> get a
    // signal-driven update, no remount.
    let is_mobile: RwSignal<bool> = RwSignal::new(false);
    provide_context(is_mobile);

    // Toast hub, provided once here so any component can call
    // use_toast().success(...). The <ToastViewport/> in the shell renders
    // the live stack. Empty on SSR + hydrate's first frame (toasts only
    // ever arrive from client event handlers), so no hydration mismatch.
    provide_context(crate::components::ui::ToastHub::new());

    // Nerd mode. Same SSR-safe pattern as is_mobile: start at false on
    // both SSR and hydrate's first frame, then a deferred spawn_local
    // reads localStorage("nerd_mode") and flips the signal. A second
    // effect persists changes back to localStorage and toggles the
    // `data-nerd="true"` attribute on <html>, which the stylesheet uses
    // to gate `.nerd-only` blocks and reveal the full skip-check
    // breakdown. Per-device, not per-account.
    let nerd_mode: RwSignal<bool> = RwSignal::new(false);
    // Gate persistence until the initial localStorage read has run. Without
    // this the persist Effect fires on mount (nerd_mode=false) and writes "0"
    // before the deferred read, so the read then sees "0" and the
    // server-default seed never takes for new users. Only the hydrate-gated
    // block below touches it; SSR builds leave it unused.
    #[allow(unused_variables)]
    let nerd_loaded = RwSignal::new(false);
    provide_context(NerdMode(nerd_mode));

    // Irrigation-presence flag. Starts `true` so a full install never flashes a
    // stripped nav before /api/v1/info resolves; the deferred info fetch below
    // flips it to the server's `has_irrigation`. SSR + hydrate's first frame both
    // see `true`, so the route list and nav render identically until the
    // client-only fetch lands. See `HasIrrigation` for the SSR-match rationale.
    let has_irrigation: RwSignal<bool> = RwSignal::new(true);
    provide_context(HasIrrigation(has_irrigation));
    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            // Defer past the initial hydration sweep, same trick as the
            // nav_log "hydrated" line. If we set is_mobile synchronously,
            // the irrigation page's mobile/desktop branch can flip mid-walk
            // and trigger the same tachys::hydration mismatch the rest of
            // the file works to avoid.
            gloo_timers::future::TimeoutFuture::new(0).await;
            if let Some(win) = web_sys::window() {
                if let Ok(Some(mql)) = win.match_media("(max-width: 760px)") {
                    is_mobile.set(mql.matches());
                    // React to viewport changes (rotation, browser-window resize,
                    // pinch-out, devtools open). Closure::forget is fine because
                    // the listener lives for the page lifetime.
                    use wasm_bindgen::closure::Closure;
                    use wasm_bindgen::JsCast;
                    let cb =
                        Closure::<dyn FnMut(_)>::new(move |ev: web_sys::MediaQueryListEvent| {
                            is_mobile.set(ev.matches());
                        });
                    let _ =
                        mql.add_event_listener_with_callback("change", cb.as_ref().unchecked_ref());
                    cb.forget();
                }
            }
        });
    }

    // Tracks whether the device has an EXPLICIT prior nerd-mode choice in
    // localStorage. When false (a new device), the /api/v1/info fetch below seeds
    // the signal from the server's `nerd_mode_default` instead of hard-coding the
    // user into either mode. Only the hydrate-gated blocks touch it.
    #[allow(unused_variables)]
    let nerd_choice_explicit = RwSignal::new(false);

    // Nerd-mode hydration: same deferred-init pattern as is_mobile so
    // SSR + hydrate's first frame both see `false` and the DOM tree
    // matches. After the hydration walker finishes we read
    // localStorage("nerd_mode") and flip the signal; the persist
    // Effect below then applies data-nerd to <html> and the
    // stylesheet does the rest.
    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(0).await;
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    match storage.get_item("nerd_mode") {
                        // Respect an explicit prior choice. A new device with no
                        // stored value is LEFT at Simple (false) here; the
                        // /api/v1/info fetch below then seeds it from the server's
                        // `nerd_mode_default` knob, so "show the math" is one tap
                        // away but is no longer forced on by default (design #3).
                        Ok(Some(v)) => {
                            nerd_mode.set(v == "1" || v == "true");
                            nerd_choice_explicit.set(true);
                        }
                        _ => {}
                    }
                }
            }
            nerd_loaded.set(true);
        });
        Effect::new(move |_| {
            let v = nerd_mode.get();
            // Don't persist/clobber until the initial read has settled.
            if !nerd_loaded.get() {
                return;
            }
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    let _ = storage.set_item("nerd_mode", if v { "1" } else { "0" });
                }
                if let Some(doc) = win.document() {
                    if let Some(html) = doc.document_element() {
                        if v {
                            let _ = html.set_attribute("data-nerd", "true");
                        } else {
                            let _ = html.remove_attribute("data-nerd");
                        }
                    }
                }
            }
        });
    }

    // One-shot fetch of /api/v1/info on hydrate. Four things ride on it:
    //   - dry_run / demo  -> data-dry-run / data-demo on <html> for the fixed
    //     warning bars (without them "nothing happened at 6 AM" reads as a
    //     regression instead of an intentional override / a demo instance).
    //   - has_irrigation  -> the HasIrrigation signal (gates the irrigation
    //     routes) + data-no-irrigation on <html> so the stylesheet hides the
    //     irrigation-only nav items on a weather-only install (design #2).
    //   - nerd_mode_default -> seeds Simple-vs-Nerd for a NEW device that has no
    //     explicit prior choice, instead of hard-coding new users into Nerd mode
    //     (design #3). An explicit localStorage choice always wins.
    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(0).await;
            let Ok(resp) = gloo_net::http::Request::get("/api/v1/info").send().await else {
                return;
            };
            let Ok(val): Result<serde_json::Value, _> = resp.json().await else {
                return;
            };
            let dry_run = val
                .get("dry_run")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let demo = val.get("demo").and_then(|v| v.as_bool()).unwrap_or(false);
            // Pre-1.13.0 servers omit these; absence is the safe default
            // (weather-only nav stays as-is, Simple mode) so an older backend
            // degrades gracefully rather than stripping the nav.
            let irrigation = val
                .get("has_irrigation")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let nerd_default = val
                .get("nerd_mode_default")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            has_irrigation.set(irrigation);
            // Seed nerd mode from the server default ONLY for a device with no
            // explicit prior choice. The localStorage read above has already run
            // (both are spawn_local'd after the same 0ms defer, and this one then
            // awaits a network round-trip), so `nerd_choice_explicit` is settled.
            if !nerd_choice_explicit.get_untracked() {
                nerd_mode.set(nerd_default);
            }

            if let Some(win) = web_sys::window() {
                if let Some(doc) = win.document() {
                    if let Some(html) = doc.document_element() {
                        if dry_run {
                            let _ = html.set_attribute("data-dry-run", "true");
                        }
                        if demo {
                            let _ = html.set_attribute("data-demo", "true");
                        }
                        // Weather-only install: the stylesheet hides the
                        // irrigation-only nav links from this attribute (same
                        // mechanism as data-nerd / data-dry-run).
                        if irrigation {
                            let _ = html.remove_attribute("data-no-irrigation");
                        } else {
                            let _ = html.set_attribute("data-no-irrigation", "true");
                        }
                    }
                }
            }
        });
    }

    // Shared connection state: every SSE subscription reports into it, the
    // PageHeader pill renders it. Provided unconditionally so use_context
    // resolves on both SSR and hydrate (status only mutates client-side).
    let conn = crate::components::connection::ConnState::new();
    provide_context(conn);

    // On the client, open one auto-reconnecting EventSource per stream and
    // overwrite the matching signal on every event. Runs only after
    // hydration. subscribe_sse owns retry/backoff + health reporting.
    #[cfg(feature = "hydrate")]
    {
        use crate::components::connection::{spawn_conn_watchdog, subscribe_sse};
        Effect::new(move |_| {
            subscribe_sse::<Snapshot>("/api/stream", set_tempest, conn);
            subscribe_sse::<IrrigationSnapshot>("/api/irrigation/stream", set_irrigation, conn);
            subscribe_sse::<ForecastSnapshot>("/api/forecast/stream", set_forecast, conn);
            spawn_conn_watchdog(conn);
        });
    }

    // Router base: under an ingress prefix the browser's location includes
    // the prefix, so the client router must strip it before matching. The
    // server receives prefix-stripped paths and must match with no base.
    let router_base = if cfg!(feature = "ssr") {
        String::new()
    } else {
        crate::base::base_path()
    };

    view! {
        // The leptos CSS link now lives in the shell <head> as a
        // <HashedStylesheet> (hash-files); a hardcoded /pkg/localsky.css here
        // would 404 against the content-hashed filename on disk.
        <Title text="LocalSky"/>

        <Router base=router_base>
            <div class="app-shell">
                // Skip link lives inside the hydration root. Putting it as a
                // sibling of <App/> in the body shell desyncs SSR vs hydrate
                // (tachys walks body.firstChild expecting <div> and finds <a>
                // instead, panicking with failed_to_cast_element at
                // hydration.rs:163).
                <a href="#main-content" class="skip-link">"Skip to main content"</a>
                <Sidebar/>
                <main class="page" id="main-content">
                    <InstallPrompt/>
                    <PageHeader/>
                    <crate::components::health_banner::HealthBanner/>
                <Routes fallback=|| view! { <NotFound/> }>
                    <Route path=path!("/")
                        view=move || view! {
                            <Title text="LocalSky · Weather"/>
                            <WeatherHome snap=tempest forecast=forecast irrigation=irrigation/>
                        }/>
                    <Route path=path!("/irrigation")
                        view=move || view! {
                            <Title text="LocalSky · Irrigation · Today"/>
                            <IrrigationPage snap=irrigation/>
                        }/>
                    <Route path=path!("/week")
                        view=move || view! {
                            <Title text="LocalSky · Watering Week"/>
                            <crate::components::watering_week::WateringWeekPage snap=irrigation/>
                        }/>
                    // ── v2 top-level destinations ──────────────────────
                    // Zones canvas + the Analyze triad (Simulator / Rule
                    // Lab / History) are promoted to peers of Weather and
                    // Irrigation. Stubs today; each real screen lands here.
                    <Route path=path!("/zones")
                        view=move || view! {
                            <Title text="LocalSky · Zones"/>
                            <crate::components::zones::ZonesPage snap=irrigation/>
                        }/>
                    <Route path=path!("/zones/:slug")
                        view=move || view! {
                            <Title text="LocalSky · Zone"/>
                            <crate::components::zones::ZoneDetailPage snap=irrigation/>
                        }/>
                    <Route path=path!("/simulator")
                        view=move || view! {
                            <Title text="LocalSky · Simulator"/>
                            <crate::components::simulator::SimulatorPage snap=irrigation/>
                        }/>
                    <Route path=path!("/rules")
                        view=move || view! {
                            <Title text="LocalSky · Rule Lab"/>
                            <crate::components::rules::RuleLabPage snap=irrigation/>
                        }/>
                    <Route path=path!("/history")
                        view=|| view! {
                            <Title text="LocalSky · History"/>
                            <crate::components::historyview::HistoryPage/>
                        }/>
                    <Route path=path!("/sensors")
                        view=move || view! {
                            <Title text="LocalSky · Sensors"/>
                            <crate::components::sensors::SensorsPage snap=irrigation weather=tempest/>
                        }/>
                    <Route path=path!("/settings")
                        view=|| view! {
                            <Title text="LocalSky · Settings"/>
                            <crate::components::settings::SettingsHome/>
                        }/>
                    <Route path=path!("/settings/theme")
                        view=|| view! {
                            <Title text="LocalSky · Theme"/>
                            <crate::components::settings::SettingsTheme/>
                        }/>
                    <Route path=path!("/settings/units")
                        view=|| view! {
                            <Title text="LocalSky · Units"/>
                            <crate::components::settings::SettingsUnits/>
                        }/>
                    <Route path=path!("/settings/location")
                        view=|| view! {
                            <Title text="LocalSky · Location"/>
                            <crate::components::settings::SettingsLocation/>
                        }/>
                    <Route path=path!("/settings/llm")
                        view=|| view! {
                            <Title text="LocalSky · LLM"/>
                            <crate::components::settings::SettingsLlm/>
                        }/>
                    <Route path=path!("/settings/account")
                        view=move || view! {
                            <Title text="LocalSky · Settings · Account"/>
                            <crate::components::settings::SettingsAccount/>
                        }/>
                    <Route path=path!("/settings/notifications")
                        view=|| view! {
                            <Title text="LocalSky · Notifications"/>
                            <crate::components::settings::SettingsNotifications/>
                        }/>
                    // Deprecated source/controller editors: the unified Devices
                    // hub (/settings/devices) replaced both (design #4, redundant
                    // surfaces). Keep the URLs alive but repoint them at the hub so
                    // stale deep-links + the in-app links that still point here land
                    // on the live editor instead of the orphaned raw-JSON pages.
                    // <Redirect> issues a real server redirect under SSR and a
                    // client navigate after hydration.
                    // Legacy source/controller/data-source editors fold into the
                    // unified Devices hub inside the settings SHELL (master-detail
                    // with the left section rail), so deep links + stale in-app
                    // links land on the live editor with the rail intact instead
                    // of the bare /settings/devices route (which drops the rail)
                    // or an orphaned raw-JSON page. Per-field source ownership +
                    // priority now lives co-located in that hub (design #4), so
                    // /settings/data-sources points there too.
                    <Route path=path!("/settings/sources")
                        view=|| view! {
                            <leptos_router::components::Redirect
                                path=crate::base::url("/settings?section=devices")/>
                        }/>
                    <Route path=path!("/settings/data-sources")
                        view=|| view! {
                            <leptos_router::components::Redirect
                                path=crate::base::url("/settings?section=devices")/>
                        }/>
                    <Route path=path!("/settings/controllers")
                        view=|| view! {
                            <leptos_router::components::Redirect
                                path=crate::base::url("/settings?section=devices")/>
                        }/>
                    <Route path=path!("/settings/help")
                        view=move || view! {
                            <Title text="LocalSky · Settings · Help"/>
                            <crate::components::settings::help::SettingsHelp/>
                        }/>
                    <Route path=path!("/settings/home-assistant")
                        view=move || view! {
                            <Title text="LocalSky · Settings · Home Assistant"/>
                            <crate::components::settings::SettingsHomeAssistant/>
                        }/>
                    <Route path=path!("/settings/devices")
                        view=|| view! {
                            <Title text="LocalSky · Devices"/>
                            <crate::components::settings::SettingsDevices/>
                        }/>
                    <Route path=path!("/settings/sensors")
                        view=|| view! {
                            <Title text="LocalSky · Sensors"/>
                            <crate::components::settings::SettingsSensors/>
                        }/>
                    <Route path=path!("/settings/zones")
                        view=|| view! {
                            <Title text="LocalSky · Zones"/>
                            <crate::components::settings::SettingsZones/>
                        }/>
                    <Route path=path!("/settings/skip-rules")
                        view=|| view! {
                            <Title text="LocalSky · Skip rules"/>
                            <crate::components::settings::SettingsSkipRules/>
                        }/>
                    <Route path=path!("/settings/restrictions")
                        view=|| view! {
                            <Title text="LocalSky · Watering restrictions"/>
                            <crate::components::settings::SettingsRestrictions/>
                        }/>
                    <Route path=path!("/settings/schedules")
                        view=|| view! {
                            <Title text="LocalSky · Manual schedules"/>
                            <crate::components::settings::SettingsSchedules/>
                        }/>
                    <Route path=path!("/settings/radar")
                        view=|| view! {
                            <Title text="LocalSky · Radar"/>
                            <crate::components::settings::SettingsRadar/>
                        }/>
                    <Route path=path!("/settings/advanced")
                        view=|| view! {
                            <Title text="LocalSky · Advanced"/>
                            <crate::components::settings::SettingsAdvanced/>
                        }/>
                    <Route path=path!("/setup")
                        view=|| view! {
                            <Title text="LocalSky · Setup"/>
                            <crate::components::setup::SetupShell/>
                        }/>
                    <Route path=path!("/setup/:step")
                        view=|| view! {
                            <Title text="LocalSky · Setup"/>
                            <crate::components::setup::SetupShell/>
                        }/>
                    <Route path=path!("/login")
                        view=move || view! {
                            <Title text="LocalSky · Sign in"/>
                            <crate::components::login::LoginPage/>
                        }/>
                    <Route path=path!("/about")
                        view=|| view! {
                            <Title text="LocalSky · About"/>
                            <crate::components::about::AboutPage/>
                        }/>
                </Routes>
                // Bottom-tab nav. Always rendered; SCSS hides it at desktop
                // widths. Lives outside <Routes> so it persists across route
                // transitions and never unmounts/remounts (which would lose
                // the active highlight animation).
                    <MobileNav/>
                // Beta feedback pill: fixed chrome like the nav, outside
                // <Routes> so it persists across navigation. CSS hides it
                // in kiosk/readonly modes and on the login gate.
                <crate::components::feedback::BetaFeedback/>
                </main>
                <crate::components::ui::ToastViewport/>
            </div>
        </Router>
    }
}

#[component]
fn WeatherHome(
    snap: ReadSignal<Snapshot>,
    forecast: ReadSignal<ForecastSnapshot>,
    irrigation: ReadSignal<IrrigationSnapshot>,
) -> impl IntoView {
    // Split into sibling helper fns and type-erase each via .into_any() so
    // the inner view trees don't propagate up into WeatherHome's
    // monomorphized type. Without the erasure, rustc walks every nested
    // HtmlElement+ attrs tuple and overflows its query depth at the grid.
    //
    // Layout (.weather-grid, see SCSS): on wide screens the hero + wind +
    // lightning share the top row, the four compact metric cards (rain /
    // humidity / pressure / sun) share the second row, and the radar spans the
    // bottom. On a true ultrawide the grid locks to the viewport so the whole
    // dashboard fits the viewport without scrolling. Each panel is placed by its
    // own root class (.wind, .rain, ...) via grid-template-areas, so the order
    // here is just DOM order: hero+wind+lightning, the four metric cards, the
    // radar (which spans the full height on the right), then the forecast strips
    // tucked under the metric cards.
    view! {
        {view! { <crate::components::welcome_card::WelcomeCard/> }.into_any()}
        // Front-door watering verdict (design #4): a compact, persistent strip
        // above the weather grid for irrigation deployments, so the product's
        // "aha" (will it water tonight, and why) is visible on the Weather home
        // and deep-links into /irrigation. Weather-only installs render nothing.
        {view! { <HomeWateringVerdict snap=irrigation/> }.into_any()}
        <div class="weather-grid">
            {render_hero(snap, forecast).into_any()}
            {view! { <WindPanel snap/> }.into_any()}
            {view! { <LightningPanel snap/> }.into_any()}
            {view! { <RainPanel snap/> }.into_any()}
            {view! { <HumidityPanel snap/> }.into_any()}
            {view! { <PressurePanel snap/> }.into_any()}
            {view! { <SolarPanel snap/> }.into_any()}
            {render_radar().into_any()}
            <div class="weather-extra">
                {view! { <HourlyForecast snap=forecast/> }.into_any()}
                {view! { <DailyForecast snap=forecast/> }.into_any()}
            </div>
        </div>
        {render_footer(snap).into_any()}
    }
}

fn render_hero(
    snap: ReadSignal<Snapshot>,
    forecast: ReadSignal<ForecastSnapshot>,
) -> impl IntoView {
    // Pass the forecast so the cloud-only condition glyph can key off the
    // current weather_code (correct rain/snow/fog) rather than only solar.
    view! { <Hero snap forecast=forecast/> }
}

fn render_radar() -> impl IntoView {
    view! {
        <section class="radar">
            <RadarPanel/>
        </section>
    }
}

fn render_footer(snap: ReadSignal<Snapshot>) -> impl IntoView {
    view! { <Footer snap/> }
}

/// Compact, persistent watering-verdict strip for the Weather home (design #4).
/// Built entirely from the irrigation snapshot the shell already holds: one
/// short verdict word (WATER / SKIP / PAUSED / WATERING NOW / OFFLINE), one
/// plain-language reason line, and a deep link into /irrigation for the full
/// hero. Deliberately small: it surfaces the product's "aha" on the front door
/// without duplicating the irrigation hero's stat grid.
///
/// Only renders on irrigation deployments. `HasIrrigation` starts `true` so the
/// strip is present on SSR + hydrate's first frame (matching DOM), then the
/// /api/v1/info fetch may collapse it to nothing on a weather-only install. The
/// inner structure is fixed (no Vec-length-dependent children), so the snapshot
/// SSE swap only changes text, never the child count, keeping hydration sound.
#[component]
fn HomeWateringVerdict(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    use crate::components::irrigation::hero::{resolve_next_run, skip_tag_string};
    use crate::components::units_fmt::use_unit_prefs;
    use crate::components::verdict::{verdict_label, verdict_token};
    use crate::reason_render::render_skip_reason;

    let has_irrigation = use_context::<HasIrrigation>().map(|h| h.0);
    let prefs = use_unit_prefs();

    // One coarse state, mirroring the irrigation hero's phase ladder so the two
    // surfaces never disagree: offline / running / paused / skip / run. This
    // strip is a thin PRESENTER over the hero's honest resolver, not a second
    // decision: a scheduled slot the engine predicts will SKIP must theme blue
    // (skip) and say so, NEVER a green "WATER / Next run HH:MM" (the W6
    // regression: the old ladder returned "run" for any next_run_epoch > 0,
    // even when resolve_next_run says the slot skips).
    let state = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "off"
        } else if s.zones.iter().any(|z| z.running) {
            "run-now"
        } else if s.skip_check.will_skip && s.skip_check.reason.starts_with("Paused") {
            "paused"
        } else if s.skip_check.will_skip && s.next_run_epoch <= 0 {
            "skip"
        } else if s.next_run_epoch > 0 {
            // Theme by what the NEXT SLOT actually does (same call the hero
            // makes): a slot predicted to water is a run, a slot predicted to
            // skip is a skip, so the strip never claims water is coming for a
            // slot the engine will skip.
            if resolve_next_run(&s).slot_skips {
                "skip"
            } else {
                "run"
            }
        } else {
            "run"
        }
    };

    // Short verdict word, colored by the shared verdict token.
    let word = move || match state() {
        "off" => "OFFLINE".to_string(),
        "run-now" => "WATERING NOW".to_string(),
        "paused" => "PAUSED".to_string(),
        // A skip state reads SKIP regardless of the morning skip_check.verdict
        // (which may say WATER for a slot that already ran this morning while
        // the NEXT slot skips); the state() ladder already decided this is a
        // skip, so word it as one. The run branch keeps the engine verdict so a
        // run_extended slot can read "WATER +".
        "skip" => verdict_label("skip").to_string(),
        // verdict_label maps the engine verdict string to WATER / WATER + / SKIP.
        _ => verdict_label(&snap.get().skip_check.verdict).to_string(),
    };
    let word_color = move || match state() {
        "off" => "var(--text-faint)".to_string(),
        "paused" => "var(--verdict-wind)".to_string(),
        "run-now" | "run" => verdict_token("run").to_string(),
        // Blue skip token, matching the honest skip word above and the hero's
        // blue skip theming, instead of coloring off the morning verdict.
        "skip" => verdict_token("skip").to_string(),
        _ => verdict_token(&snap.get().skip_check.verdict).to_string(),
    };

    // One-line reason. For a scheduled run, lead with WHEN (24h, deployment-local
    // via timefmt); for a skip/pause, the plain-English skip reason; offline says
    // so. Never the full stat breakdown, that lives on /irrigation.
    let reason = move || {
        let s = snap.get();
        match state() {
            "off" => "Irrigation backend unreachable".to_string(),
            "run-now" => {
                if let Some(z) = s.zones.iter().find(|z| z.running) {
                    format!("{} running now", z.name)
                } else {
                    "A zone is running now".to_string()
                }
            }
            "paused" => render_skip_reason(&s.skip_check, prefs.get()),
            "skip" => {
                // A skipping state has two shapes that must read honestly:
                //   - an OPEN-ENDED skip (no scheduled slot): the morning
                //     skip_check is the live reason.
                //   - a SCHEDULED slot the engine predicts will SKIP: the
                //     morning skip_check describes a DIFFERENT (already-passed)
                //     window, so use the hero's resolver to surface the slot's
                //     own reason + an explicit "Re-checks HH:MM", never a green
                //     "Next run". Reuses the hero's skip_tag_string so the strip
                //     and the hero word the skip identically.
                if s.next_run_epoch > 0 {
                    skip_tag_string(&resolve_next_run(&s), &s.timezone)
                } else {
                    render_skip_reason(&s.skip_check, prefs.get())
                }
            }
            _ => {
                // Scheduled run. Lead with the next-run time in the deployment's
                // 24h local clock (WAVE-1 timefmt, never the browser TZ).
                if s.next_run_epoch > 0 {
                    let tz = s.timezone.as_str();
                    let day = crate::timefmt::format_wday_short(s.next_run_epoch, tz);
                    let hm = crate::timefmt::format_hm(s.next_run_epoch, tz);
                    if day.is_empty() {
                        format!("Next run {hm}")
                    } else {
                        format!("Next run {day} {hm}")
                    }
                } else {
                    "Watering scheduled".to_string()
                }
            }
        }
    };

    // The whole strip is an anchor into /irrigation (the full hero). Plain <a> so
    // default navigation works if the WASM intercept misses; the global click
    // shim + leptos router handle the in-app transition. Inline-styled (this
    // shell work is scoped to .rs) so it needs no new stylesheet rule: a quiet
    // claymorphic surface with a left verdict stripe. Each `style` attribute is a
    // dynamic closure so the verdict color tracks the SSE snapshot without a CSS
    // custom-property dance (the color is interpolated straight into the string).
    let wrap_style = move || {
        format!(
            "display:flex;align-items:center;gap:var(--space-3);\
             padding:var(--space-3) var(--space-4);margin-bottom:var(--space-3);\
             background:var(--elev-1);border:1px solid var(--elev-border-strong);\
             border-left:3px solid {c};border-radius:var(--radius-lg);\
             box-shadow:var(--shadow-1);text-decoration:none;color:inherit;",
            c = word_color()
        )
    };
    let glyph_style = move || format!("flex:none;display:flex;color:{c};", c = word_color());
    let word_style = move || {
        format!(
            "flex:none;font-family:var(--font-mono);font-weight:700;\
             letter-spacing:0.04em;font-size:var(--text-body-sm);color:{c};",
            c = word_color()
        )
    };
    // Gate on irrigation presence. `true` on SSR + hydrate's first frame keeps
    // the DOM identical; a weather-only install collapses the strip after the
    // info fetch resolves (client-only reactive update). When the context is
    // somehow absent (it is always provided at app root), default to showing it.
    // The strip is rebuilt inside this reactive closure each run; `word`,
    // `reason`, and the style closures are all Copy (they capture only Copy
    // signals), so they re-copy into the inner view on every re-render.
    move || {
        let show = has_irrigation.map(|h| h.get()).unwrap_or(true);
        show.then(|| {
            view! {
                <a
                    class="home-watering-verdict is-interactive"
                    href=crate::base::url("/irrigation")
                    aria-label="Open the irrigation dashboard"
                    style=wrap_style
                >
                    <span aria-hidden="true" style=glyph_style>
                        <crate::components::ui::Icon name="droplet" size=20u32/>
                    </span>
                    <span style=word_style>
                        {word}
                    </span>
                    <span style="flex:1;min-width:0;color:var(--text-soft);\
                                 font-size:var(--text-body-sm);overflow:hidden;\
                                 text-overflow:ellipsis;white-space:nowrap;">
                        {reason}
                    </span>
                    <span aria-hidden="true" style="flex:none;display:flex;color:var(--text-faint);">
                        <crate::components::ui::Icon name="chevron-right" size=18u32/>
                    </span>
                </a>
            }
        })
    }
}

#[component]
fn NotFound() -> impl IntoView {
    view! { <div class="not-found">"404, no such page"</div> }
}

/// Server-side HTML shell that wraps the app render in a full <html>
/// document. leptos_axum hands this to axum to send as the response body
/// AND uses the same fn for the file_and_error_handler fallback.
//
// viewport-fit=cover is what makes env(safe-area-inset-*) return non-zero
// on notched iPhones. The bottom-tab nav and the standalone-PWA chrome
// both need that for safe-area padding to actually take effect.
/// Optional analytics tag for operators who run a PUBLIC instance and
/// want to measure visits with their own tool (Umami, Plausible, any
/// script-tag tracker). Strictly opt-in via two env vars read at SSR:
/// LOCALSKY_ANALYTICS_SRC (script URL) and LOCALSKY_ANALYTICS_WEBSITE_ID
/// (data-website-id). Both unset (the default) renders nothing, which
/// keeps the no-telemetry promise: a stock install never loads or sends
/// anything anywhere.
fn analytics_tag() -> Option<impl IntoView> {
    let src = std::env::var("LOCALSKY_ANALYTICS_SRC").ok()?;
    let id = std::env::var("LOCALSKY_ANALYTICS_WEBSITE_ID").ok()?;
    if src.is_empty() || id.is_empty() {
        return None;
    }
    let host_url = std::env::var("LOCALSKY_ANALYTICS_HOST_URL").unwrap_or_default();
    Some(view! {
        <script
            defer
            src=src
            data-website-id=id
            data-host-url=host_url
        ></script>
    })
}

pub fn shell(options: LeptosOptions) -> impl IntoView {
    // Ingress/prefix support (see src/base.rs). The prefix is read from the
    // per-request X-Ingress-Path header; "" for direct access. It feeds:
    //   - HydrationScripts root (prefixed /pkg/* asset URLs),
    //   - the head's static asset links,
    //   - a <meta> tag the hydrated client reads back,
    //   - a fetch/EventSource shim so the WASM app's root-relative network
    //     calls are translated at the boundary (no per-call-site changes).
    // The PWA manifest is only linked unprefixed: installing the PWA from
    // inside an embedded ingress panel is not a meaningful flow, and the
    // service worker is likewise skipped under a prefix (see lib.rs).
    let base = crate::base::base_path();
    let manifest_link = base
        .is_empty()
        .then(|| view! { <link rel="manifest" href="/manifest.webmanifest"/> });
    // base is sanitized to [A-Za-z0-9/_-.] so single-quoted embedding is
    // injection-safe.
    let shim = if base.is_empty() {
        String::new()
    } else {
        format!(
            "(function(){{\
               var B='{base}';\
               var f=window.fetch;\
               window.fetch=function(i,n){{\
                 try{{\
                   if(typeof i==='string'||i instanceof URL){{\
                     var s=String(i);\
                     if(s[0]==='/'&&s[1]!=='/'&&s.indexOf(B+'/')!==0)i=B+s;\
                   }}else if(i instanceof Request){{\
                     var u=new URL(i.url);\
                     if(u.origin===location.origin&&u.pathname.indexOf(B+'/')!==0&&u.pathname!==B){{\
                       i=new Request(B+u.pathname+u.search+u.hash,i);\
                     }}\
                   }}\
                 }}catch(e){{}}\
                 return f.call(this,i,n);\
               }};\
               var E=window.EventSource;\
               if(E){{\
                 var W=function(u,c){{\
                   var s=String(u);\
                   if(s[0]==='/'&&s[1]!=='/'&&s.indexOf(B+'/')!==0)s=B+s;\
                   return new E(s,c);\
                 }};\
                 W.prototype=E.prototype;\
                 window.EventSource=W;\
               }}\
               document.addEventListener('click',function(ev){{\
                 var a=ev.target&&ev.target.closest?ev.target.closest('a[href]'):null;\
                 if(!a)return;\
                 var h=a.getAttribute('href');\
                 if(h&&h[0]==='/'&&h[1]!=='/'&&h.indexOf(B+'/')!==0&&h!==B){{\
                   a.setAttribute('href',B+h);\
                 }}\
               }},true);\
             }})();"
        )
    };
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover"/>
                <meta name="localsky-base" content=base.clone()/>
                <script>{shim}</script>
                <AutoReload options=options.clone() />
                <HydrationScripts options=options.clone() root=base.clone()/>
                // Content-hashed leptos CSS link (hash-files). Emits
                // /pkg/localsky.<hash>.css carrying the ingress prefix (root),
                // so a new build busts the CSS cache too. Replaces the old
                // hardcoded /pkg/localsky.css <Stylesheet> in the App component.
                <HashedStylesheet options=options.clone() id="leptos" root=base.clone()/>
                <MetaTags/>
                <link rel="icon" type="image/svg+xml" href=crate::base::url("/favicon.svg")/>
                <link rel="apple-touch-icon" href=crate::base::url("/icons/apple-touch-180.png")/>
                {manifest_link}
                <meta name="theme-color" content="#0b1220"/>
                <meta name="apple-mobile-web-app-capable" content="yes"/>
                <meta name="apple-mobile-web-app-status-bar-style" content="black-translucent"/>
                <meta name="apple-mobile-web-app-title" content="LocalSky"/>
                // Operator-opt-in analytics. LocalSky itself sends nothing,
                // ever; this renders a tracker tag ONLY when the operator
                // sets both env vars (used by the hosted public demo, useful
                // for anyone running a public instance). Unset = no tag.
                {analytics_tag()}
                <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css"
                    integrity="sha256-p4NxAoJBhIIN+hmNHrzRCf9tD/miZyoHS5obTRR9BMY="
                    crossorigin=""/>
                // Leaflet + radar.js load once at app boot, not per-route.
                // When these were inside RadarPanel's view, every route
                // swap re-inserted the script tags and the browser
                // re-executed the radar.js IIFE, stacking MutationObservers
                // and racing closures so the second visit to /weather
                // sometimes showed a dead map until reload. The IIFE's
                // existing observer (in /public/radar.js) handles mount/
                // unmount of #radar-map on its own once it's set up once.
                <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"
                    integrity="sha256-20nQCchB9co0qIjJZRGuk2/Z9VM+kNiyxNV1lvTlZBo="
                    crossorigin=""
                    defer></script>
                // leaflet-velocity 2.1.4 (the wind feature's particle
                // layer), vendored under /vendor/ and served by us:
                // local-first like everything else (CSIRO BSD-style +
                // MIT windy core, header retained in the file; see
                // NOTICE). Deferred scripts execute in document order,
                // so it runs after leaflet.js (it extends window.L) and
                // before radar.js; radar.js only builds the layer when
                // L.velocityLayer exists, so a failed load degrades to
                // the wind feature being skipped.
                <script src=crate::base::url("/vendor/leaflet-velocity.min.js") defer></script>
                <script src=crate::base::url("/radar.js") defer></script>
                // Theme + kiosk-mode bootstrap. Runs synchronously before
                // first paint so [data-theme="..."] and [data-readonly]
                // attributes apply with no flash. Theme reads
                // localStorage.theme ("dark"|"light"|"hc"|"auto"); kiosk
                // mode reads localStorage.readonly ("1"|"true") and
                // adds data-readonly="true" so CSS rules can hide
                // destructive controls before any user interaction.
                <script>{r#"
                    try {
                        var t = localStorage.getItem('theme');
                        if (t && ['light','hc','auto','dark'].indexOf(t) >= 0) {
                            if (t !== 'dark') document.documentElement.setAttribute('data-theme', t);
                        }
                        var ro = localStorage.getItem('readonly');
                        if (ro === '1' || ro === 'true') {
                            document.documentElement.setAttribute('data-readonly', 'true');
                        }
                    } catch (e) { /* localStorage blocked in private mode */ }
                "#}</script>
            </head>
            <body>
                <App/>
            </body>
        </html>
    }
}
