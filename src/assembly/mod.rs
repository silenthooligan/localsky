// The pure half of a refresh: everything fetched, nothing awaited.
//
// `refresher::build_from_map` is the IO shell. It mints the one instant a
// tick describes, takes the store snapshots, reads what lives in a
// database, and hands all of it here. This module turns those inputs into
// the engine's `Inputs` and the published `IrrigationSnapshot` and never
// reads a clock, a store or a database itself, so the same inputs give
// the same snapshot on any machine, in any timezone, with no tokio.
//
// A guard in `refresher` keeps it that way: no `async`, no `.await`, no
// tokio, no clock read in this file.

use std::collections::HashMap;

use serde_json::Value;

use crate::engine::sizing::{force_run_floor, seasonal_cap_binds, seasonal_capped};
use crate::engine::skip_rules::{heat_index_f, Inputs};
use crate::engine::skip_rules::{LiveReadings, ZoneSoil};
use crate::forecast::snapshot::ForecastSnapshot;
use crate::model::{Forecast, IrrigationSnapshot, SoilProbeFault, ZoneState};
use crate::refresher::{
    budget_zones_for_active, sprinkler_prefix, BalanceTick, SoilTickEvidence, WateringPolicy,
    ZoneBudgetCfg, ZoneRuntime, ZoneSoilExtra,
};

/// What the shell fetched from a database before assembly.
#[derive(Debug, Clone, Default)]
pub struct Prefetched {
    /// The station gauge's measured rain over the observed-rain window.
    pub observed_past_gauge_in: f64,
    /// Days since the gauge last measured significant rain.
    pub observed_days_since_rain: Option<u32>,
    pub(crate) soil_extras: Vec<ZoneSoilExtra>,
    /// One resolved reading per configured soil zone; empty when no zone
    /// has a probe configured (the assembly builds unprobed rows then).
    pub soil_zones: Vec<ZoneSoil>,
    pub soil_probe_faults: Vec<SoilProbeFault>,
}

pub mod pass;
pub mod readings;
mod water_plan;
pub(crate) use pass::*;
pub(crate) use readings::*;

/// Everything one assembly reads.
pub struct AssemblyInput<'a> {
    /// Home Assistant `/api/states` keyed by entity id; empty on the
    /// native path.
    pub map: HashMap<String, Value>,
    pub forecast: std::sync::Arc<ForecastSnapshot>,
    pub tempest: std::sync::Arc<crate::tempest::state::Snapshot>,
    pub current_weather: std::collections::BTreeMap<String, crate::weather::CurrentWeatherSample>,
    pub field_sources: std::collections::BTreeMap<String, String>,
    pub rain_owner: Option<crate::tempest::state::RainOwner>,
    pub rain_today_owner: Option<crate::tempest::state::RainOwner>,
    pub zones: &'a [crate::zones::ZoneIdent],
    pub zone_runtime: &'a HashMap<String, ZoneRuntime>,
    pub watering_policy: &'a WateringPolicy,
    pub scripts: &'a crate::engine::scripting::CompiledScripts,
    pub balance: Option<&'a BalanceTick>,
    pub control: Option<&'a crate::model::IrrigationControlState>,
    /// Reasons copied from the registry's process-lifetime configuration hold.
    pub restart_reasons: Vec<String>,
    /// The one instant this pass describes.
    pub now_epoch: i64,
    pub prefetched: Prefetched,
}

/// The days this week on which any zone watered, for a restriction that
/// allows at most N days a week. From the same per-day evidence the soil
/// replay reads; empty before the first tick or without a database, which
/// reads as nothing spent.
pub fn watered_days(balance: Option<&BalanceTick>) -> Vec<crate::engine::clock::CivilDay> {
    balance
        .map(|b| {
            b.soil
                .dates
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    b.soil
                        .applied_valve_s
                        .values()
                        .any(|v| v.get(*i).is_some_and(|s| *s > 0))
                })
                .map(|(_, d)| crate::engine::clock::CivilDay::from_naive(*d))
                .collect()
        })
        .unwrap_or_default()
}

/// The last stage of a pass, run once by the shell after the controller
/// overlay and the multiplier: the sequence total and the next run. It
/// carries what `assemble` had and the shell does not: the forecast this
/// pass assembled from (a freezing pre-dawn moves the window after
/// sunrise), the week's watered days and the pass's instant.
pub struct Finalize {
    pub watered: Vec<crate::engine::clock::CivilDay>,
    pub forecast: std::sync::Arc<ForecastSnapshot>,
    pub now_epoch: i64,
}

impl Finalize {
    pub fn apply(&self, snap: &mut IrrigationSnapshot, policy: &WateringPolicy) {
        align_today_verdict(snap, policy.calendar, self.now_epoch);
        snap.next_run_total_minutes = snap
            .zones
            .iter()
            .map(|z| z.planned_run_seconds as f64)
            .sum::<f64>()
            / 60.0;
        set_next_run(snap, policy, &self.watered, &self.forecast, self.now_epoch);
    }
}

/// Today's tile is an observation of the completed decision, including scoped
/// gates and user rules. Only future tiles are synthetic weather projections.
/// Match the civil date so a stale provider day cannot inherit today's answer.
fn align_today_verdict(
    snap: &mut IrrigationSnapshot,
    calendar: crate::engine::calendar::Calendar,
    now_epoch: i64,
) {
    let Some(today) = calendar.date_of(now_epoch) else {
        return;
    };
    let Some(cell) = snap
        .seven_day_verdicts
        .iter_mut()
        .find(|cell| calendar.date_of(cell.time_epoch) == Some(today))
    else {
        return;
    };
    cell.verdict.clone_from(&snap.skip_check.verdict);
    cell.reason.clone_from(&snap.skip_check.reason);
    cell.reason_code.clone_from(&snap.skip_check.reason_code);
    cell.mixed_hold = snap.zone_verdicts.iter().any(|z| z.verdict == "skip")
        && snap
            .zone_verdicts
            .iter()
            .any(|z| matches!(z.verdict.as_str(), "run" | "run_extended"));
}

/// Smart-morning target_start epoch for the next morning that hasn't
/// already passed. Returns 0 when location is unset or sunrise can't be
/// computed (polar latitudes on the date in question), matching the
/// snapshot's default sentinel.
/// The next morning this yard both can and MAY water.
///
/// This was purely astronomical: today's sunrise-derived start, else
/// tomorrow's. It had no idea restrictions existed, so on a Sunday
/// evening it published Monday for a yard whose district allows Thursday
/// and Sunday. The morning gate caught it and skipped, so nothing
/// watered illegally, but the time on screen was a day the operator may
/// not use, and it could not express Thursday at all because it advanced
/// exactly one day.
///
/// It is a thin caller of engine::schedule::next_run now, so the module
/// that decides whether we may water also decides when we next will.
/// The snapshot's three next-run fields from one engine answer: the
/// epoch (0 for the no-answer cases, the sentinel the wire has always
/// carried), which case it is, and the day offset.
fn set_next_run(
    snap: &mut IrrigationSnapshot,
    policy: &WateringPolicy,
    watered: &[crate::engine::clock::CivilDay],
    fc: &crate::forecast::snapshot::ForecastSnapshot,
    now_epoch: i64,
) {
    use crate::engine::schedule::NextRun;
    use crate::model::NextRunState;
    let nr = compute_next_run(policy, &snap.zones, watered, fc, now_epoch);
    snap.restriction_allowed_days = allowed_weekdays(policy, watered, now_epoch);
    snap.next_run_epoch = nr.start_epoch();
    snap.next_run_state = match &nr {
        NextRun::At { .. } => NextRunState::At,
        NextRun::NoLegalDay { .. } => NextRunState::NoLegalDay,
        NextRun::NoSunrise { .. } => NextRunState::NoSunrise,
        NextRun::NoLocation => NextRunState::NoLocation,
    };
    snap.next_run_day_offset = match &nr {
        NextRun::At { day, .. } => policy.calendar.date_of(now_epoch).and_then(|today| {
            day.naive()
                .signed_duration_since(today.naive())
                .num_days()
                .try_into()
                .ok()
        }),
        _ => None,
    };
    if !snap.water_plan.is_empty() && matches!(nr, NextRun::At { .. }) {
        if let Some(day) = snap.water_plan.iter().find(|day| {
            day.start_epoch.is_some_and(|start| start > now_epoch)
                && day.zones.iter().any(|zone| zone.planned_seconds > 0)
        }) {
            snap.next_run_epoch = day.start_epoch.unwrap_or(0);
            snap.next_run_day_offset = Some(day.day_offset);
            snap.next_run_total_minutes = day
                .zones
                .iter()
                .map(|z| f64::from(z.planned_seconds))
                .sum::<f64>()
                / 60.0;
        } else {
            snap.next_run_epoch = 0;
            snap.next_run_day_offset = None;
            snap.next_run_total_minutes = 0.0;
            snap.next_run_state = NextRunState::NoWaterPlanned;
        }
    }
}

/// The weekdays the restrictions allow over the coming fortnight, for
/// the surfaces that name them. None when no restriction is configured.
fn allowed_weekdays(
    policy: &WateringPolicy,
    watered: &[crate::engine::clock::CivilDay],
    now_epoch: i64,
) -> Option<Vec<u8>> {
    if policy.restrictions.iter().all(|r| !r.enabled) {
        return None;
    }
    let cal = policy.calendar;
    let today = cal.date_of(now_epoch)?;
    let mut allowed: Vec<u8> = Vec::new();
    let mut day = today;
    for _ in 0..14 {
        let permitted = !crate::engine::restrictions::evaluate_for(
            crate::engine::clock::DecisionTime::Day(day),
            &policy.restrictions,
            policy.address_parity,
            watered,
            None,
        )
        .skip;
        if permitted {
            let wd = day.weekday().num_days_from_sunday() as u8;
            if !allowed.contains(&wd) {
                allowed.push(wd);
            }
        }
        day = day.succ()?;
    }
    allowed.sort_unstable();
    Some(allowed)
}

fn compute_next_run(
    policy: &WateringPolicy,
    zones: &[crate::model::ZoneState],
    watered: &[crate::engine::clock::CivilDay],
    fc: &crate::forecast::snapshot::ForecastSnapshot,
    now_epoch: i64,
) -> crate::engine::schedule::NextRun {
    // True wall time of the sequence (runs + soak gaps + preambles,
    // interleave aware), from the same planner the dispatcher's window
    // math uses, so the displayed next-run time and the actual dispatch
    // agree.
    let site = crate::engine::sunrise::Site::new(
        policy.location,
        crate::engine::sequence::wall_seconds(
            &policy.zone_agronomy,
            zones,
            policy.soak_minutes,
            policy.interleave_cycles,
            policy.duration_quantum_s,
        ),
    );
    let cal = policy.calendar;
    let Some(now) = cal.at(now_epoch) else {
        // No representable instant: nothing to plan from.
        // No representable instant: nothing to plan from, which is the
        // "no location" answer's shape on the wire (epoch 0).
        return crate::engine::schedule::NextRun::NoLocation;
    };
    crate::engine::schedule::next_run(
        cal,
        now,
        site,
        &policy.restrictions,
        policy.address_parity,
        watered,
        fc,
        policy.skip_rules.min_temp_f,
        crate::engine::schedule::DEFAULT_HORIZON_DAYS,
    )
}

/// Today's weekday (Sun=0..Sat=6) in the deployment's calendar, from the
/// tick when it exists and from the calendar at `now_epoch` otherwise.
fn weekday_of(policy: &WateringPolicy, tick: Option<crate::engine::Tick>, now_epoch: i64) -> u8 {
    use chrono::Datelike;
    tick.map(|t| t.weekday().num_days_from_sunday() as u8)
        .or_else(|| {
            policy
                .calendar
                .local_date(now_epoch)
                .map(|d| d.weekday().num_days_from_sunday() as u8)
        })
        .unwrap_or(0)
}

/// Assemble the `IrrigationSnapshot` from what the IO shell fetched.
///
/// The HA path passes HA `/api/states` as `map`; the native path passes
/// an empty map and the shell then overlays what its controllers report.
/// Everything read from a store or a database arrives in `AssemblyInput`,
/// already fetched, and the one instant the whole pass describes is
/// `now_epoch`. Decision logic is the shared `apply_engine`, so the
/// verdict never depends on the source.
pub fn assemble(input: AssemblyInput<'_>) -> IrrigationSnapshot {
    let AssemblyInput {
        map,
        forecast: fc_in,
        tempest: tempest_in,
        current_weather,
        field_sources,
        rain_owner,
        rain_today_owner,
        zones,
        zone_runtime,
        watering_policy,
        scripts,
        balance,
        control,
        restart_reasons,
        now_epoch,
        prefetched,
    } = input;
    // ONE instant for this whole pass, minted by the shell. Across local
    // midnight this is what keeps the restriction cap, the verdict and
    // the day-of-year the crop coefficient uses on the same day.
    let tick = crate::engine::Tick::at(watering_policy.calendar, now_epoch);
    let fc_in = match tick {
        Some(t) => std::sync::Arc::new(fc_in.for_day(t.calendar(), t.day())),
        None => fc_in,
    };
    let tick_epoch = now_epoch;
    let watered_days = watered_days(balance);

    let mut snap = IrrigationSnapshot {
        last_refresh_epoch: now_epoch,
        ha_reachable: true,
        restart_required: !restart_reasons.is_empty(),
        restart_reasons,
        tempest_last_seen_epoch: tempest_in.last_packet_epoch,
        // Live local-station serial (empty on cloud-only installs), so the
        // verdict-strip freshness pill knows whether a station exists at all
        // before it can call one "stale".
        station_serial: tempest_in.station_serial.clone(),
        forecast_last_seen_epoch: fc_in.last_refresh_epoch,
        // Household display-unit default, copied verbatim from config (mirror of
        // the per-zone photo_url copy). Display-plumbing only; the engine never
        // reads it. Default config -> Units::Imperial, so this is a no-op for
        // the default deployment.
        units: watering_policy.units,
        // Per-field provenance: which source currently owns each headline
        // reading (keyed by WeatherField name), so the UI can label "Wind:
        // Tempest" and the source picker shows the live owner. Empty until a
        // source has written a field.
        field_sources,
        ..Default::default()
    };
    snap.current_weather = Some(current_weather);

    // Evaluate watering restrictions once per refresh. The verdict feeds
    // skip-logic via Inputs.watering_restrictions below; the cap (when
    // a rule limits run length) tightens each zone's max_duration_s at
    // the two compute sites further down. Configured-timezone clock: hour
    // windows and odd/even parity are regulatory LOCAL rules, and the
    // container clock (UTC on the common Docker setup) evaluated them
    // against the wrong wall time.
    let restriction_verdict = crate::engine::restrictions::evaluate(
        crate::engine::clock::DecisionTime::at(watering_policy.calendar, now_epoch),
        &watering_policy.restrictions,
        watering_policy.address_parity,
    );
    let restriction_cap_seconds: Option<u32> = restriction_verdict
        .max_minutes_cap
        .map(|m| m.saturating_mul(60));
    // Today's weekday (Sun=0..Sat=6 per chrono::Weekday::num_days_from_sunday)
    // for per-zone manual-override gating below, in the CONFIGURED timezone: a
    // UTC container flips the weekday at evening local time, which shifted the
    // override day for any tz west of UTC.
    let today_weekday: u8 = weekday_of(watering_policy, tick, now_epoch);

    // next_run_epoch is computed below (after the per-zone planned
    // durations are known) from LocalSky's own smart-morning anchor
    // (sunrise - 15min - sequence_total). The IU bridge was the prior
    // source; it was stripped in the 2026-05-26 cutover.
    snap.iu_enabled = false;
    snap.iu_suspended = false;

    // Master enable + water level, from the operator's controller integration
    // in HA (entity prefix configurable; default "opensprinkler").
    let sp = sprinkler_prefix(watering_policy);
    snap.master_enable = state_eq(&map, &format!("switch.{sp}_enabled"), "on");
    // None when the entity is missing/unavailable: the old unwrap_or(0.0)
    // published "Water level 0%" (reads as watering fully suppressed) for a
    // sensor that simply does not exist. On this path a present entity IS the
    // capability signal; the HA refresh loop then LATCHES capability across
    // ticks (see spawn_refresher) so a transient unavailable read cannot
    // retract the manifest descriptor.
    snap.water_level_pct = state_f64(&map, &format!("sensor.{sp}_water_level"));
    snap.water_level_capable = snap.water_level_pct.is_some();

    // Vacation pause + one-day override, from LocalSky's own store on both
    // deployment paths. The Home Assistant helpers that once held them are
    // not read (0.9.0); with no persistence database there is no pause,
    // and POST /action says so with a 503 rather than pretending.
    snap.pause_until_epoch = control.map(|c| c.pause_until_epoch).unwrap_or(0);
    // Already expired against the local date by the store, so a one-day
    // override cannot outlive the day it was set on.
    snap.override_tomorrow = control
        .map(|c| c.override_tomorrow.clone())
        .unwrap_or_else(|| "none".to_string());
    // The sticky global and per-zone overrides are read from LocalSky's
    // own store on BOTH deployment paths.
    //
    // They never had a Home Assistant helper: `POST /action` wrote them to
    // sqlite on every source, while this builder reported "auto" on the
    // Home Assistant path, so a Skip or Force set from the Override panel
    // there was stored, shown as set, and never acted on. The panel and
    // the engine disagreed about the operator's own instruction. Fixed
    // deliberately as its own release-notes line: an override set on a
    // Home Assistant install before 0.9.0 takes effect at the upgrade,
    // and a Force is the first rung of the ladder, so the note says to
    // check the panel.
    let sticky_from_store = control;
    snap.global_override = sticky_from_store
        .map(|c| c.global_override.clone())
        .unwrap_or_else(|| "auto".to_string());
    // True when the pause and override controls will land somewhere: they
    // write LocalSky's own store, so exactly when one is mounted.
    snap.override_helpers_present = control.is_some();
    // The migration record, for the notice. Empty on every standalone install
    // and on a Home Assistant install before the pass runs.
    snap.ha_adoption = watering_policy.ha_adoption.clone();
    // Whether the four controls have a sink at all. Same condition the
    // planner gets: a control state exists only where a control store does.
    // The notice cannot work this out from the records, because a control
    // that DEFERRED (present, holding unavailable) is missing from them for a
    // completely different reason.
    snap.controls_persisted = control.is_some();

    // Reference ET already includes atmospheric demand. Crop use is
    // ET0 × Kc; the legacy wire multiplier remains neutral for API clients.
    // Per-zone state. Every number here is LocalSky's own: no zone field
    // is read from a Home Assistant entity, on either deployment path.
    // The soil deficit's producer is the soil model's evidence replay
    // (`apply_soil_schedule`, below): it fills `bucket_mm` for every
    // zone with agronomy config once the budget rows exist. Here it
    // starts absent, and it STAYS absent for zones with no agronomy
    // (env-var installs), never a fabricated 0.0. Today's run length
    // comes from the allocator rows (weekly, or soil-swapped) on both
    // paths. heat_mult is the global forecast multiplier; capture_eff
    // starts as the soil projection's fixed constant and
    // `apply_soil_schedule` overwrites it with the configured value on
    // soil-governed zones.
    let today_doy = { tick.map(|t| t.ordinal()).unwrap_or(1) };
    let site_lat = watering_policy.location.0;
    snap.zones = zones
        .iter()
        .map(|zone| {
            let slug = zone.slug.as_str();
            let running_id = format!("binary_sensor.{sp}_{slug}_station_running");
            let running = state_on_off(&map, &running_id);
            // Pre-plan placeholder; `apply_soil_schedule` publishes the
            // replayed deficit (negative = needs water) for every zone
            // with agronomy config. Absent, not zero, until then and on
            // zones no model can derive a bucket for.
            let bucket_mm: Option<f64> = None;
            // Kc from the native species catalog for the zone's configured
            // species, hemisphere aware. Previously the Smart Irrigation
            // entity's `multiplier` attribute, defaulting to 1.0 whenever
            // the entity was absent (which is always, on a standalone
            // install).
            // A zone with no agronomy config (unconfigured install) keeps
            // the neutral 1.0 the entity read used to fall back to.
            let kc = watering_policy
                .zone_agronomy
                .get(slug)
                .map(|a| crate::engine::kc_at_doy_lat(a.species, today_doy, site_lat))
                .unwrap_or(1.0);
            // Throughput + max-duration resolve from LocalSky's config
            // (localsky.toml zone block -> sprinkler_catalog default by
            // sprinkler_type, or precip_rate_mm_hr override when measured).
            let rt = zone_runtime
                .get(slug)
                .copied()
                .unwrap_or_else(ZoneRuntime::fallback);
            let throughput_mm_hr = rt.throughput_mm_hr;
            // Apply the active watering restriction cap (if any) on top of
            // the per-zone safety ceiling. The tighter of the two wins so
            // a regulatory "no more than 60 min per zone" rule overrides a
            // bigger operator-set ceiling.
            let max_dur = match restriction_cap_seconds {
                Some(c) => rt.max_duration_s.min(c),
                None => rt.max_duration_s,
            };
            // Today's run length comes from the weekly-budget allocator,
            // the one model that governs dispatch. It is applied in
            // `apply_budget_plan` below, once `snap.water_budgets` exists;
            // `scheduled_seconds` and `cap_binding` are pre-plan
            // placeholders that it overwrites. `raw_seconds` was the Smart
            // Irrigation bucket formula and has no producer left, so it
            // stays 0 and nothing renders it.
            let raw_seconds = 0u32;
            // Which Override schedules suppress smart dispatch for this
            // zone, so the UI can say so instead of showing a silent zero.
            let smart_suppressed = crate::scheduler::manual::override_suppression(
                &watering_policy.manual_schedules,
                slug,
                today_weekday,
            );
            let planned = 0u32;
            let math = Some(crate::model::ZoneMath {
                bucket_mm,
                kc,
                throughput_mm_hr,
                heat_mult: 1.0,
                // The fixed soil-projection constant on weekly-governed
                // zones (matches compute_soil_forecasts
                // CAPTURE_EFFICIENCY); `apply_soil_schedule` overwrites
                // it with the configured engine.capture_efficiency on
                // soil-governed zones, where the refill division reads it.
                capture_eff: 0.70,
                raw_seconds,
                max_duration_seconds: max_dur,
                scheduled_seconds: planned,
                cap_binding: false,
            });
            ZoneState {
                name: zone.display_name.clone(),
                slug: zone.slug.clone(),
                // Sticky per-zone override from the native control surface;
                // "auto" when unset, and on the Home Assistant path, where
                // this read is inert for the same reason the global one above
                // is.
                override_mode: sticky_from_store
                    .and_then(|c| c.zone_overrides.get(&zone.slug))
                    .cloned()
                    .unwrap_or_else(|| "auto".to_string()),
                hex: String::new(), // Populated in Phase 3 from device_registry if needed.
                running: running.unwrap_or(false),
                // Only an explicit on/off response certifies HA valve state.
                // Native later supplies the controller's own evidence.
                running_known: running.is_some(),
                // No device observation timestamp is supplied by this path.
                // The native path uses the controller's observation time.
                running_observed_epoch: None,
                ledger_running: false,
                controller_id: None,
                throughput_mm_hr: Some(throughput_mm_hr),
                // No producer. Nothing summarizes per-zone valve-open
                // seconds since local midnight on either path, so this is
                // absent rather than a hardcoded 0.0 printed beside a hold
                // line naming the inches already applied this week.
                today_run_minutes: None,
                bucket_mm,
                smart_suppressed,
                planned_run_seconds: planned,
                // The latest completed watering event's end, from the
                // per-tick runs evidence. This is what makes the
                // balance's min-interval spacing real on live paths (it
                // was hardcoded 0 for two releases, so spacing never
                // fired outside demo).
                last_run_epoch: balance
                    .and_then(|b| b.per_zone.get(slug))
                    .map(|e| e.last_run_epoch)
                    .unwrap_or(0),
                math,
                // photo_url is read by the dashboard from /api/config on
                // mount and joined to each zone by slug. Kept None here so
                // the snapshot remains a pure runtime-state object.
                photo_url: None,
                // Per-zone verdict is back-filled by apply_engine (which
                // runs decide_per_zone) before the snapshot is published;
                // None only until that pass. The smart-morning dispatcher
                // enforces these at dispatch time.
                verdict: None,
                // Native soil temp/EC/battery merged in after the gateway poll
                // resolves them (resolve_soil_extras, below).
                soil_temp_f: None,
                soil_ec: None,
                soil_battery_pct: None,
                // Verdict-independent suspect-probe flag, back-filled by
                // apply_engine (suspect_probes) before the snapshot publishes.
                soil_suspect: None,
            }
        })
        .collect();
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;

    // Forecast block. Aggregates the live station and the in-process
    // ForecastStore (7-day + 48h + past days) into one struct the UI can
    // render directly. Every forecast figure is LocalSky's own reading;
    // the legacy sensor.open_meteo_* REST sensors are not consulted.
    let fc = fc_in;
    // Today's modelled rain, by the deployment's calendar. Positional
    // daily[0] is the fallback with no tick, which is the old behavior.
    let rain_today_om = match tick {
        Some(t) => fc.today_precip_in_at(t.calendar(), t.day()),
        None => fc.today_precip_in(),
    };
    // The next run (the day's window under this forecast, the restrictions
    // and the week's watered days) is decided once, by `Finalize::apply`,
    // after the shell overlays what the controllers report.
    let tempest = tempest_in;

    // The deployment's IANA timezone for the client's local formatting:
    // the policy's name (configured, else inferred from the location at
    // boot), with the forecast provider's `timezone=auto` name as the
    // fallback before the policy knows one. Empty string -> the client
    // falls back to browser-local.
    snap.timezone = watering_policy
        .timezone_name
        .clone()
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| fc.timezone.clone());

    // Raw 3-day rain outlook for the display bar, from the same live
    // forecast the weighted bar, the strip and the engine read. Display
    // only: the engine's 3-day rule uses the probability-weighted total.
    let rain_3day = tick.and_then(|t| fc.future_n_day_precip_in(3, t.calendar(), t.day()));

    // Rain comes from the in-process Tempest listener, which integrates
    // the per-minute rain packets into a true daily total. The HA
    // WeatherFlow `precipitation` entity is the rain in the LAST
    // REPORTING MINUTE, not a daily accumulation; reading it as one
    // capped storm days at ~0.05" (3 in/h over one minute) and let the
    // engine schedule a full run the morning after heavy rain. Recency
    // gated by the daily field's own observation-grade owner. Regional
    // forecast rain remains a separate expected amount.
    let valid_rate =
        tempest.rain_intensity_in_hr.is_finite() && tempest.rain_intensity_in_hr >= 0.0;
    let observed_rain_nature = rain_owner
        .as_ref()
        .filter(|owner| owner.is_fresh && valid_rate)
        .map(|owner| owner.nature)
        .filter(|nature| *nature != crate::model::RainNature::Model);
    let rain_live = observed_rain_nature == Some(crate::model::RainNature::Measured)
        && rain_owner.as_ref().is_some_and(|owner| owner.is_live);
    // Daily totals need their own fresh observation-grade owner. A fresh
    // temperature packet cannot certify a model-filled or yesterday's total.
    let measured_today = matches!(
        classify_rain_today_source(rain_today_owner.as_ref()),
        "gauge" | "radar"
    ) && watering_policy
        .calendar
        .local_date(tick_epoch)
        .is_some_and(|day| {
            chrono::Datelike::num_days_from_ce(&day) == tempest.rain_today_day_ordinal
        })
        && tempest.rain_in_today.is_finite()
        && tempest.rain_in_today >= 0.0;
    let rain_today_station = if measured_today {
        tempest.rain_in_today
    } else {
        0.0
    };
    let rain_nature = observed_rain_nature.unwrap_or(crate::model::RainNature::Model);
    let rain_intensity = if observed_rain_nature.is_some() {
        Some(tempest.rain_intensity_in_hr)
    } else {
        fc.next_n_hours_precip_in(1, tick_epoch)
    };
    let rain_type = if rain_live {
        match tempest.precip_type {
            1 => "rain".to_string(),
            2 => "hail".to_string(),
            _ => "none".to_string(),
        }
    } else if rain_intensity.is_none() {
        "unknown".to_string()
    } else if rain_intensity.is_some_and(|rate| rate > 0.0) {
        "rain".to_string()
    } else {
        "none".to_string()
    };

    // NOT blended any more. `rain_today_used` is what a gauge caught, and
    // the modelled total travels beside it so the engine can hold on
    // either one and say which. Blending them with `max` is what let a
    // forecast be reported as "Already wet".
    let rain_today_used = rain_today_station;
    let rain_today_forecast = rain_today_om;
    // Live "now" readings. Prefer the in-process Tempest listener while
    // its packets are fresh (recency-gated: a station that stopped
    // reporting hours ago must not keep driving freeze/wind gates).
    // When stale or absent, fall back to the current-hour forecast and
    // mark the inputs degraded; with no forecast either, mark them
    // unavailable so the engine fails safe (skip, never a phantom run
    // on fabricated 70 °F / 0 mph defaults).
    let now_epoch = tick_epoch;
    let (temp_now, wind_now, humidity_now, live_readings) = resolve_current_conditions(
        &["air_temp_f", "wind_mph", "rh_pct"].map(|field| {
            snap.current_weather
                .as_ref()
                .and_then(|samples| samples.get(field))
                .cloned()
        }),
        fc.hourly.first(),
        now_epoch,
    );
    if live_readings != LiveReadings::Station {
        tracing::debug!(
            ?live_readings,
            tempest_last_packet_epoch = tempest.last_packet_epoch,
            "live station readings unavailable or stale; inputs degraded"
        );
    }

    // Tomorrow's rain, addressed by day.
    //
    // Read positionally this was daily[1], which on a stale snapshot is
    // TODAY. It feeds the tomorrow-rain skip gate, so a run could be held
    // for rain already falling or already past. When no row covers
    // tomorrow, the amount stays unavailable; a positional row cannot
    // certify a different civil day.
    let (rain_tomorrow_om_in, rain_tomorrow_prob) = match tick {
        Some(t) => fc
            .tomorrow_precip_with_prob_in_at(t.calendar(), t.day())
            .map(|(amount, probability)| (Some(amount), probability))
            .unwrap_or((None, None)),
        None => fc.tomorrow_precip_with_prob_in(),
    };
    let rain_3day_weighted =
        tick.and_then(|t| fc.future_n_day_weighted_precip_in(3, t.calendar(), t.day()));
    let rain_7day_weighted =
        tick.and_then(|t| fc.future_n_day_weighted_precip_in(7, t.calendar(), t.day()));
    let rain_next_4h = fc.next_n_hours_precip_in(4, tick_epoch);
    // A hard "Already wet" hold uses only actual measured rain. Regional
    // archive estimates remain optional balance evidence, never observations.
    // A NWS provider without past_daily therefore needs no synthetic history.
    let rain_observed_recent = rain_today_used + prefetched.observed_past_gauge_in;
    // Option end-to-end: None = no hourly forecast window. The engine's
    // overnight-freeze gate keys applicability off is_some(), so a real
    // sub-zero low is no longer confused with "no data".
    let temp_min_24h: Option<f64> = fc.min_temp_next_24h_f();
    let temp_max_3day = fc.max_temp_next_3d_f().unwrap_or(0.0);
    // Today's wind, addressed by the day the deployment is in.
    //
    // Read positionally this was daily[0], which on a stale snapshot is
    // YESTERDAY's peak wind, and the pre-dawn window sits inside the six
    // hours before staleness is flagged.
    let wind_max_today = tick
        .map(|t| fc.wind_max_today_mph_at(t.calendar(), t.day()))
        .unwrap_or_else(|| fc.wind_max_today_mph())
        .unwrap_or(0.0);
    let wind_gust_today = fc.wind_gust_max_today_mph().unwrap_or(0.0);
    // Days since significant rain: take the MIN of the regional model's
    // counter and the station-gauge counter from forecast_observations.
    // The gauge's memory beats the regional model for hyperlocal
    // convection: a pop-up storm that soaked this yard but never showed
    // in Open-Meteo's past_daily still counts as recent rain, so the
    // heat-advisory extend can't fire the morning after a soaking.
    let days_since_rain = {
        let model_days = fc.days_since_rain_over(
            rain_today_used,
            if watering_policy.skip_rules.already_wet_in > 0.0 {
                watering_policy.skip_rules.already_wet_in
            } else {
                crate::engine::WET_DAY_IN
            },
        );
        let observed_days = prefetched.observed_days_since_rain;
        match observed_days {
            Some(obs) => model_days.min(obs),
            None => model_days,
        }
    };

    let rain_tomorrow_used = rain_tomorrow_om_in;

    let heat_index_now = (live_readings != crate::engine::skip_rules::LiveReadings::Unavailable)
        .then(|| heat_index_f(temp_now, humidity_now));
    // 3-day peak heat index computed PER DAY (each day's high temp paired with
    // THAT day's humidity) instead of the old heat_index_f(temp_max_3day,
    // humidity_now): that pairing of the 3-day MAX temp with the CURRENT (often
    // saturated post-rain) humidity overshoots the Rothfusz regression to a
    // physically-impossible value (~147°F) that inflated both the ET heat
    // multiplier and the hero "HEAT INDEX 3D" display. Missing forecast pairs
    // remain unknown, rather than borrowing the current observation.
    let heat_index_3day = fc.max_heat_index_n_day(3);
    // VPD remains an advisory atmospheric measurement. ET0 already contains
    // the weather response, so neither VPD nor human heat index scales it again.
    let heat_mult = 1.0;

    // Source-agnostic reference ET0 (mm): provider full-day forecast > Open-Meteo
    // HA sensor > native compute from the forecast > station accumulator; None
    // when nothing real resolved (see resolve_et0_today_mm). Display +
    // soil-projection only (the live decision bucket is HA-sourced).
    let et0_lat = watering_policy.location.0;
    let et0_base_doy = {
        // Day-of-year in the CONFIGURED timezone: the UTC ordinal is tomorrow's
        // from evening local time onward, skewing the Hargreaves solar term.
        tick.map(|t| t.ordinal()).unwrap_or(1)
    };
    // The operator's configured elevation, which the ET0 path collected
    // and then discarded at the one place it matters.
    let et0_elev = watering_policy.elevation_m;
    let et0_base_date = watering_policy
        .calendar
        .local_date(now_epoch)
        .unwrap_or_default();
    let et0_today_mm =
        resolve_et0_today_mm(tempest.et0_today, &fc, et0_lat, et0_base_doy, et0_elev);

    let (temp_max_today, temp_min_today, humidity_mean_today) = resolve_today_range(&fc);

    let forecast = Forecast {
        rain_today_tempest_in: rain_today_station,
        rain_today_om_in: rain_today_om,
        // Provenance for the rain comparison cards: the live station's label and
        // the forecast provider's label (real sources, not hardcoded names).
        station_source_label: if tempest.source_label.is_empty() {
            "Station".to_string()
        } else {
            tempest.source_label.clone()
        },
        forecast_source_label: if fc.source_label.is_empty() {
            "Forecast".to_string()
        } else {
            fc.source_label.clone()
        },
        rain_intensity_in_hr: rain_intensity,
        rain_type,
        // TRUE only when a LIVE source owns the current-rain reading this refresh
        // (rain_live, gated on rain_live_epoch). On cloud-only / station-stale the
        // intensity/type above are an Open-Meteo forecast FILL, not an
        // observation, so the dashboard's "RAINING NOW" badge must not present
        // them as live observed rain (T3).
        rain_is_live: rain_live,
        // HONEST rain nature derived by the 3-tier gate above: Measured when a live
        // LAN gauge (or a fresh NWS observation) owns the rain rate, RadarQpe when
        // a fresh NOAA MRMS radar fill owns it, else Model (the forecast fallback).
        // The dashboard rain badge keys on THIS (not rain_is_live alone) and never
        // says "live" on a Model nature.
        rain_nature,
        rain_tomorrow_in: rain_tomorrow_used,
        rain_3day_in: rain_3day,
        eto_today_mm: et0_today_mm,
        eto_tomorrow_mm: forecast_day_et0_mm(&fc, 1, et0_lat, et0_base_date, 0.0, et0_elev),
        eto_3day_avg_mm: {
            // Per-day values follow the same ladder as forecast_day_et0_mm
            // (provider daily ET0 in mm > native Hargreaves) so the 3-day
            // average agrees with the today/tomorrow tiles in method + units.
            let vals: Vec<f64> = (0..3)
                .filter_map(|i| {
                    fc.daily.get(i).and_then(|d| {
                        if let Some(et0) = d.reference_et0_mm() {
                            Some(et0)
                        } else {
                            native_et0_mm(
                                d,
                                et0_lat,
                                chrono::Datelike::ordinal(
                                    &(et0_base_date + chrono::Duration::days(i as i64)),
                                ) as u16,
                                et0_elev,
                            )
                        }
                    })
                })
                .collect();
            if vals.is_empty() {
                0.0
            } else {
                vals.iter().sum::<f64>() / vals.len() as f64
            }
        },
        temp_max_today_f: temp_max_today,
        temp_min_today_f: temp_min_today,
        wind_max_today_mph: tick
            .map(|t| fc.wind_max_today_mph_at(t.calendar(), t.day()))
            .unwrap_or_else(|| fc.wind_max_today_mph()),
        wind_gust_today_mph: wind_gust_today,
        humidity_mean_today_pct: humidity_mean_today,

        rain_3day_weighted_in: rain_3day_weighted,
        rain_7day_weighted_in: rain_7day_weighted,
        rain_next_4h_in: rain_next_4h,
        rain_tomorrow_prob_pct: rain_tomorrow_prob,
        temp_min_24h_f: temp_min_24h,
        temp_max_3day_f: fc.max_temp_next_3d_f(),
        humidity_now_pct: (live_readings != crate::engine::skip_rules::LiveReadings::Unavailable)
            .then_some(humidity_now),
        heat_index_now_f: heat_index_now,
        heat_index_max_3day_f: heat_index_3day,
        heat_multiplier: heat_mult,
        days_since_significant_rain: days_since_rain,
        // Extended model context (all 0 when the provider lacks the series).
        // ET spent stays MODEL-derived (full-day minus the remaining hourly
        // curve): the bus et0_today field's contract is the FULL-DAY figure
        // (no adapter or mapping declares accumulator semantics), so treating
        // a live-owned value as "spent" would charge a mapped full-day sensor
        // at dawn. A dedicated accumulator field can revisit this.
        eto_spent_today_mm: fc.eto_spent_today_mm(now_epoch, watering_policy.calendar),
        vpd_now_kpa: fc.vpd_now_and_max_today(watering_policy.calendar).0,
        vpd_max_today_kpa: fc.vpd_now_and_max_today(watering_policy.calendar).1,
        // First hour WITH a value (a non-OM owner's window can start in the
        // past, before graft coverage; see vpd_now_and_max_today).
        soil_temp_6cm_now_f: fc
            .hourly
            .iter()
            .map(|h| h.soil_temp_6cm_f)
            .find(|v| *v > 0.0)
            .unwrap_or(0.0),
        soil_moisture_3_9_now_vwc: fc
            .hourly
            .iter()
            .map(|h| h.soil_moisture_3_9_vwc)
            .find(|v| *v > 0.0)
            .unwrap_or(0.0),
        // Last NONZERO reading, not .last(): a non-OM owner's hourly window
        // can extend past the donor's 48h graft coverage, leaving trailing
        // zeros that would fake a dry-down to 0%.
        soil_moisture_3_9_in48h_vwc: fc
            .hourly
            .iter()
            .rev()
            .map(|h| h.soil_moisture_3_9_vwc)
            .find(|v| *v > 0.0)
            .unwrap_or(0.0),
    };

    // Native per-zone soil extras (temp/EC/battery) from the gateway poll.
    // Merge them onto the published zones[] and derive the frost gate's yard
    // min/max soil temperature natively, no dependency on an HA soil-temp
    // aggregate (which used to come from the ecowitt2mqtt sidecar).
    let soil_extras = &prefetched.soil_extras;
    for z in &mut snap.zones {
        if let Some(e) = soil_extras.iter().find(|e| e.slug == z.slug) {
            z.soil_temp_f = e.temp_f;
            z.soil_ec = e.ec;
            z.soil_battery_pct = e.battery_pct;
        }
    }
    let soil_temps: Vec<f64> = soil_extras.iter().filter_map(|e| e.temp_f).collect();
    let soil_temp_yard_min_f = soil_temps.iter().copied().reduce(f64::min);
    let soil_temp_yard_max_f = soil_temps.iter().copied().reduce(f64::max);

    // Resolve each zone's live soil reading once; the engine inputs,
    // the probe-fault detector and the per-zone verdicts all consume the
    // same list. With no probe configured on any zone it is built from
    // the active zone list, so every zone still gets a verdict.
    let mut soil_zones_resolved = if watering_policy.soil_zones.is_empty() {
        unprobed_soil_zones(zones)
    } else {
        prefetched.soil_zones.clone()
    };
    // Tell the engine which zones the soil model governs, so IT applies
    // the forward-rain inertness rule rather than the refresher rewriting
    // verdicts afterwards.
    let planning_forecast_unavailable = forecast_is_stale(fc.last_refresh_epoch, now_epoch)
        || fc.planning_precip_weighted_in(24, tick_epoch).is_none();
    for z in soil_zones_resolved.iter_mut() {
        z.governed_by_soil_model = matches!(
            watering_policy.resolve_scheduling_model(&z.slug),
            crate::config::schema::SchedulingModel::Soil
        );
        z.planning_forecast_unavailable = planning_forecast_unavailable;
    }
    // Probe health: a zone with a sensor configured but no usable reading
    // silently widens the yard-wide saturation gate (it goes inapplicable
    // when any zone lacks a reading). Name the dead hardware on the
    // snapshot so the UI, /api/health, and push can surface it.
    snap.soil_probe_faults = prefetched.soil_probe_faults.clone();

    // The forecast store re-emits its last-good payload during an
    // Open-Meteo outage (last_refresh_epoch only advances on a successful fetch),
    // so age past the trust horizon means the forward-looking rain inputs are
    // untrustworthy. This marks the trace degraded and suppresses the predictive
    // rain SKIPs so a frozen "rain coming" cannot starve the yard.
    let forecast_stale = forecast_is_stale(fc.last_refresh_epoch, now_epoch);

    // The same site the dispatcher plans against, so the preview, the
    // morning it previews, and the wind gate cannot disagree about when
    // the yard waters.
    let strip_site = crate::engine::sunrise::Site::new(
        watering_policy.location,
        crate::engine::sequence::wall_seconds(
            &watering_policy.zone_agronomy,
            &snap.zones,
            watering_policy.soak_minutes,
            watering_policy.interleave_cycles,
            watering_policy.duration_quantum_s,
        ),
    );
    // The window the yard waters in today: pre-dawn unless the forecast
    // puts the pre-dawn hours below the freeze threshold and a later hour
    // clears it. One function decides this for the verdict, the strip,
    // next_run and the dispatcher, so they cannot disagree.
    let freeze_f = watering_policy.skip_rules.min_temp_f;
    let today_window = tick.and_then(|t| {
        crate::engine::dispatch_window::choose(
            t.day(),
            strip_site,
            t.calendar(),
            &fc,
            freeze_f,
            crate::engine::dispatch_window::Rules {
                restrictions: &watering_policy.restrictions,
                parity: watering_policy.address_parity,
                watered: &watered_days,
            },
        )
    });
    // Forecast wind across the minutes the yard will actually be out in.
    // The daily peak is an afternoon figure; the run finishes before
    // sunrise. None when no window is knowable, and the gate then falls
    // back to the day's peak rather than a calm zero.
    let wind_window_max = today_window.and_then(|w| fc.wind_max_over_window_mph(w.start, w.finish));
    snap.today_window = tick
        .zip(today_window)
        .map(|(t, w)| crate::engine::dispatch_window::PlannedWindow::of(t.day(), w));
    let mut inputs = Inputs {
        restart_required: snap.restart_required,
        // The deployment's calendar, resolved here where the configured
        // timezone is known. Watering restrictions are a legal question
        // about the operator's wall clock, so the engine is handed the
        // calendar rather than reading one from the process.
        calendar: watering_policy.calendar,
        temp_now_f: temp_now,
        wind_now_mph: wind_now,
        rain_today_in: rain_today_used,
        rain_today_forecast_in: rain_today_forecast,
        rain_intensity_now_in_hr: rain_intensity,
        // Honest nature of the live rain rate (same 3-tier derivation that fills
        // the snapshot's rain_nature): Measured / RadarQpe gate a HARD rain_now
        // skip; Model only a demotable soft skip. Carried so the engine's
        // observation-grade-only hard-skip rule reads the merge owner's truth.
        rain_nature,
        humidity_now_pct: humidity_now,

        forecast_in: rain_tomorrow_used,
        rain_tomorrow_prob_pct: rain_tomorrow_prob,
        rain_3day_weighted_in: rain_3day_weighted,
        rain_7day_weighted_in: rain_7day_weighted,
        rain_next_4h_in: rain_next_4h,
        rain_observed_recent_in: rain_observed_recent,
        forecast_stale,
        wind_max_today_mph: wind_max_today,
        wind_window_max_mph: wind_window_max,
        watered_days: watered_days.clone(),
        run_window: today_window.map(|w| w.kind).unwrap_or_default(),
        window_min_temp_f: today_window.and_then(|w| w.min_temp_f),
        temp_min_24h_f: temp_min_24h,
        temp_max_3day_f: temp_max_3day,
        // Forecast-derived per-day 3-day peak heat index (corrected pairing).
        heat_index_max_3day_f: heat_index_3day.unwrap_or(0.0),
        days_since_significant_rain: days_since_rain,

        // The three thresholds: Settings is the only source, so the
        // dashboard slider and the Settings page edit one number.
        max_wind_mph: watering_policy.skip_rules.max_wind_mph,
        min_temp_f: watering_policy.skip_rules.min_temp_f,
        rain_skip_in: watering_policy.skip_rules.rain_skip_in,

        // Per-zone soil readings + thresholds. Resolved above from each
        // zone's assigned sensor (`ha:` entity or `source:<id>:<key>`
        // channel) + ZoneConfig thresholds. None when a sensor is offline;
        // a configured unavailable probe holds its zone. Truly unbound zones
        // remain eligible for weather/model decisions without a measurement.
        soil_zones: soil_zones_resolved,
        soil_temp_yard_min_f,
        soil_temp_yard_max_f,
        frost_skip_soil_f: watering_policy.skip_rules.frost_skip_soil_f,

        // Provenance of the live "now" readings (resolved above). The
        // ladder fails safe (skip) when Unavailable and marks the trace
        // degraded on ForecastFallback.
        live_readings,

        // The two toggles. Both had no native column at all before M0017, so
        // on a standalone install they were permanently false; both are
        // PROTECTED gates in the ladder, which made them controls that did
        // not exist. Post-adoption they read LocalSky's own store on both
        // paths.
        is_paused: control.map(|c| c.is_paused).unwrap_or(false),
        is_dry_run: control.map(|c| c.is_dry_run).unwrap_or(false),

        // Phase 4 control surfaces. Today's verdict ignores the tomorrow
        // override (is_tomorrow=false); the verdict-strip path below sets
        // it true on the [+1] cell.
        pause_until_epoch: snap.pause_until_epoch,
        when: crate::engine::clock::DecisionTime::at(watering_policy.calendar, now_epoch),
        override_tomorrow: snap.override_tomorrow.clone(),
        is_tomorrow: false,
        // Sticky overrides (native sqlite; set on snap above). The global rides
        // pre_soil; the per-zone map (auto entries dropped) rides decide_per_zone.
        global_override: snap.global_override.clone(),
        zone_overrides: snap
            .zones
            .iter()
            .filter(|z| z.override_mode != "auto")
            .map(|z| (z.slug.clone(), z.override_mode.clone()))
            .collect(),

        // Watering restrictions resolved at boot from localsky.toml and
        // plumbed through spawn_refresher. The skip-rule ladder uses
        // these to short-circuit the live verdict with reason
        // "Watering restriction: <name>" when an active rule blocks
        // today. The seven-day strip path (verdict_strip.rs) gets its
        // own copies from `today`.
        watering_restrictions: watering_policy.restrictions.clone(),
        address_parity: watering_policy.address_parity,
    };
    snap.water_budgets = compute_water_budgets(
        &fc,
        zone_runtime,
        watering_policy.defer_threshold_in(),
        restriction_cap_seconds,
        &budget_zones_for_active(zones, &watering_policy.budget_zones),
        balance,
        watering_policy.calendar,
        tick_epoch,
    );
    // The soil-model pass: shadow-compute the bucket for every zone with
    // agronomy config (bucket_mm's producer, plus the water_budgets soil
    // block), and swap `today_seconds` for the zones the soil model
    // governs, BEFORE apply_budget_plan so the shared downstream
    // (seasonal dial, Override zeroing, force floor, verdict multiplier)
    // applies to both producers identically on both deployment paths.
    let soil_plans = prepare_soil_schedule(
        &mut snap,
        watering_policy,
        balance,
        &fc,
        restriction_cap_seconds,
        tick,
        now_epoch,
        None,
    );
    set_soil_governance(&mut inputs, &soil_plans);
    apply_engine(
        &mut snap,
        &inputs,
        scripts,
        &watering_policy.condition_rules,
        &watering_policy.skip_rules,
    );

    snap.forecast = forecast;
    snap.seven_day_verdicts = compute_seven_day_verdicts(
        &fc,
        &inputs,
        &watering_policy.skip_rules,
        strip_site,
        watering_policy.calendar,
    );
    snap.soil_forecasts = compute_soil_forecasts(
        &fc,
        &inputs,
        &inputs.soil_zones,
        &watering_policy.soil_zones,
        &watering_policy.zone_agronomy,
        tick.map(|t| t.ordinal()).unwrap_or(1),
        watering_policy.location.0,
        watering_policy.effective_capture_efficiency(),
        // The advisory projection needs SOME daily ET to draw a curve; when
        // nothing real resolved it opts into the engine-internal constant.
        // The published eto_today_mm stays None in that case.
        et0_today_mm.unwrap_or(ENGINE_ET0_FALLBACK_MM),
    );
    apply_soil_plans(
        &mut snap,
        watering_policy,
        restriction_cap_seconds,
        tick,
        now_epoch,
        soil_plans,
    );
    // ONE dispatch pipe on BOTH paths: the allocator's rows (weekly, or
    // soil-swapped above) become planned seconds here. The Home
    // Assistant path used to size runs from a Smart Irrigation entity's
    // bucket instead, which is the read the 0.7.22 release deleted, so
    // both paths plan from `water_budgets`.
    apply_budget_plan(&mut snap, watering_policy, today_weekday);

    snap.water_plan = water_plan::project(
        &snap,
        &inputs,
        watering_policy,
        scripts,
        balance,
        &fc,
        now_epoch,
    );
    water_plan::align_strip(&mut snap);

    snap
}

/// Size every zone's run from the weekly-budget allocator's `today_seconds`,
/// then apply, in order: the seasonal trust dial (re-clamped to the cap,
/// because a >100% dial can push a capped figure back over the ceiling), an
/// Override manual schedule for today (zeroes the smart dispatch so it does
/// not run on top of the operator's own run), and the force-run floor.
/// Recomputes the snapshot's next-run rollups from the result so display and
/// dispatch cannot disagree.
pub(crate) fn apply_budget_plan(
    snap: &mut IrrigationSnapshot,
    watering_policy: &WateringPolicy,
    today_weekday: u8,
) {
    let planned_by_slug: HashMap<String, u32> = snap
        .water_budgets
        .iter()
        .map(|b| (b.zone_slug.clone(), b.today_seconds))
        .collect();
    // The allocator is the only thing that computes a real cap collision
    // now that the soil-deficit formula is gone, so the math panel's cap
    // row reads `session_capped` off the budget row instead of a flag
    // nothing sets. Without this the "shorted by the safety ceiling"
    // signal was false on every install while the panel still promised it.
    let capped_by_slug: HashMap<String, bool> = snap
        .water_budgets
        .iter()
        .map(|b| (b.zone_slug.clone(), b.session_capped))
        .collect();
    // Configured timezone, not the container's: the shell's tick.
    // Read before the mutable zone loop borrows snap. The snapshot value is
    // the gated one, so re-deriving it from `control` here would put the
    // sticky override back in force on the Home Assistant path behind the
    // gate above.
    let global_ov = snap.global_override.clone();
    for z in snap.zones.iter_mut() {
        let raw_budget = planned_by_slug.get(&z.slug).copied().unwrap_or(0);
        let max_dur = z.math.as_ref().map(|m| m.max_duration_seconds).unwrap_or(0);
        let seasonal_binds =
            seasonal_cap_binds(raw_budget, watering_policy.seasonal_adjust_pct, max_dur);
        let budget_seconds =
            seasonal_capped(raw_budget, watering_policy.seasonal_adjust_pct, max_dur);
        let override_active = crate::scheduler::manual::override_active_today(
            &watering_policy.manual_schedules,
            &z.slug,
            today_weekday,
        );
        z.planned_run_seconds = if override_active {
            0
        } else if z
            .verdict
            .as_ref()
            .is_some_and(|verdict| matches!(verdict.verdict.as_str(), "run" | "run_extended"))
        {
            // Only a completed engine permission may turn a zero budget into
            // the explicit Force floor. Missing planning data cannot revive it.
            force_run_floor(&z.override_mode, &global_ov, budget_seconds, max_dur)
        } else {
            budget_seconds
        };
        if let Some(m) = z.math.as_mut() {
            m.scheduled_seconds = z.planned_run_seconds;
            // The ceiling binds only when there IS a run and that run sits ON
            // the ceiling because something wanted more: the allocator's ideal
            // weekly session (`session_capped`), or the seasonal dial scaling
            // past it (`seasonal_binds`). Both clamps report themselves here;
            // the third, the condition-rule multiplier, reports itself in
            // `apply_verdict_multiplier` with the same predicate.
            //
            // The `planned == max_dur` term is what keeps the panel from
            // describing a run that does not exist. `session_capped` is a
            // property of the IDEAL weekly slice and stays true when today's
            // plan is zero for an unrelated reason: spacing since the last
            // session, a rain defer, budget mode off, an Override schedule.
            // Reading it alone printed "0 min (capped at 60 min)". It also
            // keeps a force-run floor honest: 5 minutes over a zero budget is
            // a floor, not a run the ceiling shortened.
            m.cap_binding = !override_active
                && max_dur > 0
                && z.planned_run_seconds == max_dur
                && (capped_by_slug.get(&z.slug).copied().unwrap_or(false) || seasonal_binds);
        }
    }
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;
}

/// The soil-model pass. For EVERY zone with agronomy config, whichever
/// model governs it, replay the trailing evidence through the pure
/// planner (`engine::soil_schedule::plan_zone`) and publish the result.
/// Two evidence-quality guards hold the pass back: a window with fewer
/// than `MIN_EVIDENCE_DAYS` evidenced days
/// (`SoilZonePlan::evidence_starved`) and a tick
/// whose runs read errored (`BalanceTick::runs_degraded`) both publish
/// ABSENCE (no bucket, no soil block) and leave the weekly allocator's
/// sizing in place for governed zones, because a replay built on
/// assumption alone fabricates a deficit. Otherwise:
/// `bucket_mm` gets its producer (negative = needs water, the field's
/// documented sign since the Smart Irrigation era) and the budget row
/// gains the additive soil block, so a weekly-model install accrues
/// shadow evidence ("would water N seconds today") with its decisions
/// untouched. Every budget row is also tagged with the model that
/// governs it.
///
/// Runs between the weekly allocator and `apply_budget_plan`: when a
/// zone resolves to the soil model, this pass swaps its row's
/// `today_seconds`/`today_reason` for the soil plan's figures, and the
/// shared downstream (seasonal dial, Override zeroing, force-run floor,
/// verdict multiplier, dispatch) then applies to both producers
/// identically, one truth for display and dispatch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn prepare_soil_schedule(
    snap: &mut IrrigationSnapshot,
    watering_policy: &WateringPolicy,
    balance: Option<&BalanceTick>,
    fc: &ForecastSnapshot,
    restriction_cap_seconds: Option<u32>,
    tick: Option<crate::engine::Tick>,
    now_epoch: i64,
    forecast_as_of: Option<i64>,
) -> Vec<(String, crate::engine::soil_schedule::SoilZonePlan)> {
    use crate::config::schema::SchedulingModel;
    use crate::engine::soil_schedule::{
        plan_zone_with_coverage, resolve_et0_days, DeferHistory, ZoneDayEvidence, ZoneSoilParams,
    };
    let budget_cfg_by_slug: HashMap<&str, &ZoneBudgetCfg> = watering_policy
        .budget_zones
        .iter()
        .map(|b| (b.slug.as_str(), b))
        .collect();
    // Bias-corrected, probability-weighted next-24h rain (mm) for the
    // defer-by-deficit gate; the capture factor applies inside the gate.
    let bias_mult = {
        use chrono::Datelike;
        let month = tick
            .map(|t| t.month())
            .or_else(|| {
                watering_policy
                    .calendar
                    .local_date(now_epoch)
                    .map(|d| d.month())
            })
            .unwrap_or(1);
        balance.map(|b| b.bias.multiplier_for(month)).unwrap_or(1.0)
    };
    let expected_24h_rain_mm = match forecast_as_of {
        Some(as_of) => fc.scenario_rain_in(now_epoch, as_of, watering_policy.calendar),
        None => fc.planning_precip_weighted_in(24, now_epoch),
    }
    .map(|amount| crate::units::in_to_mm(amount * bias_mult));
    let empty_soil = SoilTickEvidence::default();
    let soil_ev = balance.map(|b| &b.soil).unwrap_or(&empty_soil);
    // The ET0 ladder's evidence rungs resolve once for the window; the
    // per-zone fallback rung applies inside build_replay_days.
    let et0_days = resolve_et0_days(&soil_ev.dates, &soil_ev.et0_ledger, &soil_ev.et0_archive);
    let eff = watering_policy.effective_capture_efficiency();
    let site_lat = watering_policy.location.0;
    // The engine default the per-zone tags diverge from, for the model
    // chips: a chip renders only where a zone's effective model differs
    // from this baseline.
    snap.engine_scheduling_model = match watering_policy.scheduling_model {
        SchedulingModel::Weekly => "weekly",
        SchedulingModel::Soil => "soil",
    }
    .to_string();
    // Zones the soil model GOVERNS this tick, with their plans, in the
    // active-list order (the admission sort's deterministic tie-break).
    let mut governed: Vec<(String, crate::engine::soil_schedule::SoilZonePlan)> = Vec::new();
    // A failed runs read leaves the replay blind to the water the system
    // itself dispatched: applied=0 on every day would reconstruct an
    // inflated depletion for a zone that watered yesterday and can
    // re-dispatch a full refill. The degraded tick keeps its model tags
    // but publishes no buckets and swaps no governed rows; the weekly
    // allocator's sizing stands until a clean read.
    let runs_degraded = balance.is_some_and(|b| b.runs_degraded);

    // Recent 6 cm soil temperature from the forecast's hourly model, for
    // the species dormancy test. None when the provider does not model
    // soil, which reads as awake.
    let soil_temp_mean_f = fc.soil_temp_6cm_mean_f(now_epoch);
    for zi in 0..snap.zones.len() {
        let slug = snap.zones[zi].slug.clone();
        let model = watering_policy.resolve_scheduling_model(&slug);
        if let Some(b) = snap.water_budgets.iter_mut().find(|b| b.zone_slug == slug) {
            b.scheduling_model = match model {
                SchedulingModel::Weekly => "weekly",
                SchedulingModel::Soil => "soil",
            }
            .to_string();
        }
        if runs_degraded {
            continue;
        }
        // No agronomy config = no texture or species to derive a bucket
        // from: the zone stays weekly-governed (resolve_scheduling_model
        // already pins it) and its soil fields stay absent.
        let Some(agr) = watering_policy.zone_agronomy.get(&slug) else {
            continue;
        };
        let rt = watering_policy
            .zone_runtime
            .get(&slug)
            .copied()
            .unwrap_or_else(ZoneRuntime::fallback);
        let max_dur = match restriction_cap_seconds {
            Some(c) => rt.max_duration_s.min(c),
            None => rt.max_duration_s,
        };
        let bz = budget_cfg_by_slug.get(slug.as_str());
        let params = ZoneSoilParams {
            slug: slug.clone(),
            species: agr.species,
            texture: agr.soil_texture,
            root_depth_mm: agr.root_depth_mm,
            mad_pct: agr.mad_pct_override,
            latitude_deg: site_lat,
            capture_efficiency: eff,
            // The zone's own head, so a drip line is not charged a
            // fixed spray's drift losses.
            sprinkler_type: agr.sprinkler_type,
            // The modeled 6 cm soil temperature, so a planting the soil
            // says is asleep holds its bucket instead of watering to a
            // growing lawn's coefficient.
            soil_temp_f: soil_temp_mean_f,
            throughput_mm_hr: rt.throughput_mm_hr,
            max_dur_s: max_dur,
            // The operator's per-day rain clip, EXPLICIT only: an
            // inferred cap is already the TAW the bucket clamp encodes.
            explicit_rain_cap_mm: bz.and_then(|b| (!b.rain_cap_inferred).then_some(b.rain_cap_mm)),
            // The weekly delivery ceiling, EXPLICIT only: an inferred
            // 1.0 in target must never starve a sandy summer.
            explicit_weekly_budget_in: bz.and_then(|b| b.weekly_budget_in),
        };
        let applied = soil_ev.applied_valve_s.get(&slug);
        let evidence: Vec<ZoneDayEvidence> = soil_ev
            .day_rows()
            .into_iter()
            .enumerate()
            .map(|(i, row)| {
                // Today (the window's last day) charges its PARTIAL
                // figure so a pre-dawn plan does not bill a full day's
                // evaporation; None falls to the module's fallback rung.
                let et0_mm = if row.is_today {
                    soil_ev.today_partial_et0_mm
                } else {
                    et0_days[i].et0_mm
                };
                ZoneDayEvidence {
                    date: row.date,
                    et0_mm,
                    gross_rain_mm: row.gross_rain_mm,
                    applied_valve_s: applied.and_then(|v| v.get(i).copied()).unwrap_or(0),
                }
            })
            .collect();
        // Gross trailing delivery for the explicit weekly ceiling: the
        // same union valve seconds x throughput the weekly balance
        // credits as applied_mm.
        let delivered_7d_mm = balance
            .and_then(|b| b.per_zone.get(&slug))
            .map(|e| e.applied_open_s.max(0) as f64 / 3600.0 * rt.throughput_mm_hr)
            .unwrap_or(0.0);
        let history = watering_policy
            .calendar
            .local_date(now_epoch)
            .map(|today| DeferHistory {
                today,
                mornings: soil_ev
                    .morning_decisions
                    .get(&slug)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            });
        let mut plan = plan_zone_with_coverage(
            &params,
            &evidence,
            expected_24h_rain_mm,
            delivered_7d_mm,
            history,
            &soil_ev.unknown_rain_dates,
        );
        let outlook = water_plan::soil_horizon(
            &params,
            watering_policy,
            balance,
            fc,
            now_epoch,
            forecast_as_of,
        );
        crate::engine::soil_outlook::apply(&mut plan, &params, &outlook, delivered_7d_mm);
        // An evidence-starved window (fewer than MIN_EVIDENCE_DAYS days
        // carrying an ET0 rung, a rain row, or applied seconds) replays
        // to a figure made almost purely of the fallback assumption:
        // near-full TAW within days on any texture. Publishing that
        // would fabricate a confident full deficit on a zone nothing
        // measured, so the absent-not-zero contract holds: no bucket, no
        // soil block, and a governed zone rides the weekly allocator's
        // sizing until enough rungs resolve. A live install leaves this
        // state within its first few mornings as ET0 ledger rows,
        // archive days, rain, and its own runs land.
        if plan.evidence_starved() && plan.planning_reason.is_none() {
            if model == SchedulingModel::Soil {
                if let Some(b) = snap.water_budgets.iter_mut().find(|b| b.zone_slug == slug) {
                    if plan.deferred_kind
                        == Some(crate::engine::soil_schedule::SoilDeferKind::Dormant)
                    {
                        b.today_seconds = 0;
                        b.today_reason = plan
                            .deferred_reason
                            .clone()
                            .unwrap_or_else(|| "Planting is dormant; watering held".into());
                        b.dormant = true;
                        b.soil_deferred_kind = plan.deferred_kind;
                        b.soil_deferred_reason = plan.deferred_reason.clone();
                    } else {
                        b.today_reason
                            .push_str("; soil estimate not established, using the weekly target");
                    }
                }
            }
            continue;
        }
        // The bucket's producer, ending the 0.7.22 "nothing computes
        // one" era: depletion published under the field's documented
        // sign (negative = needs water).
        let bucket = (plan.initial_uncertainty_mm
            <= crate::engine::soil_schedule::RESOLVED_UNCERTAINTY_MM)
            .then_some(-plan.depletion_mm);
        snap.zones[zi].bucket_mm = bucket;
        if let Some(m) = snap.zones[zi].math.as_mut() {
            m.bucket_mm = bucket;
        }
        if let Some(b) = snap.water_budgets.iter_mut().find(|b| b.zone_slug == slug) {
            b.soil_depletion_mm = bucket.map(|v| -v);
            b.soil_depletion_range_mm = Some((
                plan.depletion_mm,
                plan.depletion_mm + plan.initial_uncertainty_mm,
            ));
            b.soil_taw_mm = Some(plan.taw_mm);
            b.soil_raw_mm = Some(plan.raw_mm);
            b.soil_due = plan.due;
            b.dormant =
                plan.deferred_kind == Some(crate::engine::soil_schedule::SoilDeferKind::Dormant);
            b.soil_planned_seconds = plan.planned_seconds;
            b.soil_deferred_reason = plan.deferred_reason.clone();
            b.soil_deferred_kind = plan.deferred_kind;
            b.soil_ceiling_binding = plan.ceiling_binding;
            // The plan's confidence signal rides the wire with the block:
            // on the first post-starvation mornings the fallback days
            // dominate and the published deficit is mostly the
            // assumed-dry rule, which the soil panel's early-estimate
            // qualifier keys on; it drops on its own as coverage lands.
            b.soil_evidence_days = plan.evidence_days;
            b.soil_fallback_days = plan.fallback_days;
            b.soil_hold_is_forecast_rain = plan.hold_is_forecast_rain;
        }
        if model == SchedulingModel::Soil {
            // The math panel's capture efficiency reads the value the
            // refill division actually uses on this zone. Weekly-governed
            // zones keep the fixed 0.70: their minutes never divide by
            // it, and the weekly wire bytes are pinned.
            if let Some(m) = snap.zones[zi].math.as_mut() {
                m.capture_eff = eff;
            }
            governed.push((slug, plan));
        }
    }

    governed
}

/// Establish actual planning authority before evaluating any forecast waiver.
fn set_soil_governance(
    inputs: &mut Inputs,
    plans: &[(String, crate::engine::soil_schedule::SoilZonePlan)],
) {
    for zone in &mut inputs.soil_zones {
        zone.governed_by_soil_model = plans.iter().any(|(slug, _)| slug == &zone.slug);
    }
}

/// Fit established soil plans into the morning only after the engine has
/// decided permission for every zone, including weekly fallback zones.
pub(crate) fn apply_soil_plans(
    snap: &mut IrrigationSnapshot,
    watering_policy: &WateringPolicy,
    restriction_cap_seconds: Option<u32>,
    tick: Option<crate::engine::Tick>,
    now_epoch: i64,
    governed: Vec<(String, crate::engine::soil_schedule::SoilZonePlan)>,
) {
    // ---- The soil model GOVERNS its zones ----
    //
    // Swap each governed row's today figures for the soil plan's, then
    // fit the due set into the morning window. Everything downstream is
    // shared with the weekly rows: apply_budget_plan still applies the
    // seasonal dial, Override zeroing, and the force-run floor, the
    // verdict multiplier still scales, and the dispatcher still enforces
    // every safety verdict, so display and dispatch stay one truth.
    if governed.is_empty() {
        return;
    }
    let today_weekday: u8 = weekday_of(watering_policy, tick, now_epoch);
    let mut candidates: Vec<crate::engine::soil_schedule::AdmissionCandidate> = Vec::new();
    for (slug, plan) in &governed {
        // One formula with the demo's synthesized soil zone
        // (`soil_schedule::today_row`), so the reason strings cannot
        // drift between the live path and the screenshots.
        let cap_minutes = watering_policy
            .zone_runtime
            .get(slug)
            .map(|rt| rt.max_duration_s / 60)
            .unwrap_or(crate::config::schema::DEFAULT_MAX_RUN_MINUTES);
        let (today_seconds, today_reason, session_capped) =
            crate::engine::soil_schedule::today_row(plan, cap_minutes);
        if let Some(b) = snap.water_budgets.iter_mut().find(|b| &b.zone_slug == slug) {
            b.today_seconds = today_seconds;
            b.today_reason = today_reason;
            b.session_capped = session_capped;
        }
        // Admission candidates: due, cleared the defer and ceiling
        // holds, and not suppressed by an Override schedule today (the
        // suppressed zone would be zeroed downstream anyway; keeping it
        // out frees window for zones that will actually water).
        let override_active = crate::scheduler::manual::override_active_today(
            &watering_policy.manual_schedules,
            slug,
            today_weekday,
        );
        let permitted = snap
            .zone_verdicts
            .iter()
            .find(|zone| &zone.zone_slug == slug)
            .is_some_and(|zone| matches!(zone.verdict.as_str(), "run" | "run_extended"));
        if plan.due
            && plan.deferred_reason.is_none()
            && today_seconds > 0
            && !override_active
            && permitted
        {
            candidates.push(crate::engine::soil_schedule::AdmissionCandidate {
                slug: slug.clone(),
                depletion_mm: plan.depletion_mm,
                raw_mm: plan.raw_mm,
                planned_seconds: today_seconds,
            });
        }
    }

    // ---- Morning-window admission ----
    //
    // The window budget is exact and single-sourced: a plan fits iff its
    // TRUE wall time (cycle-soak splits, soak gaps, interleave, per-zone
    // preambles, priced by the dispatcher's own sequence_wall_seconds)
    // fits the span from local midnight to sunrise minus 15 minutes.
    // Weekly-governed zones share the same morning, so their seconds ride
    // every hypothetical set as a fixed base, priced at what they will
    // ACTUALLY dispatch: zero when an effective skip verdict blocks the
    // zone (the dispatcher's own predicate against the post-inertness
    // snapshot), zero when an Override manual schedule covers today
    // (apply_budget_plan zeroes those downstream), and otherwise the
    // seasonal-dialed figure apply_budget_plan will produce. Pricing raw
    // allocator seconds here deferred genuinely due soil zones against a
    // window that was actually empty.
    let (lat, lon) = watering_policy.location;
    if !(candidates.is_empty() || lat == 0.0 && lon == 0.0) {
        let candidate_slugs: std::collections::HashSet<&str> =
            candidates.iter().map(|c| c.slug.as_str()).collect();
        let hypo_zone = |slug: &str, seconds: u32| crate::model::ZoneState {
            slug: slug.to_string(),
            planned_run_seconds: seconds,
            ..Default::default()
        };
        // The effective per-zone run cap the dispatch arithmetic clamps
        // to: the configured maximum, tightened by an active watering
        // restriction. Base rows and candidates price against the same
        // cap.
        let effective_max_dur = |slug: &str| -> u32 {
            let max_dur = watering_policy
                .zone_runtime
                .get(slug)
                .copied()
                .unwrap_or_else(ZoneRuntime::fallback)
                .max_duration_s;
            match restriction_cap_seconds {
                Some(c) => max_dur.min(c),
                None => max_dur,
            }
        };
        let base_zones: Vec<crate::model::ZoneState> = snap
            .water_budgets
            .iter()
            .filter(|b| !candidate_slugs.contains(b.zone_slug.as_str()))
            .map(|b| {
                let blocked = snap
                    .zones
                    .iter()
                    .find(|z| z.slug == b.zone_slug)
                    .is_some_and(|z| {
                        crate::scheduler::smart_morning::zone_skip_verdict(snap, z).is_some()
                    });
                let suppressed = crate::scheduler::manual::override_active_today(
                    &watering_policy.manual_schedules,
                    &b.zone_slug,
                    today_weekday,
                );
                let seconds = if blocked || suppressed {
                    0
                } else {
                    seasonal_capped(
                        b.today_seconds,
                        watering_policy.seasonal_adjust_pct,
                        effective_max_dur(&b.zone_slug),
                    )
                };
                hypo_zone(&b.zone_slug, seconds)
            })
            .collect();
        let wall = |set: &[crate::engine::soil_schedule::AdmissionCandidate]| -> u64 {
            let mut hz = base_zones.clone();
            // Candidates take the same dispatch-truth pricing as the
            // base: apply_budget_plan runs a soil row's today_seconds
            // through the seasonal dial (re-clamped to the effective
            // cap) exactly as it does a weekly row's, so raw planned
            // seconds would admit a set a >100% dial then overruns, and
            // defer one a <100% dial actually fits.
            hz.extend(set.iter().map(|c| {
                hypo_zone(
                    &c.slug,
                    seasonal_capped(
                        c.planned_seconds,
                        watering_policy.seasonal_adjust_pct,
                        effective_max_dur(&c.slug),
                    ),
                )
            }));
            crate::engine::sequence::wall_seconds(
                &watering_policy.zone_agronomy,
                &hz,
                watering_policy.soak_minutes,
                watering_policy.interleave_cycles,
                watering_policy.duration_quantum_s,
            )
        };
        // The morning this plan actually runs: today while today's
        // window has not passed, else tomorrow (compute_next_run_epoch's
        // date logic).
        let now_utc =
            chrono::DateTime::<chrono::Utc>::from_timestamp(now_epoch, 0).unwrap_or_default();
        // Today in the DEPLOYMENT's calendar, from the same source the
        // window math uses, so the admission window and the day it
        // belongs to can never come from different clocks.
        let today_local = watering_policy
            .calendar
            .local_date(now_utc.timestamp())
            .unwrap_or_else(|| now_utc.date_naive());
        let morning = match crate::engine::sunrise::smart_morning_target_start(
            today_local,
            lat,
            lon,
            wall(&candidates),
            watering_policy.calendar,
        ) {
            Some(t) if t > now_utc => today_local,
            _ => today_local.succ_opt().unwrap_or(today_local),
        };
        // A probe sequence longer than any day always clamps the start
        // at local midnight, so available_s returns exactly the
        // midnight-to-finish span: the window's true budget.
        const WINDOW_PROBE_SEQ_S: u64 = 2 * 86_400;
        if let Some(available_s) = crate::engine::sunrise::smart_morning_available_s(
            morning,
            lat,
            lon,
            WINDOW_PROBE_SEQ_S,
            watering_policy.calendar,
        ) {
            let outcome = crate::engine::soil_schedule::admit_zones(
                &candidates,
                available_s.max(0) as u64,
                wall,
            );
            for deferred in &outcome.deferred {
                if let Some(b) = snap
                    .water_budgets
                    .iter_mut()
                    .find(|b| b.zone_slug == deferred.slug)
                {
                    b.today_seconds = 0;
                    b.today_reason = deferred.reason.clone();
                    // The wire's soil block reflects the post-admission
                    // plan: nothing runs today, and the window reason is
                    // the hold.
                    b.soil_planned_seconds = 0;
                    b.soil_deferred_reason = Some(deferred.reason.clone());
                    b.soil_deferred_kind =
                        Some(crate::engine::soil_schedule::SoilDeferKind::Window);
                }
            }
        }
    }
}

#[cfg(test)]
mod finalize_tests {
    use super::*;
    use crate::forecast::snapshot::HourlyEntry;

    /// Denver, Thursday 15 October 2026, 00:00 MDT (UTC-6).
    const DENVER: (f64, f64) = (39.74, -104.99);
    const OCT15_MIDNIGHT_MDT: i64 = 1_792_044_000;

    fn policy() -> WateringPolicy {
        let mut p = WateringPolicy::default();
        p.location = DENVER;
        p.calendar = crate::engine::calendar::Calendar::fixed_offset(-6 * 3600).unwrap();
        p.skip_rules.min_temp_f = 38.0;
        p
    }

    /// 30 F through the pre-dawn hours, warming five degrees an hour from
    /// eight; plus `warm` on every hour.
    fn hours(warm: f64) -> Vec<HourlyEntry> {
        (0..48)
            .map(|i| {
                let h = i % 24;
                let temp = if h < 8 {
                    30.0
                } else if h <= 16 {
                    30.0 + (h - 7) as f64 * 5.0
                } else {
                    75.0 - (h - 16) as f64 * 6.0
                };
                HourlyEntry {
                    time_epoch: OCT15_MIDNIGHT_MDT + i * 3600,
                    temp_f: Some(temp + warm),
                    ..Default::default()
                }
            })
            .collect()
    }

    fn snapshot() -> IrrigationSnapshot {
        IrrigationSnapshot {
            zones: vec![crate::model::ZoneState {
                slug: "front".into(),
                name: "Front".into(),
                planned_run_seconds: 1800,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The forecast the pass assembled from decides the window: a freezing
    /// pre-dawn puts the next run after sunrise, the way the dispatcher
    /// will choose it; a mild one keeps the pre-dawn start. The same
    /// `Finalize` runs on both paths, so this is the both-paths test.
    #[test]
    fn a_freezing_pre_dawn_moves_the_next_run_after_sunrise() {
        let now = OCT15_MIDNIGHT_MDT + 3600;
        let freezing = Finalize {
            watered: vec![],
            forecast: std::sync::Arc::new(ForecastSnapshot {
                hourly: hours(0.0),
                ..Default::default()
            }),
            now_epoch: now,
        };
        let mild = Finalize {
            watered: vec![],
            forecast: std::sync::Arc::new(ForecastSnapshot {
                hourly: hours(20.0),
                ..Default::default()
            }),
            now_epoch: now,
        };
        let p = policy();
        let mut a = snapshot();
        freezing.apply(&mut a, &p);
        let mut b = snapshot();
        mild.apply(&mut b, &p);
        assert_eq!(a.next_run_total_minutes, 30.0);
        assert_eq!(
            a.next_run_epoch,
            OCT15_MIDNIGHT_MDT + 9 * 3600,
            "09:00 is the first hour that clears 38 F for the run and its tail"
        );
        assert!(
            b.next_run_epoch > now && b.next_run_epoch < a.next_run_epoch,
            "a mild morning keeps the pre-dawn start ({} vs {})",
            b.next_run_epoch,
            a.next_run_epoch
        );
        assert_eq!(a.next_run_day_offset, Some(0));
        assert!(matches!(a.next_run_state, crate::model::NextRunState::At));
    }

    /// `assemble` and the budget re-plan no longer decide the next run;
    /// each shell decides it once, after the controller overlay and the
    /// multiplier, through `Finalize::apply`.
    #[test]
    fn the_next_run_is_decided_once_per_pass_after_the_overlay() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let assembly = std::fs::read_to_string(root.join("src/assembly/mod.rs")).unwrap();
        let before_tests = assembly.split("#[cfg(test)]").next().unwrap();
        let assemble_body = {
            let start = before_tests.find("pub fn assemble(").unwrap();
            let end = before_tests[start..]
                .find(
                    "
}
",
                )
                .unwrap()
                + start;
            crate::engine::clock::code_only(&before_tests[start..end])
        };
        assert!(
            !assemble_body.contains("set_next_run("),
            "assemble decides no next run"
        );
        let budget_body = {
            let start = before_tests
                .find("pub(crate) fn apply_budget_plan(")
                .unwrap();
            let end = before_tests[start..]
                .find(
                    "
}
",
                )
                .unwrap()
                + start;
            crate::engine::clock::code_only(&before_tests[start..end])
        };
        assert!(!budget_body.contains("set_next_run("));
        assert_eq!(
            crate::engine::clock::code_only(before_tests)
                .matches("set_next_run(")
                .count(),
            2,
            "one definition, one call (Finalize::apply)"
        );

        let refresher = std::fs::read_to_string(root.join("src/refresher/shell.rs")).unwrap();
        for shell in ["async fn refresh_once(", "async fn refresh_once_native("] {
            let start = refresher.find(shell).unwrap();
            let end = refresher[start..]
                .find(
                    "
}
",
                )
                .unwrap()
                + start;
            let body = crate::engine::clock::code_only(&refresher[start..end]);
            assert_eq!(
                body.matches("finalize.apply(").count(),
                1,
                "{shell} finalizes exactly once"
            );
            let overlay = body
                .find("overlay_reporting_controllers(")
                .or_else(|| body.find("native_controller_state("))
                .unwrap_or(0);
            let multiplier = body.find("apply_verdict_multiplier(").unwrap();
            let fin = body.find("finalize.apply(").unwrap();
            assert!(
                overlay < multiplier && multiplier < fin,
                "{shell}: overlay, then the multiplier, then finalize"
            );
        }
    }
}

#[cfg(test)]
mod seam_tests {
    use super::*;
    use crate::engine::conditions::{
        CmpOp, ConditionExpr, ConditionRule, Metric, RuleAction, RuleScope,
    };
    use crate::engine::scripting::CompiledScripts;
    use crate::forecast::snapshot::HourlyEntry;
    use crate::tempest::state::Snapshot as TempestSnapshot;

    /// A yard on Orlando sand with one zone, a live station reading a
    /// calm morning, and a rule that scales every run by 1.25.
    fn policy_with_a_quarter_more() -> WateringPolicy {
        let mut cfg = crate::config::schema::Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.zones.insert(
            "front".into(),
            serde_json::from_value(serde_json::json!({
                "display_name": "Front",
                "area_sqft": 1000.0,
                "species": "st_augustine",
                "soil_texture": "sand",
                "sprinkler_type": "spray",
                "precip_rate_mm_hr": 15.0,
                "precip_rate_source": "measured",
                "controller_id": "os_main",
                "controller_station": "1"
            }))
            .unwrap(),
        );
        cfg.conditions.rules.push(ConditionRule {
            id: "quarter_more".into(),
            name: "A quarter more".into(),
            enabled: true,
            scope: RuleScope::AllZones,
            condition: ConditionExpr::Compare {
                metric: Metric::TempNowF,
                op: CmpOp::Gt,
                value: -100.0,
            },
            action: RuleAction::AdjustMultiplier { factor: 1.25 },
        });
        WateringPolicy::from_config(&cfg)
    }

    fn input(map: HashMap<String, Value>, policy: &WateringPolicy) -> AssemblyInput<'_> {
        let now = 1_788_609_600; // 2026-09-05 04:00 UTC, a fixed instant
        let fc = ForecastSnapshot {
            last_refresh_epoch: now,
            source_reachable: true,
            hourly: (0..48)
                .map(|hour| HourlyEntry {
                    time_epoch: now + hour * 3600,
                    precip_in: Some(0.0),
                    temp_f: Some(72.0),
                    wind_mph: Some(4.0),
                    humidity_pct: Some(50),
                    ..Default::default()
                })
                .collect(),
            daily: (0..8)
                .map(|day| crate::forecast::snapshot::DailyEntry {
                    day_marker: crate::engine::clock::DayMarker::inside_local_day(
                        now + day * 86400,
                    ),
                    precip_sum_in: Some(0.0),
                    temp_max_f: Some(80.0),
                    temp_min_f: Some(65.0),
                    wind_max_mph: Some(4.0),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let tempest = TempestSnapshot {
            last_packet_epoch: now,
            air_temp_live_epoch: now,
            wind_live_epoch: now,
            rh_live_epoch: now,
            air_temp_f: 72.0,
            wind_avg_mph: 3.0,
            rh_pct: 50.0,
            source_label: "TestStation".into(),
            ..Default::default()
        };
        let zones: &'static [crate::zones::ZoneIdent] =
            Box::leak(vec![crate::zones::ZoneIdent::new("front", "Front")].into_boxed_slice());
        let scripts: &'static CompiledScripts = Box::leak(Box::new(CompiledScripts::compile(&[])));
        let zone_runtime: &'static HashMap<String, ZoneRuntime> =
            Box::leak(Box::new(policy.zone_runtime.clone()));
        AssemblyInput {
            map,
            forecast: std::sync::Arc::new(fc),
            current_weather: ["air_temp_f", "wind_mph", "rh_pct"]
                .into_iter()
                .zip(test_current_samples(
                    &tempest,
                    [crate::weather::arbitration::LIVE_FRESHNESS_SECS; 3],
                ))
                .filter_map(|(key, sample)| sample.map(|sample| (key.into(), sample)))
                .collect(),
            tempest: std::sync::Arc::new(tempest),
            field_sources: Default::default(),
            rain_owner: None,
            rain_today_owner: None,
            zones,
            zone_runtime,
            watering_policy: policy,
            scripts,
            balance: None,
            control: None,
            restart_reasons: Vec::new(),
            now_epoch: now,
            // What the shell's prefetch resolves for a yard with no probe:
            // one unprobed row per configured soil zone.
            prefetched: Prefetched {
                soil_zones: policy
                    .soil_zones
                    .iter()
                    .map(|z| ZoneSoil {
                        slug: z.slug.clone(),
                        name: z.name.clone(),
                        pct: None,
                        saturation_pct: z.saturation_pct,
                        target_min_pct: z.target_min_pct,
                        probe_configured: false,
                        governed_by_soil_model: false,
                        planning_forecast_unavailable: false,
                        sprinkler_type: z.sprinkler_type,
                    })
                    .collect(),
                ..Default::default()
            },
        }
    }

    /// The Home Assistant path and the native path are one function with
    /// a different map: identical inputs give an identical snapshot, and
    /// the 1.25x rule scales both the same way. Nothing here depends on
    /// the machine's clock or zone, so the timezone matrix is the same
    /// test four times.
    #[test]
    fn both_paths_assemble_the_same_snapshot_from_the_same_inputs() {
        let policy = policy_with_a_quarter_more();
        let native = assemble(input(HashMap::new(), &policy));
        // The HA map carries nothing the native path lacks.
        let mut map = HashMap::new();
        map.insert(
            "sensor.unrelated".to_string(),
            serde_json::json!({ "state": "42" }),
        );
        let ha = assemble(input(map, &policy));
        assert_eq!(native, ha);
        // Pure: the same inputs twice are the same snapshot.
        assert_eq!(native, assemble(input(HashMap::new(), &policy)));
        assert_eq!(native.last_refresh_epoch, 1_788_609_600);
        let z = &native.zones[0];
        assert_eq!(
            z.verdict.as_ref().map(|v| v.multiplier),
            Some(1.25),
            "the rule fired on both paths"
        );
    }

    #[test]
    fn week_plan_carries_storms_demand_and_its_own_irrigation_forward() {
        let mut policy = policy_with_a_quarter_more().with_utc_calendar();
        policy.scheduling_model = crate::config::schema::SchedulingModel::Soil;
        policy.condition_rules.clear();
        let sample = input(HashMap::new(), &policy);
        let now = sample.now_epoch;
        let today = policy.calendar.local_date(now).unwrap();
        let dates: Vec<_> = (0..14)
            .rev()
            .map(|back| today - chrono::Duration::days(back))
            .collect();
        let mut rain = vec![0.0; 14];
        rain[12] = 60.0;
        let evidence = BalanceTick {
            observed_rain_mm: 60.0,
            observed_rain_source: "gauge".into(),
            observed_rain_days_mm: vec![0.0, 0.0, 0.0, 0.0, 0.0, 60.0, 0.0],
            bias: crate::engine::BiasModel::identity(),
            per_zone: HashMap::new(),
            runs_degraded: false,
            soil: SoilTickEvidence {
                dates: dates.clone(),
                rain_mm: rain,
                et0_ledger: dates.into_iter().map(|d| (d, 4.0)).collect(),
                today_partial_et0_mm: Some(0.0),
                ..Default::default()
            },
        };
        let build = |rain_in: f64| {
            let mut trial = input(HashMap::new(), &policy);
            trial.balance = Some(&evidence);
            let fc = std::sync::Arc::make_mut(&mut trial.forecast);
            for day in &mut fc.daily {
                day.precip_sum_in = Some(rain_in);
                day.et0_in = crate::units::mm_to_in(4.0);
                day.et0_reported = true;
            }
            for hour in &mut fc.hourly {
                hour.precip_in = Some(rain_in / 24.0);
            }
            assemble(trial)
        };
        let wet = build(1.0);
        assert_eq!(wet.water_plan.len(), 7);
        assert!(wet
            .water_plan
            .iter()
            .flat_map(|d| &d.zones)
            .all(|z| z.planned_seconds == 0));
        assert!(wet.water_plan.iter().all(|d| d.start_epoch.is_none()));
        let dry = build(0.0);
        let first = &dry.water_plan[1].zones[0];
        let following = &dry.water_plan[2].zones[0];
        assert!(first.planned_seconds > 0, "{first:?}");
        assert!(
            following.depletion_mm.unwrap() < first.depletion_mm.unwrap(),
            "modeled irrigation must refill the following day's balance: {first:?} {following:?}"
        );
        assert_eq!(
            evidence.soil.rain_mm[13], 0.0,
            "projection must not change measured evidence"
        );
        assert!(evidence.soil.run_segments.is_empty());
        let mut final_wet = wet;
        Finalize {
            forecast: sample.forecast,
            watered: Vec::new(),
            now_epoch: now,
        }
        .apply(&mut final_wet, &policy);
        assert_eq!(final_wet.next_run_epoch, 0);
        assert_eq!(
            final_wet.next_run_state,
            crate::model::NextRunState::NoWaterPlanned
        );
    }

    #[test]
    fn forecast_rain_never_becomes_measured_wetness_and_missing_plans_hold_force() {
        let policy = policy_with_a_quarter_more().with_utc_calendar();
        let mut expected_storm = input(HashMap::new(), &policy);
        std::sync::Arc::make_mut(&mut expected_storm.forecast).daily[0].precip_sum_in = Some(2.0);
        let snapshot = assemble(expected_storm);
        assert_eq!(snapshot.forecast.rain_today_om_in, Some(2.0));
        assert_eq!(snapshot.forecast.rain_today_tempest_in, 0.0);
        assert_eq!(snapshot.skip_check.rain_today_in, 0.0);
        assert_eq!(snapshot.skip_check.rain_observed_recent_in, 0.0);
        assert_ne!(snapshot.skip_check.reason_code, "observed_rain");
        assert_ne!(snapshot.skip_check.reason_code, "already_wet");

        let mut measured = input(HashMap::new(), &policy);
        let day = policy.calendar.local_date(measured.now_epoch).unwrap();
        let station = std::sync::Arc::make_mut(&mut measured.tempest);
        station.rain_in_today = 0.04;
        station.rain_today_day_ordinal = chrono::Datelike::num_days_from_ce(&day);
        measured.rain_today_owner = Some(crate::tempest::state::RainOwner {
            nature: crate::model::RainNature::Measured,
            label: "yard_gauge".into(),
            is_live: true,
            is_fresh: true,
        });
        measured.prefetched.observed_past_gauge_in = 0.30;
        let snapshot = assemble(measured);
        assert!((snapshot.skip_check.rain_observed_recent_in - 0.34).abs() < 1e-9);
        assert_eq!(snapshot.skip_check.reason_code, "observed_rain");

        let force = crate::model::IrrigationControlState {
            global_override: "run".into(),
            ..Default::default()
        };
        let mut missing = input(HashMap::new(), &policy);
        std::sync::Arc::make_mut(&mut missing.forecast).hourly[12].precip_in = None;
        missing.control = Some(&force);
        let snapshot = assemble(missing);
        assert_eq!(snapshot.skip_check.reason_code, "planning_forecast");
        assert_eq!(
            snapshot.decision_trace.as_ref().unwrap().reason_code,
            "planning_forecast"
        );
        assert_eq!(snapshot.zones[0].planned_run_seconds, 0);
        assert_eq!(
            snapshot.zones[0].verdict.as_ref().unwrap().reason_code,
            "planning_forecast"
        );
        assert_eq!(
            snapshot.forecast.rain_next_4h_in,
            Some(0.0),
            "known near-term rain does not cover a later hole"
        );
        for stamp in [
            0,
            1_788_609_600 - crate::forecast::snapshot::FORECAST_MAX_AGE_S - 1,
            1_788_609_601,
        ] {
            let mut stale = input(HashMap::new(), &policy);
            std::sync::Arc::make_mut(&mut stale.forecast).last_refresh_epoch = stamp;
            stale.control = Some(&force);
            let snapshot = assemble(stale);
            assert_eq!(
                snapshot.skip_check.reason_code, "planning_forecast",
                "untrusted refresh {stamp}"
            );
            assert_eq!(snapshot.zones[0].planned_run_seconds, 0);
            assert!(snapshot.zones[0].verdict.as_ref().unwrap().verdict == "skip");
        }
    }

    #[test]
    fn ha_valve_readback_requires_an_explicit_on_or_off() {
        let mut policy = policy_with_a_quarter_more();
        policy.ha_sprinkler_prefix = "yard_controller".into();
        for (state, expected) in [
            (None, None),
            (Some(serde_json::json!("unavailable")), None),
            (Some(serde_json::json!("unknown")), None),
            (Some(serde_json::json!("idle")), None),
            (Some(serde_json::json!(false)), None),
            (Some(serde_json::json!("on")), Some(true)),
            (Some(serde_json::json!("off")), Some(false)),
        ] {
            let mut map = HashMap::new();
            // A stale/default-prefix entity must not certify a renamed device.
            map.insert(
                "binary_sensor.opensprinkler_front_station_running".into(),
                serde_json::json!({"state": "off"}),
            );
            if let Some(state) = state {
                map.insert(
                    "binary_sensor.yard_controller_front_station_running".into(),
                    serde_json::json!({"state": state}),
                );
            }
            let snap = assemble(input(map, &policy));
            assert_eq!(snap.zones[0].running_known, expected.is_some());
            assert_eq!(snap.zones[0].running, expected.unwrap_or(false));
        }
    }

    #[test]
    fn ha_water_level_is_absent_until_finite_and_preserves_a_measured_zero() {
        let policy = policy_with_a_quarter_more();
        for (state, expected) in [
            (None, None),
            (Some("unavailable"), None),
            (Some("unknown"), None),
            (Some("NaN"), None),
            (Some("inf"), None),
            (Some("-inf"), None),
            (Some("0"), Some(0.0)),
            (Some("85.5"), Some(85.5)),
        ] {
            let mut map = HashMap::new();
            if let Some(state) = state {
                map.insert(
                    "sensor.opensprinkler_water_level".into(),
                    serde_json::json!({"state": state}),
                );
            }
            let snap = assemble(input(map, &policy));
            assert_eq!(snap.water_level_pct, expected);
        }
    }

    /// The clock is an input, and the only one that moves: the same
    /// inputs at the same instant assemble byte-for-byte the same
    /// snapshot, and a later instant changes what the instant stamps
    /// (the refresh time) while the rest of the readings hold.
    #[test]
    fn the_clock_is_an_input_and_nothing_else_moves() {
        let policy = WateringPolicy::default().with_utc_calendar();
        let a = assemble(input(HashMap::new(), &policy));
        let b = assemble(input(HashMap::new(), &policy));
        assert_eq!(a, b, "deterministic at one instant");
        let mut later = input(HashMap::new(), &policy);
        later.now_epoch += 60;
        let c = assemble(later);
        assert_eq!(c.last_refresh_epoch, a.last_refresh_epoch + 60);
        // A minute later, with the same forecast and station, the
        // readings and the plan are unchanged.
        assert_eq!(c.zones, a.zones);
        assert_eq!(c.skip_check.temp_now_f, a.skip_check.temp_now_f);
        assert_eq!(c.water_budgets, a.water_budgets);
    }

    #[test]
    fn startup_hold_is_preserved_in_snapshot_and_every_decision_without_changing_controls() {
        let policy = policy_with_a_quarter_more();
        let control = crate::model::IrrigationControlState {
            global_override: "run".into(),
            ..Default::default()
        };
        let mut held = input(HashMap::new(), &policy);
        held.control = Some(&control);
        held.restart_reasons = vec!["the deployment location changed".into()];
        let snap = assemble(held);
        assert!(snap.restart_required);
        assert_eq!(
            snap.restart_reasons,
            vec!["the deployment location changed"]
        );
        assert_eq!(
            snap.global_override, "run",
            "runtime hold is not a control write"
        );
        assert_eq!(snap.skip_check.reason_code, "restart_required");
        assert_eq!(
            snap.decision_trace.as_ref().unwrap().reason_code,
            "restart_required"
        );
        assert!(snap
            .zones
            .iter()
            .all(|z| z.verdict.as_ref().unwrap().reason_code == "restart_required"));
        assert!(snap
            .seven_day_verdicts
            .iter()
            .all(|d| d.reason_code == "restart_required"));
        let stored = serde_json::to_string(&snap).unwrap();
        let restored: IrrigationSnapshot = serde_json::from_str(&stored).unwrap();
        assert!(restored.restart_required);
        assert_eq!(restored.restart_reasons, snap.restart_reasons);
    }

    /// The pure half reads no clock, awaits nothing and needs no runtime:
    /// every file under assembly/ and engine/, walked, not just this one.
    #[test]
    fn the_assembly_is_pure() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["src/assembly", "src/engine"] {
            walk(&root.join(dir), &mut files);
        }
        assert!(files.len() > 20, "the walk found the tree");
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            if name.ends_with("_tests.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&f).unwrap();
            // Only the code above the test module counts; the tests may
            // name what they ban.
            let code = crate::engine::clock::code_only(src.split("#[cfg(test)]").next().unwrap());
            for banned in [
                "tokio",
                ".await",
                "async fn",
                "Utc::now",
                "now_local",
                "Local::now",
                "SystemTime::now",
                "std::fs",
                "rusqlite",
                "deployment_calendar(",
            ] {
                assert!(
                    !code.contains(banned),
                    "{} must not contain {banned}: the instant and the calendar are inputs",
                    f.display()
                );
            }
        }
    }

    /// The engine names its own inputs and outputs.
    ///
    /// Every type it needs is its own, `crate::model`, or config. It read
    /// `crate::ha::snapshot` for its whole life, which meant the pure
    /// decision layer imported the module named for ONE of the several
    /// systems it can talk to, in order to describe a zone. An
    /// integration is a caller, never a vocabulary.
    #[test]
    fn the_engine_imports_no_integration() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        for dir in ["src/assembly", "src/engine"] {
            walk(&root.join(dir), &mut files);
        }
        assert!(files.len() > 20, "the walk found the tree");
        for f in files {
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            // A test may build an integration's fixture to prove the seam
            // accepts what that integration produces; the code under it
            // may not.
            if name.ends_with("_tests.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&f).unwrap();
            let code = crate::engine::clock::code_only(src.split("#[cfg(test)]").next().unwrap());
            let mut banned = vec![
                "crate::ha",
                "crate::integrations",
                "crate::api",
                "crate::persistence",
                "crate::controllers",
            ];
            // The seam may name a source KIND: turning a reading into an
            // engine input is exactly where "which provider said this"
            // still matters (a gauge's rain is measured, a model's is
            // not). By the time the engine has the input, it does not.
            if f.components().any(|c| c.as_os_str() == "engine") {
                banned.push("crate::sources");
            }
            for banned in banned {
                assert!(
                    !code.contains(banned),
                    "{} must not contain {banned}: the shared shapes are crate::model",
                    f.display()
                );
            }
        }
    }

    fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
}

// The sibling test files for pass.rs and readings.rs. Declared last so
// the source guards above, which read this file up to its first test
// module, see all of the code.
#[cfg(test)]
mod pass_tests;
#[cfg(test)]
mod readings_tests;
