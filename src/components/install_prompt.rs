// Install-PWA prompt banner. Renders at the top of the page (below TopNav)
// when:
//
//   1. The browser fired a `beforeinstallprompt` event (Chrome / Edge /
//      Samsung Internet on Android + desktop Chrome). We capture the event,
//      stash it, and reveal a "Install app" button that calls .prompt()
//      on tap, that's the only way to invoke the native UA install dialog.
//
//   2. We're on iOS Safari, not yet running standalone, and the user hasn't
//      dismissed the banner. iOS doesn't fire beforeinstallprompt; the only
//      way to install a PWA is Share -> Add to Home Screen, so we render a
//      static hint pointing at it.
//
// Hidden when:
//   - already running in standalone display-mode (matchMedia or
//     navigator.standalone)
//   - the user dismissed it (localStorage `install_prompt_dismissed`)
//
// The banner is removable via an inline ✕. Dismissal is sticky in
// localStorage so the user only sees it once.
//
// SSR + hydrate's first frame both render the empty fragment so the DOM
// shape matches; the post-hydrate effect flips the signal once it's
// determined what to show.

use leptos::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Native/Ios are only set in cfg(feature="hydrate") code
enum Mode {
    Hidden,
    Native, // beforeinstallprompt available
    Ios,    // iOS Safari, not standalone
}

#[component]
pub fn InstallPrompt() -> impl IntoView {
    let mode: RwSignal<Mode> = RwSignal::new(Mode::Hidden);

    #[cfg(feature = "hydrate")]
    {
        leptos::task::spawn_local(async move {
            // One-frame yield matches the SSR/hydrate contract: the first
            // render is Mode::Hidden everywhere; we flip after hydration
            // completes so tachys never sees a different DOM shape.
            gloo_timers::future::TimeoutFuture::new(0).await;
            init_install_detection(mode);
        });
    }

    let on_install = move |_| {
        #[cfg(feature = "hydrate")]
        trigger_native_install(mode);
    };

    let on_dismiss = move |_| {
        #[cfg(feature = "hydrate")]
        {
            if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = s.set_item("install_prompt_dismissed", "1");
            }
        }
        mode.set(Mode::Hidden);
    };

    move || {
        match mode.get() {
        Mode::Hidden => ().into_any(),
        Mode::Native => view! {
            <div class="install-prompt" role="region" aria-label="Install LocalSky">
                <div class="install-prompt-icon" aria-hidden="true"><crate::components::ui::Icon name="smartphone" size=26/></div>
                <div class="install-prompt-text">
                    <div class="install-prompt-title">"Install LocalSky"</div>
                    <div class="install-prompt-body">"Add to your home screen for full-screen mode and push notifications."</div>
                </div>
                <button class="btn-clay btn-clay-good install-prompt-cta" on:click=on_install>"Install"</button>
                <button class="install-prompt-close" aria-label="Dismiss" on:click=on_dismiss>"\u{2715}"</button>
            </div>
        }.into_any(),
        Mode::Ios => view! {
            <div class="install-prompt" role="region" aria-label="Install LocalSky on iOS">
                <div class="install-prompt-icon" aria-hidden="true"><crate::components::ui::Icon name="home" size=26/></div>
                <div class="install-prompt-text">
                    <div class="install-prompt-title">"Install LocalSky"</div>
                    <div class="install-prompt-body">"Tap the Share button " <span class="install-prompt-share" aria-hidden="true"><crate::components::ui::Icon name="share" size=14/></span> " in Safari, then \u{201C}Add to Home Screen\u{201D} to install."</div>
                </div>
                <button class="install-prompt-close" aria-label="Dismiss" on:click=on_dismiss>"\u{2715}"</button>
            </div>
        }.into_any(),
    }
    }
}

#[cfg(feature = "hydrate")]
fn init_install_detection(mode: RwSignal<Mode>) {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;

    let Some(win) = web_sys::window() else { return };

    // Already dismissed? Stay hidden.
    if let Ok(Some(s)) = win.local_storage() {
        if matches!(s.get_item("install_prompt_dismissed"), Ok(Some(_))) {
            return;
        }
    }

    // Already running standalone? Stay hidden.
    if is_standalone() {
        return;
    }

    // Listen for beforeinstallprompt. Capture + stash the event so .prompt()
    // can be called later from a user gesture (.prompt() requires it).
    let mode_for_evt = mode;
    let cb = Closure::<dyn FnMut(_)>::new(move |ev: web_sys::Event| {
        // Prevent the Chrome mini-infobar; we'll show our own UI.
        ev.prevent_default();
        // Stash the event on window so the install button can call .prompt()
        // on it later. js_sys::Reflect lets us set an arbitrary property
        // without bringing in another dep.
        if let Some(w) = web_sys::window() {
            let _ = js_sys::Reflect::set(
                &w,
                &wasm_bindgen::JsValue::from_str("__lsBipEvent"),
                ev.as_ref(),
            );
        }
        mode_for_evt.set(Mode::Native);
    });
    let _ =
        win.add_event_listener_with_callback("beforeinstallprompt", cb.as_ref().unchecked_ref());
    cb.forget();

    // Hide on appinstalled. This fires after the install completes so we
    // don't keep nagging.
    let cb_installed = Closure::<dyn FnMut(_)>::new(move |_ev: web_sys::Event| {
        mode.set(Mode::Hidden);
    });
    let _ =
        win.add_event_listener_with_callback("appinstalled", cb_installed.as_ref().unchecked_ref());
    cb_installed.forget();

    // iOS Safari: detect by UA + lack of beforeinstallprompt firing.
    if is_ios_safari() && !is_standalone() {
        // Give beforeinstallprompt a beat to fire on edge browsers that
        // ship it; if none arrived after 600ms, fall back to iOS hint.
        let mode_for_timer = mode;
        leptos::task::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(600).await;
            // If the native event already set Mode::Native, leave it.
            if matches!(mode_for_timer.get_untracked(), Mode::Hidden) {
                mode_for_timer.set(Mode::Ios);
            }
        });
    }
}

#[cfg(feature = "hydrate")]
fn trigger_native_install(mode: RwSignal<Mode>) {
    use wasm_bindgen::JsCast;
    let Some(win) = web_sys::window() else { return };
    let bip = js_sys::Reflect::get(&win, &wasm_bindgen::JsValue::from_str("__lsBipEvent"))
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null());
    let Some(bip) = bip else {
        // Lost the event somehow, fall back to iOS hint copy.
        mode.set(Mode::Ios);
        return;
    };
    let prompt_fn = js_sys::Reflect::get(&bip, &wasm_bindgen::JsValue::from_str("prompt"))
        .ok()
        .and_then(|v| v.dyn_into::<js_sys::Function>().ok());
    let Some(prompt_fn) = prompt_fn else {
        mode.set(Mode::Ios);
        return;
    };
    let promise = prompt_fn.call0(&bip);
    if promise.is_err() {
        mode.set(Mode::Ios);
        return;
    }
    // Hide the banner immediately. The native dialog handles outcome
    // reporting via userChoice; we don't need to wait for it.
    mode.set(Mode::Hidden);
    // Clear the stashed event so re-clicking doesn't try to reuse it
    // (each beforeinstallprompt event is single-use).
    let _ = js_sys::Reflect::set(
        &win,
        &wasm_bindgen::JsValue::from_str("__lsBipEvent"),
        &wasm_bindgen::JsValue::UNDEFINED,
    );
}

#[cfg(feature = "hydrate")]
fn is_standalone() -> bool {
    let Some(win) = web_sys::window() else {
        return false;
    };
    if let Ok(Some(mql)) = win.match_media("(display-mode: standalone)") {
        if mql.matches() {
            return true;
        }
    }
    // iOS uses a non-standard navigator.standalone boolean.
    let nav = win.navigator();
    js_sys::Reflect::get(&nav, &wasm_bindgen::JsValue::from_str("standalone"))
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

#[cfg(feature = "hydrate")]
fn is_ios_safari() -> bool {
    let Some(win) = web_sys::window() else {
        return false;
    };
    let ua = win.navigator().user_agent().unwrap_or_default();
    let is_ios = ua.contains("iPhone") || ua.contains("iPad") || ua.contains("iPod");
    // Safari has Safari/ but not CriOS (Chrome) / FxiOS (Firefox) / EdgiOS.
    let is_safari = ua.contains("Safari/")
        && !ua.contains("CriOS/")
        && !ua.contains("FxiOS/")
        && !ua.contains("EdgiOS/");
    is_ios && is_safari
}
