// Two-step Stop-All confirmation. The mobile UX wraps the existing
// StopAllPanel button in a confirmation sheet so a stray tap doesn't
// kill an in-progress watering by accident.
//
// Visibility is owned by the parent (a single RwSignal<bool>); this
// component is always mounted, opens when visible.set(true), closes
// on either confirm or cancel.

use crate::components::irrigation::controls::post_action;
use leptos::prelude::*;
use serde_json::json;

#[component]
pub fn StopAllConfirm(visible: RwSignal<bool>, running_count: Signal<usize>) -> impl IntoView {
    let close = move |_| visible.set(false);
    let confirm = move |_| {
        post_action(json!({"kind": "stop_all"}));
        visible.set(false);
    };

    move || {
        if !visible.get() {
            return ().into_any();
        }
        let n = running_count.get();
        let body = if n == 1 {
            "Stop the running zone?".to_string()
        } else {
            format!("Stop {n} running zones?")
        };

        view! {
            <div class="bottom-sheet-backdrop" on:click=close aria-hidden="true"></div>
            <div class="bottom-sheet bottom-sheet-confirm" role="dialog" aria-modal="true">
                <div class="bottom-sheet-handle" aria-hidden="true"></div>
                <div class="bottom-sheet-title">"Stop all?"</div>
                <p class="bottom-sheet-body">{body}</p>
                <div class="bottom-sheet-actions">
                    <button class="btn-clay" on:click=close>"Cancel"</button>
                    <button class="btn-clay btn-clay-hot" on:click=confirm>"Stop all"</button>
                </div>
            </div>
        }
        .into_any()
    }
}
