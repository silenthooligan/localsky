//! One app-owned status stream for the picker and the persistent watering strip.
use crate::model::quick_run::{QuickRunStatus, QuickRunView};
use leptos::prelude::*;

#[derive(Clone, Copy)]
pub struct QuickRunClient {
    pub state: RwSignal<Option<QuickRunView>>,
    pub pending: RwSignal<bool>,
    pub error: RwSignal<String>,
    pub offline: RwSignal<bool>,
    pub revision: RwSignal<u64>,
    pub watching: RwSignal<bool>,
    pub clock: RwSignal<i64>,
    pub receive: Callback<Result<QuickRunStatus, String>>,
}

impl QuickRunClient {
    pub fn active(self) -> bool {
        self.state
            .get()
            .and_then(|s| s.run)
            .is_some_and(|r| r.phase.active() || r.stop_unconfirmed)
    }

    pub fn stop(self) {
        if self.pending.get_untracked() {
            return;
        }
        let Some(run) = self
            .state
            .get_untracked()
            .and_then(|s| s.run)
            .filter(|r| r.phase.active() || r.stop_unconfirmed)
        else {
            return;
        };
        self.pending.set(true);
        self.revision.update(|v| *v += 1);
        self.error.set(String::new());
        super::quick_run::send(true, serde_json::json!({"id":run.id}), self.receive);
    }
}

pub fn provide_quick_run_client(#[allow(unused_variables)] has_irrigation: RwSignal<bool>) {
    let state = RwSignal::new(None::<QuickRunView>);
    let pending = RwSignal::new(false);
    let error = RwSignal::new(String::new());
    let offline = RwSignal::new(false);
    let revision = RwSignal::new(0u64);
    let clock = RwSignal::new(0i64);
    // Created by App, not a route: a response after navigation still clears
    // pending and updates every control surface.
    let receive = Callback::new(move |result: Result<QuickRunStatus, String>| {
        pending.set(false);
        revision.update(|v| *v += 1);
        match result {
            Ok(run) => {
                state.update(|s| {
                    if let Some(s) = s {
                        s.run = Some(run);
                    }
                });
                offline.set(false);
                clock.set(chrono::Utc::now().timestamp());
            }
            Err(message) => error.set(message),
        }
    });
    let client = QuickRunClient {
        state,
        pending,
        error,
        offline,
        revision,
        watching: RwSignal::new(false),
        clock,
        receive,
    };
    provide_context(client);
    #[cfg(feature = "hydrate")]
    {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cleanup = cancelled.clone();
        on_cleanup(move || cleanup.store(true, std::sync::atomic::Ordering::Relaxed));
        leptos::task::spawn_local(async move {
            gloo_timers::future::TimeoutFuture::new(0).await;
            while !cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                let token = client.revision.get_untracked();
                if has_irrigation.get_untracked() && !client.pending.get_untracked() {
                    let result = async {
                        let response = gloo_net::http::Request::get(&crate::base::url(
                            "/api/v1/irrigation/quick-run",
                        ))
                        .send()
                        .await
                        .ok()?;
                        if !response.ok() {
                            return None;
                        }
                        response.json::<QuickRunView>().await.ok()
                    }
                    .await;
                    if cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                        return;
                    }
                    if client.revision.get_untracked() == token && !client.pending.get_untracked() {
                        client.offline.set(result.is_none());
                        if let Some(value) = result {
                            client.state.set(Some(value));
                        }
                    }
                }
                client.clock.set(chrono::Utc::now().timestamp());
                let active = client
                    .state
                    .get_untracked()
                    .and_then(|s| s.run)
                    .is_some_and(|r| r.phase.active() || r.stop_unconfirmed);
                gloo_timers::future::TimeoutFuture::new(
                    if client.watching.get_untracked() || active {
                        1_000
                    } else {
                        5_000
                    },
                )
                .await;
            }
        });
    }
}
