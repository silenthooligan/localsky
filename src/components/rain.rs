// Rain panel: instantaneous rate, today accumulation, precip-type tag.

use crate::components::units_fmt::{fmt_rain_amount, fmt_rain_rate, use_unit_prefs};
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn RainPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // Cloud-only (Open-Meteo) deployment: keyed on the canonical has_live_station
    // signal (any live station, not the broken Tempest-only serial+battery test).
    // The per-minute rain delta is a station-only reading; Open-Meteo has no such
    // value, so on a cloud-only install "last min 0.00 in" reads as a false live
    // observation. Show "station only" there instead, matching the Sun/Wind
    // deliberate-empty pattern.
    let cloud_only = move || !snap.get().has_live_station;
    let kind = move || match snap.get().precip_type {
        1 => "rain",
        2 => "hail",
        _ => "none",
    };
    // Cap the visual gauge at 1 in/hr, heavier than that pegs the meter.
    // Threshold math stays in inches (the stored/internal unit).
    let pct = move || (snap.get().rain_intensity_in_hr / 1.0 * 100.0).clamp(0.0, 100.0);

    view! {
        <section class="panel rain">
            <h2 class="panel-title">"Rain"</h2>
            <div class={move || format!("rain-pill kind-{}", kind())}>{kind}</div>
            <div class="rain-rate">
                <div class="big-number">
                    {move || fmt_rain_rate(snap.get().rain_intensity_in_hr, prefs.get())}
                </div>
                <div class="rain-meter">
                    <div class="rain-meter-fill"
                        style=move || format!("width: {:.1}%;", pct())></div>
                </div>
            </div>
            <div class="rain-stats">
                <div class="kv">
                    <span class="k">"today"</span>
                    <span class="v">
                        {move || fmt_rain_amount(snap.get().rain_in_today, prefs.get())}
                    </span>
                </div>
                <Show
                    when=move || !cloud_only()
                    fallback=|| view! {
                        <div class="kv kv--muted">
                            <span class="k">"last min"</span>
                            <span class="v" style="color:var(--text-dim);font-size:0.8em;">
                                "station only"
                            </span>
                        </div>
                    }
                >
                    <div class="kv">
                        <span class="k">"last min"</span>
                        <span class="v">
                            {move || fmt_rain_amount(snap.get().rain_in_last_min, prefs.get())}
                        </span>
                    </div>
                </Show>
            </div>
        </section>
    }
}
