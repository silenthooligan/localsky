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

    // ----- Local source: Tempest UDP (only when the operator signaled one) -----
    // The legacy v0.1 image always opened a Tempest UDP listener. That is wrong
    // for a no-hardware install: a passive socket nothing transmits on drives a
    // global "tempest_lan offline / degraded" health banner ~60s after boot. So
    // we synthesize the listener ONLY when the operator explicitly opted into a
    // Tempest (a TEMPEST_BIND_ADDR or TEMPEST_HUB_SERIAL env). A v0.1 deployment
    // that actually had a Tempest set one of these (or accepts adding it from
    // the Sources UI); without either, the install is cloud-only (Open-Meteo).
    let tempest_bind = env::var("TEMPEST_BIND_ADDR").ok();
    let tempest_hub = env::var("TEMPEST_HUB_SERIAL").ok();
    if let Some(entry) = tempest_lan_entry(tempest_bind, tempest_hub) {
        cfg.sources.push(entry);
        log_lines.push("synthesized tempest_lan source from TEMPEST_* env (UDP 50222)".into());
    }

    // ----- Always-on forecast source: Open-Meteo -----
    // The keyless cloud backstop and last link in the cloud-only fallback
    // chain. Its merge priority + freshness window come from the region helper
    // (Open-Meteo always ranks 50; its ~1800s refresh cadence widens max_age to
    // ~2100 so a per-field pin survives a full refresh cycle, closing the
    // wind-pin freshness-cadence mismatch). location.lat/lon were set above from
    // WEATHER_APP_LAT/LON (or the Config::default 0,0 when absent, which resolves
    // to the Global ranking) so the priority is correct for whatever cloud a
    // user later adds.
    let om_kind = SourceKind::OpenMeteo(OpenMeteoConfig {
        forecast_days: 7,
        forecast_hours: 48,
        past_days: 1,
        // Radar on by default: the synthesized Open-Meteo source powers
        // the Live Radar precipitation overlay out of the box, matching
        // the serde default in schema::default_open_meteo_include_radar.
        include_radar: true,
        model: crate::forecast::model_catalog::DEFAULT_MODEL.to_string(),
    });
    let (om_lat, om_lon) = (cfg.deployment.location.lat, cfg.deployment.location.lon);
    // Snapshot BEFORE the Open-Meteo push: a genuinely no-hardware install has no
    // sources yet here (a TEMPEST_* opt-in already pushed a tempest_lan above, so
    // it is not empty). This gates the region keyless authority below so a
    // hardware/HA deployment is never given an unsolicited extra cloud.
    let no_local_sources = cfg.sources.is_empty();
    cfg.sources.push(SourceEntry {
        id: "open_meteo".into(),
        priority: crate::config::region::default_priority_for(&om_kind, om_lat, om_lon),
        max_age_s: crate::config::region::default_max_age_for(&om_kind),
        enabled: crate::config::region::default_enabled_for(&om_kind, om_lat, om_lon),
        source: om_kind,
    });
    log_lines.push("synthesized open_meteo source (7-day forecast, radar on)".into());

    // ----- Default-on regional keyless authority -----
    // A no-hardware user should boot with the region's KEYLESS authority live,
    // zero clicks: NWS in the US, Met.no in Europe/the Nordics (nothing extra
    // elsewhere, where Open-Meteo is the sole keyless cloud). Both are keyless
    // (the helper auto-fills the required user_agent) and land at their region
    // rank (70, above the Open-Meteo backstop at 50) with the slow-cadence
    // freshness window, via the same region helpers Open-Meteo used above. We
    // gate strictly on the pre-Open-Meteo emptiness so a hardware/HA install is
    // never handed an unsolicited cloud; a real live LAN station still outranks
    // these (priority 100, live_current=true vs these false). NEVER a keyed
    // source (Pirate/OpenWeather/WeatherKit): those stay operator opt-in.
    if no_local_sources {
        for entry in crate::config::region::region_keyless_authority_entries(om_lat, om_lon) {
            log_lines.push(format!(
                "synthesized region keyless authority source '{}' (priority {}, enabled {})",
                entry.id, entry.priority, entry.enabled
            ));
            cfg.sources.push(entry);
        }
    }

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
                soil_zone_map: Default::default(),
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

/// Decide whether to synthesize the passive Tempest UDP listener from the
/// TEMPEST_* env signals, pure (no env reads) so it is testable in isolation.
///
/// The legacy v0.1 image always opened a Tempest listener, which is wrong for a
/// no-hardware install: a passive socket nothing transmits on drives a global
/// "tempest_lan offline" health banner ~60s after boot. So the listener is
/// synthesized ONLY when the operator explicitly opted into a Tempest (either a
/// bind address or a hub serial). `None` => cloud-only (Open-Meteo) install with
/// no phantom tempest_lan.
fn tempest_lan_entry(
    tempest_bind: Option<String>,
    tempest_hub: Option<String>,
) -> Option<SourceEntry> {
    if tempest_bind.is_none() && tempest_hub.is_none() {
        return None;
    }
    Some(SourceEntry {
        id: "tempest_lan".into(),
        priority: 100,
        max_age_s: None,
        enabled: true,
        source: SourceKind::TempestUdp(TempestUdpConfig {
            bind_addr: tempest_bind.unwrap_or_else(|| "0.0.0.0:50222".into()),
            hub_serial: tempest_hub,
        }),
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    // Protects the phantom-Tempest fix: a no-hardware (no TEMPEST_*) install must
    // be cloud-only (Open-Meteo) with NO passive tempest_lan listener (which would
    // otherwise drive a false "offline" health banner), and a TEMPEST_* signal must
    // still synthesize a Tempest source. The decision is tested through the pure
    // `tempest_lan_entry` helper so it never mutates the shared process env (which
    // would race other tests reading TEMPEST_* / the synthesize fallback).

    #[test]
    fn no_tempest_env_synthesizes_no_tempest_lan() {
        // No bind addr, no hub serial => no Tempest source (cloud-only install).
        assert!(
            tempest_lan_entry(None, None).is_none(),
            "a no-hardware install must NOT synthesize a phantom tempest_lan"
        );
    }

    #[test]
    fn tempest_bind_addr_synthesizes_a_tempest_source() {
        // TEMPEST_BIND_ADDR set => a tempest_lan UDP source IS synthesized.
        let entry =
            tempest_lan_entry(Some("0.0.0.0:50222".into()), None).expect("bind addr -> tempest");
        assert_eq!(entry.id, "tempest_lan");
        match entry.source {
            SourceKind::TempestUdp(c) => {
                assert_eq!(c.bind_addr, "0.0.0.0:50222");
                assert!(c.hub_serial.is_none());
            }
            other => panic!("expected a TempestUdp source, got {other:?}"),
        }
        // A hub serial alone (no bind) also opts in, defaulting the bind addr.
        let by_hub =
            tempest_lan_entry(None, Some("HB-00012345".into())).expect("hub serial -> tempest");
        match by_hub.source {
            SourceKind::TempestUdp(c) => {
                assert_eq!(c.bind_addr, "0.0.0.0:50222");
                assert_eq!(c.hub_serial.as_deref(), Some("HB-00012345"));
            }
            other => panic!("expected a TempestUdp source, got {other:?}"),
        }
    }

    #[test]
    fn synthesized_open_meteo_carries_region_aware_priority_and_widened_max_age() {
        // The synthesized Open-Meteo backstop must seed its merge priority +
        // freshness window from the region helper: Open-Meteo always ranks 50,
        // and its ~1800s refresh cadence widens max_age to ~2100 so a per-field
        // pin survives a full refresh cycle (the wind-pin freshness mismatch).
        let cfg = synthesize();
        let om = cfg
            .sources
            .iter()
            .find(|s| s.id == "open_meteo")
            .expect("synthesize must include open_meteo");
        assert_eq!(
            om.priority, 50,
            "Open-Meteo must seed the 50-priority keyless backstop rank"
        );
        assert_eq!(
            om.max_age_s,
            Some(crate::config::region::MAX_AGE_SLOW_CADENCE_S),
            "the 1800s-cadence Open-Meteo source must widen max_age (~2100) so a pin outlives the refresh"
        );
        assert!(om.enabled, "the synthesized Open-Meteo backstop is enabled");
    }

    #[test]
    fn synthesize_always_includes_open_meteo_and_no_unsignaled_tempest() {
        // The Open-Meteo forecast source is pushed unconditionally, so any
        // synthesized config (regardless of ambient env) carries it. And because
        // this process sets no TEMPEST_* in the test env, synthesize() must not
        // produce a tempest_lan here either.
        let cfg = synthesize();
        assert!(
            cfg.sources.iter().any(|s| s.id == "open_meteo"),
            "env_compat must always synthesize a cloud-first open_meteo source"
        );
        assert!(
            !cfg.sources.iter().any(|s| s.id == "tempest_lan"),
            "with no TEMPEST_* env, synthesize must not produce a phantom tempest_lan"
        );
    }

    #[test]
    fn synthesize_at_default_location_adds_no_regional_authority() {
        // With no WEATHER_APP_LAT/LON in the test env the location stays the
        // Config::default 0,0, which resolves to the Global region: no keyless
        // regional authority covers it, so synthesize() must add Open-Meteo only
        // and NEVER a keyed source. The US/Nordic synthesis branches are covered
        // race-free by region::region_keyless_authority_entries unit tests (this
        // test must not mutate the shared process env to set a location). NWS /
        // Met.no must not appear at 0,0.
        let cfg = synthesize();
        assert!(
            cfg.sources.iter().any(|s| s.id == "open_meteo"),
            "the always-on Open-Meteo backstop is present"
        );
        assert!(
            !cfg.sources.iter().any(|s| s.id == "nws"),
            "0,0 is Global: no NWS authority is synthesized"
        );
        assert!(
            !cfg.sources.iter().any(|s| s.id == "met_norway"),
            "0,0 is Global: no Met.no authority is synthesized"
        );
        // And never a keyed provider regardless of region.
        assert!(
            !cfg.sources
                .iter()
                .any(|s| s.id == "pirate_weather" || s.id == "openweather"),
            "no keyed source is ever auto-enabled"
        );
    }
}
