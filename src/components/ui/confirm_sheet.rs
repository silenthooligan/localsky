// <ConfirmSheet/>: the shared two-step confirmation, on the
// StopAllConfirm idiom: always mounted, visibility owned by the parent
// (a single RwSignal<bool>), wrapping ui::Sheet (focus trap, Escape,
// backdrop dismiss) with `.bottom-sheet-body` copy and
// `.bottom-sheet-actions` Cancel/Confirm buttons.
//
// Used by the zone editor's run-limit raise, the tuning Apply's same
// gate, and the zone delete. Confirm renders variant="primary" by
// default: the run-limit flow is a confirm-to-allow, not a warning.
// Pass danger=true only for destructive confirms (delete).

use crate::components::ui::{Button, Sheet};
use leptos::prelude::*;

#[component]
pub fn ConfirmSheet(
    /// Parent-owned visibility. The sheet is always mounted; the parent
    /// opens it with visible.set(true); either button closes it (Escape
    /// and backdrop dismiss come from the Sheet).
    visible: RwSignal<bool>,
    /// Sheet title, e.g. "Raise run limit?".
    #[prop(into)]
    title: String,
    /// Body copy. Reactive so per-open values (a minute count, a zone
    /// name) render current.
    #[prop(into)]
    body: Signal<String>,
    /// Confirm button label; reactive for the same reason.
    #[prop(into)]
    confirm_label: Signal<String>,
    /// Danger styling for destructive confirms (delete). Default false =
    /// primary.
    #[prop(default = false)]
    danger: bool,
    /// Runs on confirm, after the sheet closes.
    on_confirm: Callback<()>,
) -> impl IntoView {
    let close = move |_| visible.set(false);
    let confirm = move |_| {
        visible.set(false);
        on_confirm.run(());
    };
    let variant = if danger { "danger" } else { "primary" };
    view! {
        <Sheet open=visible title=title.clone() aria_label=title>
            <p class="bottom-sheet-body">{move || body.get()}</p>
            <div class="bottom-sheet-actions">
                <Button variant="secondary" on_click=Callback::new(close)>"Cancel"</Button>
                <Button variant=variant on_click=Callback::new(confirm)>
                    {move || confirm_label.get()}
                </Button>
            </div>
        </Sheet>
    }
}
