// Bottom strip, battery + station identity. Battery percent comes from
// the linear voltage map in `Snapshot::battery_pct_from_v`.

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn Footer(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let bat_class = move || {
        let pct = snap.get().battery_pct;
        if pct < 25.0 {
            "bat-bad"
        } else if pct < 60.0 {
            "bat-mid"
        } else {
            "bat-good"
        }
    };
    let serial = move || {
        let s = snap.get();
        // A real Tempest reports a station serial; other sources don't, so fall
        // back to the source provenance label ("Ecowitt", "Demo", ...) rather
        // than assuming Tempest.
        if !s.station_serial.is_empty() {
            s.station_serial
        } else if !s.source_label.is_empty() {
            s.source_label
        } else {
            "-".to_string()
        }
    };
    // Only a real physical station has a battery to report. A cloud-only
    // (Open-Meteo) deployment has battery_v == 0 and no serial, so showing
    // "battery 0%, 0.000 V" (red, low-battery class) would be alarming and
    // wrong. Gate the battery span on an actual station being present: a
    // station serial OR a positive battery voltage.
    let has_station_battery = move || {
        let s = snap.get();
        s.battery_v > 0.0 || !s.station_serial.is_empty()
    };
    view! {
        <footer class="site-footer">
            <span class="footer-station">{serial}</span>
            <Show when=has_station_battery>
                <span class={move || format!("footer-battery {}", bat_class())}>
                    {move || format!("battery {:.0}%, {:.3} V", snap.get().battery_pct, snap.get().battery_v)}
                </span>
            </Show>
        </footer>
    }
}
