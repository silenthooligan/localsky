// Demo-mode synthetic data feeder. When LOCALSKY_DEMO=1, this module
// spawns a background task that periodically writes plausible weather +
// irrigation + forecast values into the live stores so the dashboard
// renders fully populated UI without any external dependency.
//
// Uses ..Default::default() on every snapshot so we only set the fields
// we want to show; the rest stay zero/empty and the UI degrades
// gracefully through its existing empty-state handling.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::interval;
use tracing::info;

use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot, HourlyEntry};
use crate::forecast::ForecastStore;
use crate::ha::snapshot::{
    DayVerdict, Forecast, IrrigationSnapshot, RainNature, SkipCheck, SoilForecast, WaterBudget,
    ZoneMath, ZoneState,
};
use crate::ha::IrrigationStore;
use crate::tempest::state::{Snapshot as TempestSnapshot, TempestStore};

/// Spawn the demo-data feeder. Tick cadence 3s; synthesized "day"
/// loops every ~8.6 min so screenshots can capture variety quickly.
///
/// Besides the snapshot stores, the feeder also keeps the HEALTH surfaces
/// honest: demo mode runs no source adapters, so without help every seeded
/// source except the Tempest ages into `offline` and /api/health reports the
/// showcase instance permanently `degraded`. Three cheap heartbeats fix that:
///   * the forecast snapshot is re-stored on a 60s cadence (its
///     `last_refresh_epoch` is the open_meteo liveness proxy),
///   * `stamp_source_provenance` claims per-field ownership in the live merge
///     maps each tick, so the station/gateway/cloud sources read `active` with
///     a real per-field provenance breakdown, and
///   * `feed_sensor_history` writes plausible ecowitt + nws readings into the
///     SAME sensor_history table /api/health's last-seen fallback queries.
pub fn spawn(
    tempest: Arc<TempestStore>,
    irrigation: Arc<IrrigationStore>,
    forecast: Arc<ForecastStore>,
    history: Option<Arc<tokio::sync::Mutex<rusqlite::Connection>>>,
) {
    info!("demo_data: spawning synthetic data feeder (LOCALSKY_DEMO=1)");
    if let Some(conn) = history.clone() {
        tokio::spawn(seed_history(conn));
    }
    if let Some(conn) = history {
        tokio::spawn(feed_sensor_history(conn));
    }
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(3));
        let started = std::time::Instant::now();
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            let elapsed_s = started.elapsed().as_secs() as f64;
            let t_sim = (elapsed_s * 167.0) % 86400.0;
            let now = chrono::Utc::now().timestamp();
            let snap = synth_tempest(t_sim);
            // Provenance first (it clones-and-restores the PREVIOUS snapshot to
            // record ownership), then the wholesale store, so the snapshot the
            // dashboard reads is always the pure synthetic one.
            stamp_source_provenance(&tempest, &snap, now);
            tempest.store(snap);
            let mut irr = synth_irrigation(t_sim);
            // Copy the live per-field provenance the stamps above produced,
            // exactly like the refresher does on a real deployment, so the
            // dashboard's per-field source labels match /api/health.
            irr.field_sources = tempest.field_source_map();
            irrigation.store(irr);
            // Forecast heartbeat: every 20th tick (60s). Keeps
            // `last_refresh_epoch` fresh so the open_meteo source stays alive
            // in /api/health without re-emitting the (larger) forecast payload
            // to SSE subscribers every 3 seconds.
            if n.is_multiple_of(20) {
                forecast.store(synth_forecast());
            }
            n += 1;
        }
    });
}

/// Claim per-field ownership in the live merge maps for the sources the demo
/// seed configures, mirroring the topology the seed describes: the Tempest
/// owns the station truth (temp / wind / humidity / rain rate), the Ecowitt
/// gateway's barometer owns pressure, and Open-Meteo cloud-fills the
/// forecast-nature scalars (rain probability, ET0). Values are the SAME ones
/// the wholesale store writes right after, so only the ownership/provenance
/// side effects persist. This is what makes /api/health and the source catalog
/// read the demo as a healthy merge (`active` sources, populated conditions
/// provenance) instead of a pile of never-seen offline entries.
fn stamp_source_provenance(tempest: &TempestStore, snap: &TempestSnapshot, now: i64) {
    use crate::ports::weather_source::WeatherField as F;
    tempest.apply_source_fields(
        &[
            (F::AirTempF, snap.air_temp_f),
            (F::WindMph, snap.wind_avg_mph),
            (F::RhPct, snap.rh_pct),
            (F::RainIntensityInHr, snap.rain_intensity_in_hr),
        ],
        now,
        true,
        crate::tempest::state::TEMPEST_LABEL,
    );
    tempest.apply_source_fields(
        &[(F::PressureInHg, snap.pressure_inhg)],
        now,
        true,
        "ecowitt",
    );
    // The demo snapshot always carries a real Pop (built with Some in
    // demo_snapshot); the expect documents that invariant rather than
    // silently re-fabricating a 0%.
    let pop = snap.pop_pct.expect("demo snapshot always sets pop_pct");
    tempest.apply_source_fields(
        &[(F::Pop, pop), (F::Et0Today, snap.et0_today)],
        now,
        false,
        "open_meteo",
    );
}

/// Heartbeat the bus-kind demo sources (the Ecowitt soil gateway + NWS) into
/// sensor_history every 60s. Demo mode spawns no adapters, so nothing else
/// ever records a last-seen for them and /api/health would report both
/// `offline` (degrading the showcase instance) ~30 minutes after boot. The
/// rows double as real telemetry: the soil channels match the zone bindings
/// the seed configures (`source:ecowitt:soilmoisture_<slug>`), so the soil
/// pickers, Sensors page, and sparklines all show plausible data.
async fn feed_sensor_history(conn: Arc<tokio::sync::Mutex<rusqlite::Connection>>) {
    use crate::persistence::sensor_history::Reading;
    use crate::sources::bus_recorder::zone_soil_key;

    let store = crate::persistence::SensorHistoryStore::new(conn);
    // Same zone slugs + baseline moisture the irrigation snapshot carries.
    let zones = [
        ("back_yard", 42.0),
        ("front_yard", 48.0),
        ("side_yard", 50.0),
        ("back_yard_shrubs", 55.0),
    ];
    let mut tick = interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        let now = chrono::Utc::now().timestamp();
        // A slow deterministic wiggle so the sparklines look measured, not flat.
        let wiggle = ((now as f64) / 1800.0).sin();
        let mut rows: Vec<Reading> = zones
            .iter()
            .map(|(slug, pct)| Reading {
                epoch: now,
                source_id: "ecowitt".to_string(),
                key: zone_soil_key(slug),
                value: (pct + wiggle * 1.5).clamp(0.0, 100.0),
            })
            .collect();
        rows.push(Reading {
            epoch: now,
            source_id: "ecowitt".to_string(),
            key: "pressure_inhg".to_string(),
            value: 30.05 + wiggle * 0.04,
        });
        rows.push(Reading {
            epoch: now,
            source_id: "nws".to_string(),
            key: "air_temp_f".to_string(),
            value: 84.0 + wiggle * 3.0,
        });
        rows.push(Reading {
            epoch: now,
            source_id: "nws".to_string(),
            key: "pop".to_string(),
            value: (35.0 + wiggle * 20.0).clamp(0.0, 100.0),
        });
        if let Err(e) = store.insert_many(rows).await {
            tracing::debug!("demo_data: sensor-history heartbeat failed: {e}");
        }
    }
}

/// Synthetic config seeded on first boot in demo mode.
///
/// The feeder above writes live weather + irrigation snapshots into the runtime
/// stores, but the CONFIG-driven surfaces read the on-disk config, not the
/// snapshot: `/api/v1/info` computes `has_irrigation` from configured
/// controllers/zones (the sidebar hides the whole irrigation nav when it is
/// false), and the Devices page + Zones/settings editors render the configured
/// sources/controllers/zones. A fresh demo volume has an empty config, so the
/// demo collapses to a weather-only shell even though the snapshot is fully
/// populated. This seeds a coherent config so the demo showcases the whole
/// product: four zones keyed to the SAME slugs [`synth_irrigation`] emits, a
/// dry-run controller (no hardware), and a spread of weather-source kinds with
/// a per-field backup chain. Only seeded when no controllers/zones exist yet
/// (see main.rs), so a persisted demo volume is left alone.
pub fn seed_config() -> crate::config::schema::Config {
    use crate::config::schema::{
        Config, ControllerEntry, ControllerKind, DeploymentMode, DryRunConfig, EcowittLocalConfig,
        GrassSpecies, Location, NwsConfig, OpenMeteoConfig, PrecipRateSource, SoilTexture,
        SourceEntry, SourceKind, SprinklerType, SunExposure, TempestUdpConfig, ZoneConfig,
    };
    use std::collections::BTreeMap;

    // Every Config field is #[serde(default)], so an empty document
    // deserializes to the all-defaults config. Building from that (rather than a
    // full struct literal) keeps this robust as new Config fields are added.
    let mut cfg: Config =
        toml::from_str("").expect("empty document deserializes to a default Config");

    cfg.deployment.display_name = "LocalSky Demo".to_string();
    cfg.deployment.location = Location {
        lat: 28.54,
        lon: -81.38,
        elevation_m: Some(30.0),
    };
    cfg.deployment.mode = DeploymentMode::Standalone;

    // A spread of source kinds so the Devices page shows a real topology: a LAN
    // station (truth), a keyless cloud forecast + radar, a soil gateway, and the
    // regional government service.
    cfg.sources = vec![
        SourceEntry {
            id: "tempest".to_string(),
            priority: 100,
            enabled: true,
            max_age_s: None,
            source: SourceKind::TempestUdp(TempestUdpConfig {
                bind_addr: "0.0.0.0:50222".to_string(),
                hub_serial: None,
            }),
        },
        SourceEntry {
            id: "open_meteo".to_string(),
            priority: 50,
            enabled: true,
            max_age_s: None,
            source: SourceKind::OpenMeteo(OpenMeteoConfig {
                forecast_days: 7,
                forecast_hours: 48,
                past_days: 3,
                include_radar: true,
                model: "best_match".to_string(),
                endpoint: None,
            }),
        },
        SourceEntry {
            id: "ecowitt".to_string(),
            priority: 90,
            enabled: true,
            max_age_s: None,
            source: SourceKind::EcowittLocal(EcowittLocalConfig {
                path: "/ingest/ecowitt".to_string(),
                shared_secret: None,
            }),
        },
        SourceEntry {
            id: "nws".to_string(),
            priority: 40,
            enabled: true,
            max_age_s: None,
            source: SourceKind::Nws(NwsConfig {
                user_agent: "LocalSky Demo (demo@localsky.io)".to_string(),
            }),
        },
    ];

    // Showcase the per-field backup chain: rain prefers the local soil gateway,
    // then the government service, then the cloud model; wind prefers the LAN
    // station, then the cloud model.
    cfg.field_source_chains = BTreeMap::from([
        (
            "rain_today_in".to_string(),
            vec![
                "ecowitt".to_string(),
                "nws".to_string(),
                "open_meteo".to_string(),
            ],
        ),
        (
            "wind_mph".to_string(),
            vec!["tempest".to_string(), "open_meteo".to_string()],
        ),
    ]);
    cfg.forecast_provider = Some("open_meteo".to_string());

    // One dry-run controller (no hardware). simulate_runs writes synthetic run
    // rows so History shows activity.
    cfg.controllers = vec![ControllerEntry {
        id: "demo_controller".to_string(),
        default: true,
        enabled: true,
        controller: ControllerKind::DryRun(DryRunConfig {
            simulate_runs: true,
        }),
    }];

    // Four zones keyed to the SAME slugs synth_irrigation() emits, so the
    // config-driven settings/Devices views and the live snapshot line up.
    let base = ZoneConfig {
        max_run_minutes: DEMO_MAX_RUN_MINUTES,
        display_name: String::new(),
        area_sqft: 1500.0,
        species: GrassSpecies::Bermuda,
        soil_texture: SoilTexture::SandyLoam,
        slope_pct: 0.0,
        sun_exposure: SunExposure::Full,
        sprinkler_type: SprinklerType::Spray,
        precip_rate_mm_hr: None,
        precip_rate_source: PrecipRateSource::Catalog,
        root_depth_mm: None,
        mad_pct_override: None,
        controller_id: "demo_controller".to_string(),
        controller_station: String::new(),
        controller_zone_name: None,
        soil_sensor_id: None,
        target_min_pct_soil: 30.0,
        saturation_pct_soil: 70.0,
        photo_url: None,
        weekly_budget_in: None,
        sessions_per_week: None,
        rain_credit_cap_in: None,
        scheduling_model: None,
    };
    // Each zone binds a soil channel on the seeded gateway
    // (`source:ecowitt:soilmoisture_<slug>`, the same channel ids the demo
    // feeder's sensor-history heartbeat produces), so the zone editor shows a
    // bound probe, and /api/health recognizes the gateway as the soil OWNER
    // (an `active` source) instead of an idle receiver.
    let soil = |slug: &str| Some(format!("source:ecowitt:soilmoisture_{slug}"));
    let mut zones = BTreeMap::new();
    zones.insert(
        "back_yard".to_string(),
        ZoneConfig {
            display_name: "Back Yard".to_string(),
            area_sqft: 2200.0,
            controller_station: "1".to_string(),
            controller_zone_name: None,
            soil_sensor_id: soil("back_yard"),
            // The balance showcase: a 1.75 in week over 2 sessions wants
            // more per session than the 60 minute cap can deliver.
            weekly_budget_in: Some(1.75),
            sessions_per_week: Some(2),
            ..base.clone()
        },
    );
    zones.insert(
        "front_yard".to_string(),
        ZoneConfig {
            display_name: "Front Yard".to_string(),
            area_sqft: 1800.0,
            species: GrassSpecies::StAugustine,
            controller_station: "2".to_string(),
            controller_zone_name: None,
            soil_sensor_id: soil("front_yard"),
            ..base.clone()
        },
    );
    zones.insert(
        "side_yard".to_string(),
        ZoneConfig {
            display_name: "Side Yard".to_string(),
            area_sqft: 900.0,
            sprinkler_type: SprinklerType::Rotor,
            controller_station: "3".to_string(),
            controller_zone_name: None,
            soil_sensor_id: soil("side_yard"),
            weekly_budget_in: Some(1.3),
            sessions_per_week: Some(2),
            // The demo's soil-governed zone: the per-zone pin puts the
            // soil model in charge of side_yard while the engine default
            // stays weekly, so the zone editor shows the override, the
            // zone detail shows the governing badge, and the snapshot
            // carries a live soil reason (`apply_demo_soil_plans`). Its
            // explicit 1.3 in weekly target doubles as the delivery
            // ceiling the soil model honors, loudly.
            scheduling_model: Some(crate::config::schema::SchedulingModel::Soil),
            ..base.clone()
        },
    );
    zones.insert(
        "back_yard_shrubs".to_string(),
        ZoneConfig {
            display_name: "Back Yard Shrubs".to_string(),
            area_sqft: 600.0,
            species: GrassSpecies::OrnamentalShrubs,
            sprinkler_type: SprinklerType::Drip,
            controller_station: "4".to_string(),
            controller_zone_name: None,
            soil_sensor_id: soil("back_yard_shrubs"),
            ..base.clone()
        },
    );
    cfg.zones = zones;

    cfg
}

fn synth_tempest(t_sim: f64) -> TempestSnapshot {
    let day_phase = (t_sim / 86400.0) * std::f64::consts::TAU;
    let solar_norm = (day_phase - std::f64::consts::FRAC_PI_2).sin();
    let solar = if solar_norm > 0.0 {
        solar_norm * 950.0
    } else {
        0.0
    };
    let temp_c = 27.0 + 5.0 * (day_phase - 0.4 * std::f64::consts::TAU).sin();
    let temp_f = temp_c * 9.0 / 5.0 + 32.0;
    let rh = (75.0 - 15.0 * (day_phase - 0.4 * std::f64::consts::TAU).sin()).clamp(35.0, 95.0);
    let wind_mph = 4.5 + 2.5 * (day_phase * 2.5).sin().abs();
    let gust_mph = wind_mph + 3.0;
    let dew_f = temp_f - (100.0 - rh) / 5.0;
    let now = chrono::Utc::now().timestamp();
    TempestSnapshot {
        last_packet_epoch: now,
        air_temp_f: temp_f,
        feels_like_f: if temp_f > 80.0 && rh > 40.0 {
            temp_f + (rh - 40.0) * 0.1
        } else {
            temp_f
        },
        dew_point_f: dew_f,
        wet_bulb_f: temp_f - (temp_f - dew_f) * 0.4,
        rh_pct: rh,
        pressure_inhg: 30.05 + (day_phase * 0.5).sin() * 0.05,
        pressure_trend_inhg: (0..72)
            .map(|i| {
                (
                    now - (72 - i) as i64 * 300,
                    30.05 + ((i as f64 / 12.0).sin() * 0.04),
                )
            })
            .collect(),
        wind_lull_mph: (wind_mph - 1.5).max(0.0),
        wind_avg_mph: wind_mph,
        wind_gust_mph: gust_mph,
        wind_dir_deg: (180.0 + 60.0 * (day_phase * 0.5).sin()).rem_euclid(360.0),
        rapid_wind_mph: wind_mph + (day_phase * 7.0).sin() * 0.6,
        rapid_wind_dir: (180.0 + 60.0 * (day_phase * 0.5).sin()).rem_euclid(360.0),
        illuminance_lx: solar * 130.0,
        uv_index: (solar / 100.0).clamp(0.0, 11.0),
        solar_w_m2: solar,
        rain_in_last_min: 0.0,
        rain_in_today: 0.0,
        rain_intensity_in_hr: 0.0,
        et0_today: 3.5 + (day_phase * 0.5).sin() * 1.0,
        flow_gpm: 0.0,
        flow_total_gal_today: 0.0,
        pop_pct: Some((20.0 + 30.0 * (day_phase * 0.7).sin()).clamp(0.0, 100.0)),
        leaf_wetness_pct: Some((35.0 + 35.0 * (day_phase * 0.6 + 1.0).sin()).clamp(0.0, 100.0)),
        precip_type: 0,
        lightning_count_last_min: 0,
        lightning_strikes_last_hour: 0,
        lightning_recent: Vec::new(),
        lightning_avg_dist_mi: None,
        last_strike_distance_mi: None,
        last_strike_epoch: None,
        battery_v: 2.68,
        battery_pct: TempestSnapshot::battery_pct_from_v(2.68),
        station_serial: "ST-DEMO0001".into(),
        hub_serial: "HB-DEMO0001".into(),
        source_label: "Demo".into(),
        owner_priority: 50,
        // Demo is a live station: all engine-critical fields are fresh.
        air_temp_live_epoch: now,
        wind_live_epoch: now,
        rh_live_epoch: now,
        rain_live_epoch: now,
        // Demo presents as a real live local station (serial + battery), so the
        // display reads it as a station, not cloud-only.
        has_live_station: true,
        // The demo accumulator is always today's.
        rain_today_day_ordinal: crate::timeutil::local_day_ordinal(now),
    }
}

fn synth_irrigation(t_sim: f64) -> IrrigationSnapshot {
    let now = chrono::Utc::now().timestamp();
    let day_phase = (t_sim / 86400.0) * std::f64::consts::TAU;

    let zones = vec![
        synth_zone(
            "back_yard",
            "Back Yard",
            // Eight days back: the balance showcase has back_yard behind
            // on its week (capped sessions could not keep up), so its
            // last completed session predates the trailing window.
            now - 8 * 86_400,
        ),
        synth_zone("front_yard", "Front Yard", now - 24 * 3600),
        synth_zone("side_yard", "Side Yard", now - 36 * 3600),
        synth_zone("back_yard_shrubs", "Back Yard Shrubs", now - 48 * 3600),
    ];

    // Five showcase phases per synthetic day. Every baked reason uses the
    // ENGINE's exact wording (the formats `reason_render::render_skip_reason`
    // reconstructs), and the SkipCheck operands below are phase-matched to the
    // numbers in these strings, so the hero's unit-aware re-render and every
    // surface showing the baked string agree byte for byte (the
    // `demo_reasons_match_the_unit_renderer` test pins this).
    let phase = (t_sim / 86400.0 * 5.0) % 5.0;
    let (verdict, reason) = if phase < 1.0 {
        ("run", String::new())
    } else if phase < 2.0 {
        ("skip", "Rain expected within 4h (0.18\" forecast)".into())
    } else if phase < 3.0 {
        (
            "run_extended",
            "Heat advisory: running planned + 15% (peak 97\u{b0}F)".into(),
        )
    } else if phase < 4.0 {
        ("skip", "Currently raining (0.05 in/hr)".into())
    } else {
        (
            "skip",
            "Tomorrow rain (0.40\" \u{d7} 85% confidence)".into(),
        )
    };
    let raining_now = verdict == "skip" && reason.starts_with("Currently");
    // Rain expected within the next 4 hours only during the phase whose
    // verdict cites it; a standing 0.18" would contradict the "run" phases.
    let rain_next_4h_in = if reason.starts_with("Rain expected within 4h") {
        0.18
    } else {
        0.0
    };

    let mut snap = IrrigationSnapshot::default();
    snap.last_refresh_epoch = now;
    snap.ha_reachable = true;
    // API contract (snapshot.rs): the override enums are never empty strings.
    // The engine's defaults are "auto" (sticky global + per-zone) and "none"
    // (the one-day tomorrow override); the demo serves the same vocabulary so
    // the manifest-driven HA sensors and any curl evaluation see in-contract
    // values, exactly like prod.
    snap.timezone = "America/New_York".to_string();
    snap.global_override = "auto".to_string();
    snap.override_tomorrow = "none".to_string();
    // Native (standalone) deployments always handle the pause + override
    // actions themselves; mirror the refresher's native posture.
    snap.override_helpers_present = true;
    snap.master_enable = true;
    snap.iu_enabled = true;
    // The demo controller poses as OpenSprinkler-class hardware: it reports a
    // real water level, so the capability flag and a Some value both ride.
    snap.water_level_pct = Some(100.0);
    snap.water_level_capable = true;
    snap.next_run_epoch = now + 6 * 3600;
    // next_run_total_minutes is summed from the zone plans once the balance
    // rows exist, below.
    snap.zones = zones;
    snap.skip_check = SkipCheck {
        temp_now_f: 82.0,
        wind_now_mph: 5.5,
        rain_today_in: 0.0,
        rain_intensity_now_in_hr: if raining_now { 0.05 } else { 0.0 },
        humidity_now_pct: 62.0,
        // Tomorrow's rain matches the forecast seed (daily[1]: 0.40" at 85%)
        // AND the tomorrow-rain phase reason above, so the hero re-render and
        // the baked string carry the same operands.
        forecast_in: 0.40,
        rain_tomorrow_prob_pct: Some(85),
        rain_3day_weighted_in: 0.42,
        rain_7day_weighted_in: 0.95,
        rain_next_4h_in,
        rain_observed_recent_in: 0.0,
        wind_max_today_mph: 8.0,
        temp_min_24h_f: 71.0,
        temp_min_24h_valid: true,
        temp_max_3day_f: 97.0,
        days_since_significant_rain: 2,
        heat_index_now_f: 88.0,
        heat_index_max_3day_f: 109.0,
        max_wind_mph: 10.0,
        min_temp_f: 38.0,
        rain_skip_in: 0.25,
        // The demo shows a stock yard, so its threshold marks are the
        // schema's own defaults rather than numbers typed again here.
        already_wet_in: crate::config::schema::SkipRuleParams::default().already_wet_in,
        wind_forecast_slack_mph: crate::config::schema::SkipRuleParams::default()
            .wind_forecast_slack_mph,
        rain_observed_window_days: crate::config::schema::SkipRuleParams::default()
            .rain_observed_window_days,
        rain_next_4h_skip_in: crate::config::schema::SkipRuleParams::default().rain_next_4h_skip_in,
        soil_fields: std::collections::BTreeMap::from([
            ("soil_back_yard_pct".to_string(), Some(42.0)),
            ("soil_front_yard_pct".to_string(), Some(48.0)),
            ("soil_side_yard_pct".to_string(), Some(50.0)),
            ("soil_back_yard_shrubs_pct".to_string(), Some(55.0)),
            ("saturation_back_yard_pct".to_string(), Some(70.0)),
            ("saturation_front_yard_pct".to_string(), Some(70.0)),
            ("saturation_side_yard_pct".to_string(), Some(70.0)),
            ("saturation_back_yard_shrubs_pct".to_string(), Some(85.0)),
        ]),
        soil_temp_yard_min_f: Some(74.0),
        soil_temp_yard_max_f: Some(82.0),
        frost_skip_soil_f: 35.0,
        is_paused: false,
        is_dry_run: false,
        will_skip: verdict == "skip",
        verdict: verdict.to_string(),
        // P1: derive the demo reason_code from the same classifier the scoreboard
        // uses, so the synthetic SkipCheck carries a coherent code (rain_next_4h /
        // heat_advisory / rain_now / tomorrow_rain / "run") without hand-listing.
        reason_code: crate::persistence::verdict_history::classify_reason_code(verdict, &reason),
        reason,
    };
    // Forecast block, mirroring the SkipCheck operands + the synth_forecast
    // seed (tomorrow: 0.40" at 85%) so every surface reads one coherent story.
    // A default (all-zero) block here violated the contract the dashboard and
    // HA manifest sensors read: empty forecast source, 0.0 ET0, 0 gust.
    snap.forecast = Forecast {
        rain_today_tempest_in: 0.0,
        rain_today_om_in: 0.0,
        station_source_label: "Demo".to_string(),
        forecast_source_label: "Open-Meteo (demo)".to_string(),
        rain_intensity_in_hr: if raining_now { 0.05 } else { 0.0 },
        rain_type: if raining_now { "rain" } else { "none" }.to_string(),
        // The demo presents as a live local station (serial + battery), so its
        // rain reading is an observation-grade gauge read, not a model fill.
        rain_is_live: true,
        rain_nature: RainNature::Measured,
        rain_tomorrow_in: 0.40,
        rain_3day_in: 0.40,
        eto_today_mm: Some(3.5 + (day_phase * 0.5).sin() * 1.0),
        eto_tomorrow_mm: 4.4,
        eto_3day_avg_mm: 4.2,
        temp_max_today_f: Some(88.0),
        temp_min_today_f: Some(71.0),
        wind_max_today_mph: 8.0,
        wind_gust_today_mph: 12.0,
        humidity_mean_today_pct: Some(65.0),
        rain_3day_weighted_in: 0.42,
        rain_7day_weighted_in: 0.95,
        rain_next_4h_in,
        rain_tomorrow_prob_pct: Some(85),
        temp_min_24h_f: 71.0,
        temp_max_3day_f: 97.0,
        humidity_now_pct: 62.0,
        heat_index_now_f: 88.0,
        heat_index_max_3day_f: 109.0,
        // Matches the per-zone ZoneMath heat_mult so the math tile agrees.
        heat_multiplier: 1.15,
        days_since_significant_rain: 2,
        // Extended model context: showcase values consistent with the demo's
        // "warm early evening after a dry spell" story (about half the day's
        // ET spent, VPD elevated but under the 1.6 kPa stress line, root
        // zone drying over the next two days).
        eto_spent_today_mm: 2.1 + (day_phase * 0.5).sin() * 0.6,
        vpd_now_kpa: 1.25,
        vpd_max_today_kpa: 1.52,
        soil_temp_6cm_now_f: 79.0,
        soil_moisture_3_9_now_vwc: 0.19,
        soil_moisture_3_9_in48h_vwc: 0.16,
    };
    snap.seven_day_verdicts = synth_seven_day_verdicts(now);
    snap.soil_forecasts = synth_soil_forecasts();
    snap.water_budgets = synth_water_budgets(
        now,
        crate::engine::calendar::Calendar {
            local_date: crate::timeutil::local_date,
            day_bounds_utc: crate::timeutil::local_day_bounds_utc,
        },
    );
    // The demo's engine default stays weekly with side_yard pinned to
    // soil, so the pinned zone's model chip (rendered only where a
    // zone's model differs from this baseline) shows in screenshots.
    snap.engine_scheduling_model = "weekly".to_string();
    // The soil-model pass, mirroring the live refresher's order: shadow
    // plans on every row, side_yard governed, BEFORE the plan copy below
    // so the governed swap reaches planned_run_seconds like any other
    // today figure.
    apply_demo_soil_plans(&mut snap.water_budgets);
    // Tonight's minutes come from the demo's own balance rows, exactly as
    // `apply_budget_plan` fills them on a live install: planned seconds are
    // the row's `today_seconds`, and the cap row goes amber only when the
    // run SITS ON the ceiling because the allocator wanted more. The demo
    // used to hand-set both numbers, which put a 20 minute run under a
    // "capped at 60 min" label and left three zones showing minutes their
    // own balance rows had already zeroed.
    //
    // `today_run_minutes` is NOT among them: nothing summarizes per-zone
    // valve-open seconds since local midnight, here or on a live install,
    // so it stays absent and the demo's Today tile shows the same dash a
    // real one does.
    //
    // The demo has no seasonal dial and no Override schedule, so those two
    // terms of the live predicate are constant here.
    let plan: std::collections::HashMap<String, (u32, bool)> = snap
        .water_budgets
        .iter()
        .map(|b| (b.zone_slug.clone(), (b.today_seconds, b.session_capped)))
        .collect();
    // The deficit tiles read the same producer a live install reads: the
    // soil block on the budget rows, published under the field's
    // documented sign (negative = needs water).
    let bucket: std::collections::HashMap<String, f64> = snap
        .water_budgets
        .iter()
        .filter_map(|b| b.soil_depletion_mm.map(|d| (b.zone_slug.clone(), -d)))
        .collect();
    for z in snap.zones.iter_mut() {
        let (today_s, session_capped) = plan.get(&z.slug).copied().unwrap_or((0, false));
        z.planned_run_seconds = today_s;
        z.bucket_mm = bucket.get(&z.slug).copied();
        if let Some(m) = z.math.as_mut() {
            m.bucket_mm = z.bucket_mm;
            m.scheduled_seconds = today_s;
            m.cap_binding =
                m.max_duration_seconds > 0 && today_s == m.max_duration_seconds && session_capped;
        }
    }
    // The hero total is the sum of what the zones actually plan, not a
    // hand-typed figure that drifts from them.
    snap.next_run_total_minutes = snap
        .zones
        .iter()
        .map(|z| z.planned_run_seconds as f64)
        .sum::<f64>()
        / 60.0;
    snap
}

/// The demo zones ship without a per-zone run-limit override, so both the
/// seeded config ([`seed_config`]) and the synthetic snapshot math
/// ([`synth_zone`]) resolve the documented 60 minute default from one place.
const DEMO_MAX_RUN_MINUTES: Option<u32> = None;

/// A demo zone WITHOUT its run length. Today's minutes come from the demo's
/// own balance rows in `synth_irrigation`, the same way `apply_budget_plan`
/// fills them on a live install; a hand-set number here is a run nothing
/// sized, which is the thing this release exists to stop printing.
fn synth_zone(slug: &str, name: &str, last_run: i64) -> ZoneState {
    let mut z = ZoneState::default();
    z.name = name.into();
    z.slug = slug.into();
    // Contract: "auto" | "skip" | "run", never empty. (`ZoneState::default()`
    // yields an empty string; the serde default only applies on deserialize.)
    z.override_mode = "auto".into();
    // The demo shows what a real install shows. Absent here as a pre-plan
    // placeholder; `apply_demo_soil_plans` publishes the replayed deficit
    // through the budget rows and the copy loop in `synth_irrigation`
    // fills this from them, the same order the live refresher uses.
    z.bucket_mm = None;
    // Pre-plan placeholder, overwritten from the balance row. Mirrors
    // `refresh_once`, which also builds the zone at 0 and lets
    // `apply_budget_plan` set the real figure.
    z.planned_run_seconds = 0;
    z.last_run_epoch = last_run;
    z.math = Some(ZoneMath {
        bucket_mm: None,
        // The zone's own species curve at midsummer, not a slug guess.
        kc: crate::engine::kc_at_doy_lat(demo_zone_species(slug), 196, 28.54),
        throughput_mm_hr: 14.2,
        heat_mult: 1.15,
        capture_eff: 0.70,
        raw_seconds: 0,
        max_duration_seconds: DEMO_MAX_RUN_MINUTES.unwrap_or(60) * 60,
        scheduled_seconds: 0,
        cap_binding: false,
    });
    z
}

fn synth_seven_day_verdicts(now: i64) -> Vec<DayVerdict> {
    // Reasons use the ENGINE's exact baked wording (the strip runs the real
    // rule ladder per day on a live deployment, so its strings always take
    // these shapes): a rainy cell is "Already wet ({:.2}\" today)" against its
    // own precip, and the heat cell is the heat-advisory format with the same
    // 97 peak the SkipCheck carries.
    // Daily highs tell the same story the reasons do: a cooler rain day, then
    // a three-day heat wave peaking at the 97 the heat-advisory cell (and the
    // SkipCheck's temp_max_3day_f) cite, then settling back.
    let highs = [88.0, 86.0, 95.0, 97.0, 96.0, 90.0, 88.0];
    let verdicts = [
        ("run", "", 7u32),
        ("skip", "Already wet (0.40\" today)", 80),
        (
            "run_extended",
            "Heat advisory: running planned + 15% (peak 97\u{b0}F)",
            2,
        ),
        ("run", "", 1),
        ("skip", "Heavy rain in next 3 days (0.62\" weighted)", 80),
        ("run", "", 2),
        ("run", "", 1),
    ];
    verdicts
        .iter()
        .enumerate()
        .map(|(i, (v, r, w_code))| {
            let mut d = DayVerdict::default();
            d.day_offset = i as u32;
            d.time_epoch = now + (i as i64) * 86400;
            d.weather_code = *w_code;
            d.temp_max_f = highs[i];
            d.temp_min_f = 71.0 + (i as f64) * 0.2;
            d.precip_in = if v.starts_with("skip") { 0.4 } else { 0.0 };
            d.precip_probability_max = Some(if v.starts_with("skip") { 85 } else { 15 });
            d.verdict = v.to_string();
            d.reason = r.to_string();
            // Same classifier the live strip's codes come from, so the cell
            // icons + unit-aware rendering key correctly.
            d.reason_code = crate::persistence::verdict_history::classify_reason_code(v, r);
            d
        })
        .collect()
}

fn synth_soil_forecasts() -> Vec<SoilForecast> {
    let zones = [
        ("back_yard", "Back Yard", 42.0, 30.0, 70.0),
        ("front_yard", "Front Yard", 48.0, 30.0, 70.0),
        ("side_yard", "Side Yard", 50.0, 30.0, 70.0),
        ("back_yard_shrubs", "Back Yard Shrubs", 55.0, 25.0, 85.0),
    ];
    zones
        .iter()
        .map(|(slug, name, start, tmin, tmax)| {
            let predicted: Vec<f64> = (0..7)
                .map(|d| (start - d as f64 * 4.5).clamp(0.0, 100.0))
                .collect();
            let min_pred = predicted.iter().copied().fold(100.0_f64, f64::min);
            let max_pred = predicted.iter().copied().fold(0.0_f64, f64::max);
            let days_below = predicted.iter().filter(|p| **p <= *tmin).count() as u32;
            let mut s = SoilForecast::default();
            s.zone_slug = slug.to_string();
            s.zone_name = name.to_string();
            s.current_pct = Some(*start);
            s.target_min_pct = *tmin;
            s.target_max_pct = *tmax;
            s.predicted_pct = predicted;
            s.min_predicted_pct = min_pred;
            s.max_predicted_pct = max_pred;
            s.days_below_target = days_below;
            s.days_above_max = 0;
            s.status = if days_below >= 2 {
                "dry".into()
            } else {
                "ok".into()
            };
            s
        })
        .collect()
}

fn synth_water_budgets(now: i64, cal: crate::engine::calendar::Calendar) -> Vec<WaterBudget> {
    // The demo rows run through the LIVE balance implementation
    // (engine::budget::compute_zone) with fabricated trailing evidence,
    // so the wire fields, reasons, and formula can never drift from the
    // engine. The story showcases every today-reason branch:
    //   back_yard: behind on its 1.75 in week (no session in the
    //     trailing window), so its half-week share is 22.2 mm = 5635 s
    //     at 14.2 mm/hr, over the 60 minute cap (the cap-raise
    //     recommendation, rounded up to 95 minutes).
    //   front_yard: a small remainder but spaced (last session 1 day
    //     back at a 3 day interval).
    //   side_yard: spaced likewise.
    //   back_yard_shrubs: covered by prior watering.
    // Observed rain reads 0.00 in with GAUGE provenance: the demo's
    // seeded ledger holds gauge rows for the trailing week (a measured
    // dry spell), and measured coverage always outranks the forecast
    // archive's regional 0.25 in from three days back.
    const THROUGHPUT_MM_HR: f64 = 14.2;
    let cap_s = DEMO_MAX_RUN_MINUTES.unwrap_or(60) * 60;
    let globals = crate::engine::BalanceGlobals {
        now_epoch: now,
        // The caller's calendar: the live demo runs on the deployment's,
        // its test pins UTC so the fabricated story lands on the same
        // numbers on every machine.
        calendar: cal,
        session_rain_defer_in: crate::engine::SESSION_RAIN_DEFER_IN,
        observed_rain_mm: 0.0,
        observed_rain_source: "gauge".to_string(),
        // A measured dry week has no covered rain days to clip.
        observed_rain_days_mm: Vec::new(),
        bias: crate::engine::BiasModel::identity(),
    };
    // The balance settles against trailing evidence only; the showcase
    // forecast block is display data (its rain would otherwise trip the
    // defer gate every synthetic day).
    let fc = ForecastSnapshot::default();
    let zones: [(&str, &str, f64, u32, i64, i64, u32); 4] = [
        // (slug, name, weekly_in, sessions/wk, last_run_epoch,
        //  trailing valve-open seconds, sessions done in the window)
        ("back_yard", "Back Yard", 1.75, 2, now - 8 * 86_400, 0, 0),
        (
            "front_yard",
            "Front Yard",
            1.00,
            2,
            now - 24 * 3600,
            5400,
            3,
        ),
        ("side_yard", "Side Yard", 1.30, 2, now - 36 * 3600, 5400, 3),
        (
            "back_yard_shrubs",
            "Back Yard Shrubs",
            0.50,
            1,
            now - 48 * 3600,
            3960,
            3,
        ),
    ];
    zones
        .iter()
        .map(|(slug, name, weekly, sessions, last_run, open_s, done)| {
            let inputs = crate::engine::ZoneBalanceInputs {
                slug: slug.to_string(),
                name: name.to_string(),
                weekly_budget_in: *weekly,
                sessions_per_week: *sessions,
                mode_active: true,
                throughput_mm_hr: THROUGHPUT_MM_HR,
                max_dur_s: cap_s,
                last_run_epoch: *last_run,
                applied_trailing_mm: *open_s as f64 / 3600.0 * THROUGHPUT_MM_HR,
                sessions_done: *done,
                // The demo sets a weekly target per zone above, so no zone
                // here is watering on an inferred one and the Zones page's
                // default-target banner stays quiet in the demo.
                target_inferred: false,
                // The derived cap for a sandy-loam yard: default turf
                // roots on the turf zones, shrub roots on the bed. The
                // demo's measured week is dry, so nothing clips and no
                // demo number moves.
                rain_cap_mm: crate::engine::taw_mm(
                    crate::config::schema::SoilTexture::SandyLoam,
                    crate::engine::species_profile(demo_zone_species(slug)).root_depth_mm,
                ),
                rain_cap_inferred: true,
            };
            crate::engine::compute_zone_balance(&inputs, &globals, &fc)
        })
        .collect()
}

/// The demo's soil-model pass, the same shape `apply_soil_schedule`
/// gives a live install: every zone gets a shadow plan through the LIVE
/// planner (`engine::soil_schedule::plan_zone`) over fabricated
/// evidence, each budget row is tagged with its governing model, and
/// side_yard (pinned to the soil model in the seeded config) has its
/// today figures swapped for the soil plan's via the shared
/// `today_row` formatter, so the governing badge, the soil block, and a
/// soil reason all show live in screenshots.
///
/// Determinism: every evidence day carries `et0_mm: None`, so the
/// replay charges the FALLBACK rung's fixed daily mean (the explicit
/// The species each demo zone is planted with, the same assignment
/// `seed_config` writes into its zone blocks. The synthesized snapshot
/// used to guess a zone's agronomy from words in its slug while its own
/// config declared the species right there, so the demo showed numbers
/// its own settings pages contradicted.
pub(crate) fn demo_zone_species(slug: &str) -> crate::config::schema::GrassSpecies {
    use crate::config::schema::GrassSpecies;
    match slug {
        "front_yard" => GrassSpecies::StAugustine,
        "back_yard_shrubs" => GrassSpecies::OrnamentalShrubs,
        _ => GrassSpecies::Bermuda,
    }
}

/// weekly target spread over seven days, else the species-class
/// climatology) and no demo number moves with the season. side_yard's
/// story: yesterday's session partially refilled the bucket, today's
/// drying pushed depletion back over the trigger, the full refill wants
/// more than the 60-minute cap, and the zone's explicit 1.3 in weekly
/// target clamps today's delivery to the remaining rolling-7-day
/// headroom, with the ceiling reason naming delivered, target, and
/// headroom.
fn apply_demo_soil_plans(budgets: &mut [WaterBudget]) {
    use crate::config::schema::{GrassSpecies, SoilTexture};
    use crate::engine::soil_schedule::{plan_zone, today_row, ZoneDayEvidence, ZoneSoilParams};
    const THROUGHPUT_MM_HR: f64 = 14.2;
    let today = crate::timeutil::now_local().date_naive();
    let dates: Vec<chrono::NaiveDate> = (0..14)
        .map(|i| today - chrono::Duration::days(13 - i))
        .collect();
    // (slug, species, weekly target, rain mm by day index, valve seconds
    // by day index). Day 13 is today.
    type DayVals = &'static [(usize, f64)];
    let stories: [(&str, GrassSpecies, Option<f64>, DayVals, DayVals); 4] = [
        // Dry two weeks, last session outside the window: the bucket sits
        // at capacity depleted, the shadow refill rides the cap.
        (
            "back_yard",
            GrassSpecies::Bermuda,
            Some(1.75),
            &[],
            &[(5, 5400.0)],
        ),
        // Watered yesterday; today's drying has it just over the trigger.
        (
            "front_yard",
            GrassSpecies::StAugustine,
            None,
            &[],
            &[(12, 5400.0)],
        ),
        // The soil-governed zone: watered yesterday, over the trigger
        // today, ceiling-clamped by its explicit weekly target.
        (
            "side_yard",
            GrassSpecies::Bermuda,
            Some(1.3),
            &[],
            &[(12, 5400.0)],
        ),
        // Rain-refilled bed drying slowly: holds, with days of margin.
        (
            "back_yard_shrubs",
            GrassSpecies::OrnamentalShrubs,
            None,
            &[(8, 30.0)],
            &[(11, 3960.0)],
        ),
    ];
    for (slug, species, weekly_in, rain_days, applied_days) in stories {
        let params = ZoneSoilParams {
            slug: slug.to_string(),
            species,
            texture: SoilTexture::SandyLoam,
            root_depth_mm: None,
            mad_pct: None,
            latitude_deg: 28.5,
            capture_efficiency: 0.70,
            throughput_mm_hr: THROUGHPUT_MM_HR,
            max_dur_s: DEMO_MAX_RUN_MINUTES.unwrap_or(60) * 60,
            explicit_rain_cap_mm: None,
            explicit_weekly_budget_in: weekly_in,
        };
        let evidence: Vec<ZoneDayEvidence> = dates
            .iter()
            .enumerate()
            .map(|(i, date)| ZoneDayEvidence {
                date: *date,
                et0_mm: None,
                gross_rain_mm: rain_days
                    .iter()
                    .find(|(d, _)| *d == i)
                    .map(|(_, v)| *v)
                    .unwrap_or(0.0),
                applied_valve_s: applied_days
                    .iter()
                    .find(|(d, _)| *d == i)
                    .map(|(_, v)| *v as i64)
                    .unwrap_or(0),
            })
            .collect();
        // Trailing gross delivery for the explicit weekly ceiling: the
        // valve seconds the evidence window's last 7 days carry, the same
        // arithmetic the live assembly uses.
        let delivered_7d_mm = applied_days
            .iter()
            .filter(|(d, _)| *d >= 7)
            .map(|(_, s)| *s / 3600.0 * THROUGHPUT_MM_HR)
            .sum::<f64>();
        let plan = plan_zone(&params, &evidence, 0.0, delivered_7d_mm);
        let Some(b) = budgets.iter_mut().find(|b| b.zone_slug == slug) else {
            continue;
        };
        let governed = slug == "side_yard";
        b.scheduling_model = if governed { "soil" } else { "weekly" }.to_string();
        b.soil_depletion_mm = Some(plan.depletion_mm);
        b.soil_taw_mm = Some(plan.taw_mm);
        b.soil_raw_mm = Some(plan.raw_mm);
        b.soil_due = plan.due;
        b.soil_planned_seconds = plan.planned_seconds;
        b.soil_deferred_reason = plan.deferred_reason.clone();
        b.soil_ceiling_binding = plan.ceiling_binding;
        if governed {
            let (today_seconds, today_reason, session_capped) =
                today_row(&plan, DEMO_MAX_RUN_MINUTES.unwrap_or(60));
            b.today_seconds = today_seconds;
            b.today_reason = today_reason;
            b.session_capped = session_capped;
        }
    }
}

fn synth_forecast() -> ForecastSnapshot {
    let now = chrono::Utc::now().timestamp();
    // Same daily highs as synth_seven_day_verdicts, so the forecast page and
    // the verdict strip tell one story (rain day, heat wave peaking at 97).
    let highs = [88.0, 86.0, 95.0, 97.0, 96.0, 90.0, 88.0];
    let daily: Vec<DailyEntry> = (0..7)
        .map(|d| {
            let mut e = DailyEntry::default();
            e.time_epoch = now + d * 86400;
            e.weather_code = if d == 1 || d == 4 { 80 } else { 2 };
            e.temp_max_f = highs[d as usize];
            e.temp_min_f = 71.0 + (d as f64) * 0.2;
            e.precip_sum_in = if d == 1 {
                0.4
            } else if d == 4 {
                0.6
            } else {
                0.0
            };
            e.precip_probability_max = Some(if d == 1 {
                85
            } else if d == 4 {
                70
            } else {
                15
            });
            e.wind_max_mph = 8.0;
            e.uv_index_max = 9.0;
            e.sunrise_epoch = now + d * 86400 + 6 * 3600;
            e.sunset_epoch = now + d * 86400 + 19 * 3600;
            e
        })
        .collect();
    let hourly: Vec<HourlyEntry> = (0..48)
        .map(|h| {
            let mut e = HourlyEntry::default();
            e.time_epoch = now + h * 3600;
            e.temp_f = 75.0 + 8.0 * ((h as f64) / 24.0 * std::f64::consts::TAU).sin();
            e.precip_in = if h > 2 && h < 6 { 0.04 } else { 0.0 };
            e.precip_probability = Some(if h > 2 && h < 6 { 75 } else { 10 });
            e.wind_mph = 5.0;
            e.humidity_pct = 60;
            e.weather_code = 2;
            e
        })
        .collect();

    let mut f = ForecastSnapshot::default();
    f.last_refresh_epoch = now;
    f.source_reachable = true;
    f.source_label = "Demo".into();
    f.timezone = "America/New_York".into();
    f.daily = daily;
    f.hourly = hourly;
    f.past_daily = (0..7)
        .map(|d| {
            let mut e = DailyEntry::default();
            e.time_epoch = now - (7 - d) * 86400;
            e.precip_sum_in = if d == 4 { 0.25 } else { 0.0 };
            e.temp_max_f = 86.0;
            e.temp_min_f = 70.0;
            e
        })
        .collect();
    f
}

/// Seed ~30 days of plausible runs, skips, and decisions so the demo's
/// History page (run log, charts, calendar, skip breakdown) tells a
/// story instead of rendering empty states, plus the rain observations
/// and per-zone probe curves the tuning report reads, so the public demo
/// renders that feature populated too. Idempotent: only fires when the
/// runs table is empty. Deterministic: every value keys off the day
/// index (no RNG).
async fn seed_history(conn: Arc<tokio::sync::Mutex<rusqlite::Connection>>) {
    use crate::history::db::{record_decision, record_run};
    use crate::history::types::{DecisionRecord, RunRecord};

    let existing: i64 = {
        let c = conn.lock().await;
        c.query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
            .unwrap_or(0)
    };
    if existing > 0 {
        return;
    }
    info!("demo_data: seeding 30 days of synthetic history");

    let zones: [(&str, i64); 4] = [
        ("back_yard", 3600),
        ("front_yard", 1800),
        ("side_yard", 1800),
        ("back_yard_shrubs", 1320),
    ];
    let now = chrono::Utc::now().timestamp();
    let day = 86_400i64;
    // Per-zone completed watering intervals, collected while seeding so
    // the probe curves below can bracket the SAME events the runs table
    // records (rises right after each run; steady drying between).
    let mut watered: std::collections::HashMap<&str, Vec<(i64, i64)>> =
        std::collections::HashMap::new();
    // Deterministic pseudo-random pattern keyed by day index: roughly
    // every other day waters, with rain and wind skip days mixed in.
    for back in (1..=30).rev() {
        let midnight = now - back * day - (now % day);
        let start = midnight + 6 * 3600 + (back % 3) * 600;
        let kind = back % 7;
        match kind {
            2 => {
                let _ = record_decision(
                    conn.clone(),
                    DecisionRecord {
                        epoch: start,
                        verdict: "skip".into(),
                        reason: "Rain expected within 4h (0.31 in forecast)".into(),
                        trace: None,
                    },
                    String::new(),
                )
                .await;
                for (slug, _) in zones {
                    let _ = record_run(
                        conn.clone(),
                        RunRecord {
                            zone: slug.into(),
                            start_epoch: start,
                            duration_s: 0,
                            skip_reason: Some("Rain expected within 4h".into()),
                            source: String::new(),
                            status: String::new(),
                        },
                    )
                    .await;
                }
            }
            5 => {
                let _ = record_decision(
                    conn.clone(),
                    DecisionRecord {
                        epoch: start,
                        verdict: "skip".into(),
                        reason: "Wind 14 mph above 10 mph threshold".into(),
                        trace: None,
                    },
                    String::new(),
                )
                .await;
            }
            0 | 3 => {
                // Rest day: bucket still comfortable, no rows.
            }
            _ => {
                let _ = record_decision(
                    conn.clone(),
                    DecisionRecord {
                        epoch: start,
                        verdict: "run".into(),
                        reason: String::new(),
                        trace: None,
                    },
                    String::new(),
                )
                .await;
                let mut t = start;
                for (slug, dur) in zones {
                    // back_yard sits out the trailing week: the balance
                    // showcase has it behind on its target (its capped
                    // sessions could not keep up), which is exactly the
                    // state the cap-raise recommendation describes. Its
                    // earlier weeks still water normally so the 14-day
                    // report window has run days to evaluate.
                    if slug == "back_yard" && back <= 7 {
                        continue;
                    }
                    let jitter = ((back * 37 + t) % 90) - 45;
                    let d = (dur + jitter).max(300);
                    let _ = record_run(
                        conn.clone(),
                        RunRecord {
                            zone: slug.into(),
                            start_epoch: t,
                            duration_s: d,
                            skip_reason: None,
                            source: String::new(),
                            status: String::new(),
                        },
                    )
                    .await;
                    watered.entry(slug).or_default().push((t, t + d));
                    t += d + 300;
                }
            }
        }
    }
    seed_tuning_signals(conn, now, &watered).await;
}

/// Seed the tuning report's raw material: daily rain observations
/// (predicted vs observed, keyed by the same configured-tz day the
/// decisions land on) and per-zone soil-probe curves in sensor_history.
/// The curves are shaped so the report demos every state: back_yard is
/// session-capped (synth_water_budgets), so the top-ranked cap check
/// recommends raising its run limit (its probe curve still exercises the
/// backout math underneath), front_yard dries far slower than its
/// configured bucket predicts (the texture-drift recommendation),
/// side_yard reads healthy, and the shrubs' probe is too sparse to judge
/// (the not-enough-data lines).
async fn seed_tuning_signals(
    conn: Arc<tokio::sync::Mutex<rusqlite::Connection>>,
    now: i64,
    watered: &std::collections::HashMap<&str, Vec<(i64, i64)>>,
) {
    use crate::config::schema::GrassSpecies;
    use crate::persistence::sensor_history::Reading;
    use crate::sources::bus_recorder::zone_soil_key;
    use chrono::Datelike;

    let day = 86_400i64;
    // Daily predicted-vs-observed rain. Rain-skip days (day index % 7 ==
    // 2) carried a 0.31" forecast; the rain actually arrived on two of
    // them (backs 30 and 16, chosen OFF the 3-day dry gaps the drift
    // check needs) and missed on the rest, so the forecast-skip
    // scorecard scores 5 days with 2 confirmations.
    let obs_store = crate::persistence::ForecastObservationsStore::new(conn.clone());
    for back in (1..=30).rev() {
        let midnight = now - back * day - (now % day);
        let Some(date) = crate::timeutil::local_date(midnight + 12 * 3600) else {
            continue;
        };
        let (predicted, observed) = if back % 7 == 2 {
            let observed = match back {
                30 => 0.35,
                16 => 0.30,
                _ => 0.0,
            };
            (0.31, observed)
        } else {
            (0.0, 0.0)
        };
        // The demo presents a live station, so its observed rows are
        // gauge-provenance (and train the bias model like a real yard).
        if let Err(e) = obs_store.upsert(date, predicted, observed, "gauge").await {
            tracing::debug!("demo_data: forecast observation seed failed: {e}");
        }
    }

    // Model-side rates computed from the SAME seed the live report reads
    // (seed_config zones + synth_forecast temps), so the seeded slopes
    // stay in the intended ratio bands in any season.
    let cfg = seed_config();
    let fc = synth_forecast();
    let lat = cfg.deployment.location.lat;
    let base_doy = crate::timeutil::now_local().date_naive().ordinal() as u16;
    let mean_daily_etc = |species: GrassSpecies| -> f64 {
        let mut sum = 0.0;
        let mut n = 0u32;
        for (i, d) in fc.daily.iter().take(7).enumerate() {
            let doy = (base_doy + i as u16 - 1) % 366 + 1;
            if let Some(et0) = crate::refresher::native_et0_mm(d, lat, doy) {
                let kc = crate::engine::kc_at_doy_lat(species, doy, lat);
                sum += crate::engine::etc_mm(et0, kc, 1.0);
                n += 1;
            }
        }
        if n > 0 {
            sum / n as f64
        } else {
            4.0
        }
    };
    // Modeled drying slope (percent of bucket per day) per zone, from the
    // seeded species/texture defaults.
    let taw_back = crate::engine::taw_mm(crate::config::schema::SoilTexture::SandyLoam, 200.0);
    let taw_front = crate::engine::taw_mm(crate::config::schema::SoilTexture::SandyLoam, 150.0);
    let slope_back = mean_daily_etc(GrassSpecies::Bermuda) / taw_back * 100.0;
    let slope_front = 0.3 * mean_daily_etc(GrassSpecies::StAugustine) / taw_front * 100.0;

    let store = crate::persistence::SensorHistoryStore::new(conn);
    let win_start = now - 30 * day;
    let empty: Vec<(i64, i64)> = Vec::new();
    let curves: [(&str, f64, f64, i64); 4] = [
        // (slug, start value, drying slope %/day, reading cadence seconds)
        ("back_yard", 78.0, slope_back, 3600),
        ("front_yard", 62.0, slope_front, 3600),
        // side_yard reads flat and healthy: no drying signal, no rises,
        // so both probe checks report their honest insufficiency.
        ("side_yard", 50.0, 0.0, 3600),
        // The shrubs' probe reports twice a day: too sparse for any
        // stretch to qualify, and flat so no event ever reads a rise.
        ("back_yard_shrubs", 55.0, 0.0, 12 * 3600),
    ];
    for (slug, v0, slope, cadence_s) in curves {
        let events = watered.get(slug).unwrap_or(&empty);
        let rows: Vec<Reading> = (0..)
            .map(|i| win_start + i * cadence_s)
            .take_while(|e| *e <= now)
            .map(|epoch| Reading {
                epoch,
                source_id: "ecowitt".to_string(),
                key: zone_soil_key(slug),
                value: sawtooth_pct(epoch, win_start, v0, slope, events),
            })
            .collect();
        if let Err(e) = store.insert_many(rows).await {
            tracing::debug!("demo_data: probe curve seed failed for {slug}: {e}");
        }
    }
}

/// Deterministic probe curve: linear drying at `slope_pct_day` with a
/// step rise at each irrigation event's end that refills exactly the
/// water lost since the previous event (so the curve is bounded and
/// periodic: no drift over the window, whatever the run cadence). The
/// stretches between runs are exactly linear, which is what the tuning
/// report's least-squares slope reads back; the per-event rise divided
/// by valve-open time is what the rate backout reads back.
fn sawtooth_pct(
    epoch: i64,
    win_start: i64,
    v0: f64,
    slope_pct_day: f64,
    events: &[(i64, i64)],
) -> f64 {
    // Last event that has ENDED at `epoch` (events are chronological).
    let last_end = events
        .iter()
        .take_while(|(_, end)| *end <= epoch)
        .last()
        .map(|(_, end)| *end)
        .unwrap_or(win_start);
    let dried = slope_pct_day * (epoch - last_end) as f64 / 86_400.0;
    (v0 - dried).clamp(1.0, 99.0)
}

#[cfg(test)]
mod seed_config_tests {
    use super::*;

    /// The seeded probe curve must read back through the tuning math the
    /// way the seed intends: exactly linear drying between events (the
    /// least-squares slope recovers the seeded rate) and a refill at each
    /// event end that returns the curve to its start value (bounded, no
    /// drift, and the rise equals the water dried since the last refill).
    #[test]
    fn sawtooth_probe_curve_is_linear_between_events_and_refills() {
        let day = 86_400i64;
        let events = vec![(2 * day, 2 * day + 3600), (5 * day, 5 * day + 3600)];
        // Hourly samples inside the dry stretch AFTER the first event.
        let readings: Vec<(i64, f64)> = (0..48)
            .map(|h| {
                let e = 2 * day + 3600 + 12_600 + h * 3600;
                (e, sawtooth_pct(e, 0, 78.0, 20.0, &events))
            })
            .collect();
        let slope = crate::engine::tuning::slope_per_day(&readings).unwrap();
        assert!((slope + 20.0).abs() < 1e-6, "got {slope}");
        // Refill: right after the second event ends the curve is back at
        // its start value, however long the preceding gap was.
        let post = sawtooth_pct(5 * day + 3600 + 3600, 0, 78.0, 20.0, &events);
        assert!((post - (78.0 - 20.0 / 24.0)).abs() < 0.01, "got {post}");
        // Pre-event value carries the full inter-event drying.
        let pre = sawtooth_pct(5 * day - 3600, 0, 78.0, 20.0, &events);
        let dried_days = (5 * day - 3600 - (2 * day + 3600)) as f64 / 86_400.0;
        assert!((pre - (78.0 - 20.0 * dried_days)).abs() < 0.01, "got {pre}");
    }

    #[test]
    fn seed_config_shows_irrigation_and_validates() {
        let cfg = seed_config();

        // has_irrigation gate (/api/v1/info) = at least one controller OR zone.
        // Assert both so the demo nav can never regress to weather-only.
        assert!(!cfg.controllers.is_empty(), "needs a controller");
        assert_eq!(cfg.zones.len(), 4, "four demo zones");

        // Zones key on the SAME slugs synth_irrigation() emits, so the
        // config-driven views line up with the live snapshot. Each binds its
        // soil channel on the seeded gateway (the exact channel ids the
        // sensor-history heartbeat writes), which is also what marks the
        // gateway an owner in /api/health's soil-owner augmentation.
        for slug in ["back_yard", "front_yard", "side_yard", "back_yard_shrubs"] {
            let zone = cfg.zones.get(slug).expect("missing demo zone");
            assert_eq!(
                zone.soil_sensor_id.as_deref(),
                Some(format!("source:ecowitt:soilmoisture_{slug}").as_str()),
                "zone {slug} binds its demo soil channel"
            );
        }

        // A spread of sources for the Devices page + a per-field backup chain.
        assert_eq!(cfg.sources.len(), 4, "four demo sources");
        assert!(cfg.field_source_chains.contains_key("rain_today_in"));

        // Must pass the exact validation save() runs before touching disk, so a
        // future schema change that would break the seed fails here, not on the
        // live demo (where it would silently fall back to weather-only).
        crate::config::loader::validate(&cfg).expect("demo seed config must validate");

        // Round-trips through TOML (what save() serializes to /data).
        let toml_text = toml::to_string_pretty(&cfg).expect("seed serializes to TOML");
        let reparsed: crate::config::schema::Config =
            toml::from_str(&toml_text).expect("seed re-parses");
        assert_eq!(reparsed.zones.len(), 4);
        assert_eq!(reparsed.controllers.len(), 1);
    }

    /// Finding: the public demo reported permanently `degraded` because the
    /// seeded sources were never fed. The feeder's provenance stamps are half
    /// of the fix (the sensor-history heartbeat is the other): pin that one
    /// stamping pass marks all three merge-visible sources as owners, which is
    /// exactly what /api/health's `owns_field` check (via
    /// `current_owner_labels`) reads to call them `active`.
    #[test]
    fn provenance_stamps_make_demo_sources_owners() {
        let store = TempestStore::new();
        let snap = synth_tempest(30_000.0);
        stamp_source_provenance(&store, &snap, 1_700_000_000);
        let owners = store.current_owner_labels();
        for label in [
            crate::tempest::state::TEMPEST_LABEL,
            "ecowitt",
            "open_meteo",
        ] {
            assert!(owners.contains(label), "{label} must own a field");
        }
        // The per-field map the feeder copies onto the irrigation snapshot
        // (the refresher's job on a real deployment) carries the same story.
        let map = store.field_source_map();
        assert_eq!(map.get("wind_mph").map(String::as_str), Some("Tempest"));
        assert_eq!(
            map.get("pressure_in_hg").map(String::as_str),
            Some("ecowitt")
        );
    }

    /// Finding: the demo snapshot served out-of-contract EMPTY enum values
    /// (global_override "" instead of auto|skip|run, override_tomorrow ""
    /// instead of none|skip|run, per-zone override_mode "") and an all-zero
    /// forecast block. Pin the contract so a future field addition cannot
    /// regress the public demo to broken-looking manifest sensors.
    #[test]
    fn demo_snapshot_honors_the_api_contract() {
        for t_sim in [0.0, 20_000.0, 40_000.0, 60_000.0, 80_000.0] {
            let s = synth_irrigation(t_sim);
            assert_eq!(s.global_override, "auto");
            assert_eq!(s.override_tomorrow, "none");
            assert!(s.override_helpers_present);
            assert_eq!(s.timezone, "America/New_York");
            for z in &s.zones {
                assert_eq!(z.override_mode, "auto", "zone {} override", z.slug);
            }
            // The forecast block is populated, not the all-zero default.
            let f = &s.forecast;
            assert!(
                f.eto_today_mm.expect("demo always carries an ET0") > 0.0,
                "ET0 today must be non-zero"
            );
            assert_eq!(f.forecast_source_label, "Open-Meteo (demo)");
            assert!(f.wind_gust_today_mph > 0.0);
            assert_eq!(f.rain_tomorrow_prob_pct, Some(85));
            assert!(f.days_since_significant_rain > 0);
            assert!(f.heat_multiplier >= 1.0);
        }
    }

    /// The demo's showcase cap state, pinned end to end: back_yard's
    /// balance leaves the full 44.45 mm target (a measured-dry gauge
    /// week credits nothing) across 2 sessions, sized by the engine
    /// formula to 5635 s (94 min) per session, the raised morning still
    /// fits the pre-sunrise dispatch window at the demo's location, and
    /// the cap check turns that state into the 95 minute
    /// max_run_minutes recommendation.
    #[test]
    fn demo_back_yard_shows_the_95_minute_cap_recommendation() {
        let now = chrono::Utc::now().timestamp();
        let budgets = synth_water_budgets(now, crate::engine::calendar::Calendar::utc());
        let by = budgets
            .iter()
            .find(|w| w.zone_slug == "back_yard")
            .expect("back_yard budget row");
        assert_eq!(
            by.seconds_per_session, 5635,
            "gross balance sizing at the demo numbers"
        );
        assert!(by.session_capped, "5635 s outgrows the 3600 s default cap");
        // The math panel must agree with the budget row it sits beside.
        let zone_math = synth_irrigation(0.0)
            .zones
            .into_iter()
            .find(|z| z.slug == "back_yard")
            .and_then(|z| z.math)
            .expect("back_yard carries math");
        assert!(
            zone_math.cap_binding,
            "the panel's cap row must say what the budget row says"
        );
        assert_eq!(
            zone_math.scheduled_seconds, zone_math.max_duration_seconds,
            "a cap-binding row must print the cap it names"
        );
        assert_eq!(
            zone_math.scheduled_seconds, by.today_seconds,
            "the panel's minutes are the balance row's minutes"
        );
        assert_eq!(
            by.observed_rain_source, "gauge",
            "measured coverage outranks the regional archive"
        );
        assert_eq!(by.observed_rain_mm, 0.0);
        assert_eq!(by.remaining_sessions, 2);

        // The reseeded siblings tell the other reason branches.
        let front = budgets
            .iter()
            .find(|w| w.zone_slug == "front_yard")
            .unwrap();
        assert_eq!(front.today_seconds, 0);
        assert!(
            front.today_reason.contains("spaced"),
            "{}",
            front.today_reason
        );
        // side_yard's weekly row still tells the spaced story HERE, before
        // the soil pass; `apply_demo_soil_plans` swaps it (pinned in
        // `demo_side_yard_is_soil_governed_and_every_zone_carries_a_bucket`).
        let side = budgets.iter().find(|w| w.zone_slug == "side_yard").unwrap();
        assert_eq!(side.today_seconds, 0);
        assert!(
            side.today_reason.contains("spaced"),
            "{}",
            side.today_reason
        );
        let shrubs = budgets
            .iter()
            .find(|w| w.zone_slug == "back_yard_shrubs")
            .unwrap();
        assert_eq!(shrubs.today_seconds, 0);
        assert!(
            shrubs.today_reason.contains("covered"),
            "{}",
            shrubs.today_reason
        );

        // The raised morning fits the dispatch window: hypothetical zone
        // list with back_yard at the needed session, laid out under the
        // seeded demo policy, against the dispatcher's own window span.
        let cfg = seed_config();
        let policy = crate::refresher::WateringPolicy::from_config(&cfg);
        let snap = synth_irrigation(0.0);
        let mut zones = snap.zones.clone();
        for z in &mut zones {
            if z.slug == "back_yard" {
                z.planned_run_seconds = by.seconds_per_session;
            }
        }
        let seq = crate::scheduler::smart_morning::sequence_wall_seconds(
            &policy.zone_agronomy,
            &zones,
            policy.soak_minutes,
            policy.interleave_cycles,
        );
        // A pinned calendar and a pinned date: the claim is that the
        // demo's raised morning fits its own window, which must not
        // depend on the clock of whoever runs the suite.
        let cal = crate::engine::calendar::Calendar::utc();
        let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 26).expect("a real date");
        let available = crate::engine::sunrise::smart_morning_available_s(
            today,
            cfg.deployment.location.lat,
            cfg.deployment.location.lon,
            seq,
            cal,
        )
        .expect("sunrise exists at the demo location");
        assert!(
            (seq as i64) <= available,
            "raised demo morning must fit: sequence {seq} s vs available {available} s"
        );

        // The check itself: cap-primary, 95 minutes.
        let inp = crate::engine::tuning::CapClampInputs {
            session_capped: true,
            deficit_cap_binding: false,
            desired_seconds: Some(by.seconds_per_session),
            deficit_refill_seconds: None,
            max_duration_s: Some(3600),
            configured_max_run_minutes: None,
            raised_fits_window: Some(true),
            restriction_clamped: false,
            // Three watering days in the 14-day report window (the
            // trailing week sits out; earlier weeks watered).
            run_days: 3,
            sessions_per_week: by.sessions_per_week,
            configured_sessions: None,
            configured_weekly_budget_in: Some(by.weekly_budget_in),
            weekly_budget_in: by.weekly_budget_in,
            throughput_mm_hr: 14.2,
            ceiling_binding: false,
            soil_weekly_demand_in: None,
            soil_depletion_mm: None,
            soil_taw_mm: None,
        };
        let out = crate::engine::tuning::check_cap_clamped("back_yard", &inp);
        let crate::engine::tuning::CheckOutcome::Recommend(rec) = out else {
            panic!("expected the cap recommendation, got {out:?}");
        };
        assert_eq!(rec.field, "max_run_minutes");
        assert_eq!(rec.suggested_value, serde_json::json!(95));
    }

    /// The soil showcase, pinned end to end: side_yard is soil-governed
    /// (the seeded config pins it), its today figures come from the live
    /// planner through the shared `today_row` formatter, and every zone
    /// carries a shadow bucket that the zones' deficit tiles read under
    /// the documented sign. Deterministic across seasons: the evidence
    /// days all ride the fallback ETc rung.
    #[test]
    fn demo_side_yard_is_soil_governed_and_every_zone_carries_a_bucket() {
        let s = synth_irrigation(0.0);
        // The chip precondition: the engine default is weekly, so the
        // pinned side_yard (model "soil") is the ONE zone whose model
        // differs and the mixed-install SOIL chip shows in screenshots.
        assert_eq!(s.engine_scheduling_model, "weekly");
        for b in &s.water_budgets {
            let expect = if b.zone_slug == "side_yard" {
                "soil"
            } else {
                "weekly"
            };
            assert_eq!(b.scheduling_model, expect, "model tag on {}", b.zone_slug);
            assert!(
                b.soil_depletion_mm.is_some(),
                "shadow bucket on {}",
                b.zone_slug
            );
        }
        let side = s
            .water_budgets
            .iter()
            .find(|b| b.zone_slug == "side_yard")
            .unwrap();
        // Watered yesterday, over the trigger today, the full refill over
        // the 60-minute cap, and the explicit 1.3 in weekly target
        // clamping today's delivery to the remaining headroom.
        assert!(side.soil_due, "side_yard is due");
        assert!(side.today_seconds > 0, "side_yard waters today");
        assert!(
            side.soil_ceiling_binding,
            "the explicit weekly target clamps delivery"
        );
        assert!(
            side.today_reason.starts_with("held to the weekly ceiling"),
            "{}",
            side.today_reason
        );
        // The zone tiles read the budget rows' producer: planned seconds
        // and the negative-signed bucket.
        let zone = s.zones.iter().find(|z| z.slug == "side_yard").unwrap();
        assert_eq!(zone.planned_run_seconds, side.today_seconds);
        let bucket = zone.bucket_mm.expect("side_yard carries a bucket");
        assert!(bucket < 0.0, "negative = needs water, got {bucket}");
        assert_eq!(bucket, -side.soil_depletion_mm.unwrap());
        assert_eq!(zone.math.as_ref().unwrap().bucket_mm, Some(bucket));
        // A weekly sibling still holds a shadow figure on its tiles.
        let shrubs = s
            .zones
            .iter()
            .find(|z| z.slug == "back_yard_shrubs")
            .unwrap();
        assert!(shrubs.bucket_mm.is_some(), "shadow bucket reaches the tile");
        let shrubs_row = s
            .water_budgets
            .iter()
            .find(|b| b.zone_slug == "back_yard_shrubs")
            .unwrap();
        assert!(
            !shrubs_row.soil_due,
            "the rain-refilled bed holds with margin"
        );
    }

    /// Finding: the baked demo reasons carried operands that contradicted the
    /// SkipCheck fields (hero re-rendered "0.18\" x 65%" while the baked string
    /// said "0.40\" x 85%"), plus wording drift from the engine's formats.
    /// Walking a full synthetic day and demanding byte-identity between the
    /// baked reason and the unit-aware re-render pins both: operands AND
    /// wording (x vs the multiplication sign, "Heat advisory:" vs a dash).
    #[test]
    fn demo_reasons_match_the_unit_renderer() {
        use crate::components::units_fmt::UnitPrefs;
        const IMPERIAL: UnitPrefs = UnitPrefs {
            temp_c: false,
            rain_mm: false,
            wind_metric: false,
            pressure_metric: false,
            distance_metric: false,
            area_metric: false,
        };
        for i in 0..200 {
            let t_sim = i as f64 * (86400.0 / 200.0);
            let s = synth_irrigation(t_sim);
            let sk = &s.skip_check;
            assert_eq!(
                crate::reason_render::render_skip_reason(sk, IMPERIAL),
                sk.reason,
                "demo reason must re-render byte-identical (t_sim={t_sim}, code={})",
                sk.reason_code
            );
        }
    }
}
