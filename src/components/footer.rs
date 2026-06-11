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
        if s.station_serial.is_empty() {
            "Tempest".to_string()
        } else {
            s.station_serial
        }
    };
    view! {
        <footer class="site-footer">
            <span class="footer-station">{serial}</span>
            <span class={move || format!("footer-battery {}", bat_class())}>
                {move || format!("battery {:.0}%, {:.3} V", snap.get().battery_pct, snap.get().battery_v)}
            </span>
        </footer>
    }
}
