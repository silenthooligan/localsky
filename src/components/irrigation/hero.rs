// Irrigation overview: a recorded normal morning beside tomorrow's forecast.
// Shared next-slot helpers below also serve the Weather page. Live trace
// details remain separate from the persisted outcome and forecast projection.

use crate::components::irrigation::advisor::AdvisorExplanation;
use crate::components::units_fmt::{deficit_value_mm, depth_unit, use_unit_prefs};
use crate::model::IrrigationSnapshot;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

/// "Now" as a UNIX epoch (seconds), the single clock the slot-tense logic reads.
/// Production always returns the real wall clock. Under test a thread-local
/// override (set via `test_support::with_frozen_now`) pins it, so
/// `today_run_passed` / `resolve_next_run` evaluate against a FIXED instant
/// instead of `Utc::now()`, removing the ~midnight / minute-boundary flake (FIX
/// 3) without changing any production behavior.
fn now_epoch_secs() -> i64 {
    #[cfg(test)]
    {
        if let Some(fixed) = test_support::frozen_now() {
            return fixed;
        }
    }
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod test_support {
    use std::cell::Cell;

    thread_local! {
        static FROZEN_NOW: Cell<Option<i64>> = const { Cell::new(None) };
    }

    /// The pinned test instant, if one is installed on this thread.
    pub(super) fn frozen_now() -> Option<i64> {
        FROZEN_NOW.with(|c| c.get())
    }

    /// Run `f` with `now_epoch_secs()` pinned to `epoch` for the current thread,
    /// restoring the previous value afterward (so nested/sequential pins are safe).
    pub(super) fn with_frozen_now<R>(epoch: i64, f: impl FnOnce() -> R) -> R {
        let prev = FROZEN_NOW.with(|c| c.replace(Some(epoch)));
        let out = f();
        FROZEN_NOW.with(|c| c.set(prev));
        out
    }
}

/// True once *today's* instance of the next run's clock-time has already passed,
/// i.e. this morning's window is behind us. The hero/explainer use it to pick
/// past vs present tense ("Skipped" vs "Skipping", "Watered" vs "Watering") so
/// the card never claims it is about to do something it has already done.
///
/// today's run-time = today's date at the next run's hour:minute; true once that
/// instant is in the past. Both clock times are read in the DEPLOYMENT timezone
/// (`s.timezone`, via `crate::timefmt::format_hm`), not the viewer's browser
/// zone, so a traveling viewer's tense matches the deployment's morning. Because
/// the next run is always today or later, comparing two same-zone zero-padded
/// "HH:MM" strings lexically is equivalent to comparing the wall clocks: the run
/// has passed today only if its clock-time is at or before the deployment's
/// current clock-time.
/// The three no-run states, told apart on screen. One blank
/// "NO RUNS SCHEDULED" used to cover a yard with no location, a polar
/// night and a fortnight the district refuses, and each needs a
/// different next step from the person reading it.
pub fn no_run_eyebrow(state: crate::model::NextRunState) -> &'static str {
    use crate::model::NextRunState::*;
    match state {
        NoLocation => "SETUP NEEDED",
        NoWaterPlanned => "NO WATERING PLANNED",
        NoLegalDay => "NOT A WATERING DAY",
        NoSunrise => "POLAR NIGHT",
        At => "NO RUNS SCHEDULED",
    }
}

pub fn no_run_headline(state: crate::model::NextRunState) -> &'static str {
    use crate::model::NextRunState::*;
    match state {
        NoLocation => "No location set",
        NoWaterPlanned => "No watering in the current outlook",
        NoLegalDay => "None in 14 days",
        NoSunrise => "No morning to aim at",
        At => "No run scheduled",
    }
}

/// The tag under the headline for a no-run state: the voice module's
/// sentence for it, so the hero, the home card and the week page say
/// the same thing. None for the legacy blank.
pub fn no_run_tag(state: crate::model::NextRunState) -> Option<&'static str> {
    use crate::model::NextRunState::*;
    match state {
        NoLocation => Some(crate::voice::next_run::NO_LOCATION),
        NoWaterPlanned => Some("The current water balance and forecast do not call for a watering run. Conditions are evaluated continuously."),
        NoLegalDay => Some(crate::voice::next_run::NO_LEGAL_DAY),
        NoSunrise => Some(crate::voice::next_run::NO_SUNRISE),
        At => None,
    }
}

pub fn today_run_passed(s: &IrrigationSnapshot) -> bool {
    // An instant compare. This used to compare "HH:MM" strings, which
    // ignores the date: a run planned for tomorrow at 05:30, looked at
    // today at 06:00, read as already passed.
    s.next_run_epoch > 0 && now_epoch_secs() >= s.next_run_epoch
}

/// What the next scheduled slot will actually DO, reconciled across the three
/// snapshot fields that describe the upcoming decision. The hero's headline reads
/// off this so it never claims "NEXT RUN" for a slot the engine is going to skip.
///
/// Honest model (the owner's complaint): a slot the engine predicts will SKIP is
/// NOT the "next run". We surface the truthful status, show the slot time only as
/// a re-evaluation, and, when a later day is predicted to water, point at it as
/// the next LIKELY run so a water-conscious user can plan.
#[derive(Debug, Clone, PartialEq)]
pub struct NextRunStatus {
    /// True when the next scheduled slot is predicted to skip (the engine is not
    /// going to water at `slot_epoch`). When false the slot is a real run.
    pub slot_skips: bool,
    /// The next scheduled slot's epoch (UTC). 0 when none is scheduled.
    pub slot_epoch: i64,
    /// Plain-language reason the slot skips (e.g. "recent rain"), empty when the
    /// slot runs. Derived from the verdict that describes THIS slot, not a generic
    /// guess.
    pub skip_reason_short: String,
    /// The slot verdict's structured reason code ("restrictions",
    /// "tomorrow_rain", ...), empty when the slot runs. Feeds the decision
    /// explainer so its "next run" lead is decided by the SAME source as this
    /// status (post-dispatch, today's trace can legitimately name a different
    /// rung than the next slot).
    pub skip_reason_code: String,
    /// The slot verdict's full engine-written reason sentence, empty when the
    /// slot runs. Fallback prose for the explainer when the code is unmapped.
    pub skip_reason_full: String,
    /// Epoch of the next FORWARD day predicted to water after a skipping slot, for
    /// "Next likely run: <day>". 0 when no upcoming day in the 7-day window runs.
    pub next_likely_run_epoch: i64,
    /// True when EVERY remaining day in the 7-day window (including the slot) is
    /// predicted to skip: there is no watering planned this week.
    pub all_week_skips: bool,
}

/// The one phase ladder the hero (eyebrow, glyph, headline, tag, theme)
/// and the home page's watering verdict all read. Priority order: an
/// offline refresher outranks everything; a running zone outranks a
/// pause; a pause outranks the next slot; then the next slot itself
/// (predicted to run or to skip); then an open-ended skip with nothing
/// scheduled; then nothing to time at all. Six copies of this ladder had
/// drifted on the running predicate; there is one now, and it counts a
/// zone the controller has not yet confirmed as running, the way the
/// WATERING NOW eyebrow always did.
#[derive(Debug, Clone, PartialEq)]
pub enum HeroPhase {
    Offline,
    Running,
    Paused,
    /// A slot is scheduled and the engine predicts it waters.
    SlotRuns(NextRunStatus),
    /// A slot is scheduled and the engine predicts it skips.
    SlotSkips(NextRunStatus),
    /// Skipping, with no slot scheduled.
    OpenSkip,
    /// Nothing scheduled and nothing skipping: a setup step, a polar
    /// night, a fortnight the district refuses.
    NoRun,
}

pub fn resolve_phase(s: &IrrigationSnapshot) -> HeroPhase {
    if !s.ha_reachable {
        HeroPhase::Offline
    } else if s.zones.iter().any(|z| z.is_running_or_unconfirmed()) {
        HeroPhase::Running
    } else if s.skip_check.will_skip && crate::model::is_pause_code(&s.skip_check.reason_code) {
        HeroPhase::Paused
    } else if s.next_run_epoch > 0 {
        let nr = resolve_next_run(s);
        if nr.slot_skips {
            HeroPhase::SlotSkips(nr)
        } else {
            HeroPhase::SlotRuns(nr)
        }
    } else if s.skip_check.will_skip {
        HeroPhase::OpenSkip
    } else {
        HeroPhase::NoRun
    }
}

/// Reconcile `next_run_epoch`, `skip_check`, and `seven_day_verdicts` into a
/// single honest answer for "what does the next slot actually do, and when can I
/// next expect water". The three fields describe the SAME upcoming decision only
/// when today's window is still ahead; once this morning's window has passed,
/// `next_run_epoch` advances to tomorrow while `skip_check` still describes the
/// (completed) morning, so the verdict that governs the SLOT is the 7-day cell
/// whose calendar date matches `next_run_epoch`, NOT `skip_check`. We match by
/// deployment-tz calendar date so the slot's time and its verdict always agree.
///
/// Precedence for the slot verdict:
///   1. The `DayVerdict` whose calendar date equals `next_run_epoch`'s date.
///   2. If no day matches (forecast strip short / absent) AND the slot is today's
///      still-pending window, fall back to `skip_check` (they describe the same
///      run in that case).
///   3. Otherwise treat the slot as a run (we have no evidence it skips, and
///      claiming a skip we can't substantiate would be its own dishonesty).
pub fn resolve_next_run(s: &IrrigationSnapshot) -> NextRunStatus {
    let tz = s.timezone.as_str();
    let slot_epoch = s.next_run_epoch;
    if slot_epoch <= 0 {
        return NextRunStatus {
            slot_skips: false,
            slot_epoch: 0,
            skip_reason_short: String::new(),
            skip_reason_code: String::new(),
            skip_reason_full: String::new(),
            next_likely_run_epoch: 0,
            all_week_skips: false,
        };
    }

    // The strip cell for the slot, by the day offset the refresher
    // computed from the same calendar the engine used. It used to be
    // found by matching rendered "Jun 28" strings, which depends on the
    // renderer agreeing with itself across a midnight.
    let slot_day = s
        .next_run_day_offset
        .and_then(|off| s.seven_day_verdicts.iter().find(|d| d.day_offset == off));
    let _ = tz;

    // Is the slot TODAY's still-pending window? For that one slot the LIVE
    // skip_check is authoritative, NOT the projected day-0 strip cell, which
    // is a synthetic projection that zeroes the live inputs.
    let slot_is_today_pending = s.next_run_day_offset == Some(0) && !today_run_passed(s);

    let (slot_skips, slot_reason, slot_reason_code) = if slot_is_today_pending {
        (
            s.skip_check.will_skip,
            s.skip_check.reason.clone(),
            s.skip_check.reason_code.clone(),
        )
    } else {
        match slot_day {
            Some(d) => (d.verdict == "skip", d.reason.clone(), d.reason_code.clone()),
            None if !today_run_passed(s) => (
                s.skip_check.will_skip,
                s.skip_check.reason.clone(),
                s.skip_check.reason_code.clone(),
            ),
            None => (false, String::new(), String::new()),
        }
    };

    // The next forward day (strictly after the slot's date) predicted to water,
    // and whether anything in the window runs at all.
    let mut next_likely_run_epoch = 0i64;
    let mut any_forward_run = false;
    for d in &s.seven_day_verdicts {
        // Only days at or after the slot are "upcoming"; the slot's own date is
        // handled by slot_skips, so a forward run must be strictly later.
        if d.time_epoch <= slot_epoch {
            continue;
        }
        if d.verdict != "skip" {
            any_forward_run = true;
            if next_likely_run_epoch == 0 {
                next_likely_run_epoch = d.time_epoch;
            }
        }
    }

    // All-week-skips only makes sense as a claim when the slot itself skips and no
    // forward day runs. If the slot runs, the week obviously has a run.
    let all_week_skips = slot_skips && !any_forward_run;

    NextRunStatus {
        slot_skips,
        slot_epoch,
        skip_reason_short: if slot_skips {
            plain_skip_phrase(&slot_reason_code, &slot_reason)
        } else {
            String::new()
        },
        skip_reason_code: if slot_skips {
            slot_reason_code
        } else {
            String::new()
        },
        skip_reason_full: if slot_skips {
            slot_reason
        } else {
            String::new()
        },
        next_likely_run_epoch,
        all_week_skips,
    }
}

/// Condense a skip reason to the shared short noun phrase (gates_catalog
/// owns the vocabulary, so the hero, the Week tab and the explainer agree).
fn plain_skip_phrase(reason_code: &str, reason: &str) -> String {
    crate::gates_catalog::skip_phrase(reason_code, reason).to_string()
}

/// The HONEST hero tag for a skipping next slot. Structure: WHY it is skipping,
/// then WHEN it re-checks, then WHAT is next (the next likely run, or that
/// nothing is planned this week). The old "(based on current conditions)"
/// parenthetical is dropped: it was redundant with "Re-checks HH:MM" (both mean
/// "provisional"), and for a schedule reason (watering restrictions) or a
/// forecast reason (rain tomorrow) it was actively wrong, since neither is a
/// "current condition". "Re-checks HH:MM" alone carries the provisional meaning.
/// Pure so it is unit-testable. Public so the Weather-home watering strip
/// (app.rs HomeWateringVerdict) renders the SAME honest skip copy as the hero
/// instead of a second, divergent implementation.
pub fn skip_tag_string(nr: &NextRunStatus, tz: &str) -> String {
    skip_tag_string_with_rules(nr, tz, None)
}

/// A restricted slot is a calendar fact, not a condition to re-check:
/// "Re-checks 05:42" on it promised a check that cannot change the
/// answer. It names the allowed days and the next run instead.
pub fn skip_tag_string_with_rules(
    nr: &NextRunStatus,
    tz: &str,
    allowed_days: Option<&str>,
) -> String {
    if nr.skip_reason_code == "restrictions" {
        let rule = match allowed_days {
            Some(days) => format!("Your watering rules allow {days}."),
            None => "Your watering rules do not allow this day.".to_string(),
        };
        let next = if nr.next_likely_run_epoch > 0 {
            format!(
                " Next run {} {}.",
                crate::timefmt::format_wday_full(nr.next_likely_run_epoch, tz),
                crate::timefmt::format_hm(nr.next_likely_run_epoch, tz)
            )
        } else {
            String::new()
        };
        return format!("{rule}{next}");
    }
    let reason = if nr.skip_reason_short.is_empty() {
        "current conditions".to_string()
    } else {
        nr.skip_reason_short.clone()
    };
    let recheck = format!("Re-checks {}", crate::timefmt::format_hm(nr.slot_epoch, tz));
    let plan = if nr.all_week_skips {
        crate::voice::idle::WEEK_OF_RAIN.to_string()
    } else if nr.next_likely_run_epoch > 0 {
        format!(
            "next likely run {}",
            format_relative_day(nr.next_likely_run_epoch, tz)
        )
    } else {
        String::new()
    };
    if plan.is_empty() {
        format!("Skipping: {reason} \u{b7} {recheck}")
    } else {
        format!("Skipping: {reason} \u{b7} {recheck} \u{b7} {plan}")
    }
}

#[component]
pub fn NextRunHero(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! { <super::overview::IrrigationOverview snap/> }
}

#[component]
pub fn WateringDecisions(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <div class="watering-decisions">
                <super::plan::WateringPlan snap/>
                {move || snap.get().force_overrode_guard.map(|guard| view! {
                    <p class="hero-forced-warn" role="status">{format!("Force bypasses {guard}. Safety checks, watering restrictions, and active holds still apply.")}</p>
                })}
                <section class="decision-live">
                    <header class="decision-section-heading"><h2>"Current decision and zone needs"</h2><span class="decision-live__badge">"Live"</span><p>"Conditions now · today’s recorded outcome is above."</p></header>
                    <CurrentZoneNeeds snap/>
                    {view! { <DecisionExplainer snap/> }.into_any()}
                </section>
        </div>
    }
}

#[component]
fn CurrentZoneNeeds(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    view! {
        <div class="decision-zone-list">
            <For each=move || snap.get().zones key=|z| z.slug.clone() children=move |initial| {
                let zone = Memo::new(move |_| snap.get().zones.into_iter().find(|z| z.slug == initial.slug).unwrap_or_else(|| initial.clone()));
                let status = Memo::new(move |_| {
                    let z = zone.get();
                    let s = snap.get();
                    if !z.running_known || z.ledger_running && !z.running { return ("off", "alert-triangle", "Checking valves", "Waiting for controller confirmation".to_string()); }
                    if z.running { return ("run", "sprinkler", "Watering now", "Controller reports watering".into()); }
                    let verdict = z.verdict.as_ref().or_else(|| s.zone_verdicts.iter().find(|v| v.zone_slug == z.slug));
                    let Some(verdict) = verdict else { return ("off", "info", "Awaiting decision", "No zone decision available".into()); };
                    if s.zone_waters_next_run(&z) { return ("run", "sprinkler", "Watering planned", "Water balance calls for a run".into()); }
                    let (code, reason) = if verdict.verdict == "skip" { (verdict.reason_code.as_str(), verdict.reason.as_str()) }
                        else { ("water_balance", s.water_budgets.iter().find(|b| b.zone_slug == z.slug).map(|b| b.today_reason.as_str()).unwrap_or("Water need unavailable")) };
                    if crate::gates_catalog::GateFamily::of(code, reason) == crate::gates_catalog::GateFamily::NoData {
                        return ("off", "alert-triangle", "Awaiting data", super::overview::short_reasons(std::iter::once((code, reason))));
                    }
                    ("skip", "sprinkler-off", "Not watering", super::overview::short_reasons(std::iter::once((code, reason))))
                });
                view! {
                    <article class="decision-zone" data-watering=move || status.get().0>
                        <div><h3>{move || zone.get().name}</h3><p>{move || status.get().3}</p></div>
                        <span class="watering-state">{move || view! { <crate::components::ui::Icon name=status.get().1 size=22/> }}{move || status.get().2}</span>
                        <a href=move || crate::base::url(&format!("/zones/{}", zone.get().slug)) aria-label=move || format!("View {} zone details", zone.get().name)>"Zone details →"</a>
                    </article>
                }
            }/>
        </div>
    }
}

/// A one-tap, deterministic plain-English "why" for the morning decision,
/// rendered from the decision trace with no LLM. Collapsed by default; expanding
/// shows the verdict in plain language, the deciding factor, the key checks that
/// passed, and a lower-confidence note when the inputs were degraded.
#[component]
fn DecisionExplainer(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    // Gate the whole expander on trace presence via a coarse boolean that flips
    // at most once (None -> Some after the first refresh, then stays). The outer
    // closure reads ONLY this boolean, so it does not re-run on every snapshot
    // tick. That keeps the nested Advisor mounted exactly ONCE: it owns a 60s
    // fetch loop with an on_cleanup abort, so remounting it each SSE tick would
    // churn fetches. The live explanation text is a separate inner closure that
    // re-renders reactively; the Advisor sits beside it as a stable child.
    let has_trace = move || snap.get().decision_trace.is_some();

    // Reactive explanation content (re-renders on each snapshot). Pulled out so
    // it can live inside the stable <details> shell.
    let explanation = move || {
        let s = snap.get();
        let Some(trace) = s.decision_trace.clone() else {
            return ().into_any();
        };
        // This panel explains live evidence only. Stored dispatch records own
        // completed outcomes; a trace captured by a refresh cannot prove one.
        let e = crate::explain::explain_decision_with_zones(&trace, false, &[], None);
        let reason = crate::reason_render::render_trace_reason(&trace, prefs.get());
        let checks = if e.considered.is_empty() {
            ().into_any()
        } else {
            view! {
                <ul class="decision-explainer__checks">
                    {e.considered
                        .into_iter()
                        .map(|c| view! { <li>{c}</li> })
                        .collect_view()}
                </ul>
            }
            .into_any()
        };
        let degraded = e.degraded.then(|| {
            view! {
                <p class="decision-explainer__degraded">
                    "Decided on backup data, so this is lower-confidence until live data returns."
                </p>
            }
        });
        view! {
            <p class="decision-explainer__why">
                <strong>"Current checks: "</strong>
                {reason}
            </p>
            {checks}
            {degraded}
        }
        .into_any()
    };

    // Stable shell: the <details> and the nested Advisor mount ONCE. When there
    // is no trace yet, the whole expander is hidden via a reactive display style
    // (an attribute toggle, not a remount), preserving the original "render
    // nothing until a decision exists" behaviour without churning the advisor.
    view! {
        <details
            class="decision-explainer"
            style=move || if has_trace() { "" } else { "display:none" }
        >
            <summary class="decision-explainer__summary"><span>"System checks & calculations"</span><crate::components::ui::Icon name="chevron-down" size=20/></summary>
            <div class="decision-explainer__body">
                {explanation}
                <HeroStats snap/>
                <SkipBreakdown snap/>
                // LLM Advisor nested UNDER the deterministic why: the rule-based
                // explanation above is the primary, always-correct answer; the
                // advisor is an optional plain-language gloss. It omits its own
                // tile entirely when offline/disabled, so a missing advisor never
                // reads as a broken decision. Mounted once (stable shell) so its
                // 60s fetch loop is not restarted on every snapshot tick.
                <AdvisorExplanation
                    verdict=Signal::derive(move || snap.get().skip_check.verdict.clone())
                />
            </div>
        </details>
    }
}

/// The four at-a-glance numbers that used to live in a separate KPI strip
/// above the page, now folded into the hero: tonight's planned minutes, how
/// many zones are due, the controller water level, and the average soil
/// deficit. Reads straight off the streamed snapshot.
#[component]
fn HeroStats(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    move || {
        let s = snap.get();
        let p = prefs.get();
        // The shared skip-aware predicate (snapshot::zone_waters_next_run)
        // keeps the minutes and the zone count in lockstep (T10) with the
        // Zones-page KPI strip and the cards: a zone the engine will skip
        // must NOT contribute, and a zone holding at zero planned seconds
        // (the normal soil-model state most mornings) is not "watering".
        let waters_tonight = |z: &crate::model::ZoneState| s.zone_waters_next_run(z);
        // Tonight's minutes EXCLUDING skip zones, so it agrees with the
        // Zones-watering count beside it: a 4-zone schedule where 3 are
        // soil-saturated shows only the one running zone's minutes, not the
        // server-side total that still folds in the skipped zones'. Sum the live
        // per-zone planned seconds for the zones that will actually water; fall
        // back to the server total ONLY before any zone has a verdict (so a
        // pre-decision frame still shows a sensible number, not 0).
        let any_decided = s
            .zones
            .iter()
            .any(|z| z.verdict.is_some() || s.zone_verdicts.iter().any(|v| v.zone_slug == z.slug));
        let tonight = if any_decided {
            let secs: u32 = s
                .zones
                .iter()
                .filter(|z| waters_tonight(z))
                .map(|z| z.planned_run_seconds)
                .sum();
            // Round to nearest minute (mirrors the card's (sec + 30)/60).
            format!("{}", (secs + 30) / 60)
        } else {
            format!("{:.0}", s.next_run_total_minutes)
        };
        // The count of zones the NEXT run will actually water (verdict != "skip"),
        // not how many are scheduled: a 4-zone schedule where 3 are soil-saturated
        // waters ONE, and the stat must read "1", matching the per-zone summary.
        let watering = s
            .zones
            .iter()
            .filter(|z| waters_tonight(z))
            .count()
            .to_string();
        // Water level is None when the controller does not report one
        // (every adapter except OpenSprinkler-class hardware). A dash in
        // a tile is a question nobody can answer, so the tile turns into
        // the days since the last real rain, which every install knows
        // once it has a forecast; before that it stays out.
        let (water, water_unit, water_label) = water_tile(&s);
        // Soil deficit is the mean of the zones that HAVE one, in
        // MILLIMETERS. The soil model's evidence replay fills it for every
        // zone with agronomy config, so this tile carries a live figure on
        // those installs; an all-absent set (env-var zones, no agronomy)
        // still renders a dash rather than averaging nothing into a
        // confident 0.00 (the guard used to be "are there any zones",
        // which on a seven-zone yard printed a fabricated zero).
        let measured: Vec<f64> = s.zones.iter().filter_map(|z| z.bucket_mm).collect();
        let deficit_empty = measured.is_empty();
        let deficit = if deficit_empty {
            "-".to_string()
        } else {
            let avg_mm = measured.iter().sum::<f64>() / measured.len() as f64;
            // Same display rule as the zone-card Deficit tiles: the label
            // carries the direction, the value is the magnitude, and the
            // sign column is reserved for a true surplus.
            deficit_value_mm(avg_mm, p)
        };
        // No unit glyph for the placeholder so "-" reads clean.
        let deficit_unit = if deficit_empty { "" } else { depth_unit(p) };
        view! {
            <div class="ir-hero-stats">
                <crate::components::ui::StatTile layout="hero" label="Next run" value=tonight unit="min"/>
                <crate::components::ui::StatTile layout="hero" label="Zones watering" value=watering/>
                {water_label.map(|label| view! {
                    <crate::components::ui::StatTile layout="hero" label=label value=water unit=water_unit/>
                })}
                <crate::components::ui::StatTile layout="hero" label="Soil deficit" value=deficit unit=deficit_unit/>
            </div>
        }
    }
}

/// The third hero tile: the controller's water level when it reports
/// one, otherwise the days since significant rain, otherwise nothing
/// (a `None` label hides the tile).
pub fn water_tile(s: &IrrigationSnapshot) -> (String, &'static str, Option<&'static str>) {
    if let Some(v) = s.water_level_pct {
        return (format!("{v:.0}"), "%", Some("Water level"));
    }
    // The figure is model-derived; with no forecast rows it is a
    // placeholder, so the tile waits for the seven-day strip.
    if s.last_refresh_epoch == 0 || s.seven_day_verdicts.is_empty() {
        return (String::new(), "", None);
    }
    (
        s.skip_check.days_since_significant_rain.to_string(),
        "d",
        Some("Since rain"),
    )
}

/// One tile of the breakdown: a rule the engine evaluated, rendered from
/// the decision trace. The hero used to re-derive the ladder here with
/// schema defaults typed in by hand (0.05, 0.10, 95 F, 1.5 x), so a
/// yard whose already-wet threshold was raised to 0.10 saw a tile that
/// tripped at 0.05 while the engine, correctly, ran; and a rule the
/// operator disabled still drew a tile. The tile is a projection of the
/// RuleEval now, and the numbers are the engine's, in the viewer's units.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakdownRow {
    pub id: String,
    pub label: String,
    /// "value vs threshold", unit-aware.
    pub detail: String,
    /// The distance to the line, when the gate has one.
    pub margin: Option<String>,
    /// The rule fired (or met its line and was overridden).
    pub tripped: bool,
    /// An earlier rule decided before this one was reached.
    pub not_reached: bool,
}

/// The rows the breakdown shows for a trace: fired, passed and
/// not-reached rules, in ladder order. A rule the operator disabled, or
/// one that had nothing to judge, draws no tile: the breakdown shows
/// what was checked, and those were not.
pub fn breakdown_rows(
    trace: &crate::model::DecisionTrace,
    p: crate::components::units_fmt::UnitPrefs,
) -> Vec<BreakdownRow> {
    trace
        .rules
        .iter()
        .filter(|r| matches!(r.outcome.as_str(), "fired" | "passed" | "not_reached"))
        .map(|r| BreakdownRow {
            id: r.id.clone(),
            label: r.label.clone(),
            detail: if r.outcome == "not_reached" {
                "not checked; an earlier rule decided".to_string()
            } else {
                crate::reason_render::render_rule_detail(r, p)
            },
            margin: if r.outcome == "not_reached" {
                None
            } else {
                crate::reason_render::render_rule_margin(r, p)
            },
            tripped: r.outcome == "fired" || r.over_line,
            not_reached: r.outcome == "not_reached",
        })
        .collect()
}

#[component]
fn SkipBreakdown(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    let rows = Signal::derive(move || {
        snap.get()
            .decision_trace
            .as_ref()
            .map(|t| breakdown_rows(t, prefs.get()))
            .unwrap_or_default()
    });
    view! {
        <div class="skip-breakdown" role="list">
            {move || {
                let rows = rows.get();
                if rows.is_empty() {
                    return view! {
                        <p class="sk-empty">"No decision yet. The first check lands within a minute of the forecast."</p>
                    }
                    .into_any();
                }
                rows.into_iter()
                    .map(|r| view! { <SkipRow row=r/> })
                    .collect_view()
                    .into_any()
            }}
        </div>
    }
}

#[component]
fn SkipRow(row: BreakdownRow) -> impl IntoView {
    let tripped = row.tripped;
    let muted = row.not_reached;
    view! {
        <div class="sk-row" class:sk-row-tripped=tripped class:sk-row-muted=muted role="listitem">
            <span class="sk-mark" aria-hidden="true">
                {
                    let name = if tripped { "x" } else if muted { "minus" } else { "check" };
                    view! { <crate::components::ui::Icon name=name size=13 stroke=2.5/> }
                }
            </span>
            <span class="sk-label">{row.label}</span>
            <span class="sk-value">{row.detail}</span>
            <span class="sk-threshold">{row.margin.unwrap_or_default()}</span>
        </div>
    }
}

/// Label a future forecast date in the deployment timezone.
fn format_relative_day(epoch: i64, tz: &str) -> String {
    use crate::timefmt::{format_md, format_wday_full};
    if epoch <= 0 {
        return "-".to_string();
    }
    let now = chrono::Utc::now().timestamp();
    let target_md = format_md(epoch, tz);
    let today_md = format_md(now, tz);
    let tomorrow_md = format_md(now + 86_400, tz);
    if !target_md.is_empty() && target_md == today_md {
        "today".to_string()
    } else if !target_md.is_empty() && target_md == tomorrow_md {
        "tomorrow".to_string()
    } else if epoch - now < 7 * 86_400 {
        // FULL day name ("Sunday"): "Sun" in run prose reads as sunshine.
        format_wday_full(epoch, tz)
    } else {
        target_md
    }
}

#[cfg(test)]
mod next_run_state_tests {
    use super::*;
    use crate::model::NextRunState;

    /// Each no-run state renders its own eyebrow, headline and the voice
    /// module's tag, so the three are told apart on screen.
    #[test]
    fn no_location_reads_as_setup() {
        assert_eq!(no_run_eyebrow(NextRunState::NoLocation), "SETUP NEEDED");
        assert_eq!(no_run_headline(NextRunState::NoLocation), "No location set");
        assert_eq!(
            no_run_tag(NextRunState::NoLocation),
            Some(crate::voice::next_run::NO_LOCATION)
        );
    }

    #[test]
    fn no_legal_day_reads_as_a_rule() {
        assert_eq!(
            no_run_eyebrow(NextRunState::NoLegalDay),
            "NOT A WATERING DAY"
        );
        assert_eq!(no_run_headline(NextRunState::NoLegalDay), "None in 14 days");
        assert_eq!(
            no_run_tag(NextRunState::NoLegalDay),
            Some(crate::voice::next_run::NO_LEGAL_DAY)
        );
    }

    #[test]
    fn no_sunrise_reads_as_the_season() {
        assert_eq!(no_run_eyebrow(NextRunState::NoSunrise), "POLAR NIGHT");
        assert_eq!(
            no_run_headline(NextRunState::NoSunrise),
            "No morning to aim at"
        );
        assert_eq!(
            no_run_tag(NextRunState::NoSunrise),
            Some(crate::voice::next_run::NO_SUNRISE)
        );
    }

    /// A run planned for tomorrow at 05:30, looked at today at 06:00,
    /// is pending. The old clock-string compare said it had passed.
    #[test]
    fn tomorrows_run_is_pending_this_evening() {
        let mut s = IrrigationSnapshot::default();
        s.next_run_epoch = now_epoch_secs() + 23 * 3600 + 1800;
        s.next_run_day_offset = Some(1);
        assert!(!today_run_passed(&s));
        let nr = resolve_next_run(&s);
        assert!(!nr.slot_skips);
        // And a run whose instant is behind us has passed, whatever the clock reads.
        s.next_run_epoch = now_epoch_secs() - 60;
        s.next_run_day_offset = Some(0);
        assert!(today_run_passed(&s));
    }

    /// Every voice constant for next-run and idle states is referenced
    /// by a component, so none of them is prose nobody can see.
    #[test]
    fn every_next_run_voice_constant_is_rendered_somewhere() {
        let voice =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/voice.rs")).unwrap();
        let mut consts = Vec::new();
        for module in ["pub mod next_run", "pub mod idle"] {
            let start = voice.find(module).expect(module);
            let end = voice[start..].find("\n}\n").unwrap() + start;
            for line in voice[start..end].lines() {
                if let Some(rest) = line.trim().strip_prefix("pub const ") {
                    consts.push((
                        module.trim_start_matches("pub mod ").to_string(),
                        rest.split(':').next().unwrap().trim().to_string(),
                    ));
                }
            }
        }
        assert!(!consts.is_empty());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/components");
        let mut sources = String::new();
        for path in crate::engine::clock::rust_sources(&root) {
            sources.push_str(&std::fs::read_to_string(path).unwrap());
        }
        for (module, name) in consts {
            let needle = format!("voice::{module}::{name}");
            assert!(
                sources.contains(&needle),
                "{needle} is referenced by no component"
            );
        }
    }
}

#[cfg(test)]
mod restricted_slot_tests {
    use super::*;

    /// A restricted slot reads as a rule: the tag names the allowed days
    /// and the next run, and never promises a re-check the calendar
    /// cannot change.
    #[test]
    fn a_restricted_slot_names_the_rule_not_a_recheck() {
        let nr = NextRunStatus {
            slot_skips: true,
            slot_epoch: 1_788_600_000,
            skip_reason_short: "watering restrictions".into(),
            skip_reason_code: "restrictions".into(),
            skip_reason_full: "Watering restriction (HOA): today is not an allowed watering day"
                .into(),
            next_likely_run_epoch: 1_788_600_000 + 2 * 86_400,
            all_week_skips: false,
        };
        let tag = skip_tag_string_with_rules(&nr, "UTC", Some("Thu and Sun"));
        assert!(!tag.contains("Re-checks"), "{tag}");
        assert!(
            tag.contains("Your watering rules allow Thu and Sun."),
            "{tag}"
        );
        assert!(tag.contains("Next run"), "{tag}");
        // A weather skip keeps its re-check.
        let mut weather = nr.clone();
        weather.skip_reason_code = "wind_forecast".into();
        weather.skip_reason_short = "high wind".into();
        assert!(skip_tag_string_with_rules(&weather, "UTC", None).contains("Re-checks"));
    }

    #[test]
    fn allowed_days_read_as_words() {
        let mut s = IrrigationSnapshot::default();
        assert_eq!(s.allowed_days_phrase(), None);
        s.restriction_allowed_days = Some(vec![4, 0]);
        assert_eq!(s.allowed_days_phrase().as_deref(), Some("Sun and Thu"));
        s.restriction_allowed_days = Some(vec![1, 3, 5]);
        assert_eq!(s.allowed_days_phrase().as_deref(), Some("Mon, Wed and Fri"));
        s.restriction_allowed_days = Some((0..7).collect());
        assert_eq!(
            s.allowed_days_phrase(),
            None,
            "every day allowed is no rule to name"
        );
    }
}

#[cfg(test)]
mod breakdown_tests {
    use super::*;
    use crate::components::units_fmt::UnitPrefs;
    use crate::config::schema::SkipRuleParams;
    use crate::engine::skip_rules::{decide_traced, Inputs};

    fn base() -> Inputs {
        Inputs {
            calendar: crate::engine::calendar::Calendar::utc(),
            rain_intensity_now_in_hr: Some(0.0),
            rain_today_forecast_in: Some(0.0),
            rain_next_4h_in: Some(0.0),
            forecast_in: Some(0.0),
            rain_3day_weighted_in: Some(0.0),
            temp_now_f: 70.0,
            wind_now_mph: 3.0,
            humidity_now_pct: 55.0,
            max_wind_mph: 10.0,
            min_temp_f: 38.0,
            rain_skip_in: 0.25,
            temp_min_24h_f: Some(60.0),
            temp_max_3day_f: 80.0,
            live_readings: Default::default(),
            ..Default::default()
        }
    }

    /// Every tile is its RuleEval: same id, the renderer's detail, the
    /// renderer's margin, tripped exactly when the engine says fired.
    #[test]
    fn each_tile_equals_its_rule_eval() {
        let trace = decide_traced(&base(), &SkipRuleParams::default());
        let rows = breakdown_rows(&trace, UnitPrefs::default());
        let shown: Vec<&crate::model::RuleEval> = trace
            .rules
            .iter()
            .filter(|r| matches!(r.outcome.as_str(), "fired" | "passed" | "not_reached"))
            .collect();
        assert_eq!(rows.len(), shown.len());
        for (row, rule) in rows.iter().zip(shown) {
            assert_eq!(row.id, rule.id);
            assert_eq!(row.label, rule.label);
            assert_eq!(
                row.tripped,
                rule.outcome == "fired" || rule.over_line,
                "{}",
                rule.id
            );
            if rule.outcome != "not_reached" {
                assert_eq!(
                    row.detail,
                    crate::reason_render::render_rule_detail(rule, UnitPrefs::default()),
                    "{}",
                    rule.id
                );
                assert_eq!(
                    row.margin,
                    crate::reason_render::render_rule_margin(rule, UnitPrefs::default())
                );
            }
        }
    }

    /// A raised already-wet threshold: 0.07 in of rain against 0.10 is
    /// passed on the tile, as it is in the engine. The old tile compared
    /// against a typed-in 0.05 and tripped.
    #[test]
    fn a_raised_threshold_renders_passed() {
        let mut i = base();
        i.rain_today_in = 0.07;
        let mut p = SkipRuleParams::default();
        p.already_wet_in = 0.10;
        let trace = decide_traced(&i, &p);
        let rows = breakdown_rows(&trace, UnitPrefs::default());
        let wet = rows
            .iter()
            .find(|r| r.id == "already_wet")
            .expect("an already_wet tile");
        assert!(!wet.tripped, "{wet:?}");
        assert!(
            wet.detail.contains("0.07") && wet.detail.contains("0.10"),
            "{wet:?}"
        );
    }

    /// A rule the operator disabled draws no tile at all.
    #[test]
    fn a_disabled_rule_renders_neither() {
        let mut i = base();
        i.temp_now_f = 30.0;
        let mut p = SkipRuleParams::default();
        p.disabled_rules = vec!["freeze_now".into()];
        let trace = decide_traced(&i, &p);
        let rows = breakdown_rows(&trace, UnitPrefs::default());
        assert!(rows.iter().all(|r| r.id != "freeze_now"), "{rows:?}");
    }

    /// No component re-derives a skip rule by comparing a skip_check
    /// input against a typed-in number. The tiles read the trace and the
    /// forms read the defaults by name; a literal beside `skip_check.` in
    /// a comparison is the old hero, and it is banned from the components
    /// tree.
    #[test]
    fn no_component_compares_a_skip_check_input_to_a_literal() {
        fn offends(line: &str) -> bool {
            // `skip_check.<field> <op> <digit>`: an input compared to a number.
            let mut from = 0;
            while let Some(i) = line[from..].find("skip_check.") {
                let after = &line[from + i + "skip_check.".len()..];
                let field_len = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                let rest = after[field_len..].trim_start();
                let op_len = if rest.starts_with(">=") || rest.starts_with("<=") {
                    2
                } else if rest.starts_with('>') || rest.starts_with('<') {
                    1
                } else {
                    0
                };
                if op_len > 0
                    && rest[op_len..]
                        .trim_start()
                        .starts_with(|c: char| c.is_ascii_digit())
                {
                    return true;
                }
                from += i + 1;
            }
            // `1.5 * s.rain_skip_in`: a factor typed in front of an input.
            line.contains("* s.")
                && line
                    .split("* s.")
                    .next()
                    .is_some_and(|before| before.trim_end().ends_with(|c: char| c.is_ascii_digit()))
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/components");
        let mut hits = Vec::new();
        for path in crate::engine::clock::rust_sources(&root) {
            let src = std::fs::read_to_string(&path).unwrap();
            for (n, line) in crate::engine::clock::code_only(&src).lines().enumerate() {
                if offends(line) {
                    hits.push(format!("{}:{}: {}", path.display(), n + 1, line.trim()));
                }
            }
        }
        assert!(
            hits.is_empty(),
            "components compare skip_check inputs to typed-in numbers:
{}",
            hits.join(
                "
"
            )
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DayVerdict, IrrigationSnapshot, SkipCheck};

    const TZ: &str = "America/New_York";
    // 00:00 America/New_York on 2026-06-25 (a fixed, deterministic anchor).
    const DAY0_MIDNIGHT: i64 = 1_782_360_000; // 2026-06-25T04:00:00Z = 00:00 EDT
    const DAY_S: i64 = 86_400;

    fn dv(offset: u32, midnight: i64, verdict: &str, reason: &str, code: &str) -> DayVerdict {
        DayVerdict {
            day_offset: offset,
            time_epoch: midnight,
            verdict: verdict.into(),
            reason: reason.into(),
            reason_code: code.into(),
            ..Default::default()
        }
    }

    /// A snapshot whose next slot is the pre-dawn window on day `slot_day_idx`,
    /// with a matching 7-day cell so `resolve_next_run` is deterministic (the
    /// calendar-date match never falls back to the now-dependent `today_run_passed`
    /// path). `verdicts` is (verdict, reason, reason_code) per day, day 0..N.
    fn snap_with(slot_day_idx: usize, verdicts: &[(&str, &str, &str)]) -> IrrigationSnapshot {
        let mut s = IrrigationSnapshot::default();
        s.ha_reachable = true;
        s.timezone = TZ.into();
        // Slot = 03:25 local on the chosen day (well after that day's midnight, so
        // format_md(slot) == format_md(that day's cell)).
        s.next_run_epoch = DAY0_MIDNIGHT + slot_day_idx as i64 * DAY_S + 3 * 3600 + 25 * 60;
        // The refresher computes the day offset from the same calendar the
        // engine used; the fixture states it outright.
        s.next_run_day_offset = Some(slot_day_idx as u32);
        s.next_run_total_minutes = 75.0;
        s.seven_day_verdicts = verdicts
            .iter()
            .enumerate()
            .map(|(i, (v, r, c))| dv(i as u32, DAY0_MIDNIGHT + i as i64 * DAY_S, v, r, c))
            .collect();
        s
    }

    #[test]
    fn next_slot_skips_resolves_as_skip_not_run() {
        // The upcoming slot (day 0) is predicted to skip for recent rain; a later
        // day (day 2) runs.
        let s = snap_with(
            0,
            &[
                ("skip", "Already wet (0.30\" today)", "already_wet"),
                ("skip", "Already wet (0.20\" today)", "already_wet"),
                ("run", "", "run"),
            ],
        );
        let nr = resolve_next_run(&s);
        assert!(nr.slot_skips, "the next scheduled slot must read as a SKIP");
        assert_eq!(nr.skip_reason_short, "recent rain");
        assert!(
            !nr.all_week_skips,
            "a later day runs, so the week is not all-skip"
        );
        assert!(
            nr.next_likely_run_epoch > nr.slot_epoch,
            "next likely run is the later running day"
        );

        // The rendered hero tag must lead with the truthful status and present the
        // slot time as a RE-CHECK, never as a promised "next run at <time>".
        let tag = skip_tag_string(&nr, TZ);
        assert!(tag.starts_with("Skipping: recent rain"), "tag={tag:?}");
        assert!(
            tag.contains("Re-checks 03:25"),
            "tag must show the slot time as a re-check, tag={tag:?}"
        );
        assert!(
            tag.contains("next likely run"),
            "tag must point at the next likely run, tag={tag:?}"
        );
        assert!(
            !tag.to_lowercase().contains("next run 03:25")
                && !tag.to_lowercase().contains("next run at"),
            "tag must NOT claim the skipped slot is the next run, tag={tag:?}"
        );
        assert!(!tag.contains('\u{2014}'), "no em dashes, tag={tag:?}");
    }

    #[test]
    fn next_slot_runs_keeps_the_run_time() {
        let s = snap_with(0, &[("run", "", "run"), ("run", "", "run")]);
        let nr = resolve_next_run(&s);
        assert!(!nr.slot_skips, "a running slot must NOT read as a skip");
        assert!(nr.skip_reason_short.is_empty());
        assert!(!nr.all_week_skips);
    }

    #[test]
    fn all_week_skips_says_nothing_planned() {
        let s = snap_with(
            0,
            &[
                (
                    "skip",
                    "Heavy rain in next 3 days (0.62\" weighted)",
                    "rain_3day",
                ),
                (
                    "skip",
                    "Heavy rain in next 3 days (0.50\" weighted)",
                    "rain_3day",
                ),
                ("skip", "Already wet (0.30\" today)", "already_wet"),
            ],
        );
        let nr = resolve_next_run(&s);
        assert!(nr.slot_skips);
        assert!(nr.all_week_skips, "every upcoming day skips");
        assert_eq!(nr.next_likely_run_epoch, 0, "no upcoming day runs");
        assert_eq!(nr.skip_reason_short, "rain forecast");

        let tag = skip_tag_string(&nr, TZ);
        assert!(
            tag.contains(crate::voice::idle::WEEK_OF_RAIN),
            "tag={tag:?}"
        );
        assert!(tag.contains("Re-checks 03:25"), "tag={tag:?}");
        assert!(!tag.contains('\u{2014}'), "no em dashes, tag={tag:?}");
    }

    #[test]
    fn skip_slot_tomorrow_is_governed_by_day1_not_skip_check() {
        // The morning window has passed, so next_run_epoch is TOMORROW (day 1).
        // Today (day 0 / skip_check) is irrelevant to the slot; the slot's verdict
        // is day 1's cell. Here day 1 skips while today ran: the headline must
        // reflect the TOMORROW skip, reconciled off seven_day_verdicts[1].
        let s = snap_with(
            1,
            &[
                ("run", "", "run"),
                ("skip", "Soil already saturated", "soil_saturation"),
                ("run", "", "run"),
            ],
        );
        let nr = resolve_next_run(&s);
        assert!(
            nr.slot_skips,
            "tomorrow's slot skips, governed by day 1 not today"
        );
        assert_eq!(nr.skip_reason_short, "soil still moist");
        assert!(nr.next_likely_run_epoch > nr.slot_epoch);
    }

    #[test]
    fn today_pending_slot_prefers_live_skip_check_over_day0_cell() {
        // Regression guard: for TODAY's still-pending slot the
        // live skip_check is authoritative, NOT the projected day-0 strip cell
        // (which zeroes rain_now/wind_now and can disagree with the live call).
        //
        // FIX 3: deterministic. We FREEZE "now" (the clock resolve_next_run +
        // today_run_passed read) at a fixed mid-morning instant and pin the slot a
        // few hours later the SAME TZ-calendar day. Both are constants, so the slot
        // is unconditionally today-and-still-pending regardless of when the suite
        // runs: no wall-clock proximity to noon or midnight, no minute-boundary
        // straddle. (Previously the test pinned the slot relative to Utc::now(),
        // which flaked in the 23:55-23:59:59 ET window when "now" crossed midnight
        // between the slot calc and the internal Utc::now() read.)
        use chrono::TimeZone;
        let ny = chrono_tz::America::New_York;
        let day = chrono::NaiveDate::from_ymd_opt(2026, 6, 25).unwrap();
        let to_epoch = |h, m| {
            ny.from_local_datetime(&day.and_hms_opt(h, m, 0).unwrap())
                .single()
                .unwrap()
                .timestamp()
        };
        let now = to_epoch(9, 0); // frozen "now": 09:00 ET
        let slot = to_epoch(12, 0); // slot: 12:00 ET, same day, strictly future

        let mut s = IrrigationSnapshot::default();
        s.ha_reachable = true;
        s.timezone = TZ.into();
        s.next_run_epoch = slot;
        // Day-0 cell (today) projects a RUN, contradicting the live decision.
        s.seven_day_verdicts = vec![dv(0, slot, "run", "", "run")];
        s.skip_check = SkipCheck {
            will_skip: true,
            reason: "Currently raining (0.20 in/hr)".into(),
            reason_code: "rain_now".into(),
            ..Default::default()
        };
        let nr = test_support::with_frozen_now(now, || resolve_next_run(&s));
        assert!(
            nr.slot_skips,
            "today's pending slot must follow the live skip_check, not the day-0 cell"
        );
        assert_eq!(nr.skip_reason_short, "recent rain");
    }

    #[test]
    fn no_scheduled_slot_is_not_a_skip() {
        let mut s = IrrigationSnapshot::default();
        s.ha_reachable = true;
        s.timezone = TZ.into();
        s.next_run_epoch = 0;
        s.skip_check = SkipCheck {
            will_skip: true,
            ..Default::default()
        };
        let nr = resolve_next_run(&s);
        assert!(!nr.slot_skips);
        assert_eq!(nr.slot_epoch, 0);
    }

    #[test]
    fn plain_skip_phrase_maps_codes_and_falls_back() {
        assert_eq!(plain_skip_phrase("already_wet", ""), "recent rain");
        assert_eq!(plain_skip_phrase("soil_saturation", ""), "soil still moist");
        assert_eq!(plain_skip_phrase("rain_3day", ""), "rain forecast");
        assert_eq!(plain_skip_phrase("wind_now", ""), "high wind");
        // Empty code -> substring fallback on the baked reason.
        assert_eq!(
            plain_skip_phrase("", "Already wet (0.30\" today)"),
            "recent rain"
        );
        assert_eq!(
            plain_skip_phrase("", "Soil already saturated"),
            "soil still moist"
        );
        assert_eq!(
            plain_skip_phrase("", "Tomorrow rain (0.40\" x 85%)"),
            "rain forecast"
        );
    }
}

#[cfg(test)]
mod water_tile_tests {
    use super::*;

    /// A controller that reports a level shows it; one that does not
    /// gives the tile to days since rain; before any forecast the tile
    /// stays out rather than showing a dash.
    #[test]
    fn the_water_tile_never_shows_a_dash() {
        let mut s = IrrigationSnapshot::default();
        assert_eq!(water_tile(&s).2, None, "no data: no tile");

        s.last_refresh_epoch = 1_700_000_000;
        s.seven_day_verdicts = vec![Default::default()];
        s.skip_check.days_since_significant_rain = 4;
        assert_eq!(water_tile(&s), ("4".to_string(), "d", Some("Since rain")));

        s.water_level_pct = Some(72.4);
        assert_eq!(water_tile(&s), ("72".to_string(), "%", Some("Water level")));
    }
}
