// HelpHint. A tiny ? glyph that reveals a 1-2 sentence explanation of
// the dashboard panel it sits next to. Implemented as a native
// <details>/<summary> disclosure widget so it works without JavaScript,
// announces correctly to screen readers, and toggles consistently on
// touch + mouse.
//
// Topics are looked up from a static table; the topic string is part of
// the component API so misspellings show up as a "(no help topic
// configured)" placeholder rather than a silent miss.

use crate::docs::doc_url;
use leptos::prelude::*;

#[component]
pub fn HelpHint(
    /// Topic key from the static lookup table in help_topic. Also used
    /// as the slug for the "Read full doc" link, which `doc_url` resolves
    /// to an app-local, ingress-aware, same-origin /docs/<topic> path
    /// (e.g. topic="verdict-strip" -> /docs/verdict-strip) served from
    /// the docs bundled into the image. The pages are authored; each
    /// topic resolves to a real help_topic body and a real docs page.
    #[prop(into)]
    topic: String,
) -> impl IntoView {
    let body = help_topic(&topic);
    let url = doc_url(&topic);
    view! {
        <details class="help-hint">
            <summary
                class="help-hint__trigger"
                aria-label="Show help for this panel"
            >
                "?"
            </summary>
            <div class="help-hint__body">
                <p class="help-hint__text">{body}</p>
                <a
                    class="help-hint__doc-link"
                    href=url
                    target="_blank"
                    rel="noopener noreferrer"
                >
                    "Read full doc \u{2192}"
                </a>
            </div>
        </details>
    }
}

/// Lookup table for panel help text. Keep entries under ~160 characters
/// so the popover doesn't dominate the panel it explains.
pub fn help_topic(topic: &str) -> &'static str {
    match topic {
        "verdict-strip" =>
            "Forecast-driven view of whether each of the next 7 days will water, skip, or run extended, using the same skip-check that fires every morning.",
        "water-budget" =>
            "How much water this week's rain + irrigation has put down vs. what the engine thinks the lawn needed. Negative means the lawn ran short.",
        "zone-math" =>
            "Step-by-step math for the next planned run: ET from the day's weather, multiplied by species Kc, capped at the controller's max duration.",
        "soil-sensors" =>
            "Live soil-moisture percentages from each zone's probe. The engine skips entirely when every zone is at or above its saturation threshold.",
        "first-soil-sensor" =>
            "Three ways to add a soil probe: an Ecowitt gateway on the LAN (native), any MQTT-published probe, or a Home Assistant soil entity. Then bind it to a zone.",
        "forecast" =>
            "Next-rain probability, expected amounts, and the heat-stress + wind metrics the skip rules check. Live, refreshed every 30 minutes.",
        "skip-breakdown" =>
            "Each row shows one skip-rule input next to its threshold. A red bar means that input is currently outside its allowed range and will trip a skip.",
        "advisor" =>
            "Optional LLM advisor. Reads the same inputs the engine uses and explains the verdict in plain English. Off by default; configure under Settings -> LLM.",
        "location" =>
            "Where this deployment sits. Latitude, longitude, and elevation feed the solar + ET math; the timezone sets when the nightly verdict runs.",
        "llm" =>
            "Connect an optional LLM provider (Ollama, llama.cpp, or any OpenAI-compatible endpoint) so the advisor can explain verdicts. Leave blank to keep it off.",
        "notifications" =>
            "Outbound channels for run/skip alerts: Web Push, MQTT discovery, ntfy, and Slack. Each is independent; fill in only the ones you use.",
        "history" =>
            "What actually happened: completed runs and skipped evenings over the selected window. The timeline plots every run per zone; per-zone cards break down minutes, cadence, and recent events.",
        "radar" =>
            "Animated precipitation on your station, plus optional alert, storm, lightning, and wind overlays. Providers default by region (Auto); switch to Custom to pick your own.",
        "restrictions" =>
            "Encode local watering rules: allowed days by address parity, forbidden hours, seasonal windows, and a per-zone minute cap. They gate the verdict before the weather rules.",
        "advanced" =>
            "Debug and recovery: Nerd mode shows the raw engine math, Kiosk mode hides controls on shared screens, plus config-rollback snapshots and full backup/restore.",
        "controllers" =>
            "The hardware that fires your valves. OpenSprinkler talks direct on the LAN; a DIY ESP32 board works over a simple HTTP contract or MQTT; Rachio, Hydrawise, B-hyve and Rain Bird use cloud APIs; HA covers the rest.",
        "devices" =>
            "Every controller, source, and sensor LocalSky uses, native or mirrored from Home Assistant. Add a source or controller here, or scan the LAN to adopt a gateway.",
        "schedules" =>
            "Fire a zone at a fixed weekday and time. Override replaces the smart engine for that zone; Floor fires alongside it. Restrictions still gate and cap each run.",
        "skip-rules" =>
            "The checks the engine runs every morning before watering: rain already fallen + forecast, freeze, wind, and heat-stress. Cross any threshold and tonight's run is skipped. The defaults suit most lawns; tune only if your climate needs it.",
        "sources" =>
            "Where weather data comes in: a local station (Tempest, Ecowitt), a cloud service (Open-Meteo, WeatherKit, NWS), MQTT, or Home Assistant. When several report the same reading, the higher-priority source wins. Add and edit these in Devices.",
        "zones" =>
            "A zone is one chunk of yard on one valve. Its grass species, soil texture, and area drive how much water the engine schedules; everything else has a sensible default under Advanced.",
        "theme" =>
            "How LocalSky looks on this device: Dark, Light, Auto (follow your system), or High-contrast. Per-browser and applied instantly.",
        "units" =>
            "Imperial or Metric for what you see (temperature, rain, wind, area). The engine works in metric internally, so switching mostly changes what you read. Zone area is the exception: you enter it, so its unit feeds the water math.",
        _ => "(no help topic configured)",
    }
}
