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
//               deficit back under RAW, for at most
//               MAX_CONSECUTIVE_DEFERS mornings running;
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
use crate::engine::soil_decisions::{MorningOutcome, PastMorning};
use crate::engine::species_catalog::{kc_at_doy_lat, lookup as species_lookup};
use crate::engine::water_balance::{
    refill_runtime_seconds, should_irrigate, step as balance_step, ZoneWaterState,
};

/// Trailing local days replayed to reconstruct a zone's depletion.
/// The window is bounded for predictable work; its length does not establish
/// the initial water content. The planner checks convergence of wet and dry
/// starting states before publishing a reconstructed deficit.
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

/// Mornings running that defer-by-deficit may hold one zone before it
/// waters anyway.
///
/// The count only grows on a forecast that did NOT deliver: rain that
/// actually falls refills the bucket, drops the zone back under RAW and
/// ends the run by itself. So this bounds consecutive forecast
/// FAILURES, never a genuinely rainy week, and a rolling forecast that
/// promises the same storm every morning can no longer park a zone at
/// the bottom of its bucket indefinitely.
///
/// Three is about a bucket's worth of stress at Florida summer ETc
/// (~4.25 mm/day) on the shallow-rooted turf profile (150 mm roots, MAD
/// 0.50): sand reaches TAW on its second due morning, sandy loam and
/// clay on their third, loam on its fourth. Past that the zone is at
/// the bottom of its bucket with the forecast still unfulfilled, and
/// the honest move is to water and say so.
pub const MAX_CONSECUTIVE_DEFERS: u32 = 3;

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

/// Resolve finite, nonnegative ET evidence for each date. A stored zero
/// is a real zero; absence and invalid values move to the next rung.
pub fn resolve_et0_days(
    dates: &[NaiveDate],
    ledger: &[(NaiveDate, f64)],
    archive: &[(NaiveDate, f64)],
) -> Vec<ResolvedEt0Day> {
    let find = |rows: &[(NaiveDate, f64)], d: NaiveDate| -> Option<f64> {
        rows.iter()
            .find(|(date, v)| *date == d && v.is_finite() && *v >= 0.0)
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
    crate::units::in_to_mm(weekly_in) / 7.0
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
    /// The operator's capture-efficiency override, or a non-positive
    /// value to take the catalog figures.
    ///
    /// This one number used to stand in for three physically different
    /// quantities: how much RAIN reaches the roots, how much IRRIGATION
    /// reaches them, and the factor a refill is grossed up by. They are
    /// not the same and they do not move together. A drip line loses
    /// almost nothing between emitter and soil; a fixed spray throws a
    /// fine mist across a wide arc. Charging both 30% waters the drip
    /// zone about a third longer than it needs.
    ///
    /// When set, the override still applies to everything, because an
    /// operator who has measured their system is describing their system.
    /// When unset, rain and irrigation each take their own figure.
    ///
    /// AS WIRED TODAY the assembly resolves the operator's value (or the
    /// 0.70 default) before this struct is built, so the unset branch is
    /// unreachable on the live path: both figures collapse to that one
    /// number, and the catalog's rain and per-head efficiencies below
    /// reach direct callers and tests only.
    pub capture_efficiency: f64,
    /// The zone's head, which decides how much of a run lands.
    pub sprinkler_type: crate::config::schema::SprinklerType,
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
    /// Recent 6 cm soil temperature, F, when the forecast models it.
    /// Judged against the species' dormancy threshold.
    pub soil_temp_f: Option<f64>,
}

/// A planting the soil says is asleep: the reading and the threshold
/// it fell below.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dormancy {
    pub soil_temp_f: f64,
    pub threshold_f: f64,
}

impl ZoneSoilParams {
    /// Whether the soil temperature puts this planting below its
    /// species' growth threshold. None when the species has no
    /// modeled dormancy or the forecast carries no soil temperature.
    pub fn dormancy(&self) -> Option<Dormancy> {
        let threshold_f = species_lookup(self.species).dormancy_soil_f?;
        let soil_temp_f = self.soil_temp_f?;
        (soil_temp_f < threshold_f).then_some(Dormancy {
            soil_temp_f,
            threshold_f,
        })
    }

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

/// Persisted decisions for completed scheduled mornings. The current local day
/// is explicit so its partial ET0 charge cannot be mistaken for a prior defer.
pub struct DeferHistory<'a> {
    pub today: NaiveDate,
    pub mornings: &'a [PastMorning],
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
/// How much of a RUN reaches the root zone.
///
/// The operator's measured override when they set one, else the figure
/// for the zone's own head. An operator who has run catch cups is
/// describing their system and is believed over the catalog.
pub fn irrigation_efficiency(p: &ZoneSoilParams) -> f64 {
    if p.capture_efficiency > 0.0 {
        return crate::engine::water_balance::resolve_capture_efficiency(p.capture_efficiency);
    }
    crate::agronomy::sprinkler_application_efficiency(crate::engine::sprinkler_slug(
        p.sprinkler_type,
    ))
}

/// How much of the RAIN that falls reaches the root zone.
///
/// Higher than irrigation efficiency, and not the same quantity. Rain
/// arrives as large drops over the whole area with no drift; what it
/// loses is canopy interception. Runoff is handled separately, by the
/// bucket clamping at field capacity and by the daily rain cap.
///
/// Charging rain a fixed spray head's losses credited the yard about a
/// fifth less rain than fell, which deepens the modelled deficit and
/// waters more.
pub fn rain_effectiveness(p: &ZoneSoilParams) -> f64 {
    if p.capture_efficiency > 0.0 {
        return crate::engine::water_balance::resolve_capture_efficiency(p.capture_efficiency);
    }
    crate::agronomy::RAIN_EFFECTIVENESS
}

/// non-positive disables clipping), and capture efficiency is clamped
/// to [0, 1] on the applied conversion.
pub fn build_replay_days(evidence: &[ZoneDayEvidence], p: &ZoneSoilParams) -> Vec<ReplayDay> {
    let eff = irrigation_efficiency(p);
    let cap = p
        .explicit_rain_cap_mm
        .filter(|c| *c > 0.0)
        .map(|c| c.clamp(crate::units::in_to_mm(0.05), crate::units::in_to_mm(5.0)));
    // A dormant planting transpires almost nothing. The soil temperature
    // in hand describes the recent day, so the crop coefficient is
    // zeroed for the newest evidence day only: earlier days keep the
    // demand they were charged, because the archive does not say when
    // the soil cooled.
    let dormant = p.dormancy().is_some();
    let newest = evidence.iter().map(|d| d.date).max();
    evidence
        .iter()
        .map(|day| {
            let etc_mm = if dormant && Some(day.date) == newest {
                0.0
            } else {
                match day.et0_mm {
                    Some(et0) if et0.is_finite() && et0 >= 0.0 => {
                        let doy = day.date.ordinal() as u16;
                        et0 * kc_at_doy_lat(p.species, doy, p.latitude_deg)
                    }
                    _ => fallback_daily_etc_mm(p.explicit_weekly_budget_in, p.species),
                }
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
    // A RAW no depletion can reach: no day reads due, the hold count
    // stays 0, and this is the plain fold.
    replay_with_holds(days, capture_efficiency, taw_mm, f64::INFINITY).0
}

/// The same fold, plus the run of HELD mornings the window ENDS on:
/// trailing days that closed with the bucket past RAW and no water
/// applied. This is a dry-stress diagnostic only: it cannot establish WHY
/// watering was withheld. The forecast-defer bound uses persisted morning
/// decisions in `plan_zone_with_history` instead.
pub fn replay_with_holds(
    days: &[ReplayDay],
    capture_efficiency: f64,
    taw_mm: f64,
    raw_mm: f64,
) -> (f64, u32) {
    let mut state = ZoneWaterState::default();
    let mut held = 0u32;
    for d in days {
        balance_step(
            &mut state,
            d.etc_mm,
            d.gross_rain_mm,
            d.applied_net_mm,
            capture_efficiency,
            taw_mm,
        );
        if d.applied_net_mm <= 0.0 && should_irrigate(state.depletion_mm, raw_mm) {
            held += 1;
        } else {
            held = 0;
        }
    }
    (state.depletion_mm, held)
}

// ---- Trigger ----

/// Defer-by-deficit: the per-zone replacement for the fixed defer
/// depth. A DUE zone holds when the expected post-rain depletion
/// max(depletion - eff x rain, 0) falls back under RAW, where `rain`
/// is the bias-corrected, probability-weighted next-24h forecast depth
/// (mm) the assembly resolves.
///
/// WHAT THE THRESHOLD ACTUALLY IS: the rain that holds a due zone is
/// (depletion - RAW) / eff, the OVERSHOOT past the trigger. On the
/// first due morning that overshoot is whatever fraction of a day's ETc
/// carried the bucket over the line, so almost any forecast holds. It
/// grows by ETc / eff each morning the promised rain fails to arrive,
/// and then STOPS: the bucket clamps at TAW, and from there the
/// threshold is flat at (TAW - RAW) / eff forever. At the live 0.70
/// factor, St Augustine on 150 mm roots at MAD 0.50, that plateau is
/// 6.4 mm (0.25 in) a morning on sand, 13.9 mm (0.55 in) on sandy loam,
/// 16.1 mm (0.63 in) on loam. A wet-season forecast that keeps
/// promising half an inch clears it every morning, which is how a
/// rolling forecast could park a zone at the bottom of its bucket
/// without limit. Hence `consecutive_defers`, counted by the caller,
/// and `MAX_CONSECUTIVE_DEFERS`.
///
/// This used to say the threshold was "zone physics: sand's small RAW
/// tolerates little forecast rain, clay's large RAW a lot". Only the
/// plateau scales with RAW. At the trigger it is the overshoot, and
/// sand's first due morning wants MORE rain than sandy loam's (5.7 mm
/// against 4.3), not less.
///
/// Returns the hold reason, or None when the zone is not due, the rain
/// leaves it due anyway, or it has already been held
/// `MAX_CONSECUTIVE_DEFERS` mornings running.
pub fn defer_by_deficit(
    depletion_mm: f64,
    raw_mm: f64,
    capture_efficiency: f64,
    expected_next_24h_rain_mm: f64,
    consecutive_defers: u32,
) -> Option<String> {
    if consecutive_defers >= MAX_CONSECUTIVE_DEFERS {
        return None;
    }
    let effective = rain_covers_deficit(
        depletion_mm,
        raw_mm,
        capture_efficiency,
        expected_next_24h_rain_mm,
    )?;
    Some(format!(
        "deferred: forecast rain refills the deficit ({effective:.1} of {depletion_mm:.1} \
         mm expected)"
    ))
}

/// The gate's arithmetic with the bound out of the way: how much of the
/// forecast rain reaches the roots, when that is enough to pull a DUE
/// zone's deficit back under RAW, else None. The gate and the
/// bound-reached row share this one copy of the physics.
fn rain_covers_deficit(
    depletion_mm: f64,
    raw_mm: f64,
    capture_efficiency: f64,
    expected_next_24h_rain_mm: f64,
) -> Option<f64> {
    if !should_irrigate(depletion_mm, raw_mm) {
        return None;
    }
    let effective = crate::engine::water_balance::resolve_capture_efficiency(capture_efficiency)
        * expected_next_24h_rain_mm.max(0.0);
    ((depletion_mm - effective).max(0.0) < raw_mm).then_some(effective)
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
    // Grossing up a refill asks how long THE HEAD must run to put a
    // depth into the root zone, so it takes the irrigation figure. A
    // drip zone charged a fixed spray's losses ran about a third longer
    // than it needed.
    let ideal_s = refill_runtime_seconds(
        depletion_mm,
        p.throughput_mm_hr,
        irrigation_efficiency(p),
        u32::MAX,
    );
    let capped_s = ideal_s.min(p.max_dur_s);
    let session_capped = ideal_s > p.max_dur_s;
    let (planned_seconds, ceiling_binding, ceiling_reason) = match p.explicit_weekly_budget_in {
        Some(target_in) if target_in > 0.0 && capped_s > 0 && p.throughput_mm_hr > 0.0 => {
            let target_mm = crate::units::in_to_mm(target_in);
            let headroom_mm = (target_mm - delivered_trailing_7d_mm.max(0.0)).max(0.0);
            let headroom_s = ((headroom_mm / p.throughput_mm_hr) * 3600.0).round() as i64;
            let headroom_s = headroom_s.clamp(0, u32::MAX as i64) as u32;
            if headroom_s < capped_s {
                let reason = format!(
                    "held to the weekly ceiling: {:.2} in of the {:.2} in target delivered \
                     in the last 7 days, {:.2} in of headroom left",
                    crate::units::mm_to_in(delivered_trailing_7d_mm.max(0.0)),
                    target_in,
                    crate::units::mm_to_in(headroom_mm)
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
    /// Horizon-aware explanation, shared by live dispatch sizing and outlook.
    #[serde(default)]
    pub planning_reason: Option<String>,
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
    /// Completed scheduled mornings held specifically for forecast rain since
    /// the last irrigation or bucket refill. Other holds do not spend this
    /// allowance, and today's partial reading never counts as a past morning.
    #[serde(default)]
    pub consecutive_defers: u32,
    /// The bound spent this tick: the forecast rain would have held this
    /// zone again, but `MAX_CONSECUTIVE_DEFERS` mornings have already
    /// gone that way, so it waters and `today_row` says how far it was
    /// carried.
    #[serde(default)]
    pub defer_bound_reached: bool,
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
    /// Remaining dependence on unknown initial depletion, mm. Two replays
    /// start at field capacity and wilting point; only convergence supports
    /// a single reconstructed deficit in any climate or root depth.
    #[serde(default)]
    pub initial_uncertainty_mm: f64,
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
        self.evidence_days < MIN_EVIDENCE_DAYS || self.initial_uncertainty_mm > 0.1
    }
}

/// Compute one zone's plan from its evidence window: replay, trigger,
/// defer-by-deficit, sizing. Window admission runs afterwards across
/// zones (`admit_zones`); a zone it defers gets `deferred_reason` set
/// and `planned_seconds` zeroed by the assembly.
pub fn plan_zone(
    p: &ZoneSoilParams,
    evidence: &[ZoneDayEvidence],
    expected_next_24h_rain_mm: Option<f64>,
    delivered_trailing_7d_mm: f64,
) -> SoilZonePlan {
    plan_zone_with_history(
        p,
        evidence,
        expected_next_24h_rain_mm,
        delivered_trailing_7d_mm,
        None,
    )
}

/// The live planner receives durable morning decisions alongside weather and
/// applied-water evidence. Missing history starts the defer count at zero;
/// ordinary waterless days are never substituted for missing decisions.
pub fn plan_zone_with_history(
    p: &ZoneSoilParams,
    evidence: &[ZoneDayEvidence],
    expected_next_24h_rain_mm: Option<f64>,
    delivered_trailing_7d_mm: f64,
    history: Option<DeferHistory<'_>>,
) -> SoilZonePlan {
    plan_zone_with_coverage(
        p,
        evidence,
        expected_next_24h_rain_mm,
        delivered_trailing_7d_mm,
        history,
        &[],
    )
}

/// Unknown historical rain widens the wet end of the state interval. It does
/// not become a zero-rain observation merely because ET was available that day.
pub fn plan_zone_with_coverage(
    p: &ZoneSoilParams,
    evidence: &[ZoneDayEvidence],
    expected_next_24h_rain_mm: Option<f64>,
    delivered_trailing_7d_mm: f64,
    history: Option<DeferHistory<'_>>,
    unknown_rain_dates: &[NaiveDate],
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
    // Replay from the first day we actually know something about.
    //
    // The window is fourteen days and a fresh install has about three,
    // so eleven leading days used to be charged the fallback ETc with
    // zero rain. That is not a cautious assumption, it is a fabricated
    // drought: enough dry days at the fallback rate drive any texture to
    // TAW, so a brand-new install read a full deficit on its first tick
    // and watered every zone to its cap on the first morning.
    //
    // A day before any evidence is UNKNOWN, not dry. The cold-start
    // anchor is field capacity, and it belongs at the first evidenced
    // day rather than fourteen days earlier. Gaps INSIDE the evidenced
    // span still charge the fallback, because there the yard demonstrably
    // existed and dried; it is only the leading run of nothing that was
    // invented.
    let first_known = evidence
        .iter()
        .position(|d| d.et0_mm.is_some() || d.gross_rain_mm > 0.0 || d.applied_valve_s > 0);
    let replayed: &[ZoneDayEvidence] = match first_known {
        Some(i) => &evidence[i..],
        // Nothing at all is known. Replaying anything would be inventing
        // it, so the bucket stays at its anchor and the plan reports as
        // starved, which the assembly publishes as absence.
        None => &[],
    };
    // The factor the replay applies is the RAIN one: balance_step
    // multiplies gross rain by it, while applied water entered the
    // evidence already net of the head's own losses. Passing one number
    // for both charged rain a fixed spray's drift.
    let rain_eff = rain_effectiveness(p);
    let mut state = ZoneWaterState::default();
    let mut upper = ZoneWaterState { depletion_mm: taw };
    let mut consecutive_defers = 0u32;
    for (day, charge) in replayed.iter().zip(build_replay_days(replayed, p)) {
        balance_step(
            &mut state,
            charge.etc_mm,
            charge.gross_rain_mm,
            charge.applied_net_mm,
            rain_eff,
            taw,
        );
        balance_step(
            &mut upper,
            charge.etc_mm,
            charge.gross_rain_mm,
            charge.applied_net_mm,
            rain_eff,
            taw,
        );
        if unknown_rain_dates.contains(&day.date) {
            state.depletion_mm = 0.0;
        }
        if let Some(history) = history.as_ref().filter(|history| day.date < history.today) {
            let decision = history
                .mornings
                .iter()
                .find(|morning| morning.date == day.date);
            if charge.applied_net_mm > 0.0
                || !should_irrigate(state.depletion_mm, raw)
                || decision.is_some_and(|morning| morning.outcome == MorningOutcome::NotDue)
            {
                consecutive_defers = 0;
            } else if decision
                .is_some_and(|morning| morning.outcome == MorningOutcome::ForecastRain)
            {
                consecutive_defers = consecutive_defers.saturating_add(1);
            }
            // Restriction, operator and unknown holds neither spend nor advance
            // the forecast-failure allowance. The next eligible morning resumes it.
        }
    }
    let depletion = state.depletion_mm;
    let due = should_irrigate(depletion, raw);
    let mut plan = SoilZonePlan {
        zone_slug: p.slug.clone(),
        depletion_mm: depletion,
        taw_mm: taw,
        raw_mm: raw,
        due,
        evidence_days,
        fallback_days,
        initial_uncertainty_mm: (upper.depletion_mm - depletion).max(0.0),
        consecutive_defers,
        ..Default::default()
    };
    // Asleep: the bucket is held where it is, whatever the deficit says.
    // Watering a dormant lawn to a growing lawn's schedule is the most
    // common way a continental yard wastes its autumn, and the deficit
    // will still be there, unchanged, when the soil warms.
    if let Some(d) = p.dormancy() {
        plan.due = false;
        plan.deferred_kind = Some(SoilDeferKind::Dormant);
        plan.deferred_reason = Some(format!(
            "Dormant: soil at {:.0}°F is below the {:.0}°F growth threshold for {}; holding the bucket",
            d.soil_temp_f,
            d.threshold_f,
            crate::engine::species_slug(p.species).replace('_', " ")
        ));
        return plan;
    }
    if !due {
        return plan;
    }
    // The balance can be due while future rain is unknown. Keep the due
    // evidence, but do not spend the rain-defer allowance or dispatch a refill
    // on a fabricated dry forecast.
    let Some(expected_next_24h_rain_mm) = expected_next_24h_rain_mm else {
        plan.deferred_kind = Some(SoilDeferKind::ForecastUnavailable);
        plan.deferred_reason =
            Some("Rain forecast unavailable for the next 24 hours; watering held".into());
        return plan;
    };
    // Deferring asks how much of the forecast RAIN will reach the roots.
    if let Some(reason) = defer_by_deficit(
        depletion,
        raw,
        rain_eff,
        expected_next_24h_rain_mm,
        consecutive_defers,
    ) {
        plan.deferred_reason = Some(reason);
        plan.deferred_kind = Some(SoilDeferKind::ForecastRain);
        return plan;
    }
    if consecutive_defers >= MAX_CONSECUTIVE_DEFERS {
        // Past the bound. When the rain WOULD have covered the deficit,
        // this zone is watering only because it cannot be held any
        // longer, and the row has to say so rather than reading like an
        // ordinary refill.
        let covered = rain_covers_deficit(depletion, raw, rain_eff, expected_next_24h_rain_mm);
        plan.defer_bound_reached = covered.is_some();
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
    if let Some(reason) = &plan.planning_reason {
        let reason = if plan.ceiling_binding {
            format!(
                "{reason}; {}",
                plan.ceiling_reason
                    .as_deref()
                    .unwrap_or("limited by the weekly ceiling")
            )
        } else if plan.session_capped {
            format!("{reason}; limited by the {cap_minutes}-minute cap")
        } else {
            reason.clone()
        };
        return (plan.planned_seconds, reason, plan.session_capped);
    }
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
    use std::fmt::Write;
    if plan.defer_bound_reached {
        // The forecast still shows rain, and the zone waters anyway: it
        // has been held as long as defer-by-deficit may hold one, so the
        // row says that rather than reading like an ordinary refill.
        let _ = write!(
            reason,
            "; deferred as far as it can be, held for forecast rain {} mornings running",
            plan.consecutive_defers
        );
    }
    if plan.session_capped {
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
    /// Required next-24h rain evidence is missing; this does not spend a
    /// forecast-rain defer morning or assert that rain is expected.
    ForecastUnavailable,
    /// The morning window could not fit this zone today.
    Window,
    /// The soil is below the species' growth threshold. The bucket is
    /// held where it is and nothing is due until the soil warms.
    Dormant,
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
        "{}the morning window fits {} of {} zones that need water, most depleted first",
        crate::voice::REASON_WAITS_FOR_TOMORROW,
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

    /// A known zero takes precedence over a model estimate; negative ET is invalid.
    #[test]
    fn zero_is_evidence_but_negative_et0_is_not() {
        let dates = [d(1), d(2)];
        let ledger = [(d(1), 0.0)];
        let archive = [(d(1), 4.1), (d(2), -1.0)];
        let out = resolve_et0_days(&dates, &ledger, &archive);
        assert_eq!(out[0].et0_mm, Some(0.0));
        assert_eq!(out[0].source, Et0DaySource::Ledger);
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
            sprinkler_type: crate::config::schema::SprinklerType::Spray,
            throughput_mm_hr: 15.0,
            max_dur_s: 3600,
            explicit_rain_cap_mm: None,
            explicit_weekly_budget_in: None,
            soil_temp_f: None,
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
    /// A drip zone is not charged a fixed spray's drift.
    ///
    /// One knob used to stand in for three physically different
    /// quantities: how much rain reaches the roots, how much irrigation
    /// reaches them, and the factor a refill is grossed up by. Charging
    /// a drip line 30% losses waters it about a third longer than it
    /// needs, every time.
    #[test]
    fn the_head_decides_how_much_of_a_run_lands() {
        use crate::config::schema::SprinklerType;
        let eff = |t: SprinklerType| {
            let mut p = zone(SoilTexture::Loam);
            p.capture_efficiency = 0.0; // no operator override
            p.sprinkler_type = t;
            irrigation_efficiency(&p)
        };
        assert!(eff(SprinklerType::Drip) > eff(SprinklerType::Rotor));
        assert!(eff(SprinklerType::Rotor) > eff(SprinklerType::Spray));
        assert!((eff(SprinklerType::Drip) - 0.90).abs() < 1e-9);
        assert!((eff(SprinklerType::Spray) - 0.65).abs() < 1e-9);
        // An unknown head takes the historical global figure, which is
        // the honest answer when we do not know what is out there.
        assert!((eff(SprinklerType::Other) - 0.70).abs() < 1e-9);
    }

    /// Rain is not irrigation, and it is credited higher.
    ///
    /// Rain arrives as large drops over the whole area with no drift and
    /// little evaporation in flight; what it loses is canopy
    /// interception. Runoff is handled elsewhere, by the bucket clamping
    /// at field capacity. Charging rain a spray head's losses credited
    /// the yard about a fifth less rain than fell, which deepens the
    /// modelled deficit and waters more.
    #[test]
    fn rain_is_credited_more_generously_than_a_sprinkler() {
        use crate::config::schema::SprinklerType;
        let mut p = zone(SoilTexture::Loam);
        p.capture_efficiency = 0.0;
        p.sprinkler_type = SprinklerType::Spray;
        assert!(
            rain_effectiveness(&p) > irrigation_efficiency(&p),
            "rain {} should beat a spray head {}",
            rain_effectiveness(&p),
            irrigation_efficiency(&p)
        );
    }

    /// An operator who measured their system is believed over the
    /// catalog, for every one of the three.
    #[test]
    fn a_measured_override_still_governs_everything() {
        use crate::config::schema::SprinklerType;
        let mut p = zone(SoilTexture::Loam);
        p.capture_efficiency = 0.55;
        p.sprinkler_type = SprinklerType::Drip;
        assert!((irrigation_efficiency(&p) - 0.55).abs() < 1e-9);
        assert!((rain_effectiveness(&p) - 0.55).abs() < 1e-9);
    }

    #[test]
    fn sand_yard_storm_holds_then_resumes_on_the_deficit() {
        let p = zone(SoilTexture::Sand);
        // July days with ET0 evidence at 5.0 mm. Kc for St Augustine in
        // July is the FAO-56 warm-season Kc_mid of 0.85, so each day
        // charges 4.25 mm of ETc. It charged 5.0 while the catalog put
        // this grass at 1.00, above even the table's cool-season row.
        let mut evidence: Vec<ZoneDayEvidence> = (1..=4)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        evidence[2].gross_rain_mm = 1.2 * 25.4; // the storm, July 3
                                                // Through the storm day: 4.25, then 8.5, then
                                                // 8.5 + 4.25 - 0.7 x 30.48 clamps at 0. The yard
                                                // holds. (It read 5 and 9 before the Kc correction.)
        let through_storm = plan_zone(&p, &evidence[..3], Some(0.0), 0.0);
        assert_eq!(
            through_storm.depletion_mm, 0.0,
            "the storm fills the bucket"
        );
        assert!(!through_storm.due);
        assert_eq!(through_storm.planned_seconds, 0);
        // One more day: depletion 4.25 crosses RAW 4.5? No: it sits just
        // under, so take two more days of evidence to cross it. The point
        // of the test is that the refill is sized to the deficit rather
        // than to a weekly quota, and that survives the Kc correction.
        let next_day = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert!(
            (next_day.depletion_mm - 4.25).abs() < 1e-9,
            "one July day at ET0 5.0 charges 5.0 x 0.85, got {}",
            next_day.depletion_mm
        );
        assert!(
            !next_day.due,
            "4.25 mm has not crossed RAW 4.5 yet, so the yard still holds"
        );

        // A second dry day does cross it. This is the correction visible
        // as behavior: a grass the table calls water-efficient reaches
        // its trigger a day later than the catalog's invented 1.00 made
        // it. The refill is still sized to the deficit rather than to a
        // weekly quota, which is what this test is really about.
        evidence.push(ZoneDayEvidence {
            date: d(5),
            et0_mm: Some(5.0),
            gross_rain_mm: 0.0,
            applied_valve_s: 0,
        });
        let next_day = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert!(
            (next_day.depletion_mm - 8.5).abs() < 1e-9,
            "two July days at 4.25 mm, got {}",
            next_day.depletion_mm
        );
        assert!(next_day.due, "depletion crossed RAW");
        assert!(
            next_day.planned_seconds > 2800 && next_day.planned_seconds < 3000,
            "refill sized to the deficit (8.5 / 0.7 / 15 mm/hr), got {}",
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
        let long = plan_zone(&p, &two_weeks, Some(0.0), 0.0);
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
        let before = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert!(before.due, "two dry days on sand read due");
        // Yesterday's run: an hour on the valve at 15 mm/hr x 0.7 puts
        // back 10.5 mm net, covering the day's charge and the standing
        // deficit both.
        evidence[1].applied_valve_s = 3600;
        let after = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert!(
            !after.due,
            "the applied evidence quenches the trigger, depletion {}",
            after.depletion_mm
        );
    }

    /// Days with no ET0 evidence charge the fallback daily mean: the
    /// explicit weekly target spread over seven days, else the species
    /// class figure. No 5.0 mm constant anywhere.
    /// A dormant bermuda lawn accrues no ETc for the day in hand and is
    /// not due, whatever its deficit says. The bucket is held.
    #[test]
    fn a_dormant_bermuda_accrues_no_etc_and_is_not_due() {
        let mut p = zone(SoilTexture::Loam);
        p.species = crate::config::schema::GrassSpecies::Bermuda;
        p.soil_temp_f = Some(45.0);
        let evidence: Vec<ZoneDayEvidence> = (1..=14)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let days = build_replay_days(&evidence, &p);
        assert_eq!(days.last().unwrap().etc_mm, 0.0, "today's demand is zero");
        assert!(
            days[0].etc_mm > 0.0,
            "earlier days keep the demand they were charged"
        );
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert!(!plan.due);
        assert_eq!(plan.deferred_kind, Some(SoilDeferKind::Dormant));
        assert!(
            plan.deferred_reason
                .as_deref()
                .unwrap()
                .starts_with("Dormant: soil at 45"),
            "{:?}",
            plan.deferred_reason
        );
        // The same soil under a cool-season lawn is still growing.
        p.species = crate::config::schema::GrassSpecies::KentuckyBluegrass;
        assert!(p.dormancy().is_none());
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert_ne!(plan.deferred_kind, Some(SoilDeferKind::Dormant));
        // And with no soil temperature in hand nothing is asleep.
        p.species = crate::config::schema::GrassSpecies::Bermuda;
        p.soil_temp_f = None;
        assert!(p.dormancy().is_none());
    }

    #[test]
    fn missing_et0_days_charge_the_fallback_rung() {
        let mut p = zone(SoilTexture::Loam);
        p.explicit_weekly_budget_in = Some(1.4);
        // A gap INSIDE the evidenced span. Day one carries a real
        // reading, so the yard demonstrably existed and dried; the two
        // days after it have no rung and take the fallback mean.
        //
        // Leading unevidenced days are a different case entirely and are
        // no longer charged at all: see all_fallback_window_reads_starved.
        let mut evidence: Vec<ZoneDayEvidence> = (1..=3)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: None,
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        evidence[0].et0_mm = Some(0.0);
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        let per_day = 1.4 * 25.4 / 7.0;
        assert!(
            (plan.depletion_mm - 2.0 * per_day).abs() < 1e-9,
            "the evidenced day charges zero; only the two \
             gap days at {per_day} mm, got {}",
            plan.depletion_mm
        );
        // Without an explicit target the species class supplies it.
        p.explicit_weekly_budget_in = None;
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        // The species starting target is 0.85 in/week now, following the
        // table's warm-season Kc_mid rather than the 1.00 the catalog
        // had invented.
        assert!((plan.depletion_mm - 2.0 * 0.85 * 25.4 / 7.0).abs() < 1e-9);
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
        let plan = plan_zone(&p, &[], Some(0.0), 0.0);
        assert_eq!(
            plan,
            SoilZonePlan {
                planning_reason: None,
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
                initial_uncertainty_mm: p.taw_mm(),
                consecutive_defers: 0,
                defer_bound_reached: false,
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
    /// A fresh install does not read a drought it never had.
    ///
    /// The window is fourteen days and a new install has about three, so
    /// eleven leading days used to be charged the species fallback ETc
    /// with zero rain. Enough dry days at that rate drive any texture to
    /// TAW, so day one showed a full deficit and every zone watered to
    /// its cap on the first morning. A day before any evidence is
    /// unknown, not dry.
    #[test]
    fn a_fresh_install_replays_only_the_days_it_knows() {
        let p = zone(SoilTexture::Loam);
        // Eleven days of nothing, then three real ones.
        let mut evidence: Vec<ZoneDayEvidence> = (1..=14)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: None,
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        for day in evidence.iter_mut().skip(11) {
            day.et0_mm = Some(5.0);
        }
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);

        // Three evidenced days of ETc, not fourteen days of assumption.
        let kc = crate::engine::kc_at_doy_lat(p.species, d(12).ordinal() as u16, p.latitude_deg);
        let expected = 3.0 * 5.0 * kc;
        assert!(
            (plan.depletion_mm - expected).abs() < 0.5,
            "three known days charge about {expected} mm, got {}",
            plan.depletion_mm
        );
        assert!(
            plan.depletion_mm < p.taw_mm(),
            "and nothing like the full {} mm the fabricated window produced",
            p.taw_mm()
        );
    }

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
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert_eq!(plan.evidence_days, 0);
        assert_eq!(plan.fallback_days, 14);
        assert!(plan.evidence_starved());
        // Nothing is known, so nothing is replayed. This used to pin TAW,
        // because fourteen fallback days at the species mean with zero
        // rain drive any texture to a full deficit. That is a fabricated
        // drought: a brand-new install read a full deficit on its first
        // tick and watered every zone to its cap on the first morning.
        //
        // An unevidenced day is unknown, not dry. The bucket stays at its
        // anchor and the starved flag is what tells the assembly to
        // publish absence.
        assert!(
            plan.depletion_mm.abs() < 1e-9,
            "an all-unknown window invents no deficit, got {}",
            plan.depletion_mm
        );
        assert!(
            !plan.due,
            "and nothing is due on a yard we know nothing about"
        );
        // One rung resolving is not enough: thirteen of the fourteen
        // days still charge the fallback mean, so the guard holds.
        evidence[13].et0_mm = Some(0.2);
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert_eq!(plan.evidence_days, 1);
        assert_eq!(plan.fallback_days, 13);
        assert!(plan.evidence_starved());
        // Two days still starve; the third lifts the guard.
        evidence[12].et0_mm = Some(0.2);
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert_eq!(plan.evidence_days, 2);
        assert!(plan.evidence_starved());
        evidence[11].et0_mm = Some(0.2);
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert_eq!(plan.evidence_days, MIN_EVIDENCE_DAYS);
        assert_eq!(plan.fallback_days, 11);
        assert!(
            plan.evidence_starved(),
            "three low-ET days cannot resolve the initial moisture"
        );
        // A nonzero rain row or applied seconds is evidence too.
        evidence[13].et0_mm = None;
        evidence[0].gross_rain_mm = 2.0;
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert_eq!(plan.evidence_days, MIN_EVIDENCE_DAYS);
        assert!(!plan.evidence_starved());
        evidence[0].gross_rain_mm = 0.0;
        evidence[5].applied_valve_s = 600;
        let plan = plan_zone(&p, &evidence, Some(0.0), 0.0);
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
        // expected refill; post-rain depletion 8.6 falls under RAW. The
        // threshold is the OVERSHOOT past RAW (1.0 mm here), not RAW.
        let held = defer_by_deficit(10.0, 9.0, 0.70, 2.0, 0);
        assert_eq!(
            held.as_deref(),
            Some("deferred: forecast rain refills the deficit (1.4 of 10.0 mm expected)")
        );
        // A deeper deficit rides through the same rain.
        assert_eq!(defer_by_deficit(20.0, 9.0, 0.70, 2.0, 0), None);
        // Not due: nothing to defer.
        assert_eq!(defer_by_deficit(8.0, 9.0, 0.70, 50.0, 0), None);
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
        let plan = plan_zone(&p, &evidence, Some(20.0), 0.0);
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

    /// THE BOUND: the rain that holds a zone on its first due morning
    /// cannot hold it forever.
    ///
    /// Sandy loam at 4.25 mm/day ETc reads due on the third dry morning
    /// (12.75 mm against RAW 9.75) and sits clamped at TAW 19.5 from the
    /// fifth, where the rain that holds it stops growing: a flat
    /// (19.5 - 9.75) / 0.70 = 13.93 mm every morning, forever. A standing
    /// 20 mm forecast clears that plateau every one of those mornings, so
    /// before the bound this zone deferred every morning for as long as
    /// the forecast kept promising rain that never fell.
    ///
    /// The gate stops at `MAX_CONSECUTIVE_DEFERS`, counting only the
    /// completed scheduled mornings the ledger actually calls forecast defers.
    #[test]
    fn defer_by_deficit_stops_at_the_consecutive_bound() {
        // The gate itself: the same inputs hold one morning and water the
        // next, on the count alone.
        assert!(
            defer_by_deficit(10.0, 9.0, 0.70, 2.0, MAX_CONSECUTIVE_DEFERS - 1).is_some(),
            "the last allowed morning still holds"
        );
        assert_eq!(
            defer_by_deficit(10.0, 9.0, 0.70, 2.0, MAX_CONSECUTIVE_DEFERS),
            None,
            "the bound waters instead of holding"
        );
        // Plan level, with a 20 mm forecast standing every morning.
        let p = zone(SoilTexture::SandyLoam);
        let dry = |n: u32| -> Vec<ZoneDayEvidence> {
            (1..=n)
                .map(|day| ZoneDayEvidence {
                    date: d(day),
                    et0_mm: Some(5.0),
                    gross_rain_mm: 0.0,
                    applied_valve_s: 0,
                })
                .collect()
        };
        let history: Vec<PastMorning> = (3..=5)
            .map(|day| PastMorning {
                date: d(day),
                outcome: MorningOutcome::ForecastRain,
            })
            .collect();
        for days in 3..=5 {
            let plan = plan_zone_with_history(
                &p,
                &dry(days),
                Some(20.0),
                0.0,
                Some(DeferHistory {
                    today: d(days),
                    mornings: &history,
                }),
            );
            assert!(plan.due, "{days} dry days read due");
            assert!(
                plan.deferred_reason.is_some(),
                "{days} dry days are still inside the bound: {plan:?}"
            );
            assert_eq!(plan.planned_seconds, 0);
            assert!(!plan.defer_bound_reached);
        }
        // One hold too many: the zone waters, at its cap, and the row
        // says how far it was carried.
        let plan = plan_zone_with_history(
            &p,
            &dry(6),
            Some(20.0),
            0.0,
            Some(DeferHistory {
                today: d(6),
                mornings: &history,
            }),
        );
        assert_eq!(plan.consecutive_defers, MAX_CONSECUTIVE_DEFERS);
        assert!(plan.defer_bound_reached);
        assert_eq!(plan.deferred_reason, None);
        assert_eq!(plan.planned_seconds, p.max_dur_s);
        let (seconds, reason, _) = today_row(&plan, p.max_dur_s / 60);
        assert!(seconds > 0, "the bound waters");
        assert!(
            reason.starts_with("soil refill:") && reason.contains("deferred as far as it can be"),
            "{reason}"
        );
        // Rain that ACTUALLY falls ends the run: the bucket refills, the
        // zone drops under RAW, and the count starts over. A genuinely
        // rainy week never spends the bound; only a forecast that keeps
        // failing does.
        let mut wet = dry(7);
        wet[3].gross_rain_mm = 30.0;
        let plan = plan_zone_with_history(
            &p,
            &wet,
            Some(20.0),
            0.0,
            Some(DeferHistory {
                today: d(7),
                mornings: &history,
            }),
        );
        assert!(plan.due);
        assert_eq!(plan.consecutive_defers, 0);
        assert!(plan.deferred_reason.is_some(), "{plan:?}");
    }

    #[test]
    fn restriction_holds_and_today_partial_charge_do_not_spend_forecast_defers() {
        let params = zone(SoilTexture::SandyLoam);
        let mut evidence: Vec<ZoneDayEvidence> = (1..=8)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let mut mornings: Vec<PastMorning> = (3..=8)
            .map(|day| PastMorning {
                date: d(day),
                outcome: if [3, 5, 8].contains(&day) {
                    MorningOutcome::ForecastRain
                } else {
                    MorningOutcome::OtherHold
                },
            })
            .collect();
        for partial in [0.001, 2.0, 5.0] {
            evidence[7].et0_mm = Some(partial);
            let plan = plan_zone_with_history(
                &params,
                &evidence,
                Some(20.0),
                0.0,
                Some(DeferHistory {
                    today: d(8),
                    mornings: &mornings,
                }),
            );
            assert_eq!(
                plan.consecutive_defers, 2,
                "only completed forecast holds count; today and restrictions do not"
            );
            assert_eq!(plan.deferred_kind, Some(SoilDeferKind::ForecastRain));
        }
        let unknown = plan_zone(&params, &evidence, Some(20.0), 0.0);
        assert_eq!(
            unknown.consecutive_defers, 0,
            "waterless days cannot invent decisions"
        );
        assert!(!unknown.defer_bound_reached);

        // One more real completed forecast failure reaches the bound exactly.
        mornings
            .iter_mut()
            .find(|morning| morning.date == d(7))
            .unwrap()
            .outcome = MorningOutcome::ForecastRain;
        let plan = plan_zone_with_history(
            &params,
            &evidence,
            Some(20.0),
            0.0,
            Some(DeferHistory {
                today: d(8),
                mornings: &mornings,
            }),
        );
        assert_eq!(plan.consecutive_defers, 3);
        assert!(plan.defer_bound_reached);

        // Even a partial real run resets the forecast-failure episode.
        evidence[5].applied_valve_s = 60;
        let plan = plan_zone_with_history(
            &params,
            &evidence,
            Some(20.0),
            0.0,
            Some(DeferHistory {
                today: d(8),
                mornings: &mornings,
            }),
        );
        assert_eq!(plan.consecutive_defers, 1);
    }

    #[test]
    fn missing_rain_holds_due_soil_even_after_defer_bound_and_zero_is_usable() {
        let params = zone(SoilTexture::SandyLoam);
        let evidence: Vec<ZoneDayEvidence> = (1..=8)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let mornings: Vec<PastMorning> = (3..=7)
            .map(|day| PastMorning {
                date: d(day),
                outcome: MorningOutcome::ForecastRain,
            })
            .collect();
        let history = || {
            Some(DeferHistory {
                today: d(8),
                mornings: &mornings,
            })
        };
        let missing = plan_zone_with_history(&params, &evidence, None, 0.0, history());
        assert!(missing.due);
        assert!(missing.consecutive_defers >= MAX_CONSECUTIVE_DEFERS);
        assert_eq!(missing.planned_seconds, 0);
        assert_eq!(
            missing.deferred_kind,
            Some(SoilDeferKind::ForecastUnavailable)
        );
        assert!(missing
            .deferred_reason
            .as_deref()
            .unwrap()
            .contains("unavailable"));
        assert!(
            !missing.defer_bound_reached,
            "missing QPF is not another forecast-rain failure"
        );
        let dry = plan_zone_with_history(&params, &evidence, Some(0.0), 0.0, history());
        assert!(
            dry.planned_seconds > 0,
            "covered zero QPF permits a due refill"
        );
        assert_eq!(dry.deferred_kind, None);
        assert_eq!(missing.consecutive_defers, dry.consecutive_defers);
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
            planning_reason: None,
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
            initial_uncertainty_mm: 0.0,
            consecutive_defers: 2,
            defer_bound_reached: false,
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
    #[test]
    fn deep_roots_and_low_et_do_not_assume_the_initial_state_washed_out() {
        let mut p = zone(SoilTexture::Clay);
        p.root_depth_mm = Some(800.0);
        p.latitude_deg = -35.0;
        let mut evidence: Vec<_> = (1..=14)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(0.2),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let unknown = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert!(unknown.initial_uncertainty_mm > 100.0);
        assert!(unknown.evidence_starved());
        evidence[13].gross_rain_mm = 1000.0;
        let filled = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert_eq!(filled.initial_uncertainty_mm, 0.0);
        assert_eq!(filled.depletion_mm, 0.0);
        assert!(!filled.evidence_starved());
    }

    #[test]
    fn zero_et_days_charge_no_fallback_crop_demand() {
        let p = zone(SoilTexture::Sand);
        let evidence = vec![ZoneDayEvidence {
            date: d(1),
            et0_mm: Some(0.0),
            gross_rain_mm: 0.0,
            applied_valve_s: 0,
        }];
        assert_eq!(build_replay_days(&evidence, &p)[0].etc_mm, 0.0);
    }

    #[test]
    fn missing_historical_rain_does_not_establish_a_drought() {
        let p = zone(SoilTexture::Sand);
        let mut evidence: Vec<_> = (1..=14)
            .map(|day| ZoneDayEvidence {
                date: d(day),
                et0_mm: Some(5.0),
                gross_rain_mm: 0.0,
                applied_valve_s: 0,
            })
            .collect();
        let dry = plan_zone(&p, &evidence, Some(0.0), 0.0);
        assert!(!dry.evidence_starved());
        let unknown = plan_zone_with_coverage(&p, &evidence, Some(0.0), 0.0, None, &[d(13)]);
        assert!(unknown.initial_uncertainty_mm > 0.1);
        assert!(unknown.evidence_starved());
        evidence[13].gross_rain_mm = 60.0;
        let storm = plan_zone_with_coverage(&p, &evidence, Some(0.0), 0.0, None, &[d(13)]);
        assert_eq!(storm.depletion_mm, 0.0);
        assert_eq!(storm.initial_uncertainty_mm, 0.0);
    }
}
