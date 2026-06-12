// Library entry, Leptos app shell. The same module tree is used for both
// the SSR binary (compiled with feature `ssr`, runs in the axum server)
// and the WASM client (compiled with feature `hydrate`, attaches to the
// HTML the server already streamed).

// Matching budget for the lib crate. The release overflow actually hits the
// BINARY crate (see the load-bearing copy + full explanation in src/main.rs);
// recursion_limit is per-crate, so the bin needs its own and this one alone
// is NOT sufficient. Kept here as a safeguard since the lib hosts the deep
// component trees and could approach the budget on its own as they grow.
// Compile-time query budget only, no runtime cost.
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

pub mod app;
pub mod base;
pub mod components;
pub mod docs;
pub mod forecast;
pub mod gates_catalog;
pub mod ha;
pub mod history;
pub mod nav_log;
pub mod radar_catalog;
pub mod tempest;

#[cfg(feature = "hydrate")]
pub mod push_client;

#[cfg(feature = "ssr")]
pub mod api;
#[cfg(feature = "ssr")]
pub mod auth;
#[cfg(feature = "ssr")]
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
pub mod engine;
#[cfg(feature = "ssr")]
pub mod instance;
#[cfg(feature = "ssr")]
pub mod llm;
#[cfg(feature = "ssr")]
pub mod network;
#[cfg(feature = "ssr")]
pub mod notifications;
#[cfg(feature = "ssr")]
pub mod persistence;
#[cfg(feature = "ssr")]
pub mod ports;
#[cfg(feature = "ssr")]
pub mod push;
#[cfg(feature = "ssr")]
pub mod runtime;
#[cfg(feature = "ssr")]
pub mod runtime_helpers;
#[cfg(feature = "ssr")]
pub mod scheduler;
#[cfg(feature = "ssr")]
pub mod sources;
#[cfg(feature = "ssr")]
pub mod sw;
#[cfg(feature = "ssr")]
pub mod timeutil;
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
    use crate::nav_log::log_nav;
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;

    let win = match web_sys::window() {
        Some(w) => w,
        None => return,
    };

    // Service workers only exist in a secure context. Over plain HTTP (LAN
    // IP, a LAN hostname, local dev) navigator.serviceWorker is
    // undefined, so container.register() below throws an uncaught TypeError
    // mid-hydration. Bail cleanly so HTTP access still boots the app fully.
    if !win.is_secure_context() {
        log_nav("sw: skipped (insecure context)");
        return;
    }

    // Under an ingress/base prefix the app is an embedded panel on someone
    // else's origin; a service worker there would fight the host app's own
    // SW and the PWA flows are meaningless. Direct access keeps the PWA.
    if !crate::base::base_path().is_empty() {
        log_nav("sw: skipped (ingress prefix)");
        return;
    }

    // Kill switch: a stuck or buggy SW can be neutralized by setting
    //   localStorage.setItem('sw_disabled', '1')
    //   navigator.serviceWorker.getRegistrations().then(rs=>rs.forEach(r=>r.unregister()))
    // in DevTools, then reloading. The flag persists across reloads so the
    // user can debug without the SW racing them.
    if let Ok(Some(storage)) = win.local_storage() {
        if matches!(storage.get_item("sw_disabled"), Ok(Some(_))) {
            log_nav("sw: disabled via localStorage");
            return;
        }
    }

    let container = win.navigator().service_worker();

    // controllerchange fires when a new SW takes over (post-activate +
    // clients.claim()). Useful signal that fresh code is now in charge.
    let cc_cb = Closure::<dyn FnMut(_)>::new(move |_e: web_sys::Event| {
        log_nav("sw: controllerchange (new SW active)");
    });
    let _ = container
        .add_event_listener_with_callback("controllerchange", cc_cb.as_ref().unchecked_ref());
    cc_cb.forget();

    let messages_cb = Closure::<dyn FnMut(_)>::new(move |_e: web_sys::MessageEvent| {
        log_nav("sw: postMessage from SW");
    });
    let _ =
        container.add_event_listener_with_callback("message", messages_cb.as_ref().unchecked_ref());
    messages_cb.forget();

    // Kick the registration. The Promise resolves to a ServiceWorkerRegistration;
    // we don't need to do anything with it here, the browser maintains the
    // registration in storage and we just want the install/activate cycle to run.
    let promise = container.register("/sw.js");
    wasm_bindgen_futures::spawn_local(async move {
        match wasm_bindgen_futures::JsFuture::from(promise).await {
            Ok(_) => log_nav("sw: registered /sw.js"),
            Err(e) => {
                let msg = e
                    .as_string()
                    .or_else(|| {
                        js_sys::Reflect::get(&e, &wasm_bindgen::JsValue::from_str("message"))
                            .ok()
                            .and_then(|v| v.as_string())
                    })
                    .unwrap_or_else(|| "register failed".into());
                log_nav(format!("sw: register error: {msg}"));
            }
        }
    });
}
