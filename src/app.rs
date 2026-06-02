// Top-level Leptos shell. The SSR pass reads the current Tempest +
// Irrigation snapshots out of context (the axum side `provide_context`s
// both Arc<TempestStore> and Arc<IrrigationStore>) so the first render
// is fully hydrated with live values — no spinner, no flash. After
// hydration, the browser subscribes to the matching SSE streams and
// replaces each signal on every server-pushed snapshot.

use crate::components::{
    footer::Footer,
    forecast::{DailyForecast, HourlyForecast},
    hero::Hero,
    install_prompt::InstallPrompt,
    irrigation::{
        mobile::MobileZoneDetail, IrrigationBudgetPage, IrrigationHistoryPage, IrrigationPage,
        IrrigationZonesPage,
    },
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

// (forecast/irrigation/tempest store imports removed — the SSR initial
// snapshot helpers don't read from the stores anymore; see comment on
// initial_*_ssr below for the rationale.)

// Why every initial_*_ssr returns a default value (even on the SSR
// build): the WASM hydrate-side signals init to ::default(), and any
// view tree whose CHILD COUNT depends on Vec length (the 7-day
// forecast row, the 48-hour hourly chart, the 7-day verdict strip,
// etc.) will produce different numbers of DOM children on SSR vs on
// hydrate's first render — which crashes tachys's hydration walker
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

    // Nav debug ring buffer is preserved as a developer affordance — log_nav()
    // calls scattered through the app no-op when the sink isn't installed, so
    // we can re-enable the in-page strip by reinstalling install_sink() and
    // re-rendering <NavLogStrip/> in the view tree below if we ever need it.
    // The visible strip was a debug build artifact; intentionally not rendered
    // in prod.
    let (nav_debug, _set_nav_debug) = signal::<Vec<String>>(Vec::new());
    provide_context(nav_debug);

    // Viewport flag for layout decisions. SSR + hydrate's first frame both
    // see `false` (desktop), so the initial DOM tree matches and tachys
    // hydrates cleanly. Post-hydrate we read window.matchMedia and flip the
    // signal — descendants reading via use_context::<RwSignal<bool>> get a
    // signal-driven update, no remount.
    let is_mobile: RwSignal<bool> = RwSignal::new(false);
    provide_context(is_mobile);

    // Toast hub — provided once here so any component can call
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
    provide_context(NerdMode(nerd_mode));
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
                    if let Ok(Some(v)) = storage.get_item("nerd_mode") {
                        nerd_mode.set(v == "1" || v == "true");
                    }
                }
            }
        });
        Effect::new(move |_| {
            let v = nerd_mode.get();
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

    // Mode banner: one-shot fetch of /api/v1/info on hydrate. If the
    // server reports LOCALSKY_SMART_DRY_RUN=1 we set data-dry-run on
    // <html> so the stylesheet drops in a fixed warning bar — the
    // morning scheduler logs dispatch but never waters, and without
    // this banner "nothing happened at 6 AM" looks like a regression
    // instead of an intentional override. Same treatment for the demo
    // flag (synthetic weather, recording-only controllers).
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
            if let Some(win) = web_sys::window() {
                if let Some(doc) = win.document() {
                    if let Some(html) = doc.document_element() {
                        if dry_run {
                            let _ = html.set_attribute("data-dry-run", "true");
                        }
                        if demo {
                            let _ = html.set_attribute("data-demo", "true");
                        }
                    }
                }
            }
        });
    }

    // On the client, open one EventSource per stream and overwrite the
    // matching signal on every event. Runs only after hydration.
    #[cfg(feature = "hydrate")]
    {
        // Tempest stream
        Effect::new(move |_| {
            use gloo_net::eventsource::futures::EventSource;
            use leptos::task::spawn_local;
            spawn_local(async move {
                let Ok(mut es) = EventSource::new("/api/stream") else {
                    return;
                };
                let Ok(mut sub) = es.subscribe("snapshot") else {
                    return;
                };
                use futures::StreamExt;
                while let Some(Ok((_, msg))) = sub.next().await {
                    if let Some(payload) = msg.data().as_string() {
                        if let Ok(s) = serde_json::from_str::<Snapshot>(&payload) {
                            set_tempest.set(s);
                        }
                    }
                }
            });
        });

        // Irrigation stream
        Effect::new(move |_| {
            use gloo_net::eventsource::futures::EventSource;
            use leptos::task::spawn_local;
            spawn_local(async move {
                let Ok(mut es) = EventSource::new("/api/irrigation/stream") else {
                    return;
                };
                let Ok(mut sub) = es.subscribe("snapshot") else {
                    return;
                };
                use futures::StreamExt;
                while let Some(Ok((_, msg))) = sub.next().await {
                    if let Some(payload) = msg.data().as_string() {
                        if let Ok(s) = serde_json::from_str::<IrrigationSnapshot>(&payload) {
                            set_irrigation.set(s);
                        }
                    }
                }
            });
        });

        // Forecast stream
        Effect::new(move |_| {
            use gloo_net::eventsource::futures::EventSource;
            use leptos::task::spawn_local;
            spawn_local(async move {
                let Ok(mut es) = EventSource::new("/api/forecast/stream") else {
                    return;
                };
                let Ok(mut sub) = es.subscribe("snapshot") else {
                    return;
                };
                use futures::StreamExt;
                while let Some(Ok((_, msg))) = sub.next().await {
                    if let Some(payload) = msg.data().as_string() {
                        if let Ok(s) = serde_json::from_str::<ForecastSnapshot>(&payload) {
                            set_forecast.set(s);
                        }
                    }
                }
            });
        });
    }

    view! {
        <Stylesheet id="leptos" href="/pkg/localsky.css"/>
        <Title text="LocalSky"/>

        <Router>
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
                <Routes fallback=|| view! { <NotFound/> }>
                    <Route path=path!("/")
                        view=move || view! {
                            <Title text="LocalSky · Weather"/>
                            <WeatherHome snap=tempest forecast=forecast/>
                        }/>
                    <Route path=path!("/irrigation")
                        view=move || view! {
                            <Title text="LocalSky · Irrigation · Today"/>
                            <IrrigationPage snap=irrigation/>
                        }/>
                    <Route path=path!("/irrigation/zones")
                        view=move || view! {
                            <Title text="LocalSky · Irrigation · Zones"/>
                            <IrrigationZonesPage snap=irrigation/>
                        }/>
                    <Route path=path!("/irrigation/budget")
                        view=move || view! {
                            <Title text="LocalSky · Irrigation · Water budget"/>
                            <IrrigationBudgetPage snap=irrigation/>
                        }/>
                    <Route path=path!("/irrigation/history")
                        view=move || view! {
                            <Title text="LocalSky · Irrigation · History"/>
                            <IrrigationHistoryPage snap=irrigation/>
                        }/>
                    <Route path=path!("/irrigation/zone/:slug")
                        view=move || view! {
                            <Title text="LocalSky · Zone"/>
                            <MobileZoneDetail snap=irrigation/>
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
                    <Route path=path!("/settings/notifications")
                        view=|| view! {
                            <Title text="LocalSky · Notifications"/>
                            <crate::components::settings::SettingsNotifications/>
                        }/>
                    <Route path=path!("/settings/sources")
                        view=|| view! {
                            <Title text="LocalSky · Sources"/>
                            <crate::components::settings::SettingsSources/>
                        }/>
                    <Route path=path!("/settings/controllers")
                        view=|| view! {
                            <Title text="LocalSky · Controllers"/>
                            <crate::components::settings::SettingsControllers/>
                        }/>
                    <Route path=path!("/settings/devices")
                        view=|| view! {
                            <Title text="LocalSky · Devices"/>
                            <crate::components::settings::SettingsDevices/>
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
) -> impl IntoView {
    // Split into sibling helper fns and type-erase each via .into_any() so
    // the inner view trees don't propagate up into WeatherHome's
    // monomorphized type. Without the erasure, rustc walks every nested
    // HtmlElement+ attrs tuple and overflows its query depth at the
    // panel grid.
    // Two-column wide-screen layout: everything weather-related stacks in the
    // left column; the radar map fills the right column at full height. Below
    // 1400px the wrappers are `display: contents` so the existing single-
    // column flow is bit-for-bit identical to the old WeatherHome.
    view! {
        <div class="weather-layout">
            <div class="weather-main">
                {render_hero(snap).into_any()}
                {view! { <crate::components::weather_telemetry::WeatherTelemetry snap/> }.into_any()}
                {render_panels(snap).into_any()}
                {view! { <HourlyForecast snap=forecast/> }.into_any()}
                {view! { <DailyForecast snap=forecast/> }.into_any()}
            </div>
            <div class="weather-side">
                {render_radar().into_any()}
            </div>
        </div>
        {render_footer(snap).into_any()}
    }
}

fn render_hero(snap: ReadSignal<Snapshot>) -> impl IntoView {
    view! { <Hero snap/> }
}

fn render_panels(snap: ReadSignal<Snapshot>) -> impl IntoView {
    view! {
        <section class="grid">
            {view! { <WindPanel snap/> }.into_any()}
            {view! { <RainPanel snap/> }.into_any()}
            {view! { <LightningPanel snap/> }.into_any()}
            {view! { <PressurePanel snap/> }.into_any()}
            {view! { <SolarPanel snap/> }.into_any()}
        </section>
    }
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

#[component]
fn NotFound() -> impl IntoView {
    view! { <div class="not-found">"404 — no such page"</div> }
}

/// Server-side HTML shell that wraps the app render in a full <html>
/// document. leptos_axum hands this to axum to send as the response body
/// AND uses the same fn for the file_and_error_handler fallback.
//
// viewport-fit=cover is what makes env(safe-area-inset-*) return non-zero
// on notched iPhones. The bottom-tab nav and the standalone-PWA chrome
// both need that for safe-area padding to actually take effect.
pub fn shell(options: LeptosOptions) -> impl IntoView {
    view! {
        <!DOCTYPE html>
        <html lang="en">
            <head>
                <meta charset="utf-8"/>
                <meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover"/>
                <AutoReload options=options.clone() />
                <HydrationScripts options/>
                <MetaTags/>
                <link rel="icon" type="image/svg+xml" href="/favicon.svg"/>
                <link rel="apple-touch-icon" href="/icons/apple-touch-180.png"/>
                <link rel="manifest" href="/manifest.webmanifest"/>
                <meta name="theme-color" content="#0b1220"/>
                <meta name="apple-mobile-web-app-capable" content="yes"/>
                <meta name="apple-mobile-web-app-status-bar-style" content="black-translucent"/>
                <meta name="apple-mobile-web-app-title" content="LocalSky"/>
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
                <script src="/radar.js" defer></script>
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
