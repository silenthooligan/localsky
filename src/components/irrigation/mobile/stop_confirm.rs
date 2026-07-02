// Two-step Stop-All confirmation. The mobile UX wraps the existing
// StopAllPanel button in a confirmation sheet so a stray tap doesn't
// kill an in-progress watering by accident.
//
// Visibility is owned by the parent (a single RwSignal<bool>); this
// component is always mounted, opens when visible.set(true), closes
// on either confirm or cancel.

use crate::components::irrigation::controls::{post_action_then, toast_on_err};
use crate::components::ui::{Button, Sheet};
use leptos::prelude::*;
use serde_json::json;

#[component]
pub fn StopAllConfirm(visible: RwSignal<bool>, running_count: Signal<usize>) -> impl IntoView {
    let stop_done = toast_on_err("Stop all failed; zones may still be running");
    let close = move |_| visible.set(false);
    let confirm = move |_| {
        post_action_then(json!({"kind": "stop_all"}), stop_done);
        visible.set(false);
    };

    // The <Sheet> handles the dialog chrome, focus trap + restore, Escape,
    // backdrop dismiss and aria. We only supply the warning copy + actions.
    view! {
        <Sheet
            open=visible
            title="Stop all?".to_string()
            aria_label="Stop all running zones".to_string()
        >
            <p class="bottom-sheet-body">
                {move || {
                    let n = running_count.get();
                    if n == 1 {
                        "Stop the running zone?".to_string()
                    } else {
                        format!("Stop {n} running zones?")
                    }
                }}
            </p>
            <div class="bottom-sheet-actions">
                <Button variant="secondary" on_click=Callback::new(close)>"Cancel"</Button>
                <Button variant="danger" on_click=Callback::new(confirm)>"Stop all"</Button>
            </div>
        </Sheet>
    }
}
