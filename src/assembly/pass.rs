// The engine pass over an assembled snapshot: the skip ladder per zone,
// the weekly water budgets, the soil forecasts, the seven-day verdict
// strip, the custom-rule multiplier. Pure: every instant and calendar is
// a parameter.

use crate::engine::scripting::CompiledScripts;
use crate::engine::skip_rules::ZoneSoil;
use crate::engine::skip_rules::{self as skip_logic, Inputs};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::model::{DayVerdict, IrrigationSnapshot, SoilForecast, WaterBudget};
use crate::refresher::*;
use std::collections::HashMap;

/// Apply each zone's custom condition-rule watering multiplier (from an
/// `AdjustMultiplier` rule action, clamped to [0.5, 1.5] by the engine in
/// `decide_per_zone`) to its dispatched run time, so a "halve the veg garden
/// when humidity is high" style rule actually shrinks (or extends) the run
/// instead of being a silent no-op. `planned_run_seconds` already reflects the
/// seasonal dial, the ET heat multiplier, and the per-zone / regulatory
/// max-duration cap; this layers the user's explicit rule on top and RE-CAPS
/// at the zone's `max_duration_seconds` so a >1.0 multiplier can never push a
/// run past its safety ceiling. A multiplier of exactly 1.0 (every zone with
/// no `AdjustMultiplier` rule) is a no-op, so installs without such a rule are
/// byte-identical. Call this ONCE per refresh, after `planned_run_seconds` is
/// finalized and `apply_engine` has back-filled `z.verdict`.
pub(crate) fn apply_verdict_multiplier(snap: &mut crate::model::IrrigationSnapshot) {
    for z in snap.zones.iter_mut() {
        if z.planned_run_seconds == 0 {
            continue;
        }
        let mult = z.verdict.as_ref().map(|v| v.multiplier).unwrap_or(1.0);
        if (mult - 1.0).abs() <= f64::EPSILON {
            continue;
        }
        let max_dur = z.math.as_ref().map(|m| m.max_duration_seconds).unwrap_or(0);
        let scaled = ((z.planned_run_seconds as f64) * mult).round().max(0.0) as u32;
        z.planned_run_seconds = if max_dur > 0 {
            scaled.min(max_dur)
        } else {
            scaled
        };
        if let Some(m) = z.math.as_mut() {
            m.scheduled_seconds = z.planned_run_seconds;
            // A rule multiplier that ran into the ceiling shortens the run
            // exactly as the allocator's own cap does, so the panel says so.
            // Assignment, not a set-only branch: a multiplier below 1.0 pulls
            // the run back under the ceiling, and then the ceiling is no
            // longer what set the minutes even if an earlier stage had
            // flagged it. Same predicate as `apply_budget_plan`: the run has
            // to sit ON the ceiling for the ceiling to be the reason.
            m.cap_binding = max_dur > 0
                && z.planned_run_seconds == max_dur
                && (scaled > max_dur || m.cap_binding);
        }
    }
}

/// Run the decision engine against `inputs` and write the results into the
/// snapshot: aggregate skip_check + decision_trace, the augment-only Rhai
/// script pass, and per-zone verdicts (back-filled onto each ZoneState).
/// Shared by the HA and native snapshot builders so the watering decision
/// is byte-identical regardless of how the inputs were gathered.
pub(crate) fn apply_engine(
    snap: &mut IrrigationSnapshot,
    inputs: &Inputs,
    scripts: &CompiledScripts,
    condition_rules: &[crate::engine::conditions::ConditionRule],
    // Operator-tuned thresholds from cfg.engine.skip_rules (threaded via
    // WateringPolicy). Previously this constructed SkipRuleParams::default()
    // locally, which silently discarded 8 of the 12 user-tunable knobs
    // (already_wet_in, rain_now_in_hr, rain_next_4h_skip_in,
    // rain_3day_factor, the three heat-advisory gates, and
    // wind_forecast_slack_mph). Defaults are unchanged, so untouched
    // configs decide identically.
    params: &crate::config::schema::SkipRuleParams,
) {
    let decisions = skip_logic::evaluate_decisions(inputs, params, condition_rules, scripts);
    snap.skip_check = decisions.skip_check;
    snap.decision_trace = Some(decisions.trace);
    snap.force_overrode_guard = decisions.force_overrode_guard;
    let verdicts = decisions.zones;
    // Verdict-INDEPENDENT suspect-probe surface (reporting only): a probe the
    // quarantine logic distrusts (offline / wild outlier vs siblings) is flagged
    // here REGARDLESS of which gate ultimately decided the zone, so a bad probe
    // shows on the anomaly banner even when a global gate masked
    // `verdict.source` away from "soil_quarantine". Computed from raw readings,
    // parallel to inputs.soil_zones; changes no decision.
    let suspects = crate::engine::skip_rules::suspect_probes(inputs, params);
    let suspect_by_slug: std::collections::HashMap<&str, &str> = inputs
        .soil_zones
        .iter()
        .zip(suspects.iter())
        .filter_map(|(z, s)| s.as_deref().map(|r| (z.slug.as_str(), r)))
        .collect();
    for z in snap.zones.iter_mut() {
        z.verdict = verdicts.iter().find(|v| v.zone_slug == z.slug).cloned();
        z.soil_suspect = suspect_by_slug.get(z.slug.as_str()).map(|r| r.to_string());
    }
    snap.zone_verdicts = verdicts;
}

/// Weekly water-balance assembly. Resolves each zone's target and
/// runtime inputs (LocalSky config -> agronomic slug default), joins the
/// pre-computed per-tick balance evidence, and calls the ONE pure
/// implementation (`engine::budget::compute_zone`) per zone.
///
/// Outputs `today_seconds` per zone: the run length that actually
/// dispatches. Zero means "don't run this zone today"; the reason names
/// what decided.
pub(crate) fn compute_water_budgets(
    fc: &ForecastSnapshot,
    zone_runtime: &HashMap<String, ZoneRuntime>,
    // Live rain-defer threshold from cfg.engine.session_rain_defer_in.
    session_rain_defer_in: f64,
    restriction_cap_seconds: Option<u32>,
    // Per-zone budget rows. The live call site passes
    // `budget_zones_for_active`, which is one row per ACTIVE zone
    // (config-backed where the operator wrote one, otherwise a row with
    // no explicit target that resolves the agronomic slug default below).
    // Empty = no zones at all -> nothing to plan until the wizard runs.
    budget_zones: &[ZoneBudgetCfg],
    // Pre-computed store evidence; `None` degrades to target-only sizing.
    balance: Option<&BalanceTick>,
    // The deployment's calendar, so the balance's day math is the
    // caller's answer rather than the process's zone.
    calendar: crate::engine::calendar::Calendar,
    // The pass's instant.
    now_epoch: i64,
) -> Vec<WaterBudget> {
    compute_water_budgets_for_horizon(
        fc,
        zone_runtime,
        session_rain_defer_in,
        restriction_cap_seconds,
        budget_zones,
        balance,
        calendar,
        now_epoch,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_water_budgets_for_horizon(
    fc: &ForecastSnapshot,
    zone_runtime: &HashMap<String, ZoneRuntime>,
    session_rain_defer_in: f64,
    restriction_cap_seconds: Option<u32>,
    budget_zones: &[ZoneBudgetCfg],
    balance: Option<&BalanceTick>,
    calendar: crate::engine::calendar::Calendar,
    now_epoch: i64,
    as_of: Option<i64>,
) -> Vec<WaterBudget> {
    let globals = crate::engine::BalanceGlobals {
        now_epoch,
        // The DEPLOYMENT's calendar, resolved here where the configured
        // timezone is known, so the balance's day math never depends on
        // the zone the process happens to run in.
        calendar,
        session_rain_defer_in,
        observed_rain_mm: balance.map(|b| b.observed_rain_mm).unwrap_or(0.0),
        observed_rain_source: balance
            .map(|b| b.observed_rain_source.clone())
            .unwrap_or_else(|| "none".to_string()),
        observed_rain_days_mm: balance
            .map(|b| b.observed_rain_days_mm.clone())
            .unwrap_or_default(),
        bias: balance
            .map(|b| b.bias.clone())
            .unwrap_or_else(crate::engine::BiasModel::identity),
    };

    let mut out = Vec::with_capacity(budget_zones.len());
    for zone_cfg in budget_zones.iter() {
        let slug = zone_cfg.slug.as_str();
        // Resolved at policy-build time from the zone's declared species
        // (see ZoneBudgetCfg::default_budget_in); a config-less zone's row
        // carries the name-based default because it has no species.
        let (default_budget_in, default_sessions) =
            (zone_cfg.default_budget_in, zone_cfg.default_sessions);
        // Precedence: per-zone config value -> agronomic slug default.
        // Home Assistant `input_number` helpers used to win over both. They
        // no longer participate: LocalSky is the engine and reads no entity
        // to make a decision, so the weekly target and session count come
        // from LocalSky's own config on every deployment path.
        let weekly_budget_in = zone_cfg.weekly_budget_in.unwrap_or(default_budget_in);
        let sessions_per_week = zone_cfg
            .sessions_per_week
            .unwrap_or(default_sessions)
            .max(1);
        // Whether this zone waters on a target the operator set or on one
        // inferred from its slug. The allocator decides dispatch on every
        // path now, so the Zones page names the inferred ones once rather
        // than letting a yard start watering on a guess with nothing on
        // screen.
        let target_inferred =
            zone_cfg.weekly_budget_in.is_none() || zone_cfg.sessions_per_week.is_none();
        // Budget mode used to be a per-zone HA toggle while the cutover was
        // in progress. LocalSky is the only source of truth, so it is on.

        // Throughput + max-duration come from LocalSky's zone config
        // (catalog default by sprinkler_type, optional precip_rate_mm_hr
        // override).
        let rt = zone_runtime
            .get(slug)
            .copied()
            .unwrap_or_else(ZoneRuntime::fallback);
        // Active watering restriction cap (if any) tightens the budget-path
        // ceiling too. Same min-of-two rule as the daily-bucket path above.
        let max_dur_s = match restriction_cap_seconds {
            Some(c) => rt.max_duration_s.min(c),
            None => rt.max_duration_s,
        };

        // Per-zone run evidence from the tick (empty = no runs on record).
        let evidence = balance
            .and_then(|b| b.per_zone.get(slug))
            .copied()
            .unwrap_or_default();
        let applied_trailing_mm = if rt.throughput_mm_hr > 0.0 {
            evidence.applied_open_s as f64 / 3600.0 * rt.throughput_mm_hr
        } else {
            0.0
        };

        let zone_inputs = crate::engine::ZoneBalanceInputs {
            slug: slug.to_string(),
            name: zone_cfg.name.clone(),
            weekly_budget_in,
            sessions_per_week,
            throughput_mm_hr: rt.throughput_mm_hr,
            max_dur_s,
            last_run_epoch: evidence.last_run_epoch,
            last_session_applied_mm: evidence
                .last_session_open_s
                .map(|seconds| seconds as f64 / 3600.0 * rt.throughput_mm_hr),
            applied_trailing_mm,
            sessions_done: evidence.sessions_done,
            target_inferred,
            rain_cap_mm: zone_cfg.rain_cap_mm,
            rain_cap_inferred: zone_cfg.rain_cap_inferred,
        };
        out.push(crate::engine::budget::compute_zone_for_horizon(
            &zone_inputs,
            &globals,
            fc,
            as_of,
        ));
    }
    out
}

/// Phase E predictive, per-zone 7-day soil-moisture projection. Uses a
/// FAO-56-flavored water balance: today's calibrated reading is the
/// starting point; each day subtracts the daily ET (scaled by zone Kc)
/// and adds the probability-weighted forecast rain (scaled by a capture
/// efficiency factor to account for runoff). Irrigation is not modeled
///, the curve answers "if I did nothing all week, would each zone stay
/// in its healthy band?"
///
/// Assumptions baked into the heuristic:
///   - Single ET value (today's, from HA's open-meteo eto_today sensor)
///     carries across the full 7-day window. Open-Meteo's daily-ET vector
///     isn't currently in localsky's ForecastSnapshot; the constant
///     approximation is good enough for the dashboard view.
///   - Per-zone soil depth + Kc are hardcoded to match SI's zone
///     multipliers (turf 1.08 / shrubs 0.50) so the predicted depletion
///     matches what SI would have computed in mm.
///   - Rain capture efficiency 0.7, empirical, accounts for runoff,
///     slope, and canopy interception. Knock-down values not modeled.
///   - Probe placement at root depth (operator's responsibility).
/// Effective Kc + root-zone depth (mm) for a zone, inferred from its slug.
/// Turf has shallower active roots than mulched shrubs/beds so equivalent
/// ET drops its moisture % faster. Heuristic so config-driven zones get
/// sensible projection tuning without extra config fields.
/// The projection's crop coefficient and root depth for one zone, from
/// the same catalogs the watering decision reads: the species' FAO-56 Kc
/// curve at today's day of year, hemisphere-shifted by site latitude,
/// and the species root depth unless the zone overrides it.
///
/// This used to guess both from the slug: a flat Kc of 1.08 for anything
/// not named shrub, garden or bed. That number belonged to no species
/// and no season, so a dormant winter lawn (Kc near 0.50) projected
/// drying about twice as fast as the engine itself expected, and a
/// southern-hemisphere yard got a northern calendar. A zone with no
/// agronomy config keeps the neutral 1.0 the rest of the assembly falls
/// back to, with the generic profile's root depth.
pub(crate) fn kc_depth_for(
    slug: &str,
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    today_doy: u16,
    site_lat: f64,
) -> (f64, f64) {
    match agronomy.get(slug) {
        Some(a) => {
            let kc = crate::engine::kc_at_doy_lat(a.species, today_doy, site_lat);
            let depth = a
                .root_depth_mm
                .unwrap_or_else(|| crate::engine::species_profile(a.species).root_depth_mm);
            (kc, depth)
        }
        None => (
            1.0,
            crate::agronomy::species_profile_by_slug("other").root_depth_mm,
        ),
    }
}

/// Agronomic weekly-budget default `(weekly_budget_in, sessions_per_week)`
/// for a zone, inferred from its slug when neither an HA helper nor config
/// sets one (A5b). Mirrors the same shrub/garden/bed heuristic as
/// `kc_depth_for`: mulched beds need less water, less often than turf.
/// The values reproduce the legacy hardcoded compute_water_budgets defaults
/// (turf 1.0"/2 sessions, shrub/garden/bed 0.5"/1) so existing zones are
/// unchanged.
pub(crate) fn agronomic_budget_default(slug: &str) -> (f64, u32) {
    if slug.contains("shrub") || slug.contains("garden") || slug.contains("bed") {
        (0.50, 1)
    } else {
        (1.00, 2)
    }
}

/// One zone's soil-forecast inputs, resolved from config (or the legacy
/// hardcoded 4 when no zone config is present).
pub(crate) struct ForecastZone {
    slug: String,
    name: String,
    target_min: f64,
    target_max: f64,
    kc: f64,
    depth: f64,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_soil_forecasts(
    fc: &ForecastSnapshot,
    _today: &Inputs,
    resolved: &[ZoneSoil],
    zone_cfg: &[ZoneSoilCfg],
    agronomy: &HashMap<String, ZoneAgronomyCfg>,
    today_doy: u16,
    site_lat: f64,
    capture_efficiency: f64,
    et0_today_mm: f64,
) -> Vec<SoilForecast> {
    // Build the working zone list from config. Empty config = unconfigured
    // install -> no soil forecasts until the wizard writes zones. Zones with
    // NO bound soil sensor are excluded outright: there is no probe to
    // project, and emitting an entry made every consumer present a phantom
    // "probe offline" for a zone whose probe was deliberately removed (the
    // Sensors rail listed all four removed probes as offline).
    let zones: Vec<ForecastZone> = zone_cfg
        .iter()
        .filter(|z| z.soil_sensor_id.is_some())
        .map(|z| {
            let (kc, depth) = kc_depth_for(&z.slug, agronomy, today_doy, site_lat);
            ForecastZone {
                slug: z.slug.clone(),
                name: z.name.clone(),
                target_min: z.target_min_pct,
                target_max: z.saturation_pct,
                kc,
                depth,
            }
        })
        .collect();

    // Daily ET, mm. Resolved source-agnostically by the caller (source-reported
    // > Open-Meteo HA sensor > native compute > fallback). Today's value carries
    // across the window. Atmospheric demand is already included in reference ET.
    let daily_et_mm = et0_today_mm;

    let n_days = fc.daily.len().min(7).max(1);
    let mut out = Vec::with_capacity(zones.len());

    for z in zones.iter() {
        // Resolve this zone's live reading via its assigned sensor, with
        // the same offline guard + calibration the decision path uses,
        // then hand the projection to the ENGINE. This loop used to
        // re-implement the whole water balance inline while
        // `engine::soil_forecast::project_zone` sat beside it with unit
        // tests and no caller: two versions of one curve, and the tested
        // one was not the one anybody saw.
        // The reading the decision path resolved for this zone: quality
        // guarded and, for a `source:` channel, stale-checked. A probe the
        // engine will not trust does not drive the projection either.
        let current = resolved
            .iter()
            .find(|r| r.slug == z.slug)
            .and_then(|r| r.pct);
        out.push(crate::engine::project_soil_forecast(
            &crate::engine::ZoneSoilInputs {
                slug: z.slug.clone(),
                name: z.name.clone(),
                kc: z.kc,
                soil_depth_mm: z.depth,
                current_pct: current,
                volumetric: false, // Existing probe calibration establishes a relative scale, not VWC.
                target_min_pct: z.target_min,
                target_max_pct: z.target_max,
            },
            fc,
            daily_et_mm,
            capture_efficiency,
            n_days,
        ));
    }

    out
}

/// Compute the 7-day forward verdict strip. For each daily forecast
/// entry (today + 6 future days), construct synthetic Inputs that
/// answer "would I water on this day?" and run the same evaluate()
/// the morning skip-check uses. Same engine, same rules, the strip
/// is a *preview* of the actual decision, not a separate heuristic.
///
/// Synthetic-input rules:
///   - rain_today = daily[N].precip_sum
///   - forecast_in = daily[N+1].precip_sum (or 0 if past horizon)
///   - rain_3day_weighted = Σ daily[N+1..N+4] × prob/100
///   - temp_min_24h = daily[N].temp_min  (best stand-in we have)
///   - temp_max_3day = max(daily[N..N+3].temp_max)
///   - wind_max_today = daily[N].wind_max
///   - humidity_now: carry today's value (forecast humidity not in OM daily)
///   - days_since_significant_rain: scan the past+now window forward through
///     daily[..N] looking for ≥0.05 days, falling back to past_daily.
///   - rain_intensity_now/wind_now/temp_now: 0 / forecast_wind / temp_min
///     respectively (so the live-only rules don't fire on a forecast day).
pub(crate) fn compute_seven_day_verdicts(
    fc: &ForecastSnapshot,
    today: &Inputs,
    // Operator-tuned thresholds (cfg.engine.skip_rules), same params the
    // live decision uses, so the strip previews the real ladder rather
    // than a defaults-only shadow of it.
    params: &crate::config::schema::SkipRuleParams,
    // Where the yard is and how long its sequence takes, so each cell is
    // judged at the instant that day would actually water.
    site: crate::engine::sunrise::Site,
    // The deployment's calendar, from the policy.
    calendar: crate::engine::calendar::Calendar,
) -> Vec<DayVerdict> {
    crate::engine::compute_verdict_strip(fc, today, params, calendar, site)
}
