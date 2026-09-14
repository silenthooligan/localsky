// What a pass needs from the database, gathered before the pure assembly
// runs: the week's balance (observed rain, run history, bias), the soil
// replay evidence, the soil probe readings. Async, store-backed; the
// shell calls these once per tick and hands the results to `assembly`.

use super::*;
use crate::assembly::*;
use crate::engine::skip_rules::ZoneSoil;
use crate::forecast::snapshot::ForecastSnapshot;
use crate::forecast::ForecastStore;
use crate::tempest::state::TempestStore;
use chrono::Utc;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Trailing window the water balance settles against: rolling 7 local
/// days ending now (day-keyed for the rain ledger, epoch-keyed for the
/// runs evidence). No calendar-week anchor.
pub const BALANCE_WINDOW_DAYS: i64 = 7;

/// One zone's run-history evidence for the balance, pre-computed once
/// per tick (never a per-zone SQLite query inside the zone loop).
#[derive(Debug, Clone, Copy, Default)]
pub struct ZoneRunEvidence {
    /// Union valve-open seconds across clustered completed watering
    /// events, clamped to the trailing window.
    pub applied_open_s: i64,
    /// Clustered watering events inside the trailing window.
    pub sessions_done: u32,
    /// End epoch of the latest completed watering event (0 = none).
    pub last_run_epoch: i64,
    pub last_session_open_s: Option<i64>,
}

/// Cross-zone balance inputs computed once per refresher tick from the
/// stores (runs history, rain ledger, bias model) and passed into the
/// sync snapshot build. `None` (no history DB and no forecast archive)
/// degrades the balance to target-only sizing, exactly the honest
/// fallback for an install with no evidence.
#[derive(Debug, Clone)]
pub struct BalanceTick {
    /// Observed rain over the trailing window (mm), ladder-resolved.
    /// The RAW sum: this is what rides the wire as `observed_rain_mm`.
    pub observed_rain_mm: f64,
    /// "gauge" | "radar" | "model_archive" | "none".
    pub observed_rain_source: String,
    /// The same window as one value per covered day (mm), from the SAME
    /// ladder rung as the sum above. Feeds the per-day rain-credit cap:
    /// the balance clips each day at the zone's root-zone capacity, and
    /// only a day series can say whether the week's rain fell in one
    /// storm or six drizzles. Sums to `observed_rain_mm` (up to float
    /// rounding); empty when the rung is "none" or no store is mounted.
    pub observed_rain_days_mm: Vec<f64>,
    /// Forecast bias model (identity when under-trained or absent).
    pub bias: crate::engine::BiasModel,
    /// Per-zone run evidence, keyed by underscore-normalized slug.
    pub per_zone: HashMap<String, ZoneRunEvidence>,
    /// Day-granular evidence for the soil model's replay window, gathered
    /// on the same cached cadence as everything above so the sync
    /// snapshot build never touches SQLite.
    pub soil: SoilTickEvidence,
    /// The runs-store window read ERRORED this tick (distinct from an
    /// empty result or no store mounted). The replay would then see none
    /// of the irrigation the system itself dispatched (applied=0 on
    /// every day), so a soil-governed zone that watered yesterday could
    /// reconstruct an inflated depletion and re-dispatch a full refill.
    /// The soil pass treats a degraded tick as evidence-unavailable:
    /// buckets are not published and the governed swap stands down until
    /// a clean read.
    pub runs_degraded: bool,
}

/// Per-tick evidence for the soil model's trailing replay window,
/// gathered beside the weekly balance's figures. One entry per trailing
/// configured-tz local day (`engine::soil_schedule::RECON_WINDOW_DAYS`,
/// oldest first, today last). Every column degrades independently, the
/// BalanceTick contract: an uncovered rain day is 0.0 (the replay's
/// [0, TAW] clamp bounds the cold-start anchor either way), a day with
/// no ET0 evidence resolves through the ladder's per-zone fallback rung
/// at plan time, and an uncovered applied day is zero valve seconds.
#[derive(Debug, Clone, Default)]
pub struct SoilTickEvidence {
    /// No measured or archived rain coverage for these dates. A missing total
    /// cannot make a soil replay converge to an invented drought.
    pub unknown_rain_dates: Vec<chrono::NaiveDate>,
    /// Exact valve intervals for progressing the rolling week in a forecast
    /// scenario. These remain measured history; scenario events stay in a clone.
    pub run_segments: HashMap<String, Vec<crate::history::rollup::RunSegment>>,
    /// The window's local days, oldest first, today last. Empty when the
    /// configured timezone cannot produce day bounds (never in practice).
    pub dates: Vec<chrono::NaiveDate>,
    /// Gross rain (mm) per day, aligned to `dates`. Resolved through the
    /// SAME coverage-precedence ladder as the weekly day series
    /// (`resolve_observed_rain_days`), extended to keep dates: measured
    /// rows (gauge/radar; legacy counts on station installs) win
    /// outright even at 0.00, else ONE whole model-side series (provider
    /// archive vs model-quality rows, by sum) supplies the days it
    /// covers. Today's model total stays on the forward side, exactly as
    /// the weekly rungs hold it.
    pub rain_mm: Vec<f64>,
    /// Dated ET0 ledger rows (mm) inside the window: the replay ladder's
    /// first rung, day-MAX with provenance, fed by the self-emit.
    pub et0_ledger: Vec<(chrono::NaiveDate, f64)>,
    /// Dated provider-archive ET0 (mm) for PAST days in the window: the
    /// ladder's second rung. Today never appears here; see below.
    pub et0_archive: Vec<(chrono::NaiveDate, f64)>,
    /// Today's PARTIAL ET0 charge (mm): the day's spent portion, so the
    /// intra-day replay does not charge a full day's evaporation at
    /// dawn. The hourly curve's spent figure when the provider carries
    /// one, else the resolved full-day figure scaled by the elapsed
    /// local-day fraction. `None` when nothing resolves; the plan then
    /// charges today from the fallback rung (a bounded overcharge on an
    /// input-starved install, in the direction of watering sooner).
    pub today_partial_et0_mm: Option<f64>,
    /// Union valve-open seconds per day per zone (underscore-normalized
    /// slug), aligned to `dates`: `history::rollup::applied_per_day`
    /// over the same clustered watering evidence the weekly balance
    /// credits, so a midnight-straddling run splits at the boundary and
    /// duplicate manual + observer rows count once.
    pub applied_valve_s: HashMap<String, Vec<i64>>,
    /// Durable decisions from completed scheduled mornings, keyed by zone.
    /// Missing history cannot be inferred from waterless replay days.
    pub morning_decisions: HashMap<String, Vec<crate::engine::soil_decisions::PastMorning>>,
}

/// One day of the replay window, with its own date attached.
///
/// The evidence used to travel as several Vecs read by a shared index.
/// A short vector did not fail: `.get(i).unwrap_or(0.0)` charged that
/// day zero rain, which under-credits rain and over-waters, and nothing
/// said so. Pairing by date means a missing day is visibly missing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoilDayRow {
    pub date: chrono::NaiveDate,
    pub gross_rain_mm: f64,
    /// True for the window's last day, which charges a partial ET0.
    pub is_today: bool,
}

impl SoilTickEvidence {
    /// The window's days paired with their own rain, by position but
    /// checked once rather than per lookup.
    ///
    /// When the parallel vectors disagree in length that is a builder
    /// bug, and it is loud here instead of silently becoming a dry day
    /// halfway through a replay.
    pub fn day_rows(&self) -> Vec<SoilDayRow> {
        if !self.rain_mm.is_empty() && self.rain_mm.len() != self.dates.len() {
            tracing::warn!(
                dates = self.dates.len(),
                rain = self.rain_mm.len(),
                "soil evidence day count and rain count disagree; days past the                  shorter series are charged no rain"
            );
        }
        let last = self.dates.len().saturating_sub(1);
        self.dates
            .iter()
            .enumerate()
            .map(|(i, date)| SoilDayRow {
                date: *date,
                gross_rain_mm: self.rain_mm.get(i).copied().unwrap_or(0.0),
                is_today: i == last,
            })
            .collect()
    }
}

/// Resolve the balance's observed-rain term from the per-source ledger
/// sums plus the forecast provider's past-day archive. Precedence is by
/// COVERAGE, never by value: when any measured rows (gauge/radar; legacy
/// rows count as gauge only on station installs) exist in the window,
/// the measured rung wins outright, even at 0.00 in; a yard that
/// measured a dry week is ground truth a wetter regional model must not
/// override. The model side (the max() of the provider archive and any
/// model-quality legacy rows, so neither hides rain the other saw)
/// supplies the term only when measured coverage is entirely absent.
/// Returns (mm, source rung).
pub(crate) fn resolve_observed_rain(
    win: &crate::persistence::ObservedRainWindow,
    station_present: bool,
    archive_past_in: f64,
) -> (f64, String) {
    let legacy_as_gauge = station_present;
    let measured_days =
        win.gauge_days + win.radar_days + if legacy_as_gauge { win.legacy_days } else { 0 };
    if measured_days > 0 {
        let gauge_in = win.gauge_in + if legacy_as_gauge { win.legacy_in } else { 0.0 };
        let radar_in = win.radar_in;
        let source = if radar_in > gauge_in {
            "radar"
        } else {
            "gauge"
        };
        (
            crate::units::in_to_mm(gauge_in + radar_in),
            source.to_string(),
        )
    } else {
        let model_rows_in = win.model_in + if legacy_as_gauge { 0.0 } else { win.legacy_in };
        let model_side_in = archive_past_in.max(model_rows_in);
        if model_side_in > 0.0 {
            (
                crate::units::in_to_mm(model_side_in),
                "model_archive".to_string(),
            )
        } else {
            (0.0, "none".to_string())
        }
    }
}

/// Day-granular companion to `resolve_observed_rain`: the SAME ladder
/// precedence, resolved to one mm value per covered day instead of the
/// window sum. Measured coverage (gauge/radar rows; legacy rows count as
/// measured only on station installs) wins outright, even at 0.00 in.
/// The model side chooses one WHOLE series, never a day-by-day mix
/// (which could exceed the sum rung's max()): the archive when its total
/// is at least the model-quality rows' total, else the rows, mirroring
/// `archive_past_in.max(model_rows_in)` so the series sums to the same
/// pre-cap figure the sum rung resolves. `archive_days_in` is the same
/// slice of `past_daily` the sum rung reads (the last window-minus-one
/// entries; today's model total belongs to the forward side).
pub(crate) fn resolve_observed_rain_days(
    days: &[crate::persistence::ObservedRainDay],
    station_present: bool,
    archive_days_in: &[f64],
) -> Vec<f64> {
    let legacy_as_gauge = station_present;
    let measured: Vec<f64> = days
        .iter()
        .filter(|d| {
            matches!(d.source.as_str(), "gauge" | "radar")
                || (legacy_as_gauge && d.source == "legacy")
        })
        .map(|d| crate::units::in_to_mm(d.observed_in))
        .collect();
    if !measured.is_empty() {
        return measured;
    }
    // Model-quality rows: everything that is not measured coverage
    // ('model', unknown tags, and legacy rows on station-less installs),
    // the same bucketing `observed_rain_window_by_source` applies.
    let model_rows_in: Vec<f64> = days
        .iter()
        .filter(|d| {
            !matches!(d.source.as_str(), "gauge" | "radar")
                && (d.source != "legacy" || !legacy_as_gauge)
        })
        .map(|d| d.observed_in)
        .collect();
    let archive_sum: f64 = archive_days_in.iter().sum();
    let rows_sum: f64 = model_rows_in.iter().sum();
    let chosen = if archive_sum >= rows_sum {
        archive_days_in
    } else {
        model_rows_in.as_slice()
    };
    chosen.iter().map(|d| crate::units::in_to_mm(*d)).collect()
}

/// `resolve_observed_rain_days` extended to KEEP DATES, for the soil
/// replay's day-aligned window (the shipped resolver strips the dates
/// its rows carry). Same coverage precedence: measured rows win
/// outright, even at 0.00 in; else ONE whole model-side series (the
/// dated provider archive vs the model-quality rows, by sum) supplies
/// the days it covers. `archive_days_in` carries real dates resolved
/// from `past_daily` epochs and must already exclude today (today's
/// model total belongs to the forward side). Returns (date, gross mm)
/// pairs; days neither series covers are simply absent, and the caller
/// treats them as dry.
pub(crate) fn resolve_observed_rain_days_dated(
    days: &[crate::persistence::ObservedRainDay],
    station_present: bool,
    archive_days_in: &[(chrono::NaiveDate, f64)],
) -> Vec<(chrono::NaiveDate, f64)> {
    let legacy_as_gauge = station_present;
    let measured: Vec<(chrono::NaiveDate, f64)> = days
        .iter()
        .filter(|d| {
            matches!(d.source.as_str(), "gauge" | "radar")
                || (legacy_as_gauge && d.source == "legacy")
        })
        .map(|d| (d.date, crate::units::in_to_mm(d.observed_in)))
        .collect();
    if !measured.is_empty() {
        return measured;
    }
    let model_rows_in: Vec<(chrono::NaiveDate, f64)> = days
        .iter()
        .filter(|d| {
            !matches!(d.source.as_str(), "gauge" | "radar")
                && (d.source != "legacy" || !legacy_as_gauge)
        })
        .map(|d| (d.date, d.observed_in))
        .collect();
    let archive_sum: f64 = archive_days_in.iter().map(|(_, v)| v).sum();
    let rows_sum: f64 = model_rows_in.iter().map(|(_, v)| v).sum();
    let chosen = if archive_sum >= rows_sum {
        archive_days_in
    } else {
        model_rows_in.as_slice()
    };
    chosen
        .iter()
        .map(|(d, v)| (*d, crate::units::in_to_mm(*v)))
        .collect()
}

/// How long a computed BalanceTick may serve before the stores are
/// re-read (a coarse timer; a run edge also invalidates it). Keeps the
/// runs/ledger/bias SQLite reads off the 10s refresh path.
pub(crate) const BALANCE_CACHE_MAX_AGE_S: i64 = 60;

/// Gather the balance's store-backed inputs once per (cached) tick:
/// per-source ledger rain sums resolved through the observed-rain
/// ladder, the bias model, and the per-zone clustered run evidence.
/// Every read degrades independently (no history DB = no applied term
/// and identity bias; the archive rung still works from the forecast).
pub(crate) async fn compute_balance_tick(
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
    runs_store: Option<&crate::persistence::RunsStore>,
    obs_store: Option<&crate::persistence::ForecastObservationsStore>,
) -> BalanceTick {
    let now = chrono::Utc::now().timestamp();
    // ONE ledger read serves both rain figures. The day rows are
    // fetched once and the per-source window sums are reconstructed
    // from the SAME rows in memory (`ObservedRainWindow::from_days`
    // groups by source exactly as the SQL GROUP BY did, so the raw wire
    // sum is unchanged), which makes the sum rung and the day series
    // describe identical rows under one window anchor by construction.
    // Two racing reads used to let the fire-and-forget day-max upsert
    // land between them: the day series could then outgrow the raw sum
    // (crediting more rain than the wire reports), and a failed day
    // read alone silently disabled the cap for a cache period. A failed
    // read now degrades BOTH figures together to the archive rung.
    let day_rows = match obs_store {
        Some(s) => s
            .observed_rain_window_days(BALANCE_WINDOW_DAYS)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "balance ledger read failed");
                Vec::new()
            }),
        None => Vec::new(),
    };
    let win = crate::persistence::ObservedRainWindow::from_days(&day_rows);
    let fc = forecast_store.snapshot();
    // Past days only (today's model total belongs to the forward side).
    let archive_past = fc.past_n_day_precip_in((BALANCE_WINDOW_DAYS - 1) as usize);
    let archive_past_in = archive_past.unwrap_or(0.0);
    // The archive's per-day view: the same last window-minus-one entries
    // `past_n_day_precip_in` sums (past_daily is stored earliest first).
    let archive_days_in: Vec<f64> = if archive_past.is_some() {
        let len = fc.past_daily.len();
        let start = len.saturating_sub((BALANCE_WINDOW_DAYS - 1) as usize);
        fc.past_daily[start..]
            .iter()
            .filter_map(|d| d.precip_sum_in)
            .collect()
    } else {
        Vec::new()
    };
    let t = tempest_store.snapshot();
    let station_present = t.has_live_station || !t.station_serial.is_empty();
    let (observed_rain_mm, observed_rain_source) =
        resolve_observed_rain(&win, station_present, archive_past_in);
    let observed_rain_days_mm =
        resolve_observed_rain_days(&day_rows, station_present, &archive_days_in);
    let bias = match obs_store {
        Some(s) => match s
            .recent(crate::engine::forecast_bias::DEFAULT_WINDOW_DAYS)
            .await
        {
            Ok(rows) => crate::engine::BiasModel::from_observations(
                &rows,
                crate::timeutil::now_local().date_naive(),
                None,
            ),
            Err(e) => {
                tracing::debug!(error = %e, "balance bias read failed");
                crate::engine::BiasModel::identity()
            }
        },
        None => crate::engine::BiasModel::identity(),
    };
    // ONE runs read serves the weekly evidence and the soil replay's
    // per-day buckets: the fetch covers the wider soil window (one extra
    // day of margin so an event straddling the window start is fetched
    // and then truncated, never missed), and the weekly reduction below
    // truncates its windowed sums to the 7-day window exactly as before.
    // The wider fetch does move one weekly-surface value, declared in
    // the 1.27.0 note: `last_run_epoch` reduces over ALL fetched rows,
    // so a zone whose newest run is 8-15 days old now reports that run's
    // end instead of 0 (the truthful figure; spacing and sizing are
    // unaffected because min_interval_days is at most 7).
    //
    // A read ERROR is not an empty result: it marks the tick degraded so
    // the soil pass cannot replay applied=0 for water the system itself
    // dispatched (see `BalanceTick::runs_degraded`).
    let (run_rows, runs_degraded) = match runs_store {
        Some(rs) => {
            let fetch_days =
                BALANCE_WINDOW_DAYS.max(crate::engine::soil_schedule::RECON_WINDOW_DAYS) + 1;
            match rs.window(now - fetch_days * 86400, now + 1).await {
                Ok(rows) => (rows, false),
                Err(e) => {
                    tracing::warn!(error = %e, "balance runs window read failed; soil tick degraded");
                    (Vec::new(), true)
                }
            }
        }
        None => (Vec::new(), false),
    };
    let per_zone = build_zone_run_evidence(&run_rows, now - BALANCE_WINDOW_DAYS * 86400, now);
    let mut soil =
        compute_soil_tick_evidence(now, &fc, obs_store, &day_rows, station_present, &run_rows)
            .await;
    if let (Some(runs), Some(first), Some(today)) = (
        runs_store,
        soil.dates.first().copied(),
        crate::timeutil::deployment_calendar().local_date(now),
    ) {
        soil.morning_decisions = runs.soil_decisions().completed_window(first, today).await
            .unwrap_or_else(|error| {
                tracing::warn!(error = %error, "soil decision history unavailable; forecast defer allowance unspent");
                HashMap::new()
            });
    }
    BalanceTick {
        observed_rain_mm,
        observed_rain_source,
        observed_rain_days_mm,
        bias,
        per_zone,
        soil,
        runs_degraded,
    }
}

/// Gather the soil replay's day-aligned evidence window. The rain rows
/// come from their own wider ledger read (the weekly figures keep their
/// single-read invariant untouched); the ET0 ladder's ledger rung and
/// the runs buckets ride the same stores the balance already reads.
pub(crate) async fn compute_soil_tick_evidence(
    now: i64,
    fc: &ForecastSnapshot,
    obs_store: Option<&crate::persistence::ForecastObservationsStore>,
    weekly_day_rows: &[crate::persistence::ObservedRainDay],
    station_present: bool,
    run_rows: &[crate::persistence::RunRow],
) -> SoilTickEvidence {
    use crate::engine::soil_schedule::RECON_WINDOW_DAYS;
    let today = crate::timeutil::now_local().date_naive();
    // The window's local days with their UTC bounds, oldest first.
    let mut dates: Vec<chrono::NaiveDate> = Vec::with_capacity(RECON_WINDOW_DAYS as usize);
    let mut frames: Vec<(i64, i64)> = Vec::with_capacity(RECON_WINDOW_DAYS as usize);
    for back in (0..RECON_WINDOW_DAYS).rev() {
        let date = today - chrono::Duration::days(back);
        if let Some((start, end)) = crate::timeutil::local_day_bounds_utc(date) {
            dates.push(date);
            frames.push((start.timestamp(), end.timestamp()));
        }
    }
    // Rain: the ledger's dated rows over the soil window. The weekly
    // 7-day rows are reused when the wider read fails or no store is
    // mounted, so the soil series can never contradict rain the weekly
    // balance credits on the shared days.
    let soil_day_rows = match obs_store {
        Some(s) => s
            .observed_rain_window_days(RECON_WINDOW_DAYS)
            .await
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "soil ledger read failed; reusing the weekly rows");
                weekly_day_rows.to_vec()
            }),
        None => weekly_day_rows.to_vec(),
    };
    // Dated provider archive, past days only (today's model total belongs
    // to the forward side, the same rule every rain rung applies).
    let archive_rain_in: Vec<(chrono::NaiveDate, f64)> = fc
        .past_daily
        .iter()
        .filter_map(|d| {
            let date = crate::timeutil::deployment_calendar()
                .day_of(d.day_marker)?
                .naive();
            (date < today)
                .then(|| d.precip_sum_in.map(|amount| (date, amount)))
                .flatten()
        })
        .collect();
    let rain_by_date: HashMap<chrono::NaiveDate, f64> =
        resolve_observed_rain_days_dated(&soil_day_rows, station_present, &archive_rain_in)
            .into_iter()
            .collect();
    let rain_mm: Vec<f64> = dates
        .iter()
        .map(|d| rain_by_date.get(d).copied().unwrap_or(0.0))
        .collect();
    // The ET0 ladder's evidence rungs: dated ledger rows (the self-emit
    // plus any station/provider writer), then the dated provider archive
    // for past days.
    let et0_ledger: Vec<(chrono::NaiveDate, f64)> = match obs_store {
        Some(s) => s
            .et0_window_days(RECON_WINDOW_DAYS)
            .await
            .map(|rows| rows.into_iter().map(|r| (r.date, r.et0_mm)).collect())
            .unwrap_or_else(|e| {
                tracing::debug!(error = %e, "et0 ledger read failed");
                Vec::new()
            }),
        None => Vec::new(),
    };
    let et0_archive: Vec<(chrono::NaiveDate, f64)> = fc
        .past_daily
        .iter()
        .filter_map(|d| {
            let date = crate::timeutil::deployment_calendar()
                .day_of(d.day_marker)?
                .naive();
            (date < today)
                .then(|| d.reference_et0_mm().map(|et0| (date, et0)))
                .flatten()
        })
        .collect();
    // Today's PARTIAL charge: the provider's spent-so-far figure when the
    // hourly curve exists, else the resolved full-day figure (ledger row,
    // else today's forecast daily ET0) scaled by the elapsed local-day
    // fraction. Floored at a hair above zero so the first tick after
    // midnight reads as a ~zero charge instead of falling through to the
    // fallback rung's full-day mean.
    let today_partial_et0_mm = {
        let spent = fc.et0_spent_with_evidence(now, crate::timeutil::deployment_calendar());
        if let Some(spent) = spent {
            Some(spent)
        } else {
            let full = et0_ledger
                .iter()
                .find(|(d, _)| *d == today)
                .map(|(_, v)| *v)
                .or_else(|| fc.daily.first().and_then(|d| d.reference_et0_mm()));
            let elapsed_fraction = frames
                .last()
                .map(|(start, end)| {
                    ((now - start) as f64 / (end - start).max(1) as f64).clamp(0.0, 1.0)
                })
                .unwrap_or(0.0);
            full.map(|v| (v * elapsed_fraction).max(0.0))
        }
    };
    // Per-day applied buckets from the SAME clustered watering evidence
    // the weekly balance credits (one filter, one union).
    let mut segments_by_zone: HashMap<String, Vec<crate::engine::tuning::RunSegment>> =
        HashMap::new();
    for r in run_rows {
        if !crate::engine::tuning::is_watering_evidence(
            &r.source,
            &r.status,
            r.skip_reason.as_deref(),
        ) {
            continue;
        }
        let end = r
            .end_epoch
            .unwrap_or(r.start_epoch + r.duration_s.unwrap_or(0) as i64);
        segments_by_zone
            .entry(r.zone_slug.replace('-', "_"))
            .or_default()
            .push(crate::engine::tuning::RunSegment {
                session_id: r.session_id.clone(),
                start_epoch: r.start_epoch,
                end_epoch: end,
            });
    }
    let applied_valve_s: HashMap<String, Vec<i64>> = segments_by_zone
        .iter()
        .map(|(slug, segs)| {
            let days = crate::history::rollup::applied_per_day(segs, &frames);
            (
                slug.clone(),
                days.into_iter().map(|d| d.valve_open_s).collect(),
            )
        })
        .collect();
    SoilTickEvidence {
        unknown_rain_dates: dates
            .iter()
            .filter(|date| !rain_by_date.contains_key(date))
            .copied()
            .collect(),
        run_segments: segments_by_zone,
        dates,
        rain_mm,
        et0_ledger,
        et0_archive,
        today_partial_et0_mm,
        applied_valve_s,
        morning_decisions: HashMap::new(),
    }
}

/// Group completed watering evidence per zone: filter rows through the
/// shared watering-evidence rule, truncate to the trailing window,
/// cluster (union semantics de-duplicate manual + observer rows), and
/// reduce to the per-zone evidence the balance reads.
pub(crate) fn build_zone_run_evidence(
    rows: &[crate::persistence::RunRow],
    window_start: i64,
    window_end: i64,
) -> HashMap<String, ZoneRunEvidence> {
    use crate::engine::tuning::{applied_in_window, is_watering_evidence, RunSegment};
    let mut segments_by_zone: HashMap<String, Vec<RunSegment>> = HashMap::new();
    let mut last_end_by_zone: HashMap<String, i64> = HashMap::new();
    for r in rows {
        if !is_watering_evidence(&r.source, &r.status, r.skip_reason.as_deref()) {
            continue;
        }
        let slug = r.zone_slug.replace('-', "_");
        let end = r
            .end_epoch
            .unwrap_or(r.start_epoch + r.duration_s.unwrap_or(0) as i64);
        segments_by_zone
            .entry(slug.clone())
            .or_default()
            .push(RunSegment {
                session_id: r.session_id.clone(),
                start_epoch: r.start_epoch,
                end_epoch: end,
            });
        let e = last_end_by_zone.entry(slug).or_insert(0);
        *e = (*e).max(end);
    }
    segments_by_zone
        .into_iter()
        .map(|(slug, segs)| {
            let applied = applied_in_window(&segs, window_start, window_end);
            let last = last_end_by_zone.get(&slug).copied().unwrap_or(0);
            (
                slug,
                ZoneRunEvidence {
                    applied_open_s: applied.valve_open_s,
                    sessions_done: applied.events,
                    last_run_epoch: last,
                    last_session_open_s: crate::history::rollup::cluster_events(&segs)
                        .into_iter()
                        .max_by_key(|e| e.end_epoch)
                        .map(|e| e.valve_open_s),
                },
            )
        })
        .collect()
}

/// Resolve a zone's assigned soil sensor to a live %. Supports three
/// address forms:
///   - `ha:sensor.x`        → HA entity state
///   - `source:<id>:<key>`  → latest sensor_history reading for that
///                            source channel (Ecowitt etc.)
///   - bare `sensor.x`      → HA entity (legacy / back-compat)
/// `None` when unassigned or the reading is unavailable.
pub(crate) async fn resolve_soil_pct(
    spec: Option<&str>,
    map: &HashMap<String, Value>,
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Option<f64> {
    let spec = spec?;
    if let Some(entity) = spec.strip_prefix("ha:") {
        return state_f64(map, entity);
    }
    if let Some(rest) = spec.strip_prefix("source:") {
        let (sid, key) = rest.split_once(':')?;
        let h = history?;
        return h
            .last_value(sid.to_string(), key.to_string())
            .await
            .ok()
            .flatten()
            .map(|r| r.value);
    }
    // Bare string: treat as an HA entity id (legacy configs).
    state_f64(map, spec)
}

/// Build the engine's per-zone soil list from the boot-resolved zone
/// config, pulling each zone's live reading via `resolve_soil_pct`.
pub(crate) async fn resolve_soil_zones(
    cfg: &[ZoneSoilCfg],
    map: &HashMap<String, Value>,
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Vec<ZoneSoil> {
    let now = Utc::now().timestamp();
    let mut out = Vec::with_capacity(cfg.len());
    for z in cfg {
        let raw = resolve_soil_pct(z.soil_sensor_id.as_deref(), map, history).await;
        let mut pct = apply_soil_quality(raw);
        // A stale `source:` reading (no fresh sample within the fault
        // window) must fail safe to offline so it can never drive the dry-soil
        // veto or a saturation skip on data the gateway stopped refreshing. The
        // engine's protected probe-data gate then holds the zone, and
        // detect_soil_probe_faults reports it. HA entities have no local
        // history to judge recency, so they are left to apply_soil_quality.
        if pct.is_some() && soil_reading_stale(z.soil_sensor_id.as_deref(), history, now).await {
            pct = None;
        }
        out.push(ZoneSoil {
            slug: z.slug.clone(),
            name: z.name.clone(),
            pct,
            probe_configured: z
                .soil_sensor_id
                .as_ref()
                .is_some_and(|id| !id.trim().is_empty()),
            saturation_pct: z.saturation_pct,
            target_min_pct: z.target_min_pct,
            governed_by_soil_model: false,
            planning_forecast_unavailable: false,
            sprinkler_type: z.sprinkler_type,
        });
    }
    out
}

/// True when a `source:` soil channel's most recent sample is older than the
/// fault window, so the cached value is stale and must not drive a watering
/// decision. `ha:` entities and missing/non-source specs are never considered
/// stale here (no local history to judge recency).
pub(crate) async fn soil_reading_stale(
    spec: Option<&str>,
    history: Option<&crate::persistence::SensorHistoryStore>,
    now: i64,
) -> bool {
    let Some(rest) = spec.and_then(|s| s.strip_prefix("source:")) else {
        return false;
    };
    let Some((sid, key)) = rest.split_once(':') else {
        return false;
    };
    let Some(h) = history else {
        return false;
    };
    match h.last_value(sid.to_string(), key.to_string()).await {
        Ok(Some(r)) => now.saturating_sub(r.epoch) >= SOIL_PROBE_FAULT_AFTER_S,
        _ => false,
    }
}

/// How long a configured soil channel may go without a valid (> 0)
/// reading before it is reported as faulted. One missed gateway poll is
/// noise; a full day of zeros is dead hardware.
pub(crate) const SOIL_PROBE_FAULT_AFTER_S: i64 = 24 * 3600;

/// Upper physical bound for a soil-moisture percentage. A reading above this
/// is not super-saturated soil, it is a garbage / over-range frame, so
/// `apply_soil_quality` nulls it to None and it feeds the same disconnected
/// fault path as a 0% / negative reading. Soil moisture is a percentage and
/// can never physically exceed 100%.
pub(crate) const SOIL_PCT_PHYSICAL_MAX: f64 = 100.0;

/// Detect configured-but-dead soil probes. A zone is faulted when it has
/// a soil sensor configured, its resolved pct is None (missing or <= 0.0,
/// see `apply_soil_quality`), AND sensor_history confirms persistence:
/// the channel's last reading above 0.0 is older than 24h, or it never
/// produced one. A dead WH51 keeps writing 0.0 rows, so the last
/// above-zero epoch is the signal. Only `source:` channels are checked;
/// an `ha:` entity has no local history to distinguish a flatline from a
/// transient blip, so it is never flagged here.
pub(crate) async fn detect_soil_probe_faults(
    cfg: &[ZoneSoilCfg],
    resolved: &[ZoneSoil],
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Vec<crate::model::SoilProbeFault> {
    let Some(h) = history else {
        return Vec::new();
    };
    let now = Utc::now().timestamp();
    let mut out = Vec::new();
    for z in cfg {
        let Some(spec) = z.soil_sensor_id.as_deref() else {
            continue;
        };
        // Healthy: the resolved reading is usable.
        if resolved
            .iter()
            .find(|r| r.slug == z.slug)
            .and_then(|r| r.pct)
            .is_some()
        {
            continue;
        }
        let Some((sid, key)) = spec
            .strip_prefix("source:")
            .and_then(|rest| rest.split_once(':'))
        else {
            continue;
        };
        let since_epoch = h
            .last_value_above(sid.to_string(), key.to_string(), 0.0)
            .await
            .ok()
            .flatten()
            .map(|r| r.epoch);
        let stale = match since_epoch {
            Some(e) => now.saturating_sub(e) >= SOIL_PROBE_FAULT_AFTER_S,
            None => true,
        };
        // TODO(G1 flatline): a probe stuck at a plausible constant (e.g. 45%)
        // keeps refreshing, so stale=false and it slips through here. Detecting
        // it needs a windowed read of the last N source: samples for this
        // (sid, key) pair; SensorHistoryStore only exposes last_value /
        // last_value_above (single row) and series (windowed but key-only, not
        // source-scoped, so it collides across gateways sharing a soilmoisture
        // key). Adding a source-scoped windowed read is new store plumbing;
        // deferred per spec D1 to a fast-follow once that read exists.
        if !stale {
            continue;
        }
        out.push(crate::model::SoilProbeFault {
            zone_slug: z.slug.clone(),
            zone_name: z.name.clone(),
            sensor_id: spec.to_string(),
            since_epoch,
        });
    }
    out
}

/// Native per-zone soil extras (temp / EC / battery) resolved alongside
/// moisture but kept OFF the engine's `ZoneSoil` (no skip rule consumes them).
/// Published to HA via the snapshot `zones[]` and used to derive the frost
/// gate's yard-min soil temperature.
#[derive(Debug, Clone, Default)]
pub(crate) struct ZoneSoilExtra {
    pub(crate) slug: String,
    pub(crate) temp_f: Option<f64>,
    pub(crate) ec: Option<f64>,
    pub(crate) battery_pct: Option<f64>,
}

/// Resolve the native temp/EC/battery sibling channels for every configured
/// zone whose moisture is a `source:<id>:soilmoisture<N>` channel.
pub(crate) async fn resolve_soil_extras(
    cfg: &[ZoneSoilCfg],
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Vec<ZoneSoilExtra> {
    let mut out = Vec::with_capacity(cfg.len());
    for z in cfg {
        let spec = z.soil_sensor_id.as_deref();
        out.push(ZoneSoilExtra {
            slug: z.slug.clone(),
            temp_f: resolve_soil_sibling(spec, |n| format!("soiltemp{n}f"), history).await,
            ec: resolve_soil_sibling(spec, |n| format!("soilec{n}"), history).await,
            battery_pct: resolve_soil_sibling(spec, |n| format!("soilbatt{n}"), history).await,
        });
    }
    out
}

/// Resolve a per-channel sibling reading (soil temp / EC / battery) for a zone
/// whose moisture sensor is a native `source:<id>:soilmoisture<N>` channel, by
/// swapping the key suffix and reading the latest history value for the same
/// source + channel. Returns `None` for non-`source:` specs (e.g. an `ha:`
/// entity has no native sibling) or when the reading is unavailable.
pub(crate) async fn resolve_soil_sibling(
    moisture_spec: Option<&str>,
    sibling_key: impl Fn(&str) -> String,
    history: Option<&crate::persistence::SensorHistoryStore>,
) -> Option<f64> {
    let rest = moisture_spec?.strip_prefix("source:")?;
    let (sid, key) = rest.split_once(':')?;
    let n = key.strip_prefix("soilmoisture")?;
    let h = history?;
    h.last_value(sid.to_string(), sibling_key(n))
        .await
        .ok()
        .flatten()
        .map(|r| r.value)
}

/// The forecast-observations ledger, when the schema has it. The table is
/// created by M0006 on the v2 boot path; a v1-only install has none, and
/// the probe runs once so no tick logs a missing-table error.
pub(crate) async fn forecast_observations_store(
    history_conn: Option<&Arc<Mutex<Connection>>>,
) -> Option<crate::persistence::ForecastObservationsStore> {
    match history_conn {
        Some(c) => {
            // `c` is an Arc<tokio::sync::Mutex<rusqlite::Connection>>; calling
            // blocking_lock() from inside a tokio task panics ("Cannot block
            // the current thread from within a runtime"). The table-existence
            // probe is a one-shot at spawn time, so await the async lock
            // instead. rusqlite's query_row is synchronous and briefly blocks
            // the worker thread, which is acceptable for a single SELECT.
            let exists = {
                let conn = c.lock().await;
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='forecast_observations'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map(|n| n > 0)
                .unwrap_or(false)
            };
            if exists {
                Some(crate::persistence::ForecastObservationsStore::new(
                    c.clone(),
                ))
            } else {
                tracing::info!(
                    "forecast_observations table absent (v1 schema); skipping bias ingest"
                );
                None
            }
        }
        None => None,
    }
}
