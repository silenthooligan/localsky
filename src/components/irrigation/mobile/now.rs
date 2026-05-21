// Mobile "Now" tab — single-column stack of the most-glanceable cells:
// next-run hero (which already nests AdvisorExplanation + SkipBreakdown),
// today/tomorrow forecast, and a compact stop-all area. The persistent
// running banner is at the top of IrrigationPage, so when something is
// actively watering the user sees it before any of this.

use crate::components::irrigation::controls::StopAllPanel;
use crate::components::irrigation::forecast::ForecastPanel;
use crate::components::irrigation::hero::NextRunHero;
use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn MobileNow(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <div class="mobile-stack">
            {view! { <NextRunHero snap/> }.into_any()}
            {view! { <ForecastPanel snap/> }.into_any()}
            {view! { <StopAllPanel snap/> }.into_any()}
        </div>
    }
}
