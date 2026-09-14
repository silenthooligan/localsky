# Sensors

LocalSky's engine produces useful output with just weather and a location. **Every sensor you add unlocks more behavior**, but nothing is required. The dashboard shows empty states with "connect a sensor to unlock _X_" affordances where data would otherwise live.

> **For standalone (no HA) users**: the question "how do my sensors get into LocalSky without HA?" has a thorough answer in [docs/standalone.md](standalone.md#sensor-ingestion-without-home-assistant). Short version: run any MQTT broker (Mosquitto is free, 5 MB), point Tasmota / ESPHome / Zigbee2MQTT at it, and LocalSky's `mqtt` source subscribes to the topics you configure. HA never touches it.

## Always-on baseline (no sensors required)

Just from weather forecasts + your latitude/longitude, LocalSky computes:

- FAO-56 reference ET₀ (Hargreaves fallback when only temp range is available; Penman-Monteith when wind + solar + humidity show up)
- Crop ET per zone from species-specific Kc curves
- Weekly per-zone water balance sizing every run
- {{LOCALSKY_SKIP_RULES}}-rule skip ladder (rain forecast, freeze, wind, already-wet, etc.)
- 7-day verdict strip projection
- Cycle-and-soak runtime splitting

The dashboard renders cleanly with this alone. The verdict tile shows green/yellow/red, the zone cards show planned next-run, the weather panels show forecast data, the radar shows local conditions.

## Receiver sources (push, not poll)

Most sources are ones LocalSky reaches out to on a timer. Three work the other way around: the hardware decides when to talk, and LocalSky records what shows up. You host all three yourself, so none of them needs a vendor cloud in the middle.

- **Ecowitt LAN push** (`ecowitt_local`): the gateway posts form-encoded readings to `/ingest/ecowitt` on the interval you set in its own **Weather Services > Customized** screen. Each source's optional shared secret decides whether it accepts a given report, and LocalSky reads that secret out of the posted form body under `PASSKEY`, `passkey`, or `key`. A secret appended to the upload URL is never looked at. Several gateways can post to the same endpoint and each report reaches the weather merge through every source whose secret it matches, but the per-field readings log and the soil channels the zone picker binds are recorded only for the first `ecowitt_local` source in your config, gated on that source's secret and stamped with its id. Soil probes on a second gateway are attributed to the first source or dropped, so keep soil on one gateway.
- **HTTP webhook** (`http_webhook`): anything that can POST JSON (a commercial station, a Pi, a cron script) posts to `/ingest/webhook/<source id>`, where the last path segment is the source's own id. An optional token rides in `?token=` or the `X-LocalSky-Token` header.
- **MQTT** (`mqtt`): LocalSky subscribes to the topics you list on your broker and maps each topic to a field. This one opens the connection rather than waiting for it, but the rhythm is the same: the sensor publishes on its own schedule and LocalSky takes what arrives.

Both HTTP endpoints are mounted twice, under `/ingest/...` and under `/api/v1/ingest/...`, so a device already pointed at either address keeps working.

Add any of them from the Sensors page with **Add a data source**. In the kind picker, the Ecowitt receiver sits under "Local weather station" while MQTT and the webhook sit under "Sensors & data bridges". The per-kind wiring, broker included, is in [standalone mode](standalone.md#sensor-ingestion-without-home-assistant).

**Confirming it works.** Open the source on the Sensors page. A receiver logs every field it takes in, so its detail pane lists the latest value per key with the age of the newest one, which is how you tell live data from a hopeful configuration. Before the first report lands, an Ecowitt push or webhook source says so and names the endpoint to point the device at. Read that log rather than the HTTP response: once at least one `ecowitt_local` source exists, an Ecowitt gateway gets a 200 back on every POST whether or not the reading was kept, because a gateway that sees anything else starts retrying in a storm. With no `ecowitt_local` source configured, which is what a gateway pointed at LocalSky before you add the source hits, the endpoint answers 503 instead. Two things make a report vanish quietly. A shared secret configured on the source that the gateway does not send drops the reading, and a wrong webhook token, or a payload that maps to no field, answers 422. A polled or cloud source logs its readings here too, so the same pane fills in once it has posted a cycle. The line about keeping no separate per-field log is the fallback the pane shows for a source that is not an Ecowitt push or webhook receiver, an MQTT source waiting on its first message included, and when you see it the source's status word is how you check on it.

Hardware you did not point at LocalSky by hand shows up on the [Devices](devices.md) page instead. That is the one hub for every source and controller, and it carries the **Scan the network** button that finds Ecowitt gateways on the LAN.

## Optional sensors and what they unlock

### Soil moisture probes

Examples: Ecowitt WH51 / WH52 (battery), Aqara Zigbee, Sonoff Zigbee, capacitive-soil-moisture sensors on ESPHome.

**Unlocks**:
- **Yard-wide saturation skip rule**: when every zone reports moisture at or above its saturation threshold, the engine skips the run.
- **Per-zone soil moisture display**: a horizontal bar per zone showing current moisture vs. target band.
- **Soil-moisture projection**: 7-day forward curve under no-irrigation, color-coded for "stays in healthy band" vs. "will dry out".
- **Smarter dry-out detection**: catches the case where ET-based math underestimates actual drying (heavy clay holding water visibly longer than expected, or sandy spots draining faster).
- **Anomaly detection** (new in 0.7.0): a probe that goes offline, or reads as a wild outlier versus its neighbors, is flagged on the irrigation and zones views so you know when to check the hardware.
- **Tuning calibration checks**: the [tuning report](tuning-report.md) compares the probe's drying rate against the configured soil model and backs the real sprinkler rate out of the probe's rise across waterings. Both need LocalSky's own probe history, so they unlock for a zone bound to a `source:` channel: a gateway poll, the Ecowitt LAN push receiver, an MQTT subscription, a webhook field, or a Home Assistant entity routed through the passthrough source's `soil_zone_map`. A zone bound straight to a live `ha:<entity_id>` pick has no local history and gets neither check.

**Connect via**: the native Ecowitt gateway poll, the Ecowitt LAN push receiver, an MQTT subscription or webhook field bound to a zone, or any Home Assistant soil entity. Once the readings are flowing, assign each probe to its zone; see [Assigning soil probes to zones](#assigning-soil-probes-to-zones) below.

### Soil temperature probes

Examples: Ecowitt WH51 (same physical probe as moisture), Aqara temp/humidity in the ground.

**Unlocks**:
- **Soil-frost skip rule**: spraying frozen ground freezes water on contact. Soil temperature lags air temperature substantially; the engine catches the "cold soil + sunny morning" case better than air-temp alone.

### Discrete rain gauge

Examples: Ecowitt RG200, AcuRite tipping bucket, RainWise.

**Unlocks**:
- **Higher rain-today accuracy** when your weather station's onboard gauge is less reliable than a dedicated unit (or you don't have a weather station at all).
- **Merge engine takes the max** across rain sources, so adding a gauge can only improve accuracy.
- **Honesty labels** (new in 0.7.0): every reading carries an honesty label so you know how it was obtained: measured (a real gauge or station), radar (live Doppler, for example NOAA MRMS rain), real-time nowcast, or model forecast.

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

Wire a moisture probe to a zone and the engine gains a measured gate: a saturated zone skips on its own, and a measured-dry zone can override a soft forecast-rain skip.

**Supported paths in:**

- **Ecowitt soil probes (WH51 and friends) via a LAN gateway**: native, no cloud. The `ecowitt_gw_poll` source polls the gateway directly and records moisture, temperature, conductivity, and battery per probe; the `ecowitt_local` push receiver works too.
- **Any Home Assistant soil sensor entity**: a Zigbee probe on ZHA, a Z-Wave probe, anything HA already knows about.
- **A DIY probe on MQTT or an HTTP webhook**: the subscription or field mapping carries a zone binding, which records the value as that zone's own soil channel instead of merging it into the global humidity reading. See [soil probes and zones](soil-sensors.md).

**Assignment** happens in the zone's settings: **Settings > Zones > pick the zone > soil sensor**. A reading you map by hand, an MQTT subscription or a webhook field, is bound to its zone on the source first and picked here second. One probe per zone. The picker lists every soil channel LocalSky has discovered: native gateway channels appear as `source:<source_id>:soilmoisture<N>`, a hand-mapped channel you bound to a zone appears as `source:<source_id>:soilmoisture_<zone_slug>`, and HA entities appear as `ha:<entity_id>`. The Sensors hub shows which zones each source feeds.

**How the engine uses it:**

- Below the zone's target band: the zone is eligible, and a measured-dry zone can override a soft forecast-rain skip. Run length is unchanged; it comes from the weekly water balance.
- Inside the band: healthy; scheduled runs still apply unless the saturation threshold says otherwise.
- At or above saturation: the zone skips on its own, even when the day's verdict is Run, and the skip reason names the probe.

The Sensors hub and each zone's detail show the probe's live reading, the target band, and a 7-day no-watering projection so you can sanity check that the moisture curve actually behaves like your yard. If the probe goes offline, the zone simply loses its soil gate; nothing blocks, and run sizing is unaffected because the weekly water balance never reads a probe.

### Worked example: a Home Assistant sensor feeding LocalSky

Say HA owns a Zigbee soil probe (`sensor.back_yard_soil_moisture`) and an outdoor thermometer (`sensor.patio_temperature`), and you want both in LocalSky.

**Step 1: add the weather source in the UI.** In **Settings > Devices**, add
**HA passthrough**, enter the HA URL and a long-lived token, then use **Field
mappings** to connect readings to the original HA sensor entities. A weather
bridge does not require environment variables or hand-edited TOML. Create the
token in HA under your profile > Security, and restart LocalSky after saving a
new connection.

If you also use the legacy HA soil-entity picker described in step 3, its HA
entity discovery uses the container's `HA_URL` and `HA_TOKEN` (or
`HA_LONG_LIVED_TOKEN`) environment variables:

```yaml
# docker-compose.yml
environment:
  - HA_URL=http://10.0.0.10:8123
  - HA_TOKEN=${HA_LONG_LIVED_TOKEN}
```

Create the long-lived token in HA under your profile > Security.

**Step 2: weather fields go through the HA passthrough source.** The HA passthrough source (kind = `"ha_passthrough"`) maps weather fields to HA entity ids via `field_map` and polls HA's `/api/states` every 30 seconds:

```toml
[[sources]]
id = "ha_bridge"
priority = 50
enabled = true
kind = "ha_passthrough"
[sources.config]
base_url = "http://10.0.0.10:8123"
bearer_token = "${HA_LONG_LIVED_TOKEN}"
[sources.config.field_map]
air_temp_f = "sensor.patio_temperature"
```

Field-map keys are LocalSky weather field names (`air_temp_f`, `rh_pct`,
`wind_mph`, `rain_today_in`, and so on); values are HA entity IDs. The example
uses priority **50**, matching the generic add-source form. Legacy environment
synthesis uses **30** for HA; saved priorities remain configurable. Choose HA
per reading in **Settings > Devices > Which source provides each reading** when
it should be preferred. Numeric
priority orders eligible sources; live observations and forecast fills remain
separate tiers. Entities reporting `unavailable` or `unknown` supply no numeric
sample and are not converted to zero.

The mapping picker also supports illuminance, lightning count and lightning
distance. For **Rain today**, use a verified daily accumulated total. HA
WeatherFlow's precipitation sensor covers the preceding minute and cannot feed
that field directly. Use another observed-rain source if no daily total exists.
Do not map LocalSky's own HA exports back into its inputs. HA passthrough reads
current values every 30 seconds; keep a forecast provider enabled. See the
[WeatherFlow handoff guide](migrating-from-ha.md#keeping-weatherflow-in-home-assistant).

**Step 3: the soil probe is assigned per zone, not through field_map.** Open **Settings > Zones > Back Yard > soil sensor** and pick the probe; it appears in the list as `ha:sensor.back_yard_soil_moisture` (the picker reads HA's entity list using the credentials from step 1). From then on the probe gates that zone as described above.

## Swapping hardware

Replacing a station or probe with a new unit? Edit the **existing** source entry (keep its id) instead of deleting it and adding a fresh one. Sensor history is keyed by source id and channel, and zone run history is keyed by zone slug, so an in-place edit keeps your charts, calibration context, and history continuous. Deleting a source and re-adding it under a new id starts those series over.

## Empty states + progressive disclosure

The dashboard uses LocalSky's `<EmptyState/>` UI primitive to render tiles for sensor data the operator hasn't connected. Each empty state:

1. Shows the kind of data that would go there
2. Names what additional logic the data unlocks
3. Links directly to the Devices page (`/settings?section=devices`) with hints for compatible sources

Example: the soil moisture panel renders as:

> 🌱 **Add soil moisture data**
> Per-zone moisture projection, yard-wide saturation skip, and visible dry-out detection light up when you connect a soil probe. Compatible sources: Ecowitt WH51, Aqara, HA passthrough.
> **[Connect a sensor source →]**

Once a source is providing the field, the tile shows its readings. A soil probe affects a zone only after you assign it to that zone. Zones without an assigned probe use weather and the soil model. Once assigned, a missing, stale, or untrusted probe holds its zone until reliable readings return; Force and a schedule's weather-safety waiver cannot bypass that hold. Missing live weather is also a protected hold when no usable station or forecast fallback is available.

## Hardware compatibility matrix

| Sensor | Direct adapter | Via HA | Notes |
|---|---|---|---|
| Tempest hub (UDP) | Tested (v0.1) | Yes | Air temp, humidity, wind, solar, lightning, rain, pressure |
| Ecowitt GW1100/GW2000 LAN | Live (v0.1) | Yes | Native direct poll: `/get_livedata_info` for moisture/temp/EC/battery per channel, `/get_cli_soilad` for raw FDR AD used in calibration |
| Ecowitt WH51/WH52 (soil) | Live (v0.1) | Yes | Polled natively via gateway; LocalSky calibrates moisture per zone against dry/wet AD endpoints in its own config; battery-powered, 868/915 MHz |
| Aqara Zigbee | Via HA | Yes | Soil moisture + temp probes; needs Zigbee coordinator |
| Sonoff Zigbee | Via HA | Yes | Same as Aqara |
| Synoptic Data | Live (v0.7.0) | N/A | A free token pulls the nearest real weather station's measured wind, pressure, temperature, and humidity from a dense mesonet. Measured readings, but from the nearest station, which may be a few miles away |
| Ambient Weather | Planned | Yes | Cloud API; socket.io |
| AcuRite tipping bucket | Via Ecowitt or HA | Yes | |
| PurpleAir / AirGradient | Display only | Yes | No engine integration |
| OpenSprinkler flow sensor | Native | Yes | Read via `/jc` water level field |

## Adding a new sensor source

Same shape as adding a weather source. See `CONTRIBUTING.md` in the repository root. The `WeatherSource` trait expects per-tick `Observation { source_id, fields: Vec<(WeatherField, f64)> }` events; soil moisture is just another `WeatherField` variant (`SoilMoisturePct` per zone, planned).

For sensors not in the WeatherField enum (e.g. flow meter readings, ambient pollen), the path is to extend the enum + add a Display-only tile to the dashboard.

## "What if I have no sensors at all?"

You'll get:

- A working weather dashboard with forecast + radar
- An engine that schedules irrigation from ET + soil + species + Kc math
- A 7-day verdict strip
- An LLM advisor (if configured) explaining decisions

You won't get:

- Soil saturation skip (with no probe there is no measured gate, so the weekly water balance decides alone)
- Soil frost skip (covered by air-temp freeze rules)
- Flow-validated runs (the engine trusts that the controller ran the requested duration)

That's a fully usable setup. Sensors take it from "useful" to "trustworthy"; they're additive, not gating.
