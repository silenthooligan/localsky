// Tuning-report assembly. The thin I/O layer around engine::tuning: it
// gathers a window of persisted outcomes (runs, probe series, rain
// observations, verdicts) plus live snapshot clamp state and the current
// config, feeds the pure checks, and returns the wire-shaped
// TuningReport. Consumed by GET /api/v1/irrigation/tuning, by the apply
// endpoint's stale-recommendation verification, and by the weekly
// notification scheduler.
//
// Day/TZ discipline: every calendar grouping derives from epoch via
// crate::timeutil (configured timezone), never chrono::Local and never
// verdict_history.date_local.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, OnceLock};

use chrono::{Datelike, NaiveDate};
use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::config::schema::{Config, ZoneConfig};
use crate::config::FileConfigStore;
use crate::engine::tuning::{
    self, BackoutInputs, CapClampInputs, CheckOutcome, DriftInputs, IntervalInputs, RunSegment,
    SkipDayRecord, SoilBinding, ZoneCheckOutcomes,
};
use crate::forecast::ForecastStore;
use crate::ha::IrrigationStore;
use crate::history::types::{TuningReport, TuningScorecard};
use crate::persistence::verdict_history::classify_reason_code;
use crate::persistence::{ForecastObservationsStore, RunsStore, SensorHistoryStore};
use crate::ports::config_store::{ConfigStore, ConfigStoreError};

/// Probe rows fetched per zone per report. 14 days of a 30s-cadence
/// gateway is ~40k rows; the cap keeps a misbehaving chatty source from
/// ballooning the read.
const PROBE_SERIES_LIMIT: usize = 50_000;

/// Everything report generation needs, registered once at boot from
/// main.rs (the set_sim_skip_params OnceLock pattern; the irrigation
/// router's history-gated block only carries the DB connection).
pub struct TuningHandles {
    pub history_conn: Arc<Mutex<Connection>>,
    pub cfg_store: Arc<FileConfigStore>,
    pub irrigation: Arc<IrrigationStore>,
    pub forecast: Arc<ForecastStore>,
    /// (lat, lon) from deployment config, for Kc hemisphere resolution.
    pub location: (f64, f64),
}

static HANDLES: OnceLock<TuningHandles> = OnceLock::new();

/// Register the report-generation handles (called from main at boot,
/// in both live and demo postures). First writer wins.
pub fn set_tuning_handles(h: TuningHandles) {
    let _ = HANDLES.set(h);
}

/// The boot-registered handles, for callers that must generate against a
/// config THEY loaded (the apply endpoint verifies against the exact
/// config it mutates, inside its write lock). None until boot registers.
pub fn handles() -> Option<&'static TuningHandles> {
    HANDLES.get()
}

#[derive(Debug, thiserror::Error)]
pub enum TuningError {
    #[error("tuning report requires the history database")]
    NotConfigured,
    #[error("store: {0}")]
    Store(String),
}

/// Generate the tuning report over the last `days` (clamped to the
/// engine's window bounds). Uses the boot-registered handles; the config
/// is loaded fresh from the store on every call so an Apply is visible
/// to the immediately following regeneration.
pub async fn generate_report(days: u32) -> Result<TuningReport, TuningError> {
    let handles = HANDLES.get().ok_or(TuningError::NotConfigured)?;
    let cfg = match handles.cfg_store.load().await {
        Ok(c) => c,
        // A fresh install with no config yet has no zones; the report is
        // honestly empty rather than an error.
        Err(ConfigStoreError::NotFound) => Config::default(),
        Err(e) => return Err(TuningError::Store(e.to_string())),
    };
    generate_report_with(handles, &cfg, days).await
}

/// Report generation against an explicit config (the apply endpoint
/// verifies against the exact config it is about to mutate).
pub async fn generate_report_with(
    handles: &TuningHandles,
    cfg: &Config,
    days: u32,
) -> Result<TuningReport, TuningError> {
    let days = days.clamp(tuning::MIN_WINDOW_DAYS, tuning::MAX_WINDOW_DAYS);
    let now = chrono::Utc::now().timestamp();
    let from_epoch = now - (days as i64) * 86_400;
    let today = crate::timeutil::now_local().date_naive();

    let runs_store = RunsStore::new(handles.history_conn.clone());
    let sensor_store = SensorHistoryStore::new(handles.history_conn.clone());
    let obs_store = ForecastObservationsStore::new(handles.history_conn.clone());

    let run_rows = runs_store
        .window(from_epoch, now + 1)
        .await
        .map_err(|e| TuningError::Store(e.to_string()))?;

    // Rain-day map over the report window (for dry stretches + backout
    // event hygiene) and the wider scorecard window.
    let obs_from = today - chrono::Duration::days(tuning::SCORECARD_WINDOW_DAYS as i64 + 3);
    let obs_rows = obs_store
        .range(obs_from, today)
        .await
        .map_err(|e| TuningError::Store(e.to_string()))?;
    let obs_by_date: BTreeMap<NaiveDate, (f64, f64)> = obs_rows
        .iter()
        .map(|o| (o.date, (o.predicted_in, o.observed_in)))
        .collect();
    // Wet-day UTC intervals inside the report window.
    let wet_day_intervals: Vec<(i64, i64)> = obs_rows
        .iter()
        .filter(|o| o.observed_in >= tuning::RAIN_DAY_IN)
        .filter_map(|o| crate::timeutil::local_day_bounds_utc(o.date))
        .map(|(a, b)| (a.timestamp(), b.timestamp()))
        .filter(|(a, b)| *b > from_epoch && *a < now)
        .collect();

    // Forward daily ETc building blocks from the live forecast: per-day
    // (et0_mm, heat_multiplier, doy). Kc is per-zone (species) below.
    let fc = handles.forecast.snapshot();
    let (lat, _lon) = handles.location;
    let base_doy = today.ordinal() as u16;
    let day_terms: Vec<(f64, f64, u16)> = fc
        .daily
        .iter()
        .take(7)
        .enumerate()
        .filter_map(|(i, d)| {
            let doy = (base_doy + i as u16 - 1) % 366 + 1;
            let et0 = if d.et0_in > 0.0 {
                Some(d.et0_in * 25.4)
            } else {
                crate::refresher::native_et0_mm(d, lat, doy)
            }?;
            let heat = if d.humidity_pct > 0 {
                crate::engine::et_heat_multiplier(crate::engine::heat_index_f(
                    d.temp_max_f,
                    d.humidity_pct as f64,
                ))
            } else {
                1.0
            };
            Some((et0, heat, doy))
        })
        .collect();

    let snap = handles.irrigation.snapshot();
    let capture = cfg.engine.capture_efficiency.clamp(0.05, 1.0);

    // Scorecard: morning verdict per configured-tz local day over the
    // scorecard window, reason code preferring the stored DecisionTrace.
    let scorecard = build_scorecard(&handles.history_conn, &obs_by_date, today, now).await;

    let mut zones = Vec::with_capacity(cfg.zones.len());
    for (slug, z) in cfg.zones.iter() {
        let runtime_slug = slug.replace('-', "_");
        let zone_rows: Vec<&crate::persistence::RunRow> = run_rows
            .iter()
            .filter(|r| r.zone_slug == runtime_slug || r.zone_slug == *slug)
            .collect();
        let segments: Vec<RunSegment> = zone_rows
            .iter()
            .filter(|r| is_watering_row(&r.source, &r.status))
            .map(|r| RunSegment {
                start_epoch: r.start_epoch,
                end_epoch: r
                    .end_epoch
                    .unwrap_or(r.start_epoch + r.duration_s.unwrap_or(0) as i64),
            })
            .collect();
        let events = tuning::cluster_events(&segments);
        let run_days: HashSet<NaiveDate> = events
            .iter()
            .filter_map(|e| crate::timeutil::local_date(e.start_epoch))
            .collect();
        let run_day_count = run_days.len() as u32;

        // Model-side numbers.
        let (root_mm, _mad) =
            tuning::resolve_root_mad(z.species, z.root_depth_mm, z.mad_pct_override);
        let taw_mm = crate::engine::taw_mm(z.soil_texture, root_mm);
        let mean_daily_etc_mm = zone_mean_daily_etc(z, &day_terms, lat);

        // Live clamp state.
        let math = snap
            .zones
            .iter()
            .find(|s| s.slug == runtime_slug || s.slug == *slug)
            .and_then(|s| s.math.clone());
        let budget = snap
            .water_budgets
            .iter()
            .find(|b| b.zone_slug == runtime_slug || b.zone_slug == *slug);
        let effective_rate =
            crate::engine::effective_precip_rate_mm_hr(z.sprinkler_type, z.precip_rate_mm_hr);
        let cap_inputs = CapClampInputs {
            session_capped: budget.map(|b| b.session_capped).unwrap_or(false),
            deficit_cap_binding: math.as_ref().map(|m| m.cap_binding).unwrap_or(false),
            // ONLY the allocator's per-session seconds: ZoneMath.raw_seconds
            // is a one-shot deficit refill, not a weekly session, and the
            // sessions/budget knobs never feed that chain.
            desired_seconds: budget
                .filter(|b| b.session_capped)
                .map(|b| b.seconds_per_session),
            max_duration_s: math.as_ref().map(|m| m.max_duration_seconds).or(Some(3600)),
            run_days: run_day_count,
            sessions_per_week: budget
                .map(|b| b.sessions_per_week)
                .or(z.sessions_per_week)
                .unwrap_or(1),
            configured_sessions: z.sessions_per_week,
            configured_weekly_budget_in: z.weekly_budget_in,
            weekly_budget_in: budget
                .map(|b| b.weekly_budget_in)
                .or(z.weekly_budget_in)
                .unwrap_or(0.0),
            throughput_mm_hr: math
                .as_ref()
                .map(|m| m.throughput_mm_hr)
                .unwrap_or(effective_rate),
            capture_efficiency: capture,
            heat_multiplier: snap.forecast.heat_multiplier.max(1.0),
        };
        let cap = tuning::check_cap_clamped(slug, &cap_inputs);

        let interval = tuning::check_interval(
            slug,
            &IntervalInputs {
                species: z.species,
                soil_texture: z.soil_texture,
                root_depth_mm_override: z.root_depth_mm,
                mad_pct_override: z.mad_pct_override,
                mean_daily_etc_mm,
            },
        );

        // Probe-dependent checks.
        let binding = classify_binding(z.soil_sensor_id.as_deref());
        let (drift, backout, has_probe_data) = match &binding {
            BindingKind::Source(sid, key) => {
                let readings_raw = sensor_store
                    .series_for_channel(
                        sid.clone(),
                        key.clone(),
                        from_epoch,
                        now + 1,
                        PROBE_SERIES_LIMIT,
                    )
                    .await
                    .map_err(|e| TuningError::Store(e.to_string()))?;
                // Same validity gate as apply_soil_quality: dead-probe zeros
                // and out-of-band values never enter the math.
                let readings: Vec<(i64, f64)> = readings_raw
                    .iter()
                    .filter(|r| r.value > 0.0 && r.value <= 100.0)
                    .map(|r| (r.epoch, r.value))
                    .collect();
                // Dry stretches: window minus irrigation events (padded with
                // the settle window) and wet days.
                let mut busy: Vec<(i64, i64)> = events
                    .iter()
                    .map(|e| (e.start_epoch, e.end_epoch + tuning::BACKOUT_SETTLE_MAX_S))
                    .collect();
                busy.extend(wet_day_intervals.iter().copied());
                let stretches =
                    tuning::dry_stretches(from_epoch, now, &busy, tuning::DRIFT_MIN_STRETCH_S);
                let modeled_slope = mean_daily_etc_mm
                    .filter(|_| taw_mm > 0.0)
                    .map(|etc| etc / taw_mm * 100.0);
                let backout_inputs = BackoutInputs {
                    events: &events,
                    readings: &readings,
                    wet_day_intervals: &wet_day_intervals,
                    taw_mm,
                    capture_efficiency: capture,
                    effective_rate_mm_hr: effective_rate,
                    configured_rate_mm_hr: z.precip_rate_mm_hr,
                };
                // Probe-scale cross-check BEFORE either check may recommend:
                // a pure probe scale error moves the drift ratio
                // (measured/modeled) and the backout ratio (median/effective)
                // by the same factor in the same direction, and then the
                // probe, not the config, is the suspect.
                let (measured, stretch_count) =
                    tuning::measured_drying_slope(&readings, &stretches);
                let drift_ratio = match (measured, modeled_slope) {
                    (Some(m), Some(md))
                        if stretch_count >= tuning::DRIFT_MIN_STRETCHES && md > 0.0 =>
                    {
                        Some(m / md)
                    }
                    _ => None,
                };
                let backout_samples = tuning::backout_rates(&backout_inputs);
                let backout_ratio = if backout_samples.len() >= tuning::BACKOUT_MIN_EVENTS
                    && effective_rate > 0.0
                {
                    tuning::median_of(backout_samples).map(|m| m / effective_rate)
                } else {
                    None
                };
                let (drift, backout) =
                    match tuning::probe_scale_crosscheck(drift_ratio, backout_ratio) {
                        Some(line) => (
                            CheckOutcome::Insufficient(line.clone()),
                            Some(CheckOutcome::Insufficient(line)),
                        ),
                        None => {
                            let drift = tuning::check_probe_drift(
                                slug,
                                &DriftInputs {
                                    readings: &readings,
                                    stretches: &stretches,
                                    modeled_slope_pct_per_day: modeled_slope,
                                    soil_texture: z.soil_texture,
                                    species: z.species,
                                    root_depth_mm_override: z.root_depth_mm,
                                },
                            );
                            // Backout is consulted only when drift did not
                            // flag the zone this report.
                            let backout = if matches!(drift, CheckOutcome::Recommend(_)) {
                                None
                            } else {
                                Some(tuning::check_precip_backout(slug, &backout_inputs))
                            };
                            (drift, backout)
                        }
                    };
                let has_data = !readings.is_empty();
                (Some(drift), backout, has_data)
            }
            _ => (None, None, false),
        };

        let outcomes = ZoneCheckOutcomes {
            cap,
            interval,
            drift,
            backout,
        };
        zones.push(tuning::assemble_zone(
            slug,
            &z.display_name,
            days,
            run_day_count,
            run_day_count > 0 || has_probe_data,
            match binding {
                BindingKind::None => SoilBinding::None,
                BindingKind::Source(..) => SoilBinding::SourceChannel,
                BindingKind::Ha => SoilBinding::HaEntity,
            },
            &outcomes,
        ));
    }

    Ok(TuningReport {
        generated_epoch: now,
        window_days: days,
        zones,
        scorecard,
    })
}

/// Watering evidence per the run-history semantics: run-edge observer
/// rows (source ha_refresher) plus manual API/scheduler rows. Skip
/// markers and the unused intended/running states never count.
fn is_watering_row(source: &str, status: &str) -> bool {
    status == "completed"
        && (source == "ha_refresher" || source == "manual" || source.starts_with("manual:"))
}

/// Mean daily crop ET over the forward forecast window for one zone.
fn zone_mean_daily_etc(z: &ZoneConfig, day_terms: &[(f64, f64, u16)], lat: f64) -> Option<f64> {
    if day_terms.is_empty() {
        return None;
    }
    let sum: f64 = day_terms
        .iter()
        .map(|(et0, heat, doy)| {
            // ALWAYS the latitude-aware Kc so Southern Hemisphere installs
            // read their season, not the calendar's.
            let kc = crate::engine::kc_at_doy_lat(z.species, *doy, lat);
            crate::engine::etc_mm(*et0, kc, *heat)
        })
        .sum();
    let mean = sum / day_terms.len() as f64;
    (mean > 0.0).then_some(mean)
}

enum BindingKind {
    None,
    Source(String, String),
    Ha,
}

fn classify_binding(spec: Option<&str>) -> BindingKind {
    match spec {
        None => BindingKind::None,
        Some(s) => match s.strip_prefix("source:").and_then(|r| r.split_once(':')) {
            Some((sid, key)) => BindingKind::Source(sid.to_string(), key.to_string()),
            // `ha:<entity>` and bare legacy specs read live HA state only;
            // there is no local history for them.
            None => BindingKind::Ha,
        },
    }
}

/// Install-wide forecast-skip scorecard: reduce verdict transitions to
/// the morning verdict per configured-tz day (the accuracy_window
/// grouping), recover the rule id (DecisionTrace.reason_code when
/// present, else classify_reason_code), and hand engine::tuning the
/// window-aware confirmation.
async fn build_scorecard(
    conn: &Arc<Mutex<Connection>>,
    obs_by_date: &BTreeMap<NaiveDate, (f64, f64)>,
    today: NaiveDate,
    now: i64,
) -> TuningScorecard {
    let from = now - (tuning::SCORECARD_WINDOW_DAYS as i64) * 86_400;
    let decisions = match crate::history::db::decisions_window(conn.clone(), from, now + 1).await {
        Ok(w) => w.decisions,
        Err(e) => {
            tracing::warn!(error = %e, "tuning scorecard: decisions read failed");
            Vec::new()
        }
    };
    let mut by_day: BTreeMap<NaiveDate, (i64, SkipDayRecord)> = BTreeMap::new();
    for d in decisions {
        let Some(date) = crate::timeutil::local_date(d.epoch) else {
            continue;
        };
        let code = d
            .trace
            .as_ref()
            .map(|t| t.reason_code.clone())
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| classify_reason_code(&d.verdict, &d.reason));
        let rec = SkipDayRecord {
            date,
            verdict: d.verdict,
            reason_code: code,
        };
        by_day
            .entry(date)
            .and_modify(|cur| {
                if d.epoch < cur.0 {
                    *cur = (d.epoch, rec.clone());
                }
            })
            .or_insert((d.epoch, rec));
    }
    let days: Vec<SkipDayRecord> = by_day.into_values().map(|(_, r)| r).collect();
    // Yesterday is the most recent day whose observed total is final;
    // today's is a partial accumulation until local midnight.
    let last_complete_day = today.pred_opt().unwrap_or(today);
    tuning::score_forecast_skips(
        &days,
        obs_by_date,
        last_complete_day,
        tuning::SCORECARD_WINDOW_DAYS,
    )
}
