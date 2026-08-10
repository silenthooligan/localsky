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
    tempest.apply_source_fields(
        &[(F::Pop, snap.pop_pct), (F::Et0Today, snap.et0_today)],
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
                past_days: 1,
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
        soil_sensor_id: None,
        target_min_pct_soil: 30.0,
        saturation_pct_soil: 70.0,
        photo_url: None,
        weekly_budget_in: None,
        sessions_per_week: None,
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
            soil_sensor_id: soil("back_yard"),
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
            soil_sensor_id: soil("side_yard"),
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
        pop_pct: (20.0 + 30.0 * (day_phase * 0.7).sin()).clamp(0.0, 100.0),
        leaf_wetness_pct: (35.0 + 35.0 * (day_phase * 0.6 + 1.0).sin()).clamp(0.0, 100.0),
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
    }
}

fn synth_irrigation(t_sim: f64) -> IrrigationSnapshot {
    let now = chrono::Utc::now().timestamp();
    let day_phase = (t_sim / 86400.0) * std::f64::consts::TAU;

    let zones = vec![
        synth_zone(
            "back_yard",
            "Back Yard",
            -12.5 + 4.0 * day_phase.sin(),
            1200,
            now - 18 * 3600,
        ),
        synth_zone(
            "front_yard",
            "Front Yard",
            -8.2 + 3.0 * (day_phase * 1.3).sin(),
            900,
            now - 24 * 3600,
        ),
        synth_zone(
            "side_yard",
            "Side Yard",
            -5.1 + 2.0 * (day_phase * 0.7).sin(),
            600,
            now - 36 * 3600,
        ),
        synth_zone(
            "back_yard_shrubs",
            "Back Yard Shrubs",
            -2.8 + 1.5 * (day_phase * 0.5).sin(),
            1800,
            now - 48 * 3600,
        ),
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
    snap.water_level_pct = 100.0;
    snap.next_run_epoch = now + 6 * 3600;
    snap.next_run_total_minutes = 75.0;
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
        rain_tomorrow_prob_pct: 85,
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
        eto_today_mm: 3.5 + (day_phase * 0.5).sin() * 1.0,
        eto_tomorrow_mm: 4.4,
        eto_3day_avg_mm: 4.2,
        temp_max_today_f: 88.0,
        temp_min_today_f: 71.0,
        wind_max_today_mph: 8.0,
        wind_gust_today_mph: 12.0,
        humidity_mean_today_pct: 65.0,
        rain_3day_weighted_in: 0.42,
        rain_7day_weighted_in: 0.95,
        rain_next_4h_in,
        rain_tomorrow_prob_pct: 85,
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
    snap.water_budgets = synth_water_budgets(now);
    snap
}

fn synth_zone(slug: &str, name: &str, bucket_mm: f64, planned_s: u32, last_run: i64) -> ZoneState {
    let mut z = ZoneState::default();
    z.name = name.into();
    z.slug = slug.into();
    // Contract: "auto" | "skip" | "run", never empty. (`ZoneState::default()`
    // yields an empty string; the serde default only applies on deserialize.)
    z.override_mode = "auto".into();
    z.bucket_mm = bucket_mm;
    z.planned_run_seconds = planned_s;
    z.last_run_epoch = last_run;
    z.math = Some(ZoneMath {
        bucket_mm,
        kc: if slug.contains("shrub") { 0.50 } else { 1.00 },
        throughput_mm_hr: 14.2,
        heat_mult: 1.15,
        capture_eff: 0.70,
        raw_seconds: planned_s + 200,
        max_duration_seconds: 3600,
        scheduled_seconds: planned_s,
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
            d.precip_probability_max = if v.starts_with("skip") { 85 } else { 15 };
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

fn synth_water_budgets(now: i64) -> Vec<WaterBudget> {
    let zones = [
        ("back_yard", "Back Yard", true, 1.00, 2u32),
        ("front_yard", "Front Yard", true, 1.00, 2),
        ("side_yard", "Side Yard", false, 1.00, 2),
        ("back_yard_shrubs", "Back Yard Shrubs", false, 0.50, 1),
    ];
    zones
        .iter()
        .map(|(slug, name, mode, budget_in, sessions)| {
            let mut w = WaterBudget::default();
            w.zone_slug = slug.to_string();
            w.zone_name = name.to_string();
            w.mode_active = *mode;
            w.weekly_budget_in = *budget_in;
            w.sessions_per_week = *sessions;
            w.expected_rain_mm = 8.5;
            w.needed_mm = 25.4 * budget_in - 8.5;
            w.mm_per_session = (25.4 * budget_in - 8.5) / (*sessions as f64);
            w.seconds_per_session = 1200;
            w.session_capped = false;
            w.last_run_epoch = now - 18 * 3600;
            w.today_seconds = if *mode { 1200 } else { 0 };
            w.today_reason = if *mode {
                "scheduled session 1 of 2 this week".into()
            } else {
                "budget mode off".into()
            };
            w
        })
        .collect()
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
            e.precip_probability_max = if d == 1 {
                85
            } else if d == 4 {
                70
            } else {
                15
            };
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
            e.precip_probability = if h > 2 && h < 6 { 75 } else { 10 };
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
/// story instead of rendering empty states. Idempotent: only fires
/// when the runs table is empty.
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
                    let jitter = ((back * 37 + t) % 90) - 45;
                    let d = (dur + jitter).max(300);
                    let _ = record_run(
                        conn.clone(),
                        RunRecord {
                            zone: slug.into(),
                            start_epoch: t,
                            duration_s: d,
                            skip_reason: None,
                        },
                    )
                    .await;
                    t += d + 300;
                }
            }
        }
    }
}

#[cfg(test)]
mod seed_config_tests {
    use super::*;

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
            assert!(f.eto_today_mm > 0.0, "ET0 today must be non-zero");
            assert_eq!(f.forecast_source_label, "Open-Meteo (demo)");
            assert!(f.wind_gust_today_mph > 0.0);
            assert_eq!(f.rain_tomorrow_prob_pct, 85);
            assert!(f.days_since_significant_rain > 0);
            assert!(f.heat_multiplier >= 1.0);
        }
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
