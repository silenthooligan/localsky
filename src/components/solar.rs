// UV / Illuminance / Solar radiation row. UV uses the WHO 0–11+ scale
// with its standard color steps (green/yellow/orange/red/purple).

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn SolarPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let uv_class = move || {
        let uv = snap.get().uv_index;
        match uv {
            x if x < 3.0 => "uv-low",
            x if x < 6.0 => "uv-moderate",
            x if x < 8.0 => "uv-high",
            x if x < 11.0 => "uv-very-high",
            _ => "uv-extreme",
        }
    };
    let uv_label = move || {
        let uv = snap.get().uv_index;
        match uv {
            x if x < 3.0 => "low",
            x if x < 6.0 => "moderate",
            x if x < 8.0 => "high",
            x if x < 11.0 => "very high",
            _ => "extreme",
        }
    };
    let uv_pct = move || (snap.get().uv_index / 11.0 * 100.0).clamp(0.0, 100.0);

    view! {
        <section class="panel solar">
            <h2 class="panel-title">"Sun"</h2>
            <div class="solar-row">
                <div class={move || format!("uv-block {}", uv_class())}>
                    <div class="uv-number">{move || format!("{:.1}", snap.get().uv_index)}</div>
                    <div class="uv-label">{uv_label}</div>
                    <div class="uv-bar">
                        <div class="uv-bar-fill" style=move || format!("width: {:.1}%;", uv_pct())></div>
                    </div>
                </div>
                <div class="solar-stats">
                    <div class="kv">
                        <span class="k">"solar"</span>
                        <span class="v">{move || format!("{:.0} W/m²", snap.get().solar_w_m2)}</span>
                    </div>
                    <div class="kv">
                        <span class="k">"illuminance"</span>
                        <span class="v">{move || format_lux(snap.get().illuminance_lx)}</span>
                    </div>
                </div>
            </div>
        </section>
    }
}

fn format_lux(lx: f64) -> String {
    if lx >= 1000.0 {
        format!("{:.1} klx", lx / 1000.0)
    } else {
        format!("{:.0} lx", lx)
    }
}
