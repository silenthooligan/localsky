// Library entry — Leptos app shell. The same module tree is used for both
// the SSR binary (compiled with feature `ssr`, runs in the axum server)
// and the WASM client (compiled with feature `hydrate`, attaches to the
// HTML the server already streamed).

pub mod app;
pub mod components;
pub mod forecast;
pub mod ha;
pub mod history;
pub mod nav_log;
pub mod tempest;

#[cfg(feature = "hydrate")]
pub mod push_client;

#[cfg(feature = "ssr")]
pub mod api;
#[cfg(feature = "ssr")]
pub mod config;
#[cfg(feature = "ssr")]
pub mod controllers;
#[cfg(feature = "ssr")]
pub mod engine;
#[cfg(feature = "ssr")]
pub mod llm;
#[cfg(feature = "ssr")]
pub mod notifications;
#[cfg(feature = "ssr")]
pub mod persistence;
#[cfg(feature = "ssr")]
pub mod ports;
#[cfg(feature = "ssr")]
pub mod push;
#[cfg(feature = "ssr")]
pub mod demo_data;
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
    let _ = container
        .add_event_listener_with_callback("message", messages_cb.as_ref().unchecked_ref());
    messages_cb.forget();

    // Kick the registration. The Promise resolves to a ServiceWorkerRegistration;
    // we don't need to do anything with it here — the browser maintains the
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
