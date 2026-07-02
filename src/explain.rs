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
    explain_decision_with_zones(trace, today_done, &[])
}

/// As [`explain_decision`], but threading the per-zone verdicts so the result
/// carries the "which zones" summary. Callers with the full snapshot (the
/// irrigation hero) use this; callers without per-zone data (the welcome card)
/// use the 2-arg [`explain_decision`], which supplies an empty slice.
pub fn explain_decision_with_zones(
    trace: &DecisionTrace,
    today_done: bool,
    zones: &[ZoneLine],
) -> DecisionExplanation {
    // The lead always describes the NEXT run. Its verdict: when today is still
    // ahead, that's the trace's verdict; once today is behind us the next run is
    // a future day, but the same deterministic ladder applies, so the trace's
    // verdict is still the best available preview of it.
    let headline = match trace.verdict.as_str() {
        "skip" => "Skipping next run",
        "run_extended" => "Watering longer next run",
        _ => "Watering next run",
    }
    .to_string();

    // The deciding rule is the one that fired (at most one, by ladder design).
    // Phrased in FUTURE tense because the lead is the upcoming run.
    let why = match trace.rules.iter().find(|r| r.outcome == "fired") {
        Some(r) => why_for_fired(r),
        None => "Every check passes and at least one zone needs water, so the \
                 next run goes as scheduled."
            .to_string(),
    };

    // WHICH zones the upcoming run touches, in plain language (owner's explicit
    // "if it is a single zone, give me the reason").
    let zones_summary = zone_run_summary(zones);

    // Reassurance: the key safety / weather gates that PASS, in ladder order.
    let considered: Vec<String> = trace
        .rules
        .iter()
        .filter(|r| r.outcome == "passed")
        .filter_map(|r| considered_phrase(&r.id))
        .map(str::to_string)
        .take(5)
        .collect();

    // Secondary context: only once this morning's window is behind us is there a
    // distinct "what already happened today" to report. Before then the lead IS
    // today's run, so there is nothing to add.
    let outcome = today_done.then(|| {
        match trace.verdict.as_str() {
            "skip" => "Skipped this morning.",
            "run_extended" => "Watered longer this morning.",
            _ => "Watered this morning.",
        }
        .to_string()
    });

    DecisionExplanation {
        headline,
        why,
        zones_summary,
        considered,
        outcome,
        degraded: trace.degraded,
    }
}

/// Plain-language summary of which zones the upcoming run waters and which skip,
/// derived purely from the per-zone verdicts. Honors the owner's explicit
/// shapes: all run, mixed, exactly one, all skip. Empty string when there are no
/// per-zone verdicts to summarize (weather-only deployments / pre-first-refresh).
pub fn zone_run_summary(zones: &[ZoneLine]) -> String {
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
        return format!("Skipping all zones: {}.", dominant_skip_reason(&skipping));
    }

    // All zones run.
    if skip_n == 0 {
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
        let others = if skip_n == 1 {
            format!("the other 1 is {}", dominant_skip_reason(&skipping))
        } else {
            format!("the other {skip_n} are {}", dominant_skip_reason(&skipping))
        };
        return format!("Watering {} only: {why_one}; {others}.", only.name);
    }

    // Mixed: several run, several skip.
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
        let e = explain_decision_with_zones(&t, false, &[]);
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
        // secondary past-tense line while the lead still describes the NEXT run.
        let past = explain_decision_with_zones(&t, true, &[]);
        assert_eq!(past.headline, "Skipping next run");
        assert_eq!(past.outcome.as_deref(), Some("Skipped this morning."));
    }

    #[test]
    fn run_with_no_fired_rule_is_all_clear() {
        let t = trace(
            "run",
            false,
            vec![rule("rain_now", "passed"), rule("freeze_now", "passed")],
        );
        let e = explain_decision_with_zones(&t, false, &[]);
        assert_eq!(e.headline, "Watering next run");
        assert!(e.why.contains("Every check passes"));
        assert_eq!(e.considered.len(), 2);
        // Once today is behind us, the outcome line reports it in past tense.
        let past = explain_decision_with_zones(&t, true, &[]);
        assert_eq!(past.headline, "Watering next run");
        assert_eq!(past.outcome.as_deref(), Some("Watered this morning."));
    }

    #[test]
    fn soil_floor_run_explains_the_moat() {
        let t = trace("run", false, vec![rule("soil_floor", "fired")]);
        let e = explain_decision_with_zones(&t, false, &[]);
        assert_eq!(e.headline, "Watering next run");
        assert!(e.why.contains("below its minimum soil moisture"));
    }

    #[test]
    fn run_extended_leads_with_the_next_run() {
        let t = trace("run_extended", false, vec![rule("heat_advisory", "fired")]);
        assert_eq!(
            explain_decision_with_zones(&t, false, &[]).headline,
            "Watering longer next run"
        );
        // The past outcome line is tense-correct once the window is behind us.
        let past = explain_decision_with_zones(&t, true, &[]);
        assert_eq!(past.headline, "Watering longer next run");
        assert_eq!(
            past.outcome.as_deref(),
            Some("Watered longer this morning.")
        );
    }

    #[test]
    fn degraded_is_carried_through() {
        let t = trace("skip", true, vec![rule("rain_3day", "fired")]);
        let e = explain_decision_with_zones(&t, false, &[]);
        assert!(e.degraded);
        assert!(e.why.contains("Heavy rain"));
    }

    #[test]
    fn unknown_fired_rule_falls_back_to_label() {
        let t = trace("skip", false, vec![rule("some_future_gate", "fired")]);
        let e = explain_decision_with_zones(&t, false, &[]);
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
        let e = explain_decision_with_zones(&t, false, &[]);
        assert_eq!(e.considered.len(), 5);
    }

    // ── zone_run_summary: the four owner-named shapes ──

    #[test]
    fn zone_summary_empty_when_no_zones() {
        assert_eq!(zone_run_summary(&[]), "");
    }

    #[test]
    fn zone_summary_all_run() {
        let zones = [
            zline("Back Yard", "run", "default", ""),
            zline("Front Yard", "run", "default", ""),
            zline("Side Yard", "run_extended", "heat", ""),
        ];
        assert_eq!(zone_run_summary(&zones), "Watering all 3 zones.");
    }

    #[test]
    fn zone_summary_single_zone_total_names_it() {
        let zones = [zline("Back Yard", "run", "default", "")];
        assert_eq!(zone_run_summary(&zones), "Watering Back Yard.");
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
            zone_run_summary(&zones),
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
            zone_run_summary(&zones),
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
        assert_eq!(zone_run_summary(&zones), "Skipping all zones: recent rain.");
    }
}
