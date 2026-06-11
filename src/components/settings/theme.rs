// SettingsTheme. Picks the active theme from a preset list and writes
// the choice to localStorage. The boot script in app.rs reads
// localStorage.theme synchronously on the next page load and applies
// the data-theme attribute before first paint.
//
// Lives client-side: this is a per-device preference, not a per-
// deployment config field, so no /api/config call is made.

use leptos::prelude::*;

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
        helptext: "House theme. Glass over deep blue.",
        swatch_bg: "#0b1220",
        swatch_accent: "#1490dc",
        swatch_text: "#e6ecf5",
    },
    ThemePreset {
        id: "light",
        label: "Light",
        helptext: "Hand-tuned. Same panels, lifted.",
        swatch_bg: "#f6f9ff",
        swatch_accent: "#1490dc",
        swatch_text: "#0b1220",
    },
    ThemePreset {
        id: "auto",
        label: "Auto",
        helptext: "Follow OS preference.",
        swatch_bg: "linear-gradient(135deg, #0b1220 50%, #f6f9ff 50%)",
        swatch_accent: "#1490dc",
        swatch_text: "#e6ecf5",
    },
    ThemePreset {
        id: "hc",
        label: "High contrast",
        helptext: "Pure black + pure white. No glass.",
        swatch_bg: "#000000",
        swatch_accent: "#5fb4ff",
        swatch_text: "#ffffff",
    },
];

#[component]
pub fn SettingsTheme() -> impl IntoView {
    let current = RwSignal::new("dark".to_string());

    // On mount, read localStorage.theme into the signal so the active
    // card highlights correctly. SSR has no localStorage so we skip.
    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    if let Ok(Some(t)) = storage.get_item("theme") {
                        current.set(t);
                    }
                }
            }
        });
    }

    let pick = move |id: String| {
        #[cfg(feature = "hydrate")]
        {
            if let Some(win) = web_sys::window() {
                if let Ok(Some(storage)) = win.local_storage() {
                    let _ = storage.set_item("theme", &id);
                }
                // Apply immediately by setting data-theme; no reload needed.
                if let Some(doc) = win.document() {
                    if let Some(html) = doc.document_element() {
                        if id == "dark" {
                            let _ = html.remove_attribute("data-theme");
                        } else {
                            let _ = html.set_attribute("data-theme", &id);
                        }
                    }
                }
            }
        }
        current.set(id);
    };

    view! {
        <main id="main-content" class="settings-page">
            <header class="settings-page__header">
                <a class="settings-page__back" href="/settings">"← Settings"</a>
                <h1 class="settings-page__title">"Theme"</h1>
                <p class="settings-page__subtitle">
                    "Applies to this browser only. Your choice persists across "
                    "page reloads via localStorage."
                </p>
            </header>

            <div class="theme-grid">
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
                            class:theme-card--active=move || current.get() == id
                            on:click=move |_| pick(id.to_string())
                        >
                            <div class="theme-card__swatch" style=format!("background: {bg}")>
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
        </main>
    }
}
