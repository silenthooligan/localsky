// Big-temperature hero block. Picks a condition glyph from the live state
// (rain → ☔, lightning → ⚡, hot sun → ☀, cloudy default → ⛅) so the page
// has a recognizable mood without needing a 3rd-party icon set.

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn Hero(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let condition = move || {
        let s = snap.get();
        if s.lightning_count_last_min > 0 || s.lightning_strikes_last_hour > 0 {
            ("⚡", "Thunderstorm", "lightning")
        } else if s.precip_type == 1 || s.rain_intensity_in_hr > 0.0 {
            ("🌧", "Raining", "rain")
        } else if s.precip_type == 2 {
            ("🌨", "Hail", "hail")
        } else if s.solar_w_m2 > 600.0 {
            ("☀️", "Sunny", "sunny")
        } else if s.solar_w_m2 > 150.0 {
            ("🌤", "Partly Sunny", "partly")
        } else if s.solar_w_m2 > 30.0 {
            ("⛅", "Cloudy", "cloudy")
        } else {
            ("🌙", "Calm Night", "night")
        }
    };

    view! {
        <section class="hero">
            <div class="hero-glyph" aria-hidden="true">{move || condition().0}</div>
            <div class="hero-numbers">
                <div class="hero-temp">
                    {move || format!("{:.0}°", snap.get().air_temp_f)}
                </div>
                <div class="hero-tag">{move || condition().1}</div>
                <div class="hero-secondary">
                    <span class="kv">
                        <span class="k">"feels"</span>
                        <span class="v">{move || format!("{:.0}°", snap.get().feels_like_f)}</span>
                    </span>
                    <span class="kv">
                        <span class="k">"dew"</span>
                        <span class="v">{move || format!("{:.0}°", snap.get().dew_point_f)}</span>
                    </span>
                    <span class="kv">
                        <span class="k">"humidity"</span>
                        <span class="v">{move || format!("{:.0}%", snap.get().rh_pct)}</span>
                    </span>
                    <span class="kv">
                        <span class="k">"wet bulb"</span>
                        <span class="v">{move || format!("{:.0}°", snap.get().wet_bulb_f)}</span>
                    </span>
                </div>
            </div>
        </section>
    }
}
