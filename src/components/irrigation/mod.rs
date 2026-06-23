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
pub mod mobile;
pub mod running_banner;
pub mod verdict_strip;
pub mod zone_math;

use crate::ha::snapshot::IrrigationSnapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

use controls::{OverrideControl, StopAllPanel};
use forecast::ForecastPanel;
use hero::NextRunHero;
use mobile::MobileIrrigation;
use running_banner::RunningBanner;
use verdict_strip::VerdictStrip;

#[component]
pub fn IrrigationPage(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let is_mobile = use_context::<RwSignal<bool>>();

    let body = move || {
        let mobile = is_mobile.map(|s| s.get()).unwrap_or(false);
        if mobile {
            view! { <MobileIrrigation snap/> }.into_any()
        } else {
            // /irrigation = "Today" summary on the v2 primitives. A live
            // KPI strip leads, then the 7-day verdict strip and the
            // hero/stop/forecast columns. Zones + History are now
            // top-level routes (/zones, /history), so the old per-route
            // tab toolbar is gone.
            view! {
                <div class="ir-stack">
                    <IrrigationKpis snap/>
                    <VerdictStrip snap/>
                    <div class="ir-two-col">
                        // Left column: hero + Stop All pill stacked.
                        // Stop All lives directly under the hero so the
                        // user's eye finds it without scrolling.
                        <div class="ir-hero-col">
                            <NextRunHero snap/>
                            <OverrideControl current=Signal::derive(move || snap.get().global_override.clone())/>
                            <StopAllPanel snap/>
                        </div>
                        // Right column: the wider data surface.
                        <ForecastPanel snap/>
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

/// Live KPI strip for the irrigation "Today" home. Reads the streamed
/// snapshot: tonight's planned total, how many zones are due, the
/// controller water level, and the average soil deficit. Built on the v2
/// StatTile so it matches the marquee pages.
#[component]
fn IrrigationKpis(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    use crate::components::ui::StatTile;
    move || {
        let s = snap.get();
        let tonight = format!("{:.0}", s.next_run_total_minutes);
        let due = s
            .zones
            .iter()
            .filter(|z| z.planned_run_seconds > 0)
            .count()
            .to_string();
        let water_level = format!("{:.0}", s.water_level_pct);
        let deficit = if s.zones.is_empty() {
            "-".to_string()
        } else {
            let avg = s.zones.iter().map(|z| z.bucket_mm).sum::<f64>() / s.zones.len() as f64;
            format!("{avg:.1}")
        };
        let verdict_accent = match s.skip_check.verdict.as_str() {
            "run" => "var(--verdict-run)",
            "run_extended" => "var(--verdict-extend)",
            "skip" => "var(--verdict-skip)",
            _ => "var(--accent)",
        };
        view! {
            <div class="ir-kpis">
                <StatTile label="Tonight" value=tonight unit="min" icon="droplet" accent=verdict_accent.to_string()/>
                <StatTile label="Zones due" value=due icon="zones" accent="var(--accent-good)".to_string()/>
                <StatTile label="Water level" value=water_level unit="%" icon="gauge" accent="var(--accent-cool)".to_string()/>
                <StatTile label="Soil deficit" value=deficit unit="mm" icon="history" accent="var(--accent-warm)".to_string()/>
            </div>
        }
    }
}
