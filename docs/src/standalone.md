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
- Direct controller dispatch (OpenSprinkler HTTP API, Rachio, Hydrawise, B-hyve, Rain Bird, MQTT command)
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
| Zigbee sensors (Aqara, Sonoff) | Via Zigbee2MQTT (no HA needed) | Subscribe to `zigbee2mqtt/<friendly_name>`. Already on ZHA or Z2M feeding HA? Use the HA passthrough source (kind = `ha_passthrough`) instead; no re-pairing. |
| Ecowitt gateway (WH51, WH52) | Via ecowitt2mqtt sidecar | Subscribe to `ecowitt/<device_id>` |
| Shelly devices | Native MQTT (firmware setting) | Subscribe to `shellies/<device>/<field>` |
| Arbitrary Arduino / Pi project | PubSubClient / paho-mqtt | Subscribe to whatever topic you publish |

Zigbee2MQTT is a particularly good fit. It's a single Docker container that talks to a USB Zigbee coordinator (Conbee II, Sonoff dongle, etc.) and publishes every Zigbee device's state to MQTT. No HA required.

### Path 2: Direct LAN adapters

For sensors that speak a documented LAN protocol, LocalSky can talk to them directly without MQTT in the middle.

| Sensor | Adapter | Status |
|---|---|---|
| Tempest hub (UDP broadcast 50222) | `tempest_udp` | Shipped |
| Ecowitt GW1100 / GW2000 (LAN push to `/ingest/ecowitt`) | `ecowitt_local` | Shipped |
| Ecowitt GW1100 / GW2000 (native LAN poll, incl. per-channel soil calibration) | `ecowitt_gw_poll` | Shipped |
| Ambient Weather (cloud REST) | `ambient_weather` | Shipped |
| ESPHome native API (protobuf over TCP) | `esphome_native` (sensor mode) | Planned |

Direct adapters bypass MQTT entirely; the device talks to LocalSky's listener directly. Less infra, no broker. Use when the device supports a documented protocol that LocalSky has an adapter for.

#### Networking note: Tempest UDP

The Tempest hub broadcasts on UDP port 50222, and broadcasts do not cross Docker's default bridge network. Run the LocalSky container with `network_mode: host` (the repo's `docker-compose.yml` already does) so the listener actually hears the hub. On a multi-homed host this also lets one NIC face the sensor subnet while another handles outbound API calls.

### Path 3: HTTP webhook receiver

For sensors with arbitrary HTTP push capability (some commercial weather stations, custom scripts), the generic `http_webhook` source accepts JSON POSTs directly:

```toml
[[sources]]
id = "lawn"
kind = "http_webhook"
[sources.config]
path = "/ingest/lawn"
token = "${WEBHOOK_TOKEN}"     # optional; sent via X-LocalSky-Token header or ?token=

[[sources.config.fields]]
field = "air_temp_f"
json_path = "outdoor.temp"     # drill into the JSON payload
scale = 1.0
offset = 0.0
```

The device POSTs JSON to `http://localsky:8090/ingest/webhook/lawn` (the URL segment is the source `id`).

Field names use the same snake_case weather-field vocabulary as the MQTT source, and the same `json_path` + `scale`/`offset` transform scheme.

## Controller dispatch without Home Assistant

You have several direct paths:

### OpenSprinkler (the recommended hardware)

Direct HTTP API on the LAN. See [docs/controllers.md](controllers.md#opensprinkler-the-ideal). US$130-180 hardware; the engine talks to it without anything else in the middle.

### ESP32 / DIY (open hardware)

For full open hardware: an ESP32 + relay board, ~US$15-40 in parts. LocalSky drives it two ways, no HA needed: the `http_generic` controller (LocalSky polls a small REST contract; a copy-and-flash Arduino sketch ships in `examples/http/`), or the `mqtt_command` controller for boards that speak MQTT (ESPHome/Tasmota; reference ESPHome firmware in `examples/esphome/`). See [DIY & ESP32 controllers](diy-controllers.md). A native ESPHome protobuf adapter is scaffolded but not yet built.

### Rachio, Hydrawise, B-hyve, Rain Bird

Vendor cloud or LAN APIs, no HA required. LocalSky ships direct adapters for all four; see [docs/controllers.md](controllers.md) for setup per vendor.

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

## Smart Irrigation? Irrigation Unlimited? Are those needed?

No. LocalSky's engine is a complete, native replacement for both:

- **Smart Irrigation (HACS)** does ET₀ + per-zone bucket + Kc + planned-run-seconds. LocalSky's [engine/et0.rs](../src/engine/et0.rs) + [engine/water_balance.rs](../src/engine/water_balance.rs) + [engine/species_catalog.rs](../src/engine/species_catalog.rs) do the same thing with the same FAO-56 math.
- **Irrigation Unlimited (HACS)** does schedule sequencing + zone dispatch. LocalSky's [engine/skip_rules.rs](../src/engine/skip_rules.rs) + [engine/budget.rs](../src/engine/budget.rs) + the controller HAL do the same thing.

The clean-room rewrite was deliberate: both projects are excellent and were the prior art that proved this design space works. LocalSky absorbs their lessons + adds:

- Multi-source weather merge with provenance
- Native ET₀ from station readings (not just forecast model output)
- Cycle-and-soak runoff prevention
- A real first-run wizard
- Settings UI
- Multi-controller HAL
- An LLM advisor

For an existing HA user already running SI + IU: see [Mode 3 in getting-started.md](getting-started.md#mode-3-ha-driven-legacy-continuity). LocalSky's `ha_service_call` controller can still dispatch through SI + IU if you'd rather keep your existing setup and use LocalSky for the dashboard + skip-rule engine only.

## The "I'm against HA" reality check

Some objections to HA we hear and how LocalSky stands up:

| Objection | LocalSky position |
|---|---|
| "HA is too heavy for one feature" | Agreed. LocalSky is one Docker container, ~30 MB resident. |
| "HA pulls in Python deps I don't trust" | Agreed. LocalSky is a single Rust binary; no plugin system, no eval'd YAML, no Python at all. |
| "I don't want a YAML automation layer" | Agreed. LocalSky's logic is in compiled Rust + a typed TOML config; no automation YAML. |
| "I want a focused single-purpose tool" | Agreed. LocalSky does irrigation + weather. That's it. |
| "I'm worried HA's roadmap will diverge from mine" | Agreed. LocalSky is governed by its repo + its license; the project's scope is irrigation forever. |
| "HA's UX isn't great for irrigation" | Agreed. LocalSky's UI was built for irrigation first, dashboard second. |

LocalSky is for the user who wants the irrigation engine without buying into the broader home automation philosophy. If you're an HA user already, LocalSky still plays well (Mode 2 / Mode 3); if you're not, you're not missing anything.

## Already running HA? There's a native integration

If you do run Home Assistant, LocalSky ships a native HA integration (installed via HACS) that creates LocalSky's entities and services in HA directly over the REST/SSE API, no MQTT broker needed. See [docs/hacs.md](hacs.md).

## Summary table

| Capability | Standalone | HA + LocalSky (Mode 2: outbound) | HA + LocalSky (Mode 3: HA-driven) |
|---|---:|---:|---:|
| Weather dashboard | ✅ | ✅ | ✅ |
| Engine (ET, bucket, skip rules) | ✅ | ✅ | ✅ |
| Controller dispatch | ✅ direct | ✅ direct | ✅ through HA |
| Sensor ingestion | ✅ MQTT subscribe + direct adapters | ✅ same + HA passthrough | ✅ HA passthrough |
| Sensor entities visible in HA | ❌ | ✅ via MQTT discovery | ✅ HA owns them |
| Native HA integration ([HACS](hacs.md)) | ❌ (no HA) | ✅ recommended over MQTT discovery | ✅ |
| HA automations on LocalSky verdicts | ❌ | ✅ via MQTT entities or the HACS integration | ✅ direct in HA |
| Web Push notifications | ✅ | ✅ | ✅ |
| LLM advisor | ✅ | ✅ | ✅ |
| Mobile PWA | ✅ | ✅ | ✅ |
| Configuration surface | LocalSky `/settings` | LocalSky `/settings` | LocalSky `/settings` |
| LocalSky depends on HA | No | No | Yes (for dispatch only) |

Pick the row that matches your current setup and your future direction.
