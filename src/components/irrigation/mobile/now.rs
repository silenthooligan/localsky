// Mobile "Now" tab, single-column stack of the most-glanceable cells:
// next-run hero (which already nests AdvisorExplanation + SkipBreakdown),
// today/tomorrow forecast, and a compact stop-all area. The persistent
// running banner is at the top of IrrigationPage, so when something is
// actively watering the user sees it before any of this. The tuning
// strip mirrors the desktop placement rule: above the data when a
// suggestion exists, in the quiet bottom slot when it only carries the
// scorecard.

use crate::components::irrigation::controls::{OverrideControl, StopAllPanel};
use crate::components::irrigation::forecast::ForecastPanel;
use crate::components::irrigation::hero::NextRunHero;
use crate::components::zones::tuning::{recommendation_count, TuningStrip};
use crate::ha::snapshot::IrrigationSnapshot;
use crate::history::types::TuningReport;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn MobileNow(
    snap: ReadSignal<IrrigationSnapshot>,
    report: RwSignal<Option<TuningReport>>,
) -> impl IntoView {
    let has_suggestions = move || {
        report
            .get()
            .map(|r| recommendation_count(&r) > 0)
            .unwrap_or(false)
    };
    view! {
        <div class="mobile-stack">
            {move || has_suggestions().then(|| view! { <TuningStrip report/> }.into_any())}
            {view! { <NextRunHero snap/> }.into_any()}
            {view! {
                <OverrideControl current=Signal::derive(move || snap.get().global_override.clone())/>
            }.into_any()}
            {view! { <ForecastPanel snap/> }.into_any()}
            {view! { <StopAllPanel snap/> }.into_any()}
            {move || (!has_suggestions()).then(|| view! { <TuningStrip report/> }.into_any())}
        </div>
    }
}
