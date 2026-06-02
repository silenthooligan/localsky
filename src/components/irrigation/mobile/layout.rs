// Mobile irrigation layout dispatcher. Reads ?tab= from the URL and renders
// the matching sub-view. Default (no query, or unknown value) is `now`.
//
// All sub-views read the same IrrigationSnapshot signal, so switching tabs
// is a pure render swap — no fetch, no flash. The running banner is rendered
// by the parent IrrigationPage above the dispatcher, so it persists across
// tab swaps.

use crate::components::irrigation::mobile::{
    now::MobileNow, schedule::MobileSchedule, zones::MobileZones,
};
use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

#[component]
pub fn MobileIrrigation(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let q = use_query_map();
    let tab = move || {
        q.get()
            .get("tab")
            .map(|s| s.to_string())
            .unwrap_or_else(|| "now".to_string())
    };

    move || match tab().as_str() {
        "zones" => view! { <MobileZones snap/> }.into_any(),
        "schedule" => view! { <MobileSchedule snap/> }.into_any(),
        _ => view! { <MobileNow snap/> }.into_any(),
    }
}
