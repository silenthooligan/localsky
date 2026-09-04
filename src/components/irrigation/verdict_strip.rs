// 7-day forward verdict strip. Each cell shows: weekday, weather glyph,
// temp range, precip × probability, and a colored border keyed to the
// predicted verdict, following the one-meaning-per-color language (Task D):
// teal=run, blue=skip-rain/wet, red=skip-freeze and skip-wind (the
// alert/stop family), amber=run-extended (heat). Clicking/hovering a cell
// surfaces the reason. The data comes from the server-precomputed
// `seven_day_verdicts` field on IrrigationSnapshot, so the strip
// reflects exactly what the morning skip-check would decide if today's
// conditions matched that day's forecast.

use crate::components::forecast::glyph::weather_code_glyph;
use crate::components::ui::HelpHint;
use crate::components::units_fmt::{
    fmt_rain_amount, fmt_temp_short, temp_unit, temp_value, use_unit_prefs, UnitPrefs,
};
use crate::ha::snapshot::{DayVerdict, IrrigationSnapshot};
use crate::timefmt::format_wday_short;
use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;

#[component]
pub fn VerdictStrip(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    let prefs = use_unit_prefs();
    view! {
        <section class="verdict-strip">
            <header class="verdict-strip-head">
                <h3 class="verdict-strip-title">
                    "7-Day Verdict"
                    <HelpHint topic="verdict-strip"/>
                </h3>
                <span class="verdict-strip-subtitle">
                    "Predicted skip / run for today + 6 days, same engine as the morning check"
                </span>
                <SourceFreshnessPill snap/>
            </header>
            <div class="verdict-strip-cells" role="region" aria-live="polite" aria-label="7-day irrigation verdict">
                {move || {
                    // Use a plain iter+collect rather than <For>. The strip
                    // is a fixed-shape 7-cell layout; the SSR-rendered DOM
                    // and the hydrate-initial Vec wouldn't reconcile cleanly
                    // through <For>'s keyed reconciler (SSR has 7 cells,
                    // hydrate starts with an empty Vec until the SSE
                    // snapshot arrives, that's a structural mismatch the
                    // <For> reconciler can't bridge without nuking the
                    // root subtree's other event handlers, which was
                    // killing the top-nav click handlers).
                    let p = prefs.get();
                    let s = snap.get();
                    // This strip is the irrigation page's LEAD element, so a
                    // blank band before the first snapshot reads as broken.
                    // Render 7 shimmer cells (the same skeleton primitive the
                    // daily-forecast row uses) until the verdicts arrive.
                    if s.seven_day_verdicts.is_empty() {
                        // ForecastPending renders 7 shimmer tiles (the grid
                        // sizes them via display:contents) for a grace period,
                        // then an honest "provider hasn't answered" note, so a
                        // forecast outage doesn't shimmer here forever. The
                        // verdicts need a forecast to exist, so "no verdicts"
                        // past the grace window IS a forecast-supply problem.
                        return vec![view! {
                            <crate::components::forecast::ForecastPending
                                variant="tiles"
                                what="verdict outlook"
                            />
                        }
                        .into_any()];
                    }
                    // Deployment IANA tz for the weekday labels (24h local,
                    // not the viewer's browser zone). Empty -> browser-local.
                    let tz = s.timezone.clone();
                    s.seven_day_verdicts
                        .into_iter()
                        .map(|v| view! { <VerdictCell v=v prefs=p tz=tz.clone()/> }.into_any())
                        .collect::<Vec<_>>()
                }}
            </div>
        </section>
    }
}

#[component]
fn VerdictCell(v: DayVerdict, prefs: UnitPrefs, tz: String) -> impl IntoView {
    let weekday = format_weekday(v.time_epoch, v.day_offset, &tz);
    let glyph = weather_code_glyph(v.weather_code, true).0;
    let kind_class = match v.verdict.as_str() {
        "skip" => verdict_skip_class(&v),
        "run_extended" => "verdict-cell-extended",
        _ => "verdict-cell-run",
    };
    let cls = format!("verdict-cell {}", kind_class);
    // The DayVerdict carries the engine's per-day SkipCheck operands only via the
    // baked reason (the 7-day strip doesn't carry per-cell operands), so the
    // tooltip uses the baked reason text. Classification keys on reason_code.
    let tooltip = if v.reason.is_empty() {
        format!("{weekday}: run")
    } else {
        format!("{weekday}: {} - {}", v.verdict, v.reason)
    };
    // temp_max_f / temp_min_f are °F; route through the unit formatter.
    let temp_str = format!(
        "{}/{}",
        fmt_temp_short(v.temp_max_f, prefs),
        fmt_temp_short(v.temp_min_f, prefs)
    );
    // precip_in is INCHES; route through the unit formatter. The percent is
    // omitted when the provider reported no probability (the old bare 0
    // claimed "0% chance").
    let rain_str = match v.precip_probability_max {
        Some(prob) => format!("{} · {prob}%", fmt_rain_amount(v.precip_in, prefs)),
        None => fmt_rain_amount(v.precip_in, prefs),
    };
    let tag = verdict_short_label(&v);
    // Full-narration label for screen readers. Color + tag carry the
    // same intent visually; the aria-label folds the temp + rain
    // context into a single sentence so a non-sighted user gets the
    // same at-a-glance summary.
    let aria = format!(
        "{weekday}: {tag_lower}, {rain_str}, high {temp_max} {unit}, low {temp_min} {unit}",
        tag_lower = tag.to_lowercase(),
        temp_max = temp_value(v.temp_max_f, prefs),
        temp_min = temp_value(v.temp_min_f, prefs),
        unit = temp_unit(prefs),
    );
    view! {
        <div class=cls title=tooltip role="group" aria-label=aria>
            <div class="verdict-cell-day">{weekday}</div>
            <div class="verdict-cell-glyph" aria-hidden="true">
                <crate::components::ui::Icon name=glyph size=22/>
            </div>
            <div class="verdict-cell-temp" aria-hidden="true">{temp_str}</div>
            <div class="verdict-cell-rain" aria-hidden="true">{rain_str}</div>
            <div class="verdict-cell-tag" aria-hidden="true">{tag}</div>
        </div>
    }
}

/// Inline pill that leads with the HANDLED state of the rain reading, the
/// strip's load-bearing input. It reads the SAME taxonomy surface the STATUS
/// agent exposes (the snapshot's live `field_sources` ownership plus the rain
/// owner's honest `forecast.rain_nature`), NOT a `now - last_seen` staleness
/// bucket, so it never says "stale" while a source is actually covering rain.
///
/// It names the active rain owner and its nature ("Rain: Open-Meteo (forecast)"
/// / "Rain: NWS (measured)") when someone owns the rain field, a calm "All
/// readings covered" when the rain reading is served but the provider label is
/// not yet stamped, and goes AMBER only on a genuine coverage gap: nothing owns
/// the rain field AND no forecast provider can stand in (zero possible owner).
/// The chain falling through one source to the next is the engine working, not
/// a fault, so this pill stays calm through every fall-through.
#[component]
fn SourceFreshnessPill(snap: ReadSignal<IrrigationSnapshot>) -> impl IntoView {
    // The live rain owner per the snapshot `field_sources` map. The merge stamps
    // the rain reading under the accumulation field key `rain_today_in` (the
    // same canonical name the per-field picker + /api/health read), with the
    // value being the FRIENDLY source label ("NWS", "Open-Meteo", "Tempest").
    // Empty means no source owns the rain field right now.
    let rain_owner = move || {
        snap.with(|s| s.field_sources.get("rain_today_in").cloned())
            .filter(|l| !l.is_empty())
    };
    // The forecast provider standing behind the rain reading when no live owner
    // is stamped yet (the calm cloud-only / warming-up posture): a non-empty
    // label here means rain still HAS a possible owner, so this is never a gap.
    let forecast_label = move || {
        snap.with(|s| s.forecast.forecast_source_label.clone())
            .trim()
            .to_string()
    };
    // The honest nature WORD for the live rain owner, off the snapshot's
    // `forecast.rain_nature` (Measured gauge / RadarQpe radar / Model forecast).
    // Never says "live" on a model fill; matches the cloud-weather rain badge
    // vocabulary so every surface speaks the same words.
    let nature_word = move || -> &'static str {
        match snap.with(|s| s.forecast.rain_nature) {
            crate::ha::snapshot::RainNature::Measured => "measured",
            crate::ha::snapshot::RainNature::RadarQpe => "radar",
            crate::ha::snapshot::RainNature::Model => "forecast",
        }
    };
    // A genuine coverage gap: NO source owns the rain field AND no forecast
    // provider can stand in for it. This is the ONLY amber condition. A source
    // covering rain (live owner OR forecast fill) is always calm, even mid
    // fall-through.
    let rain_gap = move || rain_owner().is_none() && forecast_label().is_empty();
    // The handled-state line. Owner present -> name it + its nature. No owner but
    // a forecast provider stands behind it -> name that provider as the rain
    // source (still covered, calm). No owner and no provider -> the amber gap.
    let line = move || {
        if let Some(owner) = rain_owner() {
            format!("Rain: {owner} ({})", nature_word())
        } else {
            let fl = forecast_label();
            if !fl.is_empty() {
                format!("Rain: {fl} (forecast)")
            } else {
                "No rain source yet".to_string()
            }
        }
    };
    view! {
        <span
            class="verdict-strip-freshness"
            class:verdict-strip-freshness-stale=rain_gap
            title=move || {
                if rain_gap() {
                    "No source can provide rain for this location yet. Add a cloud \
                     weather service (NWS or Open-Meteo) under Devices to cover it."
                        .to_string()
                } else if let Some(owner) = rain_owner() {
                    format!("Rain reading is covered by {owner} ({}).", nature_word())
                } else {
                    format!("Rain reading is covered by {} (forecast).", forecast_label())
                }
            }
            aria-label=move || {
                if rain_gap() {
                    "No rain source for this location. Add a cloud weather service \
                     to cover it."
                        .to_string()
                } else {
                    format!("Rain reading covered. {}", line())
                }
            }
        >
            {move || format!("● {}", if rain_gap() { "No rain source".to_string() } else { line() })}
        </span>
    }
}

/// Map the day-offset + epoch to "TODAY" / "TOM" / "Wed" / etc. Always
/// renders something even if the epoch is 0 (forecast not yet loaded). The
/// weekday is rendered in the deployment's IANA `tz` (24h local), not the
/// viewer's browser zone, so a traveling viewer sees the deployment's calendar
/// day; empty `tz` falls back to browser-local (hydrate) / UTC (ssr).
fn format_weekday(epoch: i64, offset: u32, tz: &str) -> String {
    if offset == 0 {
        return "TODAY".to_string();
    }
    if offset == 1 {
        return "TOM".to_string();
    }
    if epoch == 0 {
        return format!("+{offset}");
    }
    let wd = format_wday_short(epoch, tz);
    if wd.is_empty() {
        format!("+{offset}")
    } else {
        wd.to_uppercase()
    }
}

/// Drill the skip into a more specific class so the cell border communicates the
/// rule that fired without needing a tooltip. Keys on the structured
/// `reason_code` (P2 units architecture) so the class is unit-independent; legacy
/// cells with an empty code fall back to the baked-reason substring match.
fn verdict_skip_class(v: &DayVerdict) -> &'static str {
    match v.reason_code.as_str() {
        "freeze_now" | "overnight_freeze" | "soil_frost" => "verdict-cell-skip-freeze",
        "wind_now" | "wind_forecast" => "verdict-cell-skip-wind",
        "rain_now" | "already_wet" | "observed_rain" | "rain_next_4h" | "tomorrow_rain"
        | "rain_3day" => "verdict-cell-skip-rain",
        "paused" | "pause_until" => "verdict-cell-skip-pause",
        "" => {
            // Legacy row (no code): classify from the baked reason text.
            let r = v.reason.to_lowercase();
            if r.contains("freeze") {
                "verdict-cell-skip-freeze"
            } else if r.contains("wind") {
                "verdict-cell-skip-wind"
            } else if r.contains("rain") || r.contains("wet") {
                "verdict-cell-skip-rain"
            } else if r.contains("paused") {
                "verdict-cell-skip-pause"
            } else {
                "verdict-cell-skip"
            }
        }
        _ => "verdict-cell-skip",
    }
}

/// One-word tag for the cell footer. Echoes the verdict but condensed; keys on
/// `reason_code` with a baked-reason fallback for legacy cells. Two
/// soil-model shapes carry their own tag, because the title attribute
/// has no touch affordance and a hover-only narration is invisible on
/// mobile: a cell the inert-gate demotion rewrote to a run reads SOIL
/// (run tint, the class already keys on the verdict), and a
/// mixed-install rain skip that holds only the Weekly-model zones reads
/// RAIN* so the cell itself says the skip is partial (the Week page
/// carries the full sentence).
fn verdict_short_label(v: &DayVerdict) -> &'static str {
    match v.verdict.as_str() {
        "run_extended" => "EXTEND",
        "skip" => match crate::gates_catalog::gate_family(&v.reason_code) {
            crate::gates_catalog::GateFamily::Freeze => "FREEZE",
            crate::gates_catalog::GateFamily::Wind => "WIND",
            crate::gates_catalog::GateFamily::Water => {
                if v.mixed_hold {
                    "RAIN*"
                } else {
                    "RAIN"
                }
            }
            crate::gates_catalog::GateFamily::Pause => "PAUSE",
            // A restriction is a legal block, not weather; the Week page
            // spells it out, the strip keeps the neutral tag.
            crate::gates_catalog::GateFamily::Restriction => "SKIP",
            crate::gates_catalog::GateFamily::SoilModel => "SOIL",
            crate::gates_catalog::GateFamily::NoData => "NO DATA",
            crate::gates_catalog::GateFamily::Other if v.reason_code.is_empty() => {
                let r = v.reason.to_lowercase();
                if r.contains("freeze") {
                    "FREEZE"
                } else if r.contains("wind") {
                    "WIND"
                } else if r.contains("rain") || r.contains("wet") {
                    "RAIN"
                } else if r.contains("paused") {
                    "PAUSE"
                } else {
                    "SKIP"
                }
            }
            crate::gates_catalog::GateFamily::Other => "SKIP",
        },
        _ if v.reason_code == "soil_model" => "SOIL",
        _ => "RUN",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(verdict: &str, code: &str, reason: &str) -> DayVerdict {
        DayVerdict {
            verdict: verdict.into(),
            reason_code: code.into(),
            reason: reason.into(),
            ..Default::default()
        }
    }

    /// The demotion and the mixed partial hold are visible IN the cell,
    /// not only in the hover title: a demoted cell reads SOIL on the run
    /// tint, a mixed-install rain hold reads RAIN*, and a plain run or a
    /// yard-wide rain skip keeps its shipped tag.
    #[test]
    fn soil_model_cells_carry_their_own_tags() {
        let demoted = cell(
            "run",
            "soil_model",
            "Waters anyway: soil zones already count this forecast rain against their \
             deficit. (Rain expected)",
        );
        assert_eq!(verdict_short_label(&demoted), "SOIL");
        // The partial hold is a FIELD now, not a phrase inside the
        // sentence: the strip reads what the refresher set rather than
        // searching the prose for the mixed-install note.
        let mut mixed = cell(
            "skip",
            "tomorrow_rain",
            &format!("Rain expected. {}", crate::ha::snapshot::MIXED_SKIP_NOTE),
        );
        mixed.mixed_hold = true;
        assert_eq!(verdict_short_label(&mixed), "RAIN*");
        // The note alone no longer makes the tag: without the flag this
        // is a whole-yard hold.
        let note_only = cell(
            "skip",
            "tomorrow_rain",
            &format!("Rain expected. {}", crate::ha::snapshot::MIXED_SKIP_NOTE),
        );
        assert_eq!(verdict_short_label(&note_only), "RAIN");
        let plain_skip = cell("skip", "tomorrow_rain", "Rain expected");
        assert_eq!(verdict_short_label(&plain_skip), "RAIN");
        let plain_run = cell("run", "run", "");
        assert_eq!(verdict_short_label(&plain_run), "RUN");
    }
}
