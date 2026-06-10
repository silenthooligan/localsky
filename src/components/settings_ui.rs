// Settings UI kit. Polished, reusable building blocks for the
// configuration pages so each one (controllers, sources, zones,
// schedules...) shares a consistent visual language instead of every
// page rolling its own list + button salad. Three pieces:
//
//   SettingsCard   — expandable item with header (name, badges) +
//                    click-to-expand details + action bar.
//                    Replaces the old "settings-list__item--row" with
//                    a real card surface that lets the user browse
//                    config without entering edit mode.
//   SettingsBadge  — semantic status pill (default, enabled, disabled,
//                    warning, danger). Color + text in one component.
//   SettingsKv     — key-value display row for inside the expanded
//                    details, monospace value, dimmed label.
//   SettingsResult — the save-status message line shared verbatim by
//                    every settings page (ok/err styling + role=status).

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

/// Semantic color tone for SettingsBadge.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BadgeTone {
    /// Brand accent. "Default" controller, "Used by skip-check", etc.
    Accent,
    /// Healthy / on. "Enabled", "Online", "Connected".
    Good,
    /// Off / disabled. Muted color, no alarm.
    Muted,
    /// Soft warning. "Stale", "Sheltered", "Degraded".
    Warm,
    /// Hard danger. "Init failed", "Auth failed", "Offline".
    Danger,
}

impl BadgeTone {
    fn class(self) -> &'static str {
        match self {
            BadgeTone::Accent => "settings-badge settings-badge--accent",
            BadgeTone::Good => "settings-badge settings-badge--good",
            BadgeTone::Muted => "settings-badge settings-badge--muted",
            BadgeTone::Warm => "settings-badge settings-badge--warm",
            BadgeTone::Danger => "settings-badge settings-badge--danger",
        }
    }
}

#[component]
pub fn SettingsBadge(
    /// The label text shown inside the pill.
    label: String,
    /// Color tone. Defaults to Muted.
    #[prop(default = BadgeTone::Muted)]
    tone: BadgeTone,
) -> impl IntoView {
    view! {
        <span class=tone.class()>{label}</span>
    }
}

/// A read-only key-value row. Used inside a SettingsCard's expanded
/// details to show the controller's host:port, the source's API key
/// origin, the zone's species + soil, etc.
#[component]
pub fn SettingsKv(
    /// Label column (small caps, dim).
    label: &'static str,
    /// Value column (mono, default text color).
    value: String,
) -> impl IntoView {
    view! {
        <div class="settings-kv">
            <dt class="settings-kv__label">{label}</dt>
            <dd class="settings-kv__value">{value}</dd>
        </div>
    }
}

/// Render the top-level keys of a config JSON sub-tree as a stack of
/// SettingsKv views. Nested objects/arrays collapse to a short
/// placeholder so the card stays scannable; secret-looking values are
/// masked (so the read-only browse view never leaks a token). The
/// Edit button on the parent card opens the raw JSON textarea for
/// full control. Used by the Controllers and Sources settings pages.
pub fn config_kvs(config: &serde_json::Value) -> impl IntoView {
    use serde_json::Value;
    let rows: Vec<(String, String)> = match config {
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| {
                let value = if is_secret_key(k) {
                    if value_is_empty(v) {
                        "(not set)".to_string()
                    } else {
                        "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}".to_string()
                    }
                } else {
                    render_value(v)
                };
                (k.clone(), value)
            })
            .collect(),
        _ => Vec::new(),
    };
    rows.into_iter()
        .map(|(k, v)| {
            // Settings-kv expects a &'static label. Leak the key; the
            // total distinct config field names across all controller
            // and source kinds is bounded (under ~80) so the leak is
            // effectively a one-time-per-key intern at first paint.
            let label: &'static str = Box::leak(k.into_boxed_str());
            view! { <SettingsKv label=label value=v/> }.into_any()
        })
        .collect::<Vec<_>>()
}

fn is_secret_key(key: &str) -> bool {
    let k = key.to_lowercase();
    k == "password"
        || k == "password_md5"
        || k == "api_token"
        || k == "api_key"
        || k == "bearer_token"
        || k.contains("secret")
        || k.contains("token")
}

fn value_is_empty(v: &serde_json::Value) -> bool {
    matches!(v, serde_json::Value::Null)
        || matches!(v, serde_json::Value::String(s) if s.is_empty())
}

fn render_value(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "(none)".to_string(),
        Value::Bool(b) => if *b { "yes" } else { "no" }.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) if s.is_empty() => "(empty)".to_string(),
        Value::String(s) => s.clone(),
        Value::Array(arr) if arr.is_empty() => "(empty)".to_string(),
        Value::Array(arr) => format!("[{} items]", arr.len()),
        Value::Object(map) if map.is_empty() => "(empty)".to_string(),
        Value::Object(map) => format!("{{{} keys}}", map.len()),
    }
}

/// Expandable settings card. The header (icon + title + subtitle +
/// badges + chevron) is the expand affordance; clicking it toggles
/// the body. The body holds a `<dl>` of KV pairs (details) followed
/// by an action row. Callers always pass both children closures;
/// pass an empty `view!{}` if a section is not needed.
#[component]
pub fn SettingsCard(
    /// Icon registry name (ui::Icon). Empty string hides the icon slot.
    icon: String,
    /// Primary title (controller id, source id, zone name).
    title: String,
    /// Subtitle line below the title (controller kind, source kind,
    /// zone species). Empty string hides the subtitle row.
    #[prop(default = String::new())]
    subtitle: String,
    /// Badges to the right of the title. Pass `move || view!{}` if
    /// none.
    badges: Children,
    /// Detail rows rendered inside the expanded body. Typically a
    /// stack of SettingsKv.
    details: Children,
    /// Right-aligned action button row at the bottom of the body.
    /// Pass `move || view!{}` if no actions.
    actions: Children,
) -> impl IntoView {
    let expanded = RwSignal::new(false);
    let toggle = move |_| expanded.update(|v| *v = !*v);
    let card_class = move || {
        if expanded.get() {
            "settings-card is-expanded"
        } else {
            "settings-card"
        }
    };
    let chevron_class = move || {
        if expanded.get() {
            "settings-card__chevron is-open"
        } else {
            "settings-card__chevron"
        }
    };
    let show_subtitle = !subtitle.is_empty();

    // Body is always rendered; visibility is gated by the .is-expanded
    // class on the card root via CSS. Doing it in CSS rather than via
    // <Show> avoids consuming the FnOnce children twice when toggling,
    // and keeps the SSR-rendered HTML deterministic regardless of the
    // expanded state.
    view! {
        <article class=card_class>
            <button
                type="button"
                class="settings-card__header"
                aria-expanded=move || if expanded.get() { "true" } else { "false" }
                on:click=toggle
            >
                <span class="settings-card__icon" aria-hidden="true">
                    {(!icon.is_empty()).then(|| view! {
                        <crate::components::ui::Icon name=icon.clone() size=20/>
                    })}
                </span>
                <span class="settings-card__head-text">
                    <span class="settings-card__title">{title}</span>
                    {show_subtitle.then(|| view! {
                        <span class="settings-card__subtitle">{subtitle}</span>
                    })}
                </span>
                <span class="settings-card__badges">{badges()}</span>
                <span class=chevron_class aria-hidden="true">"\u{203A}"</span>
            </button>
            <div class="settings-card__body">
                <dl class="settings-card__kvs">{details()}</dl>
                <div class="settings-card__actions">{actions()}</div>
            </div>
        </article>
    }
}

/// Save-status line. Every settings page rendered this exact block
/// inline (a `Show` gating an ok/err-styled `<p role="status">`), so
/// it lived as copy-pasted markup in nine files. Extracted here so the
/// status styling is defined once and the page components stay a thin
/// shell. Hidden until `result_msg` is non-empty.
/// Route a completed save to the right surface: success goes to an
/// ephemeral toast (no layout shift, auto-dismiss), and any stale inline
/// error is cleared. Errors must stay inline next to the form (persistent
/// until fixed), so callers keep setting result_msg/result_ok themselves
/// on the Err path. Staging hints ("Click Save to apply") also stay
/// inline; this is only for server-acknowledged saves.
pub fn toast_saved(result_msg: RwSignal<String>, result_ok: RwSignal<bool>, msg: &str) {
    result_ok.set(true);
    result_msg.set(String::new());
    crate::components::ui::use_toast().success(msg.to_string());
}

#[component]
pub fn SettingsResult(
    /// Status text. Empty string keeps the line hidden.
    result_msg: RwSignal<String>,
    /// true → success styling, false → error styling.
    result_ok: RwSignal<bool>,
) -> impl IntoView {
    view! {
        <Show when=move || !result_msg.get().is_empty()>
            <p
                class="setup-result"
                class:setup-result--ok=move || result_ok.get()
                class:setup-result--err=move || !result_ok.get()
                role="status"
            >
                {move || result_msg.get()}
            </p>
        </Show>
    }
}
