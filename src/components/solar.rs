// UV / Illuminance / Solar radiation row. UV uses the WHO 0 to 11+ scale
// with its standard color steps (green/yellow/orange/red/purple).

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

/// True for a cloud-only (Open-Meteo) deployment. Keys on the canonical
/// `has_live_station` signal (true for any live station: Tempest / Ecowitt /
/// Davis / MQTT / ...), NOT the old Tempest-only serial + battery heuristic that
/// misread a live non-Tempest station as cloud-only. UV + solar irradiance are
/// populated from Open-Meteo, so they stay live; only the station-exclusive
/// illuminance (lux) lacks data on a true cloud-only install.
fn is_cloud_only(s: &Snapshot) -> bool {
    !s.has_live_station
}

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
    // Illuminance (lux) comes only from a physical station's light sensor;
    // Open-Meteo provides solar irradiance + UV but never lux. For a cloud-only
    // deployment, show a one-line explainer in that slot instead of a
    // permanently-zeroed "0 lx", so the panel reads as deliberate.
    let cloud_only = move || is_cloud_only(&snap.get());

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
                    <Show
                        when=move || !cloud_only()
                        fallback=|| view! {
                            <div class="kv kv--muted">
                                <span class="k">"illuminance"</span>
                                <span class="v" style="color:var(--text-dim);font-size:0.8em;">
                                    "station only"
                                </span>
                            </div>
                        }
                    >
                        <div class="kv">
                            <span class="k">"illuminance"</span>
                            <span class="v">{move || format_lux(snap.get().illuminance_lx)}</span>
                        </div>
                    </Show>
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
