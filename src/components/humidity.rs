// Humidity panel: relative humidity headline + dew point and wet-bulb as
// secondary stats. Rounds out the weather panel grid to an even six (3x2) so
// no card has to stretch to fill a short row, and surfaces dew/wet-bulb, which
// aren't shown as their own card anywhere else.

use crate::components::units_fmt::{fmt_temp_short, use_unit_prefs};
use crate::tempest::state::Snapshot;
use leptos::prelude::*;

#[component]
pub fn HumidityPanel(snap: ReadSignal<Snapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    view! {
        <section class="panel humidity">
            <h2 class="panel-title">"Humidity"</h2>
            <div class="big-number">
                {move || format!("{:.0}", snap.get().rh_pct)}
                <span class="big-unit">" %"</span>
            </div>
            <div class="panel-substats">
                <div class="panel-substat">
                    <span class="panel-substat__k">"Dew point"</span>
                    <span class="panel-substat__v">
                        {move || fmt_temp_short(snap.get().dew_point_f, prefs.get())}
                    </span>
                </div>
                <div class="panel-substat">
                    <span class="panel-substat__k">"Wet bulb"</span>
                    <span class="panel-substat__v">
                        {move || fmt_temp_short(snap.get().wet_bulb_f, prefs.get())}
                    </span>
                </div>
            </div>
        </section>
    }
}
