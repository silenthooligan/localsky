// Soil-bucket scheduling. Pure evidence derivation and planning for the
// FAO-56 depletion model (`engine::water_balance` holds the bucket
// arithmetic; this module turns trailing evidence into a per-zone plan).
// No IO and no clocks: the assembly gathers store reads and passes them
// in, mirroring the `engine::budget` split ("THE budget implementation"
// pattern), so tests and the live path exercise one formula.
//
// THE ET0 LADDER (per trailing local day, first rung with evidence
// wins):
//   1. LEDGER: the forecast_observations et0_mm day rows (day-MAX,
//      provenance-tagged; the refresher self-emits its resolved daily
//      figure there under 'localsky_engine' every tick).
//   2. ARCHIVE: the forecast provider's own past-day ET0 entries
//      (past_daily, inches converted at the read).
//   3. FALLBACK, per zone: a day with no ET0 evidence charges the
//      zone's weekly-target-derived daily mean (an EXPLICIT weekly
//      target spread over seven days), else the zone's starting target
//      spread the same way, resolved from the species by the same
//      function the weekly plan calls. The fallback is a crop-water figure
//      (ETc), so it bypasses the Kc multiplication the evidence rungs
//      get. The advisory ENGINE_ET0_FALLBACK_MM constant never
//      participates: its contract keeps fabricated evapotranspiration
//      out of decisions, and the replay holds the same line.

// THE CORE LOOP (wired by the refresher's `apply_soil_schedule`, which
// runs it in shadow on every agronomy zone and governs the zones that
// resolve to the soil model; every step here is a pure function of its
// inputs):
//   replay      reconstruct depletion_mm from the trailing evidence
//               window through water_balance::step, one call per local
//               day, anchored at depletion 0 (field capacity) with the
//               [0, TAW] clamp erasing the anchor;
//   trigger     should_irrigate(depletion, RAW), then defer-by-deficit:
//               hold when the capture-adjusted, bias-corrected,
//               probability-weighted next-24h rain would pull the
//               deficit back under RAW;
//   sizing      refill to field capacity via refill_runtime_seconds,
//               capped at the zone's effective max duration; an
//               EXPLICIT weekly target additionally clamps today's
//               delivery to the remaining rolling-7-day headroom
//               (inferred targets never cap);
//   admission   stress-ratio ordering (depletion/RAW descending),
//               greedy fit against the caller's wall-seconds closure,
//               the most-stressed zone always admitted, deferred zones
//               named with the window reason.

use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};

use crate::config::schema::{GrassSpecies, SoilTexture};
use crate::engine::soil_catalog::{raw_mm as raw_for, taw_mm as taw_for};
use crate::engine::species_catalog::{kc_at_doy_lat, lookup as species_lookup};
use crate::engine::water_balance::{
    refill_runtime_seconds, should_irrigate, step as balance_step, ZoneWaterState,
};

/// Trailing local days replayed to reconstruct a zone's depletion.
/// Chosen so the cold-start anchor (depletion 0 at window start) has
/// washed out by the time the window ends: the [0, TAW] clamp erases
/// any anchor offset at the first saturating or fully-depleting day,
/// which arrives within ~2 days on sand and ~5 on clay at Florida
/// summer ETc. Fourteen days sits comfortably past both, and the
/// 90-day ledger retention covers it several times over.
pub const RECON_WINDOW_DAYS: i64 = 14;

/// Minimum days in the replay window that must carry evidence (a
/// resolved ET0 rung, a nonzero rain row, or applied valve seconds)
/// before the replayed figure counts as a reconstruction. One rung over
/// thirteen fallback days would lift the guard while ~93% of the figure
/// is still the fallback mean charged on assumption, so a single
/// self-emitted partial ET0 read must not flip a fresh install from
/// publish-absence to a confident near-full deficit. Three days keep
/// the confident surfaces (bucket, soil block) and the governed swap
/// held back until the window carries more than a lucky read; a live
/// install crosses it within its first few mornings as the engine's own
/// resolved days land in the ledger.
pub const MIN_EVIDENCE_DAYS: u32 = 3;

/// Which ladder rung resolved a day's ET0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Et0DaySource {
    /// forecast_observations et0_mm day row.
    Ledger,
    /// Forecast archive past-day entry.
    Archive,
    /// No evidence: the day charges the zone's fallback daily mean.
    Fallback,
}

/// One trailing day's resolved ET0. `et0_mm` is `Some` only on the
/// evidence rungs; a `Fallback` day carries `None` so no fabricated
/// reference figure can leak out of the resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedEt0Day {
    pub date: NaiveDate,
    pub et0_mm: Option<f64>,
    pub source: Et0DaySource,
}

/// Resolve the ladder's evidence rungs for each requested day: ledger
/// day rows first, archive entries second, `Fallback` marker when
/// neither has a positive value for the date. Only positive values are
/// evidence (the same rule every ET0 read in the codebase applies): a
/// zero or negative entry reads as an absent measurement, not a
/// rain-forest day with no evaporation.
pub fn resolve_et0_days(
    dates: &[NaiveDate],
    ledger: &[(NaiveDate, f64)],
    archive: &[(NaiveDate, f64)],
) -> Vec<ResolvedEt0Day> {
    let find = |rows: &[(NaiveDate, f64)], d: NaiveDate| -> Option<f64> {
        rows.iter()
            .find(|(date, v)| *date == d && *v > 0.0)
            .map(|(_, v)| *v)
    };
    dates
        .iter()
        .map(|&date| {
            if let Some(v) = find(ledger, date) {
                ResolvedEt0Day {
                    date,
                    et0_mm: Some(v),
                    source: Et0DaySource::Ledger,
                }
            } else if let Some(v) = find(archive, date) {
                ResolvedEt0Day {
                    date,
                    et0_mm: Some(v),
                    source: Et0DaySource::Archive,
                }
            } else {
                ResolvedEt0Day {
                    date,
                    et0_mm: None,
                    source: Et0DaySource::Fallback,
                }
            }
        })
        .collect()
}

/// Daily mean crop-water demand (mm/day) charged on a replay day with
/// no ET0 evidence: the ladder's last rung. An EXPLICIT weekly target
/// spreads the operator's own figure over seven days; without one, the
/// zone's starting target does, resolved by the same function the
/// weekly plan uses so the two cannot disagree about what a species
/// wants. The returned figure is ETc (crop water), not reference ET0, so
/// replay days on this rung skip the Kc multiplication.
pub fn fallback_daily_etc_mm(explicit_weekly_target_in: Option<f64>, species: GrassSpecies) -> f64 {
    let weekly_in = match explicit_weekly_target_in {
        Some(t) if t > 0.0 => t,
        // The same starting target the weekly plan resolves for a zone
        // with none set, read from the one function that decides it
        // rather than restated here. This used to hold its own copy of
        // the superseded rule (a flat half inch for shrubs, vegetables
        // and xeriscape alike, an inch for everything else), so a zone
        // with no measured evidence and no target dried at a rate no
        // other part of the engine agreed with, and vegetables dried at
        // half their real demand.
        _ => crate::agronomy::default_weekly_target_in(crate::engine::species_slug(species)).0,
    };
    weekly_in * 25.4 / 7.0
}

// ---- Per-zone parameters and evidence shapes ----

/// Per-zone physical and policy inputs, resolved once by the assembly
/// from `ZoneConfig` + `EngineParams` at policy-build time.
#[derive(Debug, Clone)]
pub struct ZoneSoilParams {
    pub slug: String,
    pub species: GrassSpecies,
    pub texture: SoilTexture,
    /// Root depth override (mm); None = species profile default.
    pub root_depth_mm: Option<f64>,
    /// Management Allowed Depletion override; None = species default.
    pub mad_pct: Option<f64>,
    pub latitude_deg: f64,
    /// EngineParams::capture_efficiency (default 0.70): the wet loss
    /// between sky or hose and root zone. Applied symmetrically: rain
    /// credits at gross x eff inside the bucket step, applied runs
    /// enter the evidence at valve x throughput x eff, and the refill
    /// divides by it on the way out.
    pub capture_efficiency: f64,
    pub throughput_mm_hr: f64,
    /// Effective cap: zone max duration min any active restriction cap.
    pub max_dur_s: u32,
    /// Operator per-day rain cap (mm, gross), honored as min(day, cap)
    /// BEFORE the capture factor so the operator's figure keeps its
    /// 0.7.23 gross semantics. None = no explicit clip; the [0, TAW]
    /// clamp is the emergent physical cap either way.
    pub explicit_rain_cap_mm: Option<f64>,
    /// Weekly target (inches), Some ONLY when the operator set it
    /// explicitly. Promoted to a rolling-7-day delivery ceiling on the
    /// refill; an inferred target never caps (a guessed 1.0 in ceiling
    /// would starve a sandy yard in a dry month). Also the fallback
    /// rung's daily mean when set.
    pub explicit_weekly_budget_in: Option<f64>,
}

impl ZoneSoilParams {
    fn root_depth(&self) -> f64 {
        self.root_depth_mm
            .unwrap_or_else(|| species_lookup(self.species).root_depth_mm)
    }
    fn mad(&self) -> f64 {
        self.mad_pct
            .unwrap_or_else(|| species_lookup(self.species).mad_pct)
    }
    /// Total available water in the root zone (mm).
    pub fn taw_mm(&self) -> f64 {
        taw_for(self.texture, self.root_depth())
    }
    /// Readily available water (mm): the trigger threshold.
    pub fn raw_mm(&self) -> f64 {
        raw_for(self.texture, self.root_depth(), self.mad())
    }
}

/// One trailing local day of gathered evidence for a zone: the ladder's
/// resolved ET0 (None = fallback rung), the day's gross rain, and the
/// union valve-open seconds `history::rollup::applied_per_day`
/// attributed to the day.
#[derive(Debug, Clone, PartialEq)]
pub struct ZoneDayEvidence {
    pub date: NaiveDate,
    pub et0_mm: Option<f64>,
    pub gross_rain_mm: f64,
    pub applied_valve_s: i64,
}

/// One replay-ready day: the charges and credits `replay` folds through
/// the bucket step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReplayDay {
    /// Full-day crop water charge (mm). Evidence days: ET0 x Kc at the
    /// day's DOY, with NO heat multiplier (a measured day's ET0 already
    /// encodes that day's weather; the heat factor stays a
    /// forward-projection input). Fallback days: the zone's daily mean.
    pub etc_mm: f64,
    /// Gross rain (mm); the capture factor applies inside the step.
    pub gross_rain_mm: f64,
    /// Net applied irrigation (mm): valve seconds x throughput x
    /// capture efficiency, per the step's applied contract.
    pub applied_net_mm: f64,
}

/// Turn gathered evidence into replay-ready days. Deterministic: Kc
/// comes from the date's own day-of-year, the explicit rain cap is
/// band-held exactly as the weekly engine holds it (0.05..=5.0 in;
/// non-positive disables clipping), and capture efficiency is clamped
/// to [0, 1] on the applied conversion.
pub fn build_replay_days(evidence: &[ZoneDayEvidence], p: &ZoneSoilParams) -> Vec<ReplayDay> {
    let eff = crate::engine::water_balance::resolve_capture_efficiency(p.capture_efficiency);
    let cap = p
        .explicit_rain_cap_mm
        .filter(|c| *c > 0.0)
        .map(|c| c.clamp(0.05 * 25.4, 5.0 * 25.4));
    evidence
        .iter()
        .map(|day| {
            let etc_mm = match day.et0_mm {
                Some(et0) if et0 > 0.0 => {
                    let doy = day.date.ordinal() as u16;
                    et0 * kc_at_doy_lat(p.species, doy, p.latitude_deg)
                }
                _ => fallback_daily_etc_mm(p.explicit_weekly_budget_in, p.species),
            };
            let gross_rain_mm = match cap {
                Some(c) => day.gross_rain_mm.min(c),
                None => day.gross_rain_mm,
            };
            let applied_net_mm =
                (day.applied_valve_s.max(0) as f64 / 3600.0) * p.throughput_mm_hr * eff;
            ReplayDay {
                etc_mm,
                gross_rain_mm,
                applied_net_mm,
            }
        })
        .collect()
}

/// Reconstruct depletion (mm) by folding the trailing days through the
/// bucket step, oldest first. COLD-START ANCHOR: depletion 0 (field
/// capacity) at window start. The anchor's error is bounded by TAW and
/// decays to zero at the first clamping event: a day where credits meet
/// or beat depletion plus ETc pins the bucket at 0 regardless of the
/// start, and a dry unirrigated stretch pins it at TAW, so a
/// `RECON_WINDOW_DAYS` window ends well past both on any texture. The
/// result is clamped to [0, TAW] by construction (every step clamps).
pub fn replay(days: &[ReplayDay], capture_efficiency: f64, taw_mm: f64) -> f64 {
    let mut state = ZoneWaterState::default();
    for d in days {
        balance_step(
            &mut state,
            d.etc_mm,
            d.gross_rain_mm,
            d.applied_net_mm,
            capture_efficiency,
            taw_mm,
        );
    }
    state.depletion_mm
}

// ---- Trigger ----

/// Defer-by-deficit: the per-zone replacement for the fixed defer
/// depth. A DUE zone holds when the expected post-rain depletion
/// max(depletion - eff x rain, 0) falls back under RAW, where `rain`
/// is the bias-corrected, probability-weighted next-24h forecast depth
/// (mm) the assembly resolves. The threshold is zone physics: sand's
/// small RAW tolerates little forecast rain, clay's large RAW a lot.
/// Returns the hold reason, or None when the zone is not due or the
/// rain leaves it due anyway.
pub fn defer_by_deficit(
    depletion_mm: f64,
    raw_mm: f64,
    capture_efficiency: f64,
    expected_next_24h_rain_mm: f64,
) -> Option<String> {
    if !should_irrigate(depletion_mm, raw_mm) {
        return None;
    }
    let effective = crate::engine::water_balance::resolve_capture_efficiency(capture_efficiency)
        * expected_next_24h_rain_mm.max(0.0);
    let post = (depletion_mm - effective).max(0.0);
    if post < raw_mm {
        Some(format!(
            "deferred: forecast rain refills the deficit ({effective:.1} of {depletion_mm:.1} \
             mm expected)"
        ))
    } else {
        None
    }
}

// ---- Sizing ----

/// A sized refill: seconds to dispatch plus which clamp, if any, set
/// them.
#[derive(Debug, Clone, PartialEq)]
pub struct SizedRefill {
    pub planned_seconds: u32,
    /// The max-duration cap shorted the full refill. The residual
    /// depletion survives the next replay, so the zone re-triggers on
    /// consecutive mornings until it drops under RAW: the carry is the
    /// bucket itself, no ledger.
    pub session_capped: bool,
    /// The explicit weekly delivery ceiling shorted today's run.
    pub ceiling_binding: bool,
    /// Set exactly when `ceiling_binding`: names delivered-of-target.
    pub ceiling_reason: Option<String>,
}

/// Size a due zone's run: refill the full depletion back to field
/// capacity (gross = depletion / capture efficiency), capped at the
/// zone's effective max duration. When the operator EXPLICITLY set a
/// weekly target, today's delivery is additionally held to the
/// remaining rolling-7-day headroom (target minus `delivered
/// _trailing_7d_mm`, the gross trailing applied depth): the operator's
/// figure stays a real ceiling while the model modernizes, loudly,
/// with partial delivery rather than a parked zone. Inferred targets
/// never cap.
pub fn size_refill(
    depletion_mm: f64,
    p: &ZoneSoilParams,
    delivered_trailing_7d_mm: f64,
) -> SizedRefill {
    let ideal_s = refill_runtime_seconds(
        depletion_mm,
        p.throughput_mm_hr,
        p.capture_efficiency,
        u32::MAX,
    );
    let capped_s = ideal_s.min(p.max_dur_s);
    let session_capped = ideal_s > p.max_dur_s;
    let (planned_seconds, ceiling_binding, ceiling_reason) = match p.explicit_weekly_budget_in {
        Some(target_in) if target_in > 0.0 && capped_s > 0 && p.throughput_mm_hr > 0.0 => {
            let target_mm = target_in * 25.4;
            let headroom_mm = (target_mm - delivered_trailing_7d_mm.max(0.0)).max(0.0);
            let headroom_s = ((headroom_mm / p.throughput_mm_hr) * 3600.0).round() as i64;
            let headroom_s = headroom_s.clamp(0, u32::MAX as i64) as u32;
            if headroom_s < capped_s {
                let reason = format!(
                    "held to the weekly ceiling: {:.2} in of the {:.2} in target delivered \
                     in the last 7 days, {:.2} in of headroom left",
                    delivered_trailing_7d_mm.max(0.0) / 25.4,
                    target_in,
                    headroom_mm / 25.4
                );
                (headroom_s, true, Some(reason))
            } else {
                (capped_s, false, None)
            }
        }
        _ => (capped_s, false, None),
    };
    SizedRefill {
        planned_seconds,
        session_capped,
        ceiling_binding,
        ceiling_reason,
    }
}

// ---- Per-zone plan ----

/// One zone's soil plan for the tick: what the assembly copies onto the
/// wire's `WaterBudget` soil block. Additive-ready serde (every field
/// defaulted).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SoilZonePlan {
    #[serde(default)]
    pub zone_slug: String,
    /// Reconstructed depletion below field capacity (mm), in [0, TAW].
    #[serde(default)]
    pub depletion_mm: f64,
    #[serde(default)]
    pub taw_mm: f64,
    #[serde(default)]
    pub raw_mm: f64,
    /// Depletion crossed RAW this tick.
    #[serde(default)]
    pub due: bool,
    /// The hold that zeroed a due zone: defer-by-deficit here, or the
    /// window-admission reason written by the admission pass.
    #[serde(default)]
    pub deferred_reason: Option<String>,
    /// WHICH hold it was, as data. The sentence above is written for
    /// logs and external consumers; surfaces that need to know the kind
    /// (to compose their own copy in the viewer's units) read this
    /// instead of matching the sentence's opening words.
    #[serde(default)]
    pub deferred_kind: Option<SoilDeferKind>,
    #[serde(default)]
    pub planned_seconds: u32,
    /// The max-duration cap shorted the refill (Check A's deficit arm
    /// reads this later).
    #[serde(default)]
    pub session_capped: bool,
    /// The explicit weekly delivery ceiling shorted today's run.
    #[serde(default)]
    pub ceiling_binding: bool,
    #[serde(default)]
    pub ceiling_reason: Option<String>,
    /// Trailing days that carried ANY evidence: a resolved ET0 rung, a
    /// nonzero rain row, or nonzero applied valve seconds. The count is
    /// the plan's confidence signal; see `evidence_starved`.
    #[serde(default)]
    pub evidence_days: u32,
    /// Trailing days with no evidence at all: they charged the fallback
    /// ETc mean with zero credits.
    #[serde(default)]
    pub fallback_days: u32,
}

impl SoilZonePlan {
    /// The window carries too little evidence to trust (an empty window
    /// included): under `MIN_EVIDENCE_DAYS` evidenced days, nearly every
    /// replayed day charged the fallback mean by assumption alone, so
    /// the depletion figure is fabricated certainty, not a
    /// reconstruction. The assembly publishes ABSENCE for a starved plan
    /// (the 0.7.22 absent-not-zero contract) and lets the weekly
    /// allocator size a governed zone until enough rungs resolve.
    pub fn evidence_starved(&self) -> bool {
        self.evidence_days < MIN_EVIDENCE_DAYS
    }
}

/// Compute one zone's plan from its evidence window: replay, trigger,
/// defer-by-deficit, sizing. Window admission runs afterwards across
/// zones (`admit_zones`); a zone it defers gets `deferred_reason` set
/// and `planned_seconds` zeroed by the assembly.
pub fn plan_zone(
    p: &ZoneSoilParams,
    evidence: &[ZoneDayEvidence],
    expected_next_24h_rain_mm: f64,
    delivered_trailing_7d_mm: f64,
) -> SoilZonePlan {
    let taw = p.taw_mm();
    let raw = p.raw_mm();
    // Evidence census: a day counts as evidenced when any rung resolved
    // its ET0, its rain row is nonzero, or applied seconds landed on it.
    // An all-fallback window replays to a figure made purely of
    // assumption (TAW after a few days), which the assembly must publish
    // as absence, never as a confident full deficit.
    let evidence_days = evidence
        .iter()
        .filter(|d| d.et0_mm.is_some() || d.gross_rain_mm > 0.0 || d.applied_valve_s > 0)
        .count() as u32;
    let fallback_days = evidence.len() as u32 - evidence_days;
    let depletion = replay(&build_replay_days(evidence, p), p.capture_efficiency, taw);
    let due = should_irrigate(depletion, raw);
    let mut plan = SoilZonePlan {
        zone_slug: p.slug.clone(),
        depletion_mm: depletion,
        taw_mm: taw,
        raw_mm: raw,
        due,
        evidence_days,
        fallback_days,
        ..Default::default()
    };
    if !due {
        return plan;
    }
    if let Some(reason) = defer_by_deficit(
        depletion,
        raw,
        p.capture_efficiency,
        expected_next_24h_rain_mm,
    ) {
        plan.deferred_reason = Some(reason);
        plan.deferred_kind = Some(SoilDeferKind::ForecastRain);
        return plan;
    }
    let sized = size_refill(depletion, p, delivered_trailing_7d_mm);
    plan.planned_seconds = sized.planned_seconds;
    plan.session_capped = sized.session_capped;
    plan.ceiling_binding = sized.ceiling_binding;
    plan.ceiling_reason = sized.ceiling_reason;
    plan
}

/// A governed zone's today figures from its plan: the seconds the row
/// dispatches, the reason the card renders, and the session_capped flag.
/// One formula for the refresher's governed swap and the demo's
/// synthesized soil zone, so their reason strings cannot drift.
/// `cap_minutes` names the zone's configured run limit in the
/// shorted-by-cap suffix. The window-admission pass may still zero the
/// returned seconds afterwards with its own reason.
pub fn today_row(plan: &SoilZonePlan, cap_minutes: u32) -> (u32, String, bool) {
    if !plan.due {
        return (
            0,
            format!(
                "soil bucket holds: {:.1} of {:.1} mm depleted; waters when depletion \
                 crosses {:.1} mm",
                plan.depletion_mm, plan.taw_mm, plan.raw_mm
            ),
            plan.session_capped,
        );
    }
    if let Some(reason) = plan.deferred_reason.clone() {
        // Defer-by-deficit: the soil model's own rain gate, replacing
        // the fixed session_rain_defer depth for this zone.
        return (0, reason, plan.session_capped);
    }
    if plan.ceiling_binding {
        // The EXPLICIT weekly target as a delivery ceiling: partial
        // delivery (or a parked day at zero headroom) with the loud
        // reason; depletion carries, the zone is never parked.
        return (
            plan.planned_seconds,
            plan.ceiling_reason
                .clone()
                .unwrap_or_else(|| "held to the weekly ceiling".to_string()),
            plan.session_capped,
        );
    }
    let mut reason = format!(
        "soil refill: {:.1} mm deficit over {:.0} min (bucket {:.0}% depleted)",
        plan.depletion_mm,
        (plan.planned_seconds as f64 / 60.0).round(),
        (plan.depletion_mm / plan.taw_mm.max(f64::EPSILON) * 100.0).round()
    );
    if plan.session_capped {
        use std::fmt::Write;
        let _ = write!(
            reason,
            "; shorted by the {cap_minutes}-min cap, the rest carries to tomorrow"
        );
    }
    (plan.planned_seconds, reason, plan.session_capped)
}

// ---- Window admission ----

/// One due zone as the admission pass sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmissionCandidate {
    pub slug: String,
    pub depletion_mm: f64,
    pub raw_mm: f64,
    pub planned_seconds: u32,
}

/// A zone the window could not fit today.
#[derive(Debug, Clone, PartialEq)]
pub struct DeferredZone {
    pub slug: String,
    pub reason: String,
}

/// The admission pass result. `admitted` and `deferred` are both in
/// stress order (most depleted first).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AdmissionOutcome {
    pub admitted: Vec<String>,
    pub deferred: Vec<DeferredZone>,
}

/// Why a due zone was held. Two holds can zero a zone that wants water,
/// and a surface that needs to tell them apart used to do it by testing
/// how the engine's sentence began, which made a copy edit a silent
/// behavior change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SoilDeferKind {
    /// Forecast rain is expected to refill the deficit on its own.
    ForecastRain,
    /// The morning window could not fit this zone today.
    Window,
}

/// Fit the due zones into the morning window. Ordering is the STRESS
/// RATIO depletion/RAW, descending (absolute mm would systematically
/// favor clay's large bucket over visibly wilting sand); ties keep
/// input order. Greedy first-fit: each candidate joins the tentative
/// set, `wall_seconds` prices the set (the caller closes over its live
/// `sequence_wall_seconds` inputs: agronomy map, soak minutes,
/// interleave policy), and the candidate stays when the wall fits
/// `available_s`. The single most-stressed zone is ALWAYS admitted,
/// even alone over the window, so admission can never produce an empty
/// morning while something needs water. A zone that does not fit today
/// carries no state: tomorrow its ratio has grown by ETc/RAW and it
/// sorts earlier.
pub fn admit_zones<F>(
    due: &[AdmissionCandidate],
    available_s: u64,
    wall_seconds: F,
) -> AdmissionOutcome
where
    F: Fn(&[AdmissionCandidate]) -> u64,
{
    let stress = |c: &AdmissionCandidate| -> f64 {
        if c.raw_mm > 0.0 {
            c.depletion_mm / c.raw_mm
        } else {
            f64::INFINITY
        }
    };
    let mut ordered: Vec<&AdmissionCandidate> = due.iter().collect();
    // Stable sort: ties keep input (snapshot) order.
    ordered.sort_by(|a, b| {
        stress(b)
            .partial_cmp(&stress(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut admitted_set: Vec<AdmissionCandidate> = Vec::new();
    let mut deferred_slugs: Vec<String> = Vec::new();
    for cand in ordered {
        let mut tentative = admitted_set.clone();
        tentative.push(cand.clone());
        if admitted_set.is_empty() || wall_seconds(&tentative) <= available_s {
            admitted_set = tentative;
        } else {
            deferred_slugs.push(cand.slug.clone());
        }
    }
    let reason = format!(
        "waits for tomorrow: the morning window fits {} of {} zones that need water, most \
         depleted first",
        admitted_set.len(),
        due.len()
    );
    AdmissionOutcome {
        admitted: admitted_set.into_iter().map(|c| c.slug).collect(),
        deferred: deferred_slugs
            .into_iter()
            .map(|slug| DeferredZone {
                slug,
                reason: reason.clone(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 7, day).unwrap()
    }

    /// Rung precedence per day: a ledger row beats the archive for the
    /// same date, the archive fills ledger gaps, and a day neither
    /// covers is marked for the fallback rung with no fabricated value.
    #[test]
    fn ladder_resolves_ledger_then_archive_then_fallback() {
        let dates = [d(1), d(2), d(3)];
        let ledger = [(d(1), 5.2)];
        let archive = [(d(1), 4.0), (d(2), 4.4)];
        let out = resolve_et0_days(&dates, &ledger, &archive);
        assert_eq!(
            out,
            vec![
                ResolvedEt0Day {
                    date: d(1),
                    et0_mm: Some(5.2),
                    source: Et0DaySource::Ledger,
                },
                ResolvedEt0Day {
                    date: d(2),
                    et0_mm: Some(4.4),
                    source: Et0DaySource::Archive,
                },
                ResolvedEt0Day {
                    date: d(3),
                    et0_mm: None,
                    source: Et0DaySource::Fallback,
                },
            ]
        );
    }

    /// Zero and negative entries are absent measurements, not evidence:
    /// a zeroed ledger row falls through to the archive, and a zeroed
    /// archive entry falls through to the fallback rung.
    #[test]
    fn non_positive_entries_are_not_evidence() {
        let dates = [d(1), d(2)];
        let ledger = [(d(1), 0.0)];
        let archive = [(d(1), 4.1), (d(2), -1.0)];
        let out = resolve_et0_days(&dates, &ledger, &archive);
        assert_eq!(out[0].et0_mm, Some(4.1));
        assert_eq!(out[0].source, Et0DaySource::Archive);
        assert_eq!(out[1].et0_mm, None);
        assert_eq!(out[1].source, Et0DaySource::Fallback);
    }

    /// St Augustine on the given texture at species defaults (150 mm
    /// roots, MAD 0.50), Florida latitude, capture 0.70, 15 mm/hr.
    fn zone(texture: SoilTexture) -> ZoneSoilParams {
        ZoneSoilParams {
            slug: "front".into(),
            species: GrassSpecies::StAugustine,
            texture,
            root_depth_mm: None,
            mad_pct: None,
            latitude_deg: 28.5,
            capture_efficiency: 0.70,
            throughput_mm_hr: 15.0,
            max_dur_s: 3600,
            explicit_rain_cap_mm: None,
            explicit_weekly_budget_in: None,
        }
    }

    fn dry_days(etc: f64, n: usize) -> Vec<ReplayDay> {
        vec![
            ReplayDay {
                etc_mm: etc,
                gross_rain_mm: 0.0,
                applied_net_mm: 0.0,
            };
            n
        ]
    }

    /// The texture triad drives cadence with zero user math: at a fixed
    /// 5 mm/day ETc, sand (RAW 4.5) triggers after one dry day, sandy
    /// loam (RAW 9.75) after two, loam (RAW 11.25) after three, and a
    /// long dry stretch clamps each bucket at its own TAW. The buckets
    /// come from the FAO-56 Table 19 profiles at this zone's 150 mm
    /// roots, so a catalog edit that moved a texture would land here.
    #[test]
    fn triad_cadence_is_emergent_from_texture() {
        let cases = [
            (SoilTexture::Sand, 9.0, 4.5, 1usize),
            (SoilTexture::SandyLoam, 19.5, 9.75, 2),
            (SoilTexture::Loam, 22.5, 11.25, 3),
        ];
        for (texture, taw, raw, due_after_days) in cases {
            let p = zone(texture);
            assert!((p.taw_mm() - taw).abs() < 1e-9, "{texture:?} TAW");
            assert!((p.raw_mm() - raw).abs() < 1e-9, "{texture:?} RAW");
            let before = replay(&dry_days(5.0, due_after_days - 1), 0.70, taw);
            assert!(
                !should_irrigate(before, raw),
                "{texture:?} not yet due at {} days ({before} mm)",
                due_after_days - 1
            );
            let at = replay(&dry_days(5.0, due_after_days), 0.70, taw);
            assert!(
                should_irrigate(at, raw),
                "{texture:?} due at {due_after_days} days ({at} mm)"
            );
            let parched = replay(&dry_days(5.0, 14), 0.70, taw);
            assert!(
                (parched - taw).abs() < 1e-9,
                "{texture:?} clamps at TAW, got {parched}"
            );
        }
    }

    /// THE ISSUE #9 SAND YARD, module level: a 1.2 in storm day fills
    /// the 9 mm bucket and the excess drains through the clamp (the
    /// emergent per-day cap); the zone holds right after the storm and
    /// resumes the next day when daily ETc pushes depletion back over
    /// RAW, sized to the actual deficit instead of a weekly quota.
    #[test]
    fn sand_yard_storm_holds_then_resumes_on_the_deficit() {
        let p = zone(SoilTexture::Sand);
        // July days with ET0 evidence at 5.0 mm; Kc for St Augustine in
        // July is exactly 1.00, so each day charges 5.0 mm of ETc.
        let mut evidence: Vec<ZoneDayEvidence> = (1..=4)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        evidence[2].gross_rain_mm = 1.2 * 25.4; // the storm, July 3
                                                // Through the storm day: 5, clamp 9, then 9 + 5 - 0.7 x 30.48
                                                // clamps at 0. The yard holds.
        let through_storm = plan_zone(&p, &evidence[..3], 0.0, 0.0);
        assert_eq!(
            through_storm.depletion_mm, 0.0,
            "the storm fills the bucket"
        );
        assert!(!through_storm.due);
        assert_eq!(through_storm.planned_seconds, 0);
        // One more 5 mm day: depletion 5.0 crosses RAW 4.5 and the zone
        // resumes with a refill sized to the deficit: 5 / 0.7 / 15 mm/hr
        // is about 1714 s, nowhere near a weekly-quota session.
        let next_day = plan_zone(&p, &evidence, 0.0, 0.0);
        assert!((next_day.depletion_mm - 5.0).abs() < 1e-9);
        assert!(next_day.due, "depletion crossed RAW");
        assert!(
            next_day.planned_seconds > 1700 && next_day.planned_seconds < 1800,
            "refill sized to the deficit, got {}",
            next_day.planned_seconds
        );
        assert!(!next_day.session_capped);
        assert!(!next_day.ceiling_binding);
        assert_eq!(next_day.deferred_reason, None);
        // Whole-window sanity: depletion can never leave [0, TAW].
        let two_weeks: Vec<ZoneDayEvidence> = (1..=14)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: if day == 3 { 1.2 * 25.4 } else { 0.0 },
                applied_valve_s: 0,
            })
            .collect();
        let long = plan_zone(&p, &two_weeks, 0.0, 0.0);
        assert!(long.depletion_mm <= p.taw_mm() + 1e-9);
    }

    /// An applied run feeds back as evidence: the morning's own water
    /// lands in the next replay and the zone reads not-due, the same
    /// self-quenching loop the weekly model has.
    #[test]
    fn applied_evidence_quenches_the_trigger() {
        let p = zone(SoilTexture::Sand);
        let mut evidence: Vec<ZoneDayEvidence> = (1..=2)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let before = plan_zone(&p, &evidence, 0.0, 0.0);
        assert!(before.due, "two dry days on sand read due");
        // Yesterday's run: an hour on the valve at 15 mm/hr x 0.7 puts
        // back 10.5 mm net, covering the day's charge and the standing
        // deficit both.
        evidence[1].applied_valve_s = 3600;
        let after = plan_zone(&p, &evidence, 0.0, 0.0);
        assert!(
            !after.due,
            "the applied evidence quenches the trigger, depletion {}",
            after.depletion_mm
        );
    }

    /// Days with no ET0 evidence charge the fallback daily mean: the
    /// explicit weekly target spread over seven days, else the species
    /// class figure. No 5.0 mm constant anywhere.
    #[test]
    fn missing_et0_days_charge_the_fallback_rung() {
        let mut p = zone(SoilTexture::Loam);
        p.explicit_weekly_budget_in = Some(1.4);
        let evidence: Vec<ZoneDayEvidence> = (1..=2)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: None,
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        let per_day = 1.4 * 25.4 / 7.0;
        assert!(
            (plan.depletion_mm - 2.0 * per_day).abs() < 1e-9,
            "two fallback days at {per_day} mm, got {}",
            plan.depletion_mm
        );
        // Without an explicit target the species class supplies it.
        p.explicit_weekly_budget_in = None;
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        assert!((plan.depletion_mm - 2.0 * 25.4 / 7.0).abs() < 1e-9);
    }

    /// An EMPTY evidence vector replays to depletion 0 (the anchor) and
    /// plans nothing, and it reads as starved: zero evidence days. The
    /// live assembly never produces this shape while a persistence DB
    /// exists (the window always carries dated days); the real cold
    /// start is the all-fallback window below, which the assembly
    /// publishes as ABSENCE rather than this anchor figure.
    #[test]
    fn cold_start_plans_nothing() {
        let p = zone(SoilTexture::Sand);
        let plan = plan_zone(&p, &[], 0.0, 0.0);
        assert_eq!(
            plan,
            SoilZonePlan {
                zone_slug: "front".into(),
                depletion_mm: 0.0,
                taw_mm: p.taw_mm(),
                raw_mm: p.raw_mm(),
                due: false,
                deferred_reason: None,
                deferred_kind: None,
                planned_seconds: 0,
                session_capped: false,
                ceiling_binding: false,
                ceiling_reason: None,
                evidence_days: 0,
                fallback_days: 0,
            }
        );
        assert!(plan.evidence_starved());
    }

    /// The evidence census: an all-fallback window (14 dated days, no
    /// ET0 rung, no rain, no applied) replays to TAW purely by
    /// assumption and is flagged STARVED, the signal the assembly reads
    /// to publish absence and stand the governed swap down. One rung
    /// over thirteen fallback days does not lift the starvation;
    /// `MIN_EVIDENCE_DAYS` evidenced days anywhere in the window do.
    #[test]
    fn all_fallback_window_reads_starved() {
        let p = zone(SoilTexture::Sand);
        let mut evidence: Vec<ZoneDayEvidence> = (1..=14)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: None,
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        assert_eq!(plan.evidence_days, 0);
        assert_eq!(plan.fallback_days, 14);
        assert!(plan.evidence_starved());
        // The internal replay does pin TAW (assumption, not evidence),
        // which is exactly why the flag exists: publishing this figure
        // would fabricate a full deficit on day one.
        assert!((plan.depletion_mm - p.taw_mm()).abs() < 1e-9);
        assert!(plan.due);
        // One rung resolving is not enough: thirteen of the fourteen
        // days still charge the fallback mean, so the guard holds.
        evidence[13].et0_mm = Some(0.2);
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        assert_eq!(plan.evidence_days, 1);
        assert_eq!(plan.fallback_days, 13);
        assert!(plan.evidence_starved());
        // Two days still starve; the third lifts the guard.
        evidence[12].et0_mm = Some(0.2);
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        assert_eq!(plan.evidence_days, 2);
        assert!(plan.evidence_starved());
        evidence[11].et0_mm = Some(0.2);
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        assert_eq!(plan.evidence_days, MIN_EVIDENCE_DAYS);
        assert_eq!(plan.fallback_days, 11);
        assert!(!plan.evidence_starved());
        // A nonzero rain row or applied seconds is evidence too.
        evidence[13].et0_mm = None;
        evidence[0].gross_rain_mm = 2.0;
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        assert_eq!(plan.evidence_days, MIN_EVIDENCE_DAYS);
        assert!(!plan.evidence_starved());
        evidence[0].gross_rain_mm = 0.0;
        evidence[5].applied_valve_s = 600;
        let plan = plan_zone(&p, &evidence, 0.0, 0.0);
        assert_eq!(plan.evidence_days, MIN_EVIDENCE_DAYS);
        assert!(!plan.evidence_starved());
    }

    /// The explicit per-day rain cap keeps its 0.7.23 gross semantics
    /// inside the bucket: min(day, cap) BEFORE the capture factor, band
    /// held, non-positive disables.
    #[test]
    fn explicit_rain_cap_clips_gross_before_capture() {
        let mut p = zone(SoilTexture::Loam);
        p.explicit_rain_cap_mm = Some(9.0);
        let evidence = [ZoneDayEvidence {
            date: d(1),
            et0_mm: Some(5.0),
            gross_rain_mm: 30.0,
            applied_valve_s: 0,
        }];
        let days = build_replay_days(&evidence, &p);
        assert!((days[0].gross_rain_mm - 9.0).abs() < 1e-9, "clipped gross");
        // Non-positive disables clipping (a caller predating the field).
        p.explicit_rain_cap_mm = Some(0.0);
        let days = build_replay_days(&evidence, &p);
        assert!((days[0].gross_rain_mm - 30.0).abs() < 1e-9);
        // Band clamp: 0.01 in on disk is held up to 0.05 in.
        p.explicit_rain_cap_mm = Some(0.01 * 25.4);
        let days = build_replay_days(&evidence, &p);
        assert!((days[0].gross_rain_mm - 0.05 * 25.4).abs() < 1e-9);
    }

    /// Capture-efficiency bounds hold everywhere: an out-of-band value
    /// cannot inflate the applied credit, zero capture credits no rain,
    /// and sizing survives a near-zero value through the refill
    /// function's own floor.
    #[test]
    fn capture_efficiency_bounds() {
        // Applied conversion clamps eff to [0, 1]: 3600 valve seconds
        // at 10 mm/hr credits at most 10 mm net.
        let mut p = zone(SoilTexture::Loam);
        p.throughput_mm_hr = 10.0;
        p.capture_efficiency = 1.5;
        let evidence = [ZoneDayEvidence {
            date: d(1),
            et0_mm: Some(5.0),
            gross_rain_mm: 0.0,
            applied_valve_s: 3600,
        }];
        let days = build_replay_days(&evidence, &p);
        assert!(
            (days[0].applied_net_mm - 10.0).abs() < 1e-9,
            "eff held to 1"
        );
        // A pathological capture efficiency resolves to the SAME floor on
        // both sides of the balance. It used to credit rain at zero while
        // the refill divided by the 0.05 floor, so the model believed rain
        // delivered nothing to the root zone while irrigation delivered
        // five percent of itself, and sized a run twenty times the
        // deficit off the back of it.
        let rain_day = [
            ReplayDay {
                etc_mm: 5.0,
                gross_rain_mm: 0.0,
                applied_net_mm: 0.0,
            },
            ReplayDay {
                etc_mm: 0.0,
                gross_rain_mm: 10.0,
                applied_net_mm: 0.0,
            },
        ];
        let floor = crate::engine::water_balance::MIN_CAPTURE_EFFICIENCY;
        let dep = replay(&rain_day, 0.0, 30.0);
        assert!(
            (dep - (5.0 - 10.0 * floor)).abs() < 1e-9,
            "rain credits at the same floor the refill divides by, got {dep}"
        );
        // Sizing at a pathological eff: refill_runtime_seconds floors
        // the divisor at 0.05, so the figure stays finite and the max
        // duration cap contains it.
        let mut p2 = zone(SoilTexture::Sand);
        p2.capture_efficiency = 0.0001;
        let sized = size_refill(5.0, &p2, 0.0);
        assert_eq!(sized.planned_seconds, p2.max_dur_s);
        assert!(sized.session_capped);
    }

    /// Defer-by-deficit: a due zone holds when capture-adjusted
    /// forecast rain would pull the deficit back under RAW, with the
    /// reason naming expected refill against the deficit; heavier
    /// depletion rides through the same rain, and a not-due zone never
    /// defers.
    #[test]
    fn defer_by_deficit_holds_exactly_when_rain_covers() {
        // Due at 10.0 mm against RAW 9.0; 2 mm forecast x 0.7 = 1.4 mm
        // expected refill; post-rain depletion 8.6 falls under RAW.
        let held = defer_by_deficit(10.0, 9.0, 0.70, 2.0);
        assert_eq!(
            held.as_deref(),
            Some("deferred: forecast rain refills the deficit (1.4 of 10.0 mm expected)")
        );
        // A deeper deficit rides through the same rain.
        assert_eq!(defer_by_deficit(20.0, 9.0, 0.70, 2.0), None);
        // Not due: nothing to defer.
        assert_eq!(defer_by_deficit(8.0, 9.0, 0.70, 50.0), None);
        // The plan-level composition: a due sand zone with heavy
        // forecast rain holds with the reason and zero seconds.
        let p = zone(SoilTexture::Sand);
        let evidence: Vec<ZoneDayEvidence> = (1..=2)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let plan = plan_zone(&p, &evidence, 20.0, 0.0);
        assert!(plan.due);
        assert_eq!(plan.planned_seconds, 0);
        assert!(
            plan.deferred_reason
                .as_deref()
                .is_some_and(|r| r.starts_with("deferred: forecast rain refills")),
            "{:?}",
            plan.deferred_reason
        );
    }

    /// The weekly ceiling binds only for an EXPLICIT target: partial
    /// clamp to the remaining headroom with the loud reason, zero
    /// headroom parks today's run (never the zone: depletion carries),
    /// and an inferred target never caps.
    #[test]
    fn weekly_ceiling_explicit_only_partial_and_zero_headroom() {
        let mut p = zone(SoilTexture::Loam);
        p.throughput_mm_hr = 10.0;
        // Ideal refill for 6 mm at 0.7 capture: 6 / 0.7 / 10 x 3600
        // rounds to 3086 s.
        p.explicit_weekly_budget_in = Some(1.0);
        let delivered = 0.8 * 25.4;
        let sized = size_refill(6.0, &p, delivered);
        // Headroom 0.2 in = 5.08 mm caps delivery at 1829 s.
        assert_eq!(sized.planned_seconds, 1829);
        assert!(sized.ceiling_binding);
        assert!(!sized.session_capped);
        assert_eq!(
            sized.ceiling_reason.as_deref(),
            Some(
                "held to the weekly ceiling: 0.80 in of the 1.00 in target delivered in \
                 the last 7 days, 0.20 in of headroom left"
            )
        );
        // Zero headroom: today delivers nothing, loudly.
        let sized = size_refill(6.0, &p, 25.4);
        assert_eq!(sized.planned_seconds, 0);
        assert!(sized.ceiling_binding);
        assert!(sized.ceiling_reason.is_some());
        // Inferred target: no ceiling at all.
        p.explicit_weekly_budget_in = None;
        let sized = size_refill(6.0, &p, 25.4 * 4.0);
        assert_eq!(sized.planned_seconds, 3086);
        assert!(!sized.ceiling_binding);
        assert_eq!(sized.ceiling_reason, None);
        // Ample headroom under an explicit target: no clamp reported.
        p.explicit_weekly_budget_in = Some(3.0);
        let sized = size_refill(6.0, &p, 0.0);
        assert_eq!(sized.planned_seconds, 3086);
        assert!(!sized.ceiling_binding);
    }

    /// The max-duration cap shorts a deep refill and reports itself;
    /// the residual depletion is the carry, so no ledger exists to
    /// test.
    #[test]
    fn deep_refill_shorts_at_the_cap() {
        let mut p = zone(SoilTexture::Clay);
        p.throughput_mm_hr = 8.0;
        // 20 mm at 0.7 capture and 8 mm/hr wants 12857 s.
        let sized = size_refill(20.0, &p, 0.0);
        assert_eq!(sized.planned_seconds, 3600);
        assert!(sized.session_capped);
        assert!(!sized.ceiling_binding);
    }

    fn cand(slug: &str, depletion: f64, raw: f64, planned: u32) -> AdmissionCandidate {
        AdmissionCandidate {
            slug: slug.into(),
            depletion_mm: depletion,
            raw_mm: raw,
            planned_seconds: planned,
        }
    }

    /// Seven due zones, a window that fits three: stress order decides
    /// who waters, the rest are named with the window reason, and
    /// nobody is dropped silently.
    #[test]
    fn admission_fits_three_of_seven() {
        // Stress ratios 1.9, 1.8, ... 1.3 in input order.
        let due: Vec<AdmissionCandidate> = (0..7)
            .map(|i| cand(&format!("z{i}"), (19.0 - i as f64) / 10.0 * 4.5, 4.5, 1200))
            .collect();
        // Wall price: run seconds plus a 120 s preamble per zone.
        let wall = |set: &[AdmissionCandidate]| -> u64 {
            set.iter()
                .map(|c| c.planned_seconds as u64 + 120)
                .sum::<u64>()
        };
        let out = admit_zones(&due, 4000, wall);
        assert_eq!(out.admitted, vec!["z0", "z1", "z2"]);
        assert_eq!(out.deferred.len(), 4);
        assert_eq!(
            out.deferred[0].reason,
            "waits for tomorrow: the morning window fits 3 of 7 zones that need water, \
             most depleted first"
        );
        assert_eq!(
            out.deferred
                .iter()
                .map(|z| z.slug.as_str())
                .collect::<Vec<_>>(),
            vec!["z3", "z4", "z5", "z6"]
        );
    }

    /// The most-stressed zone is admitted even when it alone overshoots
    /// the window: admission can never produce an empty morning while
    /// something needs water. The existing overshoot warn covers the
    /// rest.
    #[test]
    fn admission_always_admits_the_most_stressed() {
        let due = [
            cand("thirsty", 9.0, 4.5, 7200),
            cand("second", 8.0, 4.5, 600),
        ];
        let wall = |set: &[AdmissionCandidate]| -> u64 {
            set.iter().map(|c| c.planned_seconds as u64).sum()
        };
        let out = admit_zones(&due, 3600, wall);
        assert_eq!(out.admitted, vec!["thirsty"], "overshoot admits anyway");
        assert_eq!(out.deferred.len(), 1);
        assert_eq!(out.deferred[0].slug, "second");
        assert!(out.deferred[0].reason.contains("fits 1 of 2"));
    }

    /// Ordering is the stress RATIO, not absolute millimetres: wilting
    /// sand at 120% of its small RAW beats clay at 60% of its large one
    /// even though the clay deficit is bigger in mm.
    #[test]
    fn admission_orders_by_stress_ratio_not_absolute_depth() {
        let due = [
            cand("clay", 7.65, 12.75, 1200), // 0.6 ratio, 7.65 mm
            cand("sand", 5.4, 4.5, 1200),    // 1.2 ratio, 5.4 mm
        ];
        let wall = |set: &[AdmissionCandidate]| -> u64 {
            set.iter().map(|c| c.planned_seconds as u64).sum()
        };
        let out = admit_zones(&due, 1200, wall);
        assert_eq!(out.admitted, vec!["sand"], "ratio wins over depth");
        assert_eq!(out.deferred[0].slug, "clay");
    }

    /// A later, smaller zone may still fit after a bigger one deferred:
    /// greedy first-fit fills the window instead of stopping at the
    /// first overflow, and the deferred count names the true fit.
    #[test]
    fn admission_first_fit_keeps_walking_past_an_overflow() {
        let due = [
            cand("a", 9.0, 4.5, 1200), // ratio 2.0
            cand("b", 8.1, 4.5, 3000), // ratio 1.8, too big
            cand("c", 7.2, 4.5, 600),  // ratio 1.6, fits
        ];
        let wall = |set: &[AdmissionCandidate]| -> u64 {
            set.iter().map(|c| c.planned_seconds as u64).sum()
        };
        let out = admit_zones(&due, 2000, wall);
        assert_eq!(out.admitted, vec!["a", "c"]);
        assert_eq!(out.deferred.len(), 1);
        assert_eq!(out.deferred[0].slug, "b");
        assert!(out.deferred[0].reason.contains("fits 2 of 3"));
    }

    /// The plan row is additive-ready on the wire: absent fields
    /// default, a full row round-trips.
    #[test]
    fn soil_zone_plan_serde_additive_ready() {
        let minimal: SoilZonePlan = serde_json::from_str("{\"zone_slug\":\"front\"}").unwrap();
        assert_eq!(minimal.zone_slug, "front");
        assert_eq!(minimal.planned_seconds, 0);
        assert_eq!(minimal.deferred_reason, None);
        assert!(!minimal.due);
        let full = SoilZonePlan {
            zone_slug: "back".into(),
            depletion_mm: 7.5,
            taw_mm: 9.0,
            raw_mm: 4.5,
            due: true,
            deferred_kind: Some(SoilDeferKind::Window),
            deferred_reason: Some(
                "waits for tomorrow: the morning window fits 1 of 2 \
                                   zones that need water, most depleted first"
                    .into(),
            ),
            planned_seconds: 0,
            session_capped: false,
            ceiling_binding: false,
            ceiling_reason: None,
            evidence_days: 9,
            fallback_days: 5,
        };
        let json = serde_json::to_string(&full).unwrap();
        let back: SoilZonePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, full);
    }

    /// The fallback rung spreads the operator's explicit weekly target
    /// over seven days. Without one it spreads the zone's STARTING
    /// target, and it has to be the same starting target the weekly plan
    /// resolves, or a zone with no evidence would dry at a rate no other
    /// part of the engine agrees with. A non-positive explicit value (a
    /// file already on disk) falls through to the starting target rather
    /// than charging nothing.
    #[test]
    fn fallback_daily_mean_agrees_with_the_weekly_plans_starting_target() {
        let explicit = fallback_daily_etc_mm(Some(1.4), GrassSpecies::StAugustine);
        assert!((explicit - 1.4 * 25.4 / 7.0).abs() < 1e-9, "got {explicit}");
        for sp in [
            GrassSpecies::Bermuda,
            GrassSpecies::StAugustine,
            GrassSpecies::TallFescue,
            GrassSpecies::OrnamentalShrubs,
            GrassSpecies::VegetableGarden,
            GrassSpecies::DripXeriscape,
        ] {
            let slug = crate::engine::species_slug(sp);
            let (weekly_in, _) = crate::agronomy::default_weekly_target_in(slug);
            let got = fallback_daily_etc_mm(None, sp);
            assert!(
                (got - weekly_in * 25.4 / 7.0).abs() < 1e-9,
                "{slug}: fallback {got} disagrees with the starting target {weekly_in} in a week"
            );
        }
        // Vegetables transpire harder than turf, so the rung has to charge
        // them MORE, which the superseded flat rule got backwards.
        assert!(
            fallback_daily_etc_mm(None, GrassSpecies::VegetableGarden)
                > fallback_daily_etc_mm(None, GrassSpecies::Bermuda)
        );
        let zeroed = fallback_daily_etc_mm(Some(0.0), GrassSpecies::StAugustine);
        let (st_aug_in, _) = crate::agronomy::default_weekly_target_in(
            crate::engine::species_slug(GrassSpecies::StAugustine),
        );
        assert!((zeroed - st_aug_in * 25.4 / 7.0).abs() < 1e-9);
    }
}
