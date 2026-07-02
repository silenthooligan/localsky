// Hero card for the irrigation page. Mirrors the Tempest hero in
// visual weight: huge mono headline, status tag, glyph. Says one of:
//
//   - "TOMORROW · 06:06" / scheduled total / each zone duration
//   - "SKIPPED" with the reason that tripped the morning skip-check
//   - "RUNNING NOW" if a zone is currently active
//   - "HA UNREACHABLE" if the refresher hasn't connected yet
//
// The detail row below the headline always carries the live skip-check
// inputs so the user can see why the system is making the call it is,
// not just the verdict.

use crate::components::irrigation::advisor::AdvisorExplanation;
use crate::components::units_fmt::{
    depth_unit, depth_value_mm, fmt_rain_amount, fmt_temp_short, fmt_wind, temp_unit,
    use_unit_prefs, wind_unit, wind_value,
};
use crate::ha::snapshot::IrrigationSnapshot;
use crate::reason_render::render_skip_reason;
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
pub fn today_run_passed(s: &IrrigationSnapshot) -> bool {
    if s.next_run_epoch <= 0 {
        return false;
    }
    let run_hm = crate::timefmt::format_hm(s.next_run_epoch, &s.timezone);
    let now_hm = crate::timefmt::format_hm(now_epoch_secs(), &s.timezone);
    if run_hm.is_empty() || now_hm.is_empty() {
        return false;
    }
    // Zero-padded "HH:MM" sorts chronologically, so lexical >= is "now is at or
    // past the run's clock-time today".
    now_hm >= run_hm
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
    /// Epoch of the next FORWARD day predicted to water after a skipping slot, for
    /// "Next likely run: <day>". 0 when no upcoming day in the 7-day window runs.
    pub next_likely_run_epoch: i64,
    /// True when EVERY remaining day in the 7-day window (including the slot) is
    /// predicted to skip: there is no watering planned this week.
    pub all_week_skips: bool,
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
            next_likely_run_epoch: 0,
            all_week_skips: false,
        };
    }

    let slot_md = crate::timefmt::format_md(slot_epoch, tz);
    // The 7-day cell that describes this slot, matched by calendar date.
    let slot_day = (!slot_md.is_empty())
        .then(|| {
            s.seven_day_verdicts
                .iter()
                .find(|d| crate::timefmt::format_md(d.time_epoch, tz) == slot_md)
        })
        .flatten();

    // Is the slot TODAY's still-pending window? For that one slot the LIVE
    // skip_check is authoritative, NOT the projected day-0 strip cell. The day-0
    // cell is a synthetic projection that zeroes the live rain_now/wind_now
    // inputs, forces forecast_stale=false, and defaults live_readings=Station, so
    // it can show NEXT RUN while it is actually raining (live rain skip), or
    // SKIPPING when a stale forecast made the projection run. skip_check is the
    // exact decision the dispatcher will act on at the slot, so the hero must
    // match it. (W6 regression guard, fix #6/T2.) Anchored on the slot's calendar
    // date EQUALLING today (in the deployment tz) and the morning still being
    // ahead, NOT the stored day_offset: in real data day_offset==0 is today, but
    // keying on the live calendar date keeps a passed morning or a later day on
    // the cell path and stays correct as the day rolls over.
    let now_md = crate::timefmt::format_md(now_epoch_secs(), tz);
    let slot_is_today_pending = !slot_md.is_empty() && slot_md == now_md && !today_run_passed(s);

    // Does the slot skip? For today's still-pending window prefer the live
    // skip_check. Otherwise prefer the matched 7-day cell; failing a match, defer
    // to skip_check only when the slot is today's still-pending window (same run).
    // A passed morning with no matching cell falls through to "runs" (no evidence
    // of skip).
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
        next_likely_run_epoch,
        all_week_skips,
    }
}

/// Condense a skip reason to a short, plain-language noun phrase for the hero tag
/// ("recent rain", "soil still moist", "rain forecast"). Keys on the structured
/// `reason_code` first (unit-independent, P1 architecture), falling back to a
/// substring match on the baked reason so legacy rows still read. Mirrors the
/// vocabulary the Week tab and `explain::skip_phrase` use so the surfaces agree.
fn plain_skip_phrase(reason_code: &str, reason: &str) -> String {
    match reason_code {
        "already_wet" | "observed_rain" | "rain_now" => return "recent rain".to_string(),
        "soil_saturation" => return "soil still moist".to_string(),
        "rain_next_4h" | "tomorrow_rain" | "rain_3day" => return "rain forecast".to_string(),
        "wind_now" | "wind_forecast" => return "high wind".to_string(),
        "freeze_now" | "overnight_freeze" | "soil_frost" => return "freeze risk".to_string(),
        "restrictions" => return "watering restrictions".to_string(),
        "paused" | "pause_until" => return "paused".to_string(),
        _ => {}
    }
    let r = reason.to_ascii_lowercase();
    if r.contains("saturat") || r.contains("moist") {
        "soil still moist".to_string()
    } else if r.contains("already wet") || r.contains("currently raining") || r.contains("observed")
    {
        "recent rain".to_string()
    } else if r.contains("rain") {
        "rain forecast".to_string()
    } else if r.contains("wind") {
        "high wind".to_string()
    } else if r.contains("freez") || r.contains("frost") {
        "freeze risk".to_string()
    } else if r.contains("restrict") {
        "watering restrictions".to_string()
    } else if r.contains("paus") || r.contains("vacation") {
        "paused".to_string()
    } else {
        "current conditions".to_string()
    }
}

/// The HONEST hero tag for a skipping next slot. Leads with the plain reason,
/// shows the slot time ONLY as a re-evaluation ("Re-checks HH:MM", never "next
/// run"), then points at the next likely run so a water-conscious user can plan,
/// or says nothing is planned this week. "(based on current conditions)" keeps it
/// honest about the prediction without burying the truth that it is currently not
/// going to water. Pure so it is unit-testable. Public so the Weather-home
/// watering strip (app.rs HomeWateringVerdict) renders the SAME honest skip copy
/// as the hero instead of a second, divergent implementation.
pub fn skip_tag_string(nr: &NextRunStatus, tz: &str) -> String {
    let reason = if nr.skip_reason_short.is_empty() {
        "current conditions".to_string()
    } else {
        nr.skip_reason_short.clone()
    };
    let recheck = format!("Re-checks {}", crate::timefmt::format_hm(nr.slot_epoch, tz));
    let plan = if nr.all_week_skips {
        "No watering planned this week, re-checks daily".to_string()
    } else if nr.next_likely_run_epoch > 0 {
        format!(
            "Next likely run {}",
            format_relative_time(nr.next_likely_run_epoch, tz)
        )
    } else {
        String::new()
    };
    if plan.is_empty() {
        format!("Skipping, {reason} (based on current conditions) \u{b7} {recheck}")
    } else {
        format!("Skipping, {reason} (based on current conditions) \u{b7} {recheck} \u{b7} {plan}")
    }
}

/// Condense the snapshot's per-zone verdicts into the plain `ZoneLine`s the
/// explainer's `zone_run_summary` consumes. Reads each zone's back-filled
/// `verdict` first (the canonical per-zone path), falling back to the
/// snapshot-level `zone_verdicts` list by slug, mirroring the engine's own
/// `zone_skip_verdict` lookup. Zones with no verdict yet are dropped so the
/// summary only counts zones the engine has actually decided.
fn zone_lines(s: &IrrigationSnapshot) -> Vec<crate::explain::ZoneLine> {
    s.zones
        .iter()
        .filter_map(|z| {
            let v = z
                .verdict
                .as_ref()
                .or_else(|| s.zone_verdicts.iter().find(|v| v.zone_slug == z.slug))?;
            Some(crate::explain::ZoneLine {
                name: z.name.clone(),
                verdict: v.verdict.clone(),
                reason: v.reason.clone(),
                source: v.source.clone(),
            })
        })
        .collect()
}

#[component]
pub fn NextRunHero(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // The hero answers ONE question consistently: what's the watering status,
    // and when does it run next. Three lines that must always agree and never
    // duplicate each other:
    //   EYEBROW  = a short, tense-aware VERDICT word (colored by --hero-verdict).
    //   HEADLINE = the big WHEN (next run time) or a status word.
    //   TAG      = the supporting detail (skip reason / run plan / running zone).
    // The phase ladder below is in priority order; each closure resolves to the
    // same phase, so the three lines stay in lockstep.

    // EYEBROW: IDENTIFIES the headline so the big time is never non-descript.
    // It labels what the headline is (the next run / skipping / running / paused),
    // while the tense-aware VERDICT detail lives in the tag below. Colored by
    // --hero-verdict. Honest about skips: when the next scheduled slot is predicted
    // to skip, the eyebrow says so ("SKIPPING") instead of "NEXT RUN", so the big
    // headline time below reads as a re-check, never a promise to water.
    let eyebrow = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "OFFLINE"
        } else if s.zones.iter().any(|z| z.running) {
            "WATERING NOW"
        } else if s.skip_check.will_skip && s.skip_check.reason.starts_with("Paused") {
            "PAUSED"
        } else if s.next_run_epoch > 0 {
            if resolve_next_run(&s).slot_skips {
                "SKIPPING"
            } else {
                "NEXT RUN"
            }
        } else {
            "NO RUNS SCHEDULED"
        }
    };

    let glyph = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "x"
        } else if s.zones.iter().any(|z| z.running) {
            "droplet"
        } else if s.skip_check.will_skip && s.skip_check.reason.starts_with("Paused") {
            "pause"
        } else if resolve_next_run(&s).slot_skips
            || (s.skip_check.will_skip && s.next_run_epoch <= 0)
        {
            // The next scheduled slot is predicted to SKIP (or there is an
            // open-ended skip with no scheduled run). Glyph the reason, not a
            // droplet, so the icon never implies water is coming. Keys on the
            // structured reason_code that DESCRIBES THE SLOT (P2 units
            // architecture) so the glyph is correct regardless of rendered unit;
            // legacy rows with an empty code fall back to the baked reason.
            let nr = resolve_next_run(&s);
            let (code, reason) = if s.next_run_epoch > 0 && nr.slot_skips {
                // Use the slot's own reason vocabulary (already condensed).
                ("", nr.skip_reason_short.clone())
            } else {
                (
                    s.skip_check.reason_code.as_str(),
                    s.skip_check.reason.clone(),
                )
            };
            match code {
                "rain_now" | "already_wet" | "observed_rain" | "rain_next_4h" | "tomorrow_rain"
                | "rain_3day" => "cloud-rain",
                "wind_now" | "wind_forecast" => "wind",
                "freeze_now" | "overnight_freeze" | "soil_frost" => "snowflake",
                _ => {
                    let r = reason.to_ascii_lowercase();
                    if r.contains("rain") || r.contains("wet") || r.contains("moist") {
                        "cloud-rain"
                    } else if r.contains("wind") {
                        "wind"
                    } else if r.contains("freez") || r.contains("frost") {
                        "snowflake"
                    } else {
                        "cloud-sun"
                    }
                }
            }
        } else {
            "droplet"
        }
    };

    // HEADLINE: the big WHEN, or the truthful STATUS when there is nothing to time
    // (running now / offline / no run scheduled) OR when the next scheduled slot is
    // predicted to SKIP. A skip headline must NOT show a time as if it were a run:
    // the slot time moves to the tag below as an explicit re-check.
    let headline = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "HA unreachable".to_string()
        } else if s.zones.iter().any(|z| z.running) {
            "Running now".to_string()
        } else if s.skip_check.will_skip && s.skip_check.reason.starts_with("Paused") {
            if s.next_run_epoch > 0 {
                format_relative_time(s.next_run_epoch, &s.timezone)
            } else {
                "Paused".to_string()
            }
        } else if s.next_run_epoch > 0 {
            if resolve_next_run(&s).slot_skips {
                // Truthful: the engine is not going to water at the next slot.
                "Not watering".to_string()
            } else {
                format_relative_time(s.next_run_epoch, &s.timezone)
            }
        } else {
            "No run scheduled".to_string()
        }
    };

    // Per-device unit preference; the skip reason re-renders unit-aware from the
    // structured SkipCheck (P2 units architecture). Read prefs.get() inside the
    // tag closure so a units change re-renders.
    let prefs = use_unit_prefs();

    // TAG: the supporting detail. For a real upcoming run it leads with WHY it
    // will run and WHICH zones (the owner's #1 ask). For a skipping slot it tells
    // the truth: the plain reason, the slot time as an explicit RE-CHECK (never a
    // "next run"), and the next likely run so a water-conscious user can plan.
    let tag = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "Refresher offline".to_string()
        } else if let Some(z) = s.zones.iter().find(|z| z.running) {
            let running_count = s.zones.iter().filter(|z| z.running).count();
            if running_count > 1 {
                format!(
                    "{} running, {} of {} zones active",
                    z.name,
                    running_count,
                    s.zones.len()
                )
            } else {
                format!("{} running", z.name)
            }
        } else if s.skip_check.will_skip && s.skip_check.reason.starts_with("Paused") {
            // Eyebrow says PAUSED; the tag is the live reason ("Paused until ...").
            render_skip_reason(&s.skip_check, prefs.get())
        } else if s.skip_check.will_skip && s.next_run_epoch <= 0 {
            // Open-ended skip with no scheduled run: the only thing to say is the
            // skip and why. Tense-aware (Skipped/Skipping today).
            let verb = if today_run_passed(&s) {
                "Skipped"
            } else {
                "Skipping"
            };
            format!(
                "{verb} today · {}",
                render_skip_reason(&s.skip_check, prefs.get())
            )
        } else {
            let nr = resolve_next_run(&s);
            if nr.slot_skips {
                skip_tag_string(&nr, &s.timezone)
            } else {
                // A real run is scheduled: lead with the upcoming run in FUTURE
                // tense and name the zones, so the user sees what is coming BEFORE
                // it runs. The per-zone summary is the substance; fall back to the
                // zone count + minutes.
                let summary = crate::explain::zone_run_summary(&zone_lines(&s));
                if !summary.is_empty() {
                    return summary;
                }
                // Count only zones that will actually WATER (verdict != "skip"),
                // matching the HeroStats "Zones watering" tile and the per-zone
                // summary; counting s.zones.len() here would promise water for
                // zones the engine is going to skip. Before any zone has a verdict
                // (the fall-through reason this branch exists), default to all
                // zones so a pre-decision frame still reads sensibly.
                let any_decided = s.zones.iter().any(|z| {
                    z.verdict.is_some() || s.zone_verdicts.iter().any(|v| v.zone_slug == z.slug)
                });
                let watering_n =
                    if any_decided {
                        s.zones
                            .iter()
                            .filter(|z| {
                                match z.verdict.as_ref().or_else(|| {
                                    s.zone_verdicts.iter().find(|v| v.zone_slug == z.slug)
                                }) {
                                    Some(v) => v.verdict != "skip",
                                    None => z.planned_run_seconds > 0,
                                }
                            })
                            .count()
                    } else {
                        s.zones.len()
                    };
                format!(
                    "Will water {} zones for {:.0} min total",
                    watering_n, s.next_run_total_minutes
                )
            }
        }
    };

    // Quiet caption beneath the next-run line so the user understands the
    // morning call is re-evaluated at run time and CAN change overnight: the
    // owner was blindsided when a "skipped" card ran at 3:40 AM. One honest line,
    // only when a real future RUN is scheduled. Suppressed on a skipping slot,
    // whose tag already carries the explicit "Re-checks HH:MM" (no double note).
    let recheck_note = move || {
        let s = snap.get();
        s.ha_reachable
            && !s.zones.iter().any(|z| z.running)
            && s.next_run_epoch > 0
            && !resolve_next_run(&s).slot_skips
    };

    // P1-1: honest confidence. When the decision ran on substituted inputs (a
    // stale station and/or an aged forecast, both folded into the trace's degraded
    // flag in P0-2), say so on the main hero, not only in the Rule Lab.
    let degraded = move || {
        snap.get()
            .decision_trace
            .as_ref()
            .map(|t| t.degraded)
            .unwrap_or(false)
    };

    // Forced-run warning: when a sticky Force override is watering THROUGH a hard
    // guard (freeze / restriction / raining-now / dry-run), the engine surfaces
    // the would-be guard in `force_overrode_guard`. Name it on the hero so the
    // operator KNOWS what they are running past. Worded so it never implies the
    // override won't run: the run still happens, this only flags the protection
    // it is bypassing. None when there is no force-run or it overrides nothing.
    let forced_guard = move || snap.get().force_overrode_guard.clone();

    // #1: state-aware hero. One verdict state drives a CSS custom-property
    // cascade (--hero-verdict) so the glyph glow, a subtle surface tint, and the
    // top-edge stripe all match the decision: teal = watering/scheduled, blue =
    // skip, amber = paused, neutral = offline. The hero stops looking identical
    // whether it is watering tonight, skipping for a week, or paused.
    let hero_state = move || {
        let s = snap.get();
        if !s.ha_reachable {
            "off"
        } else if s.zones.iter().any(|z| z.running) {
            "run"
        } else if s.skip_check.will_skip && s.skip_check.reason.starts_with("Paused") {
            "paused"
        } else if s.next_run_epoch > 0 {
            // Theme by what the NEXT SLOT actually does. A slot the engine will
            // water is teal ("run"); a slot it will skip is blue ("skip"), so the
            // surface tint matches the honest headline instead of always reading
            // teal when a later run exists.
            if resolve_next_run(&s).slot_skips {
                "skip"
            } else {
                "run"
            }
        } else if s.skip_check.will_skip {
            // Open-ended skip with no scheduled run stays blue.
            "skip"
        } else {
            "run"
        }
    };

    view! {
        <section
            class="next-run-hero"
            class:hero-run=move || hero_state() == "run"
            class:hero-skip=move || hero_state() == "skip"
            class:hero-paused=move || hero_state() == "paused"
            class:hero-off=move || hero_state() == "off"
        >
            <div class="next-run-glyph" aria-hidden="true">
                {move || view! { <crate::components::ui::Icon name=glyph() size=44/> }}
            </div>
            <div class="next-run-body">
                // #4 (design): the deterministic verdict is the LEAD. Eyebrow
                // (verdict word) + headline (WHEN) + tag (plain-English reason /
                // run plan) sit at the very top, and the "Explain this decision"
                // expander follows immediately, BEFORE the agronomy stats, so the
                // product's differentiator ("will it water tonight, and why") is
                // the first thing read, not buried under four numbers.
                <div class="next-run-eyebrow">{eyebrow}</div>
                <h1 class="next-run-headline">{headline}</h1>
                <div
                    class="next-run-tag"
                    class:next-run-tag-skip=move || {
                        let s = snap.get();
                        // Style as a skip when the NEXT SLOT skips, or an
                        // open-ended skip with no scheduled run, matching the
                        // honest headline rather than today's morning verdict.
                        (s.next_run_epoch > 0 && resolve_next_run(&s).slot_skips)
                            || (s.skip_check.will_skip && s.next_run_epoch <= 0)
                    }
                >
                    {tag}
                </div>
                {move || forced_guard().map(|guard| view! {
                    // Forced-run warning. Names the hard guard the Force override
                    // is watering THROUGH, without implying the run won't happen
                    // (it will). Amber caution, keyed off the same wind/caution
                    // token the degraded chip uses; inline-styled to avoid a new
                    // stylesheet rule (this refactor is scoped to .rs).
                    <div
                        class="hero-forced-warn"
                        role="status"
                        style="display:inline-flex;align-items:center;gap:0.4rem;\
                               margin-top:var(--space-2);padding:0.32rem 0.7rem;\
                               border-radius:999px;font-size:var(--text-body-sm);\
                               font-weight:600;line-height:1.3;color:var(--verdict-wind);\
                               background:color-mix(in oklab, var(--verdict-wind) 12%, transparent);\
                               border:1px solid color-mix(in oklab, var(--verdict-wind) 40%, transparent);"
                        title="A Force override is active, so this run WILL water. It is running past a safety guard that would otherwise skip; this names that guard so the call is intentional."
                    >
                        <crate::components::ui::Icon name="alert-triangle" size=14/>
                        <span>{format!("Force will water through {}", guard)}</span>
                    </div>
                })}
                {move || recheck_note().then(|| view! {
                    // Subtle, honest caption: the morning call is re-evaluated at
                    // run time and can change if overnight conditions change.
                    // Inline-styled (this refactor is scoped to .rs only) to read
                    // quiet without a new stylesheet rule.
                    <div
                        class="next-run-recheck"
                        style="margin-top:0.35rem;font-size:var(--text-meta);\
                               letter-spacing:0.04em;color:var(--text-faint);"
                    >
                        "Re-checked at run time"
                    </div>
                })}
                {move || degraded().then(|| view! {
                    <div
                        class="hero-confidence hero-confidence--degraded"
                        title="The live station was stale or the forecast was aged when this was decided, so it used substituted or backup data. The deterministic rules still applied; treat the verdict as lower-confidence until live data returns."
                    >
                        <crate::components::ui::Icon name="alert-triangle" size=14/>
                        <span>"Decided on backup data"</span>
                    </div>
                })}
                // #4 (design) + nesting nit: the deterministic, no-LLM
                // plain-English "why" leads, directly under the verdict and
                // BEFORE the agronomy stats. The LLM Advisor is nested INSIDE
                // this explainer (it omits itself when offline/disabled), so the
                // two "why" surfaces read as primary + supplement, never as two
                // unrelated siblings where an offline advisor looks like a fault.
                {view! { <DecisionExplainer snap/> }.into_any()}
                {view! { <HeroStats snap/> }.into_any()}
                {view! { <SkipBreakdown snap/> }.into_any()}
            </div>
        </section>
    }
}

/// P2-3: a one-tap, deterministic plain-English "why" for the morning decision,
/// rendered from the decision trace with no LLM. Collapsed by default; expanding
/// shows the verdict in plain language, the deciding factor, the key checks that
/// passed, and a lower-confidence note when the inputs were degraded.
#[component]
fn DecisionExplainer(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
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
        let e = crate::explain::explain_decision_with_zones(
            &trace,
            today_run_passed(&s),
            &zone_lines(&s),
        );
        // WHICH zones the upcoming run touches, the primary "what to expect".
        let zones_line = (!e.zones_summary.is_empty()).then(|| {
            view! {
                <p
                    class="decision-explainer__zones"
                    style="margin:0 0 var(--space-2);line-height:1.5;\
                           color:var(--text-bright);"
                >
                    {e.zones_summary}
                </p>
            }
        });
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
        // Secondary, past-tense context: what already happened this morning. Sits
        // BELOW the upcoming-run lead so the card leads with the actionable.
        let outcome = e.outcome.map(|o| {
            view! {
                <p
                    class="decision-explainer__outcome"
                    style="margin:var(--space-2) 0 0;font-size:var(--text-body-sm);\
                           color:var(--text-dim);"
                >
                    {o}
                </p>
            }
        });
        let degraded = e.degraded.then(|| {
            view! {
                <p class="decision-explainer__degraded">
                    "Decided on backup data, so this is lower-confidence until live data returns."
                </p>
            }
        });
        view! {
            <p class="decision-explainer__why">
                <strong>{e.headline}". "</strong>
                {e.why}
            </p>
            {zones_line}
            {checks}
            {outcome}
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
            <summary class="decision-explainer__summary">"Explain this decision"</summary>
            <div class="decision-explainer__body">
                {explanation}
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
        // Single per-zone "waters tonight" predicate so the Tonight minutes and
        // the Zones-watering count stay in lockstep (T10): a zone the engine will
        // skip (verdict == "skip") must NOT contribute to either tile. Falls back
        // to planned-run-seconds for zones the engine hasn't decided yet.
        let waters_tonight = |z: &crate::ha::snapshot::ZoneState| -> bool {
            match z
                .verdict
                .as_ref()
                .or_else(|| s.zone_verdicts.iter().find(|v| v.zone_slug == z.slug))
            {
                Some(v) => v.verdict != "skip",
                None => z.planned_run_seconds > 0,
            }
        };
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
        let water = format!("{:.0}", s.water_level_pct);
        // Soil deficit is the mean of each zone's bucket, in MILLIMETERS.
        let deficit_empty = s.zones.is_empty();
        let deficit = if deficit_empty {
            "-".to_string()
        } else {
            let avg_mm = s.zones.iter().map(|z| z.bucket_mm).sum::<f64>() / s.zones.len() as f64;
            depth_value_mm(avg_mm, p)
        };
        // No unit glyph for the placeholder so "-" reads clean.
        let deficit_unit = if deficit_empty { "" } else { depth_unit(p) };
        view! {
            <div class="ir-hero-stats">
                <div class="ir-hero-stat">
                    <span class="ir-hero-stat__v">{tonight}<span class="ir-hero-stat__u">"min"</span></span>
                    <span class="ir-hero-stat__k">"Tonight"</span>
                </div>
                <div class="ir-hero-stat">
                    <span class="ir-hero-stat__v">{watering}</span>
                    <span class="ir-hero-stat__k">"Zones watering"</span>
                </div>
                <div class="ir-hero-stat">
                    <span class="ir-hero-stat__v">{water}<span class="ir-hero-stat__u">"%"</span></span>
                    <span class="ir-hero-stat__k">"Water level"</span>
                </div>
                <div class="ir-hero-stat">
                    <span class="ir-hero-stat__v">{deficit}<span class="ir-hero-stat__u">{deficit_unit}</span></span>
                    <span class="ir-hero-stat__k">"Soil deficit"</span>
                </div>
            </div>
        }
    }
}

#[component]
fn SkipBreakdown(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // Each row pairs an input with its threshold and a tripped-or-not
    // marker. Always rendered so the user can see what was checked and
    // why it passed (or didn't), not just the final verdict.
    // Each row is type-erased so the breakdown's monomorphized type
    // stays flat (5 AnyViews under one div, vs. 5 deeply-nested
    // 4-span tuples that explode rustc's query-depth budget).
    let prefs = use_unit_prefs();
    view! {
        <div class="skip-breakdown" role="list">
            {view! {
                <SkipRow
                    label="Paused"
                    value=Signal::derive(move || if snap.get().skip_check.is_paused { "on".into() } else { "off".into() })
                    threshold=Signal::derive(|| "off".to_string())
                    tripped=Signal::derive(move || snap.get().skip_check.is_paused)
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Freeze"
                    value=Signal::derive(move || {
                        let p = prefs.get();
                        format!("{}{}", fmt_temp_short(snap.get().skip_check.temp_now_f, p), temp_unit(p))
                    })
                    threshold=Signal::derive(move || {
                        let p = prefs.get();
                        format!("≥ {}{}", fmt_temp_short(snap.get().skip_check.min_temp_f, p), temp_unit(p))
                    })
                    tripped=Signal::derive(move || {
                        let s = snap.get();
                        s.skip_check.temp_now_f < s.skip_check.min_temp_f
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Wind"
                    value=Signal::derive(move || fmt_wind(snap.get().skip_check.wind_now_mph, prefs.get()))
                    threshold=Signal::derive(move || {
                        let p = prefs.get();
                        format!("≤ {} {}", wind_value(snap.get().skip_check.max_wind_mph, p), wind_unit(p))
                    })
                    tripped=Signal::derive(move || {
                        let s = snap.get();
                        s.skip_check.wind_now_mph > s.skip_check.max_wind_mph
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Wet today"
                    value=Signal::derive(move || fmt_rain_amount(snap.get().skip_check.rain_today_in, prefs.get()))
                    threshold=Signal::derive(move || format!("< {}", fmt_rain_amount(0.05, prefs.get())))
                    tripped=Signal::derive(move || snap.get().skip_check.rain_today_in >= 0.05)
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Forecast (×prob)"
                    value=Signal::derive(move || {
                        let s = snap.get().skip_check;
                        format!(
                            "{} × {}%",
                            fmt_rain_amount(s.forecast_in, prefs.get()), s.rain_tomorrow_prob_pct
                        )
                    })
                    threshold=Signal::derive(move || format!("< {}", fmt_rain_amount(snap.get().skip_check.rain_skip_in, prefs.get())))
                    tripped=Signal::derive(move || {
                        let s = snap.get().skip_check;
                        s.forecast_in * (s.rain_tomorrow_prob_pct as f64) / 100.0
                            >= s.rain_skip_in
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Rain next 4h"
                    value=Signal::derive(move || fmt_rain_amount(snap.get().skip_check.rain_next_4h_in, prefs.get()))
                    threshold=Signal::derive(move || format!("< {}", fmt_rain_amount(0.10, prefs.get())))
                    tripped=Signal::derive(move || snap.get().skip_check.rain_next_4h_in >= 0.10)
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Rain forecast 3d"
                    value=Signal::derive(move || fmt_rain_amount(snap.get().skip_check.rain_3day_weighted_in, prefs.get()))
                    threshold=Signal::derive(move || format!("< {}", fmt_rain_amount(snap.get().skip_check.rain_skip_in * 1.5, prefs.get())))
                    tripped=Signal::derive(move || {
                        let s = snap.get().skip_check;
                        s.rain_3day_weighted_in >= 1.5 * s.rain_skip_in
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Overnight low"
                    value=Signal::derive(move || {
                        let p = prefs.get();
                        format!("{}{}", fmt_temp_short(snap.get().skip_check.temp_min_24h_f, p), temp_unit(p))
                    })
                    threshold=Signal::derive(move || {
                        let p = prefs.get();
                        format!("≥ {}{}", fmt_temp_short(snap.get().skip_check.min_temp_f, p), temp_unit(p))
                    })
                    tripped=Signal::derive(move || {
                        // Validity flag (not the old 0.0 sentinel) so a real
                        // sub-zero forecast low still trips the row.
                        let s = snap.get().skip_check;
                        s.temp_min_24h_valid && s.temp_min_24h_f < s.min_temp_f
                    })
                />
            }.into_any()}
            {view! {
                <SkipRow
                    label="Heat index 3d"
                    value=Signal::derive(move || {
                        let p = prefs.get();
                        format!("{}{}", fmt_temp_short(snap.get().skip_check.heat_index_max_3day_f, p), temp_unit(p))
                    })
                    threshold=Signal::derive(move || {
                        let p = prefs.get();
                        format!("< {}{}", fmt_temp_short(95.0, p), temp_unit(p))
                    })
                    tripped=Signal::derive(move || snap.get().skip_check.heat_index_max_3day_f >= 95.0)
                />
            }.into_any()}
        </div>
    }
}

#[component]
fn SkipRow(
    label: &'static str,
    value: Signal<String>,
    threshold: Signal<String>,
    tripped: Signal<bool>,
) -> impl IntoView {
    view! {
        <div class="sk-row" class:sk-row-tripped=move || tripped.get()>
            <span class="sk-mark" aria-hidden="true">
                {move || {
                    let name = if tripped.get() { "x" } else { "check" };
                    view! { <crate::components::ui::Icon name=name size=13 stroke=2.5/> }
                }}
            </span>
            <span class="sk-label">{label}</span>
            <span class="sk-value">{move || value.get()}</span>
            <span class="sk-threshold">{move || threshold.get()}</span>
        </div>
    }
}

/// Format an epoch as "TODAY · HH:MM" / "TOMORROW · HH:MM" / "WED · HH:MM" /
/// "JUN 28 · HH:MM", in 24-hour LOCAL time rendered in the deployment's IANA
/// `tz` (not the viewer's browser zone), so the hero matches the deployment's
/// wall clock and the HA mobile notification. Empty `tz` falls back to
/// browser-local (hydrate) / UTC (ssr) via `crate::timefmt`.
///
/// The day label is derived by comparing the deployment-tz calendar date of the
/// target against today (and tomorrow), using `format_md` for the date identity
/// so the TODAY/TOMORROW determination is itself in the deployment timezone. The
/// "within a week -> weekday" bucket uses coarse epoch arithmetic (a DST day is
/// off by an hour, which never crosses the 7-day bucket boundary in practice).
fn format_relative_time(epoch: i64, tz: &str) -> String {
    use crate::timefmt::{format_hm, format_md, format_wday_short};
    if epoch <= 0 {
        return "-".to_string();
    }
    let hhmm = format_hm(epoch, tz);
    let now = chrono::Utc::now().timestamp();
    // Calendar-date identity in the deployment tz (e.g. "Jun 28"). Comparing the
    // rendered md strings keeps the TODAY/TOMORROW call in the deployment zone.
    let target_md = format_md(epoch, tz);
    let today_md = format_md(now, tz);
    let tomorrow_md = format_md(now + 86_400, tz);
    if !target_md.is_empty() && target_md == today_md {
        format!("TODAY · {hhmm}")
    } else if !target_md.is_empty() && target_md == tomorrow_md {
        format!("TOMORROW · {hhmm}")
    } else if epoch - now < 7 * 86_400 {
        format!("{} · {hhmm}", format_wday_short(epoch, tz).to_uppercase())
    } else {
        format!("{} · {hhmm}", target_md.to_uppercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::snapshot::{DayVerdict, IrrigationSnapshot, SkipCheck};

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
        assert!(tag.starts_with("Skipping, recent rain"), "tag={tag:?}");
        assert!(
            tag.contains("Re-checks 03:25"),
            "tag must show the slot time as a re-check, tag={tag:?}"
        );
        assert!(
            tag.contains("Next likely run"),
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
            tag.contains("No watering planned this week, re-checks daily"),
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
        // W6 regression guard (fix #6/T2): for TODAY's still-pending slot the
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
