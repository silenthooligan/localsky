// ZoneCard, a rich, scannable card per zone for the Zones master-detail.
// Clicking the card selects it (the detail slides into the right pane, no
// navigation). Shows live status (color-coded), the key numbers, an
// optional zone photo, and an inline Stop when running.

use leptos::prelude::*;
use serde_json::json;

use crate::components::irrigation::controls::{post_action_note_then, OverrideControl};
use crate::components::ui::{Button, Icon};
use crate::components::units_fmt::{
    deficit_value_mm, depth_phrase_in, depth_phrase_mm, depth_unit, use_unit_prefs, UnitPrefs,
};
use crate::ha::snapshot::{SmartSuppression, WaterBudget, ZoneState};

/// (status key, label, color token) for a zone's live state. A zone the engine
/// will SKIP must never read "SCHEDULED" even if it carries a leftover planned
/// duration: the skip verdict is the truth, so it reads "SKIPPING" (blue) and
/// the planned minutes below are suppressed. The scheduled label matches
/// the zone detail's pill (one name per state), and the stat labels use
/// the dispatcher's own morning framing: the run is the pre-sunrise
/// smart morning, which "TONIGHT" misnamed beside soil sentences that
/// say "today".
///
/// A zone the weekly budget zeroed is NOT idle: the engine made a decision
/// about it and can say which one. It reads "ON HOLD", and the caller pairs
/// that with the reason line, which is either the budget's own sentence or
/// the Override schedule covering today. Idle is reserved for a zone
/// nothing decided anything about (no budget row, no schedule).
pub fn zone_status(z: &ZoneState, budget_held: bool) -> (&'static str, &'static str, &'static str) {
    let skipping = z.verdict.as_ref().is_some_and(|v| v.verdict == "skip");
    if z.running {
        ("running", "RUNNING", "var(--verdict-run)")
    } else if skipping {
        ("skipping", "SKIPPING", "var(--verdict-skip)")
    } else if z.planned_run_seconds > 0 {
        ("scheduled", "SCHEDULED", "var(--accent)")
    } else if budget_held {
        ("held", "ON HOLD", "var(--accent-cool)")
    } else {
        ("idle", "IDLE", "var(--verdict-off)")
    }
}

/// Plain-English sentence naming the weekdays a set of `0 = Sun .. 6 = Sat`
/// day numbers covers: "Mon", "Mon and Wed", "Mon, Wed and Fri".
pub fn weekday_list(days: &[u8]) -> String {
    const NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let names: Vec<&str> = days
        .iter()
        .filter_map(|d| NAMES.get(*d as usize).copied())
        .collect();
    match names.len() {
        0 => String::new(),
        1 => names[0].to_string(),
        _ => format!(
            "{} and {}",
            names[..names.len() - 1].join(", "),
            names[names.len() - 1]
        ),
    }
}

/// The ON HOLD body for a held soil-governed row, composed CLIENT-side
/// from the structured soil fields so every number renders in the
/// viewer's display unit. The wire's `today_reason` keeps its baked
/// machine string (millimeter operands, its own lead-ins) for logs and
/// external consumers; echoing it here stacked two lead-ins ("Not
/// watering today: deferred: ...") and printed raw mm beside a Deficit
/// tile that honors the units setting. `None` when the row is not a
/// held soil row; the caller then falls back to the wire reason (weekly
/// reasons carry no lead-ins and no depth operands).
pub fn soil_hold_body(b: &WaterBudget, p: UnitPrefs) -> Option<String> {
    if b.scheduling_model != "soil" || b.today_seconds != 0 {
        return None;
    }
    let dep = depth_phrase_mm(b.soil_depletion_mm?, p);
    if let Some(reason) = b.soil_deferred_reason.as_deref() {
        use crate::engine::soil_schedule::SoilDeferKind;
        // WHICH hold this is comes from the engine as data. It used to be
        // read off the sentence's opening words, so a copy edit in the
        // engine would silently drop this row back to echoing a raw
        // machine string with millimeters in it.
        match b.soil_deferred_kind {
            Some(SoilDeferKind::ForecastRain) => {
                // The wire's parenthetical bakes mm; the composed
                // sentence states the same hold from the structured
                // deficit instead, in the viewer's units.
                return Some(format!(
                    "the soil model expects forecast rain to cover the {dep} deficit"
                ));
            }
            Some(SoilDeferKind::Window) => {
                // Window admission: counts only, no depth operands, so
                // the wire sentence reads correctly once its lead-in is
                // gone.
                let rest = reason
                    .strip_prefix("waits for tomorrow: ")
                    .unwrap_or(reason);
                return Some(format!("{rest}; this zone waits for tomorrow"));
            }
            None => return Some(reason.to_string()),
        }
    }
    if !b.soil_due {
        let raw = depth_phrase_mm(b.soil_raw_mm.unwrap_or(0.0), p);
        return Some(format!(
            "the soil is wet enough. Waters when the deficit reaches {raw}; it is at {dep} now"
        ));
    }
    if b.soil_ceiling_binding {
        // Due but parked at zero headroom under the explicit weekly
        // target (one name everywhere: the editor field).
        return Some("the weekly target leaves no headroom this week".to_string());
    }
    None
}

/// The ON HOLD sentence for a zone whose plan is zero: what actually held it.
///
/// Both gates can fire on the same day. An Override schedule zeroes the plan
/// in `apply_budget_plan` AFTER the allocator has already sized it, so the
/// schedule is named first; the allocator's own sentence follows when it also
/// had a reason. Reporting only the schedule hid the second gate, so lifting
/// the Override left the zone dry with nothing on screen saying why.
/// `budget_reason` is display-ready (a soil row passes `soil_hold_body`'s
/// composition, a weekly row its wire reason); `soil_governed` picks
/// which model the also-held sentence names.
pub fn hold_reason_text(
    suppression: Option<&SmartSuppression>,
    budget_reason: Option<&str>,
    soil_governed: bool,
) -> String {
    match suppression.filter(|x| x.active_today) {
        Some(x) => {
            let names = x.schedules.join(", ");
            let mut body = if names.is_empty() {
                "Not watering today: a manual schedule set to Override covers today.".to_string()
            } else {
                format!("Not watering today: the Override schedule {names} covers today.")
            };
            if let Some(r) = budget_reason {
                let model = if soil_governed {
                    "The soil model also held it"
                } else {
                    "The weekly budget also held it"
                };
                body.push_str(&format!(" {model}: {r}"));
            }
            body
        }
        None => format!("Not watering today: {}", budget_reason.unwrap_or_default()),
    }
}

/// The partial weekly-target clamp's reason line for a run that still
/// waters. A soil-governed zone the explicit weekly target shorted (but
/// did not zero) carries the delivered-of-target sentence in
/// `today_reason`, and both reason consumers used to drop it because
/// they only surface `today_reason` at zero seconds; the partial clamp
/// was invisible on the one day it applied. The zero-headroom day still
/// rides the ON HOLD path. The wire sentence is re-composed at the
/// display boundary: capitalized lead, the operator's field name
/// ("weekly target", not "weekly ceiling"), sprinkler-only delivery
/// named, and the inch operands converted to the display unit.
pub fn ceiling_note_text(b: &WaterBudget, p: UnitPrefs) -> Option<String> {
    (b.scheduling_model == "soil"
        && b.soil_ceiling_binding
        && b.today_seconds > 0
        && !b.today_reason.is_empty())
    .then(|| ceiling_sentence(&b.today_reason, p))
}

/// Compose the partial-clamp sentence from the wire's ceiling reason.
/// The producer's format is pinned ("held to the weekly ceiling: X in of
/// the Y in target delivered in the last 7 days, Z in of headroom
/// left"); the three inch operands are the number tokens followed by
/// the unit word "in". When they parse, the sentence is rebuilt in the
/// display unit; when they do not (a future producer shape), the wire
/// body rides behind the capitalized lead unchanged rather than
/// dropping the clamp on the one day it applies.
pub(crate) fn ceiling_sentence(wire: &str, p: UnitPrefs) -> String {
    let toks: Vec<&str> = wire.split_whitespace().collect();
    let mut vals: Vec<f64> = Vec::new();
    for w in toks.windows(2) {
        if w[1].trim_end_matches(',') == "in" {
            if let Ok(v) = w[0].parse::<f64>() {
                vals.push(v);
            }
        }
    }
    if let [delivered, target, headroom] = vals[..] {
        return format!(
            "Today's run is held to the weekly target: sprinklers delivered {} of the {} \
             target in the last 7 days, and today waters the remaining {}.",
            depth_phrase_in(delivered, p),
            depth_phrase_in(target, p),
            depth_phrase_in(headroom, p),
        );
    }
    let body = wire
        .strip_prefix("held to the weekly ceiling: ")
        .unwrap_or(wire);
    format!("Today's run is held to the weekly target: {body}")
}

/// The Override frequency line: HOW OFTEN smart watering is off for this zone.
///
/// This is the only line that separates "blocked today" from "blocked all
/// seven days", so it stays on the day the Override fires. That is the day a
/// user opens the zone to ask why it did nothing. When `hold_named_today` is
/// true the hold line above has already named WHICH schedule covers today, so
/// this shortens to the frequency alone rather than disappearing.
pub fn override_days_text(x: &SmartSuppression, hold_named_today: bool) -> String {
    let days = weekday_list(&x.weekdays);
    let names = x.schedules.join(", ");
    if hold_named_today {
        format!("Smart watering is off on {days}.")
    } else if names.is_empty() {
        format!(
            "Smart watering is off for this zone on {days}: a manual schedule set to Override \
             covers those days."
        )
    } else {
        format!(
            "Smart watering is off for this zone on {days}: the Override schedule {names} \
             covers those days."
        )
    }
}

#[component]
pub fn ZoneCard(
    zone: ZoneState,
    selected: RwSignal<Option<String>>,
    /// Live soil moisture % from the zone's assigned probe (joined from
    /// the snapshot's soil_forecasts by the caller). None = no probe.
    #[prop(optional_no_strip)]
    soil_pct: Option<f64>,
    /// Whether the tuning report carries a recommendation for this zone
    /// (joined from the page-level report signal by the caller; the card
    /// itself never fetches). Renders the attention pill in the head row.
    #[prop(optional)]
    has_suggestion: bool,
    /// This zone's row from the snapshot's weekly budget, joined by slug by
    /// the caller. The allocator is what decides whether a zone waters, and
    /// its `today_reason` is a plain-English sentence naming which gate
    /// fired. It had no reader anywhere in the app, so a zone that would
    /// not water showed a bare zero and nothing else.
    #[prop(optional_no_strip)]
    budget: Option<WaterBudget>,
    /// The engine-level scheduling-model default ("weekly" | "soil",
    /// empty on older snapshots), from the snapshot. A small model chip
    /// renders only when this zone's effective model DIFFERS from it, so
    /// a single-model install stays quiet and a mixed install (the demo
    /// side_yard shape) is scannable from the list.
    #[prop(optional, into)]
    engine_model: String,
    /// Where a Stop result is delivered. Owned by the PAGE, because the
    /// card list is rebuilt on every streamed snapshot and a callback
    /// created here would be disposed with its own request still in
    /// flight (see `on_stop`).
    stop_done: Callback<Result<Option<String>, String>>,
) -> impl IntoView {
    // A zone the budget zeroed is "on hold" with a reason, never idle.
    // Held only when nothing else already explains the zero: a skip verdict
    // and a force-run floor both outrank the allocator.
    let budget_reason = budget
        .as_ref()
        .filter(|b| b.today_seconds == 0 && !b.today_reason.is_empty())
        .map(|b| b.today_reason.clone());
    let suppression = zone.smart_suppressed.clone();
    // An Override schedule zeroes the plan in `apply_budget_plan`, AFTER
    // the allocator has already sized a positive `today_seconds`. The
    // allocator never sees the schedule, so it writes no reason, and
    // without this the one day the schedule is the whole explanation was
    // the one day the zone read IDLE.
    let suppressed_today = suppression.as_ref().is_some_and(|x| x.active_today);
    let budget_held = (budget_reason.is_some() || suppressed_today)
        && zone.planned_run_seconds == 0
        && !zone.running
        && !zone.verdict.as_ref().is_some_and(|v| v.verdict == "skip");
    let (status, label, color) = zone_status(&zone, budget_held);
    // Per-device display-unit preference; read prefs.get() in render
    // closures so a units change (or post-hydration localStorage load)
    // re-renders the convertible values.
    let prefs = use_unit_prefs();
    let name = zone.name.clone();
    let slug = zone.slug.clone();
    let slug_sel = slug.clone();
    let slug_active = slug.clone();
    let is_active = move || selected.get().as_deref() == Some(slug_active.as_str());
    // A zone the engine will SKIP waters 0 minutes tonight, regardless of any
    // leftover planned duration on the snapshot: show "0" so the morning stat
    // matches the SKIPPING status and the zone's verdict (T4), never a planned
    // figure the zone will not actually run.
    let zone_skipping = zone.verdict.as_ref().is_some_and(|v| v.verdict == "skip");
    let planned = if zone_skipping {
        "0".to_string()
    } else {
        ((zone.planned_run_seconds + 30) / 60).to_string()
    };
    // Nothing summarizes today's run minutes on either path, so this is a
    // dash rather than a "0 min" printed beside a hold line that names the
    // inches already applied this week.
    let today = zone
        .today_run_minutes
        .map(|v| format!("{v:.0}"))
        .unwrap_or_else(|| "-".to_string());
    let today_known = zone.today_run_minutes.is_some();
    // Deficit is a soil-water DEPTH stored in millimeters; convert at the
    // display boundary (helpers respect units_rain). Engine math + wire
    // format stay mm. The soil model's evidence replay produces it for
    // every zone with agronomy config (negative = needs water); `None`
    // means no bucket could be derived (env-var zones) and renders a
    // dash, never the fabricated 0.00 this tile printed before 0.7.22.
    let deficit_mm = zone.bucket_mm;
    let deficit_known = deficit_mm.is_some();
    let running = zone.running;
    let stop_slug = slug.clone();
    // Sticky per-zone override (Auto/Skip/Force). Card is re-created per
    // snapshot, so this value is current; the control POSTs set_zone_override.
    let ov_mode = zone.override_mode.clone();
    let ov_slug = slug.clone();
    // Disabled-after-click guard; the next streamed snapshot recreates the
    // card with the real state, so this only needs to cover the gap.
    let stopping = RwSignal::new(false);
    let on_stop = move |ev: leptos::ev::MouseEvent| {
        ev.stop_propagation();
        if stopping.get_untracked() {
            return;
        }
        stopping.set(true);
        // `stop_done` is owned by the PAGE, not by this card. Building a
        // callback here instead put it in the card's owner, which the next
        // streamed snapshot disposes: a Stop whose response landed after
        // that either showed nothing or, because the old callback resolved
        // the toast hub from an owner that no longer existed, aborted the
        // wasm module. The guard clears on the next snapshot, which
        // rebuilds this card with the controller's real state.
        post_action_note_then(
            json!({ "kind": "stop", "zone": stop_slug.clone() }),
            stop_done,
        );
    };
    let photo = zone.photo_url.clone().filter(|p| !p.is_empty());

    // The model chip, mixed installs only: which model sizes this zone,
    // shown exactly where it differs from the engine default so the
    // master list can answer why one zone waters on a rain day its
    // neighbors skip without opening the detail.
    let model_chip = budget
        .as_ref()
        .map(|b| b.scheduling_model.as_str())
        .filter(|m| !m.is_empty() && !engine_model.is_empty() && *m != engine_model)
        .map(|m| {
            let label = if m == "soil" { "SOIL" } else { "WEEKLY" };
            view! { <span class="zone-card__model">{label}</span> }
        });

    // Per-zone verdict (from decide_per_zone): a colored pill + reason so a
    // zone skipping on its own soil reading is visible at a glance.
    let verdict = zone.verdict.clone();
    let verdict_pill = verdict.as_ref().map(|v| {
        let vc = crate::components::verdict::verdict_token(&v.verdict);
        let vl = crate::components::verdict::verdict_label(&v.verdict);
        view! {
            <span class="zone-card__verdict" style=format!("--vc:{vc}")>{vl}</span>
        }
    });
    let verdict_reason = verdict
        .as_ref()
        // Show the reason on skips and on a soil-floor run (P1-2), so the green
        // WATER pill explains "soil below minimum; forecast-rain skip overridden".
        .filter(|v| (v.verdict == "skip" || v.source == "soil_floor") && !v.reason.is_empty())
        .cloned()
        .map(|v| {
            // P2 units architecture: render the reason unit-aware from the
            // structured ZoneVerdict (soil reasons are percent / unit-invariant;
            // global-bound reasons fall back to the baked string). Read prefs.get()
            // inside the closure so a units toggle re-renders.
            view! {
                <div class="zone-card__reason">
                    {move || crate::reason_render::render_zone_reason(&v, prefs.get())}
                </div>
            }
        });

    // Why the zero, shown where the zero is. Only when the zone reads ON
    // HOLD, so it never contradicts a run, a skip verdict, or a force-run
    // floor. A schedule covering today outranks the allocator's sentence:
    // the allocator's number was overwritten by the suppression, so its
    // reason would describe a plan that is not what zeroed the zone. A
    // soil-governed row's body composes client-side from the structured
    // soil fields (unit-aware, no stacked wire lead-ins); reading
    // prefs.get() inside the closure keeps it current with the setting.
    let budget_note = budget_held.then(|| {
        let supp = suppression.clone();
        let b = budget.clone();
        let wire_reason = budget_reason.clone();
        view! {
            <div class="zone-card__reason zone-card__reason--budget">
                {move || {
                    let composed = b
                        .as_ref()
                        .and_then(|bb| soil_hold_body(bb, prefs.get()));
                    // The also-held sentence names the soil model only
                    // when the soil composition actually produced the
                    // body: a governed-but-starved zone's reason is the
                    // weekly allocator's and reads as such.
                    let soil = composed.is_some();
                    let body = composed.or_else(|| wire_reason.clone());
                    hold_reason_text(supp.as_ref(), body.as_deref(), soil)
                }}
            </div>
        }
    });
    // A partial ceiling clamp on a run that still waters: the reason
    // renders beside the nonzero minutes it explains, so the clamp is
    // visible on the day it applies rather than only at zero headroom.
    let ceiling_budget = budget.clone();
    let ceiling_note = budget
        .as_ref()
        .is_some_and(|b| ceiling_note_text(b, UnitPrefs::default()).is_some())
        .then(|| {
            view! {
                <div class="zone-card__reason zone-card__reason--budget">
                    {move || {
                        ceiling_budget
                            .as_ref()
                            .and_then(|b| ceiling_note_text(b, prefs.get()))
                    }}
                </div>
            }
        });
    // An Override manual schedule silently zeroed the smart plan and nothing
    // said so, which turned "add a schedule" into a permanent lockout. The
    // hold line names WHICH schedule; this line names HOW OFTEN, so it stays
    // on the day the Override fires. That is the day the user opens the card
    // to ask why the zone did nothing, and this is the only line that
    // separates "blocked today" from "blocked all seven days". It shortens to
    // the frequency alone when the hold line above already named today's
    // schedule, so the card does not say the same thing twice.
    let suppression_note = suppression
        .as_ref()
        .filter(|x| !x.weekdays.is_empty())
        .map(|x| {
            let body = override_days_text(x, budget_held && x.active_today);
            view! { <div class="zone-card__reason zone-card__reason--suppressed">{body}</div> }
        });

    let select_label = format!("Open {} details", zone.name);
    // Selection pattern is form-factor aware: on desktop/tablet the card
    // drives the side-by-side detail pane; on phones (where that pane
    // would land below the fold) the tap pushes the standalone
    // /zones/:slug page instead, the list -> detail flow phones expect.
    let is_mobile = use_context::<RwSignal<bool>>();
    let nav_slug = slug.clone();
    let navigate = leptos_router::hooks::use_navigate();
    let on_select = move |_| {
        let mobile = is_mobile.map(|s| s.get_untracked()).unwrap_or(false);
        if mobile {
            // Plain route, not base::url(): navigate() resolves against the
            // Router base itself, so pre-prefixing double-prefixes under HA
            // ingress (issue #3).
            navigate(
                &format!("/zones/{nav_slug}"),
                leptos_router::NavigateOptions::default(),
            );
        } else {
            selected.set(Some(slug_sel.clone()));
        }
    };
    view! {
        <div
            class=format!("zone-card zone-card--{status}")
            class:is-selected=is_active
            style=format!("--zc:{color}")
        >
            // A real <button> overlay carries the select action (keyboard +
            // AT correct); the inline Stop sits above it via z-index, so no
            // nested-interactive markup.
            <button
                type="button"
                class="zone-card__hit"
                aria-label=select_label
                on:click=on_select
            ></button>
            {photo.map(|src| view! {
                <div class="zone-card__photo" style=format!("background-image:url('{src}')")></div>
            })}
            <div class="zone-card__body">
                <div class="zone-card__head">
                    <span class="zone-card__dot"></span>
                    <span class="zone-card__name">{name}</span>
                    <span class="zone-card__pill">{label}</span>
                    {model_chip}
                    {verdict_pill}
                    {has_suggestion.then(|| view! {
                        <span class="attention-pill">"Suggestion"</span>
                    })}
                </div>
                {verdict_reason}
                {budget_note}
                {ceiling_note}
                {suppression_note}
                <div class="zone-card__stats">
                    <div class="zone-card__stat">
                        <span class="zone-card__k">"This morning"</span>
                        <span class="zone-card__v">{planned}<small>" min"</small></span>
                    </div>
                    <div class="zone-card__stat">
                        <span class="zone-card__k">"Today"</span>
                        <span class="zone-card__v">
                            {today}
                            <small>{if today_known { " min" } else { "" }}</small>
                        </span>
                    </div>
                    <div class="zone-card__stat">
                        <span class="zone-card__k">"Deficit"</span>
                        <span class="zone-card__v">
                            // Magnitude via the shared deficit formatter: the
                            // label carries the direction, so the tile never
                            // shows the wire's minus sign as a fake surplus.
                            {move || match deficit_mm {
                                Some(v) => deficit_value_mm(v, prefs.get()),
                                None => "-".to_string(),
                            }}
                            <small>{move || if deficit_known {
                                format!(" {}", depth_unit(prefs.get()))
                            } else {
                                String::new()
                            }}</small>
                        </span>
                    </div>
                    {soil_pct.map(|pct| view! {
                        <div class="zone-card__stat">
                            <span class="zone-card__k">"Soil"</span>
                            <span class="zone-card__v">{format!("{pct:.0}")}<small>"%"</small></span>
                        </div>
                    })}
                </div>
                // Per-zone override. stop_propagation so tapping a segment sets
                // the override instead of selecting/opening the zone.
                <div
                    class="zone-card__override"
                    on:click=move |ev: leptos::ev::MouseEvent| ev.stop_propagation()
                >
                    <span class="zone-card__override-label">"Override"</span>
                    <OverrideControl current=Signal::derive(move || ov_mode.clone()) zone=ov_slug/>
                </div>
                {running.then(|| view! {
                    <div class="zone-card__foot">
                        <Button
                            variant="danger"
                            class="zone-card__stop"
                            disabled=Signal::derive(move || stopping.get())
                            on_click=Callback::new(on_stop)
                        >
                            <Icon name="stop" size=14/>
                            {move || if stopping.get() { "Stopping…" } else { "Stop" }}
                        </Button>
                    </div>
                })}
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supp(days: &[u8], names: &[&str], active_today: bool) -> SmartSuppression {
        SmartSuppression {
            weekdays: days.to_vec(),
            schedules: names.iter().map(|s| s.to_string()).collect(),
            active_today,
        }
    }

    #[test]
    fn override_frequency_survives_the_day_the_override_fires() {
        // The day the Override fires is the day a user opens the zone to ask
        // why it did nothing, and it used to be the one day the frequency was
        // hidden as "duplicative". It is not duplicative: the hold line names
        // WHICH schedule, this line names HOW OFTEN. A seven-day lockout and
        // a one-day pause read identically without it, and telling those two
        // apart is the whole diagnosis for a zone that never waters.
        let all_week = supp(&[0, 1, 2, 3, 4, 5, 6], &["Lawn Days"], true);
        let line = override_days_text(&all_week, true);
        assert_eq!(
            line, "Smart watering is off on Sun, Mon, Tue, Wed, Thu, Fri and Sat.",
            "the seven-day lockout has to be visible on a day it fires"
        );
        // Not today: the full sentence, which still names the schedule since
        // no hold line is showing.
        let line = override_days_text(&supp(&[1, 3], &["Lawn Days"], false), false);
        assert!(line.contains("Mon and Wed"), "{line}");
        assert!(line.contains("Lawn Days"), "{line}");
    }

    /// The demo side_yard shape: a soil-governed zone the explicit
    /// weekly target shorted but did not zero surfaces the clamp beside
    /// the nonzero minutes, re-composed at the display boundary: a
    /// capitalized lead, the field's own name ("weekly target"),
    /// sprinkler-only delivery named, and the operands in the display
    /// unit. The zero-headroom day rides the ON HOLD path (no double
    /// note), and a weekly-governed row's shadow flag never surfaces
    /// the weekly allocator's sentence.
    #[test]
    fn ceiling_note_surfaces_the_partial_clamp_on_a_watering_day() {
        let p = UnitPrefs::default();
        let mut b = WaterBudget {
            scheduling_model: "soil".into(),
            soil_ceiling_binding: true,
            today_seconds: 900,
            today_reason: "held to the weekly ceiling: 1.10 in of the 1.30 in target \
                           delivered in the last 7 days, 0.20 in of headroom left"
                .into(),
            ..Default::default()
        };
        assert_eq!(
            ceiling_note_text(&b, p).as_deref(),
            Some(
                "Today's run is held to the weekly target: sprinklers delivered 1.10\" of \
                 the 1.30\" target in the last 7 days, and today waters the remaining 0.20\"."
            )
        );
        // A metric household reads the same clamp in millimeters.
        let metric = crate::components::units_fmt::METRIC;
        let note = ceiling_note_text(&b, metric).unwrap();
        assert!(note.contains("27.9 mm of the 33.0 mm target"), "{note}");
        assert!(
            !note.contains('\"'),
            "no baked inch glyphs for metric: {note}"
        );
        b.today_seconds = 0;
        assert_eq!(
            ceiling_note_text(&b, p),
            None,
            "zero rides the ON HOLD path"
        );
        b.today_seconds = 900;
        b.scheduling_model = "weekly".into();
        assert_eq!(ceiling_note_text(&b, p), None, "shadow flag stays silent");
    }

    /// A wire ceiling reason whose operands do not parse still surfaces
    /// behind the capitalized lead rather than vanishing on the one day
    /// the clamp applies.
    #[test]
    fn ceiling_sentence_degrades_to_the_wire_body() {
        let p = UnitPrefs::default();
        assert_eq!(
            ceiling_sentence("held to the weekly ceiling", p),
            "Today's run is held to the weekly target: held to the weekly ceiling"
        );
    }

    /// The soil hold body composes from the STRUCTURED fields, in the
    /// display unit, with no wire lead-ins: the four shapes are the
    /// defer, the window wait, the under-trigger hold, and the
    /// zero-headroom park.
    #[test]
    fn soil_hold_body_composes_each_shape_unit_aware() {
        let p = UnitPrefs::default();
        let base = WaterBudget {
            scheduling_model: "soil".into(),
            soil_depletion_mm: Some(5.2),
            soil_raw_mm: Some(10.0),
            soil_taw_mm: Some(20.0),
            ..Default::default()
        };
        // Defer-by-deficit: composed from the deficit, no baked mm.
        let mut b = base.clone();
        b.soil_due = true;
        b.soil_deferred_reason =
            Some("deferred: forecast rain refills the deficit (3.2 of 5.2 mm expected)".into());
        b.soil_deferred_kind = Some(crate::engine::soil_schedule::SoilDeferKind::ForecastRain);
        assert_eq!(
            soil_hold_body(&b, p).as_deref(),
            Some("the soil model expects forecast rain to cover the 0.20\" deficit")
        );
        let metric = crate::components::units_fmt::METRIC;
        assert_eq!(
            soil_hold_body(&b, metric).as_deref(),
            Some("the soil model expects forecast rain to cover the 5.2 mm deficit")
        );
        // Window admission: counts only, the lead-in stripped.
        let mut b = base.clone();
        b.soil_due = true;
        b.soil_deferred_reason = Some(
            "waits for tomorrow: the morning window fits 1 of 3 zones that need water, \
             most depleted first"
                .into(),
        );
        b.soil_deferred_kind = Some(crate::engine::soil_schedule::SoilDeferKind::Window);
        assert_eq!(
            soil_hold_body(&b, p).as_deref(),
            Some(
                "the morning window fits 1 of 3 zones that need water, most depleted \
                 first; this zone waits for tomorrow"
            )
        );
        // Not due: trigger and current deficit, both converted.
        let b = base.clone();
        assert_eq!(
            soil_hold_body(&b, p).as_deref(),
            Some(
                "the soil is wet enough. Waters when the deficit reaches 0.39\"; it is \
                 at 0.20\" now"
            )
        );
        // Due, parked at zero headroom under the explicit weekly target.
        let mut b = base.clone();
        b.soil_due = true;
        b.soil_ceiling_binding = true;
        assert_eq!(
            soil_hold_body(&b, p).as_deref(),
            Some("the weekly target leaves no headroom this week")
        );
        // A weekly row, a watering row, and a row with no soil block all
        // fall back to the wire reason path.
        let mut b = base.clone();
        b.scheduling_model = "weekly".into();
        assert_eq!(soil_hold_body(&b, p), None);
        let mut b = base.clone();
        b.today_seconds = 600;
        assert_eq!(soil_hold_body(&b, p), None);
        let mut b = base;
        b.soil_depletion_mm = None;
        assert_eq!(soil_hold_body(&b, p), None);
    }

    #[test]
    fn a_zone_held_by_both_gates_reports_both() {
        // An Override schedule zeroes the plan AFTER the allocator sized it,
        // so a zone can be spaced AND Override-suppressed at once. Naming
        // only the schedule meant lifting the Override left the zone dry with
        // nothing on screen saying why.
        let both = hold_reason_text(
            Some(&supp(&[0, 1, 2, 3, 4, 5, 6], &["Lawn Days"], true)),
            Some(
                "spaced 1 day(s) after the last session; sessions run 3 day(s) apart at 2 per week",
            ),
            false,
        );
        assert!(
            both.contains("the Override schedule Lawn Days covers today."),
            "{both}"
        );
        assert!(
            both.contains("The weekly budget also held it: spaced 1 day(s)"),
            "{both}"
        );

        // A soil-governed zone names the model that also held it.
        let soil_both = hold_reason_text(
            Some(&supp(&[1], &["Lawn Days"], true)),
            Some("the soil model expects forecast rain to cover the 0.20\" deficit"),
            true,
        );
        assert!(
            soil_both.contains("The soil model also held it: the soil model expects"),
            "{soil_both}"
        );

        // Override only: one sentence, unchanged.
        let sched_only = hold_reason_text(Some(&supp(&[1], &["Lawn Days"], true)), None, false);
        assert_eq!(
            sched_only,
            "Not watering today: the Override schedule Lawn Days covers today."
        );

        // Allocator only: the schedule is not active today, so it does not
        // outrank anything.
        let budget_only = hold_reason_text(
            Some(&supp(&[1], &["Lawn Days"], false)),
            Some("covered by prior watering"),
            false,
        );
        assert_eq!(budget_only, "Not watering today: covered by prior watering");

        // An unnamed schedule still reads, and still carries the second gate.
        let unnamed =
            hold_reason_text(Some(&supp(&[1], &[], true)), Some("budget mode off"), false);
        assert!(
            unnamed.starts_with("Not watering today: a manual schedule set to Override"),
            "{unnamed}"
        );
        assert!(
            unnamed.ends_with("The weekly budget also held it: budget mode off"),
            "{unnamed}"
        );
    }
}
