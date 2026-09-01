// The one-time Home Assistant helper adoption pass (0.7.22).
//
// Seven helper entities decided things LocalSky can decide for itself: three
// skip thresholds that OUTRANKED the matching values in Settings, and four
// operator controls whose native store the Home Assistant path never
// consulted. This pass reads each of them once, writes the value into
// LocalSky's own storage, records what it did, and from that moment the read
// is bypassed and LocalSky's value governs.
//
// The rules differ between the two classes, and the difference is the whole
// safety argument.
//
// A THRESHOLD retires on any answer. Its read was
// `helper_value.unwrap_or(config_value)`, so a missing, unavailable or
// unparseable helper already resolved to the config value, and retiring it
// changes nothing at all. A helper holding a number outside what LocalSky can
// represent IS adopted, clamped to the nearest value LocalSky can hold, and
// the record carries both numbers: that helper WAS the number deciding, and
// reverting it to the config value would move the threshold without saying so.
//
// A CONTROL has three outcomes, not two, because before 0.7.22 the Home
// Assistant path never wrote the native control store: those columns hold
// M0017 defaults nobody chose. Retiring a control read on absence therefore
// swaps a PROTECTED gate from "the helper says paused" to "a fresh column
// default says not paused".
//
//   PRESENT and parseable: the value is written into the store and the read
//   retires.
//
//   PRESENT and holding `unavailable`, `unknown`, nothing, or carrying Home
//   Assistant's `restored` marker: DEFERRED. Nothing is recorded and the read
//   stays live. `unavailable` is positive evidence that the helper EXISTS and
//   is momentarily broken (a helpers reload, a restore from backup), and it
//   is a STABLE answer, so the stability window gives no protection against
//   it at all. Deferring costs nothing: an unadopted control behaves exactly
//   as it does today, and a later tick adopts it once Home Assistant has
//   finished coming up.
//
//   ABSENT: recorded `not_found` and the read retires, but only once the
//   caller has strong evidence that Home Assistant finished starting. See
//   `refresher::adopt_tick`: the entity count rides in the stability
//   fingerprint, so a still-registering Home Assistant resets the counter,
//   and a missing control helper is held to a five minute window rather than
//   twenty seconds. That matters most on the Home Assistant OS add-on, where
//   LocalSky and Home Assistant restart together on every host reboot and the
//   supervisor starts the add-on before Home Assistant finishes bootstrapping,
//   so an all-absent answer during startup is common rather than theoretical.
//
// LOCALSKY'S OWN ANSWER OUTRANKS A LEGACY HELPER. A control column holding a
// non-default value can only have been written natively, because the Home
// Assistant path never wrote it. That is a more recent answer than any helper
// left over from an earlier standalone install that later gained `HA_URL`, so
// the record reads `kept_local` and the store's value stands. A pause that has
// already ended is the exception: nothing ever clears that column, so a past
// epoch is a fossil rather than an answer and a live helper pause is adopted
// over it.
//
// IDEMPOTENCY IS BY ENTITY ID. The pass consults `cfg.ha_adoption` and never
// a config value. `FileConfigStore::save` serializes every default out
// explicitly, so a value that looks like the default proves nothing about
// whether a human typed it. This is the rule `seeded_source_ids` already
// follows, for the same reason.
//
// There is no watering hold. An entity that has not been adopted is still
// read exactly as it was before, so a Home Assistant outage during the
// upgrade is the same non-event it has always been: the refresher keeps the
// last good snapshot and the yard waters on the values it already had.

use std::collections::HashMap;

use serde_json::Value;

use crate::config::schema::Config;
use crate::ha::snapshot::HaAdoptedHelper;
use crate::persistence::IrrigationControlState;

/// The three skip thresholds. Home Assistant outranked Settings for these,
/// so their values are the ones that were actually deciding.
pub const MAX_WIND: &str = "input_number.irrigation_max_wind_mph";
pub const MIN_TEMP: &str = "input_number.irrigation_min_temp_f";
pub const RAIN_SKIP: &str = "input_number.irrigation_rain_skip_in";
/// The four operator controls. Their native store existed and the Home
/// Assistant path passed `None` for it, so it was unreachable.
pub const PAUSE_UNTIL: &str = "input_datetime.irrigation_pause_until";
pub const OVERRIDE_TOMORROW: &str = "input_select.irrigation_override_tomorrow";
pub const PAUSE_TOGGLE: &str = "input_boolean.irrigation_pause";
pub const DRY_RUN_TOGGLE: &str = "input_boolean.irrigation_dry_run";

/// Every entity this pass handles, in the order the notice lists them.
pub const ENTITIES: [&str; 7] = [
    MAX_WIND,
    MIN_TEMP,
    RAIN_SKIP,
    PAUSE_UNTIL,
    OVERRIDE_TOMORROW,
    PAUSE_TOGGLE,
    DRY_RUN_TOGGLE,
];

/// The four entities that need a control store to land anywhere.
pub const CONTROL_ENTITIES: [&str; 4] =
    [PAUSE_UNTIL, OVERRIDE_TOMORROW, PAUSE_TOGGLE, DRY_RUN_TOGGLE];

/// Accepted range per threshold, matching what `POST /action set_threshold`
/// accepts and what the manifest publishes on its `number` descriptors. A
/// helper outside it is still adopted, clamped to the nearest end: it was the
/// value deciding, and 99 mph clamped to 50 still means "effectively never
/// wind-skip", where reverting to the config value would quietly start
/// skipping.
///
/// The wind ceiling is 50 because that is the top of the slider the shipping
/// Home Assistant integration builds for `number.localsky_max_wind_mph` from
/// its own fixed limits (0 to 50 mph, 20 to 60 F, 0 to 1 in). A server bound
/// below it answered 400 to a value LocalSky's own entity offered; the other
/// two integration ranges sit inside the server's.
const MAX_WIND_RANGE: (f64, f64) = (0.0, 50.0);
const MIN_TEMP_RANGE: (f64, f64) = (20.0, 70.0);
/// The rain threshold is a free numeric input rather than a slider, so the
/// bound is physical rather than editorial: ten inches of rain in a day is
/// already past any threshold anyone means.
const RAIN_SKIP_RANGE: (f64, f64) = (0.0, 10.0);

/// Step published alongside the range on the `number` descriptors, so the
/// integration builds each threshold entity on the bound the server enforces
/// rather than on the Home Assistant platform defaults.
pub fn threshold_step(key: &str) -> Option<f64> {
    match key {
        "max_wind_mph" | "min_temp_f" => Some(1.0),
        "rain_skip_in" => Some(0.05),
        _ => None,
    }
}

pub const OUTCOME_ADOPTED: &str = "adopted";
pub const OUTCOME_NOT_FOUND: &str = "not_found";
pub const OUTCOME_UNREADABLE: &str = "unreadable";
/// LocalSky's own store already held an operator answer for this control, so
/// the helper's value was not taken. The read still retires.
pub const OUTCOME_KEPT_LOCAL: &str = "kept_local";
/// What one pass would do. Produced purely from the entity map, the config
/// and the control state, so every interesting decision is testable without
/// Home Assistant, SQLite or a clock.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdoptionPlan {
    /// One record per entity this pass is handling now, in `ENTITIES` order.
    /// An entity already in `cfg.ha_adoption` produces nothing.
    pub records: Vec<HaAdoptedHelper>,
    /// Threshold values to write into `engine.skip_rules`. `None` means the
    /// helper was not adopted and the config value stands.
    pub max_wind_mph: Option<f64>,
    pub min_temp_f: Option<f64>,
    pub rain_skip_in: Option<f64>,
    /// Control values to write into `irrigation_control`. `None` means not
    /// adopted, and the stored value stands.
    pub pause_until_epoch: Option<i64>,
    pub override_tomorrow: Option<String>,
    pub is_paused: Option<bool>,
    pub is_dry_run: Option<bool>,
    /// At least one control was present and holding nothing usable, so it was
    /// left unrecorded and its read is still live. The caller stays armed and
    /// re-earns its evidence rather than concluding anything.
    pub deferred: bool,
}

impl AdoptionPlan {
    /// Nothing left to record. The caller disarms the pass on this, unless
    /// something deferred.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// True when the plan writes at least one control value, i.e. the commit
    /// has to reach SQLite, durably, before it may record anything.
    pub fn writes_controls(&self) -> bool {
        self.pause_until_epoch.is_some()
            || self.override_tomorrow.is_some()
            || self.is_paused.is_some()
            || self.is_dry_run.is_some()
    }
}

/// Stable text of the seven entities' current answers. Compared across ticks
/// so nothing is concluded from a Home Assistant that is answering but has
/// not finished restoring: a registry mid-restore gives a different string on
/// tick one than on tick three, which resets the caller's counter. The caller
/// folds the total entity count in on top, because a Home Assistant still
/// registering entities answers `<absent>` for all seven identically while
/// its count climbs.
pub fn fingerprint(map: &HashMap<String, Value>) -> String {
    let mut out = String::new();
    for id in ENTITIES {
        out.push_str(id);
        out.push('=');
        match map.get(id) {
            None => out.push_str("<absent>"),
            Some(v) => {
                out.push_str(state_str(v).unwrap_or("<no-state>"));
                // The vacation pause carries its value in an attribute, so
                // two different pauses can share a state string.
                if id == PAUSE_UNTIL {
                    out.push('@');
                    match timestamp_attr(v) {
                        Some(t) => out.push_str(&t.to_string()),
                        None => out.push_str("<none>"),
                    }
                }
            }
        }
        out.push(';');
    }
    out
}

/// Plan the pass. `control` is `None` when no persistence DB is mounted: the
/// four operator controls then have nowhere to land, so they are left
/// unhandled and their reads stay live. The three thresholds still adopt,
/// because their sink is the config file.
pub fn plan(
    map: &HashMap<String, Value>,
    cfg: &Config,
    control: Option<&IrrigationControlState>,
    now_epoch: i64,
) -> AdoptionPlan {
    let mut plan = AdoptionPlan::default();
    let done = |id: &str| cfg.ha_adoption.iter().any(|h| h.entity == id);

    for id in ENTITIES {
        if done(id) {
            continue;
        }
        if CONTROL_ENTITIES.contains(&id) && control.is_none() {
            // No store, no sink. Leaving it unrecorded keeps the entity read
            // live, which is exactly today's behavior, and lets a later boot
            // with a mounted DB adopt it properly.
            continue;
        }
        let Some(read) = read_entity(map, id, cfg, control) else {
            // A control that is present and holding nothing usable. Not an
            // answer, so not a conclusion: the read stays live and the caller
            // stays armed.
            plan.deferred = true;
            continue;
        };
        // LocalSky's own store already holds an operator answer for this
        // control, so the helper does not get to overwrite it. The read
        // retires either way: the store is the authority from here on.
        let keep_local = native_answer_present(id, control, now_epoch);
        if !keep_local {
            match (&read.value, id) {
                (Adopted::Number(v), MAX_WIND) => plan.max_wind_mph = Some(*v),
                (Adopted::Number(v), MIN_TEMP) => plan.min_temp_f = Some(*v),
                (Adopted::Number(v), RAIN_SKIP) => plan.rain_skip_in = Some(*v),
                (Adopted::Epoch(v), PAUSE_UNTIL) => plan.pause_until_epoch = Some(*v),
                (Adopted::Text(v), OVERRIDE_TOMORROW) => plan.override_tomorrow = Some(v.clone()),
                (Adopted::Flag(v), PAUSE_TOGGLE) => plan.is_paused = Some(*v),
                (Adopted::Flag(v), DRY_RUN_TOGGLE) => plan.is_dry_run = Some(*v),
                _ => {}
            }
        }
        plan.records.push(HaAdoptedHelper {
            entity: id.to_string(),
            outcome: if keep_local {
                OUTCOME_KEPT_LOCAL.to_string()
            } else {
                read.outcome.to_string()
            },
            target: target_of(id).to_string(),
            adopted_value: if keep_local { None } else { read.adopted_text },
            observed_value: if keep_local { None } else { read.observed_text },
            previous_value: Some(read.previous_text),
            epoch: now_epoch,
        });
    }

    plan
}

/// True when LocalSky's own store already holds a LIVE operator answer for
/// this control. The Home Assistant path never wrote these four columns
/// before 0.7.22, so a non-default value here can only have come from a
/// native write, which is a more recent answer than any legacy helper left
/// over from a standalone install that later gained `HA_URL`.
///
/// "Non-default" is not the same as "still an answer". `override_tomorrow` is
/// day-stamped, so `get_on` already resolves a stale one to "none" before it
/// reaches here; `pause_until_epoch` carries its own expiry in the value and
/// has to be compared against the clock here, or a spent pause outranks a
/// live helper forever.
fn native_answer_present(
    id: &str,
    control: Option<&IrrigationControlState>,
    now_epoch: i64,
) -> bool {
    let Some(c) = control else {
        return false;
    };
    match id {
        // A pause that has already ended is not an operator answer. Nothing
        // ever zeroes this column: `set_pause_until` is its only writer and
        // the read applies no expiry, so a past epoch is the fossil of a
        // vacation that ended months ago and means exactly what the 0 default
        // means. Reading it as a native write would discard a live helper
        // pause and retire the read, and the yard would water through the
        // vacation the owner is actually on. Only a pause still running
        // outranks the helper.
        PAUSE_UNTIL => c.pause_until_epoch > now_epoch,
        OVERRIDE_TOMORROW => c.override_tomorrow != "none",
        PAUSE_TOGGLE => c.is_paused,
        DRY_RUN_TOGGLE => c.is_dry_run,
        _ => false,
    }
}

/// Write the plan's threshold values and its records into `cfg`. The control
/// values go to SQLite separately, and before this, so a crash between the
/// two leaves the controls written and nothing marked: the next tick redoes
/// the identical writes and marks then. The reverse order would retire a read
/// whose value never landed.
pub fn apply(plan: &AdoptionPlan, cfg: &mut Config) {
    if let Some(v) = plan.max_wind_mph {
        cfg.engine.skip_rules.max_wind_mph = v;
    }
    if let Some(v) = plan.min_temp_f {
        cfg.engine.skip_rules.min_temp_f = v;
    }
    if let Some(v) = plan.rain_skip_in {
        cfg.engine.skip_rules.rain_skip_in = v;
    }
    for r in &plan.records {
        if !cfg.ha_adoption.iter().any(|h| h.entity == r.entity) {
            cfg.ha_adoption.push(r.clone());
        }
    }
}

/// The helper entity behind a threshold key ("max_wind_mph"), so the write
/// path and the read path consult the same marker for the same knob.
pub fn threshold_entity(key: &str) -> Option<&'static str> {
    match key {
        "max_wind_mph" => Some(MAX_WIND),
        "min_temp_f" => Some(MIN_TEMP),
        "rain_skip_in" => Some(RAIN_SKIP),
        _ => None,
    }
}

/// The accepted range for a threshold key. Shared with the write path, with
/// the Settings editor's own bounds, and with the manifest, so a value
/// LocalSky refuses to be given is also the bound its `number` entity carries.
pub fn threshold_range(key: &str) -> Option<(f64, f64)> {
    match key {
        "max_wind_mph" => Some(MAX_WIND_RANGE),
        "min_temp_f" => Some(MIN_TEMP_RANGE),
        "rain_skip_in" => Some(RAIN_SKIP_RANGE),
        _ => None,
    }
}

/// The helper entity behind a toggle key ("irrigation_pause").
pub fn toggle_entity(key: &str) -> Option<&'static str> {
    match key {
        "irrigation_pause" => Some(PAUSE_TOGGLE),
        "irrigation_dry_run" => Some(DRY_RUN_TOGGLE),
        _ => None,
    }
}

/// Where each entity's value lives now. Recorded so the config file answers
/// "why is this number what it is" without anyone reading the source.
pub fn target_of(entity: &str) -> &'static str {
    match entity {
        MAX_WIND => "engine.skip_rules.max_wind_mph",
        MIN_TEMP => "engine.skip_rules.min_temp_f",
        RAIN_SKIP => "engine.skip_rules.rain_skip_in",
        PAUSE_UNTIL => "irrigation_control.pause_until_epoch",
        OVERRIDE_TOMORROW => "irrigation_control.override_tomorrow",
        PAUSE_TOGGLE => "irrigation_control.is_paused",
        DRY_RUN_TOGGLE => "irrigation_control.is_dry_run",
        _ => "",
    }
}

/// Bumped before every pre-adoption write to a Home Assistant helper this
/// pass adopts. The commit refuses to plan from an answer set taken before a
/// write that has since landed: the owner taps Rain delay at 05:59, the
/// handler writes `input_datetime.irrigation_pause_until` because the read is
/// not retired yet, and a commit planning from the pre-write answer would
/// write the OLD value into SQLite and then retire the read, losing the pause
/// with a 200 on screen.
pub static PREADOPT_WRITE_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Read the counter. Sampled once before the states() call that produces an
/// answer set, and again at commit time.
pub fn write_seq() -> u64 {
    PREADOPT_WRITE_SEQ.load(std::sync::atomic::Ordering::SeqCst)
}

/// Bump the counter. Called BEFORE the service call fires, not after: a write
/// already in flight when the answer set was taken must still force the
/// commit to re-earn its evidence.
pub fn note_preadopt_write() {
    PREADOPT_WRITE_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// The counter is one process-global, and cargo runs the test binary's tests
/// concurrently. A `HelperReadout` constructor samples it, `adopt_tick`
/// compares against the live value at commit time, and another test's
/// `note_preadopt_write` landing in between makes the first test defer and
/// fail an assertion that has nothing to do with what it is testing. Every
/// test that samples or bumps the counter holds this for the span between its
/// sample and its assertion.
///
/// An async mutex rather than `std`: the tests that need it hold it across
/// `.await` points, and it does not poison, so one failing test cannot cascade
/// into the rest.
#[cfg(test)]
pub static SEQ_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ─────────────────────────────────────────────────────────────────────
// Reading one entity
// ─────────────────────────────────────────────────────────────────────

enum Adopted {
    None,
    Number(f64),
    Epoch(i64),
    Text(String),
    Flag(bool),
}

struct Read {
    outcome: &'static str,
    value: Adopted,
    /// The adopted value as text, for the record. None unless adopted.
    adopted_text: Option<String>,
    /// What the helper actually held, when that differs from what was
    /// adopted. Set only where a threshold was clamped into range, so the
    /// notice can print both numbers.
    observed_text: Option<String>,
    /// What LocalSky held before, as text. Always present: on a
    /// non-adoption it is the value LocalSky goes on using.
    previous_text: String,
}

/// Present, but this is not an answer: `unavailable`, `unknown`, empty, or
/// carrying Home Assistant's `restored` marker, which is what a
/// registry-restored entity reports until its platform finishes setting up.
fn state_is_transient(entity: &Value) -> bool {
    if entity
        .get("attributes")
        .and_then(|a| a.get("restored"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        return true;
    }
    !matches!(state_str(entity), Some(s) if !matches!(s, "unavailable" | "unknown" | ""))
}

/// Read one entity. `None` means DEFER: the caller records nothing and the
/// read stays live. Only a control defers, and only while it is present and
/// holding nothing usable.
fn read_entity(
    map: &HashMap<String, Value>,
    id: &str,
    cfg: &Config,
    control: Option<&IrrigationControlState>,
) -> Option<Read> {
    let previous_text = previous_text(id, cfg, control);
    let unreadable = |previous_text: String| Read {
        outcome: OUTCOME_UNREADABLE,
        value: Adopted::None,
        adopted_text: None,
        observed_text: None,
        previous_text,
    };
    let Some(entity) = map.get(id) else {
        return Some(Read {
            outcome: OUTCOME_NOT_FOUND,
            value: Adopted::None,
            adopted_text: None,
            observed_text: None,
            previous_text,
        });
    };
    if state_is_transient(entity) {
        if CONTROL_ENTITIES.contains(&id) {
            // The helper exists and is momentarily broken. Retiring the read
            // here would move a protected gate onto a column no human wrote,
            // and `unavailable` is a STABLE answer, so the stability window
            // is no protection at all. Wait it out instead.
            return None;
        }
        // A threshold read was `helper.unwrap_or(config)`, so an unusable
        // helper already resolved to the config value. Retiring changes
        // nothing.
        return Some(unreadable(previous_text));
    }
    let Some(state) = state_str(entity) else {
        return Some(unreadable(previous_text));
    };

    match id {
        MAX_WIND | MIN_TEMP | RAIN_SKIP => {
            let range = match id {
                MAX_WIND => MAX_WIND_RANGE,
                MIN_TEMP => MIN_TEMP_RANGE,
                _ => RAIN_SKIP_RANGE,
            };
            match state.parse::<f64>() {
                Ok(v) if v.is_finite() => {
                    // Whatever it held, this number WAS the threshold
                    // deciding: the old read parsed any finite value and it
                    // outranked Settings. Clamping keeps the intent, since 99
                    // mph becomes 40 and still means effectively never
                    // wind-skip, where reverting to the config value would
                    // start skipping on the first breezy morning.
                    let clamped = v.clamp(range.0, range.1);
                    Some(Read {
                        outcome: OUTCOME_ADOPTED,
                        value: Adopted::Number(clamped),
                        adopted_text: Some(fmt_num(clamped)),
                        observed_text: (clamped != v).then(|| fmt_num(v)),
                        previous_text,
                    })
                }
                _ => Some(unreadable(previous_text)),
            }
        }
        PAUSE_UNTIL => match timestamp_attr(entity) {
            // A negative timestamp is not a pause anyone set; clamp the way
            // the store's own setter does rather than refusing the adoption.
            Some(t) => {
                let t = t.max(0);
                Some(Read {
                    outcome: OUTCOME_ADOPTED,
                    value: Adopted::Epoch(t),
                    adopted_text: Some(t.to_string()),
                    observed_text: None,
                    previous_text,
                })
            }
            None => Some(unreadable(previous_text)),
        },
        OVERRIDE_TOMORROW => match state {
            "none" | "skip" | "run" => Some(Read {
                outcome: OUTCOME_ADOPTED,
                value: Adopted::Text(state.to_string()),
                adopted_text: Some(state.to_string()),
                observed_text: None,
                previous_text,
            }),
            // A hand-edited input_select with a fourth option is not a mode
            // the engine has a meaning for, and never was: the old read
            // passed the string straight through and the ladder treats
            // anything but skip or run as no override. Retiring it leaves the
            // yard where it already was.
            _ => Some(unreadable(previous_text)),
        },
        PAUSE_TOGGLE | DRY_RUN_TOGGLE => match state {
            "on" | "off" => Some(Read {
                outcome: OUTCOME_ADOPTED,
                value: Adopted::Flag(state == "on"),
                adopted_text: Some(state.to_string()),
                observed_text: None,
                previous_text,
            }),
            // Anything else read false through `state_eq` before, and reads
            // false from the store after.
            _ => Some(unreadable(previous_text)),
        },
        _ => Some(unreadable(previous_text)),
    }
}

/// What LocalSky is using for this entity right now, as text. Present on
/// every outcome, because on a non-adoption it is the value LocalSky goes on
/// using and the notice has to name it.
fn previous_text(id: &str, cfg: &Config, control: Option<&IrrigationControlState>) -> String {
    let r = &cfg.engine.skip_rules;
    match id {
        MAX_WIND => fmt_num(r.max_wind_mph),
        MIN_TEMP => fmt_num(r.min_temp_f),
        RAIN_SKIP => fmt_num(r.rain_skip_in),
        PAUSE_UNTIL => control
            .map(|c| c.pause_until_epoch)
            .unwrap_or(0)
            .to_string(),
        OVERRIDE_TOMORROW => control
            .map(|c| c.override_tomorrow.clone())
            .unwrap_or_else(|| "none".to_string()),
        PAUSE_TOGGLE => on_off(control.map(|c| c.is_paused).unwrap_or(false)),
        DRY_RUN_TOGGLE => on_off(control.map(|c| c.is_dry_run).unwrap_or(false)),
        _ => String::new(),
    }
}

fn on_off(v: bool) -> String {
    if v {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

/// Compact decimal, so 15.0 records as "15" and 0.25 as "0.25". The record is
/// read by people.
fn fmt_num(v: f64) -> String {
    format!("{v}")
}

fn state_str(entity: &Value) -> Option<&str> {
    entity.get("state").and_then(Value::as_str)
}

/// The epoch on an `input_datetime`, which Home Assistant reports as a
/// FLOAT: a cleared pause comes over the wire as `0.0` and a set one as
/// `1757000000.0`. `Value::as_i64` returns None for a JSON float, so
/// reading this with `as_i64` alone resolves every real install's pause
/// to "no timestamp", which is how the vacation pause came to do nothing
/// at all on a Home Assistant deployment. Accept both shapes.
pub(crate) fn timestamp_attr(entity: &Value) -> Option<i64> {
    let v = entity.get("attributes").and_then(|a| a.get("timestamp"))?;
    v.as_i64().or_else(|| {
        let f = v.as_f64()?;
        // Reject a value no clock produced rather than saturating it into
        // a plausible looking epoch.
        f.is_finite().then(|| f.trunc() as i64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entity(state: &str) -> Value {
        json!({ "state": state })
    }

    fn pause_entity(ts: i64) -> Value {
        // Home Assistant sends this as a float; build it the way the wire
        // does so these tests cannot pass on a shape no install produces.
        json!({ "state": "2026-09-04 06:00:00", "attributes": { "timestamp": ts as f64 } })
    }

    /// The exact body a live Home Assistant returns for a cleared pause,
    /// captured from a running instance. `as_i64` alone reads None here.
    #[test]
    fn a_live_input_datetime_float_timestamp_parses() {
        let cleared = json!({
            "entity_id": "input_datetime.irrigation_pause_until",
            "state": "1969-12-31 19:00:00",
            "attributes": { "has_date": true, "has_time": true, "timestamp": 0.0 }
        });
        assert_eq!(timestamp_attr(&cleared), Some(0));

        let set = json!({
            "state": "2026-09-04 06:00:00",
            "attributes": { "timestamp": 1_757_000_000.0_f64 }
        });
        assert_eq!(timestamp_attr(&set), Some(1_757_000_000));

        // An integer still works, in case a future version sends one.
        let as_int = json!({ "attributes": { "timestamp": 42 } });
        assert_eq!(timestamp_attr(&as_int), Some(42));

        let nan = json!({ "attributes": { "timestamp": f64::NAN } });
        assert_eq!(timestamp_attr(&nan), None);
    }

    /// The shape a live Home Assistant returns for an `input_number`: the
    /// state is a STRING, and min/max/step ride in the attributes. Built the
    /// way the wire does so the parse cannot pass on a shape no install
    /// produces.
    #[test]
    fn a_live_input_number_state_is_a_string_beside_its_bounds() {
        let wire = json!({
            "entity_id": "input_number.irrigation_max_wind_mph",
            "state": "12.0",
            "attributes": {
                "min": 0.0, "max": 30.0, "step": 1.0, "mode": "slider",
                "initial": 10.0, "editable": false,
                "unit_of_measurement": "mph", "icon": "mdi:weather-windy",
                "friendly_name": "Irrigation - Max Wind (mph)"
            }
        });
        let mut m = HashMap::new();
        m.insert(MAX_WIND.to_string(), wire);
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(p.max_wind_mph, Some(12.0));
        assert_eq!(
            p.records[0].adopted_value.as_deref(),
            Some("12"),
            "the record is read by people, so 12.0 records as 12"
        );
    }

    /// And the shape a live `input_select` returns: a state string plus the
    /// option list.
    #[test]
    fn a_live_input_select_state_is_the_chosen_option() {
        let wire = json!({
            "entity_id": "input_select.irrigation_override_tomorrow",
            "state": "skip",
            "attributes": {
                "options": ["none", "skip", "run"], "editable": false,
                "icon": "mdi:calendar-arrow-right",
                "friendly_name": "Irrigation - Override Tomorrow"
            }
        });
        let mut m = HashMap::new();
        m.insert(OVERRIDE_TOMORROW.to_string(), wire);
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(p.override_tomorrow.as_deref(), Some("skip"));
    }

    fn full_map() -> HashMap<String, Value> {
        let mut m = HashMap::new();
        m.insert(MAX_WIND.to_string(), entity("12"));
        m.insert(MIN_TEMP.to_string(), entity("38"));
        m.insert(RAIN_SKIP.to_string(), entity("0.3"));
        m.insert(PAUSE_UNTIL.to_string(), pause_entity(1_900_000_000));
        m.insert(OVERRIDE_TOMORROW.to_string(), entity("skip"));
        m.insert(PAUSE_TOGGLE.to_string(), entity("on"));
        m.insert(DRY_RUN_TOGGLE.to_string(), entity("off"));
        m
    }

    fn cfg() -> Config {
        Config::default()
    }

    fn control() -> IrrigationControlState {
        IrrigationControlState::default()
    }

    fn helpers(p: &AdoptionPlan) -> Vec<&HaAdoptedHelper> {
        p.records.iter().collect()
    }

    #[test]
    fn adopts_every_present_helper_with_its_target() {
        let p = plan(&full_map(), &cfg(), Some(&control()), 100);
        assert_eq!(helpers(&p).len(), 7);
        assert!(helpers(&p).iter().all(|r| r.outcome == OUTCOME_ADOPTED));
        assert_eq!(p.max_wind_mph, Some(12.0));
        assert_eq!(p.min_temp_f, Some(38.0));
        assert_eq!(p.rain_skip_in, Some(0.3));
        assert_eq!(p.pause_until_epoch, Some(1_900_000_000));
        assert_eq!(p.override_tomorrow.as_deref(), Some("skip"));
        assert_eq!(p.is_paused, Some(true));
        assert_eq!(p.is_dry_run, Some(false));
        assert!(!p.deferred);
        let wind = &p.records[0];
        assert_eq!(wind.target, "engine.skip_rules.max_wind_mph");
        assert_eq!(wind.adopted_value.as_deref(), Some("12"));
        assert_eq!(wind.observed_value, None, "nothing was clamped");
        assert_eq!(
            wind.previous_value.as_deref(),
            Some("10"),
            "the record must name the Settings value the helper was outranking"
        );
        assert!(wind.changed_the_value());
    }

    #[test]
    fn an_absent_helper_is_recorded_and_never_written() {
        let mut m = full_map();
        m.remove(RAIN_SKIP);
        m.remove(PAUSE_UNTIL);
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(helpers(&p).len(), 7);
        assert_eq!(p.rain_skip_in, None);
        assert_eq!(p.pause_until_epoch, None);
        let rain = p.records.iter().find(|r| r.entity == RAIN_SKIP).unwrap();
        assert_eq!(rain.outcome, OUTCOME_NOT_FOUND);
        assert_eq!(rain.adopted_value, None);
        assert_eq!(
            rain.previous_value.as_deref(),
            Some("0.25"),
            "an absent helper leaves the config value standing, and says which"
        );
        assert!(!rain.changed_the_value());
    }

    // The assertion this whole design exists for. A partially restored Home
    // Assistant omits input_* entities from /api/states rather than reporting
    // them unavailable; recording that absence as a value would write 0 over
    // a live vacation pause and water the yard.
    #[test]
    fn absence_never_overwrites_a_live_pause() {
        let mut m = full_map();
        m.remove(PAUSE_UNTIL);
        let mut c = control();
        c.pause_until_epoch = 1_950_000_000;
        let p = plan(&m, &cfg(), Some(&c), 100);
        assert_eq!(
            p.pause_until_epoch, None,
            "an absent input_datetime must plan no pause write at all"
        );
        let rec = p.records.iter().find(|r| r.entity == PAUSE_UNTIL).unwrap();
        assert_eq!(rec.previous_value.as_deref(), Some("1950000000"));
        assert_eq!(
            rec.outcome, OUTCOME_KEPT_LOCAL,
            "LocalSky's own live pause is the answer, so it is the one kept"
        );
    }

    // And the other half of that rule. Nothing clears `pause_until_epoch`, so
    // an install that ran standalone years ago can still be carrying the
    // epoch of a vacation that ended. Treating that as an operator answer
    // would discard the pause the owner has set in Home Assistant for the
    // vacation they leave for tomorrow, and retire the read on the way out.
    #[test]
    fn an_expired_native_pause_does_not_discard_a_live_helper_pause() {
        let now = 1_800_000_000;
        let mut m = full_map();
        m.insert(PAUSE_UNTIL.to_string(), pause_entity(now + 86_400));
        let mut c = control();
        c.pause_until_epoch = now - 3_600; // set on a native install, long over
        let p = plan(&m, &cfg(), Some(&c), now);
        assert_eq!(
            p.pause_until_epoch,
            Some(now + 86_400),
            "a spent pause is not an answer, so the live helper pause is taken"
        );
        let rec = p.records.iter().find(|r| r.entity == PAUSE_UNTIL).unwrap();
        assert_eq!(rec.outcome, OUTCOME_ADOPTED);
        assert_eq!(
            rec.previous_value.as_deref(),
            Some("1799996400"),
            "and the record still names the fossil it replaced"
        );
        // A pause still running keeps outranking the helper.
        let mut live = control();
        live.pause_until_epoch = now + 3_600;
        let p = plan(&m, &cfg(), Some(&live), now);
        assert_eq!(p.pause_until_epoch, None);
        assert_eq!(
            p.records
                .iter()
                .find(|r| r.entity == PAUSE_UNTIL)
                .unwrap()
                .outcome,
            OUTCOME_KEPT_LOCAL
        );
    }

    // The second route to the same loss, and the one the stability window
    // cannot help with: `unavailable` is a STABLE answer, so three identical
    // ticks of it prove only that it is still broken.
    #[test]
    fn an_unavailable_control_defers_instead_of_retiring_its_read() {
        for state in ["unavailable", "unknown", ""] {
            let mut m = full_map();
            m.insert(PAUSE_TOGGLE.to_string(), entity(state));
            let p = plan(&m, &cfg(), Some(&control()), 100);
            assert!(
                !p.records.iter().any(|r| r.entity == PAUSE_TOGGLE),
                "{state}: an unavailable pause switch must not be recorded, \
                 because recording it retires the read onto a column no human wrote"
            );
            assert_eq!(p.is_paused, None);
            assert!(p.deferred, "{state}: the caller has to stay armed");
            // Everything else still adopts on the same tick.
            assert_eq!(p.max_wind_mph, Some(12.0));
        }
    }

    // Home Assistant marks a registry-restored entity `restored: true` until
    // its platform finishes setting up. That is the same "exists but has no
    // answer yet" case, wearing a different state string.
    #[test]
    fn a_restored_placeholder_control_defers_too() {
        let mut m = full_map();
        // The exact shape a live Home Assistant gives a registry-restored
        // entity whose platform has not set up: state "unavailable" with the
        // `restored` marker beside it, captured from a running instance.
        m.insert(
            PAUSE_UNTIL.to_string(),
            json!({
                "entity_id": "input_datetime.irrigation_pause_until",
                "state": "unavailable",
                "attributes": {
                    "restored": true,
                    "friendly_name": "Irrigation - Pause Until",
                    "supported_features": 0
                }
            }),
        );
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert!(!p.records.iter().any(|r| r.entity == PAUSE_UNTIL));
        assert_eq!(p.pause_until_epoch, None);
        assert!(p.deferred);
    }

    // A threshold is not a control: its read was helper.unwrap_or(config), so
    // an unusable helper already resolved to the config value and retiring it
    // moves nothing.
    #[test]
    fn an_unavailable_threshold_still_retires_because_it_decided_nothing() {
        let mut m = full_map();
        m.insert(MAX_WIND.to_string(), entity("unavailable"));
        m.insert(MIN_TEMP.to_string(), entity("unknown"));
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(p.max_wind_mph, None);
        assert_eq!(p.min_temp_f, None);
        assert!(!p.deferred, "a threshold never defers");
        for id in [MAX_WIND, MIN_TEMP] {
            let r = p.records.iter().find(|r| r.entity == id).unwrap();
            assert_eq!(r.outcome, OUTCOME_UNREADABLE);
        }
    }

    // Present and parseable-as-something-meaningless is not the transient
    // case: the old read passed the string through and the ladder ignored it,
    // so retiring changes nothing and the entity should not defer forever.
    #[test]
    fn an_override_option_the_engine_has_no_meaning_for_is_unreadable() {
        let mut m = full_map();
        m.insert(OVERRIDE_TOMORROW.to_string(), entity("vacation"));
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(p.override_tomorrow, None);
        assert!(!p.deferred);
        let r = p
            .records
            .iter()
            .find(|r| r.entity == OVERRIDE_TOMORROW)
            .unwrap();
        assert_eq!(r.outcome, OUTCOME_UNREADABLE);
    }

    #[test]
    fn a_datetime_with_no_timestamp_attribute_is_unreadable() {
        let mut m = full_map();
        m.insert(PAUSE_UNTIL.to_string(), entity("2026-09-04 06:00:00"));
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(p.pause_until_epoch, None);
        let r = p.records.iter().find(|r| r.entity == PAUSE_UNTIL).unwrap();
        assert_eq!(r.outcome, OUTCOME_UNREADABLE);
    }

    // The "turn this rule off" idiom: an input_number created through the HA
    // helpers UI defaults to min 0 / max 100, so 99 mph and 0 F are how a
    // person disables the wind and freeze gates. Both were the value
    // deciding. Reverting them to the config default starts skipping.
    #[test]
    fn a_threshold_outside_localskys_range_is_clamped_and_records_both_numbers() {
        let mut m = full_map();
        m.insert(MAX_WIND.to_string(), entity("99"));
        m.insert(MIN_TEMP.to_string(), entity("0"));
        m.insert(RAIN_SKIP.to_string(), entity("-1"));
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(p.max_wind_mph, Some(50.0), "99 mph clamps to the ceiling");
        assert_eq!(p.min_temp_f, Some(20.0), "0 F clamps to the floor");
        assert_eq!(p.rain_skip_in, Some(0.0));
        let wind = p.records.iter().find(|r| r.entity == MAX_WIND).unwrap();
        assert_eq!(wind.outcome, OUTCOME_ADOPTED);
        assert_eq!(wind.adopted_value.as_deref(), Some("50"));
        assert_eq!(
            wind.observed_value.as_deref(),
            Some("99"),
            "the notice has to print the number the helper actually held"
        );
    }

    #[test]
    fn a_threshold_that_is_not_a_number_at_all_is_unreadable() {
        let mut m = full_map();
        m.insert(MAX_WIND.to_string(), entity("windy"));
        let p = plan(&m, &cfg(), Some(&control()), 100);
        assert_eq!(p.max_wind_mph, None);
        assert_eq!(
            p.records
                .iter()
                .find(|r| r.entity == MAX_WIND)
                .unwrap()
                .outcome,
            OUTCOME_UNREADABLE
        );
    }

    // A standalone install that later gained HA_URL: LocalSky's own store
    // holds the operator's live answer and Home Assistant holds a helper from
    // before the switch. The store wins, and the read still retires.
    #[test]
    fn a_live_native_control_is_never_overwritten_by_a_stale_helper() {
        let mut m = full_map();
        m.insert(PAUSE_TOGGLE.to_string(), entity("off"));
        m.insert(OVERRIDE_TOMORROW.to_string(), entity("none"));
        let mut c = control();
        c.is_paused = true;
        c.pause_until_epoch = 1_950_000_000;
        c.override_tomorrow = "skip".to_string();
        c.is_dry_run = true;
        let p = plan(&m, &cfg(), Some(&c), 100);
        assert_eq!(p.is_paused, None);
        assert_eq!(p.pause_until_epoch, None);
        assert_eq!(p.override_tomorrow, None);
        assert_eq!(p.is_dry_run, None);
        for id in CONTROL_ENTITIES {
            let r = p.records.iter().find(|r| r.entity == id).unwrap();
            assert_eq!(r.outcome, OUTCOME_KEPT_LOCAL, "{id}");
            assert_eq!(r.adopted_value, None, "{id}");
        }
        // The thresholds are unaffected: their store is the config file, and
        // the helper genuinely was the one deciding.
        assert_eq!(p.max_wind_mph, Some(12.0));
    }

    #[test]
    fn an_already_marked_entity_is_skipped_whatever_the_helper_now_says() {
        let mut c = cfg();
        c.engine.skip_rules.max_wind_mph = 22.0;
        c.ha_adoption.push(HaAdoptedHelper {
            entity: MAX_WIND.to_string(),
            outcome: OUTCOME_ADOPTED.to_string(),
            target: target_of(MAX_WIND).to_string(),
            adopted_value: Some("12".to_string()),
            observed_value: None,
            previous_value: Some("15".to_string()),
            epoch: 1,
        });
        let p = plan(&full_map(), &c, Some(&control()), 100);
        assert_eq!(p.max_wind_mph, None, "a marked entity is never re-read");
        assert!(!p.records.iter().any(|r| r.entity == MAX_WIND));
        assert_eq!(helpers(&p).len(), 6);
    }

    #[test]
    fn a_fully_marked_config_plans_nothing() {
        let mut c = cfg();
        for id in ENTITIES.iter() {
            c.ha_adoption.push(HaAdoptedHelper {
                entity: (*id).to_string(),
                outcome: OUTCOME_NOT_FOUND.to_string(),
                target: target_of(id).to_string(),
                adopted_value: None,
                observed_value: None,
                previous_value: None,
                epoch: 1,
            });
        }
        let p = plan(&full_map(), &c, Some(&control()), 100);
        assert!(p.is_empty());
        assert!(!p.deferred);
    }

    #[test]
    fn without_a_control_store_the_four_controls_are_left_alone() {
        // No persistence DB: the controls have nowhere to land, so they stay
        // unhandled and their reads stay live. The thresholds still adopt,
        // because their sink is the config file.
        let p = plan(&full_map(), &cfg(), None, 100);
        assert_eq!(p.records.len(), 3);
        assert!(p
            .records
            .iter()
            .all(|r| r.entity.starts_with("input_number")));
        assert!(!p.writes_controls());
        assert_eq!(p.max_wind_mph, Some(12.0));
    }

    #[test]
    fn apply_writes_the_thresholds_and_the_markers_once() {
        let mut c = cfg();
        let p = plan(&full_map(), &c, Some(&control()), 100);
        apply(&p, &mut c);
        assert_eq!(c.engine.skip_rules.max_wind_mph, 12.0);
        assert_eq!(c.engine.skip_rules.min_temp_f, 38.0);
        assert_eq!(c.engine.skip_rules.rain_skip_in, 0.3);
        assert_eq!(c.ha_adoption.len(), ENTITIES.len());
        // Applying the same plan twice must not duplicate a marker.
        apply(&p, &mut c);
        assert_eq!(c.ha_adoption.len(), ENTITIES.len());
        // And a second pass over the same map now plans nothing.
        assert!(plan(&full_map(), &c, Some(&control()), 200).is_empty());
    }

    #[test]
    fn adoption_never_overwrites_a_later_settings_edit() {
        let mut c = cfg();
        let p = plan(&full_map(), &c, Some(&control()), 100);
        apply(&p, &mut c);
        // The owner reads the notice and puts their own number back.
        c.engine.skip_rules.max_wind_mph = 18.0;
        let again = plan(&full_map(), &c, Some(&control()), 200);
        apply(&again, &mut c);
        assert_eq!(
            c.engine.skip_rules.max_wind_mph, 18.0,
            "the marker, not the value, decides whether the pass runs"
        );
    }

    #[test]
    fn the_fingerprint_moves_when_an_answer_moves() {
        let base = fingerprint(&full_map());
        let mut m = full_map();
        m.remove(PAUSE_TOGGLE);
        assert_ne!(
            base,
            fingerprint(&m),
            "an entity appearing or leaving moves it"
        );
        let mut m2 = full_map();
        m2.insert(PAUSE_UNTIL.to_string(), pause_entity(1_900_000_001));
        assert_ne!(
            base,
            fingerprint(&m2),
            "the pause carries its value in an attribute, so the attribute counts"
        );
        assert_eq!(base, fingerprint(&full_map()), "and it is stable otherwise");
    }

    #[tokio::test]
    async fn the_write_counter_moves_once_per_preadoption_write() {
        let _seq = SEQ_TEST_LOCK.lock().await;
        let before = write_seq();
        note_preadopt_write();
        assert_eq!(write_seq(), before + 1);
    }
}
