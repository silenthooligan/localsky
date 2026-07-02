// Weekly water-budget allocation. Per-zone target depth (inches/week)
// across N sessions, rain-aware, with minimum-interval pacing so each
// session is a real soak rather than daily light sprinkles.
//
// Phase 3E extraction from src/refresher.rs::compute_water_budgets.
// Pure function: takes typed per-zone inputs and the merged forecast,
// returns a WaterBudget record per zone. HA-entity reading + zone
// enumeration stay in refresher.rs as the v0.1 glue path; the v2
// scheduler will enumerate zones from config.zones instead.

use crate::forecast::snapshot::ForecastSnapshot;
use crate::ha::snapshot::WaterBudget;

/// Per-zone inputs for the budget allocator. Populated either from HA
/// helpers (v0.1) or from config.zones + controller state (v2+).
#[derive(Debug, Clone)]
pub struct ZoneBudgetInputs {
    pub slug: String,
    pub name: String,
    pub weekly_budget_in: f64,
    pub sessions_per_week: u32,
    pub mode_active: bool,
    pub throughput_mm_hr: f64,
    pub max_dur_s: u32,
    pub last_run_epoch: i64,
}

/// Global parameters (same across all zones in one tick).
#[derive(Debug, Clone, Copy)]
pub struct BudgetGlobals {
    /// EngineParams.capture_efficiency (default 0.70). The fraction of
    /// gross rain that reaches the root zone after runoff + canopy +
    /// evaporation losses.
    pub capture_efficiency: f64,
    /// Skip a session when forecast 24h rain >= this depth (inches).
    pub session_rain_defer_in: f64,
    /// Heat-index ET multiplier (1.00..=1.30). Bumps session length on
    /// hot days so the planned depth lands in the root zone after the
    /// accelerated evaporation.
    pub heat_multiplier: f64,
    pub now_epoch: i64,
}

/// Compute today's recommendation for a single zone given the merged
/// 7-day forecast.
pub fn compute_zone(
    zone: &ZoneBudgetInputs,
    g: &BudgetGlobals,
    fc: &ForecastSnapshot,
) -> WaterBudget {
    let sessions = zone.sessions_per_week.max(1);

    // Forecast: next-24h rain (sum of hourly[0..24] precip).
    let next_24h_rain_in = fc.next_n_hours_precip_in(24);
    // 7-day probability-weighted total rain.
    let week_rain_weighted_in: f64 = fc
        .daily
        .iter()
        .take(7)
        .map(|d| d.precip_sum_in * (d.precip_probability_max as f64) / 100.0)
        .sum();

    // Water-balance math: weekly budget, minus expected captured rain.
    let weekly_budget_mm = zone.weekly_budget_in * 25.4;
    let expected_rain_mm = week_rain_weighted_in * 25.4 * g.capture_efficiency;
    let needed_mm = (weekly_budget_mm - expected_rain_mm).max(0.0);
    let mm_per_session = needed_mm / sessions as f64;

    let seconds_per_session = if zone.throughput_mm_hr > 0.0 {
        ((mm_per_session / zone.throughput_mm_hr) * 3600.0 * g.heat_multiplier
            / g.capture_efficiency) as u32
    } else {
        0
    };
    let session_capped = seconds_per_session > zone.max_dur_s;
    let session_final = seconds_per_session.min(zone.max_dur_s);

    let min_interval_days = (7.0 / sessions as f64).floor() as i64;
    let days_since_last_run = if zone.last_run_epoch > 0 {
        (g.now_epoch - zone.last_run_epoch) / 86400
    } else {
        i64::MAX / 2
    };

    let (today_seconds, today_reason) = if !zone.mode_active {
        (0u32, "budget mode off (zone paused)".to_string())
    } else if next_24h_rain_in >= g.session_rain_defer_in {
        (
            0,
            format!(
                "rain expected next 24h ({:.2}\" forecast >= {:.2}\")",
                next_24h_rain_in, g.session_rain_defer_in
            ),
        )
    } else if days_since_last_run < min_interval_days {
        (
            0,
            format!(
                "last run {} day(s) ago -- minimum interval is {} days at {} sessions/wk",
                days_since_last_run, min_interval_days, sessions
            ),
        )
    } else if needed_mm <= 0.0 {
        (
            0,
            format!(
                "forecast rain {:.2}\" covers the {:.2}\" weekly budget",
                week_rain_weighted_in, zone.weekly_budget_in
            ),
        )
    } else {
        (
            session_final,
            format!(
                "scheduled session 1 of {} this week -- {:.2} mm depth = {:.0} min",
                sessions,
                mm_per_session,
                session_final as f64 / 60.0
            ),
        )
    };

    WaterBudget {
        zone_slug: zone.slug.clone(),
        zone_name: zone.name.clone(),
        mode_active: zone.mode_active,
        weekly_budget_in: zone.weekly_budget_in,
        sessions_per_week: sessions,
        expected_rain_mm,
        needed_mm,
        mm_per_session,
        seconds_per_session,
        session_capped,
        last_run_epoch: zone.last_run_epoch,
        today_seconds,
        today_reason,
    }
}
