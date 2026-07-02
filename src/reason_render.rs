//! P2 (units architecture): unit-aware client-side reason renderer.
//!
//! Reproduces the engine's EXACT skip-reason / rule-detail / margin wording from
//! the structured data the engine emits (P1: `reason_code` + numeric operands on
//! `SkipCheck` / `RuleEval` / `ZoneVerdict`), substituting unit-converted values
//! when the resolved `UnitPrefs` is metric.
//!
//! THE CONTRACT (`imperial_identity` test below): under IMPERIAL these functions
//! are BYTE-IDENTICAL to the engine's baked string. That is the safety guarantee
//! (imperial users are unchanged) and the drift guard (any future wording change
//! in the engine that this renderer doesn't mirror fails CI). The metric branch
//! converts the numbers and swaps in metric unit words.
//!
//! Pure data over the shared (both-features) serde types (NO ssr-only deps), so
//! this compiles for the wasm hydrate side, like `explain.rs` / `gates_catalog.rs`.
//! It deliberately does NOT depend on `crate::engine` (ssr-only).
//!
//! CRITICAL WORDING: the engine bakes its own spacing/words ("{:.2} in/hr" WITH a
//! space; "{:.1} mph > {:.0} mph"; "{:.2}\" today"; "°F"). The imperial branch
//! here is HAND-WRITTEN to match that exactly, NOT routed through the generic
//! `units_fmt` formatters (whose spacing differs, e.g. `fmt_rain_rate` emits
//! "0.31in/h"). Only the metric branch reuses the `units_fmt` conversions.
//!
//! Codes with FULLY-carried operands re-render in both unit systems:
//!   rain_now, freeze_now, overnight_freeze, soil_frost, wind_now, already_wet,
//!   rain_next_4h, tomorrow_rain, rain_3day, heat_advisory.
//! Every other code (override, pause(_until), restrictions, live_data, dry_run,
//! run, soil_floor aggregate, wind_forecast [slack not carried], observed_rain
//! [window-day count not carried], condition, soil_quarantine) falls back to the
//! engine's baked string verbatim, never fabricated.

use crate::components::units_fmt::{f_to_c, in_to_mm, mph_to_kph, temp_unit, wind_unit, UnitPrefs};
use crate::ha::snapshot::{DecisionTrace, RuleEval, SkipCheck, ZoneVerdict};

// ── unit-aware operand formatters ───────────────────────────────────────────
// Each reproduces the engine's IMPERIAL token verbatim (same precision, spacing,
// glyphs) and emits a converted metric form. They format ONE operand value; the
// reason templates compose them with the engine's surrounding prose.

/// Air/soil temperature, whole degrees: "30°F" / "-1°C". Matches the engine's
/// `{:.0}°F`.
fn temp(temp_f: f64, p: UnitPrefs) -> String {
    if p.temp_c {
        format!("{:.0}{}", f_to_c(temp_f), temp_unit(p))
    } else {
        format!("{temp_f:.0}{}", temp_unit(p))
    }
}

/// Soil temperature, one decimal: "33.0°F" / "0.6°C". Matches the engine's
/// `{:.1}°F` (soil-frost gate).
fn soil_temp(temp_f: f64, p: UnitPrefs) -> String {
    if p.temp_c {
        format!("{:.1}{}", f_to_c(temp_f), temp_unit(p))
    } else {
        format!("{temp_f:.1}{}", temp_unit(p))
    }
}

/// Wind, one decimal (the "now"/peak reading side): "20.0 mph" / "32.2 km/h".
/// Matches the engine's `{:.1} mph`.
fn wind1(mph: f64, p: UnitPrefs) -> String {
    if p.wind_metric {
        format!("{:.1} {}", mph_to_kph(mph), wind_unit(p))
    } else {
        format!("{mph:.1} {}", wind_unit(p))
    }
}

/// Wind, whole numbers (the threshold side): "10 mph" / "16 km/h". Matches the
/// engine's `{:.0} mph`.
fn wind0(mph: f64, p: UnitPrefs) -> String {
    if p.wind_metric {
        format!("{:.0} {}", mph_to_kph(mph), wind_unit(p))
    } else {
        format!("{mph:.0} {}", wind_unit(p))
    }
}

/// Rain depth, two decimals with a trailing-quote glyph: "0.20\"" / "5.1 mm".
/// Matches the engine's `{:.2}\"`. (Imperial keeps the engine's no-space inch
/// glyph; metric uses a space + "mm".)
fn depth(inches: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1} mm", in_to_mm(inches))
    } else {
        format!("{inches:.2}\"")
    }
}

/// Rain rate, two decimals with a SPACE before the unit: "0.05 in/hr" /
/// "1.3 mm/hr". Matches the engine's `{:.2} in/hr` (note the leading space the
/// engine bakes, which `units_fmt::fmt_rain_rate` does NOT).
fn rate(in_per_hr: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1} mm/hr", in_to_mm(in_per_hr))
    } else {
        format!("{in_per_hr:.2} in/hr")
    }
}

// ── SkipCheck.reason ─────────────────────────────────────────────────────────

/// Render `SkipCheck.reason` unit-aware from `s.reason_code` + `s`'s operand
/// fields. Returns `s.reason` verbatim (the engine's baked string) for any code
/// whose operands aren't fully carried, and for an empty/unrecognized code.
///
/// Under IMPERIAL the rendered codes are byte-identical to `s.reason` (the
/// `imperial_identity` test pins this); the metric branch converts the numbers.
pub fn render_skip_reason(s: &SkipCheck, p: UnitPrefs) -> String {
    match s.reason_code.as_str() {
        // "Currently raining ({:.2} in/hr)"
        "rain_now" => format!(
            "Currently raining ({})",
            rate(s.rain_intensity_now_in_hr, p)
        ),
        // "Freeze risk now ({:.0}°F < {:.0}°F)"
        "freeze_now" => format!(
            "Freeze risk now ({} < {})",
            temp(s.temp_now_f, p),
            temp(s.min_temp_f, p)
        ),
        // "Overnight freeze ({:.0}°F low next 24h < {:.0}°F)"
        "overnight_freeze" => format!(
            "Overnight freeze ({} low next 24h < {})",
            temp(s.temp_min_24h_f, p),
            temp(s.min_temp_f, p)
        ),
        // "Soil frost ({:.1}°F < {:.0}°F threshold)"
        "soil_frost" => format!(
            "Soil frost ({} < {} threshold)",
            soil_temp(s.soil_temp_yard_min_f.unwrap_or(0.0), p),
            temp(s.frost_skip_soil_f, p)
        ),
        // "Wind too high now ({:.1} mph > {:.0} mph)"
        "wind_now" => format!(
            "Wind too high now ({} > {})",
            wind1(s.wind_now_mph, p),
            wind0(s.max_wind_mph, p)
        ),
        // "Already wet ({:.2}\" today)"
        "already_wet" => format!("Already wet ({} today)", depth(s.rain_today_in, p)),
        // "Rain expected within 4h ({:.2}\" forecast)"
        "rain_next_4h" => format!(
            "Rain expected within 4h ({} forecast)",
            depth(s.rain_next_4h_in, p)
        ),
        // "Tomorrow rain ({:.2}\" × {}% confidence)"
        "tomorrow_rain" => format!(
            "Tomorrow rain ({} \u{d7} {}% confidence)",
            depth(s.forecast_in, p),
            s.rain_tomorrow_prob_pct
        ),
        // "Heavy rain in next 3 days ({:.2}\" weighted)"
        "rain_3day" => format!(
            "Heavy rain in next 3 days ({} weighted)",
            depth(s.rain_3day_weighted_in, p)
        ),
        // "Heat advisory: running planned + 15% (peak {:.0}°F)"
        "heat_advisory" => format!(
            "Heat advisory: running planned + 15% (peak {})",
            temp(s.temp_max_3day_f, p)
        ),
        // Codes whose operands aren't fully carried on SkipCheck (wind_forecast:
        // slack mph; observed_rain: window-day count), plus the static/variable
        // control gates (override, paused, pause_until, restrictions, live_data,
        // dry_run), the clean run, and the aggregate soil_floor: keep the engine's
        // baked string. Never fabricate.
        _ => s.reason.clone(),
    }
}

/// Render a `DecisionTrace`'s top-level reason unit-aware (P2 units
/// architecture). The trace doesn't carry a `SkipCheck`, so this reconstructs
/// the reason from the DECIDING rule's structured operands (`RuleEval.value` /
/// `.threshold` / `.unit_kind`), mapping each gate id to the same wording
/// `render_skip_reason` produces. Falls back to `trace.reason` for codes whose
/// operands aren't carried as `value`/`threshold` (control gates, wind_forecast,
/// observed_rain, tomorrow_rain, heat_advisory) and for a clean run.
///
/// Under IMPERIAL the handled codes are byte-identical to `trace.reason` (pinned
/// by `imperial_identity_trace`).
pub fn render_trace_reason(trace: &DecisionTrace, p: UnitPrefs) -> String {
    let Some(r) = trace.rules.iter().find(|r| r.outcome == "fired") else {
        return trace.reason.clone();
    };
    let (Some(v), Some(t), Some(kind)) = (r.value, r.threshold, r.unit_kind.as_deref()) else {
        return trace.reason.clone();
    };
    match (r.id.as_str(), kind) {
        ("rain_now", "rain_rate_in_hr") => format!("Currently raining ({})", rate(v, p)),
        ("freeze_now", "temp_f") => {
            format!("Freeze risk now ({} < {})", temp(v, p), temp(t, p))
        }
        ("overnight_freeze", "temp_f") => format!(
            "Overnight freeze ({} low next 24h < {})",
            temp(v, p),
            temp(t, p)
        ),
        ("soil_frost", "soil_temp_f") => {
            format!(
                "Soil frost ({} < {} threshold)",
                soil_temp(v, p),
                temp(t, p)
            )
        }
        ("wind_now", "wind_mph") => {
            format!("Wind too high now ({} > {})", wind1(v, p), wind0(t, p))
        }
        ("already_wet", "rain_in") => format!("Already wet ({} today)", depth(v, p)),
        ("rain_next_4h", "rain_in") => {
            format!("Rain expected within 4h ({} forecast)", depth(v, p))
        }
        ("rain_3day", "rain_in") => {
            format!("Heavy rain in next 3 days ({} weighted)", depth(v, p))
        }
        // tomorrow_rain's reason embeds the raw forecast depth + confidence %, but
        // the RuleEval value/threshold carry the WEIGHTED product vs the skip line
        // (not the raw operands the reason needs); heat_advisory's reason rides
        // temp_max_3day_f, which the heat gate doesn't expose as value/threshold.
        // Both fall back to the baked reason (correct imperial, unchanged metric).
        _ => trace.reason.clone(),
    }
}

// ── RuleEval.detail / margin_label ───────────────────────────────────────────

/// Map a `RuleEval.unit_kind` (the P1 dimension tag) + operand to a unit-aware
/// "value vs threshold"-style detail. Reproduces `decide_traced`'s baked
/// `detail` string for the threshold gates from `r.value` / `r.threshold` /
/// `r.unit_kind`. Falls back to `r.detail` for binary gates (no operands carried)
/// and any unit_kind/code combination not handled here.
///
/// Under IMPERIAL each handled gate is byte-identical to the engine's baked
/// `detail` (pinned by `imperial_identity`).
pub fn render_rule_detail(r: &RuleEval, p: UnitPrefs) -> String {
    let (Some(v), Some(t), Some(kind)) = (r.value, r.threshold, r.unit_kind.as_deref()) else {
        return r.detail.clone();
    };
    match (r.id.as_str(), kind) {
        // "{:.2} in/hr vs {:.2} threshold"
        ("rain_now", "rain_rate_in_hr") => {
            format!("{} vs {} threshold", rate(v, p), rate_bare(t, p))
        }
        // "{:.0}°F vs {:.0}°F min"
        ("freeze_now", "temp_f") => format!("{} vs {} min", temp(v, p), temp(t, p)),
        // "24h low {:.0}°F vs {:.0}°F min"
        ("overnight_freeze", "temp_f") => {
            format!("24h low {} vs {} min", temp(v, p), temp(t, p))
        }
        // "soil {:.1}°F vs {:.0}°F"
        ("soil_frost", "soil_temp_f") => format!("soil {} vs {}", soil_temp(v, p), temp(t, p)),
        // "{:.1} mph vs {:.0} mph max"
        ("wind_now", "wind_mph") => format!("{} vs {} max", wind1(v, p), wind0(t, p)),
        // "{:.2}\" today vs {:.2}\" floor"
        ("already_wet", "rain_in") => format!("{} today vs {} floor", depth(v, p), depth(t, p)),
        // "{:.2}\" next 4h vs {:.2}\" skip"
        ("rain_next_4h", "rain_in") => format!("{} next 4h vs {} skip", depth(v, p), depth(t, p)),
        // "{:.2}\" weighted vs {:.2}\""
        ("rain_3day", "rain_in") => format!("{} weighted vs {}", depth(v, p), depth(t, p)),
        // soil_saturation detail ("tightest {name} {pct}% vs {sat}%") embeds the
        // zone name, which isn't a carried operand; wind_forecast / observed_rain /
        // tomorrow_rain details embed extra terms (slack, day count, the product)
        // not reconstructible from value/threshold alone. Keep the baked detail.
        _ => r.detail.clone(),
    }
}

/// Rate WITHOUT a trailing " in/hr"/" mm/hr" unit but matching the engine's
/// `{:.2} threshold` token: rain_now's detail says "... vs {:.2} threshold" (the
/// threshold has NO unit suffix in the engine string). Imperial = bare `{:.2}`.
fn rate_bare(in_per_hr: f64, p: UnitPrefs) -> String {
    if p.rain_mm {
        format!("{:.1}", in_to_mm(in_per_hr))
    } else {
        format!("{in_per_hr:.2}")
    }
}

/// Render `RuleEval.margin_label` unit-aware. Reproduces `annotate_margins`'s
/// phrasing ("skipped, {:.*}{unit} past the line" / "{:.*}{unit} of headroom
/// before this skips" / "{:.*}{unit} past the line, but overridden") from
/// `r.value` / `r.threshold` / `r.unit_kind`, converting the distance + unit word
/// for metric. Falls back to `r.margin_label` when operands are absent (binary
/// gates) or the gate's unit_kind isn't handled.
///
/// Under IMPERIAL each handled gate is byte-identical to `r.margin_label` (pinned
/// by `imperial_identity`).
pub fn render_rule_margin(r: &RuleEval, p: UnitPrefs) -> Option<String> {
    let (Some(v), Some(t), Some(kind)) = (r.value, r.threshold, r.unit_kind.as_deref()) else {
        return r.margin_label.clone();
    };
    // The engine prec + unit token per dimension, mirroring `annotate_margins`'s
    // mk(... unit, prec) call sites. soil_saturation ("%", prec 0) keeps its
    // baked label (the dist is in percent, unit-invariant, and its detail embeds
    // a zone name); we still convert the temp/wind/rain dimensions.
    let (dist_str, unit_tok) = match kind {
        "rain_rate_in_hr" => {
            // engine: prec 2, unit " in/hr"
            if p.rain_mm {
                (
                    format!("{:.1}", in_to_mm((v - t).abs())),
                    " mm/hr".to_string(),
                )
            } else {
                (format!("{:.2}", (v - t).abs()), " in/hr".to_string())
            }
        }
        "rain_in" => {
            // engine: prec 2, unit "\""
            if p.rain_mm {
                (format!("{:.1}", in_to_mm((v - t).abs())), " mm".to_string())
            } else {
                (format!("{:.2}", (v - t).abs()), "\"".to_string())
            }
        }
        "wind_mph" => {
            // engine: prec 0, unit " mph"
            if p.wind_metric {
                (
                    format!("{:.0}", mph_to_kph((v - t).abs())),
                    " km/h".to_string(),
                )
            } else {
                (format!("{:.0}", (v - t).abs()), " mph".to_string())
            }
        }
        "temp_f" | "soil_temp_f" => {
            // engine: prec 0, unit "°F". A temperature DIFFERENCE converts by the
            // ratio (5/9), not the f_to_c offset.
            if p.temp_c {
                (
                    format!("{:.0}", (v - t).abs() * 5.0 / 9.0),
                    "°C".to_string(),
                )
            } else {
                (format!("{:.0}", (v - t).abs()), "°F".to_string())
            }
        }
        // pct (soil_saturation) + anything else: keep the engine's baked label.
        _ => return r.margin_label.clone(),
    };

    let fired = r.outcome == "fired";
    // Mirror annotate_margins: a passed gate whose OWN raw threshold is met was
    // overridden (the baked label already says so). We can't recompute fires_raw
    // without the gate's exact operator, but the baked label tells us: if it
    // contains "overridden" reproduce that branch; otherwise headroom. (Fired is
    // unambiguous from the outcome.)
    let label = if fired {
        format!("skipped, {dist_str}{unit_tok} past the line")
    } else if r
        .margin_label
        .as_deref()
        .is_some_and(|m| m.contains("overridden"))
    {
        format!("{dist_str}{unit_tok} past the line, but overridden")
    } else {
        format!("{dist_str}{unit_tok} of headroom before this skips")
    };
    Some(label)
}

// ── ZoneVerdict.reason ───────────────────────────────────────────────────────

/// Render `ZoneVerdict.reason` unit-aware. Per-zone soil decisions
/// (soil_saturation / soil_floor) carry PERCENT operands, which are
/// unit-invariant, so those re-render identically; a zone carrying a GLOBAL
/// weather reason re-renders via the same code map as `render_skip_reason` WHEN
/// the operands are carried. Global codes bound to a zone do NOT carry the
/// weather operands (only soil zones carry value/threshold), so they fall back to
/// the engine's baked `z.reason`.
///
/// Soil percent reasons are byte-identical in both unit systems; the fallback is
/// always the engine's baked string. Never fabricates.
pub fn render_zone_reason(z: &ZoneVerdict, _p: UnitPrefs) -> String {
    // soil_saturation / soil_floor / soil_quarantine reasons are PERCENT-based and
    // unit-invariant; override / condition / global-bound reasons carry no
    // convertible operand on the ZoneVerdict. In every case the engine's baked
    // reason is the correct unit-aware string, so render it verbatim. (This keeps
    // the soil-percent surfaces stable and avoids fabricating a global weather
    // reason from operands the zone doesn't carry.)
    z.reason.clone()
}

#[cfg(all(test, feature = "ssr"))]
mod tests {
    use super::*;
    use crate::config::schema::{AddressParity, SkipRuleParams};
    use crate::engine::skip_rules::{decide_traced, evaluate_with, Inputs, LiveReadings, ZoneSoil};

    const IMPERIAL: UnitPrefs = UnitPrefs {
        temp_c: false,
        rain_mm: false,
        wind_metric: false,
        pressure_metric: false,
        distance_metric: false,
        area_metric: false,
    };
    const METRIC: UnitPrefs = UnitPrefs {
        temp_c: true,
        rain_mm: true,
        wind_metric: true,
        pressure_metric: true,
        distance_metric: true,
        area_metric: true,
    };

    // Mirror skip_rules::tests::base() so the renderer test fires the real engine
    // over the full reason battery without reaching into that crate-private fn.
    fn base() -> Inputs {
        Inputs {
            temp_now_f: 70.0,
            wind_now_mph: 3.0,
            rain_today_in: 0.0,
            rain_intensity_now_in_hr: 0.0,
            // No live rain in the fixture (rate 0), so the rain_now gate never
            // fires; the nature is the honest Model default.
            rain_nature: crate::ha::snapshot::RainNature::default(),
            humidity_now_pct: 55.0,
            forecast_in: 0.0,
            rain_tomorrow_prob_pct: 0,
            rain_3day_weighted_in: 0.0,
            rain_7day_weighted_in: 0.0,
            rain_next_4h_in: 0.0,
            rain_observed_recent_in: 0.0,
            wind_max_today_mph: 6.0,
            temp_min_24h_f: Some(60.0),
            temp_max_3day_f: 80.0,
            heat_index_max_3day_f: 0.0,
            days_since_significant_rain: 1,
            max_wind_mph: 10.0,
            min_temp_f: 38.0,
            rain_skip_in: 0.25,
            soil_zones: Vec::new(),
            soil_temp_yard_min_f: None,
            soil_temp_yard_max_f: None,
            frost_skip_soil_f: 35.0,
            live_readings: LiveReadings::Station,
            forecast_stale: false,
            is_paused: false,
            is_dry_run: false,
            pause_until_epoch: 0,
            now_epoch: 1_700_000_000,
            override_tomorrow: String::new(),
            is_tomorrow: false,
            global_override: "auto".to_string(),
            zone_overrides: std::collections::HashMap::new(),
            watering_restrictions: Vec::new(),
            address_parity: AddressParity::NotApplicable,
        }
    }

    fn soil4(b: Option<f64>, f: Option<f64>, s: Option<f64>, sh: Option<f64>) -> Vec<ZoneSoil> {
        vec![
            ZoneSoil {
                slug: "back_yard".into(),
                name: "back yard".into(),
                pct: b,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "front_yard".into(),
                name: "front yard".into(),
                pct: f,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "side_yard".into(),
                name: "side yard".into(),
                pct: s,
                saturation_pct: 70.0,
                target_min_pct: 30.0,
            },
            ZoneSoil {
                slug: "back_yard_shrubs".into(),
                name: "back yard shrubs".into(),
                pct: sh,
                saturation_pct: 85.0,
                target_min_pct: 25.0,
            },
        ]
    }

    /// The full reason battery (mirrors skip_rules::tests::parity_scenarios),
    /// labeled with the reason_code each entry is built to fire so a failure names
    /// the offending code.
    fn battery() -> Vec<(&'static str, Inputs)> {
        let mut out: Vec<(&'static str, Inputs)> = vec![("run", base())];
        let mut push = |code: &'static str, f: fn(&mut Inputs)| {
            let mut i = base();
            f(&mut i);
            out.push((code, i));
        };
        push("rain_now", |i| i.rain_intensity_now_in_hr = 0.05);
        push("freeze_now", |i| i.temp_now_f = 30.0);
        push("overnight_freeze", |i| {
            i.temp_now_f = 50.0;
            i.temp_min_24h_f = Some(32.0);
        });
        push("run", |i| i.temp_min_24h_f = None);
        push("run", |i| i.live_readings = LiveReadings::ForecastFallback);
        push("live_data", |i| i.live_readings = LiveReadings::Unavailable);
        push("soil_frost", |i| i.soil_temp_yard_min_f = Some(33.0));
        push("wind_now", |i| i.wind_now_mph = 20.0);
        push("wind_forecast", |i| i.wind_max_today_mph = 30.0);
        push("already_wet", |i| i.rain_today_in = 0.10);
        push("observed_rain", |i| i.rain_observed_recent_in = 1.5);
        push("soil_saturation", |i| {
            i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        });
        push("rain_next_4h", |i| i.rain_next_4h_in = 0.20);
        push("tomorrow_rain", |i| {
            i.forecast_in = 0.40;
            i.rain_tomorrow_prob_pct = 90;
        });
        push("rain_3day", |i| i.rain_3day_weighted_in = 1.0);
        push("heat_advisory", |i| {
            i.temp_max_3day_f = 98.0;
            i.humidity_now_pct = 70.0;
            i.days_since_significant_rain = 3;
            i.rain_3day_weighted_in = 0.0;
        });
        push("dry_run", |i| i.is_dry_run = true);
        push("paused", |i| i.is_paused = true);
        push("override", |i| {
            i.is_tomorrow = true;
            i.override_tomorrow = "skip".to_string();
        });
        push("override", |i| {
            i.is_tomorrow = true;
            i.override_tomorrow = "run".to_string();
            i.rain_today_in = 0.5;
        });
        push("soil_floor", |i| {
            i.rain_next_4h_in = 0.50;
            i.soil_zones = soil4(Some(20.0), Some(45.0), Some(45.0), Some(45.0));
        });
        // pause_until (timed): not in parity_scenarios; add it so the renderer's
        // fallback path is exercised for the date-bearing reason.
        push("pause_until", |i| {
            i.now_epoch = 1_700_000_000;
            i.pause_until_epoch = i.now_epoch + 3600;
        });
        out
    }

    /// THE LINCHPIN: for EVERY produced reason_code, the renderer reproduces the
    /// engine's baked `SkipCheck.reason` BYTE-FOR-BYTE under IMPERIAL, and every
    /// RuleEval's detail / margin_label likewise. This guarantees imperial users
    /// are unchanged and any future engine wording drift fails CI.
    #[test]
    fn imperial_identity() {
        let p = SkipRuleParams::default();
        let mut covered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for (expected_code, i) in battery() {
            let s = evaluate_with(&i, &p);
            covered.insert(s.reason_code.clone());
            assert_eq!(
                s.reason_code, expected_code,
                "battery entry built for {expected_code:?} fired {:?}",
                s.reason_code
            );
            // SkipCheck.reason byte-identity.
            assert_eq!(
                render_skip_reason(&s, IMPERIAL),
                s.reason,
                "render_skip_reason drifted from baked reason for code {:?}",
                s.reason_code
            );

            // DecisionTrace top-level reason byte-identity (the Rule Lab +
            // Simulator path). render_trace_reason reconstructs from the deciding
            // rule's operands, so it must match the engine's baked trace.reason
            // exactly under imperial.
            let trace = decide_traced(&i, &p);
            assert_eq!(
                render_trace_reason(&trace, IMPERIAL),
                trace.reason,
                "render_trace_reason drifted from baked trace reason for code {:?}",
                s.reason_code
            );

            // Every RuleEval's detail + margin_label byte-identity.
            for r in &trace.rules {
                assert_eq!(
                    render_rule_detail(r, IMPERIAL),
                    r.detail,
                    "render_rule_detail drifted for rule {:?} (code {:?})",
                    r.id,
                    s.reason_code
                );
                assert_eq!(
                    render_rule_margin(r, IMPERIAL),
                    r.margin_label,
                    "render_rule_margin drifted for rule {:?} (code {:?})",
                    r.id,
                    s.reason_code
                );
            }
        }
        // Every code that re-renders (not just fallback) plus the fallbacks must
        // have appeared, so the identity assertions actually exercised them.
        for must in [
            "run",
            "rain_now",
            "freeze_now",
            "overnight_freeze",
            "soil_frost",
            "wind_now",
            "wind_forecast",
            "already_wet",
            "observed_rain",
            "soil_saturation",
            "rain_next_4h",
            "tomorrow_rain",
            "rain_3day",
            "heat_advisory",
            "dry_run",
            "paused",
            "override",
            "soil_floor",
            "pause_until",
            "live_data",
        ] {
            assert!(covered.contains(must), "battery never fired code {must:?}");
        }
    }

    /// Per-zone soil reasons (percent) are byte-identical in both unit systems
    /// (percent is unit-invariant); the renderer reproduces them verbatim.
    #[test]
    fn imperial_identity_zone_reasons() {
        let p = SkipRuleParams::default();
        // A saturation skip zone + a soil-floor demotion zone.
        let mut i = base();
        i.soil_zones = soil4(Some(80.0), Some(80.0), Some(80.0), Some(90.0));
        for z in crate::engine::skip_rules::decide_per_zone(&i, &p, &[]) {
            assert_eq!(render_zone_reason(&z, IMPERIAL), z.reason);
            assert_eq!(render_zone_reason(&z, METRIC), z.reason);
        }
        let mut j = base();
        j.rain_next_4h_in = 0.50;
        j.soil_zones = soil4(Some(20.0), Some(45.0), Some(45.0), Some(45.0));
        for z in crate::engine::skip_rules::decide_per_zone(&j, &p, &[]) {
            assert_eq!(render_zone_reason(&z, IMPERIAL), z.reason);
        }
    }

    // ── metric assertions: at least one per convertible unit kind ─────────────

    #[test]
    fn metric_temp_freeze() {
        // freeze_now: 30°F now < 38°F min -> -1°C < 3°C.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.temp_now_f = 30.0;
        let s = evaluate_with(&i, &p);
        let out = render_skip_reason(&s, METRIC);
        assert!(out.contains("\u{b0}C"), "metric temp missing °C: {out}");
        assert!(out.contains("-1\u{b0}C"), "30°F should read -1°C: {out}");
        assert!(out.contains("3\u{b0}C"), "38°F should read 3°C: {out}");
        assert!(!out.contains("\u{b0}F"), "metric must not show °F: {out}");
    }

    #[test]
    fn metric_soil_temp_frost() {
        // soil_frost: 33.0°F soil < 35°F -> 0.6°C soil < 2°C.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.soil_temp_yard_min_f = Some(33.0);
        let s = evaluate_with(&i, &p);
        let out = render_skip_reason(&s, METRIC);
        assert!(
            out.contains("0.6\u{b0}C"),
            "33.0°F soil should read 0.6°C: {out}"
        );
    }

    #[test]
    fn metric_wind_now() {
        // wind_now: 20.0 mph > 10 mph -> 32.2 km/h > 16 km/h.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.wind_now_mph = 20.0;
        let s = evaluate_with(&i, &p);
        let out = render_skip_reason(&s, METRIC);
        assert!(out.contains("km/h"), "metric wind missing km/h: {out}");
        assert!(
            out.contains("32.2 km/h"),
            "20 mph should read 32.2 km/h: {out}"
        );
        assert!(out.contains("16 km/h"), "10 mph should read 16 km/h: {out}");
        assert!(!out.contains("mph"), "metric must not show mph: {out}");
    }

    #[test]
    fn metric_rain_depth_already_wet() {
        // already_wet: 0.10" today -> 2.5 mm today.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_today_in = 0.10;
        let s = evaluate_with(&i, &p);
        let out = render_skip_reason(&s, METRIC);
        assert!(out.contains("mm"), "metric depth missing mm: {out}");
        assert!(out.contains("2.5 mm"), "0.10\" should read 2.5 mm: {out}");
        assert!(!out.contains('"'), "metric must not show inch glyph: {out}");
    }

    #[test]
    fn metric_rain_rate_now() {
        // rain_now: 0.05 in/hr -> 1.3 mm/hr.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.rain_intensity_now_in_hr = 0.05;
        let s = evaluate_with(&i, &p);
        let out = render_skip_reason(&s, METRIC);
        assert!(out.contains("mm/hr"), "metric rate missing mm/hr: {out}");
        assert!(
            out.contains("1.3 mm/hr"),
            "0.05 in/hr should read 1.3 mm/hr: {out}"
        );
    }

    #[test]
    fn metric_rule_detail_and_margin() {
        // wind_now detail + margin convert to km/h under metric.
        let p = SkipRuleParams::default();
        let mut i = base();
        i.wind_now_mph = 20.0;
        let trace = decide_traced(&i, &p);
        let wind = trace.rules.iter().find(|r| r.id == "wind_now").unwrap();
        let detail = render_rule_detail(wind, METRIC);
        assert!(
            detail.contains("km/h"),
            "metric detail missing km/h: {detail}"
        );
        let margin = render_rule_margin(wind, METRIC).unwrap();
        assert!(
            margin.contains("km/h"),
            "metric margin missing km/h: {margin}"
        );
        assert!(
            margin.contains("past the line"),
            "fired wind margin: {margin}"
        );
    }
}
