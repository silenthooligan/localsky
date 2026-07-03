//! Deterministic, no-LLM plain-English explanation of a decision (P2-3).
//!
//! Turns a `DecisionTrace` into a short narrative a non-savvy user can read:
//! the verdict, the deciding factor in plain language, and a few reassurance
//! lines for the key checks that passed. Pure data over the shared
//! `DecisionTrace` (no ssr-only deps), so both the engine side and the wasm UI
//! compile it. The LLM advisor stays subordinate to this (AI summary; the
//! decision is rule-based).

use crate::ha::snapshot::{DecisionTrace, RuleEval};

/// A rendered plain-English explanation of one morning's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionExplanation {
    /// One-line plain verdict for the LEAD subject (the upcoming run when one is
    /// pending, else this morning's decision), e.g. "Watering next run".
    pub headline: String,
    /// The deciding factor in plain language (the rule that fired), or the
    /// all-clear sentence when nothing blocked a run.
    pub why: String,
    /// Plain-language summary of WHICH zones the upcoming run touches, derived
    /// from the per-zone verdicts (e.g. "Watering Back Yard only - it's the one
    /// zone below its soil target; the other 3 are saturated."). Empty when no
    /// per-zone data is available.
    pub zones_summary: String,
    /// A few key checks phrased as reassurance ("Not raining now", ...), so the
    /// reader sees what the engine actually considered. May be empty.
    pub considered: Vec<String>,
    /// Secondary, past-tense context for what already happened today, shown
    /// BELOW the upcoming-run lead so the card leads with the actionable. `None`
    /// when the lead already IS today's run (nothing to add as context).
    pub outcome: Option<String>,
    /// True when the decision ran on degraded inputs (stale station / aged
    /// forecast); the UI adds a lower-confidence note.
    pub degraded: bool,
}

/// A per-zone verdict line condensed to just what the upcoming-run summary
/// needs: did this zone run, what is it called, and (when it skips) why. Keeps
/// `explain.rs` decoupled from the full `ZoneState`/`ZoneVerdict` shape so the
/// summary is a pure, unit-testable function over plain data.
#[derive(Debug, Clone)]
pub struct ZoneLine {
    /// Friendly zone name.
    pub name: String,
    /// "run" | "run_extended" | "skip".
    pub verdict: String,
    /// Why this zone reached its verdict.
    pub reason: String,
    /// Which layer decided ("global" | "soil_saturation" | "soil_floor" | ...).
    pub source: String,
}

/// What the NEXT scheduled slot is predicted to do, as reconciled by the hero's
/// `resolve_next_run` (the 7-day cell matched to `next_run_epoch`). Once this
/// morning's window has passed, THIS is the upcoming decision the lead must
/// explain: the live trace still describes the (completed) morning, and its
/// deciding rung can legitimately differ from the next slot's (e.g. today
/// skipped on observed rain at 05:37 while tomorrow is a restricted day), so
/// previewing "next run" from today's trace showed a different reason than the
/// hero headline right above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextSlotPreview {
    /// True when the slot is predicted to skip.
    pub skips: bool,
    /// Structured reason id for the slot's skip ("restrictions",
    /// "tomorrow_rain", ...). Empty when the slot runs or the code is unknown.
    pub reason_code: String,
    /// The engine-written reason sentence for the slot's skip. Empty when the
    /// slot runs.
    pub reason: String,
}

/// Build the plain-English explanation, LEADING with the upcoming run so the
/// user learns WHY the next run will happen and WHICH zones BEFORE it runs (the
/// owner's #1 ask). The aggregate `trace` carries the deciding rule + checks;
/// `zones` drives the per-zone "which zones" summary; `today_done` is the
/// `today_run_passed` signal.
///
/// Tense / subject model:
/// - `today_done == false`: the next run is still ahead today; the trace IS the
///   upcoming run. Future tense ("Watering next run"), no separate outcome line.
/// - `today_done == true`: this morning is behind us and the next run is a later
///   day. The lead becomes that FUTURE run (future tense), and what happened
///   this morning drops to a secondary past-tense `outcome` line below it.
pub fn explain_decision(trace: &DecisionTrace, today_done: bool) -> DecisionExplanation {
    explain_decision_with_zones(trace, today_done, &[], None)
}

/// As [`explain_decision`], but threading the per-zone verdicts and the
/// reconciled next-slot preview. Callers with the full snapshot (the irrigation
/// hero) pass both; callers without (the welcome card) use the 2-arg
/// [`explain_decision`], which supplies an empty slice and no preview.
pub fn explain_decision_with_zones(
    trace: &DecisionTrace,
    today_done: bool,
    zones: &[ZoneLine],
    next: Option<&NextSlotPreview>,
) -> DecisionExplanation {
    // The lead always describes the NEXT run. When today's window is still
    // ahead, the live trace IS that run. Once the morning is behind us the next
    // run is a FUTURE slot: its verdict comes from the reconciled next-slot
    // preview (the same source as the hero headline), NOT from re-purposing
    // today's trace, whose deciding rung can legitimately differ (today skipped
    // on observed rain at dawn; tomorrow is a restricted day). Without a preview
    // (welcome card), the trace remains the best available approximation.
    let lead_from_preview = today_done && next.is_some();
    let headline = if lead_from_preview {
        let n = next.expect("checked is_some");
        if n.skips {
            "Skipping next run"
        } else {
            "Watering next run"
        }
    } else {
        match trace.verdict.as_str() {
            "skip" => "Skipping next run",
            "run_extended" => "Watering longer next run",
            _ => "Watering next run",
        }
    }
    .to_string();

    // The deciding factor for the LEAD, in plain language.
    let why = if lead_from_preview {
        let n = next.expect("checked is_some");
        if n.skips {
            why_for_next_slot(&n.reason_code, &n.reason)
        } else {
            "Every check is expected to pass, so the next run goes as scheduled.".to_string()
        }
    } else {
        match trace.rules.iter().find(|r| r.outcome == "fired") {
            Some(r) => why_for_fired(r),
            None => "Every check passes and at least one zone needs water, so the \
                 next run goes as scheduled."
                .to_string(),
        }
    };

    // WHICH zones, in plain language (owner's explicit "if it is a single zone,
    // give me the reason"). The per-zone verdicts describe TODAY's decision, so
    // once the morning is behind us they are past-tense context, not the
    // upcoming run.
    let zones_summary = zone_run_summary(zones, today_done);

    // Reassurance: the key safety / weather gates that PASS right now, in
    // ladder order. These are live current-conditions facts, so they read
    // correctly in both tenses.
    let considered: Vec<String> = trace
        .rules
        .iter()
        .filter(|r| r.outcome == "passed")
        .filter_map(|r| considered_phrase(&r.id))
        .map(str::to_string)
        .take(5)
        .collect();

    // Secondary context: only once this morning's window is behind us is there a
    // distinct "what already happened today" to report. Names the SPECIFIC rule
    // that decided the morning (with its measured margin when available) so the
    // outcome line answers "why was today skipped" on its own, instead of a bare
    // "Skipped this morning." next to a lead about a different day.
    let outcome = today_done.then(|| outcome_for_today(trace));

    DecisionExplanation {
        headline,
        why,
        zones_summary,
        considered,
        outcome,
        degraded: trace.degraded,
    }
}

/// Past-tense, self-contained summary of what TODAY's decision did and why,
/// built from today's trace: verdict + the fired rule's plain name, plus the
/// rule's measured margin ("1.02\" over the window") when the engine recorded
/// one. This is the "why was today skipped" answer once the lead has moved on
/// to a future slot.
fn outcome_for_today(trace: &DecisionTrace) -> String {
    let lead = match trace.verdict.as_str() {
        "skip" => "Skipped this morning",
        "run_extended" => "Watered longer this morning",
        _ => "Watered this morning",
    };
    let fired = trace.rules.iter().find(|r| r.outcome == "fired");
    match fired {
        Some(r) => {
            let name = if r.label.is_empty() {
                r.id.clone()
            } else {
                r.label.clone()
            };
            // lowercase the rule label so it reads as prose, not a heading.
            let mut name = name;
            if let Some(first) = name.get(..1) {
                let lower = first.to_lowercase();
                name.replace_range(..1, &lower);
            }
            match &r.margin_label {
                Some(m) if !m.is_empty() => format!("{lead}: {name} ({m})."),
                _ => format!("{lead}: {name}."),
            }
        }
        None => format!("{lead}."),
    }
}

/// Plain-language sentence for WHY the next slot is predicted to skip, keyed on
/// the slot's structured reason code (the same rule-id vocabulary the ladder
/// emits). Future-phrased, because the subject is an upcoming day. Falls back to
/// the engine-written reason sentence so an unmapped code still reads.
fn why_for_next_slot(reason_code: &str, reason: &str) -> String {
    match reason_code {
        "restrictions" => {
            "Local watering restrictions do not allow watering on that day.".to_string()
        }
        "already_wet" | "observed_rain" => {
            "Recent rain already covers it, so that run is not needed.".to_string()
        }
        "rain_now" => "Rain is expected then, so watering would be wasted.".to_string(),
        "rain_next_4h" => {
            "Rain is expected around that time, so watering would be wasted.".to_string()
        }
        "tomorrow_rain" => {
            "Rain is likely the following day, so the run is skipped to let it do the work."
                .to_string()
        }
        "rain_3day" => {
            "Heavy rain is expected over the surrounding days, so the run is skipped.".to_string()
        }
        "freeze_now" | "overnight_freeze" | "soil_frost" => {
            "Freezing conditions are expected, so watering is held to avoid ice damage.".to_string()
        }
        "wind_now" | "wind_forecast" => {
            "High wind is expected; spray would drift, so the run is held.".to_string()
        }
        "soil_saturation" => {
            "The soil is projected to still be saturated, so no watering is needed.".to_string()
        }
        "override" => "A manual override is in effect for that day.".to_string(),
        "paused" | "pause_until" => "Watering is paused (vacation mode).".to_string(),
        "dry_run" => "Dry-run mode is on, so nothing is actually watered.".to_string(),
        _ if !reason.is_empty() => {
            let r = reason.trim_end_matches('.');
            format!("{r}.")
        }
        _ => "The engine predicts a skip for that day.".to_string(),
    }
}

/// Plain-language summary of which zones the run waters and which skip, derived
/// purely from the per-zone verdicts. Honors the owner's explicit shapes: all
/// run, mixed, exactly one, all skip. Empty string when there are no per-zone
/// verdicts to summarize (weather-only deployments / pre-first-refresh).
///
/// `past` flips the summary to past tense: the per-zone verdicts always describe
/// TODAY's decision, so once the morning window has passed they narrate what
/// already happened ("All zones skipped this morning: ...") instead of
/// masquerading as the upcoming run.
pub fn zone_run_summary(zones: &[ZoneLine], past: bool) -> String {
    let total = zones.len();
    if total == 0 {
        return String::new();
    }
    let running: Vec<&ZoneLine> = zones.iter().filter(|z| z.verdict != "skip").collect();
    let skipping: Vec<&ZoneLine> = zones.iter().filter(|z| z.verdict == "skip").collect();
    let run_n = running.len();
    let skip_n = skipping.len();

    // All zones skip -> name the dominant skip reason.
    if run_n == 0 {
        if past {
            return format!(
                "All zones skipped this morning: {}.",
                dominant_skip_reason(&skipping)
            );
        }
        return format!("Skipping all zones: {}.", dominant_skip_reason(&skipping));
    }

    // All zones run.
    if skip_n == 0 {
        if past {
            if total == 1 {
                return format!("{} watered this morning.", running[0].name);
            }
            return format!("All {total} zones watered this morning.");
        }
        if total == 1 {
            return format!("Watering {}.", running[0].name);
        }
        return format!("Watering all {total} zones.");
    }

    // Exactly one zone runs while the rest skip -> name it and say WHY it runs
    // while the others don't (the soil-floor moat case).
    if run_n == 1 {
        let only = running[0];
        let why_one = why_single_runs(only);
        if past {
            let others = if skip_n == 1 {
                format!("the other 1 was {}", dominant_skip_reason(&skipping))
            } else {
                format!(
                    "the other {skip_n} were {}",
                    dominant_skip_reason(&skipping)
                )
            };
            return format!(
                "Only {} watered this morning: {why_one}; {others}.",
                only.name
            );
        }
        let others = if skip_n == 1 {
            format!("the other 1 is {}", dominant_skip_reason(&skipping))
        } else {
            format!("the other {skip_n} are {}", dominant_skip_reason(&skipping))
        };
        return format!("Watering {} only: {why_one}; {others}.", only.name);
    }

    // Mixed: several run, several skip.
    if past {
        return format!(
            "{} watered this morning ({run_n} of {total}); {skip_n} skipped ({}).",
            join_names(&running),
            dominant_skip_reason(&skipping)
        );
    }
    format!(
        "Watering {} ({run_n} of {total}); {skip_n} skipping ({}).",
        join_names(&running),
        dominant_skip_reason(&skipping)
    )
}

/// Why a single running zone waters while every other zone skips. Leads with the
/// soil-floor moat when that's the source (the common "one dry zone overrides a
/// blanket forecast-rain skip" case the owner called out), else a generic line.
fn why_single_runs(z: &ZoneLine) -> String {
    match z.source.as_str() {
        "soil_floor" => "it's the one zone below its soil target".to_string(),
        "soil_saturation" => "it's the one zone still short on moisture".to_string(),
        _ => "it's the only zone that needs water".to_string(),
    }
}

/// The dominant (most common) skip reason among skipping zones, condensed to a
/// short noun phrase so it slots into "...; 3 skipping (soil saturated)."
fn dominant_skip_reason(skipping: &[&ZoneLine]) -> &'static str {
    use std::collections::HashMap;
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for z in skipping {
        *counts.entry(skip_phrase(z)).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|(phrase, _)| phrase)
        .unwrap_or("skipping")
}

/// Condense one zone's skip into a short plain-language noun phrase, keyed off
/// the decision source first (most specific) then the reason text.
fn skip_phrase(z: &ZoneLine) -> &'static str {
    match z.source.as_str() {
        "soil_saturation" => return "soil saturated",
        "condition" => return "a custom rule",
        _ => {}
    }
    let r = z.reason.to_ascii_lowercase();
    if r.contains("saturat") {
        "soil saturated"
    } else if r.contains("rain") {
        "recent rain"
    } else if r.contains("wind") {
        "high wind"
    } else if r.contains("freez") || r.contains("frost") {
        "freeze risk"
    } else if r.contains("paus") || r.contains("vacation") {
        "paused"
    } else if r.contains("restrict") {
        "watering restrictions"
    } else {
        "skipping"
    }
}

/// Join up to two running-zone names plainly ("Back Yard and Front Yard"),
/// collapsing three or more to a count-led phrase to keep the line short.
fn join_names(running: &[&ZoneLine]) -> String {
    match running.len() {
        0 => String::new(),
        1 => running[0].name.clone(),
        2 => format!("{} and {}", running[0].name, running[1].name),
        n => format!("{} and {} more", running[0].name, n - 1),
    }
}

/// Plain-language sentence for the rule that decided. Falls back to the rule's
/// human label for any id without a bespoke phrasing, so a new gate still reads.
fn why_for_fired(r: &RuleEval) -> String {
    match r.id.as_str() {
        "override" => "A manual override is in effect for this decision.",
        "pause_until" | "paused" => "Watering is paused (vacation mode).",
        "restrictions" => "Local watering restrictions block watering at this time.",
        "live_data" => {
            "Live weather data is unavailable, so the engine fails safe and skips \
             rather than guess."
        }
        "rain_now" => "It is raining right now, so watering would be wasted.",
        "freeze_now" => {
            "It is cold enough to risk freezing, so watering is held to protect \
             the plants and pipes."
        }
        "overnight_freeze" => {
            "A freeze is forecast tonight, so watering is held to avoid ice damage."
        }
        "soil_frost" => "The soil is at frost temperature, so watering is held.",
        "wind_now" => {
            "It is too windy right now; spray would drift instead of landing on \
             the lawn."
        }
        "wind_forecast" => "High wind is forecast today; spray would drift, so watering is held.",
        "already_wet" => {
            "Enough rain has already fallen today, so the lawn does not need watering."
        }
        "observed_rain" => {
            "Enough rain has fallen in the last few days, so the yard is already covered."
        }
        "soil_saturation" => "The soil is already saturated, so no watering is needed.",
        "rain_next_4h" => {
            "Rain is expected within the next few hours, so watering now would be wasted."
        }
        "tomorrow_rain" => "Rain is likely tomorrow, so watering is skipped to let it do the work.",
        "rain_3day" => "Heavy rain is expected over the next few days, so watering is skipped.",
        "soil_floor" => {
            "A zone is measured below its minimum soil moisture, so it waters even \
             though rain is in the forecast."
        }
        "heat_advisory" => "A hot, dry stretch is forecast, so today's run is extended a little.",
        "dry_run" => "Dry-run mode is on, so nothing is actually watered today.",
        _ => return format!("Decided by: {}.", r.label),
    }
    .to_string()
}

/// Positive reassurance phrase for a key gate that PASSED. `None` for gates
/// that aren't worth surfacing as a reassurance line.
fn considered_phrase(id: &str) -> Option<&'static str> {
    Some(match id {
        "rain_now" => "Not raining now",
        "freeze_now" => "No freeze risk now",
        "overnight_freeze" => "No overnight freeze",
        "soil_frost" => "Soil is above frost",
        "wind_now" => "Wind is calm enough",
        "wind_forecast" => "No high wind forecast",
        "already_wet" => "Little or no rain today",
        "soil_saturation" => "Soil isn't saturated",
        "rain_next_4h" => "No rain expected soon",
        "tomorrow_rain" => "No significant rain tomorrow",
        "rain_3day" => "No heavy rain forecast",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, outcome: &str) -> RuleEval {
        RuleEval {
            id: id.into(),
            label: format!("{id} label"),
            category: "weather".into(),
            detail: String::new(),
            outcome: outcome.into(),
            verdict: if outcome == "fired" {
                Some("skip".into())
            } else {
                None
            },
            margin_label: None,
            // P1 additive operand fields default to None for these test fixtures.
            ..Default::default()
        }
    }

    fn trace(verdict: &str, degraded: bool, rules: Vec<RuleEval>) -> DecisionTrace {
        DecisionTrace {
            verdict: verdict.into(),
            reason: String::new(),
            degraded,
            rules,
            // P1 additive reason_code defaults to "" for these test fixtures.
            ..Default::default()
        }
    }

    fn zline(name: &str, verdict: &str, source: &str, reason: &str) -> ZoneLine {
        ZoneLine {
            name: name.into(),
            verdict: verdict.into(),
            reason: reason.into(),
            source: source.into(),
        }
    }

    #[test]
    fn skip_names_the_deciding_rule_in_plain_language() {
        let t = trace(
            "skip",
            false,
            vec![rule("rain_now", "passed"), rule("rain_next_4h", "fired")],
        );
        let e = explain_decision_with_zones(&t, false, &[], None);
        assert_eq!(e.headline, "Skipping next run");
        assert!(
            e.why.contains("within the next few hours"),
            "why={:?}",
            e.why
        );
        // The passed rain_now becomes a reassurance line.
        assert!(e.considered.iter().any(|c| c == "Not raining now"));
        assert!(!e.degraded);
        // Before this morning passes there is no separate "today" context.
        assert_eq!(e.outcome, None);
        // Once the morning run-time has passed, today's outcome drops to a
        // secondary past-tense line that NAMES the rule that decided it (so it
        // stands alone as "why was today skipped"), while the lead still
        // describes the NEXT run.
        let past = explain_decision_with_zones(&t, true, &[], None);
        assert_eq!(past.headline, "Skipping next run");
        assert_eq!(
            past.outcome.as_deref(),
            Some("Skipped this morning: rain_next_4h label.")
        );
    }

    #[test]
    fn run_with_no_fired_rule_is_all_clear() {
        let t = trace(
            "run",
            false,
            vec![rule("rain_now", "passed"), rule("freeze_now", "passed")],
        );
        let e = explain_decision_with_zones(&t, false, &[], None);
        assert_eq!(e.headline, "Watering next run");
        assert!(e.why.contains("Every check passes"));
        assert_eq!(e.considered.len(), 2);
        // Once today is behind us, the outcome line reports it in past tense.
        let past = explain_decision_with_zones(&t, true, &[], None);
        assert_eq!(past.headline, "Watering next run");
        assert_eq!(past.outcome.as_deref(), Some("Watered this morning."));
    }

    #[test]
    fn soil_floor_run_explains_the_moat() {
        let t = trace("run", false, vec![rule("soil_floor", "fired")]);
        let e = explain_decision_with_zones(&t, false, &[], None);
        assert_eq!(e.headline, "Watering next run");
        assert!(e.why.contains("below its minimum soil moisture"));
    }

    #[test]
    fn run_extended_leads_with_the_next_run() {
        let t = trace("run_extended", false, vec![rule("heat_advisory", "fired")]);
        assert_eq!(
            explain_decision_with_zones(&t, false, &[], None).headline,
            "Watering longer next run"
        );
        // The past outcome line is tense-correct once the window is behind us.
        let past = explain_decision_with_zones(&t, true, &[], None);
        assert_eq!(past.headline, "Watering longer next run");
        assert_eq!(
            past.outcome.as_deref(),
            Some("Watered longer this morning: heat_advisory label.")
        );
    }

    #[test]
    fn degraded_is_carried_through() {
        let t = trace("skip", true, vec![rule("rain_3day", "fired")]);
        let e = explain_decision_with_zones(&t, false, &[], None);
        assert!(e.degraded);
        assert!(e.why.contains("Heavy rain"));
    }

    #[test]
    fn unknown_fired_rule_falls_back_to_label() {
        let t = trace("skip", false, vec![rule("some_future_gate", "fired")]);
        let e = explain_decision_with_zones(&t, false, &[], None);
        assert!(e.why.contains("some_future_gate label"));
    }

    #[test]
    fn considered_is_capped_at_five() {
        let passed = [
            "rain_now",
            "freeze_now",
            "overnight_freeze",
            "soil_frost",
            "wind_now",
            "wind_forecast",
            "already_wet",
        ];
        let t = trace(
            "run",
            false,
            passed.iter().map(|id| rule(id, "passed")).collect(),
        );
        let e = explain_decision_with_zones(&t, false, &[], None);
        assert_eq!(e.considered.len(), 5);
    }

    #[test]
    fn post_dispatch_lead_is_the_next_slot_reason_not_todays() {
        // The exact reported case: today skipped on OBSERVED RAIN at dawn, but
        // the next slot is a RESTRICTED day. Once the morning has passed, the
        // lead must describe the NEXT slot (restrictions), while the outcome
        // line reports what today did (observed rain). Previously the lead
        // reused today's trace and read "recent rain", contradicting the
        // headline that already said restrictions.
        let t = trace("skip", false, vec![rule("observed_rain", "fired")]);
        let next = NextSlotPreview {
            skips: true,
            reason_code: "restrictions".into(),
            reason: "Watering restriction: no watering Wednesdays".into(),
        };
        let e = explain_decision_with_zones(&t, true, &[], Some(&next));
        assert_eq!(e.headline, "Skipping next run");
        assert!(
            e.why.contains("restrictions do not allow"),
            "lead should explain the NEXT slot (restrictions), got: {}",
            e.why
        );
        // The morning's own reason is still reported, in the outcome line.
        assert_eq!(
            e.outcome.as_deref(),
            Some("Skipped this morning: observed_rain label.")
        );
    }

    #[test]
    fn post_dispatch_next_slot_runs_leads_future_positive() {
        // Today skipped, but the next slot is predicted to run: the lead flips
        // to the upcoming run even though today's trace verdict is "skip".
        let t = trace("skip", false, vec![rule("already_wet", "fired")]);
        let next = NextSlotPreview {
            skips: false,
            reason_code: String::new(),
            reason: String::new(),
        };
        let e = explain_decision_with_zones(&t, true, &[], Some(&next));
        assert_eq!(e.headline, "Watering next run");
        assert!(e.why.contains("expected to pass"));
        assert_eq!(
            e.outcome.as_deref(),
            Some("Skipped this morning: already_wet label.")
        );
    }

    #[test]
    fn zones_summary_is_past_tense_once_the_morning_passed() {
        let zones = [
            zline("Back Yard", "skip", "global", "recent rain"),
            zline("Front Yard", "skip", "global", "recent rain"),
        ];
        // Ahead of the window: present tense.
        assert_eq!(
            zone_run_summary(&zones, false),
            "Skipping all zones: recent rain."
        );
        // Behind the window: past tense, so it does not masquerade as the next run.
        assert_eq!(
            zone_run_summary(&zones, true),
            "All zones skipped this morning: recent rain."
        );
    }

    // ── zone_run_summary: the four owner-named shapes ──

    #[test]
    fn zone_summary_empty_when_no_zones() {
        assert_eq!(zone_run_summary(&[], false), "");
    }

    #[test]
    fn zone_summary_all_run() {
        let zones = [
            zline("Back Yard", "run", "default", ""),
            zline("Front Yard", "run", "default", ""),
            zline("Side Yard", "run_extended", "heat", ""),
        ];
        assert_eq!(zone_run_summary(&zones, false), "Watering all 3 zones.");
    }

    #[test]
    fn zone_summary_single_zone_total_names_it() {
        let zones = [zline("Back Yard", "run", "default", "")];
        assert_eq!(zone_run_summary(&zones, false), "Watering Back Yard.");
    }

    #[test]
    fn zone_summary_mixed_names_runners_and_dominant_skip() {
        let zones = [
            zline("Back Yard", "run", "default", ""),
            zline("Front Yard", "run", "default", ""),
            zline("Side Yard", "skip", "soil_saturation", "Soil saturated"),
            zline("Shrubs", "skip", "soil_saturation", "Soil saturated"),
        ];
        assert_eq!(
            zone_run_summary(&zones, false),
            "Watering Back Yard and Front Yard (2 of 4); 2 skipping (soil saturated)."
        );
    }

    #[test]
    fn zone_summary_single_runner_explains_the_soil_floor_moat() {
        let zones = [
            zline("Back Yard", "run", "soil_floor", "Below soil target"),
            zline("Front Yard", "skip", "soil_saturation", "Soil saturated"),
            zline("Side Yard", "skip", "soil_saturation", "Soil saturated"),
            zline("Shrubs", "skip", "soil_saturation", "Soil saturated"),
        ];
        assert_eq!(
            zone_run_summary(&zones, false),
            "Watering Back Yard only: it's the one zone below its soil target; \
             the other 3 are soil saturated."
        );
    }

    #[test]
    fn zone_summary_all_skip_names_the_reason() {
        let zones = [
            zline("Back Yard", "skip", "global", "Rain forecast tomorrow"),
            zline("Front Yard", "skip", "global", "Rain forecast tomorrow"),
        ];
        assert_eq!(
            zone_run_summary(&zones, false),
            "Skipping all zones: recent rain."
        );
    }
}
