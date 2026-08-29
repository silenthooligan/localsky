// Toast notifications. A `ToastHub` (Copy handle over two signals) is
// provided once at the shell level via context; any component calls
// `use_toast().success("Saved")` etc. The `<ToastViewport/>` is rendered
// once in the app shell and shows the live stack. Routine toasts
// auto-dismiss after a few seconds; ERROR toasts stay until dismissed,
// because they carry the server's own reason and are the only copy of
// it. Every toast is manually dismissable.
//
// The stack starts empty on SSR (no toasts exist server-side), so the
// SSR/hydrate first frame match, toasts only ever appear from
// client-side event handlers.
//
// A11y: the viewport keeps two permanently-mounted live-region
// containers (polite role="status" + assertive role="alert"), rendered
// empty from SSR, and toast messages are inserted INTO them. ARIA live
// regions are only reliably announced when the region exists in the DOM
// before its content changes; a role="status" node inserted together
// with its text is skipped by several screen reader / browser pairs.

use std::time::Duration;

use leptos::prelude::*;

use crate::components::ui::Icon;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

impl ToastKind {
    fn icon(self) -> &'static str {
        match self {
            ToastKind::Info => "info",
            ToastKind::Success => "check",
            ToastKind::Warn => "bell",
            ToastKind::Error => "x",
        }
    }
    fn class(self) -> &'static str {
        match self {
            ToastKind::Info => "ui-toast--info",
            ToastKind::Success => "ui-toast--success",
            ToastKind::Warn => "ui-toast--warn",
            ToastKind::Error => "ui-toast--error",
        }
    }
}

#[derive(Clone)]
pub struct ToastItem {
    pub id: u64,
    pub kind: ToastKind,
    pub message: String,
}

/// Copy handle to the toast stack. Stored in context.
#[derive(Clone, Copy)]
pub struct ToastHub {
    items: RwSignal<Vec<ToastItem>>,
    next_id: RwSignal<u64>,
}

impl Default for ToastHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastHub {
    pub fn new() -> Self {
        Self {
            items: RwSignal::new(Vec::new()),
            next_id: RwSignal::new(1),
        }
    }

    pub fn push(&self, kind: ToastKind, message: impl Into<String>) {
        let id = self.next_id.get_untracked();
        self.next_id.set(id + 1);
        self.items.update(|v| {
            v.push(ToastItem {
                id,
                kind,
                message: message.into(),
            })
        });
        // Auto-dismiss ROUTINE toasts only. An error toast carries the
        // server's own reason, which on the controller action paths is a
        // multi-sentence hint naming the zone map's keys and the remedies
        // (the unknown-zone body alone runs past 250 characters). Five
        // seconds is well short of reading that, there is no toast
        // history to re-read it from, and re-triggering it costs another
        // call against the controller's daily budget. Errors stay until
        // dismissed; toast_view renders the close button on every kind.
        if kind != ToastKind::Error {
            let items = self.items;
            set_timeout(
                move || items.update(|v| v.retain(|t| t.id != id)),
                Duration::from_secs(5),
            );
        }
    }

    pub fn info(&self, m: impl Into<String>) {
        self.push(ToastKind::Info, m);
    }
    pub fn success(&self, m: impl Into<String>) {
        self.push(ToastKind::Success, m);
    }
    pub fn warn(&self, m: impl Into<String>) {
        self.push(ToastKind::Warn, m);
    }
    pub fn error(&self, m: impl Into<String>) {
        self.push(ToastKind::Error, m);
    }

    pub fn dismiss(&self, id: u64) {
        self.items.update(|v| v.retain(|t| t.id != id));
    }
}

/// Fetch the toast hub from context. Panics only if the shell forgot to
/// provide it (a programming error caught immediately in dev).
pub fn use_toast() -> ToastHub {
    use_context::<ToastHub>().expect("ToastHub not provided at shell level")
}

#[component]
pub fn ToastViewport() -> impl IntoView {
    let hub = use_toast();
    let items = hub.items;
    // Two permanently-mounted sibling live regions, both present (empty)
    // in the SSR HTML so they pre-exist any content change: routine
    // toasts (info/success/warn) go into the polite role="status"
    // region, error toasts into the assertive role="alert" region. This
    // preserves the per-kind politeness the old per-toast roles encoded
    // while making announcements actually fire (dynamically-inserted
    // role="status" nodes are skipped by several SR/browser combos).
    // aria-atomic="false" scopes re-announcement to the toast that was
    // added instead of re-reading the whole stack.
    view! {
        <div class="ui-toast-viewport">
            <div class="ui-toast-region" role="status" aria-live="polite" aria-atomic="false">
                {move || {
                    items
                        .get()
                        .into_iter()
                        .filter(|t| t.kind != ToastKind::Error)
                        .map(|t| toast_view(t, hub))
                        .collect_view()
                }}
            </div>
            <div class="ui-toast-region" role="alert" aria-atomic="false">
                {move || {
                    items
                        .get()
                        .into_iter()
                        .filter(|t| t.kind == ToastKind::Error)
                        .map(|t| toast_view(t, hub))
                        .collect_view()
                }}
            </div>
        </div>
    }
}

/// One toast row. Built fresh inside the viewport's reactive closures
/// (never pre-built and cloned in, per the view-clone rule).
fn toast_view(t: ToastItem, hub: ToastHub) -> impl IntoView {
    let id = t.id;
    view! {
        <div class=format!("ui-toast {}", t.kind.class())>
            <span class="ui-toast__icon"><Icon name=t.kind.icon() size=16/></span>
            <span class="ui-toast__msg">{t.message}</span>
            <button
                type="button"
                class="ui-toast__close"
                aria-label="Dismiss"
                on:click=move |_| hub.dismiss(id)
            >
                <Icon name="x" size=14/>
            </button>
        </div>
    }
}
