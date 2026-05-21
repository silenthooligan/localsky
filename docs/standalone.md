# Standalone Mode (No Home Assistant)

LocalSky is a complete, native irrigation + weather product. Home Assistant is one of several integration paths, not a dependency. This document is for users who:

- Don't run Home Assistant and don't want to
- Run HA but want LocalSky to own irrigation end-to-end
- Need to understand exactly what works without HA

## What "standalone" gets you (the short answer)

Everything. The full LocalSky feature set runs without HA:

- Live weather dashboard (Tempest UDP / Open-Meteo / Ecowitt / NWS / etc.)
- FAO-56 reference ET₀ with Hargreaves fallback
- Per-zone water balance + MAD-driven scheduling
- 17-rule skip ladder
- 7-day forward verdict strip
- Cycle-and-soak runtime splitting
- Direct controller dispatch (OpenSprinkler HTTP API, ESPHome native protocol, Rachio cloud)
- Sensor ingestion via MQTT subscribe + direct LAN adapters
- LLM advisor (Ollama, llama.cpp, or any OpenAI-compatible)
- Web Push notifications (browser, per device)
- PWA install on iOS + Android
- Settings UI + first-run wizard

What you don't get without HA:
- HA's broader home-automation ecosystem (lights, locks, scenes)
- HA's dashboard widgets and other integrations

That's a fair trade if you don't already run HA.

## Sensor ingestion without Home Assistant

This is the question that surfaces most often: "I have soil moisture sensors. How do they get into LocalSky without HA?" Three paths, none requiring HA.

### Path 1: MQTT broker (the universal path)

Most modern sensors publish to MQTT. LocalSky's `mqtt` source subscribes to topics directly. The architecture:

```
[Sensor: Tasmota / ESPHome / Zigbee2MQTT / etc.]
              |
              v (publishes to topic)
        [MQTT broker: Mosquitto]
              |
              v (LocalSky subscribes)
        [LocalSky source: kind = "mqtt"]
```

The broker can be Mosquitto (open-source, free, runs in a 5 MB Docker container), EMQX, HiveMQ, or anything that speaks MQTT 3.1.1 or 5.0. **HA's broker works too if you already have one; the point is the broker is the standard, not HA.**

#### Set up Mosquitto

```bash
mkdir -p /opt/mosquitto/{config,data,log}
cat > /opt/mosquitto/config/mosquitto.conf <<'EOF'
listener 1883
allow_anonymous true
persistence true
persistence_location /mosquitto/data/
log_dest file /mosquitto/log/mosquitto.log
EOF

docker run -d \
  --name mosquitto \
  --restart unless-stopped \
  -p 1883:1883 \
  -v /opt/mosquitto/config:/mosquitto/config \
  -v /opt/mosquitto/data:/mosquitto/data \
  -v /opt/mosquitto/log:/mosquitto/log \
  eclipse-mosquitto:latest
```

Lock this down with username/password before exposing to anything but localhost.

#### Configure LocalSky to subscribe

In `/data/localsky.toml` (or via `/settings/sources` once the editor lands):

```toml
[[sources]]
id = "mqtt_sensors"
priority = 80
enabled = true
kind = "mqtt"
[sources.config]
broker_host = "192.0.2.5"     # the mosquitto host
broker_port = 1883
username = "${MQTT_USER}"
password = "${MQTT_PASSWORD}"

[[sources.config.subscriptions]]
topic = "tasmota/soil/back_yard/SENSOR"
field = "soil_moisture_pct"   # planned WeatherField variant for per-zone soil
json_path = "ANALOG.A0"
zone_slug = "back_yard"
scale = 0.0976                # adjust for sensor calibration
offset = 0.0

[[sources.config.subscriptions]]
topic = "esphome/lawn/temperature/state"
field = "air_temp_f"
# no json_path means parse whole payload as a number
# (ESPHome native API publishes raw values to /state topics)
```

The adapter handles:

- MQTT 3.1.1 + 5.0
- Wildcards: `+` for one segment, `#` for trailing segments. Example:
  `tasmota/+/SENSOR` matches every Tasmota device's SENSOR topic
- Plain numeric payloads (Tasmota / ESPHome /state topics)
- JSON payloads with arbitrary nesting and arrays via `json_path`. Examples:
  - `"soil.moisture"` reads `obj["soil"]["moisture"]`
  - `"sensors.0.value"` reads `obj["sensors"][0]["value"]`
- Tasmota-style number-as-string payloads
- Linear transforms: `published_value * scale + offset` for unit conversion
  or sensor calibration

#### Hardware that works this way

| Device | How it gets to MQTT | LocalSky path |
|---|---|---|
| ESPHome-flashed ESP32 + sensor | Native MQTT publish (or via HA's MQTT integration) | Subscribe to `esphome/<device>/<sensor>/state` |
| Tasmota-flashed device | Native MQTT publish | Subscribe to `tasmota/<device>/SENSOR` |
| Zigbee sensors (Aqara, Sonoff) | Via Zigbee2MQTT (no HA needed) | Subscribe to `zigbee2mqtt/<friendly_name>` |
| Ecowitt gateway (WH51, WH52) | Via ecowitt2mqtt sidecar | Subscribe to `ecowitt/<device_id>` |
| Shelly devices | Native MQTT (firmware setting) | Subscribe to `shellies/<device>/<field>` |
| Arbitrary Arduino / Pi project | PubSubClient / paho-mqtt | Subscribe to whatever topic you publish |

Zigbee2MQTT is a particularly good fit. It's a single Docker container that talks to a USB Zigbee coordinator (Conbee II, Sonoff dongle, etc.) and publishes every Zigbee device's state to MQTT. No HA required.

### Path 2: Direct LAN adapters

For sensors that speak a documented LAN protocol, LocalSky can talk to them directly without MQTT in the middle.

| Sensor | Adapter | Status |
|---|---|---|
| Tempest hub (UDP broadcast 50222) | `tempest_udp` | Tested |
| Ecowitt GW1100 / GW2000 (HTTP POST) | `ecowitt_local` | Planned |
| Ambient Weather (socket.io) | `ambient_weather` | Planned |
| ESPHome native API (protobuf over TCP) | `esphome_native` (sensor mode) | Planned |

Direct adapters bypass MQTT entirely; the device talks to LocalSky's listener directly. Less infra, no broker. Use when the device supports a documented protocol that LocalSky has an adapter for.

### Path 3: HTTP webhook receiver (planned)

For sensors with arbitrary HTTP push capability (some commercial weather stations, custom scripts), a generic webhook receiver source is planned:

```toml
[[sources]]
id = "webhook_sensors"
kind = "http_webhook"
[sources.config]
path = "/ingest/<token>"
# Sensor POSTs JSON to http://localsky:8090/ingest/<token>
```

Status: planned. Until then, run a small Python or Node bridge that converts your HTTP source to MQTT publishes.

## Controller dispatch without Home Assistant

You have three direct paths:

### OpenSprinkler (the recommended hardware)

Direct HTTP API on the LAN. See [docs/controllers.md](controllers.md#opensprinkler-the-ideal). $130-180 hardware; the engine talks to it without anything else in the middle.

### ESPHome sprinkler (DIY)

For people who want full open hardware: an ESP32 + relay board + ESPHome's `sprinkler` component. ~$15-40 in parts. LocalSky's `esphome_native` controller (planned) speaks the protobuf protocol directly. Until that adapter lands, run the ESPHome device under ESPHome's standalone web interface (no HA needed) and use MQTT for state, with manual valve control from LocalSky still pending the native adapter.

### Rachio (planned)

Cloud API, no HA required. LocalSky speaks Rachio v1 directly.

### Existing setups: what if I already have HA driving Rachio / Hunter / B-hyve?

Use the `ha_service_call` controller. LocalSky dispatches through your existing HA setup. This is the "legacy continuity" mode; you keep HA in the loop because the integration to your hardware already lives there.

## Reaching LocalSky remotely without HA

HA is sometimes used as a remote-access shim. If you're not running HA, options:

- **Tailscale** -- recommended; works on any platform
- **Reverse proxy + TLS** -- Caddy / nginx / Traefik with Let's Encrypt
- **Cloudflare Tunnel** -- no port forwarding, no public IP needed

See [getting-started.md#remote-reachability](getting-started.md#remote-reachability).

## Notifications without HA

LocalSky has four notification channels, none requiring HA:

- **Web Push** -- per-device, via VAPID. Works in any modern browser
- **ntfy.sh** -- free public service or self-host
- **Slack** -- incoming webhook
- **Email (SMTP)** -- planned

Configure under `/settings/notifications`. None of these touch HA.

## Do I still need Smart Irrigation or Irrigation Unlimited?

No. LocalSky's engine includes everything those integrations provide:

- ET₀, per-zone soil bucket, Kc curves, and planned-run-seconds: [engine/et0.rs](../src/engine/et0.rs) + [engine/water_balance.rs](../src/engine/water_balance.rs) + [engine/species_catalog.rs](../src/engine/species_catalog.rs).
- Schedule sequencing, skip rules, and zone dispatch: [engine/skip_rules.rs](../src/engine/skip_rules.rs) + [engine/budget.rs](../src/engine/budget.rs) + the controller HAL.

If you already run Smart Irrigation + Irrigation Unlimited and want to keep that setup, LocalSky's `ha_service_call` controller can dispatch through them and use LocalSky only for the dashboard + skip-rule engine; see [Mode 3 in getting-started.md](getting-started.md#mode-3-ha-driven-legacy-continuity).

## When standalone is a good fit

LocalSky as a standalone service is a single Docker container, ~30 MB resident, and a single Rust binary with a typed TOML config. That makes it a natural fit when:

- You want a focused tool for irrigation + weather rather than a broader home automation platform.
- You prefer a compiled engine with a small footprint to a Python plugin system.
- You want the irrigation UI and engine on hardware that doesn't run Home Assistant.

If you already run Home Assistant, LocalSky still plays well alongside it (Mode 2 outbound MQTT discovery, or Mode 3 HA-driven). Both paths are documented and supported.

## Future: LocalSky as an HACS integration

There's a path the other direction: a Python-side HACS integration that polls LocalSky's REST API and creates HA entities natively, without going through MQTT. This lets HA users get LocalSky as a "first-class HA integration" experience:

- One-click install via HACS
- Configuration through HA's UI
- LocalSky entities appear under "LocalSky" in HA's device list
- LocalSky's verdicts + zone state become available to HA automations and Lovelace dashboards without YAML

Status: roadmap. Once the LocalSky API stabilizes at v1.0, the HACS integration is a separate ~300-line Python project that wraps the REST endpoints documented in [docs/api.md](api.md). If you're a Python developer interested in building this, see the [CONTRIBUTING.md](../CONTRIBUTING.md) on cross-project contribution.

## Summary table

| Capability | Standalone | HA + LocalSky (Mode 2: outbound) | HA + LocalSky (Mode 3: HA-driven) |
|---|---:|---:|---:|
| Weather dashboard | ✅ | ✅ | ✅ |
| Engine (ET, bucket, skip rules) | ✅ | ✅ | ✅ |
| Controller dispatch | ✅ direct | ✅ direct | ✅ through HA |
| Sensor ingestion | ✅ MQTT subscribe + direct adapters | ✅ same + HA passthrough | ✅ HA passthrough |
| Sensor entities visible in HA | ❌ | ✅ via MQTT discovery | ✅ HA owns them |
| HA automations on LocalSky verdicts | ❌ | ✅ via MQTT entities | ✅ direct in HA |
| Web Push notifications | ✅ | ✅ | ✅ |
| LLM advisor | ✅ | ✅ | ✅ |
| Mobile PWA | ✅ | ✅ | ✅ |
| Configuration surface | LocalSky `/settings` | LocalSky `/settings` | LocalSky `/settings` |
| LocalSky depends on HA | No | No | Yes (for dispatch only) |

Pick the row that matches your current setup and your future direction.
