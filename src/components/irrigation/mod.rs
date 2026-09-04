// Irrigation page orchestrator. Renders the bento layout and wires
// each cell to the IrrigationSnapshot signal. Reads from the same
// arc-swap-backed signal pattern the Tempest page uses.
//
// Type-erase each cell via .into_any() so rustc's query depth doesn't
// overflow on the fully-monomorphized view tree. Same workaround the
// weather page uses (see app.rs::WeatherHome).

pub mod advisor;
pub mod anomaly_banner;
pub mod controls;
pub mod default_budget_banner;
pub mod forecast;
pub mod ha_adoption_banner;
pub mod hero;
pub mod mobile;
pub mod running_banner;
pub mod verdict_strip;

use crate::components::ui::EmptyState;
use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use crate::components::notice_center::NoticeCenter;
use anomaly_banner::AnomalyBanner;
use controls::{OverrideControl, RainDelayPanel, StopAllPanel};
use forecast::ForecastPanel;
use hero::NextRunHero;
use mobile::MobileIrrigation;
use running_banner::RunningBanner;
use verdict_strip::VerdictStrip;

/// No-hardware empty state for the irrigation page. A zero-zone install has no
/// controller/zones, so the hero + Stop-All + rain-delay + override would all be
/// inert controls acting on nothing. The page is still reachable by direct URL
/// even when the nav entry is hidden, so render a clear "set up" CTA here
/// instead of dead buttons. Zones need a controller first (see settings/
/// zones.rs: "configure one under /settings/controllers first"), so the CTA
/// points at /settings/controllers, the required first step.
#[component]
fn NoZonesEmpty() -> impl IntoView {
    view! {
        <EmptyState
            icon="controllers"
            title="No zones yet".to_string()
            body="Irrigation is idle: no controllers or zones are configured, so there is nothing to schedule, skip, or stop. Add a controller, then your zones. The weather home works without any of this.".to_string()
            cta_label="Add a controller".to_string()
            cta_href="/settings/controllers".to_string()
        />
    }
}

#[component]
pub fn IrrigationPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let is_mobile = use_context::<RwSignal<bool>>();
    // The app-level tuning report (one fetch, provided by App()); the
    // desktop and mobile branches share it, and the strip placement below
    // reads its count.
    let tuning_report = crate::components::zones::tuning::use_tuning_report();
    // Entering the page re-fetches the shared report (the per-mount
    // freshness the page-local fetch had before the app context), so the
    // strip reflects server-side changes without a reload. On-mount only:
    // the closure reads no signals.
    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        crate::components::zones::tuning::refresh_tuning_report();
    });

    let body = move || {
        // No controller/zones: every irrigation primitive below acts on nothing,
        // so swap the whole control surface for a no-hardware CTA instead of
        // rendering inert hero/Stop-All/rain-delay/override controls.
        if snap.get().zones.is_empty() {
            return view! { <NoZonesEmpty/> }.into_any();
        }
        // Attention precedes data: with a recommendation pending the strip
        // renders ABOVE the data columns; a scorecard-only strip keeps the
        // quiet bottom slot.
        let has_suggestions = tuning_report
            .get()
            .map(|r| crate::components::zones::tuning::recommendation_count(&r) > 0)
            .unwrap_or(false);
        let mobile = is_mobile.map(|s| s.get()).unwrap_or(false);
        if mobile {
            view! { <MobileIrrigation snap report=tuning_report/> }.into_any()
        } else {
            // /irrigation = "Today" summary on the v2 primitives. The 7-day
            // verdict strip leads, then the hero (which now carries the
            // tonight/zones-due/water-level/soil-deficit stats inline) +
            // controls on the left, and the forecast data on the right.
            // Zones + History are now top-level routes (/zones, /history).
            view! {
                <div class="ir-stack">
                    <VerdictStrip snap/>
                    {has_suggestions.then(|| view! {
                        <crate::components::zones::tuning::TuningStrip report=tuning_report/>
                    })}
                    <div class="ir-two-col">
                        // Left column: hero + Stop All pill stacked.
                        // Stop All lives directly under the hero so the
                        // user's eye finds it without scrolling.
                        <div class="ir-hero-col">
                            <NextRunHero snap/>
                            <OverrideControl current=Signal::derive(move || snap.get().global_override.clone())/>
                            <RainDelayPanel snap/>
                            <StopAllPanel snap/>
                        </div>
                        // Right column: the wider data surface.
                        <ForecastPanel snap/>
                    </div>
                    {(!has_suggestions).then(|| view! {
                        <crate::components::zones::tuning::TuningStrip report=tuning_report/>
                    })}
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
        <AnomalyBanner snap/>
        // The one-time notices (the soil opt-in offer, the Home Assistant
        // migration, the inferred-target warning) pop once from the
        // centralized popup and leave the page clear; nothing sits here.
        <NoticeCenter snap/>
        {body}
    }
}
