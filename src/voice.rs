// The product's voice, in one place, with a line drawn through it.
//
// LocalSky decides whether to open a valve, and it tells the operator
// why. Some of those messages are a lawn report and some are evidence.
// The difference matters more than the jokes do, so it is enforced here
// rather than left to whoever is writing the next string.
//
// PLAYFUL is allowed for: routine watering outcomes, empty states,
// nothing-to-do days, and edge cases that are genuinely absurd (the sun
// declining to rise, a yard with no location on earth). These are places
// where a dry aside costs nothing and reads like a product someone made
// on purpose.
//
// STRAIGHT is required for: jurisdictional restrictions, freeze, wind,
// hardware and dispatch failures, and any message that says the engine
// could not see the weather. An operator reading one of those is either
// checking a legal obligation, diagnosing why their lawn is dying, or
// deciding whether to trust us with their water bill. A joke there is
// not charming, it is a reason to doubt the number next to it.
//
// The engine's own reason strings are never touched by this module. They
// carry the thresholds and the measured values, they are what the
// decision log records, and they are the thing an operator quotes back
// to us in a bug report. This is a presentation layer over the top.

/// What a save says when the change is already working.
///
/// Fourteen settings pages had fourteen versions of this sentence, each
/// naming the machinery that picks the change up: the engine's next
/// tick, the registry hot-reloading, the advisor's next call, the
/// scheduler, the radar menu rebuilding. The operator is not asking
/// which subsystem reloads. They are asking the one question a save can
/// leave open, which is whether they have to do anything else.
pub const SAVED_LIVE: &str = "Saved. No restart needed.";

/// What a save says when it does NOT take effect until a restart.
///
/// The other half of the same question, and the half worth being loud
/// about: a change that looks saved and is not yet running is how an
/// operator ends up debugging a setting they already fixed.
pub const SAVED_NEEDS_RESTART: &str = "Saved. Restart LocalSky to use it.";

/// What a save says when it never left the browser.
///
/// The units and theme preferences are per-device on purpose, so the
/// confirmation has to say where the change went. "No restart needed"
/// would be true and useless: there is no server in this story.
pub const SAVED_ON_THIS_DEVICE: &str = "Saved on this device";

/// What a save says during setup, before anything is live.
///
/// The wizard writes a draft, not a configuration; a change made there
/// is real but inert until the last step applies it, and saying "no
/// restart needed" would imply it is already running.
pub const SAVED_TO_DRAFT: &str =
    "Saved to your setup draft. It takes effect when you finish setup.";

/// The prefix a deferral reason carries when the plan is waiting on
/// forecast rain, and the one it carries when the morning window was
/// already full.
///
/// A contract, not decoration: the engine writes these in front of the
/// sentence it composes, and the zone card and the zone detail strip
/// them back off so the reason reads as prose under a heading that
/// already says "deferred". Three files spelled the same two prefixes,
/// and a typo in any of them would have shown the operator the marker
/// instead of hiding it.
pub const REASON_DEFERRED: &str = "deferred: ";
pub const REASON_WAITS_FOR_TOMORROW: &str = "waits for tomorrow: ";

/// The question the controller picker asks, on the settings page and in
/// the wizard step that shows the same picker.
pub const WHAT_RUNS_YOUR_SPRINKLERS: &str = "What runs your sprinklers?";

/// What a location search accepts, shown in both places one is offered.
pub const LOCATION_SEARCH_EXAMPLE: &str = "e.g. Springfield, Sydney, or 51.5, -0.1";

/// Which register a message is allowed to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Facts, numbers, and no personality. Compliance, safety, outages.
    Straight,
    /// A dry aside is welcome.
    Playful,
}

/// The tone a gate family must be written in.
///
/// Keyed off the same families the verdict surfaces already use, so a new
/// gate cannot quietly land in the wrong register.
pub fn tone_for(family: crate::gates_catalog::GateFamily) -> Tone {
    use crate::gates_catalog::GateFamily;
    match family {
        // A legal obligation. The operator may be reading this to check
        // whether they are about to be fined.
        GateFamily::Restriction => Tone::Straight,
        // Safety. Ice on a walkway is not a punchline.
        GateFamily::Freeze => Tone::Straight,
        // Wasted water and a wet driveway, with a measured gust behind it.
        GateFamily::Wind => Tone::Straight,
        // "We could not see the weather, so we did not guess." The whole
        // value of that sentence is that it sounds like an engineer wrote
        // it.
        GateFamily::NoData => Tone::Straight,
        // The lawn has water. Nobody is in trouble.
        GateFamily::Water => Tone::Playful,
        // The operator did this on purpose and knows it.
        GateFamily::Pause => Tone::Playful,
        GateFamily::SoilModel => Tone::Playful,
        GateFamily::Other => Tone::Playful,
    }
}

/// Why there is no next run, in the operator's language.
///
/// The engine distinguishes these three because they need different
/// actions: one is a setting, one is a fact about the sky, and one is a
/// fact about the law. They used to be a single zero that rendered
/// identically to a fully restricted week.
pub mod next_run {
    /// No location configured.
    ///
    /// Playful: this is a setup step, not a failure, and the operator is
    /// two clicks from fixing it.
    pub const NO_LOCATION: &str = "No location set yet. \
        Sunrise math works anywhere on earth, but not nowhere in particular. \
        Add your location under Settings and the schedule fills itself in.";

    /// Polar latitude, no sunrise on the days in range.
    ///
    /// Playful, because the alternative is a blank screen and a support
    /// ticket. The second sentence is the actually useful part.
    pub const NO_SUNRISE: &str = "The sun is not scheduled to rise here for a while. \
        Watering runs before dawn, so there is no morning to aim at. \
        Nothing is broken, and normal service resumes with the daylight.";

    /// Every day in the horizon is refused by a restriction.
    ///
    /// STRAIGHT. This is the legal surface, and the operator may be
    /// checking it against their district's published rules.
    pub const NO_LEGAL_DAY: &str = "No watering day is permitted in the next two weeks \
        under your configured restrictions. Check Settings, then Restrictions, \
        if that does not match your district's published rules.";
}

/// Watch-only mode: LocalSky decides, something else waters.
///
/// The capability was already complete and effectively undiscoverable.
/// It was labeled "Dry-run (logs, no dispatch)" in one place and "No
/// hardware (simulate)" in another, and its description opened with "No
/// irrigation hardware?", which frames it as a fallback for people with
/// nothing to water. The bigger audience is the opposite: people who
/// have a working system and are not ready to hand over the valves.
///
/// That is a reasonable thing to want from software that opens valves
/// and spends water, and it should be an obvious choice rather than a
/// debugging flag.
pub mod watch_only {
    /// The picker entry.
    pub const LABEL: &str = "Watch only (never waters)";

    /// The one-line label in the controller list.
    pub const SHORT: &str = "Watch only, never opens a valve";

    /// What it is and who it is for. Leads with the evaluation case.
    pub const BLURB: &str = "LocalSky works out what it would water each day and never opens         a valve. Use it to see how it handles your yard while your current system keeps         watering, or to look around before your hardware arrives. Its decisions still show         on the dashboard and in Home Assistant.";

    /// The fact that makes the mode genuinely useful rather than a toy,
    /// and the one nobody would guess.
    pub const COUNTS_REAL_WATER: &str = "It still notices water your own system puts down, so         what it shows you stays true to your yard rather than drifting dry.";

    /// Shown on the dashboard while the mode is active, so nobody
    /// mistakes an advisory plan for something that watered.
    pub const DASHBOARD_BANNER: &str = "Watch only. LocalSky is not watering anything.         Everything below is what it would have done.";
}

/// Nothing-to-do days, where a dry line beats an empty panel.
pub mod idle {
    /// The whole week skips on rain or wet soil.
    pub const WEEK_OF_RAIN: &str = "Nothing to do all week. The sky is handling it.";

    /// No zones configured at all.
    pub const NO_ZONES: &str = "No zones yet, so LocalSky is watching the weather \
        with great enthusiasm and no sprinklers. Add a zone to give it something to do.";

    /// The yard is watered and the plan is simply waiting.
    pub const NO_WATER_PLANNED: &str = "No watering planned for tomorrow";
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gates_catalog::GateFamily;

    /// The families where a joke would cost the operator something.
    const MUST_BE_STRAIGHT: &[GateFamily] = &[
        GateFamily::Restriction,
        GateFamily::Freeze,
        GateFamily::Wind,
        GateFamily::NoData,
    ];

    #[test]
    fn compliance_and_safety_are_never_playful() {
        for f in MUST_BE_STRAIGHT {
            assert_eq!(
                tone_for(*f),
                Tone::Straight,
                "{f:?} is a legal, safety or data-integrity surface"
            );
        }
    }

    /// Every gate family has a tone. A new one cannot land without an
    /// author deciding which register it belongs in, because the match in
    /// `tone_for` is exhaustive and this walks the same list.
    #[test]
    fn every_family_has_a_declared_tone() {
        for f in [
            GateFamily::Restriction,
            GateFamily::Freeze,
            GateFamily::Wind,
            GateFamily::Water,
            GateFamily::Pause,
            GateFamily::SoilModel,
            GateFamily::NoData,
            GateFamily::Other,
        ] {
            // Panics rather than returning a default, which is the point.
            let _ = tone_for(f);
        }
    }

    /// The straight copy stays straight. Not a style opinion: a district
    /// restriction message is something an operator may hold up next to
    /// their water authority's own wording.
    #[test]
    fn the_legal_message_carries_no_jokes_and_says_where_to_look() {
        let s = next_run::NO_LEGAL_DAY;
        assert!(s.contains("restrictions"), "names the cause");
        assert!(s.contains("Settings"), "says where to go");
        for tell in ["!", "great enthusiasm", "handling it", "punchline"] {
            assert!(!s.contains(tell), "legal copy must stay flat: {tell}");
        }
    }

    /// Playful copy still has to do its job. A joke that leaves the
    /// operator not knowing what to do is just noise.
    #[test]
    fn playful_copy_still_tells_the_operator_what_to_do() {
        assert!(next_run::NO_LOCATION.contains("Settings"));
        assert!(idle::NO_ZONES.contains("Add a zone"));
        // And says plainly that nothing is wrong, because a polar winter
        // looks exactly like a broken scheduler from the outside.
        assert!(next_run::NO_SUNRISE.contains("Nothing is broken"));
    }

    /// Watch-only is described by who it is FOR, not by what hardware
    /// someone lacks.
    ///
    /// The old copy opened with "No irrigation hardware?", which reads as
    /// a fallback for people with nothing to water and quietly excludes
    /// the larger audience: people with a working system who are not
    /// ready to hand over the valves.
    #[test]
    fn watch_only_speaks_to_someone_who_already_waters() {
        let b = watch_only::BLURB;
        assert!(
            b.contains("current system keeps"),
            "must address someone who already waters: {b}"
        );
        assert!(
            !b.contains("No irrigation hardware"),
            "must not open by assuming the reader has nothing"
        );
        // The name says what it does, not how it is implemented.
        assert!(watch_only::LABEL.contains("never waters"));
        for jargon in ["Dry-run", "dry run", "simulate", "dispatch", "stub"] {
            assert!(
                !watch_only::LABEL.contains(jargon) && !watch_only::SHORT.contains(jargon),
                "the label must not be implementation talk: {jargon}"
            );
        }
    }

    /// The dashboard has to say plainly that nothing watered, or an
    /// advisory plan reads as a record of what happened.
    #[test]
    fn watch_only_says_out_loud_that_nothing_watered() {
        let d = watch_only::DASHBOARD_BANNER;
        assert!(d.contains("not watering"));
        assert!(d.contains("would have done"));
    }

    /// House style, applied to our own copy.
    #[test]
    fn the_copy_follows_house_style() {
        let all = [
            next_run::NO_LOCATION,
            next_run::NO_SUNRISE,
            next_run::NO_LEGAL_DAY,
            idle::WEEK_OF_RAIN,
            idle::NO_ZONES,
            idle::NO_WATER_PLANNED,
            watch_only::LABEL,
            watch_only::SHORT,
            watch_only::BLURB,
            watch_only::COUNTS_REAL_WATER,
            watch_only::DASHBOARD_BANNER,
        ];
        for s in all {
            assert!(!s.contains('\u{2014}'), "no em dashes: {s}");
            // British spellings that creep in through weather vocabulary.
            for brit in ["colour", "vapour", "metre", "organise", "recognise"] {
                assert!(!s.to_lowercase().contains(brit), "American English: {s}");
            }
            assert!(!s.is_empty());
        }
    }
}
