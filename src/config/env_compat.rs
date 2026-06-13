// env_compat synthesizes a v2 Config from the legacy v0.1 environment
// variables. This is the homelab continuity path: when /data/localsky.toml
// is absent but the operator's docker-compose still sets HA_URL, HA_TOKEN,
// VAPID_*, WEATHER_APP_LAT/LON, LLM_BASE_URL, etc., we synthesize a
// best-effort Config that preserves v0.1 behavior so existing
// deployments run unchanged after the v2 image swap.
//
// First-run wizards never go through here; they write a fresh TOML.

use std::env;

use tracing::info;

use crate::config::schema::*;

/// Build a Config from process environment variables. The returned config
/// reflects "what v0.1 would have done" given the same env. The caller is
/// expected to persist this synthesized Config to /data/localsky.toml on
/// first boot so subsequent boots use the file (and the env vars become
/// no-ops). Never panics; missing optional bits silently no-op.
pub fn synthesize() -> Config {
    let mut cfg = Config::default();
    let mut log_lines: Vec<String> = Vec::new();

    // ----- Deployment / location -----
    if let (Ok(lat), Ok(lon)) = (env::var("WEATHER_APP_LAT"), env::var("WEATHER_APP_LON")) {
        if let (Ok(lat), Ok(lon)) = (lat.parse::<f64>(), lon.parse::<f64>()) {
            cfg.deployment.location.lat = lat;
            cfg.deployment.location.lon = lon;
            log_lines.push(format!("location from WEATHER_APP_LAT/LON ({lat}, {lon})"));
        }
    }
    if let Ok(tz) = env::var("TZ") {
        cfg.deployment.timezone = Some(tz.clone());
        log_lines.push(format!("timezone from TZ env ({tz})"));
    }

    // ----- Always-on local source: Tempest UDP -----
    cfg.sources.push(SourceEntry {
        id: "tempest_lan".into(),
        priority: 100,
        max_age_s: None,
        enabled: true,
        source: SourceKind::TempestUdp(TempestUdpConfig {
            bind_addr: env::var("TEMPEST_BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:50222".into()),
            hub_serial: env::var("TEMPEST_HUB_SERIAL").ok(),
        }),
    });
    log_lines.push("synthesized tempest_lan source (UDP 50222)".into());

    // ----- Always-on forecast source: Open-Meteo -----
    cfg.sources.push(SourceEntry {
        id: "open_meteo".into(),
        priority: 50,
        max_age_s: None,
        enabled: true,
        source: SourceKind::OpenMeteo(OpenMeteoConfig {
            forecast_days: 7,
            forecast_hours: 48,
            past_days: 1,
            include_radar: false,
            model: crate::forecast::model_catalog::DEFAULT_MODEL.to_string(),
        }),
    });
    log_lines.push("synthesized open_meteo source (7-day forecast)".into());

    // ----- Optional HA passthrough source + service-call controller -----
    let ha_url = env::var("HA_URL").ok();
    let ha_token = env::var("HA_LONG_LIVED_TOKEN")
        .or_else(|_| env::var("HA_TOKEN"))
        .ok();
    if let (Some(url), Some(token)) = (ha_url.clone(), ha_token.clone()) {
        log_lines.push(format!("HA env detected; synthesizing ha_passthrough source + ha_service_call controller ({url})"));
        let _ = token;
        // Reading-side source so legacy sensors keep contributing to merge.
        cfg.sources.push(SourceEntry {
            id: "ha_passthrough".into(),
            priority: 30,
            max_age_s: None,
            enabled: true,
            source: SourceKind::HaPassthrough(HaPassthroughConfig {
                base_url: url.clone(),
                bearer_token: token.clone(),
                field_map: Default::default(),
            }),
        });
        // Default controller routes to HA service calls (matches v0.1).
        cfg.controllers.push(ControllerEntry {
            id: "ha_main".into(),
            default: true,
            enabled: true,
            controller: ControllerKind::HaServiceCall(HaServiceCallConfig {
                base_url: url,
                bearer_token: token,
                start_service: "script.os_zone_toggle".into(),
                stop_service: "opensprinkler.stop".into(),
                zone_entity_map: Default::default(),
            }),
        });

        // Seed the v0.1 four-zone layout so the dashboard renders something
        // immediately on first v2 boot. The wizard (when run) overwrites
        // these with controller-scanned zones.
        seed_legacy_zones(&mut cfg);
    }

    // ----- LLM advisor -----
    let llm_disabled = env::var("LLM_ADVISOR_DISABLED").ok().as_deref() == Some("1");
    if !llm_disabled {
        let llm = build_llm_config();
        let provider_name = match &llm.provider {
            LlmProviderKind::Auto(_) => "auto-detect",
            LlmProviderKind::Ollama(_) => "ollama",
            LlmProviderKind::Llamacpp(_) => "llamacpp",
            LlmProviderKind::OpenaiCompat(_) => "openai_compat",
        };
        log_lines.push(format!("synthesized llm config (provider={provider_name})"));
        cfg.llm = Some(llm);
    } else {
        cfg.features.enable_advisor = false;
        log_lines.push("LLM_ADVISOR_DISABLED=1; advisor disabled".into());
    }

    // ----- Web Push notifications -----
    if let (Ok(pub_key), Ok(priv_path), Ok(subject)) = (
        env::var("VAPID_PUBLIC_KEY"),
        env::var("VAPID_PRIVATE_KEY_PATH"),
        env::var("VAPID_SUBJECT"),
    ) {
        if !pub_key.is_empty() {
            cfg.notifications.web_push = Some(WebPushConfig {
                vapid_public: pub_key,
                vapid_private_path: priv_path,
                vapid_subject: subject,
            });
            log_lines.push("synthesized web_push config from VAPID_* env".into());
        }
    }

    // ----- MQTT (optional) -----
    if let Ok(host) = env::var("MQTT_HOST") {
        if !host.is_empty() {
            cfg.notifications.mqtt = Some(MqttConfig {
                host: host.clone(),
                port: env::var("MQTT_PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1883),
                username: env::var("MQTT_USER").ok(),
                password: env::var("MQTT_PASSWORD").ok(),
                discovery_prefix: env::var("MQTT_DISCOVERY_PREFIX")
                    .unwrap_or_else(|_| "homeassistant".into()),
                publish_enabled: true,
                subscribe_enabled: false,
            });
            log_lines.push(format!("synthesized mqtt config (broker={host})"));
        }
    }

    // ----- Demo mode -----
    if env::var("LOCALSKY_DEMO").ok().as_deref() == Some("1") {
        log_lines.push(
            "LOCALSKY_DEMO=1; switching to demo mode (DryRun controller + DemoReplay source)"
                .into(),
        );
        cfg.features.demo_mode = true;
        cfg.sources.push(SourceEntry {
            id: "demo_replay".into(),
            priority: 100,
            max_age_s: None,
            enabled: true,
            source: SourceKind::DemoReplay(DemoReplayConfig {
                rate: 10.0,
                replay_path: None,
            }),
        });
        // Demo always uses a DryRun controller; existing HA controller is
        // left enabled but the engine prefers `default = true` which we
        // flip here.
        for c in cfg.controllers.iter_mut() {
            c.default = false;
        }
        cfg.controllers.push(ControllerEntry {
            id: "dry_run".into(),
            default: true,
            enabled: true,
            controller: ControllerKind::DryRun(DryRunConfig {
                simulate_runs: true,
            }),
        });
    }

    if log_lines.is_empty() {
        info!("env_compat: no recognized env vars; synthesized minimal default config");
    } else {
        for line in &log_lines {
            info!(synthesized = %line, "env_compat");
        }
    }

    cfg
}

fn build_llm_config() -> LlmConfig {
    let provider = env::var("LLM_PROVIDER").unwrap_or_else(|_| "auto".into());
    let timeout_s = env::var("LLM_TIMEOUT_S")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let kind = match provider.as_str() {
        "auto" => LlmProviderKind::Auto(AutoProviderConfig::default()),
        "ollama" => LlmProviderKind::Ollama(OllamaProviderConfig {
            base_url: env::var("LLM_BASE_URL").unwrap_or_else(|_| "http://localhost:11434".into()),
            model: env::var("LLM_MODEL").unwrap_or_else(|_| "llama3.2:3b-instruct".into()),
        }),
        "llamacpp" => LlmProviderKind::Llamacpp(LlamacppProviderConfig {
            base_url: env::var("LLM_BASE_URL").unwrap_or_else(|_| "http://localhost:8080".into()),
            model: env::var("LLM_MODEL").ok(),
        }),
        "openai_compat" | "openai" => LlmProviderKind::OpenaiCompat(OpenaiCompatConfig {
            base_url: env::var("LLM_BASE_URL").unwrap_or_default(),
            model: env::var("LLM_MODEL").unwrap_or_default(),
            api_key: env::var("LLM_API_KEY").ok(),
        }),
        // Unrecognized providers fall through to auto-detect. The
        // pre-v2 deployments that set a deployment-specific gateway
        // env var (any LLM_BASE_URL points at an OpenAI-compatible
        // endpoint) get mapped to OpenaiCompat with that URL.
        _ => {
            if let Ok(url) = env::var("LLM_BASE_URL") {
                LlmProviderKind::OpenaiCompat(OpenaiCompatConfig {
                    base_url: url,
                    model: env::var("LLM_MODEL")
                        .or_else(|_| env::var("LLM_ADVISOR_MODEL"))
                        .unwrap_or_default(),
                    api_key: env::var("LLM_API_KEY").ok(),
                })
            } else {
                LlmProviderKind::Auto(AutoProviderConfig::default())
            }
        }
    };

    LlmConfig {
        provider: kind,
        timeout_s,
        explanation_ttl_s: 300,
        anomaly_ttl_s: 3600,
    }
}

/// Seed the four hardcoded v0.1 zones (back_yard, front_yard, side_yard,
/// back_yard_shrubs) with conservative defaults. Operator can edit later.
fn seed_legacy_zones(cfg: &mut Config) {
    let turf = |display: &str, station: u32| ZoneConfig {
        display_name: display.into(),
        area_sqft: 1000.0,
        species: GrassSpecies::StAugustine,
        soil_texture: SoilTexture::SandyLoam,
        slope_pct: 0.0,
        sun_exposure: SunExposure::Full,
        sprinkler_type: SprinklerType::Rotor,
        precip_rate_mm_hr: None,
        precip_rate_source: PrecipRateSource::Catalog,
        root_depth_mm: None,
        mad_pct_override: None,
        controller_id: "ha_main".into(),
        controller_station: station.to_string(),
        soil_sensor_id: None,
        target_min_pct_soil: 30.0,
        saturation_pct_soil: 70.0,
        photo_url: None,
        // None = agronomic default by slug (turf 1.0"/2, shrub 0.5"/1),
        // matching the historical hardcoded compute_water_budgets defaults.
        weekly_budget_in: None,
        sessions_per_week: None,
    };
    cfg.zones.insert("back_yard".into(), turf("Back Yard", 1));
    cfg.zones.insert("front_yard".into(), turf("Front Yard", 2));
    cfg.zones.insert("side_yard".into(), turf("Side Yard", 3));
    cfg.zones.insert(
        "back_yard_shrubs".into(),
        ZoneConfig {
            display_name: "Back Yard Shrubs".into(),
            species: GrassSpecies::OrnamentalShrubs,
            sprinkler_type: SprinklerType::Drip,
            ..turf("Back Yard Shrubs", 4)
        },
    );
}
