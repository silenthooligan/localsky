// Weather telemetry strip for the Weather home. A row of v2 StatTiles —
// the big number is the live reading from the Tempest snapshot, the
// sparkline is the last 24h from /api/v1/weather/history (populated by the
// weather sampler). Brings the data-dense, trend-aware dashboard look to
// the weather side, matching the marquee pages.
//
// SSR-safe: history is empty on the SSR + hydrate first frame (so tiles
// render without sparklines), then the one-shot fetch fills them in.

use leptos::prelude::*;

use crate::components::ui::StatTile;
use crate::history::types::WeatherHistory;
use crate::tempest::state::Snapshot;

#[component]
pub fn WeatherTelemetry(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let hist = RwSignal::new(WeatherHistory::default());

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            leptos::task::spawn_local(async move {
                if let Ok(resp) = gloo_net::http::Request::get("/api/v1/weather/history?hours=24")
                    .send()
                    .await
                {
                    if let Ok(w) = resp.json::<WeatherHistory>().await {
                        hist.set(w);
                    }
                }
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = hist;

    view! {
        <section class="wx-telemetry" aria-label="Current weather telemetry">
            {move || {
                let s = snap.get();
                let h = hist.get();
                view! {
                    <StatTile label="Temp" value=format!("{:.0}", s.air_temp_f) unit="°F" icon="thermometer" spark=h.air_temp_f accent="var(--accent-warm)".to_string()/>
                    <StatTile label="Humidity" value=format!("{:.0}", s.rh_pct) unit="%" icon="droplet" spark=h.rh_pct accent="var(--accent-rain)".to_string()/>
                    <StatTile label="Wind" value=format!("{:.0}", s.wind_avg_mph) unit="mph" icon="wind" spark=h.wind_avg_mph accent="var(--accent-cool)".to_string()/>
                    <StatTile label="Pressure" value=format!("{:.2}", s.pressure_inhg) unit="inHg" icon="gauge" spark=h.pressure_inhg accent="var(--accent)".to_string()/>
                    <StatTile label="Solar" value=format!("{:.0}", s.solar_w_m2) unit="W/m²" icon="sun" spark=h.solar_w_m2 accent="var(--accent-warm)".to_string()/>
                    <StatTile label="UV index" value=format!("{:.0}", s.uv_index) icon="sun" spark=h.uv_index accent="var(--accent-hot)".to_string()/>
                }
            }}
        </section>
    }
}
