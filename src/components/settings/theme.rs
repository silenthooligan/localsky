// SettingsTheme. Picks the active theme from a preset list and writes
// the choice to localStorage. The boot script in app.rs reads
// localStorage.theme synchronously on the next page load and applies
// the data-theme attribute before first paint.
//
// Lives client-side: this is a per-device preference, not a per-
// deployment config field, so no /api/config call is made.

use leptos::prelude::*;

use crate::components::ui::HelpHint;

/// One browser preference shared by the header and Settings. A header choice
/// must update an already-open Settings card without a reload (and vice versa).
#[derive(Clone, Copy)]
pub struct ThemePreference(pub RwSignal<String>);

#[derive(Clone, Copy)]
pub struct StylePreference(pub RwSignal<String>);

#[derive(Clone, Copy)]
pub struct ThemeUpgrade(pub RwSignal<bool>);

pub fn provide_theme() {
    let current = RwSignal::new("dark".to_string());
    provide_context(ThemePreference(current));
    let style = RwSignal::new("field".to_string());
    provide_context(StylePreference(style));
    provide_context(ThemeUpgrade(RwSignal::new(false)));
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        let theme = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.document_element())
            .and_then(|h| h.get_attribute("data-theme"))
            .unwrap_or_else(|| "dark".to_string());
        current.set(theme);
        style.set(
            web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.document_element())
                .and_then(|h| h.get_attribute("data-style"))
                .unwrap_or_else(|| "field".to_string()),
        );
    });
}

impl StylePreference {
    pub fn pick(self, id: &str) {
        if !matches!(id, "field" | "slate" | "classic") {
            return;
        }
        #[cfg(feature = "hydrate")]
        if let Some(win) = web_sys::window() {
            if let Ok(Some(storage)) = win.local_storage() {
                let _ = storage.set_item("style", id);
            }
            if let Some(html) = win.document().and_then(|d| d.document_element()) {
                let _ = html.set_attribute("data-style", id);
            }
        }
        self.0.set(id.to_string());
    }
}

impl ThemeUpgrade {
    /// A configured installation without an explicit style predates the theme
    /// picker. Keep its blue palette until the owner chooses. New installs get
    /// Field Green; explicit choices (including the local 0.9.4 preview) always win.
    #[cfg(feature = "hydrate")]
    pub fn resolve(self, style: StylePreference, configured: bool, demo: bool) {
        let storage = web_sys::window().and_then(|w| w.local_storage().ok().flatten());
        let saved = storage
            .as_ref()
            .and_then(|s| s.get_item("style").ok().flatten());
        if matches!(saved.as_deref(), Some("field" | "slate" | "classic")) {
            return;
        }
        if !configured {
            style.pick("field");
        } else if demo {
            // A demo isn't an upgrade. Do not persist an unsolicited choice.
            if let Some(html) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.document_element())
            {
                let _ = html.set_attribute("data-style", "field");
            }
            style.0.set("field".into());
        } else {
            if let Some(html) = web_sys::window()
                .and_then(|w| w.document())
                .and_then(|d| d.document_element())
            {
                let _ = html.set_attribute("data-style", "classic");
            }
            style.0.set("classic".into());
            self.0.set(true);
        }
    }
}

/// Inline, non-blocking upgrade choice. A selection is explicit; merely opening
/// the app never opts an existing installation into the new appearance.
#[component]
pub fn ThemeUpgradeChoice() -> impl IntoView {
    let upgrade = expect_context::<ThemeUpgrade>();
    let style = expect_context::<StylePreference>();
    let visible = RwSignal::new(true);
    view! {
        <Show when=move || upgrade.0.get() && visible.get()>
            <section class="theme-upgrade" aria-labelledby="theme-upgrade-title">
                <div>
                    <h2 id="theme-upgrade-title">"Make yourself at home"</h2>
                    <p>"Keep the familiar blue theme or try Field Green, the new default. Your light or dark mode stays the same."</p>
                </div>
                <div class="theme-upgrade__choices">
                    {[("classic", "Keep Classic Blue", "Familiar navy and blue."),
                      ("field", "Use Field Green", "Paper, forest green, and serif headings.")].into_iter().map(|(id, label, detail)| view! {
                        <button type="button" class="theme-upgrade__option" on:click=move |_| {
                            style.pick(id);
                            upgrade.0.set(false);
                        }>
                            <span class="theme-upgrade__swatch" data-preview=id aria-hidden="true"></span>
                            <strong>{label}</strong><small>{detail}</small>
                        </button>
                    }).collect_view()}
                </div>
                <div class="theme-upgrade__footer">
                    <span>"Change this anytime in Settings → Theme."</span>
                    <button type="button" class="btn btn--ghost" on:click=move |_| visible.set(false)>"Decide later"</button>
                </div>
            </section>
        </Show>
    }
}

pub fn use_theme() -> ThemePreference {
    use_context::<ThemePreference>()
        .unwrap_or_else(|| ThemePreference(RwSignal::new("dark".to_string())))
}

impl ThemePreference {
    pub fn pick(self, id: String) {
        if !matches!(id.as_str(), "dark" | "light" | "auto" | "hc") {
            return;
        }
        #[cfg(feature = "hydrate")]
        if let Some(win) = web_sys::window() {
            if let Ok(Some(storage)) = win.local_storage() {
                let _ = storage.set_item("theme", &id);
            }
            if let Some(html) = win.document().and_then(|d| d.document_element()) {
                if id == "dark" {
                    let _ = html.remove_attribute("data-theme");
                } else {
                    let _ = html.set_attribute("data-theme", &id);
                }
            }
        }
        self.0.set(id);
    }
}

struct ThemePreset {
    id: &'static str,
    label: &'static str,
    helptext: &'static str,
    swatch_bg: &'static str,
    swatch_accent: &'static str,
    swatch_text: &'static str,
}

const PRESETS: &[ThemePreset] = &[
    ThemePreset {
        id: "dark",
        label: "Dark",
        helptext: "Dark surfaces, bright text.",
        swatch_bg: "#091a17",
        swatch_accent: "#126089",
        swatch_text: "#e4e9df",
    },
    ThemePreset {
        id: "light",
        label: "Light",
        helptext: "Light surfaces, dark text.",
        swatch_bg: "#eeefe5",
        swatch_accent: "#126089",
        swatch_text: "#091a17",
    },
    ThemePreset {
        id: "auto",
        label: "Auto",
        helptext: "Follow OS preference.",
        swatch_bg: "linear-gradient(135deg, #091a17 50%, #eeefe5 50%)",
        swatch_accent: "#126089",
        swatch_text: "#e4e9df",
    },
    ThemePreset {
        id: "hc",
        label: "High contrast",
        helptext: "Black and white with strong borders.",
        swatch_bg: "#000000",
        swatch_accent: "#5fb4ff",
        swatch_text: "#ffffff",
    },
];

/// Compact appearance choices for setup. Uses the same browser preferences as
/// Settings and the app header, including the pre-paint bootstrap on reload.
#[component]
pub fn AppearancePicker() -> impl IntoView {
    let theme = use_theme();
    let upgrade = use_context::<ThemeUpgrade>();
    let style = use_context::<StylePreference>()
        .unwrap_or_else(|| StylePreference(RwSignal::new("field".to_string())));
    view! {
        <div class="appearance-picker" role="group" aria-label="Appearance">
            <label>
                <span>"Theme"</span>
                <select class="ui-input" prop:value=move || style.0.get()
                    on:change=move |ev| {
                        style.pick(&event_target_value(&ev));
                        if let Some(upgrade) = upgrade { upgrade.0.set(false); }
                    }>
                    <option value="field">"Field Green"</option>
                    <option value="slate">"Slate"</option>
                    <option value="classic">"Classic Blue"</option>
                </select>
            </label>
            <label>
                <span>"Display mode"</span>
                <select class="ui-input" prop:value=move || theme.0.get()
                    on:change=move |ev| theme.pick(event_target_value(&ev))>
                    {PRESETS.iter().map(|p| view! {
                        <option value=p.id>{p.label}</option>
                    }).collect_view()}
                </select>
            </label>
        </div>
    }
}

#[component]
pub fn SettingsTheme() -> impl IntoView {
    let theme = use_theme();
    let upgrade = use_context::<ThemeUpgrade>();
    let current = theme.0;
    let style = use_context::<StylePreference>()
        .unwrap_or_else(|| StylePreference(RwSignal::new("field".to_string())));

    view! {
        <div class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Theme"<HelpHint topic="theme"/></h1>
                <p class="settings-page__subtitle">
                    "Choose a theme and display mode. Saved on this browser."
                </p>
            </header>

            <section aria-label="Theme">
            <h2 class="theme-section-title">"Theme"</h2>
            <div class="theme-grid theme-styles">
                {[("field", "Field Green", "The new default. Paper, forest green, and serif headings."),
                  ("slate", "Slate", "Blue and teal, cool surfaces, and sans-serif headings."),
                  ("classic", "Classic Blue", "The familiar navy and blue palette from earlier releases.")]
                    .into_iter().map(|(id, label, description)| view! {
                        <button type="button" class="theme-card" class:theme-card--active=move || style.0.get() == id
                            aria-pressed=move || style.0.get() == id on:click=move |_| {
                                style.pick(id);
                                if let Some(upgrade) = upgrade { upgrade.0.set(false); }
                            }>
                            <div class="theme-style-preview" data-preview=id aria-hidden="true">
                                <span>"LocalSky"</span><strong>"A good day to grow."</strong>
                                <div><i></i><i></i><i></i></div>
                            </div>
                            <div class="theme-card__label">{label}</div>
                            <div class="theme-card__helptext">{description}</div>
                        </button>
                    }).collect_view()}
            </div>
            </section>
            <section aria-label="Display mode">
            <h2 class="theme-section-title">"Display mode"</h2>
            <div class="theme-grid theme-color-modes">
                {PRESETS.iter().map(|p| {
                    let id = p.id;
                    let label = p.label;
                    let helptext = p.helptext;
                    let bg = p.swatch_bg;
                    let accent = p.swatch_accent;
                    let text = p.swatch_text;
                    view! {
                        <button
                            type="button"
                            class="theme-card"
                            aria-pressed=move || if current.get() == id { "true" } else { "false" }
                            class:theme-card--active=move || current.get() == id
                            on:click=move |_| theme.pick(id.to_string())
                        >
                            <div class="theme-card__swatch" aria-hidden="true" style=move || {
                                let bg = if style.0.get() == "slate" {
                                    match id {
                                        "dark" => "#0b1220",
                                        "light" => "#edf2f8",
                                        "auto" => "linear-gradient(135deg, #0b1220 50%, #edf2f8 50%)",
                                        _ => bg,
                                    }
                                } else if style.0.get() == "classic" {
                                    match id {
                                        "dark" => "#11161f",
                                        "light" => "#f6f9ff",
                                        "auto" => "linear-gradient(135deg, #11161f 50%, #f6f9ff 50%)",
                                        _ => bg,
                                    }
                                } else { bg };
                                format!("background: {bg}")
                            }>
                                <span
                                    class="theme-card__swatch-accent"
                                    style=format!("background: {accent}")
                                ></span>
                                <span
                                    class="theme-card__swatch-text"
                                    style=format!("color: {text}")
                                >"Aa"</span>
                            </div>
                            <div class="theme-card__label">{label}</div>
                            <div class="theme-card__helptext">{helptext}</div>
                        </button>
                    }
                }).collect_view()}
            </div>
            </section>
        </div>
    }
}
