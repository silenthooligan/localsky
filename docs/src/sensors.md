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

**Connect via**: any source that publishes `sensor.<zone_slug>_soil_moisture` (HA passthrough), or a direct adapter (Ecowitt GW1100/GW2000 native LAN poll, Aqara via HA, Tasmota via MQTT).

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
