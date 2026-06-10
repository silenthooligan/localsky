// Connection state for the live SSE streams. One shared ConnState is
// provided from App; every stream subscription reports activity into it
// and the PageHeader pill renders it. SSR and the first hydrate frame
// always render the Live default (status only changes client-side after
// hydration, so the DOM matches).
//
// Status model:
//   Live         - at least one stream is connected and events flow
//   Reconnecting - a stream dropped; subscribe loop is backing off
//   Offline      - the browser itself reports no network
// plus an orthogonal `stale` flag: connected but no data event for 90s
// (backend hung, station dark). Staleness clears on the next event.

use leptos::prelude::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnStatus {
    Live,
    Reconnecting,
    Offline,
}

#[derive(Clone, Copy)]
pub struct ConnState {
    pub status: RwSignal<ConnStatus>,
    pub stale: RwSignal<bool>,
    /// ms timestamp (Date.now) of the last data event on any stream.
    pub last_event_ms: RwSignal<f64>,
}

impl ConnState {
    pub fn new() -> Self {
        Self {
            status: RwSignal::new(ConnStatus::Live),
            stale: RwSignal::new(false),
            last_event_ms: RwSignal::new(0.0),
        }
    }
}

impl Default for ConnState {
    fn default() -> Self {
        Self::new()
    }
}

pub fn use_conn_state() -> Option<ConnState> {
    use_context::<ConnState>()
}

#[cfg(feature = "hydrate")]
fn now_ms() -> f64 {
    js_sys::Date::now()
}

/// Subscribe to one SSE stream forever: parse each `snapshot` event into T,
/// write it into `set`, and report connection health into `conn`. On stream
/// error/end, flips to Reconnecting and retries with capped exponential
/// backoff. Never returns.
#[cfg(feature = "hydrate")]
pub fn subscribe_sse<T>(url: &'static str, set: WriteSignal<T>, conn: ConnState)
where
    T: serde::de::DeserializeOwned + Send + Sync + 'static,
{
    use futures::StreamExt;
    use gloo_net::eventsource::futures::EventSource;

    leptos::task::spawn_local(async move {
        let mut backoff_ms: u32 = 1_000;
        loop {
            let connected = async {
                let mut es = EventSource::new(url).ok()?;
                let mut sub = es.subscribe("snapshot").ok()?;
                let mut got_any = false;
                while let Some(Ok((_, msg))) = sub.next().await {
                    if let Some(payload) = msg.data().as_string() {
                        if let Ok(s) = serde_json::from_str::<T>(&payload) {
                            set.set(s);
                            got_any = true;
                            backoff_ms = 1_000;
                            conn.last_event_ms.set(now_ms());
                            conn.stale.set(false);
                            if conn.status.get_untracked() != ConnStatus::Live {
                                conn.status.set(ConnStatus::Live);
                            }
                        }
                    }
                }
                // Keep `es` alive until the subscription ends.
                drop(es);
                Some(got_any)
            }
            .await;
            let _ = connected;

            // Stream ended or failed to open: back off and retry. Only
            // downgrade Live -> Reconnecting; never stomp Offline (the
            // window listeners own that).
            if conn.status.get_untracked() == ConnStatus::Live {
                conn.status.set(ConnStatus::Reconnecting);
            }
            gloo_timers::future::TimeoutFuture::new(backoff_ms).await;
            backoff_ms = (backoff_ms * 2).min(30_000);
        }
    });
}

/// Wire window online/offline listeners + the staleness watchdog.
/// Call once from App after hydration.
#[cfg(feature = "hydrate")]
pub fn spawn_conn_watchdog(conn: ConnState) {
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;

    if let Some(win) = web_sys::window() {
        let on_offline = Closure::<dyn FnMut()>::new(move || {
            conn.status.set(ConnStatus::Offline);
        });
        let on_online = Closure::<dyn FnMut()>::new(move || {
            // Streams reconnect on their own; show the intermediate state.
            conn.status.set(ConnStatus::Reconnecting);
        });
        let _ =
            win.add_event_listener_with_callback("offline", on_offline.as_ref().unchecked_ref());
        let _ = win.add_event_listener_with_callback("online", on_online.as_ref().unchecked_ref());
        on_offline.forget();
        on_online.forget();
    }

    leptos::task::spawn_local(async move {
        loop {
            gloo_timers::future::TimeoutFuture::new(15_000).await;
            let last = conn.last_event_ms.get_untracked();
            if conn.status.get_untracked() == ConnStatus::Live
                && last > 0.0
                && now_ms() - last > 90_000.0
            {
                conn.stale.set(true);
            }
        }
    });
}

/// Status pill for the page header. Renders nothing while Live and fresh
/// (the healthy steady state needs no chrome); pops a labeled dot when
/// reconnecting, offline, or stale.
#[component]
pub fn ConnPill() -> impl IntoView {
    let conn = use_conn_state();
    move || {
        let Some(conn) = conn else {
            return ().into_any();
        };
        let status = conn.status.get();
        let stale = conn.stale.get();
        let (class, label) = match (status, stale) {
            (ConnStatus::Offline, _) => ("conn-pill conn-pill--offline", "Offline"),
            (ConnStatus::Reconnecting, _) => ("conn-pill conn-pill--reconnect", "Reconnecting"),
            (ConnStatus::Live, true) => ("conn-pill conn-pill--stale", "Stale data"),
            (ConnStatus::Live, false) => return ().into_any(),
        };
        view! {
            <span class=class role="status">
                <span class="conn-pill__dot" aria-hidden="true"></span>
                {label}
            </span>
        }
        .into_any()
    }
}
