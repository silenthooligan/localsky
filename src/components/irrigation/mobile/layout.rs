// Mobile irrigation layout. Backs the mobile /irrigation page by rendering
// the "Now" overview (hero, advisor, controls).
//
// The running banner is rendered by the parent IrrigationPage above this
// component, so it persists across navigation. The tuning-report signal
// comes from the parent too (one fetch per page), so the mobile branch
// carries the same tuning strip as desktop.

use crate::components::irrigation::mobile::now::MobileNow;
use crate::ha::snapshot::IrrigationSnapshot;
use crate::history::types::TuningReport;
use leptos::prelude::*;

#[component]
pub fn MobileIrrigation(
    snap: ReadSignal<IrrigationSnapshot>,
    report: RwSignal<Option<TuningReport>>,
) -> impl IntoView {
    view! { <MobileNow snap report/> }
}
