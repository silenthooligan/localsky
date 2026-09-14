// Library entry, Leptos app shell. The same module tree is used for both
// the SSR binary (compiled with feature `ssr`, runs in the axum server)
// and the WASM client (compiled with feature `hydrate`, attaches to the
// HTML the server already streamed).

// The query budget the release build needs. The overflow ("queries
// overflow the depth limit!") hits wherever leptos_axum's
// generate_route_list + LeptosRoutes + the SSR shell monomorphize the
// whole component tree in one place: that is `boot::api` in THIS crate
// since the boot moved out of main.rs. recursion_limit is per-crate, so
// the bin keeps its own copy for the day something deep lands there
// again. Compile-time only, no runtime cost.
#![recursion_limit = "512"]
// Lint baseline: stylistic clippy classes the codebase predates. CI
// runs -D warnings; these allows keep that gate meaningful for new
// warning classes while the baseline is burned down over time.
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::too_many_arguments)]
#![allow(clippy::type_complexity)]
#![allow(clippy::doc_lazy_continuation)]
#![allow(clippy::doc_overindented_list_items)]
#![allow(clippy::unused_unit)]
#![allow(clippy::unit_arg)]
#![allow(clippy::manual_clamp)]

pub mod agronomy;
pub mod app;
pub mod base;
pub mod components;
pub mod docs;
pub mod explain;
pub mod forecast;
pub mod gates_catalog;
pub mod history;
pub mod model;
pub mod radar_catalog;
pub mod reason_render;
// The controller-station shapes. Ungated on purpose: dispatch (runtime),
// the config check (config::validate) and the zone editor all judge the
// same values, and a validator that disagrees with dispatch is worse than
// no validator.
pub mod station_id;
pub mod tempest;
pub mod text;
pub mod timefmt;
pub mod units;
pub mod voice;
pub mod weather;

#[cfg(feature = "hydrate")]
pub mod push_client;

#[cfg(feature = "ssr")]
pub mod api;
#[cfg(feature = "ssr")]
pub mod auth;
// Config types, catalogs and the validator are pure and compile for the
// browser too; only its disk-touching submodules are server-gated.
pub mod config;
#[cfg(feature = "ssr")]
pub mod controllers;
#[cfg(feature = "ssr")]
pub mod demo_data;
#[cfg(feature = "ssr")]
pub mod devices;
#[cfg(feature = "ssr")]
pub mod discovery;
#[cfg(feature = "ssr")]
pub mod docs_serve;
// The engine is pure arithmetic: no IO, no async, no database, in any of
// its modules. It is shared so the UI can CALL it rather than carry a
// second copy of what it computes; only `engine::scripting` (the Rhai
// interpreter) stays server-side. What remains server-gated below is the
// machinery around the engine that genuinely needs a server: the clock
// and history the refresher owns, persistence, controllers, the API.
pub mod engine;
#[cfg(feature = "ssr")]
pub mod ha_adopt;
#[cfg(feature = "ssr")]
pub mod instance;
#[cfg(feature = "ssr")]
pub mod integrations;
#[cfg(feature = "ssr")]
pub mod llm;
#[cfg(feature = "ssr")]
pub mod mdns;
#[cfg(feature = "ssr")]
pub mod metrics;
#[cfg(feature = "ssr")]
pub mod net;
#[cfg(feature = "ssr")]
pub mod notifications;
#[cfg(feature = "ssr")]
pub mod persistence;
#[cfg(feature = "ssr")]
pub mod ports;
#[cfg(feature = "ssr")]
pub mod push;
// The irrigation snapshot refresher: the ten-second tick that reads the
// stores, decides, and stores the result. It supports Home Assistant or
// native sources, which is why it is not under an integration. The
// forecast has its own poller, `forecast::open_meteo`.
#[cfg(feature = "ssr")]
pub mod assembly;
#[cfg(feature = "ssr")]
pub mod boot;
#[cfg(feature = "ssr")]
pub mod logring;
#[cfg(feature = "ssr")]
pub mod refresher;
#[cfg(feature = "ssr")]
pub mod runtime;
#[cfg(feature = "ssr")]
pub mod runtime_helpers;
#[cfg(feature = "ssr")]
pub mod scheduler;
#[cfg(feature = "ssr")]
pub mod setup_gate;
#[cfg(feature = "ssr")]
pub mod sources;
#[cfg(feature = "ssr")]
pub mod sw;
#[cfg(feature = "ssr")]
pub mod timeutil;
#[cfg(feature = "ssr")]
pub mod tuning;
#[cfg(feature = "ssr")]
pub mod updates;
#[cfg(feature = "ssr")]
pub mod zones;

#[cfg(feature = "hydrate")]
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn hydrate() {
    use crate::app::App;
    console_error_panic_hook::set_once();
    leptos::mount::hydrate_body(App);
    register_service_worker();
}

#[cfg(feature = "hydrate")]
fn register_service_worker() {
    let win = match web_sys::window() {
        Some(w) => w,
        None => return,
    };

    // Service workers only exist in a secure context. Over plain HTTP (LAN
    // IP, a LAN hostname, local dev) navigator.serviceWorker is
    // undefined, so container.register() below throws an uncaught TypeError
    // mid-hydration. Bail cleanly so HTTP access still boots the app fully.
    if !win.is_secure_context() {
        return;
    }

    // Under an ingress/base prefix the app is an embedded panel on someone
    // else's origin; a service worker there would fight the host app's own
    // SW and the PWA flows are meaningless. Direct access keeps the PWA.
    if !crate::base::base_path().is_empty() {
        return;
    }

    // Kill switch: a stuck or buggy SW can be neutralized by setting
    //   localStorage.setItem('sw_disabled', '1')
    //   navigator.serviceWorker.getRegistrations().then(rs=>rs.forEach(r=>r.unregister()))
    // in DevTools, then reloading. The flag persists across reloads so the
    // user can debug without the SW racing them.
    if let Ok(Some(storage)) = win.local_storage() {
        if matches!(storage.get_item("sw_disabled"), Ok(Some(_))) {
            return;
        }
    }

    let container = win.navigator().service_worker();

    // Kick the registration. The Promise resolves to a ServiceWorkerRegistration;
    // we don't need to do anything with it here, the browser maintains the
    // registration in storage and we just want the install/activate cycle to run.
    // A failure is logged to the console and otherwise ignored: the app runs
    // fully without the worker.
    let promise = container.register("/sw.js");
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = wasm_bindgen_futures::JsFuture::from(promise).await {
            web_sys::console::warn_2(&wasm_bindgen::JsValue::from_str("sw register failed"), &e);
        }
    });
}
