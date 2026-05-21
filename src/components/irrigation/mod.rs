// Irrigation page orchestrator. Renders the bento layout and wires
// each cell to the IrrigationSnapshot signal. Reads from the same
// arc-swap-backed signal pattern the Tempest page uses.
//
// Type-erase each cell via .into_any() so rustc's query depth doesn't
// overflow on the fully-monomorphized view tree. Same workaround the
// weather page uses (see app.rs::WeatherHome).

pub mod advisor;
pub mod controls;
pub mod forecast;
pub mod hero;
pub mod history;
pub mod mobile;
pub mod per_zone_history;
pub mod running_banner;
pub mod soil_sensors;
pub mod verdict_strip;
pub mod water_budget;
pub mod zone_math;
pub mod zones;

use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use controls::{StopAllPanel, ThresholdsPanel};
use forecast::ForecastPanel;
use hero::NextRunHero;
use history::HistoryPanel;
use mobile::MobileIrrigation;
use per_zone_history::PerZoneHistory;
use running_banner::RunningBanner;
use soil_sensors::SoilSensors;
use verdict_strip::VerdictStrip;
use water_budget::WaterBudgetPanel;
use zone_math::ZoneMathPanel;
use zones::ZoneGrid;

#[component]
pub fn IrrigationPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let is_mobile = use_context::<RwSignal<bool>>();

    let body = move || {
        let mobile = is_mobile.map(|s| s.get()).unwrap_or(false);
        if mobile {
            view! { <MobileIrrigation snap/> }.into_any()
        } else {
            view! {
                <div class="bento bento-irrigation">
                    <div class="bento-area-verdict-strip">
                        {view! { <VerdictStrip snap/> }.into_any()}
                    </div>
                    <div class="bento-area-hero">
                        {view! { <NextRunHero snap/> }.into_any()}
                    </div>
                    <div class="bento-area-forecast">
                        {view! { <ForecastPanel snap/> }.into_any()}
                    </div>
                    <div class="bento-area-zones">
                        {view! { <ZoneGrid snap/> }.into_any()}
                    </div>
                    <div class="bento-area-zone-math">
                        {view! { <ZoneMathPanel snap/> }.into_any()}
                    </div>
                    <div class="bento-area-water-budget">
                        {view! { <WaterBudgetPanel snap/> }.into_any()}
                    </div>
                    <div class="bento-area-soil">
                        {view! { <SoilSensors snap/> }.into_any()}
                    </div>
                    <div class="bento-area-stop">
                        {view! { <StopAllPanel snap/> }.into_any()}
                    </div>
                    <div class="bento-area-thresholds">
                        {view! { <ThresholdsPanel snap/> }.into_any()}
                    </div>
                    <div class="bento-area-history">
                        {view! { <HistoryPanel/> }.into_any()}
                    </div>
                    <div class="bento-area-zone-history">
                        {view! { <PerZoneHistory/> }.into_any()}
                    </div>
                </div>
            }
            .into_any()
        }
    };

    view! {
        // Banner is sticky at the top so a running zone is always visible
        // and one tap from being stopped, regardless of scroll position.
        // Hidden when no zone is active. Renders identically on mobile and
        // desktop; SCSS handles position differences.
        <RunningBanner snap/>
        {body}
    }
}
