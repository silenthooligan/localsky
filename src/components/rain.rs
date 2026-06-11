// Rain panel: instantaneous rate, today accumulation, precip-type tag.

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn RainPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let kind = move || match snap.get().precip_type {
        1 => "rain",
        2 => "hail",
        _ => "none",
    };
    // Cap the visual gauge at 1 in/hr, heavier than that pegs the meter.
    let pct = move || (snap.get().rain_intensity_in_hr / 1.0 * 100.0).clamp(0.0, 100.0);

    view! {
        <section class="panel rain">
            <h2 class="panel-title">"Rain"</h2>
            <div class={move || format!("rain-pill kind-{}", kind())}>{kind}</div>
            <div class="rain-rate">
                <div class="big-number">
                    {move || format!("{:.2}", snap.get().rain_intensity_in_hr)}
                    <span class="big-unit">" in/hr"</span>
                </div>
                <div class="rain-meter">
                    <div class="rain-meter-fill"
                        style=move || format!("width: {:.1}%;", pct())></div>
                </div>
            </div>
            <div class="rain-stats">
                <div class="kv">
                    <span class="k">"today"</span>
                    <span class="v">{move || format!("{:.2} in", snap.get().rain_in_today)}</span>
                </div>
                <div class="kv">
                    <span class="k">"last min"</span>
                    <span class="v">{move || format!("{:.3} in", snap.get().rain_in_last_min)}</span>
                </div>
            </div>
        </section>
    }
}
