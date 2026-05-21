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
    irrigation::{mobile::MobileZoneDetail, IrrigationPage},
    lightning::LightningPanel,
    mobile_nav::MobileNav,
    nav::TopNav,
    pressure::PressurePanel,
    radar::RadarPanel,
    rain::RainPanel,
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
                    let cb = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::MediaQueryListEvent| {
                        is_mobile.set(ev.matches());
                    });
                    let _ = mql
                        .add_event_listener_with_callback("change", cb.as_ref().unchecked_ref());
                    cb.forget();
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
                let Ok(mut es) = EventSource::new("/api/stream") else { return; };
                let Ok(mut sub) = es.subscribe("snapshot") else { return; };
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
                let Ok(mut es) = EventSource::new("/api/irrigation/stream") else { return; };
                let Ok(mut sub) = es.subscribe("snapshot") else { return; };
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
                let Ok(mut es) = EventSource::new("/api/forecast/stream") else { return; };
                let Ok(mut sub) = es.subscribe("snapshot") else { return; };
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
            <main class="page" id="main-content">
                // Skip link lives inside <main> so it ends up inside the
                // <App/> hydration root. Putting it as a sibling of <App/>
                // in the body shell desyncs SSR vs hydrate (tachys walks
                // body.firstChild expecting <main> and finds <a> instead,
                // panicking with failed_to_cast_element at hydration.rs:163).
                <a href="#main-content" class="skip-link">"Skip to main content"</a>
                <TopNav/>
                <InstallPrompt/>
                <Routes fallback=|| view! { <NotFound/> }>
                    <Route path=path!("/")
                        view=move || view! {
                            <Title text="LocalSky · Weather"/>
                            <WeatherHome snap=tempest forecast=forecast/>
                        }/>
                    <Route path=path!("/irrigation")
                        view=move || view! {
                            <Title text="LocalSky · Irrigation"/>
                            <IrrigationPage snap=irrigation/>
                        }/>
                    <Route path=path!("/irrigation/zone/:slug")
                        view=move || view! {
                            <Title text="LocalSky · Zone"/>
                            <MobileZoneDetail snap=irrigation/>
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
    view! {
        {render_hero(snap).into_any()}
        {render_panels(snap).into_any()}
        {view! { <HourlyForecast snap=forecast/> }.into_any()}
        {view! { <DailyForecast snap=forecast/> }.into_any()}
        {render_radar().into_any()}
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
                <meta name="theme-color" content="#07090f"/>
                <meta name="apple-mobile-web-app-capable" content="yes"/>
                <meta name="apple-mobile-web-app-status-bar-style" content="black-translucent"/>
                <meta name="apple-mobile-web-app-title" content="LocalSky"/>
                <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css"
                    integrity="sha256-p4NxAoJBhIIN+hmNHrzRCf9tD/miZyoHS5obTRR9BMY="
                    crossorigin=""/>
                // Theme bootstrap. Runs synchronously before first paint so
                // [data-theme="..."] tokens apply with no flash. Reads
                // localStorage.theme ("dark"|"light"|"hc"|"auto"); defaults
                // to the dark house theme when unset.
                <script>{r#"
                    try {
                        var t = localStorage.getItem('theme');
                        if (t && ['light','hc','auto','dark'].indexOf(t) >= 0) {
                            if (t !== 'dark') document.documentElement.setAttribute('data-theme', t);
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
