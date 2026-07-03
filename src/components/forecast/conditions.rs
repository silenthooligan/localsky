// Condition-aware "Heads up" cards. The dashboard adapts to what the
// atmosphere is actually doing at THIS location: a Colorado install
// surfaces snow depth and the freezing level, a coastal install surfaces
// fog windows, a Florida install surfaces storm potential, and none of
// them see the others' cards. Every card has a TRIGGER derived from the
// forecast snapshot and renders nothing when its condition is absent, so
// the section costs zero attention on a calm day (and disappears
// entirely).
//
// Data honesty: all inputs are extended model variables (0 = provider
// doesn't report it), so every trigger requires a POSITIVE signal, never
// "zero looks alarming" (e.g. visibility 0 means "unknown", not "fog").
// Advisory display only; nothing here feeds a skip decision.

use crate::forecast::snapshot::ForecastSnapshot;
use crate::timefmt::format_hm;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

/// Feet per statute mile, for the visibility card.
const FT_PER_MI: f64 = 5280.0;

struct ConditionCard {
    icon: &'static str,
    title: &'static str,
    /// (key, value) rows.
    rows: Vec<(String, String)>,
    /// Escalation styles the card border/title.
    alert: bool,
}

fn winter_card(s: &ForecastSnapshot) -> Option<ConditionCard> {
    let snow_48h: f64 = s.hourly.iter().take(48).map(|h| h.snowfall_in).sum();
    let depth_ft = s
        .hourly
        .iter()
        .map(|h| h.snow_depth_ft)
        .find(|v| *v > 0.0)
        .unwrap_or(0.0);
    if snow_48h < 0.1 && depth_ft < 0.05 {
        return None;
    }
    let mut rows = Vec::new();
    if snow_48h >= 0.1 {
        rows.push(("Next 48h snowfall".to_string(), format!("{snow_48h:.1} in")));
    }
    if depth_ft >= 0.05 {
        rows.push((
            "Snow on ground".to_string(),
            format!("{:.1} in", depth_ft * 12.0),
        ));
    }
    let freezing_ft = s
        .hourly
        .iter()
        .map(|h| h.freezing_level_ft)
        .find(|v| *v > 0.0)
        .unwrap_or(0.0);
    if freezing_ft > 0.0 {
        rows.push((
            "Freezing level".to_string(),
            format!("{:.1} kft", freezing_ft / 1000.0),
        ));
    }
    if let Some(min) = s
        .daily
        .iter()
        .take(2)
        .map(|d| d.temp_min_f)
        .min_by(|a, b| a.total_cmp(b))
    {
        rows.push(("Coldest night".to_string(), format!("{min:.0}F")));
    }
    Some(ConditionCard {
        icon: "snowflake",
        title: "Winter",
        rows,
        alert: snow_48h >= 3.0,
    })
}

fn fog_card(s: &ForecastSnapshot) -> Option<ConditionCard> {
    // Positive-signal only: hours with visibility REPORTED and under 2 mi.
    let low: Vec<&crate::forecast::snapshot::HourlyEntry> = s
        .hourly
        .iter()
        .take(24)
        .filter(|h| h.visibility_ft > 0.0 && h.visibility_ft < 2.0 * FT_PER_MI)
        .collect();
    let worst = low
        .iter()
        .min_by(|a, b| a.visibility_ft.total_cmp(&b.visibility_ft))?;
    Some(ConditionCard {
        icon: "cloud-fog",
        title: "Low visibility",
        rows: vec![
            (
                "Worst".to_string(),
                format!(
                    "{:.1} mi at {}",
                    worst.visibility_ft / FT_PER_MI,
                    format_hm(worst.time_epoch, &s.timezone)
                ),
            ),
            (
                "Hours under 2 mi (24h)".to_string(),
                format!("{}", low.len()),
            ),
        ],
        alert: worst.visibility_ft < 0.5 * FT_PER_MI,
    })
}

fn storm_card(s: &ForecastSnapshot) -> Option<ConditionCard> {
    let cape = s
        .daily
        .iter()
        .take(2)
        .map(|d| d.cape_max_jkg)
        .fold(0.0_f64, f64::max);
    let (gust, gust_epoch) = s
        .hourly
        .iter()
        .take(48)
        .map(|h| (h.wind_gusts_mph, h.time_epoch))
        .max_by(|a, b| a.0.total_cmp(&b.0))
        .unwrap_or((0.0, 0));
    // Forecast pressure trend over the next 6 hours; a fast fall means an
    // approaching system. Anchored on the first hour that HAS pressure
    // (a non-OM owner's window can start before graft coverage), and both
    // ends of the 6h pair must be present.
    let p_start = s
        .hourly
        .iter()
        .position(|h| h.pressure_msl_hpa > 0.0)
        .unwrap_or(0);
    let p_now = s
        .hourly
        .get(p_start)
        .map(|h| h.pressure_msl_hpa)
        .unwrap_or(0.0);
    let p_6h = s
        .hourly
        .get(p_start + 6)
        .map(|h| h.pressure_msl_hpa)
        .unwrap_or(0.0);
    let p_drop = if p_now > 0.0 && p_6h > 0.0 {
        p_6h - p_now
    } else {
        0.0
    };
    let triggered = cape >= 1500.0 || gust >= 35.0 || p_drop <= -3.0;
    if !triggered {
        return None;
    }
    let mut rows = Vec::new();
    if cape >= 500.0 {
        let tier = if cape >= 2500.0 {
            "strongly unstable"
        } else if cape >= 1500.0 {
            "unstable"
        } else {
            "marginal"
        };
        rows.push((
            "Storm fuel (CAPE)".to_string(),
            format!("{cape:.0} J/kg, {tier}"),
        ));
    }
    if gust >= 25.0 {
        rows.push((
            "Peak gust (48h)".to_string(),
            format!("{gust:.0} mph at {}", format_hm(gust_epoch, &s.timezone)),
        ));
    }
    if p_drop.abs() >= 1.0 {
        rows.push((
            "Pressure (6h)".to_string(),
            format!("{}{:.1} hPa", if p_drop >= 0.0 { "+" } else { "" }, p_drop),
        ));
    }
    Some(ConditionCard {
        icon: "cloud-lightning",
        title: "Storm potential",
        rows,
        alert: cape >= 2500.0 || gust >= 50.0,
    })
}

fn heat_card(s: &ForecastSnapshot) -> Option<ConditionCard> {
    let wb = s
        .hourly
        .iter()
        .take(24)
        .map(|h| h.wet_bulb_f)
        .fold(0.0_f64, f64::max);
    let feels = s
        .daily
        .first()
        .map(|d| d.apparent_temp_max_f)
        .unwrap_or(0.0);
    if wb < 78.0 && feels < 102.0 {
        return None;
    }
    let mut rows = Vec::new();
    if feels > 0.0 {
        rows.push(("Feels like (peak)".to_string(), format!("{feels:.0}F")));
    }
    if wb > 0.0 {
        rows.push((
            "Wet bulb (peak)".to_string(),
            format!(
                "{wb:.0}F{}",
                if wb >= 80.0 {
                    ", evaporative cooling limit"
                } else {
                    ""
                }
            ),
        ));
    }
    Some(ConditionCard {
        icon: "thermometer",
        title: "Heat",
        rows,
        // Escalate on EITHER a dangerous wet bulb OR an extreme feels-like: a
        // dry-heat climate (very high apparent temp, low/absent wet bulb) is
        // still dangerous and would otherwise never leave the calm state.
        alert: wb >= 82.0 || feels >= 108.0,
    })
}

/// "Heads up" strip: zero to four condition cards, rendered only while
/// their condition holds. Absent conditions render NOTHING (no header).
#[component]
pub fn ConditionCards(snap: ReadSignal<ForecastSnapshot>) -> impl IntoView {
    move || {
        let s = snap.get();
        let cards: Vec<ConditionCard> =
            [winter_card(&s), fog_card(&s), storm_card(&s), heat_card(&s)]
                .into_iter()
                .flatten()
                .collect();
        if cards.is_empty() {
            return ().into_any();
        }
        view! {
            <section class="condition-cards" aria-label="Active weather conditions">
                <header class="forecast-section-head">
                    <h2 class="forecast-section-title">"Heads up"</h2>
                    <span class="forecast-section-meta">"conditions worth knowing about"</span>
                </header>
                <div class="condition-cards-row">
                    {cards
                        .into_iter()
                        .map(|c| {
                            view! {
                                <article class=if c.alert {
                                    "condition-card is-alert"
                                } else {
                                    "condition-card"
                                }>
                                    <header class="condition-card__head">
                                        <crate::components::ui::Icon name=c.icon size=18/>
                                        <span class="condition-card__title">{c.title}</span>
                                    </header>
                                    <dl class="condition-card__rows">
                                        {c.rows
                                            .into_iter()
                                            .map(|(k, v)| {
                                                view! {
                                                    <div class="kv">
                                                        <dt class="k">{k}</dt>
                                                        <dd class="v">{v}</dd>
                                                    </div>
                                                }
                                            })
                                            .collect::<Vec<_>>()}
                                    </dl>
                                </article>
                            }
                            .into_any()
                        })
                        .collect::<Vec<_>>()}
                </div>
            </section>
        }
        .into_any()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::snapshot::{DailyEntry, HourlyEntry};

    fn base() -> ForecastSnapshot {
        ForecastSnapshot {
            timezone: "America/New_York".into(),
            daily: vec![DailyEntry::default(), DailyEntry::default()],
            hourly: (0..48)
                .map(|i| HourlyEntry {
                    time_epoch: 1_700_000_000 + i * 3600,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn calm_default_snapshot_triggers_no_cards() {
        // All-zero extended fields = "provider doesn't report them"; every
        // trigger requires a positive signal, so nothing may render.
        let s = base();
        assert!(winter_card(&s).is_none());
        assert!(
            fog_card(&s).is_none(),
            "visibility 0 means unknown, not fog"
        );
        assert!(storm_card(&s).is_none());
        assert!(heat_card(&s).is_none());
    }

    #[test]
    fn winter_triggers_on_snowfall_or_depth() {
        let mut s = base();
        s.hourly[3].snowfall_in = 0.4;
        let c = winter_card(&s).expect("snowfall triggers");
        assert!(!c.alert);

        let mut s2 = base();
        s2.hourly[0].snow_depth_ft = 0.5;
        assert!(winter_card(&s2).is_some(), "snow on ground triggers");

        let mut s3 = base();
        for h in s3.hourly.iter_mut().take(10) {
            h.snowfall_in = 0.5;
        }
        assert!(winter_card(&s3).unwrap().alert, "3in+ escalates");
    }

    #[test]
    fn fog_triggers_only_on_reported_low_visibility() {
        let mut s = base();
        s.hourly[2].visibility_ft = 0.8 * FT_PER_MI;
        s.hourly[3].visibility_ft = 6.0 * FT_PER_MI;
        let c = fog_card(&s).expect("sub-2mi visibility triggers");
        assert!(!c.alert, "0.8mi is low but not alert tier");
        let mut s2 = base();
        s2.hourly[1].visibility_ft = 0.3 * FT_PER_MI;
        assert!(fog_card(&s2).unwrap().alert, "sub-half-mile escalates");
    }

    #[test]
    fn storm_triggers_on_cape_gusts_or_pressure_fall() {
        let mut s = base();
        s.daily[0].cape_max_jkg = 2000.0;
        assert!(storm_card(&s).is_some());

        let mut s2 = base();
        s2.hourly[5].wind_gusts_mph = 40.0;
        assert!(storm_card(&s2).is_some());

        let mut s3 = base();
        s3.hourly[0].pressure_msl_hpa = 1012.0;
        s3.hourly[6].pressure_msl_hpa = 1008.0;
        assert!(storm_card(&s3).is_some(), "4 hPa fall in 6h triggers");

        // A pressure RISE of the same magnitude must not.
        let mut s4 = base();
        s4.hourly[0].pressure_msl_hpa = 1008.0;
        s4.hourly[6].pressure_msl_hpa = 1012.0;
        assert!(storm_card(&s4).is_none());
    }

    #[test]
    fn heat_triggers_on_wet_bulb_or_feels_like() {
        let mut s = base();
        s.hourly[4].wet_bulb_f = 79.0;
        assert!(heat_card(&s).is_some());
        assert!(!heat_card(&s).unwrap().alert);

        let mut s2 = base();
        s2.hourly[4].wet_bulb_f = 83.0;
        assert!(heat_card(&s2).unwrap().alert, "82F+ wet bulb escalates");

        let mut s3 = base();
        s3.daily[0].apparent_temp_max_f = 104.0;
        assert!(heat_card(&s3).is_some());
    }
}
