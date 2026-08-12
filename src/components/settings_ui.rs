// Settings UI kit. Polished, reusable building blocks for the
// configuration pages so each one (controllers, sources, zones,
// schedules...) shares a consistent visual language instead of every
// page rolling its own list + button salad. Three pieces:
//
//   SettingsCard  , expandable item with header (name, badges) +
//                    click-to-expand details + action bar.
//                    Replaces the old "settings-list__item--row" with
//                    a real card surface that lets the user browse
//                    config without entering edit mode.
//   SettingsBadge , semantic status pill (default, enabled, disabled,
//                    warning, danger). Color + text in one component.
//   SettingsKv    , key-value display row for inside the expanded
//                    details, monospace value, dimmed label.
//   SettingsResult, the save-status message line shared verbatim by
//                    every settings page (ok/err styling + role=status).

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::ui::Button;

/// Status hero for an integration/settings page: icon + title + a status chip +
/// a plain-English meaning. Promotes the Home Assistant page's hero pattern into
/// a shared component so every integration page (LLM, Notifications, Radar, ...)
/// reads as one family. `ok` drives the chip color + the highlighted border;
/// `chip` is the short status word; `meaning` is the one-line explanation.
/// Also keeps each page's monomorphized view tree flat (it's one component, not
/// inline nesting that overflows rustc's type-depth budget).
#[component]
pub fn StatusHero(
    icon: &'static str,
    title: &'static str,
    #[prop(into)] ok: Signal<bool>,
    #[prop(into)] chip: Signal<String>,
    #[prop(into)] meaning: Signal<String>,
) -> impl IntoView {
    view! {
        <div class="ha-hero" class:ha-hero--ok=move || ok.get()>
            <span class="ha-hero__icon">
                <crate::components::ui::Icon name=icon size=24/>
            </span>
            <div class="ha-hero__text">
                <div class="ha-hero__row">
                    <strong>{title}</strong>
                    <span class=move || {
                        if ok.get() { "ha-chip ha-chip--on" } else { "ha-chip ha-chip--off" }
                    }>
                        <span class="ha-chip__dot" aria-hidden="true"></span>
                        {move || chip.get()}
                    </span>
                </div>
                <p>{move || meaning.get()}</p>
            </div>
        </div>
    }
}

/// The four first-class entities in LocalSky's mental model. Drives the
/// left-stripe color + the uppercase identity badge on every entity card, so
/// "is this a source, a sensor, a controller, or a zone?" is answerable at a
/// glance. SOURCE provides -> SENSOR (a reading) binds to -> ZONE <- fired by
/// CONTROLLER.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EntityKind {
    Source,
    Sensor,
    Controller,
    Zone,
}

impl EntityKind {
    /// CSS slug for the entity-stripe-- / entity-badge-- modifier classes.
    pub fn slug(self) -> &'static str {
        match self {
            EntityKind::Source => "source",
            EntityKind::Sensor => "sensor",
            EntityKind::Controller => "controller",
            EntityKind::Zone => "zone",
        }
    }
    /// Badge label.
    pub fn label(self) -> &'static str {
        match self {
            EntityKind::Source => "Source",
            EntityKind::Sensor => "Sensor",
            EntityKind::Controller => "Controller",
            EntityKind::Zone => "Zone",
        }
    }
}

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
    /// Entity identity. When set, the card gets a left color-stripe + an
    /// uppercase identity badge so its category is instantly legible.
    #[prop(default = None)]
    entity: Option<EntityKind>,
    /// True when the card opens an editor. Adds the shared `.is-editable`
    /// affordance (a persistent pencil cue in the top-right per the affordance
    /// grammar), so a native, editable device is distinguishable at rest without
    /// having to expand it to discover the Edit button.
    #[prop(default = false)]
    editable: bool,
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
    // Entity stripe class is fixed for the card's lifetime; fold it into the
    // (reactive) expanded class so the left color-stripe renders.
    let stripe = entity
        .map(|e| format!(" entity-stripe entity-stripe--{}", e.slug()))
        .unwrap_or_default();
    // The editable pencil cue is fixed for the card's lifetime; fold it into the
    // (reactive) class alongside the stripe.
    let editable_cls = if editable { " is-editable" } else { "" };
    let card_class = move || {
        let base = if expanded.get() {
            "settings-card is-expanded"
        } else {
            "settings-card"
        };
        format!("{base}{stripe}{editable_cls}")
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
                    <span class="settings-card__badges">
                        {entity.map(|e| view! {
                            <span class=format!("entity-badge entity-badge--{}", e.slug())>
                                {e.label()}
                            </span>
                        })}
                        {badges()}
                    </span>
                    {show_subtitle.then(|| view! {
                        <span class="settings-card__subtitle">{subtitle}</span>
                    })}
                </span>
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

/// The structured reading of a server JSON error body, shared by
/// `save_error_message` and `load_error_message` so the save toasts and the
/// load banners surface the same fields and can never drift.
enum ParsedErrorBody {
    /// A config_invalid-style body whose substance is the joined
    /// validation.errors[].detail list (first three rules, then a count).
    /// Callers add their own framing ("Not saved: " for a refused save,
    /// nothing for a failed load).
    ValidationRules(String),
    /// error code + hint/detail (or the bare code), already formatted with
    /// the status.
    Coded(String),
}

/// Parse a server JSON error body into its most useful human text. The
/// server's error bodies carry structured fields the raw text hides: `hint`
/// (the privileged-gate 401s name the auth.trusted_proxies /
/// auth.proxy_auth_header keys that fix a proxy-shaped refusal), `detail`
/// (config store errors), and `validation.errors[].detail` (the rules that
/// refused a config write). Returns `None` when the body is not the server's
/// error shape (not JSON, or no non-empty `error` code); callers then fall
/// back to a raw status + body line.
fn parse_error_body(status: u16, body: &str) -> Option<ParsedErrorBody> {
    let v = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let err = v
        .get("error")
        .and_then(|e| e.as_str())
        .unwrap_or_default()
        .to_string();
    if err.is_empty() {
        return None;
    }
    // A config_invalid 422 carries its substance in
    // validation.errors[].detail, not in error/hint/detail. Rendering
    // only the code left the operator with "config_invalid (HTTP 422)"
    // and no clue WHICH rule refused the save (issue #6: an unset
    // location blocked enabling a weather source, and the toast never
    // said the word location). List the rule details; every settings
    // page saves through this one function, and any pre-existing
    // config error blocks any page's whole-config save, so the
    // message must name rules from anywhere in the config.
    if let Some(errors) = v
        .pointer("/validation/errors")
        .and_then(|e| e.as_array())
        .filter(|a| !a.is_empty())
    {
        let details: Vec<&str> = errors
            .iter()
            .filter_map(|e| e.get("detail").and_then(|d| d.as_str()))
            .collect();
        const SHOWN: usize = 3;
        let mut msg = details
            .iter()
            .take(SHOWN)
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        if details.len() > SHOWN {
            msg.push_str(&format!(" | and {} more", details.len() - SHOWN));
        }
        if !msg.is_empty() {
            return Some(ParsedErrorBody::ValidationRules(msg));
        }
    }
    if let Some(hint) = v.get("hint").and_then(|h| h.as_str()) {
        return Some(ParsedErrorBody::Coded(format!(
            "{err} (HTTP {status}). {hint}"
        )));
    }
    if let Some(detail) = v.get("detail").and_then(|d| d.as_str()) {
        return Some(ParsedErrorBody::Coded(format!(
            "{err} (HTTP {status}): {detail}"
        )));
    }
    Some(ParsedErrorBody::Coded(format!("{err} (HTTP {status})")))
}

/// Human-readable message for a failed settings save. Shares the body parse
/// with `load_error_message` (see `parse_error_body`); anything unparseable
/// falls back to the old "HTTP <status>: <body>" shape.
pub fn save_error_message(status: u16, body: &str) -> String {
    match parse_error_body(status, body) {
        Some(ParsedErrorBody::ValidationRules(rules)) => format!("Not saved: {rules}"),
        Some(ParsedErrorBody::Coded(msg)) => msg,
        None => format!("HTTP {status}: {body}"),
    }
}

/// Human-readable message for a failed settings/setup LOAD (a config or
/// inventory GET behind a page). The load paths used to format the bare
/// status ("HTTP 422") and drop the response body, so the server's
/// error/detail/hint JSON never reached the user (issue #7). Same parse as
/// `save_error_message`, minus the "Not saved" framing (a failed load is not
/// a refused save); an unparseable body keeps the raw status + body, and an
/// empty body reads as the bare status alone.
pub fn load_error_message(status: u16, body: &str) -> String {
    match parse_error_body(status, body) {
        Some(ParsedErrorBody::ValidationRules(rules)) => rules,
        Some(ParsedErrorBody::Coded(msg)) => msg,
        None if body.trim().is_empty() => format!("HTTP {status}"),
        None => format!("HTTP {status}: {body}"),
    }
}

/// Error banner + Retry for a failed initial config GET, shared by the
/// settings editor pages (zones, sources, controllers). Rendered INSTEAD
/// of the editor body: an empty editor after a failed load reads as data
/// loss, and "Save all changes" from it would PUT a hollow config over
/// the real one.
#[component]
pub fn SettingsLoadError(
    /// Some(message) when the initial config GET failed.
    error: RwSignal<Option<String>>,
    /// Bump to re-run the page's load effect.
    retry: RwSignal<u32>,
) -> impl IntoView {
    view! {
        <crate::components::ui::Panel title="".to_string()>
            <p class="setup-result setup-result--err" role="alert">
                {move || format!(
                    "Couldn't load the current configuration: {}",
                    error.get().unwrap_or_default()
                )}
            </p>
            <p class="sensors-section__hint">
                "Your settings are intact on the server; this page just couldn't "
                "fetch them. Editing is disabled until the load succeeds so a "
                "save can't overwrite the real configuration with an empty form."
            </p>
            <Button
                variant="primary"
                on_click=Callback::new(move |_| retry.update(|n| *n += 1))
            >"Retry"</Button>
        </crate::components::ui::Panel>
    }
}

#[cfg(test)]
mod tests {
    use super::{load_error_message, save_error_message};

    #[test]
    fn save_error_surfaces_server_error_and_hint() {
        // The privileged-gate 401 shape: error + hint. The hint (which names
        // auth.trusted_proxies / auth.proxy_auth_header) must reach the user.
        let body = r#"{"error":"unauthorized","hint":"Set auth.trusted_proxies to your proxy's address."}"#;
        let msg = save_error_message(401, body);
        assert!(msg.contains("unauthorized"));
        assert!(msg.contains("401"));
        assert!(msg.contains("Set auth.trusted_proxies"));

        // config store errors carry `detail` instead.
        let msg = save_error_message(
            422,
            r#"{"error":"config_store_error","detail":"validation: zone slug"}"#,
        );
        assert!(msg.contains("config_store_error") && msg.contains("zone slug"));

        // Non-JSON bodies keep the legacy fallback shape.
        assert_eq!(
            save_error_message(500, "boom"),
            "HTTP 500: boom".to_string()
        );
    }

    #[test]
    fn save_error_names_the_validation_rules_that_refused_the_save() {
        // The issue #6 shape: a config_invalid 422 whose substance lives in
        // validation.errors[].detail. Rendering only the code left the
        // operator with "config_invalid (HTTP 422)" while the real reason
        // (an unset location) was never shown.
        let body = r#"{"error":"config_invalid","detail":"location is 0,0 (null island); set your real coordinates","validation":{"errors":[{"severity":"error","code":"location_unset","detail":"location is 0,0 (null island); set your real coordinates"}],"warnings":[]}}"#;
        let msg = save_error_message(422, body);
        assert!(msg.starts_with("Not saved:"), "got: {msg}");
        assert!(msg.contains("location is 0,0"), "got: {msg}");
        assert!(!msg.contains("config_invalid"), "raw code hidden: {msg}");

        // Many errors: the first three are listed, the rest are counted.
        let body = r#"{"error":"config_invalid","validation":{"errors":[
            {"severity":"error","code":"a","detail":"first"},
            {"severity":"error","code":"b","detail":"second"},
            {"severity":"error","code":"c","detail":"third"},
            {"severity":"error","code":"d","detail":"fourth"},
            {"severity":"error","code":"e","detail":"fifth"}
        ],"warnings":[]}}"#;
        let msg = save_error_message(422, body);
        assert!(msg.contains("first") && msg.contains("third"), "got: {msg}");
        assert!(!msg.contains("fourth"), "capped: {msg}");
        assert!(msg.contains("and 2 more"), "got: {msg}");

        // An empty errors array (should not happen on a 422, but a malformed
        // body must not panic) falls back to the code shape.
        let body = r#"{"error":"config_invalid","validation":{"errors":[],"warnings":[]}}"#;
        let msg = save_error_message(422, body);
        assert!(msg.contains("config_invalid"), "got: {msg}");
    }

    #[test]
    fn load_error_surfaces_server_error_hint_and_detail() {
        // The load-banner twin of the save test (issue #7): a failed config
        // GET carries the same JSON error shapes, and the banner used to
        // show only "HTTP <status>" with the body dropped.
        let body = r#"{"error":"unauthorized","hint":"Set auth.trusted_proxies to your proxy's address."}"#;
        let msg = load_error_message(401, body);
        assert!(msg.contains("unauthorized"));
        assert!(msg.contains("401"));
        assert!(msg.contains("Set auth.trusted_proxies"));

        let msg = load_error_message(
            500,
            r#"{"error":"config_store_error","detail":"read: permission denied"}"#,
        );
        assert!(msg.contains("config_store_error") && msg.contains("permission denied"));

        // Non-JSON bodies keep the raw fallback; an empty body reads as the
        // bare status (nothing useful to append).
        assert_eq!(
            load_error_message(502, "bad gateway"),
            "HTTP 502: bad gateway".to_string()
        );
        assert_eq!(load_error_message(422, ""), "HTTP 422".to_string());
    }

    #[test]
    fn load_error_names_validation_rules_without_the_save_framing() {
        // A validation-shaped body on a load names the rules, but a failed
        // load is not a refused save, so the "Not saved" framing stays off.
        let body = r#"{"error":"config_invalid","validation":{"errors":[{"severity":"error","code":"location_unset","detail":"location is 0,0 (null island); set your real coordinates"}],"warnings":[]}}"#;
        let msg = load_error_message(422, body);
        assert!(msg.contains("location is 0,0"), "got: {msg}");
        assert!(!msg.starts_with("Not saved:"), "got: {msg}");
        // The save shape keeps its framing off the SAME parse, so the two
        // surfaces cannot drift.
        assert!(save_error_message(422, body).starts_with("Not saved:"));
    }
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
