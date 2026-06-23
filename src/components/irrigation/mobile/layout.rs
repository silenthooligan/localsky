// Mobile irrigation layout. Backs the mobile /irrigation page by rendering
// the "Now" overview (hero, advisor, controls).
//
// The running banner is rendered by the parent IrrigationPage above this
// component, so it persists across navigation.

use crate::components::irrigation::mobile::now::MobileNow;
use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;

#[component]
pub fn MobileIrrigation(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! { <MobileNow snap/> }
}
