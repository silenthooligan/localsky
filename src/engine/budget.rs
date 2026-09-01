// Weekly water balance. Per-zone gross target depth (inches/week)
// settled against what the week has already received: observed rain
// (trailing window), irrigation already applied (trailing window), and
// a bias-corrected forecast credit covering only the days between now
// and the zone's next expected session. The remainder is split across
// the sessions still expected this week.
//
// This is THE budget implementation: `compute_water_budgets` in
// src/refresher.rs is a thin assembly that gathers inputs (store reads,
// config resolution) and calls `compute_zone` per zone, so tests and
// the live path exercise one formula.
//
// Semantics are GROSS, homeowner-facing ("an inch a week including
// rain"): no capture-efficiency division and no heat multiplier on
// delivery. `et_heat_multiplier` stays live for the ETc/soil paths;
// capture efficiency stays live for the soil projection. Neither
// belongs in session sizing against a gross target.

use crate::engine::forecast_bias::BiasModel;
use crate::forecast::snapshot::ForecastSnapshot;
use crate::ha::snapshot::WaterBudget;

/// Defer today's session when the next-24h forecast rain reaches this
/// depth (inches). The forward credit deliberately excludes the next 24
/// hours (and today's model rain in general); this gate is what handles
/// imminent rain.
pub const SESSION_RAIN_DEFER_IN: f64 = 0.10;

/// Capture factor on the WIRE's `expected_rain_mm` field. Every release
/// before the balance emitted that field as the weighted 7-day forecast
/// x 25.4 x 0.7, and external consumers (the HA budget-override
/// automation's install, template sensors thresholding on it) read that
/// scaling; the balance itself never consumes this figure, so the wire
/// keeps the historical scaling rather than silently growing 43%.
pub const EXPECTED_RAIN_WIRE_CAPTURE: f64 = 0.7;

/// Per-zone inputs for the balance. The assembly resolves these from
/// config, the runs history, and the zone runtime map.
#[derive(Debug, Clone)]
pub struct ZoneBalanceInputs {
    pub slug: String,
    pub name: String,
    /// Gross weekly target, inches (homeowner semantics: includes rain).
    pub weekly_budget_in: f64,
    pub sessions_per_week: u32,
    pub mode_active: bool,
    pub throughput_mm_hr: f64,
    /// Effective cap: zone max_duration min any active restriction cap.
    pub max_dur_s: u32,
    /// End epoch of the zone's most recent completed watering event
    /// (clustered run-history evidence). 0 = none on record in the
    /// trailing window; the zone is then eligible immediately.
    pub last_run_epoch: i64,
    /// Gross irrigation depth applied in the trailing window (mm),
    /// reconstructed as union valve-open seconds x throughput.
    pub applied_trailing_mm: f64,
    /// Completed watering events inside the trailing window (clustered:
    /// cycle-soak segments and duplicate manual/observer rows count as
    /// one event).
    pub sessions_done: u32,
    /// True when `weekly_budget_in` / `sessions_per_week` above are the
    /// slug-inferred agronomic default rather than the operator's own
    /// setting. Carried straight out on the `WaterBudget` so the UI can
    /// say which zones are watering on a guess; it changes no math here.
    pub target_inferred: bool,
}

/// Cross-zone inputs, computed once per tick by the assembly.
#[derive(Debug, Clone)]
pub struct BalanceGlobals {
    pub now_epoch: i64,
    /// Defer threshold (inches over the next 24 forecast hours).
    pub session_rain_defer_in: f64,
    /// Observed rain over the trailing window (mm), resolved through the
    /// source ladder (gauge/radar ledger rows, model archive, none).
    pub observed_rain_mm: f64,
    /// Which ladder rung supplied `observed_rain_mm`:
    /// "gauge" | "radar" | "model_archive" | "none".
    pub observed_rain_source: String,
    /// Forecast bias model (identity when under-trained); applied ONLY
    /// to the forward forecast credit, never to observed terms.
    pub bias: BiasModel,
}

/// Month of a daily entry in the configured timezone, falling back to
/// the month of `now` when the entry has no resolvable date.
fn entry_month(entry_epoch: i64, now_epoch: i64) -> u32 {
    use chrono::Datelike;
    crate::timeutil::local_date(entry_epoch)
        .or_else(|| crate::timeutil::local_date(now_epoch))
        .map(|d| d.month())
        .unwrap_or(1)
}

/// Compute today's recommendation for a single zone.
pub fn compute_zone(
    zone: &ZoneBalanceInputs,
    g: &BalanceGlobals,
    fc: &ForecastSnapshot,
) -> WaterBudget {
    use chrono::Datelike;
    // Clamped, not just floored at 1. The pacing below is
    // floor(7/sessions) days, so anything above 7 gives a 0 day interval
    // and the spacing gate stops holding a zone that already watered
    // today. The config API rejects out-of-range values on the way in;
    // this covers a file already on disk carrying one, which must not
    // start double-watering just because it loads.
    let sessions = zone.sessions_per_week.clamp(1, 7);

    // Forward context: next-24h rain for the defer gate, and the 7-day
    // probability-weighted total. The weighted total is wire-only
    // (`expected_rain_mm`, kept at its historical capture-adjusted
    // scaling for the external consumers that read it); the balance
    // itself never subtracts a whole-week forecast.
    //
    // The defer gate reads the PROBABILITY-WEIGHTED next-24h depth, the
    // same weighting the forward credit and the wire total use. It used a
    // raw model sum, so a low-probability drizzle carried the full weight
    // of certain rain against a 0.10 inch threshold and could zero every
    // zone nearly every day in a convective climate.
    let next_24h_rain_in = fc.next_n_hours_precip_weighted_in(24);
    let week_rain_weighted_in: f64 = fc
        .daily
        .iter()
        .take(7)
        .map(|d| d.precip_sum_in * d.precip_weight())
        .sum();
    let expected_rain_mm = week_rain_weighted_in * 25.4 * EXPECTED_RAIN_WIRE_CAPTURE;

    // Pacing: sessions space out at floor(7/sessions) days. The forward
    // credit window runs from tomorrow until the next expected session,
    // NEVER the whole week: rain past the next session will be observed
    // (and credited as such) before it matters, and crediting it now
    // would double-count.
    let min_interval_days = (7.0 / sessions as f64).floor() as i64;
    // LOCAL-CALENDAR-DAY difference, not floored elapsed seconds: the
    // evidence anchors on the previous event's END (near sunrise) while
    // the next dispatch evaluates EARLIER in the morning, so an epoch
    // floor reads interval-minus-a-few-hours as interval-1 and blocks
    // every intended session day (a 2/week zone would stretch to every
    // 4 days). "Sessions run N days apart" is calendar semantics.
    let days_since_last_run = if zone.last_run_epoch > 0 {
        match (
            crate::timeutil::local_date(g.now_epoch),
            crate::timeutil::local_date(zone.last_run_epoch),
        ) {
            (Some(now_d), Some(last_d)) => (now_d - last_d).num_days(),
            // Unresolvable dates: nearest-day rounding (morning-to-morning
            // deltas sit within half a day of a whole day).
            _ => (g.now_epoch - zone.last_run_epoch + 43_200) / 86_400,
        }
    } else {
        i64::MAX / 2
    };
    let days_until_next = if zone.last_run_epoch > 0 {
        (min_interval_days - days_since_last_run).max(0)
    } else {
        0
    };
    // daily[0] is today; today's rain is either measured (observed term)
    // or imminent (defer gate), so the credit starts at tomorrow and
    // stops BEFORE the session day itself.
    let credit_days = (days_until_next.saturating_sub(1)).max(0) as usize;
    let forecast_credit_mm: f64 = fc
        .daily
        .iter()
        .skip(1)
        .take(credit_days)
        .map(|d| {
            d.precip_sum_in
                * d.precip_weight()
                * g.bias
                    .multiplier_for(entry_month(d.time_epoch, g.now_epoch))
                * 25.4
        })
        .sum();
    let forecast_credit_source = if credit_days == 0 {
        "none".to_string()
    } else {
        "bias_forecast".to_string()
    };

    // The balance. All terms gross mm.
    let weekly_target_gross_mm = zone.weekly_budget_in * 25.4;
    let remainder = (weekly_target_gross_mm
        - g.observed_rain_mm
        - zone.applied_trailing_mm
        - forecast_credit_mm)
        .max(0.0);
    let remaining_sessions = sessions.saturating_sub(zone.sessions_done).max(1);
    let session_gross_mm = remainder / remaining_sessions as f64;
    let seconds_per_session = if zone.throughput_mm_hr > 0.0 {
        // round(), not truncate: float error in the mm arithmetic must
        // not shave a second off an exact figure (38.1 mm at 25.4 mm/hr
        // is 5400 s, not 5399).
        ((session_gross_mm / zone.throughput_mm_hr) * 3600.0).round() as u32
    } else {
        0
    };
    let session_capped = seconds_per_session > zone.max_dur_s;
    let session_final = seconds_per_session.min(zone.max_dur_s);

    let current_month = crate::timeutil::local_date(g.now_epoch)
        .map(|d| d.month())
        .unwrap_or(1);
    let bias_multiplier = g.bias.multiplier_for(current_month);
    let bias_sample_count = g.bias.sample_count_for(current_month) as u32;

    // Today's recommendation. Reasons name what actually decided:
    // covered beats defer beats spacing, since "the week is already
    // covered" is the stronger truth than "a needed session is pushed".
    let (today_seconds, today_reason) = if !zone.mode_active {
        // Defensive only; mode_active is hard-true on live paths.
        (0u32, "budget mode off".to_string())
    } else if remainder <= 0.0 {
        (
            0,
            format!(
                "covered by rain and prior watering ({:.2}\" rain + {:.2}\" applied against \
                 the {:.2}\" weekly target)",
                g.observed_rain_mm / 25.4,
                zone.applied_trailing_mm / 25.4,
                zone.weekly_budget_in
            ),
        )
    } else if next_24h_rain_in >= g.session_rain_defer_in {
        (
            0,
            format!(
                "deferred for forecast rain ({:.2}\" expected in the next 24h weighted by \
                 probability, threshold {:.2}\")",
                next_24h_rain_in, g.session_rain_defer_in
            ),
        )
    } else if days_since_last_run < min_interval_days {
        (
            0,
            format!(
                "spaced {} day(s) after the last session; sessions run {} day(s) apart at \
                 {} per week",
                days_since_last_run, min_interval_days, sessions
            ),
        )
    } else {
        (
            session_final,
            format!(
                "session {} of {} this week: {:.1} mm over {:.0} min",
                zone.sessions_done + 1,
                sessions,
                session_gross_mm,
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
        needed_mm: remainder,
        mm_per_session: session_gross_mm,
        seconds_per_session,
        session_capped,
        last_run_epoch: zone.last_run_epoch,
        today_seconds,
        today_reason,
        observed_rain_mm: g.observed_rain_mm,
        observed_rain_source: g.observed_rain_source.clone(),
        applied_mm: zone.applied_trailing_mm,
        forecast_credit_mm,
        forecast_credit_source,
        bias_multiplier,
        bias_sample_count,
        remaining_sessions,
        target_inferred: zone.target_inferred,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
    use chrono::TimeZone;

    /// A fixed mid-July noon UTC anchor: the local month is July in any
    /// timezone the test machine runs in (+-14h stays July 14-16).
    fn now_epoch() -> i64 {
        chrono::Utc
            .with_ymd_and_hms(2026, 7, 15, 12, 0, 0)
            .unwrap()
            .timestamp()
    }

    fn fc_with_daily(precip_in: &[f64]) -> ForecastSnapshot {
        let now = now_epoch();
        let mut fc = ForecastSnapshot::default();
        fc.daily = precip_in
            .iter()
            .enumerate()
            .map(|(i, p)| DailyEntry {
                time_epoch: now + i as i64 * 86_400,
                precip_sum_in: *p,
                precip_probability_max: None, // full weight
                ..Default::default()
            })
            .collect();
        fc
    }

    fn zone(weekly_in: f64, sessions: u32) -> ZoneBalanceInputs {
        ZoneBalanceInputs {
            slug: "front".into(),
            name: "Front".into(),
            weekly_budget_in: weekly_in,
            sessions_per_week: sessions,
            mode_active: true,
            throughput_mm_hr: 10.0,
            max_dur_s: 14_400,
            last_run_epoch: 0,
            applied_trailing_mm: 0.0,
            sessions_done: 0,
            target_inferred: false,
        }
    }

    /// The spacing gate paces sessions at floor(7/sessions) days. Above 7
    /// sessions a week that floors to 0 days, and `days_since_last_run < 0`
    /// is never true, so a zone that already watered TODAY gets planned
    /// again. The config API refuses such a value, but a file already on
    /// disk carrying one must not start double-watering just because it
    /// loads, so the engine clamps too.
    #[test]
    fn sessions_above_seven_cannot_collapse_the_spacing_gate() {
        let mut z = zone(1.0, 14);
        // Watered today already.
        z.last_run_epoch = now_epoch();
        z.sessions_done = 1;
        let b = compute_zone(&z, &globals(0.0, "gauge"), &ForecastSnapshot::default());
        assert_eq!(
            b.today_seconds, 0,
            "a zone that watered today must not be re-planned today"
        );
        assert!(b.today_reason.contains("spaced"), "{}", b.today_reason);
        // The clamp is reported honestly: the reason names 7, the value the
        // engine actually paced on, not the 14 that was configured.
        assert!(
            b.today_reason.contains("7 per week"),
            "the reason names the clamped rate: {}",
            b.today_reason
        );
    }

    fn globals(observed_mm: f64, source: &str) -> BalanceGlobals {
        BalanceGlobals {
            now_epoch: now_epoch(),
            session_rain_defer_in: SESSION_RAIN_DEFER_IN,
            observed_rain_mm: observed_mm,
            observed_rain_source: source.into(),
            bias: BiasModel::identity(),
        }
    }

    /// Every term subtracts from the gross target, and the forward
    /// credit covers ONLY the days between tomorrow and the next
    /// expected session (never today, never the session day, never the
    /// whole week).
    #[test]
    fn terms_subtract_and_credit_window_stops_before_the_session() {
        let fc = fc_with_daily(&[0.5, 0.3, 0.4, 0.4, 0.4, 0.4, 0.4]);
        let mut z = zone(2.0, 2); // interval floor(7/2) = 3 days
        z.applied_trailing_mm = 10.0;
        z.sessions_done = 1;
        z.last_run_epoch = now_epoch() - 86_400; // 1 day ago -> next in 2
        let g = globals(5.0, "gauge");
        let b = compute_zone(&z, &g, &fc);
        // Credit window: next session in 2 days -> daily[1] only (7.62 mm).
        assert!(
            (b.forecast_credit_mm - 0.3 * 25.4).abs() < 1e-9,
            "got credit {}",
            b.forecast_credit_mm
        );
        assert_eq!(b.forecast_credit_source, "bias_forecast");
        // remainder = 50.8 - 5 - 10 - 7.62 = 28.18; one session remains.
        assert!((b.needed_mm - 28.18).abs() < 1e-9, "got {}", b.needed_mm);
        assert_eq!(b.remaining_sessions, 1);
        assert!((b.mm_per_session - 28.18).abs() < 1e-9);
        // Gross delivery: 28.18 mm / 10 mm/hr = 2.818 h = 10145 s (rounded).
        assert_eq!(b.seconds_per_session, 10_145);
        // Spaced today (1 < 3).
        assert_eq!(b.today_seconds, 0);
        assert!(b.today_reason.contains("spaced"), "{}", b.today_reason);
        // expected_rain_mm keeps its historical wire scaling for the
        // legacy automation: weighted 7-day sum x 25.4 x 0.7.
        let wire_week: f64 = (0.5 + 0.3 + 0.4 * 5.0) * 25.4 * EXPECTED_RAIN_WIRE_CAPTURE;
        assert!((b.expected_rain_mm - wire_week).abs() < 1e-9);
    }

    /// A zone with no run on record is due now: no forward credit (the
    /// window is empty) and the session runs today.
    #[test]
    fn no_forward_credit_when_the_session_is_due_now() {
        let fc = fc_with_daily(&[0.0, 0.4, 0.4, 0.0, 0.0, 0.0, 0.0]);
        let z = zone(1.0, 2);
        let g = globals(0.0, "none");
        let b = compute_zone(&z, &g, &fc);
        assert_eq!(b.forecast_credit_mm, 0.0, "no window, no credit");
        assert_eq!(b.forecast_credit_source, "none");
        assert!((b.needed_mm - 25.4).abs() < 1e-9);
        assert_eq!(b.remaining_sessions, 2);
        assert!((b.mm_per_session - 12.7).abs() < 1e-9);
        assert_eq!(b.seconds_per_session, 4572);
        assert_eq!(b.today_seconds, 4572);
        assert!(
            b.today_reason.contains("session 1 of 2"),
            "{}",
            b.today_reason
        );
    }

    /// The bias multiplier scales ONLY the forward credit, and the wire
    /// carries the current month's multiplier + sample count.
    #[test]
    fn bias_scales_the_forward_credit_only() {
        let fc = fc_with_daily(&[0.0, 0.2, 0.0, 0.0, 0.0, 0.0, 0.0]);
        let mut z = zone(2.0, 2);
        z.last_run_epoch = now_epoch() - 86_400; // next session in 2 days
        let mut g = globals(10.0, "gauge");
        // Train July at a 1.5x under-prediction correction (6 wet days).
        let today = crate::timeutil::local_date(now_epoch()).unwrap();
        let obs: Vec<crate::engine::forecast_bias::Observation> = (1..=6)
            .map(|d| {
                crate::engine::forecast_bias::Observation::new(
                    chrono::NaiveDate::from_ymd_opt(2026, 7, d).unwrap(),
                    0.20,
                    0.30,
                )
            })
            .collect();
        g.bias = BiasModel::from_observations(&obs, today, None);
        let b = compute_zone(&z, &g, &fc);
        assert!(
            (b.forecast_credit_mm - 0.2 * 25.4 * 1.5).abs() < 1e-6,
            "credit must be bias-corrected, got {}",
            b.forecast_credit_mm
        );
        assert!((b.bias_multiplier - 1.5).abs() < 1e-9);
        assert_eq!(b.bias_sample_count, 6);
        // The observed term is NEVER bias-scaled.
        assert!((b.observed_rain_mm - 10.0).abs() < 1e-9);
    }

    /// remaining_sessions floors at 1: a week with more completed events
    /// than planned sessions still sizes a sane (single-session) figure.
    #[test]
    fn remaining_sessions_floors_at_one() {
        let fc = fc_with_daily(&[0.0; 7]);
        let mut z = zone(1.0, 2);
        z.sessions_done = 5;
        z.applied_trailing_mm = 5.0;
        z.last_run_epoch = now_epoch() - 4 * 86_400; // eligible
        let g = globals(0.0, "none");
        let b = compute_zone(&z, &g, &fc);
        assert_eq!(b.remaining_sessions, 1);
        assert!((b.mm_per_session - (25.4 - 5.0)).abs() < 1e-9);
        assert!(
            b.today_reason.contains("session 6 of 2"),
            "{}",
            b.today_reason
        );
    }

    /// The spacing gate passes ON the intended session day even though
    /// the previous event ENDED later in the morning than the next
    /// dispatch evaluates (the end-anchored epoch floor would read
    /// three-days-minus-an-hour as 2 days and block every intended day,
    /// stretching a 2/week cadence to every 4 days). Local calendar
    /// days, not elapsed seconds.
    #[test]
    fn spacing_gate_passes_on_the_intended_session_day() {
        let fc = fc_with_daily(&[0.0; 7]);
        let today = crate::timeutil::now_local().date_naive();
        let midnight = crate::timeutil::local_day_bounds_utc(today)
            .expect("local midnight resolves")
            .0
            .timestamp();
        // Dispatch at 05:30 local; the previous session ended at 06:30
        // local three days earlier (one hour LATER in the day).
        let dispatch = midnight + 5 * 3600 + 1800;
        let mut z = zone(1.0, 2); // interval floor(7/2) = 3 days
        z.last_run_epoch = dispatch - 3 * 86_400 + 3_600;
        let mut g = globals(0.0, "none");
        g.now_epoch = dispatch;
        let b = compute_zone(&z, &g, &fc);
        assert!(
            b.today_seconds > 0,
            "the intended day dispatches: {}",
            b.today_reason
        );
        assert!(b.today_reason.contains("session"), "{}", b.today_reason);
        // One calendar day earlier is inside the interval and blocks.
        z.last_run_epoch = dispatch - 2 * 86_400 + 3_600;
        let b = compute_zone(&z, &g, &fc);
        assert_eq!(b.today_seconds, 0);
        assert!(b.today_reason.contains("spaced"), "{}", b.today_reason);
    }

    /// The 24h defer gate still fires (distinct from balance coverage),
    /// and coverage wins over defer when the week is already met.
    #[test]
    fn defer_gate_and_coverage_attribution() {
        let now = now_epoch();
        let mut fc = fc_with_daily(&[0.0; 7]);
        fc.hourly = (0..24)
            .map(|h| HourlyEntry {
                time_epoch: now + h * 3600,
                precip_in: 0.01, // 0.24" over 24h >= the 0.10" defer gate
                ..Default::default()
            })
            .collect();
        let z = zone(1.0, 2);
        let g = globals(0.0, "none");
        let b = compute_zone(&z, &g, &fc);
        assert_eq!(b.today_seconds, 0);
        assert!(b.today_reason.contains("deferred"), "{}", b.today_reason);
        // With the week covered, the reason is coverage, not the defer.
        let g_covered = globals(30.0, "gauge");
        let b = compute_zone(&z, &g_covered, &fc);
        assert!(b.today_reason.contains("covered"), "{}", b.today_reason);
    }

    /// The defer gate weighs forecast rain by probability, the same way
    /// the forward credit and the wire's weekly total already do. 0.24"
    /// of raw model rain at 30% probability is 0.072" of expected water:
    /// under the 0.10" threshold, so the session runs. The raw sum this
    /// gate used to read would have zeroed it.
    #[test]
    fn defer_gate_weighs_forecast_rain_by_probability() {
        let now = now_epoch();
        let mut fc = fc_with_daily(&[0.0; 7]);
        fc.hourly = (0..24)
            .map(|h| HourlyEntry {
                time_epoch: now + h * 3600,
                precip_in: 0.01, // 0.24" raw over 24h
                precip_probability: Some(30),
                ..Default::default()
            })
            .collect();
        let z = zone(1.0, 2);
        let g = globals(0.0, "none");
        let b = compute_zone(&z, &g, &fc);
        assert!(b.today_seconds > 0, "{}", b.today_reason);
        assert!(b.today_reason.contains("session"), "{}", b.today_reason);

        // Same depth at 90% probability is 0.216" expected: over the
        // threshold, so it defers.
        for h in fc.hourly.iter_mut() {
            h.precip_probability = Some(90);
        }
        let b = compute_zone(&z, &g, &fc);
        assert_eq!(b.today_seconds, 0);
        assert!(b.today_reason.contains("deferred"), "{}", b.today_reason);
    }

    /// The configured threshold is what the gate compares against, not a
    /// compile-time constant: the same forecast defers at 0.10" and runs
    /// at 0.50".
    #[test]
    fn defer_threshold_is_the_configured_value() {
        let now = now_epoch();
        let mut fc = fc_with_daily(&[0.0; 7]);
        fc.hourly = (0..24)
            .map(|h| HourlyEntry {
                time_epoch: now + h * 3600,
                precip_in: 0.01, // 0.24" expected (no probability = full weight)
                ..Default::default()
            })
            .collect();
        let z = zone(1.0, 2);

        let mut g = globals(0.0, "none");
        g.session_rain_defer_in = SESSION_RAIN_DEFER_IN; // 0.10"
        let b = compute_zone(&z, &g, &fc);
        assert_eq!(b.today_seconds, 0);
        assert!(b.today_reason.contains("deferred"), "{}", b.today_reason);

        g.session_rain_defer_in = 0.50;
        let b = compute_zone(&z, &g, &fc);
        assert!(b.today_seconds > 0, "{}", b.today_reason);
        assert!(b.today_reason.contains("session"), "{}", b.today_reason);
    }

    /// The cap still clamps the session and flags it.
    #[test]
    fn session_cap_flags_and_clamps() {
        let fc = fc_with_daily(&[0.0; 7]);
        let mut z = zone(2.0, 2);
        z.max_dur_s = 3600;
        let g = globals(0.0, "none");
        let b = compute_zone(&z, &g, &fc);
        // 25.4 mm per session at 10 mm/hr = 9144 s, over the hour cap.
        assert_eq!(b.seconds_per_session, 9144);
        assert!(b.session_capped);
        assert_eq!(b.today_seconds, 3600, "dispatched at the cap");
    }
}
