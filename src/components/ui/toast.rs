// Toast notifications. A `ToastHub` (Copy handle over two signals) is
// provided once at the shell level via context; any component calls
// `use_toast().success("Saved")` etc. The `<ToastViewport/>` is rendered
// once in the app shell and shows the live stack. Toasts auto-dismiss
// after a few seconds; each is also manually dismissable.
//
// The stack starts empty on SSR (no toasts exist server-side), so the
// SSR/hydrate first frame match, toasts only ever appear from
// client-side event handlers.

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
    /// ARIA live-region role for a single toast. Error/danger toasts are
    /// assertive (`role="alert"`) so a screen reader interrupts and announces
    /// a failed Stop / override immediately; info, success, and warn stay
    /// polite (`role="status"`) so routine confirmations queue without
    /// cutting off whatever the user is doing.
    fn role(self) -> &'static str {
        match self {
            ToastKind::Error => "alert",
            ToastKind::Info | ToastKind::Success | ToastKind::Warn => "status",
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
        // Auto-dismiss.
        let items = self.items;
        set_timeout(
            move || items.update(|v| v.retain(|t| t.id != id)),
            Duration::from_secs(5),
        );
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
    // No container-level aria-live: each toast carries its own role so that
    // error/danger toasts announce assertively (role="alert") while routine
    // toasts stay polite (role="status"). A container aria-live would flatten
    // every child to the same politeness and swallow the assertive alerts.
    view! {
        <div class="ui-toast-viewport">
            {move || {
                items.get().into_iter().map(|t| {
                    let id = t.id;
                    let role = t.kind.role();
                    view! {
                        <div class=format!("ui-toast {}", t.kind.class()) role=role>
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
                }).collect_view()
            }}
        </div>
    }
}
