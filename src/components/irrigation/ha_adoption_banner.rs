// One-time notice: the Home Assistant helpers LocalSky read for the last
// time, and what it uses instead.
//
// 0.7.22 retired seven helper reads. Three of them, the input_number skip
// thresholds, OUTRANKED the matching values in Settings, so on those installs
// the number deciding when the yard skipped was not the number the Settings
// page showed. Four were operator controls whose native store the Home
// Assistant path never consulted. The migration read each one once, wrote the
// value into LocalSky's own storage, and stopped reading the entity.
//
// A value that moved without anyone asking, with nothing on screen saying so,
// is the failure this notice exists to prevent. So it names every entity, says
// what LocalSky uses now, and where the adopted value differs from what was on
// screen it prints BOTH numbers and says which one was actually deciding. A
// line where they agree says "nothing changed" rather than staying silent,
// because silence there reads as an omission.
//
// EVERY CLOSING SENTENCE IS DERIVED FROM THE RECORD SET. Hardcoding them cost
// this notice its truth twice. "Your helpers still exist" is false on an
// install where all seven were absent. "They no longer do anything, delete
// them when you are ready" is false, and dangerous, on an install with no
// persistence database, where the four controls were never taken over and are
// still deciding: deleting the pause helper there drops a live pause with
// nothing on screen. So the notice names only what it handled, and says
// plainly which helpers are still live.
//
// It speaks whenever anything was retired or changed, including the install
// where every helper was absent. Something did happen there: the four
// controls are LocalSky's own now, which is what makes Rain delay work.
//
// Quiet in the normal case: `ha_adoption` is empty on every standalone install
// and on every Home Assistant install until the pass runs, so nothing renders.
//
// Dismissal is sticky in localStorage, keyed by the exact set adopted, the same
// rule the default-budget notice follows. SSR and the first hydrate frame
// render nothing: the dismissal is read in a hydrate-only effect, so the server
// DOM and the first client DOM match.

use leptos::prelude::*;

use crate::ha::snapshot::{HaAdoptedHelper, IrrigationSnapshot};

/// localStorage key holding the adopted set the operator dismissed.
#[cfg(feature = "hydrate")]
const DISMISS_KEY: &str = "ha_adoption_banner_dismissed";

/// The four operator controls. They only adopt when a persistence database is
/// mounted, so an install without one keeps reading them and the notice has to
/// say so. This component compiles for the browser, where the planner's module
/// does not, so it carries its own copy; an ssr-only test pins the two lists
/// together.
const CONTROLS: [&str; 4] = [
    "input_datetime.irrigation_pause_until",
    "input_select.irrigation_override_tomorrow",
    "input_boolean.irrigation_pause",
    "input_boolean.irrigation_dry_run",
];

const PAUSE_UNTIL: &str = "input_datetime.irrigation_pause_until";
const PAUSE_TOGGLE: &str = "input_boolean.irrigation_pause";

#[component]
pub fn HaAdoptionBanner(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // The dismissed set key. Empty until the hydrate effect reads it, so SSR
    // and the first client frame agree.
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
        let key = adopted_key(&snap.get_untracked());
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
        if !worth_showing(&s) || dismissed.get() == adopted_key(&s) {
            return ().into_any();
        }
        let tz = s.timezone.clone();
        let lines: Vec<String> = if awaiting_config(&s) {
            vec![awaiting_config_line()]
        } else {
            let mut lines = vec![header_line(&s.ha_adoption)];
            lines.extend(s.ha_adoption.iter().map(|h| helper_line(h, &tz)));
            lines.extend(hold_lines(&s.ha_adoption, s.last_refresh_epoch, &tz));
            lines.extend(still_exist_line(&s.ha_adoption));
            lines.extend(still_live_line(&s.ha_adoption, s.controls_persisted));
            lines.extend(automation_line(&s.ha_adoption));
            lines
        };
        view! {
            <div class="anomaly-banner" role="status" aria-live="polite">
                <div class="anomaly-banner-icon" aria-hidden="true">"!"</div>
                <div class="anomaly-banner-text">
                    {lines
                        .into_iter()
                        .map(|l| {
                            view! {
                                <div class="anomaly-banner-line anomaly-banner-line--wrap">{l}</div>
                            }
                        })
                        .collect_view()}
                </div>
                <a class="anomaly-banner-link" href="/settings/skip-rules">"Open Settings"</a>
                <button
                    type="button"
                    class="anomaly-banner-dismiss"
                    aria-label="Dismiss Home Assistant migration notice"
                    on:click=on_dismiss
                >
                    "\u{2715}"
                </button>
            </div>
        }
        .into_any()
    }
}

/// Whether this install has anything to be told.
///
/// Anything retired or changed speaks, including a set where every helper was
/// absent: on that install the four controls became LocalSky's own, which is
/// what makes Rain delay work, and the read gate flipped for all seven. The
/// notice used to require a helper to have been PRESENT, which silenced it on
/// exactly the install where the pass concluded the most from the least.
fn worth_showing(s: &IrrigationSnapshot) -> bool {
    !s.ha_adoption.is_empty() || awaiting_config(s)
}

/// The migration has not run because this install has no config file to
/// record it in. Every helper is still deciding there, so the notice speaks
/// even though nothing was adopted: silence would leave the release notes'
/// invitation to delete the helpers standing over a live pause.
fn awaiting_config(s: &IrrigationSnapshot) -> bool {
    s.ha_adoption_awaiting_config && s.ha_adoption.is_empty()
}

fn awaiting_config_line() -> String {
    "LocalSky has not migrated your Home Assistant helpers: this install has no localsky.toml \
     to record the migration in, so all seven helpers are still deciding, exactly as before this \
     release. Do not delete them. Finishing the setup wizard writes the file, and the migration \
     runs on its own after that."
        .to_string()
}

/// True when this record is about an entity that was in Home Assistant.
fn was_present(h: &HaAdoptedHelper) -> bool {
    h.entity.starts_with("input_")
        && matches!(h.outcome.as_str(), "adopted" | "unreadable" | "kept_local")
}

fn header_line(recs: &[HaAdoptedHelper]) -> String {
    if recs.iter().any(was_present) {
        "LocalSky read your Home Assistant helpers one last time. These values are its own now, \
         and it will not read those entities again."
            .to_string()
    } else {
        // Saying "read your helpers" to an install that had none is the kind
        // of small lie that makes the rest of the notice unbelievable.
        "LocalSky has stopped reading these Home Assistant helpers. None of them were in Home \
         Assistant when it looked, so it holds these values itself now."
            .to_string()
    }
}

/// Where each hold went, and whether one is now in force.
///
/// Two controls hold the yard and each has two shapes that have to be named.
/// The Rain delay the release notes told owners to set before upgrading, and
/// the pause switch, come across `adopted`. The other shape is `kept_local`:
/// a timed pause, or a pause switch, already sitting in LocalSky's own
/// storage from a standalone era, which a Home Assistant deployment stored
/// and never read, and which starts deciding with this release. That is the
/// one place a control that was NOT deciding begins to, so saying only "the
/// pause switch lives here now" would leave a hold nobody was told about.
///
/// A timed pause counts as a hold only while it is still running, judged
/// against the snapshot's own clock: the record is permanent, the hold is
/// not, and a notice claiming a hold months after it ended is a different
/// lie. Each hold line names the control it is released from by the label
/// the irrigation page uses: Rain delay for the timed pause, the Vacation
/// pause toggle for the switch.
fn hold_lines(recs: &[HaAdoptedHelper], now: i64, tz: &str) -> Vec<String> {
    let mut out = Vec::new();
    let live_epoch = |v: Option<&str>| v.and_then(|s| s.parse::<i64>().ok()).filter(|e| *e > now);
    if let Some(rec) = recs.iter().find(|h| h.entity == PAUSE_UNTIL) {
        match rec.outcome.as_str() {
            "adopted" => {
                if let Some(epoch) = live_epoch(rec.adopted_value.as_deref()) {
                    out.push(format!(
                        "Watering is held: the Rain delay you had set in {PAUSE_UNTIL} came \
                         across with it, {}. Release it under Rain delay on this page. Clearing \
                         the helper in Home Assistant will not release it.",
                        render_value(PAUSE_UNTIL, &epoch.to_string(), tz)
                    ));
                }
            }
            "kept_local" => {
                if let Some(epoch) = live_epoch(rec.previous_value.as_deref()) {
                    out.push(format!(
                        "Watering is held by a Rain delay LocalSky already had in its own \
                         storage from before this install talked to Home Assistant, {}. That \
                         value was stored but never read on a Home Assistant deployment, and it \
                         decides from this release. Release it under Rain delay on this page.",
                        render_value(PAUSE_UNTIL, &epoch.to_string(), tz)
                    ));
                }
            }
            _ => {}
        }
    }
    if let Some(rec) = recs.iter().find(|h| h.entity == PAUSE_TOGGLE) {
        out.push(match rec.outcome.as_str() {
            "adopted" if rec.adopted_value.as_deref() == Some("on") => {
                "Watering is held: the pause you had set in input_boolean.irrigation_pause came \
                 across with it. Clear it from the Vacation pause toggle on this page. Turning \
                 the helper back off in Home Assistant will not release it."
                    .to_string()
            }
            "kept_local" if rec.previous_value.as_deref() == Some("on") => {
                "Watering is held by the pause switch in LocalSky's own storage, set before this \
                 install talked to Home Assistant. That value was stored but never read on a \
                 Home Assistant deployment, and it decides from this release. Clear it from the \
                 Vacation pause toggle on this page."
                    .to_string()
            }
            _ => {
                "The pause switch is LocalSky's Vacation pause toggle on this page now.".to_string()
            }
        });
    }
    out
}

/// Only the helpers that were actually there can still exist, and only they
/// can be deleted.
fn still_exist_line(recs: &[HaAdoptedHelper]) -> Option<String> {
    let present: Vec<&HaAdoptedHelper> = recs.iter().filter(|h| was_present(h)).collect();
    let example = present
        .iter()
        .find(|h| h.entity == PAUSE_TOGGLE)
        .or(present.first())?;
    Some(format!(
        "The helpers listed above that exist in Home Assistant were not deleted and will not be. \
         They no longer do anything: writing to {} will not change anything here. Delete those \
         when you are ready, or leave them where they are.",
        example.entity
    ))
}

/// A control missing from the record set is still deciding, so it must not be
/// deleted. There are two reasons it can be missing and they need different
/// sentences.
///
/// No persistence database: the control has nowhere to land, so it can never
/// be adopted here and the owner has something to fix.
///
/// A DEFERRAL: the helper exists and was answering `unavailable`, `unknown` or
/// `restored` when the pass looked, which is what a Home Assistant restart or
/// a helpers reload looks like, so it was left alone while the rest of the set
/// committed. Nothing is wrong and there is nothing to do. Telling that owner
/// their /data mount is missing and to restart is false, and the restart it
/// prescribes resets the pass's own stability counter.
///
/// The records cannot tell the two apart, so the snapshot carries the bit.
fn still_live_line(recs: &[HaAdoptedHelper], controls_persisted: bool) -> Option<String> {
    let live: Vec<&str> = CONTROLS
        .iter()
        .copied()
        .filter(|e| !recs.iter().any(|h| h.entity == *e))
        .collect();
    if live.is_empty() {
        return None;
    }
    let cause = if controls_persisted {
        "These were not answering when LocalSky looked, which is what a Home Assistant restart \
         or a helpers reload looks like, so it left them alone rather than taking over a value \
         nobody set. It takes them over on its own as soon as they answer, with nothing for you \
         to do."
    } else {
        "LocalSky has no persistence database mounted, so it has nowhere to keep their values \
         and did not take them over. Mount /data and restart to finish."
    };
    Some(format!(
        "These helpers are still live and still deciding: {}. {cause} Do not delete these: \
         deleting {PAUSE_UNTIL} while a pause is set drops the pause with nothing on screen.",
        live.join(", ")
    ))
}

/// Only worth saying where an automation could be pointed at something that
/// exists.
fn automation_line(recs: &[HaAdoptedHelper]) -> Option<String> {
    if !recs.iter().any(was_present) {
        return None;
    }
    let controls_handled = CONTROLS.iter().all(|e| recs.iter().any(|h| h.entity == *e));
    let mut s = "If a Home Assistant automation writes to any of the helpers above, point it at \
                 LocalSky or it will stop having an effect with nothing to show for it. The three \
                 thresholds are number entities the LocalSky integration already publishes."
        .to_string();
    if controls_handled {
        s.push_str(
            " The pause, the one-day override and dry run are POST /api/irrigation/action with an \
             API token.",
        );
    }
    Some(s)
}

/// One helper's line. Names the entity, says what LocalSky uses now, and where
/// the value moved says both numbers and which one was in effect.
fn helper_line(h: &HaAdoptedHelper, tz: &str) -> String {
    let label = label_for(&h.entity);
    let holder = holder_for(&h.entity);
    let previous = h
        .previous_value
        .as_deref()
        .map(|v| render_value(&h.entity, v, tz))
        .unwrap_or_else(|| "nothing".to_string());
    match h.outcome.as_str() {
        "adopted" => {
            let now = h
                .adopted_value
                .as_deref()
                .map(|v| render_value(&h.entity, v, tz))
                .unwrap_or_else(|| "nothing".to_string());
            // A value LocalSky could not represent was still the value
            // deciding, so it is adopted at the nearest end of the range and
            // the line prints what the helper actually held.
            let clamped = h.observed_value.as_deref().map(|obs| {
                format!(
                    ", which held {}, outside what LocalSky can hold, so it is using the nearest \
                     value it can",
                    render_value(&h.entity, obs, tz)
                )
            });
            let clamped = clamped.unwrap_or_default();
            if h.changed_the_value() {
                format!(
                    "{label}: {now}, from {}{clamped}. {holder} showed {previous}; the helper was \
                     the one deciding.",
                    h.entity
                )
            } else {
                format!(
                    "{label}: {now}, from {}{clamped}. Same as {holder}; nothing changed.",
                    h.entity
                )
            }
        }
        "unreadable" => format!(
            "{label}: {} was there but could not be read. {holder} keeps {previous}.",
            h.entity
        ),
        "kept_local" => format!(
            "{label}: LocalSky keeps {previous}, its own value, and stops reading {}. The helper \
             was left over from before this install talked to Home Assistant.",
            h.entity
        ),
        // "not_found" and anything a later release records.
        _ => format!(
            "{label}: {} was not found. {holder} keeps {previous}.",
            h.entity
        ),
    }
}

/// The human name for a helper's knob.
fn label_for(entity: &str) -> &'static str {
    match entity {
        "input_number.irrigation_max_wind_mph" => "Max wind",
        "input_number.irrigation_min_temp_f" => "Min temp",
        "input_number.irrigation_rain_skip_in" => "Rain skip",
        // The labels the irrigation page uses for the two pause controls, so
        // the notice and the control it points at agree.
        "input_datetime.irrigation_pause_until" => "Rain delay",
        "input_select.irrigation_override_tomorrow" => "Tomorrow's override",
        "input_boolean.irrigation_pause" => "Vacation pause",
        "input_boolean.irrigation_dry_run" => "Dry run",
        _ => "This helper",
    }
}

/// Where LocalSky's own copy of this value lives, named the way the owner
/// would find it. The thresholds are on a Settings page; the four controls are
/// LocalSky's own storage with no page of their own.
fn holder_for(entity: &str) -> &'static str {
    if entity.starts_with("input_number.") {
        "Settings"
    } else {
        "LocalSky"
    }
}

/// A recorded value as a person reads it. The three thresholds stay imperial,
/// matching the Settings sliders they round-trip through, so the notice and
/// the page it points at print the same number.
fn render_value(entity: &str, raw: &str, tz: &str) -> String {
    match entity {
        "input_number.irrigation_max_wind_mph" => format!("{raw} mph"),
        "input_number.irrigation_min_temp_f" => format!("{raw} F"),
        "input_number.irrigation_rain_skip_in" => format!("{raw} in"),
        "input_datetime.irrigation_pause_until" => match raw.parse::<i64>() {
            Ok(0) => "no pause".to_string(),
            Ok(epoch) => format!(
                "paused until {}, {} {}",
                crate::timefmt::format_wday_full(epoch, tz),
                crate::timefmt::format_md(epoch, tz),
                crate::timefmt::format_hm(epoch, tz)
            ),
            Err(_) => raw.to_string(),
        },
        "input_select.irrigation_override_tomorrow" => match raw {
            "none" => "no override".to_string(),
            other => other.to_string(),
        },
        _ => raw.to_string(),
    }
}

/// Stable identity for the set this notice is about. Dismissal stores it, so
/// silencing one migration never silences a different one: if a later release
/// retires another read, the key changes and the notice speaks up again.
fn adopted_key(s: &IrrigationSnapshot) -> String {
    if awaiting_config(s) {
        // Distinct from the empty set's key, which is what "nothing to show"
        // looks like: the empty key would read as already dismissed.
        return "awaiting-config".to_string();
    }
    let mut parts: Vec<String> = s
        .ha_adoption
        .iter()
        .map(|h| {
            format!(
                "{}:{}:{}",
                h.entity,
                h.outcome,
                h.adopted_value.as_deref().unwrap_or("")
            )
        })
        .collect();
    parts.sort();
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(
        entity: &str,
        outcome: &str,
        adopted: Option<&str>,
        previous: Option<&str>,
    ) -> HaAdoptedHelper {
        HaAdoptedHelper {
            entity: entity.to_string(),
            outcome: outcome.to_string(),
            target: String::new(),
            adopted_value: adopted.map(str::to_string),
            observed_value: None,
            previous_value: previous.map(str::to_string),
            epoch: 1,
        }
    }

    /// The full seven, all present and adopted, as a working install produces.
    fn every_helper_adopted() -> Vec<HaAdoptedHelper> {
        vec![
            rec(
                "input_number.irrigation_max_wind_mph",
                "adopted",
                Some("12"),
                Some("10"),
            ),
            rec(
                "input_number.irrigation_min_temp_f",
                "adopted",
                Some("38"),
                Some("38"),
            ),
            rec(
                "input_number.irrigation_rain_skip_in",
                "adopted",
                Some("0.25"),
                Some("0.25"),
            ),
            rec(
                "input_datetime.irrigation_pause_until",
                "adopted",
                Some("0"),
                Some("0"),
            ),
            rec(
                "input_select.irrigation_override_tomorrow",
                "adopted",
                Some("none"),
                Some("none"),
            ),
            rec(
                "input_boolean.irrigation_pause",
                "adopted",
                Some("off"),
                Some("off"),
            ),
            rec(
                "input_boolean.irrigation_dry_run",
                "adopted",
                Some("off"),
                Some("off"),
            ),
        ]
    }

    // A value that moved has to print both numbers and say which one was
    // deciding, or the owner reads the new number as the one they set.
    #[test]
    fn a_moved_threshold_prints_both_numbers_and_names_the_one_in_effect() {
        let line = helper_line(
            &rec(
                "input_number.irrigation_max_wind_mph",
                "adopted",
                Some("12"),
                Some("10"),
            ),
            "UTC",
        );
        assert_eq!(
            line,
            "Max wind: 12 mph, from input_number.irrigation_max_wind_mph. \
             Settings showed 10 mph; the helper was the one deciding."
        );
    }

    // A helper set to 99 mph to switch the wind gate off was the number
    // deciding. LocalSky adopts it at the nearest value it can hold and has to
    // print what the helper actually said, or the owner reads 50 as a number
    // somebody chose.
    #[test]
    fn a_clamped_threshold_prints_the_value_the_helper_actually_held() {
        let mut h = rec(
            "input_number.irrigation_max_wind_mph",
            "adopted",
            Some("50"),
            Some("10"),
        );
        h.observed_value = Some("99".into());
        let line = helper_line(&h, "UTC");
        assert_eq!(
            line,
            "Max wind: 50 mph, from input_number.irrigation_max_wind_mph, which held 99 mph, \
             outside what LocalSky can hold, so it is using the nearest value it can. Settings \
             showed 10 mph; the helper was the one deciding."
        );
    }

    // Silence on an unchanged value reads as an omission, so it says so.
    #[test]
    fn an_unchanged_value_says_nothing_changed_rather_than_staying_quiet() {
        let line = helper_line(
            &rec(
                "input_number.irrigation_min_temp_f",
                "adopted",
                Some("38"),
                Some("38"),
            ),
            "UTC",
        );
        assert_eq!(
            line,
            "Min temp: 38 F, from input_number.irrigation_min_temp_f. \
             Same as Settings; nothing changed."
        );
    }

    #[test]
    fn a_helper_that_was_never_there_names_the_value_localsky_keeps() {
        let line = helper_line(
            &rec(
                "input_number.irrigation_rain_skip_in",
                "not_found",
                None,
                Some("0.25"),
            ),
            "UTC",
        );
        assert_eq!(
            line,
            "Rain skip: input_number.irrigation_rain_skip_in was not found. \
             Settings keeps 0.25 in."
        );
    }

    // Present but unreadable is a different sentence from absent, because
    // telling somebody an entity they can see in Home Assistant "was not
    // found" is not true.
    #[test]
    fn an_unreadable_helper_is_not_described_as_missing() {
        let line = helper_line(
            &rec(
                "input_select.irrigation_override_tomorrow",
                "unreadable",
                None,
                Some("none"),
            ),
            "UTC",
        );
        assert_eq!(
            line,
            "Tomorrow's override: input_select.irrigation_override_tomorrow was there \
             but could not be read. LocalSky keeps no override."
        );
    }

    #[test]
    fn a_live_pause_reads_as_a_date_and_an_empty_one_as_no_pause() {
        // 2026-09-04 06:00 UTC.
        let epoch = 1_788_501_600_i64;
        let live = render_value(
            "input_datetime.irrigation_pause_until",
            &epoch.to_string(),
            "UTC",
        );
        assert!(live.starts_with("paused until "), "{live}");
        assert_eq!(
            render_value("input_datetime.irrigation_pause_until", "0", "UTC"),
            "no pause"
        );
    }

    // The banner compiles for the browser, where the planner's module does
    // not, so it carries its own copy of the entity ids. This is what stops
    // the two drifting into a notice that says "This helper" about something
    // the migration definitely handled.
    #[cfg(feature = "ssr")]
    #[test]
    fn every_retired_helper_has_a_label_of_its_own() {
        for id in crate::ha_adopt::ENTITIES {
            assert_ne!(label_for(id), "This helper", "no label for {id}");
        }
        assert_eq!(
            CONTROLS.len(),
            crate::ha_adopt::CONTROL_ENTITIES.len(),
            "the browser copy of the control list has drifted"
        );
        for id in crate::ha_adopt::CONTROL_ENTITIES {
            assert!(CONTROLS.contains(&id), "{id} missing from the browser copy");
        }
    }

    #[test]
    fn nothing_adopted_says_nothing() {
        let s = IrrigationSnapshot::default();
        assert!(!worth_showing(&s));
    }

    // The install where every helper was absent is the one the notice used to
    // be silent about, and it is the one where the pass concluded the most
    // from the least: seven reads retired, four controls handed to LocalSky.
    #[test]
    fn an_install_with_no_helpers_at_all_is_still_told_what_changed() {
        let mut s = IrrigationSnapshot::default();
        s.ha_adoption = vec![
            rec(
                "input_number.irrigation_max_wind_mph",
                "not_found",
                None,
                Some("10"),
            ),
            rec(
                "input_boolean.irrigation_pause",
                "not_found",
                None,
                Some("off"),
            ),
        ];
        assert!(worth_showing(&s));
        let head = header_line(&s.ha_adoption);
        assert!(
            head.contains("None of them were in Home Assistant"),
            "the header must not claim it read helpers that were not there: {head}"
        );
        assert_eq!(
            still_exist_line(&s.ha_adoption),
            None,
            "nothing exists, so nothing is invited to be deleted"
        );
        assert_eq!(automation_line(&s.ha_adoption), None);
    }

    // The specific hazard finding 18 names: with no persistence database the
    // four controls were never taken over, so telling the owner they are dead
    // and inviting deletion drops a live pause.
    #[test]
    fn a_no_database_install_is_told_which_helpers_are_still_live() {
        let recs: Vec<HaAdoptedHelper> = every_helper_adopted()
            .into_iter()
            .filter(|h| h.entity.starts_with("input_number"))
            .collect();
        let live = still_live_line(&recs, false).expect("the four controls are still live");
        for id in CONTROLS {
            assert!(live.contains(id), "{id} not named as still live");
        }
        assert!(live.contains("Do not delete these"), "{live}");
        assert!(live.contains("Mount /data and restart"), "{live}");
        let exists = still_exist_line(&recs).unwrap();
        assert!(
            !CONTROLS.iter().any(|c| exists.contains(c)),
            "the delete invitation must never name a helper that is still deciding: {exists}"
        );
        let auto = automation_line(&recs).unwrap();
        assert!(
            !auto.contains("POST /api/irrigation/action"),
            "the controls were not handled, so there is nothing to repoint at that endpoint"
        );
    }

    // The mirror image of the no-database case, and the one the hardcoded
    // diagnosis got wrong. A control that DEFERRED is absent from the record
    // set too, while the other six commit in the same pass, so an install with
    // /data mounted and a Home Assistant mid-helpers-reload was told its mount
    // was missing and to restart. The restart resets the pass's own stability
    // counter, so the advice made it worse.
    #[test]
    fn a_deferred_control_is_not_told_to_mount_data() {
        let recs: Vec<HaAdoptedHelper> = every_helper_adopted()
            .into_iter()
            .filter(|h| h.entity != PAUSE_TOGGLE)
            .collect();
        let live = still_live_line(&recs, true).expect("the deferred control is still live");
        assert!(live.contains(PAUSE_TOGGLE), "{live}");
        assert!(
            !live.contains("no persistence database") && !live.contains("Mount /data"),
            "an install with /data mounted must never be told to mount /data: {live}"
        );
        assert!(
            live.contains("Do not delete these"),
            "the helper is still deciding either way: {live}"
        );
    }

    // And on a full migration the notice does invite deletion, names an
    // entity that really exists, and points automations at the action
    // endpoint.
    #[test]
    fn a_complete_migration_invites_deletion_and_names_a_real_entity() {
        let recs = every_helper_adopted();
        assert_eq!(still_live_line(&recs, true), None);
        let exists = still_exist_line(&recs).unwrap();
        assert!(
            exists.contains("input_boolean.irrigation_pause"),
            "{exists}"
        );
        assert!(
            exists.contains("Delete those when you are ready"),
            "{exists}"
        );
        assert!(automation_line(&recs)
            .unwrap()
            .contains("POST /api/irrigation/action"));
    }

    // The release notes told owners to set the pause helper before upgrading.
    // Adoption brings that hold across, and turning the helper back off does
    // nothing, so the notice has to say where the off switch went.
    #[test]
    fn a_pause_that_came_across_says_where_to_clear_it() {
        let mut recs = every_helper_adopted();
        for h in recs.iter_mut() {
            if h.entity == PAUSE_TOGGLE {
                h.adopted_value = Some("on".into());
                h.previous_value = Some("off".into());
            }
        }
        let lines = hold_lines(&recs, 1, "UTC");
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert!(line.starts_with("Watering is held"), "{line}");
        assert!(line.contains("Vacation pause toggle"), "{line}");
        assert!(
            line.contains("will not release it"),
            "the owner has to be told the helper cannot clear it: {line}"
        );
        // And when nothing was held, it just says where the switch lives.
        let quiet = hold_lines(&every_helper_adopted(), 1, "UTC");
        assert_eq!(
            quiet,
            vec!["The pause switch is LocalSky's Vacation pause toggle on this page now."]
        );
    }

    // The pre-upgrade Rain delay the release notes recommend comes across
    // `adopted`, and the notice has to say the yard is held and that the
    // hold is released under Rain delay, not by clearing the helper. Once
    // the pause has run out the record is still there and the hold is not.
    #[test]
    fn an_adopted_rain_delay_says_the_yard_is_held_and_where_to_release_it() {
        let mut recs = every_helper_adopted();
        for h in recs.iter_mut() {
            if h.entity == PAUSE_UNTIL {
                h.adopted_value = Some("1900000000".into());
                h.previous_value = Some("0".into());
            }
        }
        let lines = hold_lines(&recs, 1_800_000_000, "UTC");
        assert_eq!(lines.len(), 2, "{lines:?}");
        let held = &lines[0];
        assert!(
            held.starts_with("Watering is held: the Rain delay"),
            "{held}"
        );
        assert!(held.contains("paused until"), "{held}");
        assert!(held.contains("Release it under Rain delay"), "{held}");
        assert!(held.contains("will not release it"), "{held}");
        assert!(
            !held.contains("Vacation pause"),
            "the timed pause is not the toggle: {held}"
        );
        // Expired: the record stays, the hold line goes.
        let later = hold_lines(&recs, 1_900_000_001, "UTC");
        assert_eq!(later.len(), 1, "{later:?}");
        assert!(later[0].starts_with("The pause switch is"), "{later:?}");
    }

    // Both holds at once are both named.
    #[test]
    fn a_rain_delay_and_a_pause_switch_hold_are_both_named() {
        let mut recs = every_helper_adopted();
        for h in recs.iter_mut() {
            if h.entity == PAUSE_UNTIL {
                h.adopted_value = Some("1900000000".into());
            }
            if h.entity == PAUSE_TOGGLE {
                h.adopted_value = Some("on".into());
            }
        }
        let lines = hold_lines(&recs, 1, "UTC");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].contains("Rain delay"), "{}", lines[0]);
        assert!(lines[1].contains("Vacation pause toggle"), "{}", lines[1]);
    }

    // The other hold, and the one nothing used to say: a pause or a pause
    // switch already in LocalSky's own storage from a standalone era. A Home
    // Assistant deployment stored those and never read them, so they start
    // deciding with this release, and the reassuring "the switch lives here
    // now" line would have covered a yard that is now held.
    #[test]
    fn a_kept_local_hold_is_named_rather_than_covered_by_the_quiet_line() {
        let mut recs = every_helper_adopted();
        for h in recs.iter_mut() {
            if h.entity == PAUSE_UNTIL {
                h.outcome = "kept_local".into();
                h.adopted_value = None;
                h.previous_value = Some("1900000000".into());
            }
        }
        let lines = hold_lines(&recs, 1, "UTC");
        let line = &lines[0];
        assert!(
            line.starts_with("Watering is held by a Rain delay"),
            "{line}"
        );
        assert!(line.contains("Release it under Rain delay"), "{line}");

        let mut recs = every_helper_adopted();
        for h in recs.iter_mut() {
            if h.entity == PAUSE_TOGGLE {
                h.outcome = "kept_local".into();
                h.adopted_value = None;
                h.previous_value = Some("on".into());
            }
        }
        let lines = hold_lines(&recs, 1, "UTC");
        let line = lines
            .iter()
            .find(|l| l.contains("pause switch"))
            .expect("the switch hold is named");
        assert!(
            line.starts_with("Watering is held by the pause switch"),
            "{line}"
        );
        assert!(line.contains("Vacation pause toggle"), "{line}");
    }

    // An install with no config file has not run the migration at all, and
    // every helper is still deciding. The notice must speak there, and the
    // empty-set dismissal key must not silence it.
    #[test]
    fn an_install_with_no_config_file_is_told_the_helpers_still_decide() {
        let mut s = IrrigationSnapshot::default();
        assert!(!worth_showing(&s));
        s.ha_adoption_awaiting_config = true;
        assert!(worth_showing(&s));
        assert_ne!(
            adopted_key(&s),
            "",
            "the empty key would read as already dismissed"
        );
        let line = awaiting_config_line();
        assert!(line.contains("localsky.toml"), "{line}");
        assert!(line.contains("still deciding"), "{line}");
        assert!(line.contains("Do not delete them"), "{line}");
    }

    #[test]
    fn the_dismissal_key_covers_the_exact_set_and_is_order_stable() {
        let mut s = IrrigationSnapshot::default();
        assert_eq!(adopted_key(&s), "", "nothing adopted, nothing to silence");
        s.ha_adoption = vec![
            rec(
                "input_number.irrigation_min_temp_f",
                "adopted",
                Some("38"),
                None,
            ),
            rec(
                "input_number.irrigation_max_wind_mph",
                "adopted",
                Some("12"),
                None,
            ),
        ];
        let key = adopted_key(&s);
        assert_eq!(
            key,
            "input_number.irrigation_max_wind_mph:adopted:12,\
             input_number.irrigation_min_temp_f:adopted:38"
        );
        // A later release retiring another read produces a different key, so
        // an old dismissal cannot silence a new migration.
        s.ha_adoption.push(rec(
            "input_boolean.irrigation_pause",
            "adopted",
            Some("off"),
            None,
        ));
        assert_ne!(key, adopted_key(&s));
    }
}
