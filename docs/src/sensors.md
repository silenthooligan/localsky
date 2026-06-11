# Sensors

LocalSky's engine produces useful output with just weather and a location. **Every sensor you add unlocks more behavior**, but nothing is required. The dashboard shows empty states with "connect a sensor to unlock _X_" affordances where data would otherwise live.

> **For standalone (no HA) users**: the question "how do my sensors get into LocalSky without HA?" has a thorough answer in [docs/standalone.md](standalone.md#sensor-ingestion-without-home-assistant). Short version: run any MQTT broker (Mosquitto is free, 5 MB), point Tasmota / ESPHome / Zigbee2MQTT at it, and LocalSky's `mqtt` source subscribes to the topics you configure. HA never touches it.

## Always-on baseline (no sensors required)

Just from weather forecasts + your latitude/longitude, LocalSky computes:

- FAO-56 reference ET₀ (Hargreaves fallback when only temp range is available; Penman-Monteith when wind + solar + humidity show up)
- Crop ET per zone from species-specific Kc curves
- Single-bucket water balance with TAW + MAD-driven scheduling
- 17-rule skip ladder (rain forecast, freeze, wind, already-wet, etc.)
- 7-day verdict strip projection
- Cycle-and-soak runtime splitting

The dashboard renders cleanly with this alone. The verdict tile shows green/yellow/red, the zone cards show planned next-run, the weather panels show forecast data, the radar shows local conditions.

## Optional sensors and what they unlock

### Soil moisture probes

Examples: Ecowitt WH51 / WH52 (battery), Aqara Zigbee, Sonoff Zigbee, capacitive-soil-moisture sensors on ESPHome.

**Unlocks**:
- **Yard-wide saturation skip rule**: when every zone reports moisture at or above its saturation threshold, the engine skips the run.
- **Per-zone soil moisture display**: a horizontal bar per zone showing current moisture vs. target band.
- **Soil-moisture projection**: 7-day forward curve under no-irrigation, color-coded for "stays in healthy band" vs. "will dry out".
- **Smarter dry-out detection**: catches the case where ET-based math underestimates actual drying (heavy clay holding water visibly longer than expected, or sandy spots draining faster).

**Connect via**: the native Ecowitt gateway poll, the Ecowitt LAN push receiver, or any Home Assistant soil entity. Once the readings are flowing, assign each probe to its zone; see [Assigning soil probes to zones](#assigning-soil-probes-to-zones) below.

### Soil temperature probes

Examples: Ecowitt WH51 (same physical probe as moisture), Aqara temp/humidity in the ground.

**Unlocks**:
- **Soil-frost skip rule**: spraying frozen ground freezes water on contact. Soil temperature lags air temperature substantially; the engine catches the "cold soil + sunny morning" case better than air-temp alone.

### Discrete rain gauge

Examples: Ecowitt RG200, AcuRite tipping bucket, RainWise.

**Unlocks**:
- **Higher rain-today accuracy** when your weather station's onboard gauge is less reliable than a dedicated unit (or you don't have a weather station at all).
- **Merge engine takes the max** across rain sources, so adding a gauge can only improve accuracy.

### Lightning detector

Examples: Tempest hub (built-in), Ecowitt WS6006, RainWise.

**Unlocks**:
- **Lightning panel**: shows last-strike distance + count over last 3 hours.
- **Safety skip during active storms**: paired with the existing rain rule; the engine doesn't fire valves when there's active lightning within a configurable radius (planned).

### Flow meter on the controller

Examples: OpenSprinkler flow meter input, Rachio flow sensors.

**Unlocks**:
- **Actual-delivered-water validation**: compares the flow-meter reading to the engine's computed mm depth. A discrepancy >20% indicates a stuck valve, a busted line, or a calibration drift.
- **Leak detection**: flow at zero-zones-running is a leak; the engine alerts.
- **Per-zone precipitation rate auto-calibration** (planned): the catch-cup measurement is replaced by automatic estimation from flow + zone area.

### Ambient air-quality / pollen / PM2.5 (display only)

Examples: PurpleAir, AirGradient, Ecowitt WH41.

**Unlocks**:
- Display tiles only. The engine doesn't make irrigation decisions on air quality (yet).

## Assigning soil probes to zones

Wire a moisture probe to a zone and the engine stops guessing: the probe's reading sits alongside the modeled bucket as the zone's gate.

**Supported paths in:**

- **Ecowitt soil probes (WH51 and friends) via a LAN gateway**: native, no cloud. The `ecowitt_gw_poll` source polls the gateway directly and records moisture, temperature, conductivity, and battery per probe; the `ecowitt_local` push receiver works too.
- **Any Home Assistant soil sensor entity**: a Zigbee probe on ZHA, a Z-Wave probe, anything HA already knows about.

**Assignment** happens in the zone's settings: **Settings > Zones > pick the zone > soil sensor**. One probe per zone. The picker lists every soil channel LocalSky has discovered: native gateway channels appear as `source:<source_id>:soilmoisture<N>`, HA entities as `ha:<entity_id>`. The Sensors hub shows which zones each source feeds.

**How the engine uses it:**

- Below the zone's target band: the zone is eligible; runs size to the deficit as usual.
- Inside the band: healthy; scheduled runs still apply unless the saturation threshold says otherwise.
- At or above saturation: the zone skips on its own, even when the day's verdict is Run, and the skip reason names the probe.

The Sensors hub and each zone's detail show the probe's live reading, the target band, and a 7-day no-watering projection so you can sanity check that the moisture curve actually behaves like your yard. If the probe goes offline, the zone falls back to the modeled bucket automatically; nothing blocks.

### Worked example: a Home Assistant sensor feeding LocalSky

Say HA owns a Zigbee soil probe (`sensor.back_yard_soil_moisture`) and an outdoor thermometer (`sensor.patio_temperature`), and you want both in LocalSky.

**Step 1: give LocalSky HA credentials.** HA-backed sensing uses the `HA_URL` and `HA_TOKEN` (or `HA_LONG_LIVED_TOKEN`) environment variables on the LocalSky container:

```yaml
# docker-compose.yml
environment:
  - HA_URL=http://192.168.1.10:8123
  - HA_TOKEN=${HA_LONG_LIVED_TOKEN}
```

Create the long-lived token in HA under your profile > Security.

**Step 2: weather fields go through the HA passthrough source.** The HA passthrough source (kind = `"ha_passthrough"`) maps weather fields to HA entity ids via `field_map` and polls HA's `/api/states` every 30 seconds:

```toml
[[sources]]
id = "ha_bridge"
priority = 30
enabled = true
kind = "ha_passthrough"
[sources.config]
base_url = "http://192.168.1.10:8123"
bearer_token = "${HA_LONG_LIVED_TOKEN}"
[sources.config.field_map]
air_temp_f = "sensor.patio_temperature"
```

Field-map keys are LocalSky weather field names (`air_temp_f`, `rh_pct`, `wind_mph`, `rain_today_in`, and so on); values are HA entity ids. Passthrough values merge at priority 30: above raw forecast data, below any direct station adapter, since they're a routed copy of some other system's reading. Entities reporting `unavailable` or `unknown` are skipped, not zeroed.

**Step 3: the soil probe is assigned per zone, not through field_map.** Open **Settings > Zones > Back Yard > soil sensor** and pick the probe; it appears in the list as `ha:sensor.back_yard_soil_moisture` (the picker reads HA's entity list using the credentials from step 1). From then on the probe gates that zone as described above.

## Swapping hardware

Replacing a station or probe with a new unit? Edit the **existing** source entry (keep its id) instead of deleting it and adding a fresh one. Sensor history is keyed by source id and channel, and zone run history is keyed by zone slug, so an in-place edit keeps your charts, calibration context, and history continuous. Deleting a source and re-adding it under a new id starts those series over.

## Empty states + progressive disclosure

The dashboard uses LocalSky's `<EmptyState/>` UI primitive to render tiles for sensor data the operator hasn't connected. Each empty state:

1. Shows the kind of data that would go there
2. Names what additional logic the data unlocks
3. Links directly to `/settings/sources` with hints for compatible sources

Example: the soil moisture panel renders as:

> 🌱 **Add soil moisture data**
> Per-zone moisture projection, yard-wide saturation skip, and visible dry-out detection light up when you connect a soil probe. Compatible sources: Ecowitt WH51, Aqara, HA passthrough.
> **[Connect a sensor source →]**

Once a source is providing the field, the tile lights up and the skip rules incorporating that field activate automatically. The engine never blocks on missing sensor data; weather + ET-based math is the always-on baseline.

## Hardware compatibility matrix

| Sensor | Direct adapter | Via HA | Notes |
|---|---|---|---|
| Tempest hub (UDP) | Tested (v0.1) | Yes | Air temp, humidity, wind, solar, lightning, rain, pressure |
| Ecowitt GW1100/GW2000 LAN | Live (v0.1) | Yes | Native direct poll: `/get_livedata_info` for moisture/temp/EC/battery per channel, `/get_cli_soilad` for raw FDR AD used in calibration |
| Ecowitt WH51/WH52 (soil) | Live (v0.1) | Yes | Polled natively via gateway; LocalSky calibrates moisture per zone against dry/wet AD endpoints in its own config; battery-powered, 868/915 MHz |
| Aqara Zigbee | Via HA | Yes | Soil moisture + temp probes; needs Zigbee coordinator |
| Sonoff Zigbee | Via HA | Yes | Same as Aqara |
| Ambient Weather | Planned | Yes | Cloud API; socket.io |
| AcuRite tipping bucket | Via Ecowitt or HA | Yes | |
| PurpleAir / AirGradient | Display only | Yes | No engine integration |
| OpenSprinkler flow sensor | Native | Yes | Read via `/jc` water level field |

## Adding a new sensor source

Same shape as adding a weather source. See [CONTRIBUTING.md](../CONTRIBUTING.md). The `WeatherSource` trait expects per-tick `Observation { source_id, fields: Vec<(WeatherField, f64)> }` events; soil moisture is just another `WeatherField` variant (`SoilMoisturePct` per zone, planned).

For sensors not in the WeatherField enum (e.g. flow meter readings, ambient pollen), the path is to extend the enum + add a Display-only tile to the dashboard.

## "What if I have no sensors at all?"

You'll get:

- A working weather dashboard with forecast + radar
- An engine that schedules irrigation from ET + soil + species + Kc math
- A 7-day verdict strip
- An LLM advisor (if configured) explaining decisions

You won't get:

- Soil saturation skip (the engine assumes the bucket model is correct, which it usually is)
- Soil frost skip (covered by air-temp freeze rules)
- Flow-validated runs (the engine trusts that the controller ran the requested duration)

That's a fully usable setup. Sensors take it from "useful" to "trustworthy"; they're additive, not gating.
