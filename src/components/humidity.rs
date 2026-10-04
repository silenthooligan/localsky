// Humidity panel: relative humidity headline + dew point and wet-bulb as
// secondary stats. Rounds out the weather panel grid to an even six (3x2) so
// no card has to stretch to fill a short row, and surfaces dew/wet-bulb, which
// aren't shown as their own card anywhere else.

use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn HumidityPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    view! {
        <section class="panel humidity">
            <h2 class="panel-title">"Humidity"</h2>
            <div class="big-number">
                {move || format!("{:.0}", snap.get().rh_pct)}
                <span class="big-unit">" %"</span>
            </div>
            <div class="panel-substats">
                <crate::components::ui::StatTile layout="compact" label="Dew point"
                    temperature_f=Signal::derive(move || snap.get().dew_point_f)/>
                <crate::components::ui::StatTile layout="compact" label="Wet bulb"
                    temperature_f=Signal::derive(move || snap.get().wet_bulb_f)/>
            </div>
        </section>
    }
}
