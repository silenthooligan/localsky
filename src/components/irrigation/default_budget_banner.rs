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
// the notice names every affected zone and the target it will use, and
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
// PRESENTATION lives in the centralized notice popup
// (`components::notice_center`), which pops the notice once and keeps the
// page clear; this module owns the copy, the identity key, and the stored
// dismissal, all unchanged from the page-strip era so a dismissal recorded
// then still holds.

use crate::components::units_fmt::{fmt_rain_amount, UnitPrefs};
use crate::ha::snapshot::IrrigationSnapshot;

/// localStorage key holding the zone-set the operator dismissed.
#[cfg(feature = "hydrate")]
const DISMISS_KEY: &str = "default_budget_banner_dismissed";

/// The stored dismissal key ("" when none). Off the browser there is no
/// storage and nothing was dismissed. Only the hydrate build has a call
/// site (the popup center's mount effect), hence the cfg_attr.
#[cfg_attr(not(feature = "hydrate"), allow(dead_code))]
pub(crate) fn read_dismissed() -> String {
    #[cfg(feature = "hydrate")]
    {
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            if let Ok(Some(v)) = s.get_item(DISMISS_KEY) {
                return v;
            }
        }
    }
    String::new()
}

/// Record a dismissal for the given zone-set key.
pub(crate) fn store_dismissed(key: &str) {
    #[cfg(feature = "hydrate")]
    {
        if let Some(s) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
            let _ = s.set_item(DISMISS_KEY, key);
        }
    }
    #[cfg(not(feature = "hydrate"))]
    let _ = key;
}

/// A row the notice's claim is TRUE for: an inferred target on a zone
/// the WEEKLY model governs. A soil-governed zone never waters on the
/// inferred figure (an inferred target caps nothing there), so listing
/// it steered owners into setting a weekly target that becomes a
/// delivery ceiling, the exact starvation the tuning report's ceiling
/// arm exists to catch; and on a fresh install, which lands
/// soil-governed on day one, every listed zone was wrong. An empty
/// `scheduling_model` (older producer) reads as weekly.
fn weekly_inferred(b: &crate::ha::snapshot::WaterBudget) -> bool {
    b.on_inferred_weekly_target()
}

/// The notice's copy: the instruction first, then one line per zone
/// still watering on an inferred target. Empty when every zone carries
/// a target the operator set, or waters by soil deficit, which is what
/// retires the notice.
pub(crate) fn lines(s: &IrrigationSnapshot, p: UnitPrefs) -> Vec<String> {
    let rows: Vec<String> = s
        .water_budgets
        .iter()
        .filter(|b| weekly_inferred(b))
        .map(|b| zone_line(&b.zone_name, b.weekly_budget_in, b.sessions_per_week, p))
        .collect();
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        "These zones water on a starting target taken from what each one is \
         planted with. Nobody set a target, so the engine used the species. \
         Below is what they run on today. Set your own under Settings, then \
         Zones, if you want different."
            .to_string(),
    ];
    lines.extend(rows);
    lines
}

/// One zone's line: the name, the weekly target it will water on, and how
/// many sessions that target is split across.
pub(crate) fn zone_line(
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

/// Stable identity for the CURRENT set of listed zones (same filter as
/// `lines`, so the dismissal key always matches what was shown).
/// Dismissal stores this, so silencing one set never silences a
/// different one; a zone leaving soil governance later produces a new
/// key and speaks up again.
pub(crate) fn inferred_key(s: &IrrigationSnapshot) -> String {
    let mut slugs: Vec<&str> = s
        .water_budgets
        .iter()
        .filter(|b| weekly_inferred(b))
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
        assert!(lines(&s, UnitPrefs::default()).is_empty());
    }

    /// A soil-governed zone never waters on the inferred target, so it
    /// is never listed and never keys the dismissal: an all-soil install
    /// (a fresh install's day-one shape) shows nothing at all, and a
    /// mixed install lists only the weekly-governed zones the claim is
    /// true for.
    #[test]
    fn soil_governed_zones_are_never_listed() {
        let mut s = IrrigationSnapshot::default();
        let mut soil = budget("front_yard", true);
        soil.scheduling_model = "soil".into();
        s.water_budgets = vec![soil, budget("back_yard", true)];
        assert_eq!(inferred_key(&s), "back_yard");
        let lines = lines(&s, UnitPrefs::default());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("front_yard")), "{lines:?}");

        // All zones soil-governed: the notice retires entirely.
        for b in s.water_budgets.iter_mut() {
            b.scheduling_model = "soil".into();
        }
        assert_eq!(inferred_key(&s), "");
        assert!(super::lines(&s, UnitPrefs::default()).is_empty());
    }

    /// The popup copy: the instruction leads, then one line per zone on
    /// an inferred target and no line for a configured one.
    #[test]
    fn the_notice_lines_lead_with_the_instruction_then_the_zones() {
        let mut s = IrrigationSnapshot::default();
        let mut back = budget("back_yard", true);
        back.zone_name = "Back Yard".into();
        back.weekly_budget_in = 1.0;
        back.sessions_per_week = 2;
        s.water_budgets = vec![back, budget("front_yard", false)];
        let lines = lines(&s, UnitPrefs::default());
        assert_eq!(
            lines,
            vec![
                "These zones water on a starting target taken from what each one is \
                 planted with. Nobody set a target, so the engine used the species. \
                 Below is what they run on today. Set your own under Settings, then \
                 Zones, if you want different."
                    .to_string(),
                "Back Yard: 1.00\" a week over 2 sessions".to_string(),
            ]
        );
    }
}
