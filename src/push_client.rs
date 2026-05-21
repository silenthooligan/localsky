// Frontend push helpers (hydrate-only). Wraps the browser's PushManager
// + Notification APIs into a small async surface the schedule tab can call.
//
// Flow:
//   1. permission_state() -> "granted" | "denied" | "default"
//   2. fetch_vapid_key() -> the server's VAPID public key (base64url)
//   3. subscribe() -> requests Notification permission, calls
//      pushManager.subscribe with the VAPID key, POSTs the resulting
//      endpoint+keys to /api/push/subscribe.
//   4. unsubscribe() -> tears down the browser-side subscription and
//      DELETE /api/push/subscribe.
//   5. is_subscribed() -> queries pushManager.getSubscription().
//
// Errors are surfaced as Result<_, String> with the JS error message
// extracted, so the UI can show what actually went wrong.

#![cfg(feature = "hydrate")]

use base64::Engine;
use gloo_net::http::Request;
use serde_json::json;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    js_sys, PushSubscription, PushSubscriptionOptionsInit,
};

fn err_to_string(v: &wasm_bindgen::JsValue) -> String {
    v.as_string()
        .or_else(|| {
            js_sys::Reflect::get(v, &wasm_bindgen::JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| "(unknown error)".to_string())
}

/// Returns "granted" | "denied" | "default", or an error if Notifications
/// aren't available (e.g. iOS Safari without an installed PWA).
pub fn permission_state() -> Result<String, String> {
    let win = web_sys::window().ok_or("no window")?;
    // Notification.permission is a static property on the Notification
    // constructor, accessed via Reflect because web-sys doesn't expose
    // a static accessor consistently across versions.
    let notif = js_sys::Reflect::get(&win, &"Notification".into())
        .map_err(|e| err_to_string(&e))?;
    if notif.is_undefined() || notif.is_null() {
        return Err("Notifications API unavailable".into());
    }
    let perm = js_sys::Reflect::get(&notif, &"permission".into())
        .map_err(|e| err_to_string(&e))?;
    Ok(perm.as_string().unwrap_or_else(|| "default".to_string()))
}

pub async fn request_permission() -> Result<String, String> {
    let win = web_sys::window().ok_or("no window")?;
    let notif = js_sys::Reflect::get(&win, &"Notification".into())
        .map_err(|e| err_to_string(&e))?;
    let req_fn = js_sys::Reflect::get(&notif, &"requestPermission".into())
        .map_err(|e| err_to_string(&e))?;
    let func: js_sys::Function = req_fn.dyn_into().map_err(|_| "requestPermission not a function")?;
    let promise: js_sys::Promise = func
        .call0(&notif)
        .map_err(|e| err_to_string(&e))?
        .dyn_into()
        .map_err(|_| "requestPermission did not return Promise")?;
    let result = JsFuture::from(promise).await.map_err(|e| err_to_string(&e))?;
    Ok(result.as_string().unwrap_or_else(|| "default".to_string()))
}

pub async fn fetch_vapid_key() -> Result<String, String> {
    let resp = Request::get("/api/push/vapid-key")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() != 200 {
        return Err(format!("vapid-key {}", resp.status()));
    }
    #[derive(serde::Deserialize)]
    struct Body {
        public_key: String,
    }
    let body: Body = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body.public_key)
}

pub async fn current_subscription() -> Result<Option<PushSubscription>, String> {
    let win = web_sys::window().ok_or("no window")?;
    let sw = win.navigator().service_worker();
    let reg = JsFuture::from(sw.ready().map_err(|e| err_to_string(&e))?)
        .await
        .map_err(|e| err_to_string(&e))?;
    let reg: web_sys::ServiceWorkerRegistration = reg
        .dyn_into()
        .map_err(|_| "ready did not return registration")?;
    let pm = reg.push_manager().map_err(|e| err_to_string(&e))?;
    let sub = JsFuture::from(pm.get_subscription().map_err(|e| err_to_string(&e))?)
        .await
        .map_err(|e| err_to_string(&e))?;
    if sub.is_null() || sub.is_undefined() {
        return Ok(None);
    }
    Ok(Some(sub.dyn_into().map_err(|_| "not a PushSubscription")?))
}

pub async fn is_subscribed() -> bool {
    matches!(current_subscription().await, Ok(Some(_)))
}

/// Subscribe the browser, persist server-side. Idempotent: if a
/// subscription already exists, returns it without reprompting.
pub async fn subscribe() -> Result<(), String> {
    let perm = match permission_state() {
        Ok(p) => p,
        Err(e) => return Err(e),
    };
    if perm == "denied" {
        return Err("Notifications denied. Enable in your browser settings.".into());
    }
    if perm == "default" {
        let new_perm = request_permission().await?;
        if new_perm != "granted" {
            return Err("Permission not granted".into());
        }
    }

    let win = web_sys::window().ok_or("no window")?;
    let sw = win.navigator().service_worker();
    let reg = JsFuture::from(sw.ready().map_err(|e| err_to_string(&e))?)
        .await
        .map_err(|e| err_to_string(&e))?;
    let reg: web_sys::ServiceWorkerRegistration = reg
        .dyn_into()
        .map_err(|_| "ready did not return registration")?;
    let pm = reg.push_manager().map_err(|e| err_to_string(&e))?;

    let key = fetch_vapid_key().await?;
    let key_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&key)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(&key))
        .map_err(|e| format!("vapid key decode: {e}"))?;
    let key_array = js_sys::Uint8Array::from(&key_bytes[..]);

    let opts = PushSubscriptionOptionsInit::new();
    opts.set_user_visible_only(true);
    opts.set_application_server_key(&key_array);

    let sub = JsFuture::from(
        pm.subscribe_with_options(&opts).map_err(|e| err_to_string(&e))?,
    )
    .await
    .map_err(|e| err_to_string(&e))?;
    let sub: PushSubscription = sub.dyn_into().map_err(|_| "subscribe returned non-PushSubscription")?;

    let endpoint = sub.endpoint();
    let p256dh = b64u(extract_key(&sub, web_sys::PushEncryptionKeyName::P256dh)?)?;
    let auth = b64u(extract_key(&sub, web_sys::PushEncryptionKeyName::Auth)?)?;

    let payload = json!({
        "endpoint": endpoint,
        "keys": { "p256dh": p256dh, "auth": auth }
    })
    .to_string();
    let resp = Request::post("/api/push/subscribe")
        .header("Content-Type", "application/json")
        .body(payload)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() != 200 {
        return Err(format!("subscribe POST {}", resp.status()));
    }
    Ok(())
}

pub async fn unsubscribe() -> Result<(), String> {
    let sub = current_subscription().await?;
    let Some(sub) = sub else { return Ok(()) };
    let endpoint = sub.endpoint();
    // Tell the browser first; if that fails, we don't want to leave the
    // server with a row pointing at a still-active subscription.
    let _ = JsFuture::from(sub.unsubscribe().map_err(|e| err_to_string(&e))?)
        .await
        .map_err(|e| err_to_string(&e))?;

    let payload = json!({ "endpoint": endpoint }).to_string();
    let resp = Request::post("/api/push/unsubscribe")
        .header("Content-Type", "application/json")
        .body(payload)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status() != 200 {
        return Err(format!("unsubscribe POST {}", resp.status()));
    }
    Ok(())
}

fn extract_key(sub: &PushSubscription, kind: web_sys::PushEncryptionKeyName) -> Result<Vec<u8>, String> {
    let buf = sub.get_key(kind).map_err(|e| err_to_string(&e))?;
    let buf = match buf {
        Some(b) => b,
        None => return Err(format!("missing key: {kind:?}")),
    };
    let array = js_sys::Uint8Array::new(&buf);
    let mut out = vec![0u8; array.length() as usize];
    array.copy_to(&mut out);
    Ok(out)
}

fn b64u(bytes: Vec<u8>) -> Result<String, String> {
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
