// One-time notice: zones about to water on a target LocalSky inferred from
// their name rather than one the operator set.
//
// 0.7.22 made the weekly-budget allocator the single model that sizes
// dispatch on BOTH deployment paths. On a Home Assistant deployment the
// previous model was a Smart Irrigation entity, and an install without that
// integration read an absent entity as a 0.00 deficit, planned zero minutes
// on every zone, and dispatched nothing on every morning since it was
// installed. Those installs begin watering for real on the first morning
// after the upgrade, sized from `agronomic_budget_default`: 0.50 in a week
// over one session for a zone whose slug contains shrub, garden or bed, and
// 1.00 in over two sessions for every other zone.
//
// A yard that starts watering unattended, on a target nobody reviewed, with
// nothing on screen saying so is the failure this release exists to end. So
// the banner names every affected zone and the target it will use, and
// points at the page where the target is set.
//
// It fires on every install where a zone's `weekly_budget_in` or
// `sessions_per_week` is unset, and that includes every zone the setup wizard
// created: the wizard never asked for a target either, so those zones water
// on the same inferred default and the notice is as true for them as for a
// Home Assistant install. A zone with both fields set carries
// `target_inferred = false` and is never listed, so the notice goes quiet
// once the owner has set both on every zone, which the zone editor under
// Settings, then Zones can do from this release.
//
// Dismissal is sticky in localStorage, keyed by the exact set of zones
// listed. Dismissing silences THAT set for good; a zone added later, or one
// whose target is cleared, produces a different key and speaks up again.
// (The same rule the health banner applies per session, persisted, because
// this one is about a decision the operator makes once.)
//
// SSR and the first hydrate frame render nothing: the dismissal is read in a
// hydrate-only effect, so the server DOM and the first client DOM match.

use leptos::prelude::*;

use crate::components::units_fmt::{fmt_rain_amount, use_unit_prefs};
use crate::ha::snapshot::IrrigationSnapshot;

/// localStorage key holding the zone-set the operator dismissed.
#[cfg(feature = "hydrate")]
const DISMISS_KEY: &str = "default_budget_banner_dismissed";

#[component]
pub fn DefaultBudgetBanner(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // The dismissed zone-set key. Empty until the hydrate effect reads it,
    // so SSR and the first client frame agree.
    let dismissed: RwSignal<String> = RwSignal::new(String::new());

    #[cfg(feature = "hydrate")]
    Effect::new(move |_| {
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            if let Ok(Some(v)) = s.get_item(DISMISS_KEY) {
                dismissed.set(v);
            }
        }
    });

    let on_dismiss = move |_| {
        let key = inferred_key(&snap.get_untracked());
        #[cfg(feature = "hydrate")]
        {
            if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
                let _ = s.set_item(DISMISS_KEY, &key);
            }
        }
        dismissed.set(key);
    };

    move || {
        let s = snap.get();
        let p = prefs.get();
        let rows: Vec<String> = s
            .water_budgets
            .iter()
            .filter(|b| b.target_inferred)
            .map(|b| zone_line(&b.zone_name, b.weekly_budget_in, b.sessions_per_week, p))
            .collect();
        if rows.is_empty() || dismissed.get() == inferred_key(&s) {
            return ().into_any();
        }
        view! {
            <div class="anomaly-banner" role="status" aria-live="polite">
                <div class="anomaly-banner-icon" aria-hidden="true">"!"</div>
                <div class="anomaly-banner-text">
                    <div class="anomaly-banner-line">
                        "These zones water on a weekly target inferred from the zone name, because none is set. Set Weekly target and Sessions per week under Settings, then Zones, if that is not what you want."
                    </div>
                    {rows
                        .into_iter()
                        .map(|l| view! { <div class="anomaly-banner-line">{l}</div> })
                        .collect_view()}
                </div>
                <a class="anomaly-banner-link" href="/settings/zones">"Set targets"</a>
                <button
                    type="button"
                    class="anomaly-banner-dismiss"
                    aria-label="Dismiss default watering target notice"
                    on:click=on_dismiss
                >
                    "\u{2715}"
                </button>
            </div>
        }
        .into_any()
    }
}

/// One zone's line: the name, the weekly target it will water on, and how
/// many sessions that target is split across.
fn zone_line(
    name: &str,
    weekly_budget_in: f64,
    sessions_per_week: u32,
    p: crate::components::units_fmt::UnitPrefs,
) -> String {
    let amount = fmt_rain_amount(weekly_budget_in, p);
    let sessions = if sessions_per_week == 1 {
        "1 session".to_string()
    } else {
        format!("{sessions_per_week} sessions")
    };
    format!("{name}: {amount} a week over {sessions}")
}

/// Stable identity for the CURRENT set of inferred-target zones. Dismissal
/// stores this, so silencing one set never silences a different one.
fn inferred_key(s: &IrrigationSnapshot) -> String {
    let mut slugs: Vec<&str> = s
        .water_budgets
        .iter()
        .filter(|b| b.target_inferred)
        .map(|b| b.zone_slug.as_str())
        .collect();
    slugs.sort_unstable();
    slugs.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::units_fmt::UnitPrefs;
    use crate::ha::snapshot::WaterBudget;

    fn budget(slug: &str, inferred: bool) -> WaterBudget {
        WaterBudget {
            zone_slug: slug.to_string(),
            zone_name: slug.to_string(),
            target_inferred: inferred,
            ..Default::default()
        }
    }

    #[test]
    fn zone_line_names_the_target_and_the_session_count() {
        let p = UnitPrefs::default();
        assert_eq!(
            zone_line("Back Yard", 1.00, 2, p),
            "Back Yard: 1.00\" a week over 2 sessions"
        );
        // One session reads as a singular, not "1 sessions".
        assert_eq!(
            zone_line("Side Bed", 0.50, 1, p),
            "Side Bed: 0.50\" a week over 1 session"
        );
        // Metric households read the same line in millimeters.
        let m = crate::components::units_fmt::METRIC;
        assert_eq!(
            zone_line("Back Yard", 1.00, 2, m),
            "Back Yard: 25.4mm a week over 2 sessions"
        );
    }

    #[test]
    fn dismiss_key_covers_the_inferred_zones_only_and_is_order_stable() {
        let mut s = IrrigationSnapshot::default();
        s.water_budgets = vec![
            budget("side_bed", true),
            budget("front_yard", false),
            budget("back_yard", true),
        ];
        assert_eq!(inferred_key(&s), "back_yard,side_bed");
        // A zone whose target the operator later sets changes the key, so a
        // previous dismissal does not silence the remaining zone.
        s.water_budgets[0].target_inferred = false;
        assert_eq!(inferred_key(&s), "back_yard");
    }

    #[test]
    fn a_fully_configured_install_produces_no_key_and_nothing_to_show() {
        let mut s = IrrigationSnapshot::default();
        s.water_budgets = vec![budget("front_yard", false), budget("back_yard", false)];
        assert_eq!(inferred_key(&s), "");
    }
}
