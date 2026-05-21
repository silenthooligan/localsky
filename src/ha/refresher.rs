// Long-running task that polls HA REST and rebuilds the irrigation
// snapshot on each cycle. One spawn per process; failures back off and
// keep going (we'd rather show stale data than crash the whole app).

use crate::forecast::snapshot::ForecastSnapshot;
use crate::forecast::ForecastStore;
use crate::ha::rest::HaClient;
use crate::ha::skip_logic::{self, et_heat_multiplier, heat_index_f, Inputs};
use crate::ha::snapshot::{DayVerdict, Forecast, IrrigationSnapshot, SoilForecast, WaterBudget, ZoneState};
use crate::ha::store::IrrigationStore;
use crate::history::IngestState;
use crate::tempest::state::TempestStore;
use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Stations 1-4 on the Aperture controller, in IU sequence order. Used
/// by both the snapshot builder and (in Phase 3) the SQLite ingest.
pub const ZONE_SLUGS: [&str; 4] = ["back_yard", "front_yard", "side_yard", "back_yard_shrubs"];

/// Friendly names for each slug. The HA device registry overrides
/// these per-device via `name_by_user`, but we don't read that today;
/// the slug-derived mapping below tracks the user's actual labels in
/// the dashboard's lovelace YAML.
fn zone_display_name(slug: &str) -> &'static str {
    match slug {
        "back_yard" => "Back Yard",
        "front_yard" => "Front Yard",
        "side_yard" => "Side Yard",
        "back_yard_shrubs" => "Back Yard Shrubs",
        _ => "?",
    }
}

/// Default poll interval. Irrigation state is low-frequency so 10s is
/// plenty; manual zone runs surface within a tap-of-an-eyeblink.
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);

pub fn spawn_refresher(
    store: Arc<IrrigationStore>,
    forecast_store: Arc<ForecastStore>,
    tempest_store: Arc<TempestStore>,
    history_conn: Option<Arc<Mutex<Connection>>>,
    push: crate::push::PushDispatcher,
) {
    tokio::spawn(async move {
        let client = match HaClient::from_env() {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("ha_client init failed: {e:#}");
                return;
            }
        };

        let mut ingest = IngestState::new();
        // Edge-detection state for push events. Tracks per-zone running
        // and the start_epoch when each zone last transitioned to running
        // so ZoneStopped can include duration_min.
        let mut prev_zone_running: std::collections::HashMap<String, bool> =
            std::collections::HashMap::new();
        let mut zone_started_at: std::collections::HashMap<String, i64> =
            std::collections::HashMap::new();
        // Daily verdict push fires once per local-day; the date string
        // is the dedupe key.
        let mut last_verdict_day: Option<String> = None;

        loop {
            match refresh_once(&client, &forecast_store, &tempest_store).await {
                Ok(snap) => {
                    if let Some(db) = history_conn.as_ref() {
                        ingest.observe(db, &snap).await;
                    }
                    emit_push_events(
                        &push,
                        &snap,
                        &mut prev_zone_running,
                        &mut zone_started_at,
                        &mut last_verdict_day,
                    );
                    store.store(snap);
                }
                Err(e) => {
                    tracing::warn!("ha refresh failed: {e:#}");
                    // Mark the existing snapshot as stale rather than
                    // overwriting it with empty data; the UI shows the
                    // last good values with an "HA unreachable" badge.
                    let mut prev = (*store.snapshot()).clone();
                    prev.ha_reachable = false;
                    store.store(prev);
                }
            }
            tokio::time::sleep(REFRESH_INTERVAL).await;
        }
    });
}

/// Walk the snapshot and emit push events on edge transitions:
/// - ZoneStarted/ZoneStopped on each zone's running flag flip.
/// - DailyVerdict once per local day, the first time we see a non-empty
///   verdict for that day.
fn emit_push_events(
    push: &crate::push::PushDispatcher,
    snap: &IrrigationSnapshot,
    prev_running: &mut std::collections::HashMap<String, bool>,
    started_at: &mut std::collections::HashMap<String, i64>,
    last_verdict_day: &mut Option<String>,
) {
    use crate::push::PushEvent;
    let now = Utc::now().timestamp();
    for z in &snap.zones {
        let was = *prev_running.get(&z.slug).unwrap_or(&false);
        if z.running && !was {
            started_at.insert(z.slug.clone(), now);
            push.emit(PushEvent::ZoneStarted {
                name: z.name.clone(),
                slug: z.slug.clone(),
            });
        } else if !z.running && was {
            let dur_s = started_at
                .remove(&z.slug)
                .map(|start| (now - start).max(0))
                .unwrap_or(0);
            let duration_min = ((dur_s as f64) / 60.0).round() as u32;
            push.emit(PushEvent::ZoneStopped {
                name: z.name.clone(),
                slug: z.slug.clone(),
                duration_min,
            });
        }
        prev_running.insert(z.slug.clone(), z.running);
    }

    // Daily verdict fires once per local day. The "today" label is the
    // local-date YYYY-MM-DD; on the first refresh after midnight rolls
    // we emit one event with the new verdict.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let verdict = snap.skip_check.verdict.clone();
    if !verdict.is_empty() && last_verdict_day.as_deref() != Some(today.as_str()) {
        push.emit(crate::push::PushEvent::DailyVerdict {
            verdict,
            reason: snap.skip_check.reason.clone(),
        });
        *last_verdict_day = Some(today);
    }
}

/// Pull /api/states once, blend with the in-process forecast + tempest
/// stores, and build the snapshot. Pure read-only with respect to HA
/// (we don't mutate any HA state from here).
async fn refresh_once(
    client: &HaClient,
    forecast_store: &ForecastStore,
    tempest_store: &TempestStore,
) -> anyhow::Result<IrrigationSnapshot> {
    let states = client.states().await?;
    let map: HashMap<String, Value> = states
        .into_iter()
        .filter_map(|v| {
            v.get("entity_id")
                .and_then(|e| e.as_str())
                .map(|id| (id.to_string(), v.clone()))
        })
        .collect();

    let mut snap = IrrigationSnapshot {
        last_refresh_epoch: Utc::now().timestamp(),
        ha_reachable: true,
        ..Default::default()
    };

    // IU sequence: next start, enabled/suspended. Total minutes is
    // computed below from per-zone Smart Irrigation values, NOT from
    // IU's zones array, because IU's per-zone duration stays at the
    // YAML placeholder (1 min) until SI's nightly sync at 23:30
    // overwrites it. Summing SI gives the actual minutes the morning
    // run will produce, matching what the dashboard should advertise.
    if let Some(s) = map.get("binary_sensor.irrigation_unlimited_c1_s1") {
        let attrs = s.get("attributes").cloned().unwrap_or(Value::Null);
        snap.iu_enabled = attrs
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // suspended is null when the sequence is armed, a string when
        // currently suspended (and absent only on a malformed payload).
        snap.iu_suspended = !matches!(
            attrs.get("suspended"),
            Some(Value::Null) | None
        );
        if let Some(next_start) = attrs.get("next_start").and_then(Value::as_str) {
            if let Ok(dt) = DateTime::parse_from_rfc3339(next_start) {
                snap.next_run_epoch = dt.timestamp();
            }
        }
    }

    // Master enable + water level.
    snap.master_enable = state_eq(&map, "switch.aperture_sprinklers_enabled", "on");
    snap.water_level_pct =
        state_f64(&map, "sensor.aperture_sprinklers_water_level").unwrap_or(0.0);

    // Vacation pause + one-day override helpers. Both are user-created
    // HA helpers (input_datetime + input_select). When missing, the snapshot
    // exposes override_helpers_present=false so the mobile UI can disable
    // the controls with a "(HA helper not configured)" hint rather than
    // letting the action POST fail with a 502 on tap.
    let pause_state = map.get("input_datetime.irrigation_pause_until");
    let override_state = map.get("input_select.irrigation_override_tomorrow");
    snap.override_helpers_present = pause_state.is_some() && override_state.is_some();
    snap.pause_until_epoch = pause_state
        .and_then(|s| s.get("attributes"))
        .and_then(|a| a.get("timestamp"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    snap.override_tomorrow = override_state
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_string();

    // Pre-compute the heat multiplier here (the snapshot.forecast struct
    // also recomputes this later — the dupe is intentional because the
    // zone loop needs it before forecast_store.snapshot() is consumed
    // below, and the cost is one heat-index calc per refresh).
    let zone_loop_heat_mult = {
        let fc_peek = forecast_store.snapshot();
        let humidity_peek = tempest_store.snapshot().rh_pct;
        let tmax_peek = fc_peek.max_temp_next_3d_f().unwrap_or(0.0);
        et_heat_multiplier(heat_index_f(tmax_peek, humidity_peek))
    };

    // Per-zone state. Sum planned_run_seconds across the four zones to
    // get the real next-run total (since IU's zones array carries the
    // YAML placeholder until SI's nightly sync overwrites it).
    //
    // The math tile reads SI's per-zone attributes directly so the
    // displayed formula matches SI's internal compute. heat_mult is the
    // global forecast multiplier (same one SI multiplies into ET via
    // the Phase C HA automation); capture_efficiency is the constant
    // LocalSky uses in the Phase E water-balance projection.
    snap.zones = ZONE_SLUGS
        .iter()
        .map(|slug| {
            let running_id = format!("binary_sensor.aperture_sprinklers_{slug}_station_running");
            let si_id = format!("sensor.smart_irrigation_{slug}");
            let attrs = map.get(&si_id).and_then(|s| s.get("attributes"));
            let bucket_mm = attrs
                .and_then(|a| a.get("bucket"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let kc = attrs
                .and_then(|a| a.get("multiplier"))
                .and_then(Value::as_f64)
                .unwrap_or(1.0);
            let throughput_mm_hr = attrs
                .and_then(|a| a.get("throughput"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let max_dur = attrs
                .and_then(|a| a.get("maximum_duration"))
                .and_then(Value::as_f64)
                .unwrap_or(3600.0) as u32;
            let planned = state_f64(&map, &si_id).unwrap_or(0.0) as u32;
            // SI's flex math (matches custom_components/smart_irrigation):
            //   seconds = (|bucket_mm| / throughput_mm_hr) * 3600 * multiplier
            // Then capped at maximum_duration.
            let raw_seconds = if throughput_mm_hr > 0.0 && bucket_mm < 0.0 {
                (bucket_mm.abs() / throughput_mm_hr * 3600.0 * kc) as u32
            } else {
                0
            };
            let math = Some(crate::ha::snapshot::ZoneMath {
                bucket_mm,
                kc,
                throughput_mm_hr,
                heat_mult: zone_loop_heat_mult,
                capture_eff: 0.70, // matches compute_soil_forecasts CAPTURE_EFFICIENCY
                raw_seconds,
                max_duration_seconds: max_dur,
                scheduled_seconds: planned,
                cap_binding: raw_seconds > max_dur,
            });
            ZoneState {
                name: zone_display_name(slug).to_string(),
                slug: (*slug).to_string(),
                hex: String::new(), // Populated in Phase 3 from device_registry if needed.
                running: state_eq(&map, &running_id, "on"),
                today_run_minutes: 0.0, // Populated by SQLite history in Phase 3.
                bucket_mm,
                planned_run_seconds: planned,
                last_run_epoch: 0, // Populated by SQLite history in Phase 3.
                math,
                // photo_url is read by the dashboard from /api/config on
                // mount and joined to each zone by slug. Kept None here so
                // the snapshot remains a pure runtime-state object.
                photo_url: None,
            }
        })
        .collect();
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;

    // Forecast block. Aggregates Tempest live + Open-Meteo regional
    // forecast into one struct the UI can render directly. The
    // Tempest precipitation entity reports in inches in this HA
    // install (HA's WeatherFlow integration emits in user-display
    // units when imperial is configured); do NOT divide by 25.4.
    let rain_today_tempest = state_f64(&map, "sensor.st_00206451_precipitation").unwrap_or(0.0);
    let rain_today_om = state_f64(&map, "sensor.open_meteo_rain_today")
        .map(|mm| mm / 25.4)
        .unwrap_or(0.0);
    let rain_intensity = state_f64(&map, "sensor.st_00206451_precipitation_intensity").unwrap_or(0.0);
    let rain_type = map
        .get("sensor.st_00206451_precipitation_type")
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_string();
    let rain_tomorrow = state_f64(&map, "sensor.open_meteo_rain_tomorrow")
        .map(|mm| mm / 25.4)
        .unwrap_or(0.0);
    let rain_3day = state_f64(&map, "sensor.open_meteo_rain_3day")
        .map(|mm| mm / 25.4)
        .unwrap_or(0.0);
    // Phase A: pull forecast intelligence directly from the in-process
    // ForecastStore (Open-Meteo 7-day + 48h + 3-day past) and the live
    // Tempest store. No round-trip via HA REST sensors — single source,
    // fewer moving parts.
    let fc = forecast_store.snapshot();
    let tempest = tempest_store.snapshot();

    let rain_today_used = rain_today_tempest.max(rain_today_om);
    let humidity_now = tempest.rh_pct;
    // Prefer the in-process Tempest listener once it's received its
    // first packet. Falls back to HA's Tempest-derived sensors before
    // the first obs_st lands so the dashboard still has live values
    // immediately after a container restart.
    let tempest_alive = tempest.last_packet_epoch > 0;
    let temp_now = if tempest_alive {
        tempest.air_temp_f
    } else {
        state_f64(&map, "sensor.st_00206451_temperature").unwrap_or(70.0)
    };
    let wind_now = if tempest_alive {
        tempest.wind_avg_mph
    } else {
        state_f64(&map, "sensor.st_00206451_wind_speed_average").unwrap_or(0.0)
    };

    let (rain_tomorrow_om_in, rain_tomorrow_prob) = fc.tomorrow_precip_with_prob_in();
    let rain_3day_weighted = fc.future_n_day_weighted_precip_in(3);
    let rain_7day_weighted = fc.future_n_day_weighted_precip_in(7);
    let rain_next_4h = fc.next_n_hours_precip_in(4);
    let temp_min_24h = fc.min_temp_next_24h_f().unwrap_or(0.0);
    let temp_max_3day = fc.max_temp_next_3d_f().unwrap_or(0.0);
    let wind_max_today = fc.wind_max_today_mph().unwrap_or(0.0);
    let days_since_rain = fc.days_since_significant_rain(rain_today_used);

    // Tomorrow's rain: prefer the live OM forecast snapshot over HA's
    // REST sensor (the latter only refreshes every 4h vs our 30 min).
    let rain_tomorrow_used = if fc.has_tomorrow() {
        rain_tomorrow_om_in
    } else {
        rain_tomorrow
    };

    let heat_index_now = heat_index_f(temp_now, humidity_now);
    let heat_index_3day = heat_index_f(temp_max_3day, humidity_now);
    let heat_mult = et_heat_multiplier(heat_index_3day);

    let forecast = Forecast {
        rain_today_tempest_in: rain_today_tempest,
        rain_today_om_in: rain_today_om,
        rain_intensity_in_hr: rain_intensity,
        rain_type,
        rain_tomorrow_in: rain_tomorrow_used,
        rain_3day_in: rain_3day,
        eto_today_mm: state_f64(&map, "sensor.open_meteo_eto_today").unwrap_or(0.0),
        eto_tomorrow_mm: state_f64(&map, "sensor.open_meteo_eto_tomorrow").unwrap_or(0.0),
        eto_3day_avg_mm: state_f64(&map, "sensor.open_meteo_eto_3day_avg").unwrap_or(0.0),
        temp_max_today_f: state_f64(&map, "sensor.open_meteo_temp_max_today").unwrap_or(0.0),
        temp_min_today_f: state_f64(&map, "sensor.open_meteo_temp_min_today").unwrap_or(0.0),
        wind_max_today_mph: wind_max_today,
        humidity_mean_today_pct: state_f64(&map, "sensor.open_meteo_humidity_mean_today").unwrap_or(0.0),

        rain_3day_weighted_in: rain_3day_weighted,
        rain_7day_weighted_in: rain_7day_weighted,
        rain_next_4h_in: rain_next_4h,
        rain_tomorrow_prob_pct: rain_tomorrow_prob,
        temp_min_24h_f: temp_min_24h,
        temp_max_3day_f: temp_max_3day,
        humidity_now_pct: humidity_now,
        heat_index_now_f: heat_index_now,
        heat_index_max_3day_f: heat_index_3day,
        heat_multiplier: heat_mult,
        days_since_significant_rain: days_since_rain,
    };

    let inputs = Inputs {
        temp_now_f: temp_now,
        wind_now_mph: wind_now,
        rain_today_in: rain_today_used,
        rain_intensity_now_in_hr: rain_intensity,
        humidity_now_pct: humidity_now,

        forecast_in: rain_tomorrow_used,
        rain_tomorrow_prob_pct: rain_tomorrow_prob,
        rain_3day_weighted_in: rain_3day_weighted,
        rain_7day_weighted_in: rain_7day_weighted,
        rain_next_4h_in: rain_next_4h,
        wind_max_today_mph: wind_max_today,
        temp_min_24h_f: temp_min_24h,
        temp_max_3day_f: temp_max_3day,
        days_since_significant_rain: days_since_rain,

        max_wind_mph: state_f64(&map, "input_number.irrigation_max_wind_mph").unwrap_or(10.0),
        min_temp_f: state_f64(&map, "input_number.irrigation_min_temp_f").unwrap_or(38.0),
        rain_skip_in: state_f64(&map, "input_number.irrigation_rain_skip_in").unwrap_or(0.25),

        // Phase E: soil sensor inputs (WH52 via ecowitt2mqtt + HA template).
        // None when the underlying entity is unavailable; the skip-logic
        // rules silently no-op so missing data falls back to weather-only.
        soil_back_yard_pct: state_f64(&map, "sensor.back_yard_soil_moisture"),
        soil_front_yard_pct: state_f64(&map, "sensor.front_yard_soil_moisture"),
        soil_side_yard_pct: state_f64(&map, "sensor.side_yard_soil_moisture"),
        soil_back_yard_shrubs_pct: state_f64(&map, "sensor.back_yard_shrubs_soil_moisture"),
        soil_temp_yard_min_f: state_f64(&map, "sensor.soil_temp_yard_now_min"),
        soil_temp_yard_max_f: state_f64(&map, "sensor.soil_temp_yard_now_max"),
        frost_skip_soil_f: state_f64(&map, "input_number.irrigation_frost_skip_f").unwrap_or(35.0),
        saturation_back_yard_pct: state_f64(&map, "input_number.irrigation_back_yard_saturation_pct").unwrap_or(70.0),
        saturation_front_yard_pct: state_f64(&map, "input_number.irrigation_front_yard_saturation_pct").unwrap_or(70.0),
        saturation_side_yard_pct: state_f64(&map, "input_number.irrigation_side_yard_saturation_pct").unwrap_or(70.0),
        saturation_back_yard_shrubs_pct: state_f64(&map, "input_number.irrigation_back_yard_shrubs_saturation_pct").unwrap_or(85.0),

        is_paused: state_eq(&map, "input_boolean.irrigation_pause", "on"),
        is_dry_run: state_eq(&map, "input_boolean.irrigation_dry_run", "on"),

        // Phase 4 control surfaces. Today's verdict ignores the tomorrow
        // override (is_tomorrow=false); the verdict-strip path below sets
        // it true on the [+1] cell.
        pause_until_epoch: snap.pause_until_epoch,
        now_epoch: Utc::now().timestamp(),
        override_tomorrow: snap.override_tomorrow.clone(),
        is_tomorrow: false,
    };
    snap.skip_check = skip_logic::evaluate(&inputs);
    snap.forecast = forecast;
    snap.seven_day_verdicts = compute_seven_day_verdicts(&fc, &inputs);
    snap.soil_forecasts = compute_soil_forecasts(&fc, &inputs, &map);
    snap.water_budgets = compute_water_budgets(&fc, &inputs, &map, &snap.zones);

    Ok(snap)
}

/// Phase H — weekly water-budget plan per zone. Replaces SI's daily-bucket
/// flex math with a deep-and-infrequent schedule that allocates a weekly
/// water target across N sessions, defers when rain is forecast, and
/// spaces sessions by `7 / sessions_per_week` days so each run is a real
/// soak rather than a daily light sprinkle.
///
/// Outputs `today_seconds` per zone — what the HA budget-override
/// automation at 23:30:25 calls `IU.adjust_time(actual=...)` with. Zero
/// means "don't run this zone today" (rain incoming, recently watered,
/// or mode is off).
fn compute_water_budgets(
    fc: &ForecastSnapshot,
    today_inputs: &Inputs,
    map: &HashMap<String, Value>,
    zones: &[ZoneState],
) -> Vec<WaterBudget> {
    // Per-zone agronomic defaults (matches docs/MANUAL.md St. Augustine
    // FL guidance). Override via HA input_numbers per zone.
    let zone_defaults: [(&str, &str, f64, u32); 4] = [
        ("back_yard",        "Back Yard",         1.00, 2),
        ("front_yard",       "Front Yard",        1.00, 2),
        ("side_yard",        "Side Yard",         1.00, 2),
        ("back_yard_shrubs", "Back Yard Shrubs",  0.50, 1),
    ];
    const CAPTURE_EFFICIENCY: f64 = 0.7;
    const SESSION_RAIN_DEFER_IN: f64 = 0.10; // ≥0.10" forecast next 24h → defer

    let heat_mult = today_inputs.temp_max_3day_f.max(0.0); // dummy to silence unused; real heat below
    let _ = heat_mult; // suppress warning
    let heat_mult_eff = {
        let hi = heat_index_f(today_inputs.temp_max_3day_f, today_inputs.humidity_now_pct);
        et_heat_multiplier(hi)
    };

    let now_epoch = chrono::Utc::now().timestamp();
    // Forecast: next-24h rain (sum of hourly[0..24] precip).
    let next_24h_rain_in = fc.next_n_hours_precip_in(24);
    // 7-day probability-weighted total rain.
    let week_rain_weighted_in: f64 = fc
        .daily
        .iter()
        .take(7)
        .map(|d| d.precip_sum_in * (d.precip_probability_max as f64) / 100.0)
        .sum();

    let mut out = Vec::with_capacity(zone_defaults.len());
    for (slug, name, default_budget_in, default_sessions) in zone_defaults.iter() {
        // Operator-tunable HA helpers (no initial: per the established
        // convention so recorder restore_state preserves edits).
        let weekly_budget_in = state_f64(map, &format!("input_number.irrigation_{slug}_weekly_budget_in"))
            .unwrap_or(*default_budget_in);
        let sessions_per_week = state_f64(map, &format!("input_number.irrigation_{slug}_sessions_per_week"))
            .map(|v| v.round() as u32)
            .unwrap_or(*default_sessions)
            .max(1);
        let mode_active = state_eq(map, &format!("input_boolean.irrigation_{slug}_weekly_budget_mode"), "on");

        // Per-zone SI inputs we need: throughput + maximum_duration.
        let si_id = format!("sensor.smart_irrigation_{slug}");
        let attrs = map.get(&si_id).and_then(|s| s.get("attributes"));
        let throughput_mm_hr = attrs
            .and_then(|a| a.get("throughput"))
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let max_dur_s = attrs
            .and_then(|a| a.get("maximum_duration"))
            .and_then(Value::as_f64)
            .unwrap_or(3600.0) as u32;

        // Water-balance math: weekly budget, minus expected captured rain.
        let weekly_budget_mm = weekly_budget_in * 25.4;
        let expected_rain_mm = week_rain_weighted_in * 25.4 * CAPTURE_EFFICIENCY;
        let needed_mm = (weekly_budget_mm - expected_rain_mm).max(0.0);
        let mm_per_session = needed_mm / sessions_per_week as f64;
        let seconds_per_session = if throughput_mm_hr > 0.0 {
            // Multiply by heat_mult to compensate for accelerated ET
            // (same Kc-style bias SI applies). Divide by CAPTURE_EFFICIENCY
            // so that the *root-zone* depth matches mm_per_session after
            // runoff/canopy losses.
            ((mm_per_session / throughput_mm_hr) * 3600.0 * heat_mult_eff / CAPTURE_EFFICIENCY) as u32
        } else {
            0
        };
        let session_capped = seconds_per_session > max_dur_s;
        let session_final = seconds_per_session.min(max_dur_s);

        // Last run epoch for this zone — pulled from ZoneState (which the
        // history ingest populates) so we don't have to round-trip SQLite.
        let last_run_epoch = zones
            .iter()
            .find(|z| z.slug == *slug)
            .map(|z| z.last_run_epoch)
            .unwrap_or(0);

        // Today's recommendation.
        let min_interval_days = (7.0 / sessions_per_week as f64).floor() as i64;
        let days_since_last_run = if last_run_epoch > 0 {
            (now_epoch - last_run_epoch) / 86400
        } else {
            i64::MAX / 2
        };
        let (today_seconds, today_reason) = if !mode_active {
            (
                0u32,
                "budget mode off — SI's daily flex owns this zone".to_string(),
            )
        } else if next_24h_rain_in >= SESSION_RAIN_DEFER_IN {
            (
                0,
                format!(
                    "rain expected next 24h ({:.2}\" forecast ≥ {:.2}\")",
                    next_24h_rain_in, SESSION_RAIN_DEFER_IN
                ),
            )
        } else if days_since_last_run < min_interval_days {
            (
                0,
                format!(
                    "last run {} day(s) ago — minimum interval is {} days at {} sessions/wk",
                    days_since_last_run, min_interval_days, sessions_per_week
                ),
            )
        } else if needed_mm <= 0.0 {
            (
                0,
                format!(
                    "forecast rain {:.2}\" covers the {:.2}\" weekly budget",
                    week_rain_weighted_in, weekly_budget_in
                ),
            )
        } else {
            (
                session_final,
                format!(
                    "scheduled session {} of {} this week — {:.2} mm depth = {:.0} min",
                    1, // session_index — proper allocation logic deferred
                    sessions_per_week,
                    mm_per_session,
                    session_final as f64 / 60.0
                ),
            )
        };

        out.push(WaterBudget {
            zone_slug: slug.to_string(),
            zone_name: name.to_string(),
            mode_active,
            weekly_budget_in,
            sessions_per_week,
            expected_rain_mm,
            needed_mm,
            mm_per_session,
            seconds_per_session,
            session_capped,
            last_run_epoch,
            today_seconds,
            today_reason,
        });
    }
    out
}

/// Phase E predictive — per-zone 7-day soil-moisture projection. Uses a
/// FAO-56-flavored water balance: today's calibrated reading is the
/// starting point; each day subtracts the daily ET (scaled by zone Kc)
/// and adds the probability-weighted forecast rain (scaled by a capture
/// efficiency factor to account for runoff). Irrigation is not modeled
/// — the curve answers "if I did nothing all week, would each zone stay
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
///   - Rain capture efficiency 0.7 — empirical, accounts for runoff,
///     slope, and canopy interception. Knock-down values not modeled.
///   - Probe placement at root depth (operator's responsibility).
fn compute_soil_forecasts(
    fc: &ForecastSnapshot,
    today: &Inputs,
    map: &HashMap<String, Value>,
) -> Vec<SoilForecast> {
    // Per-zone tuning. Slug + display name + Kc + effective root-zone
    // depth (mm) — turf has shallower active roots than mulched shrubs
    // so equivalent ET drops moisture % faster.
    let zones: [(&str, &str, f64, f64); 4] = [
        ("back_yard",         "Back Yard",          1.08, 150.0),
        ("front_yard",        "Front Yard",         1.08, 150.0),
        ("side_yard",         "Side Yard",          1.08, 150.0),
        ("back_yard_shrubs",  "Back Yard Shrubs",   0.50, 200.0),
    ];
    const CAPTURE_EFFICIENCY: f64 = 0.7;

    // Daily ET, mm. Today's value carries across the window. heat_multiplier
    // bumps it on heat-advisory days so a 95°F+ forecast tracks realistically.
    let et0_today_mm = state_f64(map, "sensor.open_meteo_eto_today").unwrap_or(5.0);
    let daily_et_mm = et0_today_mm * fc_heat_multiplier(today);

    let n_days = fc.daily.len().min(7).max(1);
    let mut out = Vec::with_capacity(zones.len());

    for (slug, name, kc, soil_depth_mm) in zones.iter() {
        // Pull this zone's live + threshold state. Defaults match the HA
        // input_number initials so a missing helper doesn't break the math.
        let current = state_f64(map, &format!("sensor.{slug}_soil_moisture"));
        let target_min = state_f64(
            map,
            &format!("input_number.irrigation_{slug}_target_min_pct"),
        )
        .unwrap_or(if *slug == "back_yard_shrubs" { 25.0 } else { 30.0 });
        let target_max = state_f64(
            map,
            &format!("input_number.irrigation_{slug}_saturation_pct"),
        )
        .unwrap_or(if *slug == "back_yard_shrubs" { 85.0 } else { 70.0 });

        // No probe data → emit a no_data entry the dashboard renders as
        // a grey "(probe offline)" tile rather than rendering a flat zero.
        let Some(start_pct) = current else {
            out.push(SoilForecast {
                zone_slug: slug.to_string(),
                zone_name: name.to_string(),
                current_pct: None,
                target_min_pct: target_min,
                target_max_pct: target_max,
                predicted_pct: vec![0.0; n_days],
                min_predicted_pct: 0.0,
                max_predicted_pct: 0.0,
                days_below_target: 0,
                days_above_max: 0,
                status: "no_data".to_string(),
            });
            continue;
        };

        let mut series = Vec::with_capacity(n_days);
        let mut moisture = start_pct;
        series.push(moisture);

        // Step through each future day applying the water-balance delta.
        // Day 0 is "today" (the current reading), so the deltas start at
        // day 1 using daily[0]'s rain prediction (the rest of today) and
        // daily[N]'s rain for day N onward.
        for d in fc.daily.iter().take(n_days).skip(1) {
            let rain_effective_mm =
                d.precip_sum_in * 25.4 * (d.precip_probability_max as f64) / 100.0;
            let captured_mm = rain_effective_mm * CAPTURE_EFFICIENCY;
            let et_loss_mm = daily_et_mm * kc;
            let delta_mm = captured_mm - et_loss_mm;
            let delta_pct = delta_mm / soil_depth_mm * 100.0;
            moisture = (moisture + delta_pct).clamp(0.0, 100.0);
            series.push(moisture);
        }

        let min_predicted = series
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
            .max(0.0);
        let max_predicted = series
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
            .min(100.0);
        let days_below = series.iter().filter(|p| **p <= target_min).count() as u32;
        let days_above = series.iter().filter(|p| **p >= target_max).count() as u32;

        // Status classification: "wet" wins over "dry" so a saturated
        // start doesn't get flagged as dry from a forecast dry stretch
        // that hasn't happened yet. "dry" requires either crossing the
        // target_min floor at any point OR ≥2 days under it.
        let status = if max_predicted >= target_max {
            "wet"
        } else if min_predicted <= target_min || days_below >= 2 {
            "dry"
        } else {
            "ok"
        };

        out.push(SoilForecast {
            zone_slug: slug.to_string(),
            zone_name: name.to_string(),
            current_pct: Some(start_pct),
            target_min_pct: target_min,
            target_max_pct: target_max,
            predicted_pct: series,
            min_predicted_pct: min_predicted,
            max_predicted_pct: max_predicted,
            days_below_target: days_below,
            days_above_max: days_above,
            status: status.to_string(),
        });
    }

    out
}

/// Pull the heat_multiplier the engine has already computed for today's
/// Inputs (avoids recomputing the NOAA Steadman heat index from scratch).
/// The multiplier bumps daily ET on heat-advisory days so the projection
/// tracks the same depletion-acceleration SI applies to its bucket math.
fn fc_heat_multiplier(today: &Inputs) -> f64 {
    let hi = heat_index_f(today.temp_max_3day_f, today.humidity_now_pct);
    et_heat_multiplier(hi)
}

/// Compute the 7-day forward verdict strip. For each daily forecast
/// entry (today + 6 future days), construct synthetic Inputs that
/// answer "would I water on this day?" and run the same evaluate()
/// the morning skip-check uses. Same engine, same rules — the strip
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
fn compute_seven_day_verdicts(fc: &ForecastSnapshot, today: &Inputs) -> Vec<DayVerdict> {
    crate::engine::compute_verdict_strip(fc, today, &crate::config::schema::SkipRuleParams::default())
}


fn state_eq(map: &HashMap<String, Value>, eid: &str, expected: &str) -> bool {
    map.get(eid)
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .map(|s| s == expected)
        .unwrap_or(false)
}

fn state_f64(map: &HashMap<String, Value>, eid: &str) -> Option<f64> {
    map.get(eid)
        .and_then(|s| s.get("state"))
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<f64>().ok())
}

