use leptos::prelude::*;

struct SearchEntry {
    label: &'static str,
    section: &'static str,
    description: &'static str,
    keywords: &'static str,
}

const CONTROLS: &[SearchEntry] = &[
    SearchEntry { label: "Device notifications", section: "notifications", description: "Weather and watering alerts, quiet hours and optional daily outlook", keywords: "pwa push alerts notification midnight quiet schedule morning forecast summary per device rain wind heat freeze lightning" },
    SearchEntry { label: "History retention", section: "history", description: "How long to keep sensor readings and watering records", keywords: "storage cleanup delete pruning days logs database data" },
    SearchEntry { label: "Source freshness", section: "advanced#source-freshness", description: "Connection status and last reading for each source", keywords: "offline online active standby health weather" },
    SearchEntry { label: "Nerd mode & kiosk mode", section: "advanced", description: "Detailed readings and controls for shared screens", keywords: "simple detail read only readonly" },
    SearchEntry { label: "Backup & restore", section: "advanced#backup", description: "Download or restore configuration and history", keywords: "export import database recovery" },
    SearchEntry { label: "Rain, wind & freeze thresholds", section: "skip-rules", description: "Weather conditions that skip watering", keywords: "temperature hot heat cold rainfall precipitation" },
    SearchEntry { label: "Soil or weekly watering", section: "engine", description: "Scheduling model, seasonal adjustment, and cycle-and-soak", keywords: "budget soak cycles interleave seasonal deficit" },
    SearchEntry { label: "Sprinkler rate & soil", section: "zones", description: "Zone area, grass, root depth, and application rate", keywords: "calibration catch cup clay sand texture species water runtime" },
    SearchEntry { label: "Watering days & time windows", section: "restrictions", description: "Allowed days, hours, and blackout dates", keywords: "schedule ban drought weekday municipal" },
    SearchEntry { label: "Source priority", section: "devices", description: "Choose which weather source supplies each reading", keywords: "sensor fallback override weather station tempest nws open meteo controller rachio opensprinkler hydrawise b hyve mqtt http" },
    SearchEntry { label: "Theme & display mode", section: "theme", description: "Field Green, Slate, or Classic Blue; light, dark, or system preference", keywords: "theme appearance color aesthetic contrast auto palette green blue upgrade" },
    SearchEntry { label: "Timezone & coordinates", section: "location", description: "Your location, elevation, and local time", keywords: "latitude longitude address clock" },
    SearchEntry { label: "Temperature, rain & wind units", section: "units", description: "Metric or imperial measurements", keywords: "celsius fahrenheit inches millimeters mph kph" },
    SearchEntry { label: "Login & API tokens", section: "account", description: "Owner access and integration credentials", keywords: "password security authentication key" },
];

fn matches(query: &str, text: &str) -> bool {
    let text = text.to_lowercase();
    query
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}

fn results(query: &str) -> Vec<(&'static str, &'static str, &'static str)> {
    if query.trim().is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for entry in CONTROLS {
        if matches(
            query,
            &format!("{} {} {}", entry.label, entry.description, entry.keywords),
        ) {
            found.push((entry.label, entry.section, entry.description));
        }
    }
    for group in super::home::GROUPS {
        for entry in group.links {
            if !found.iter().any(|(_, section, _)| *section == entry.key)
                && matches(query, &format!("{} {}", entry.label, entry.helptext))
            {
                found.push((entry.label, entry.key, entry.helptext));
            }
        }
    }
    found
}

#[component]
pub fn SettingsSearch() -> impl IntoView {
    let query = RwSignal::new(String::new());
    let found = Memo::new(move |_| results(&query.get()));
    view! {
        <div class="settings-search" role="search" aria-label="Settings">
            <label for="settings-search">"Find a setting"</label>
            <div class="settings-search__field">
                <crate::components::ui::Icon name="search" size=18/>
                <input id="settings-search" type="search" placeholder="Search settings, e.g. retention or wind"
                    autocomplete="off" prop:value=move || query.get()
                    on:input=move |ev| query.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Escape" { query.set(String::new()); }/>
                <Show when=move || !query.get().is_empty()>
                    <button type="button" aria-label="Clear settings search" on:click=move |_| query.set(String::new())>
                        <crate::components::ui::Icon name="x" size=16/>
                    </button>
                </Show>
            </div>
            <Show when=move || !query.get().trim().is_empty()>
                <div class="settings-search__results">
                    <p role="status">{move || match found.get().len() {
                        0 => "No matching settings. Try a different word.".to_string(),
                        1 => "1 matching setting".to_string(),
                        n => format!("{n} matching settings"),
                    }}</p>
                    <ul>
                        {move || found.get().into_iter().map(|(label, section, description)| view! {
                            <li><a href=format!("/settings?section={section}") on:click=move |_| query.set(String::new())>
                                <strong>{label}</strong><span>{description}</span>
                            </a></li>
                        }).collect_view()}
                    </ul>
                </div>
            </Show>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_finds_controls_and_synonyms_case_insensitively() {
        assert_eq!(results("RETENTION")[0].1, "history");
        assert_eq!(results("offline")[0].1, "advanced#source-freshness");
        assert_eq!(results("wind threshold")[0].1, "skip-rules");
        assert!(results("MQTT")
            .iter()
            .any(|(_, section, _)| *section == "notifications"));
        assert!(results("  ").is_empty());
        assert!(results("not-a-setting").is_empty());
    }
}
