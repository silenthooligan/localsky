# LocalSky Irrigation, Manual & Rachio Comparison

> A self-hosted, sensor-driven, predictive irrigation stack. Built to do
> everything Rachio does and a substantial amount Rachio refuses to.
>
> **Last updated:** 2026-05-19

---

## What it is

LocalSky is the irrigation engine for a self-hosted lawn + garden installation.
It is the user-facing front end (`your LocalSky URL`) and the deterministic
Rust service behind it. LocalSky decides, schedules, and actuates OpenSprinkler
directly (HTTP to the controller). It polls the Ecowitt GW1100B gateway natively
for per-zone soil moisture, soil temp, EC, and raw FDR AD values, and calibrates
moisture per zone itself. It publishes sensor state back to Home Assistant so HA
can surface entities and drive automations, but HA is not the irrigation back-end.
The hardware stack is: LocalSky + OpenSprinkler + Ecowitt soil sensors + a
Tempest weather station.

It runs **entirely on local hardware**. There is no Rachio-cloud equivalent.
The only outbound traffic is an Open-Meteo forecast pull every 30 minutes
(free, no API key, no account); everything else, including the dashboard,
the SSE event stream, the decision engine, and the run history, lives on
your LAN.

The stack is open-source end-to-end. Every component is replaceable.

---

## Architecture

```
                                ┌──────────────────────────────────────┐
                                │       your LocalSky URL  (TLS)       │
                                │   Caddy reverse proxy, OAuth gate    │
                                └──────────────────┬───────────────────┘
                                                   │
┌──────────────────────────────────────────────────┴──────────────────┐
│ container 281 · LocalSky (Rust / Leptos SSR + WASM)                 │
│   • Forecast intelligence engine (skip_logic::evaluate)             │
│   • 7-day verdict strip + soil-moisture projection                  │
│   • Live Tempest UDP listener (port 50222)                          │
│   • Open-Meteo forecast refresher (30 min)                          │
│   • Ecowitt GW1100B poller (native HTTP, /get_livedata_info +       │
│     /get_cli_soilad); per-zone moisture calibration                 │
│   • Irrigation scheduler: computes per-zone durations,              │
│     applies skip/override rules, actuates OpenSprinkler via REST    │
│   • SQLite history (365-day run log)                                │
│   • Web Push via VAPID (PWA notifications)                          │
│   • SSE stream at /api/irrigation/stream                            │
│   • LLM advisor (optional)                                          │
│   • Publishes sensor.localsky_* entities to HA via MQTT discovery   │
└────┬──────────────────────────────────────────┬───────────────────┬─┘
     │ HTTP REST (actuate)                       │ HTTP (LAN poll)   │ MQTT discovery
     ▼                                           ▼                   ▼
┌─────────────┐                      ┌────────────────────┐  ┌──────────────────────┐
│OpenSprinkler│                      │ Ecowitt GW1100B    │  │ container 279        │
│ 192.0.2.60  │                      │ 192.0.2.61         │  │ Home Assistant       │
│ DC, latching│                      │  + 4× WH52 3-in-1  │  │ (entity aggregation, │
│ 4 zones     │                      │ polled every 60 s  │  │  automations, voice) │
└──────┬──────┘                      └────────────────────┘  └──────────┬───────────┘
       │ DC pulses (~25 ms)                                              │ ZHA / Zigbee
       ▼                                                                 ▼
   solenoids                                                  ┌──────────────────┐
                                                              │ Water-leak nodes │
                                    ┌──────────────────────┐  │ (Samjin IM6001)  │
                                    │ WeatherFlow Tempest   │  └──────────────────┘
                                    │ hub 192.0.2.62        │
                                    │ UDP 50222 every 3 s   │
                                    │ 1 min full obs         │
                                    └──────────────────────┘
```

**One-paragraph elevator pitch.** LocalSky takes a Rust engine that reads
both a real on-premise weather station and a 7-day forecast, blends it
with calibrated soil sensors and an evapotranspiration bucket per zone,
and decides whether and how much to water tomorrow morning. It then
actuates OpenSprinkler directly via HTTP, with per-zone durations computed
and overrides applied inside the engine itself. Everything is observable,
every threshold is tunable, every line of code is yours.

---

## Hardware bill of materials

| Component | Model | Role | Notes |
|---|---|---|---|
| **Sprinkler controller** | OpenSprinkler DC (8-zone) | Drives the solenoids | Open hardware, **DC + latching solenoid support** (Rachio is 24 VAC only). Drinks ~0.5 W idle. |
| **Weather station** | WeatherFlow Tempest | Hyperlocal weather | All-in-one wind/rain/temp/humidity/pressure/UV/illuminance/lightning. Pushes to your hub on UDP 50222 every 3 s. |
| **Hub** | Tempest Hub | UDP relay | LAN-only when paired; no cloud account required for local UDP path. |
| **Soil sensors** | Ecowitt WH52 ×4 | Per-zone soil | 3-in-1: capacitive moisture (FDR), soil temp, EC. AAA battery, IP66. 12-month battery life. |
| **Soil hub** | Ecowitt GW1100B | RF receiver, LAN HTTP API | LocalSky polls `/get_livedata_info` and `/get_cli_soilad` every 60 s directly. No push relay or sidecar needed. |
| **Server** | Proxmox host (any x86) | container fleet | LocalSky in container 281, HA in container 279. Modest spec (~2 cores, 4 GB total for both). |
| **Reverse proxy** | Caddy (container 220) | TLS + optional OAuth gate | Bypasses webhook + static asset routes per [feedback_oauth_gate_vs_crossorigin.md]. |
| **Optional add-ons** | UDM Pro, Cloudflare Tunnel | Edge networking | For remote access. Stack works without any of these. |

Total BOM on a fresh install lands around **$400-700 one-time** depending on
station/sensor choices, with **no recurring fees**. Rachio 16-zone + Wireless
Flow Meter + Valve Monitoring unlock = ~$500-550 with no soil sensors and
hard cloud dependency.

---

## Software stack

| Layer | Component | What it owns |
|---|---|---|
| **Engine** | LocalSky (Rust) | Forecast intelligence, skip/run verdict, FAO-56 ET bucket per zone, per-zone scheduling, OpenSprinkler actuation, Ecowitt soil polling + calibration, 7-day verdict strip, 7-day moisture projection, SSE stream, dashboard, run history, push notifications, LLM advisor |
| **Soil sensors** | Ecowitt GW1100B + 4× WH52 | LocalSky polls `/get_livedata_info` + `/get_cli_soilad` directly; gateway pushes every 60 s; LocalSky owns calibration |
| **Controller** | OpenSprinkler (DC, 8-zone) | Receives per-zone run commands via LocalSky HTTP REST; executes DC pulses to latching solenoids |
| **HA integration** | Home Assistant | Receives `sensor.localsky_*` entities via MQTT discovery; provides entity registry, automations, voice, and alerting |
| **MQTT broker** | Eclipse Mosquitto | Local broker for LocalSky outbound discovery |
| **Edge proxy** | Caddy | TLS, OAuth, asset bypass |

---

## Feature matrix

Side-by-side with Rachio 3 (the current consumer flagship, May 2026
firmware). "✅" = shipping, "🟡" = partial / requires add-on / opaque,
"❌" = not available. Notes follow.

| Dimension | Rachio 3 | LocalSky stack |
|:---|:---:|:---:|
| **Connectivity** | | |
| Operates fully offline (no internet) | ❌ (degrades to fixed schedule after ~2 weeks) | ✅ (LAN-only; only outbound is Open-Meteo every 30 min) |
| Local API for automation | ❌ (cloud API only, 1700 calls/day) | ✅ (HA REST + WebSocket + LocalSky REST + SSE + MQTT) |
| Vendor cloud lock-in | ✅ required | ❌ none |
| Subscription required | ✅ for some features | ❌ never |
| **Weather inputs** | | |
| Hyperlocal forecast | 🟡 (interpolated PWS grid) | ✅ (Open-Meteo lat/lon precise to ~5 km) |
| On-prem weather station | 🟡 (Tempest as 1st-party PWS, cloud-routed) | ✅ (Tempest via direct LAN UDP every 3 s) |
| Tempest UDP rapid_wind (3 s) | ❌ (cloud-only path) | ✅ |
| Lightning event push | ❌ | ✅ (Tempest event field, surfaced as HA event entity) |
| **Skip rules** | | |
| Rain skip | ✅ | ✅ |
| Freeze skip (air temp) | ✅ | ✅ |
| Overnight freeze look-ahead | ❌ | ✅ (24h forecast min) |
| **Soil-temp** frost skip | ❌ | ✅ (yard min from WH52 probes) |
| Wind skip | ✅ | ✅ |
| Forecast peak wind skip | ❌ | ✅ (with +5 mph slack vs live limit) |
| 4-hour rain forecast skip | ❌ | ✅ (sums next-4h hourly precip) |
| 3-day probability-weighted rain skip | ❌ | ✅ (Σ daily × prob/100) |
| Heat advisory pre-water | 🟡 ("Heat Wave Boost" premium unlock, separately priced) | ✅ (built-in; Steadman heat index + ET multiplier) |
| **Soil sensor support** | | |
| First-party soil sensor | ❌ (no Rachio sensor product) | ✅ (Ecowitt WH52, $30/each) |
| Sensor-driven saturation skip | ❌ (virtual bucket only) | ✅ (per-zone moisture ≥ threshold) |
| Sensor-driven frost skip | ❌ | ✅ |
| Per-zone moisture binding | ❌ | ✅ |
| Operator-controlled calibration | ❌ (no sensor exists) | ✅ (LocalSky dashboard Capture DRY/WET, audit trail in run log) |
| Raw AD value visibility | ❌ | ✅ (separate `sensor.<zone>_soil_ad` diagnostic entity) |
| EC (fertilizer salt) trend monitoring | ❌ | ✅ (7-day mean + change statistics) |
| Soil temperature surface | ❌ | ✅ (per zone + yard min/max aggregates) |
| **Predictive view** | | |
| 7-day skip/run preview strip | ❌ (calendar shows next runs, not the engine's verdict for each) | ✅ (same engine runs against synthetic forecast inputs) |
| **7-day soil moisture projection** | ❌ (bucket is opaque) | ✅ (per-zone water-balance trajectory with target band overlay) |
| Forecast ET visibility | ❌ (ET is internal) | ✅ (`sensor.localsky_open_meteo_eto_today` + tomorrow + 3-day avg surfaced) |
| Per-zone bucket exposure | ❌ | ✅ (`bucket_mm` and planned duration surfaced per zone in the LocalSky dashboard and via `sensor.localsky_<zone>_budget_seconds`) |
| **Per-zone overrides** | | |
| Per-zone one-day skip | ❌ ("disable zone" is permanent until re-enabled) | ✅ (LocalSky zeroes that zone's duration when moisture ≥ saturation threshold) |
| Per-zone Kc bump on heat | ❌ | ✅ (LocalSky applies 120% duration when soil temp ≥ heat-stress threshold) |
| Per-zone target band (min ↔ max) | 🟡 (allowed depletion, internal-only) | ✅ (`target_min_pct` + `saturation_pct` operator-tunable) |
| Yard-wide saturation skip (engine-level) | ❌ | ✅ (all 4 zones ≥ threshold AND known) |
| **Scheduling** | | |
| Sun-relative trigger | 🟡 (sunrise as fixed time, no offset before sunrise) | ✅ (LocalSky anchors sequence to finish before sunrise, computes start time dynamically) |
| Inter-zone preamble (DC latching) | 🟡 (Rachio is AC only) | ✅ (configurable inter-zone preamble delay for DC latching solenoids) |
| Manual run with custom duration | ✅ | ✅ |
| Cancel next | ✅ | ✅ |
| **Operator visibility** | | |
| Engine reasoning surfaced | ❌ ("watered/skipped" with no explanation) | ✅ (skip-check `reason` string, logbook entries per gate) |
| LLM-generated daily explainer | ❌ | ✅ (the LLM provider via Extended OpenAI Conversation) |
| Run history retention | 🟡 (cloud, undocumented duration) | ✅ (365-day SQLite, queryable) |
| CSV/data export | 🟡 (daily totals only) | ✅ (full SQLite db; arbitrary SQL queries) |
| **Notifications** | | |
| Push to mobile app | ✅ (Rachio app) | ✅ (HA Companion app) |
| Email | ✅ | ✅ (HA `notify` services) |
| **Web push to PWA** | ❌ | ✅ (VAPID-signed, served by LocalSky) |
| Voice ack | 🟡 (Alexa/Google: cloud route) | ✅ (HA Assist local "GLaDOS" pipeline + Google route) |
| **Voice / home integration** | | |
| Alexa | ✅ | ✅ (HA Alexa skill) |
| Google Assistant | ✅ | ✅ (HA manual Actions SDK) |
| Apple HomeKit | ❌ (Rachio dropped 2022) | ✅ (HA HomeKit bridge integration) |
| Matter | ❌ | ✅ (HA's OTBR + Matter Server) |
| Thread | ❌ | ✅ (HA's OTBR) |
| IFTTT / SmartThings | ✅ | ✅ (HA bridges) |
| **Other** | | |
| HACS / open-source ecosystem | ❌ | ✅ |
| Per-zone calendar overrides | ❌ | ✅ |
| Multi-property | ❌ (one controller, one app account) | ✅ (one HA instance, many controllers) |
| DC + latching solenoid support | ❌ | ✅ |
| Flow meter | 🟡 (Wireless Flow Meter ~$200-250 add-on, accuracy gripes) | 🟡 (OpenSprinkler flow sensor input; not currently wired) |

**Where Rachio still wins, honestly:**
- A polished out-of-the-box experience: one box, one app, working in 30 minutes.
- Wider pool of curated PWS data via their hyperlocal model (300k+ stations).
- Single vendor support (one phone number when things break).
- No infrastructure for the user to maintain (no container, no Caddy, no certificates).

**Where LocalSky genuinely exceeds Rachio**, with technical specifics, follows.

---

## Capability deep-dive

### 1. Local-first operation

LocalSky runs on a Proxmox inside the user's LAN. The Rust binary
plus the SQLite history database is ~80 MB total. There is no Rachio
cloud equivalent, when your internet is down, the dashboard, the
scheduler, the soil sensors, the verdict engine, and the run history
are all still online and decisive.

The only external dependency is **Open-Meteo** (forecast, every 30 min,
free, no API key). If that fails, LocalSky degrades to live Tempest +
soil + last-known forecast snapshot. The skip-check rules silently fall
back to their default thresholds.

The official Rachio Home Assistant integration is documented "Cloud
Push", when Rachio.com is offline, your HA instance has no idea what
the controller is doing.

### 2. Hyperlocal weather, the actual kind

Rachio's "hyperlocal" weather is an interpolated grid stitched together
from PWS uploads. The user can override it to a specific PWS, including
a Tempest station, but the path is **cloud-routed**. Rachio's servers
ingest Tempest cloud data, blend it with their grid model, and emit a
verdict. The verdict reaches the controller via Rachio's cloud.

LocalSky's `tempest::Snapshot` is populated from the **Tempest UDP path
directly**: port 50222 on the sensor LAN, every 3 s for `rapid_wind`, every
1 min for `obs_st`. The verdict engine sees the same data the Tempest
hub sees, with no round-trip. Wind compass updates 20× per minute on
the dashboard.

If a thunderstorm sets up over your back yard at 6 AM, LocalSky sees
the wind shift and the precipitation start before Rachio's cloud has
even ingested the next Tempest cloud upload.

### 3. Forecast intelligence (Phase A)

The skip-check engine in [src/ha/skip_logic.rs](../src/ha/skip_logic.rs)
runs 14 rules in priority order. The marketing copy from Rachio
("hyperlocal weather intelligence") is a bucket of ~4-5 rules. The
LocalSky engine has all of those plus:

- **Next-4-hour rain skip**: `Σ hourly[0..4].precip ≥ 0.10"`: catches
  a shower that the daily total would mask.
- **Probability-weighted tomorrow rain**: `forecast_in × prob/100 ≥
  rain_skip_in`. A 0.4" forecast at 30% confidence is treated as 0.12",
  below threshold; a 0.4" at 90% confidence trips at 0.36".
- **3-day weighted rollup**: `Σ daily[1..4] × prob/100 ≥ 1.5 × threshold`
  for catching a multi-day rain event ahead.
- **Overnight freeze look-ahead**: minimum hourly forecast temp in the
  next 24 h; skips if dipping below the freeze threshold tonight even
  if the current run-time temp is fine.
- **Forecast peak wind**: `daily.wind_max_today_mph > max_wind + 5`,
  with the +5 mph slack because forecast peaks routinely overshoot.
- **Heat advisory (run_extended)**: if the 3-day forecast peak ≥ 95 °F,
  humidity ≥ 60%, ≥ 2 dry days, AND less than half a rain-skip's worth
  of weighted rain coming → bump the planned run +15% to pre-water
  ahead of the ET acceleration.

Every rule emits a **human-readable reason** that surfaces on the
dashboard verdict tile and in the HA logbook. The opaque "Saturation
skip" Rachio shows becomes `"Soil saturated (tightest: back yard 72% ≥
70% threshold)"`.

### 4. Soil sensors with operator-controlled calibration (Phase D)

Rachio has no first-party soil sensor and no native third-party binding.
Their "saturation skip" is a **virtual bucket** computed from ET + rain
+ soil/plant type. The user has no way to ground-truth it.

LocalSky uses **4× Ecowitt WH52 3-in-1 probes** polled natively by the
LocalSky Rust engine. LocalSky calls `/get_livedata_info` on the
GW1100B gateway every 60 s for per-zone moisture %,
soil temp, and EC, and `/get_cli_soilad` for raw FDR AD counts. The
gateway's factory moisture % is captured as a `_raw_pct` diagnostic;
LocalSky applies its own calibration on top.

The operator runs `Capture DRY` (probe in air) and `Capture WET`
(probe in saturated soil) from the dashboard. LocalSky maps raw AD
between captured endpoints linearly, clamps 0..100, and stores the
calibration in its own config (not in HA helpers). The audit trail
lands in LocalSky's run log and is surfaced as `sensor.localsky_<zone>_soil_ad`
alongside the calibrated `sensor.localsky_<zone>_soil_moisture` entities
published to HA. No HA-side template sensors, no `input_number` helpers,
no ecowitt2mqtt translation step is involved.

### 5. Predictive 7-day moisture projection (Phase E)

This is LocalSky's biggest single advantage over Rachio.

`compute_soil_forecasts` in [src/ha/refresher.rs](../src/ha/refresher.rs)
walks each daily forecast entry for the next 7 days and runs a
**FAO-56-flavored water balance** per zone:

```
delta_mm  = rain_mm × precip_probability/100 × CAPTURE_EFFICIENCY
          - et0_today_mm × heat_multiplier × zone_Kc
delta_pct = delta_mm / soil_depth_mm × 100
```

Where:
- `CAPTURE_EFFICIENCY = 0.7` (accounts for runoff, slope, canopy
  interception, 30% of forecast rain doesn't make it into the root
  zone),
- `zone_Kc` is the operator-configured crop coefficient (1.08 for turf, 0.50
  for shrubs),
- `soil_depth_mm` is the effective root zone (150 mm turf, 200 mm
  shrubs),
- `heat_multiplier` is the same Steadman-derived ET bump the engine
  applies to today's plan.

The result is a `Vec<SoilForecast>` per zone with:
- current_pct (today's calibrated live reading)
- predicted_pct[7] (each day in the window)
- min/max predicted, days_below_target, days_above_max
- a status pill: `ok` / `dry` / `wet` / `no_data`

The dashboard renders each zone as a tile with a 7-day sparkline
overlaid on the target band (`target_min_pct` ↔ `saturation_pct`). A
red status pill means the zone is going to fall through the floor at
some point this week even with the forecast rain factored in. A wet
status means heavy forecast rain will push it past saturation.

Rachio's calendar view shows the planned run; LocalSky's forecast view
shows **whether you'll be in your healthy band next Friday**. That's
the difference between "the engine ran something this morning" and
"the engine is keeping the lawn alive over the week."

### 6. Per-zone surgical overrides (Phase E)

Rachio has no per-zone calendar overrides. To skip one zone tomorrow,
the user disables the entire zone (which then stays disabled until
manually re-enabled).

LocalSky computes per-zone durations and applies overrides inside the
engine before sending the run command to OpenSprinkler. The decision
pipeline runs nightly at:

```
23:30:00  FAO-56 bucket math → per-zone base duration
23:30:30  verdict check      → suspend entire sequence if LocalSky says skip
23:30:35  saturation check   → zero duration for zones where moisture ≥ threshold
23:30:40  heat-stress bump   → 120% duration for zones where soil temp ≥ threshold
```

Saturation runs before heat-stress so `120% × 0 = 0` (saturation wins
when a zone is both wet and hot). Each rule logs the per-zone decision
to LocalSky's run log and publishes the result as `sensor.localsky_*`
entities in HA. OpenSprinkler receives the final per-zone run commands
directly via HTTP REST.

### 7. Soil-temp frost skip + warm-season binary

Air-temp freeze skip is fine for "don't water in literal frost." But
the **soil** temperature is the actual constraint, soil retains heat
slower than air, so a 38 °F dawn after a 28 °F overnight has soil
still cold enough that sprays freeze on contact and dormant roots
won't drink.

LocalSky's skip-logic ladder fires `Soil frost` when
`sensor.localsky_soil_temp_yard_min < irrigation_frost_skip_f` (default
35 °F). The yard-min is a min/max aggregate over the 4 zone temps.

Inverse use case: `binary_sensor.localsky_warm_season_active` flips on
when the 7-day rolling minimum of yard-min soil temperature crosses
`soil_warm_season_threshold_f` (default 65 °F). For Florida
St. Augustine and Bahia, ≥ 65 °F sustained = pre-emergent window
opens. The user can hang their own automation off this in HA; Rachio
doesn't expose anything like it.

### 8. EC (electrical conductivity) for fertilizer salt monitoring

Each WH52 also reports soil EC (µS/cm). LocalSky tracks statistics
for each zone and publishes them to HA:

- `sensor.localsky_<zone>_ec_mean_7d`: rolling 7-day mean
- `sensor.localsky_<zone>_ec_change_7d`: total change across the 7-day window

Rising EC = salt accumulation (fertilizer or coastal intrusion).
Sudden drop after rain = leaching event. The dashboard surfaces both
plus an EC-flush threshold input_number. No automation acts on EC
yet, but the user can see it. Rachio doesn't surface fertilizer or
salt at all.

### 9. Operator visibility & explainability

Every irrigation decision lands in three places:

1. **HA logbook** entry with timestamp and the per-zone reason
   (e.g. `"back_yard saturation-skip: soil 75.2% ≥ 70% threshold;
   IU zone 1 muted for tomorrow"`).
2. **LocalSky `skip_check.reason`** in the snapshot JSON, displayed
   on the verdict tile.
3. **LLM advisor** (optional, when the LLM provider is reachable) emits
   a daily human-language explanation: "Skipping back yard tomorrow
   because the probe is reading saturated; expecting 0.3" of rain
   Wednesday so the other zones can wait."

Rachio shows you "Saturation skip" with no underlying numbers; the
advanced settings that drive the decision are app-only and the model
itself is closed.

### 10. Notifications

LocalSky ships **VAPID-signed web push** so the PWA can wake an iPhone
or Pixel home-screen icon directly. Notifications fire on:

- Zone start
- Zone stop
- Daily verdict (the morning verdict, with reason)
- Heat advisory triggered
- Low soil battery (any WH52 binary `on`)
- Sequence skipped (with the reason)

Rachio is push-to-mobile-app only; no PWA path. The HA stack adds
mobile push via the Companion app **and** voice ack via the local
Assist pipeline.

### 11. History + export

LocalSky maintains a **365-day SQLite-backed run history** at
`/data/irrigation.db` per [src/history/](../src/history/). Every IU
finish event, every skip event, every manual run is recorded with
zone, duration, start/end epoch, and (when known) flow volume.

The dashboard surfaces:
- Per-zone Gantt strip (last 14 days)
- Utilization heatmap (daily hours by zone)
- Run-count + total-minutes summary

The SQLite file is queryable from any host (`sqlite3 irrigation.db`)
for ad-hoc reports. CSV export is a one-liner. Rachio's app shows a
per-event history list but their CSV export is **daily totals only**,
a multi-year-open community feature request.

---

## Cost analysis

| Item | Rachio path | LocalSky path |
|---|---|---|
| Controller hardware | Rachio 3 16-zone $250 | OpenSprinkler 16-zone $179 |
| Outdoor enclosure | $30 add-on | $0 (OpenSprinkler IP65 stock) |
| Weather station | Tempest $339 (paired to Rachio cloud) | Tempest $339 (paired to local hub) |
| Soil sensors | Not available (or 3rd-party IFTTT-only) | 4× WH52 $30 each + GW1100B $69 = $189 |
| Flow meter | Rachio Wireless Flow Meter $200-250 | OpenSprinkler flow sensor input (BYO sensor ~$20-40) |
| Valve Monitoring | $30 one-time unlock | Free |
| Heat Wave Boost | Separately priced premium | Free |
| Subscription | $0 base, premium unlocks per-controller | $0 |
| **Year-1 total (8-zone, w/ soil)** | ~$580 (no soil) | ~$650 (with full soil sensing) |
| **Year-3 total** | Same + premium per replacement | Same (no recurring) |
| Hidden cost: replace controller | All premium unlocks lost | Bring your config and Postgres/SQLite forward |
| Hidden cost: vendor changes mind | HomeKit dropped 2022 | n/a (you own the stack) |

---

## What you're trusting vs what you can verify

Every decision LocalSky makes is built from open inputs you can
inspect:

```
sensor.localsky_irrigation_verdict         ← the engine's call
sensor.localsky_irrigation_reason          ← the human-readable why
sensor.localsky_<zone>_soil_moisture       ← live calibrated % (LocalSky-owned)
sensor.localsky_<zone>_soil_ad             ← raw FDR AD (no math applied)
sensor.localsky_<zone>_soil_temp           ← per-zone soil temperature
sensor.localsky_<zone>_budget_seconds      ← planned run duration for tonight
sensor.localsky_<zone>_ec                  ← soil EC (µS/cm)
sensor.localsky_open_meteo_eto_today       ← reference ET₀
sensor.localsky_soil_temp_yard_min         ← min_max aggregate across zones
binary_sensor.localsky_warm_season_active  ← 7d rolling soil-temp gate
input_number.irrigation_<zone>_*           ← every threshold, tunable in HA
```

Every threshold has a dashboard slider. Every automation has a logbook
entry. Every decision can be traced from input to output in code that
ships with the repo.

Rachio's equivalent is **trust the model.** That's a fine answer for
most users; this stack is the answer for the people who want to see
the gauges.

---

## Watering-time math (the "why is this zone 30 min?" answer)

LocalSky computes each zone's planned run-time internally using FAO-56-based
bucket math:

```
seconds = ( |bucket_mm| / throughput_mm_per_hr ) × 3600 × multiplier
final   = min(seconds, maximum_duration)        # safety ceiling
```

Where each input is operator-tunable:

| Input | Meaning | Per-zone or global |
|---|---|---|
| `bucket_mm` | Soil-water deficit from LocalSky's FAO-56 bucket; negative when ET has depleted from field capacity. | Per zone |
| `throughput_mm_hr` | The head's precipitation rate. Fixed sprays land 20-40 mm/hr; rotors 6-14 mm/hr; drip 1-5 mm/hr. | Per zone (configured in LocalSky settings) |
| `multiplier` | Crop coefficient (Kc). St. Augustine warm-season turf runs Kc 1.08 in summer; mulched shrubs ~0.50. | Per zone |
| `maximum_duration` | A safety ceiling so a misconfigured throughput can't run a zone for 12 hours. Default is 3600 s (60 min). | Per zone |

Two zones with the same bucket deficit and Kc can have **very different** scheduled
run-times if their throughput differs. A rotor zone at 2.6 mm/hr will run ~3× longer
than a fixed-spray zone at 7.8 mm/hr to deliver the same depth of water; that's not
a bug, it's heads spreading water more slowly on purpose (better infiltration, less
runoff).

The dashboard's "Why this duration?" tile (right under the zone-status grid)
surfaces the live numbers per zone so you can see exactly what produced each
run-time. When the safety ceiling is binding (the calculated need exceeds
`maximum_duration`), that row turns amber and the tile flags the % short.

## Weekly water-budget mode (Phase H)

Opt-in per zone: when a zone has weekly budget mode enabled, the nightly
pipeline replaces FAO-56 daily-bucket flex for that zone with LocalSky's
weekly plan. The model:

```
weekly_budget_mm  = irrigation_<zone>_weekly_budget_in × 25.4
expected_rain_mm  = 7d_weighted_forecast × 25.4 × 0.7  (CAPTURE_EFFICIENCY)
needed_mm         = max(0, weekly_budget_mm − expected_rain_mm)
mm_per_session    = needed_mm / sessions_per_week
seconds_per_session = (mm_per_session / throughput) × 3600 × heat_mult ÷ 0.7

today_seconds = if (days_since_last_run >= 7/sessions)
                   AND (next_24h_rain_in < 0.10")
                   AND (needed_mm > 0)
                then seconds_per_session
                else 0
```

This is the answer to "saturation is not the goal, keep the lawn in a
healthy band for St. Augustine." Defaults match Florida extension service
guidance:

| Zone | Weekly budget | Sessions/wk | Notes |
|---|---|---|---|
| Turf (3 zones) | 1.0" | 2 | UF/IFAS warm-season guideline |
| Shrubs (mulched) | 0.5" | 1 | Mulch retains, slower dry-down |

All four numbers are operator-tunable per zone via LocalSky's zone
configuration. The "Weekly water budget" bento card on the irrigation
page shows each zone's plan + reason for today's recommendation (skipped
because rain incoming, skipped because ran 2 days ago, or running
because last run was 4 days ago and the next 24h is dry).

The pipeline order (nightly, inside LocalSky):

```
23:30:00  FAO-56 bucket calc → per-zone base duration
23:30:25  weekly_budget_override
            for zones with mode=on, overwrites base with weekly plan
23:30:30  verdict check (LocalSky skip rules + frost)
23:30:35  per-zone saturation skip (moisture ≥ threshold → zero duration)
23:30:40  heat-stress Kc bump (soil temp ≥ threshold → 120%)
sunrise -15m  OpenSprinkler run sequence finishes
```

The budget override at 23:30:25 lands before the verdict and skip checks
so the LocalSky verdict (skip/run/skip-on-frost) still has the final say
on whether the sequence runs, but per-zone budget values win over the
FAO-56 default when budget mode is on.

## Scheduling: finish before sunrise, not start before sunrise

LocalSky schedules zones to **finish 15 minutes before sunrise**. It
computes sunrise from lat/lon and works backward: start time =
`sunrise − 15 min − total_sequence_seconds`. When nightly per-zone
durations are updated, the start time auto-adjusts; no separate
automation is needed.

This matches arborist guidance for warm-season grasses: water finishing
before peak ET hits gives the lawn ~15 min to drain so canopy + soil
aren't standing wet when direct sun arrives. Reduces fungal pressure on
St. Augustine in humid climates.

---

## Roadmap

| Phase | Status | What it adds |
|---|---|---|
| A | done | Forecast intelligence engine (14 rules) |
| B | done | HA reads LocalSky verdict via REST sensor |
| C | done | Heat-stress Kc bump (120% duration on hot zones) |
| D | done | Ecowitt GW1100B + 4× WH52 polled natively by LocalSky; per-zone moisture calibration inside the engine; ecowitt2mqtt and HA-side template calibration retired |
| E | done | Soil-aware verdict (frost + yard-wide saturation) + per-zone duration overrides (saturation skip, heat-stress bump) computed in LocalSky + 7-day moisture projection + dashboard tiles |
| F | done (degraded) | LLM advisor (the LLM provider); offline when vLLM unreachable |
| PWA | done | Manifest, service worker, web push, mobile shell |
| **E.1** | done | "Why this duration?" math tile per zone + per-zone history visualization (summary tiles, daily bar chart, recent-runs list) + sequence anchored to finish before sunrise |
| **H** | done | **Weekly water-budget mode**: per-zone opt-in scheduler that allocates a weekly water target (default 1.0" turf / 0.5" shrubs, operator-tunable) across N sessions/week (default 2 turf / 1 shrubs), subtracts probability-weighted forecast rain × 0.7 capture, defers when next-24h rain ≥ 0.10" or the zone ran within the minimum interval (7 / sessions days), and emits today's recommended seconds. LocalSky applies the budget override at 23:30:25, before the verdict and skip checks, so the skip ladder still has final say. Per-zone budget mode toggles each zone between weekly-budget and FAO-56 daily flex independently. |
| **G (planned)** | | Per-zone forward verdict based on live soil moisture trajectory (moisture-driven `set_bucket` overrides in the FAO-56 model) |
| **I (planned)** | | Flow-meter integration (OpenSprinkler flow input → real-time leak detection + per-event volume) |
| **J (planned)** | | EC-driven flush-watering recommendation (informational → optional automation) |
| **K (planned)** | | Forecast ET vector (Open-Meteo per-day `et0_fao_evapotranspiration`) added to `DailyEntry` so the 7-day projection uses real future ET instead of today's value |

---

## Quick start (one-paragraph summary)

If you've got a Proxmox host, a few hundred dollars of hardware, and
patience, you can deploy LocalSky, set the gateway IPs and zone names
to match your install, wire OpenSprinkler to your solenoids, plant 4
WH52 probes, point them at a GW1100B, and have a verdict engine
running by tomorrow morning. Put the dashboard behind a reverse proxy
with TLS, and a VPN jump reaches it when you're away.

It will out-explain Rachio, out-observe Rachio, and outlast Rachio's
next pivot.

---

## License

Apache-2.0. See the `LICENSE` file at the repository root.

---

## Where this doc lives

Canonical: [localsky/docs/src/manual.md](https://github.com/silenthooligan/localsky/blob/main/docs/src/manual.md)

Update either, then `make-mirror` (TODO: tooling, currently a manual `cp`).
