# Configuration reference

LocalSky's configuration is a single TOML file at `/data/localsky.toml`. The first-run wizard writes it; the settings UI edits it; every `PUT /api/v1/config` validates and then writes it atomically (write to a temp file, rename). Schema lives in [src/config/schema.rs](../src/config/schema.rs).

This document is the field-by-field reference. The wizard ([docs/getting-started.md](getting-started.md)) is the conversational walkthrough; this is the lookup table.

## Top-level structure

```toml
schema_version = 1

[deployment]
[features]
[[sources]]
forecast_provider = "..."     # optional: pin the forecast provider (a source id)
[field_source_overrides]      # optional: per-reading single pin (reading -> source id)
[field_source_chains]         # optional: per-reading ordered backup chain
[[controllers]]
[zones.<slug>]
[llm]
[notifications]
[engine]
[[manual_schedules]]
[scripting]
[conditions]
[auth]
[network]
[updates]
[persistence]
[ui]
```

Every section except `deployment` is optional (zero-source / zero-controller configs are valid for first boots before the wizard has been completed). `schema_version` is required; a config whose `schema_version` is higher than the binary supports is refused at load (see [Upgrading LocalSky](upgrading.md#downgrading-and-rollback)).

## `[deployment]`

```toml
[deployment]
location = { lat = 52.52, lon = 13.40, elevation_m = 34 }   # your coordinates, decimal degrees
units = "metric"
timezone = "Europe/Berlin"                                  # your IANA timezone
display_name = "My Yard"
```

Or, for a US install:

```toml
[deployment]
location = { lat = 28.5, lon = -81.4, elevation_m = 30 }
units = "imperial"
timezone = "America/New_York"
display_name = "My Yard"
```

- `location.lat` / `location.lon`: required, decimal degrees
- `location.elevation_m`: optional, used by FAO-56 net-radiation
- `units`: `"metric"` or `"imperial"`. The setup wizard pre-selects this from your location; existing configs keep their value. Configs written without the field fall back to `"imperial"` for backward compatibility. Per-field overrides live in browser localStorage, not here
- `timezone`: optional IANA name. Null derives from lat/lon at boot
- `display_name`: surfaces in the MQTT discovery node_id (slugified) and the dashboard title

## `[features]`

```toml
[features]
demo_mode             = false
enable_mqtt_publish   = true
enable_advisor        = true
enable_push           = true
nerd_mode_default     = false
telemetry             = false
```

All defaults shown. `demo_mode` swaps every controller for DryRun and uses the synthetic DemoReplay source.

## `[[sources]]`

A list. Each entry has an `id`, `priority`, `enabled`, and a `kind` discriminator with per-kind `config` block.

```toml
[[sources]]
id = "tempest_lan"
priority = 100
enabled = true
kind = "tempest_udp"
[sources.config]
bind_addr = "0.0.0.0:50222"
hub_serial = null  # filter to a specific Tempest hub; null = accept any
```

Supported `kind` values: `tempest_udp`, `tempest_ws`, `open_meteo`, `ecowitt_local`, `ecowitt_gw_poll`, `davis_wll`, `nws`, `openweather`, `pirate_weather`, `met_norway`, `synoptic`, `noaa_mrms`, `ambient_weather`, `netatmo`, `yolink`, `lacrosse`, `tuya_cloud`, `ha_passthrough`, `mqtt`, `http_webhook`, `rest_poll`, `prometheus`, `influxdb`, `weatherkit`, `demo_replay`. See [src/config/schema.rs](../src/config/schema.rs) `SourceKind` enum for per-kind config fields.

New in 0.7.0: `synoptic` (Synoptic Data / MesoWest, a dense real-station observation network keyed by a free API token, current wind/pressure/temp/humidity like NWS but from a much denser mesonet) and `noaa_mrms` (NOAA Multi-Radar Multi-Sensor, keyless US-only gauge-corrected radar rain that sees the rain on your block, refreshed about every 2 minutes). Both emit only current scalars into the merge, not forecast snapshots.

Two kinds deserve a callout because they accept data from anything:

- `mqtt` subscribes to broker topics (Tasmota, ESPHome, Zigbee2MQTT, any raw publisher). Config: `broker_host`, `broker_port` (default 1883), optional `username`/`password`, and a `subscriptions` list mapping each topic to a weather field with optional scale/offset.
- `http_webhook` accepts JSON POSTs at a path you choose under `/ingest/` from anything that can speak HTTP (Arduino, a Pi script, a commercial gateway). Config: `path`, optional shared-secret `token` (sent as the `X-LocalSky-Token` header or `?token=` query parameter), and a `fields` mapping list.

`priority` matters when multiple sources report the same field. Convention: 100 = LAN station; 50 = forecast model; 10 = fallback. Cloud forecast sources added through the UI are re-ranked automatically to the researched region defaults (US: NWS 70 > Pirate 60 > OpenWeather/WeatherKit 55 > Open-Meteo 50; Europe/Nordics: Met.no 70; NWS is auto-disabled outside the US where it has no coverage).

### Default forecast failover chain

Every located install automatically carries its region's keyless forecast authority alongside Open-Meteo: **NWS + NOAA MRMS** in the US, **MET Norway** in Europe and the Nordics. They are seeded once, at their region rank (above the Open-Meteo 50 backstop), including on existing installs at upgrade, so a single provider outage cannot blank the forecast, the 7-day verdicts, or the rain-skip inputs. Deleting a seeded source is permanent; LocalSky records the id in `seeded_source_ids` and never re-adds it. When a lower-ranked provider is serving because the primary has gone quiet, the forecast header shows an amber "via [provider] · backup" link into the source status page. Open-Meteo itself also retries against two verified Open-Meteo mirror hosts before giving up, and data served that way is labeled "Open-Meteo (mirror)".

### Open-Meteo past days

The `open_meteo` source's `past_days` (default 3, clamped 1..=7) sets how many past days of daily model data ride along with the forecast. Those archived days are the model-archive rung of the observed-rain ladder: on installs without a gauge or radar day totals they are what the weekly water balance and the days-since-rain backstop read. The value is honored live since 0.7.17 (earlier builds always fetched 3 regardless of the setting); a config save applies on the next forecast refresh, no restart. A persisted `past_days = 1` is rewritten to 3 on load: 1 was the old never-honored template default, not an operator choice, and honoring it as written would shrink the archive on upgrade.

Related: the observed-rain ledger (`forecast_observations`) tags each day's total with its source (`gauge` | `radar` | `none`). A day whose rain owner is a model fill, stale, or absent records a `none` placeholder that never trains the bias model and never counts as a measured-dry day (a model's "rain today" is a whole-day forecast, not an observation); rows from before 0.7.17 read `legacy` and are treated as gauge-quality only on installs with a station source.

### Self-hosting Open-Meteo

Open-Meteo's engine is open source, and LocalSky can point at your own instance: set `endpoint` on the `open_meteo` source to its base URL. Your instance becomes the FIRST rung of the endpoint ladder; the hosted api.open-meteo.com and its mirrors stay behind it as automatic fallback, so a down self-hosted box degrades gracefully instead of blanking the forecast. Data served this way is labeled "Open-Meteo (self-hosted)".

```toml
[[sources]]
id = "open_meteo"
[sources.config]
endpoint = "http://192.0.2.10:8080"
```

What self-hosting actually does: the open-meteo container serves the same `/v1/forecast` API from model data it syncs to local disk. The data still comes from upstream (it downloads processed model runs from Open-Meteo's public AWS open-data bucket on a schedule), so you are not eliminating the data dependency; you are moving the failure domain. The API tier becomes yours (LAN latency, no rate limits, immune to api.open-meteo.com outages like 2026-07), while the sync runs in the background and a missed sync just means a gradually aging model run instead of an immediate outage.

Honest sizing: one regional high-resolution model with a limited history window is a few GB of disk and modest steady bandwidth; adding global models multiplies that quickly (tens of GB and up). A reasonable single-home setup syncs just the model that covers you (e.g. `ncep_hrrr_conus` plus `ncep_gfs013` fallback in the US, `dwd_icon_d2` + `dwd_icon` in Europe). See [github.com/open-meteo/open-meteo](https://github.com/open-meteo/open-meteo/blob/main/docs/getting-started.md) for the container and sync configuration. For most installs the hosted service plus LocalSky's built-in mirror ladder and regional failover chain is plenty; self-host when you want LAN-only forecasts, you hit rate limits, or you simply like running your own weather API.

## Per-field source selection

Three optional top-level keys let you steer which source drives each reading, on top of the per-source `priority`. All three are additive: leave them out and the plain priority merge applies unchanged.

`field_source_chains` maps a reading name (`temperature`, `humidity`, `wind_mph`, `rain_today_in`, `pressure_in_hg`, and so on) to an ordered list of source ids. The first source in the chain that is reporting fresh data owns the reading; if it goes quiet the next takes over. A reading with no chain falls back to the global per-source priority arbitration. Example:

```toml
[field_source_chains]
rain_today_in = ["ecowitt", "nws", "open_meteo"]
wind_mph = ["tempest", "open_meteo"]
```

`field_source_overrides` is the older single-pin form of the same idea: a reading name mapped to one source id. A pin is just a one-element chain, and the safety fallback is the same (the pin only wins while its source has a fresh value; otherwise the priority merge takes over). New configs should prefer `field_source_chains`; a bare pin still works.

```toml
[field_source_overrides]
pressure_in_hg = "davis"
```

`forecast_provider` pins which forecast-capable source drives the whole forecast pipeline (the daily/hourly arrays, ET0, and rain-tomorrow). The value is a source `id` for a forecast kind (`open_meteo`, `nws`, `met_norway`, `openweather`, `pirate_weather`, `weatherkit`). `null` (the default) keeps the priority arbitration, with Open-Meteo as the low-priority failover; naming a provider pins it to win regardless of ranking. An absent or disabled id is silently ignored, so a pin never blanks the forecast.

```toml
forecast_provider = "nws_forecast"
```

The Settings > Devices data-sources editor is the visual equivalent: drag rows or use arrow keys to reorder each reading's chain (toggling between Automatic smart defaults and Custom), and pick the forecast provider. Soil moisture is out of scope here; it is bound per zone via `soil_sensor_id`.

## `[[controllers]]`

```toml
[[controllers]]
id = "os_main"
default = true
enabled = true
kind = "opensprinkler_direct"
[controllers.config]
host = "192.0.2.10"
port = 80
password_md5 = "..."
poll_interval_s = 10
```

Exactly one controller should have `default = true`. The validator rejects PUTs that leave the system with zero defaults when any controller exists.

Supported `kind` values: `opensprinkler_direct`, `http_generic`, `mqtt_command`, `ha_service_call`, `rachio`, `hydrawise`, `bhyve`, `rainbird`, `dry_run`. (`esphome_native` is scaffolded but not yet built, so it is not offered in the UI; use `mqtt_command` or `http_generic` for ESPHome hardware.)

## Editable, migrating ids

Source and controller `id` fields are editable. Renaming one migrates every reference automatically: the `field_source_chains` picks, the `forecast_provider` pin, each zone's `soil_sensor_id`, and each zone's `controller_id`. A rename never leaves a dangling reference. Rename through the Settings UI or by editing the config and applying it.

## `[zones.<slug>]`

Keyed by zone slug. Each zone:

```toml
[zones.back_yard]
display_name = "Back Yard"
area_sqft = 1800
species = "st_augustine"
soil_texture = "sandy_loam"
slope_pct = 2.0
sun_exposure = "full"           # full | partial | shade
sprinkler_type = "rotor"         # rotor | spray | mp_rotator | drip | bubbler
precip_rate_mm_hr = 14.2         # measured via catch-cup; null = catalog default
precip_rate_source = "measured"  # measured | catalog
root_depth_mm = null             # null = species default
mad_pct_override = null          # null = species default
max_run_minutes = null           # null = 60; longest single run (whole minutes, 5..=360)
controller_id = "os_main"
controller_station = "1"         # the controller's own id for this zone
controller_zone_name = null      # the controller's name for it; a label only
soil_sensor_id = null            # optional; no probe just means no soil gate
target_min_pct_soil = 30.0
saturation_pct_soil = 70.0
weekly_budget_in = null          # null = 1.00 turf, 0.50 shrub/garden/bed
sessions_per_week = null         # 1..=7; null = 2 turf, 1 shrub/garden/bed
photo_url = null
```

`weekly_budget_in` and `sessions_per_week` are the two settings that size a
run; the zone editor (Settings, then Zones) carries them as **Weekly target**
and **Sessions per week**. The weekly balance settles `weekly_budget_in` (a gross weekly depth,
inches including rain) against observed rain, water already applied, and a
probability-weighted forecast credit, then splits the remainder across the
sessions still expected this week. Neither moves with the season: nothing
recomputes them from ET0 or the crop coefficient. When unset, both come
from a default by zone type, inferred from the zone's slug: 1.00 in over 2
sessions for turf, 0.50 in over 1 session for a slug containing `shrub`,
`garden` or `bed`. See [Weekly water budget](water-budget.md).

`sessions_per_week` accepts 1 through 7. Sessions space at
`floor(7 / sessions_per_week)` days, so above 7 that spacing works out to
less than a day and stops holding a zone that has already watered today. A
save outside the range is refused; a value already in a config file is read
as 7 rather than blocking the file from loading.

`species` enum: `st_augustine`, `bermuda`, `zoysia`, `bahia`, `centipede`, `kentucky_bluegrass`, `tall_fescue`, `perennial_ryegrass`, `ornamental_shrubs`, `vegetable_garden`, `drip_xeriscape`, `other`. See [grass-species.md](grass-species.md).

`soil_texture` enum: `sand`, `loamy_sand`, `sandy_loam`, `loam`, `silt_loam`, `clay_loam`, `clay`. See [soil-textures.md](soil-textures.md).

`max_run_minutes` caps a single dispatch for the zone (60 minutes when unset). It applies live on save. Raising it past 60 is confirmed in the UI at save time and sends a notification to subscribed devices; an active watering restriction's per-zone cap still wins via min().

The [tuning report](tuning-report.md)'s Apply action writes exactly these fields (`soil_texture`, `precip_rate_mm_hr` plus `precip_rate_source`, `root_depth_mm`, `weekly_budget_in`, `sessions_per_week`, `max_run_minutes`) through the same validated save path as the settings editor; `null` restores the documented default.

## `[llm]`

```toml
[llm]
provider = "auto"            # auto | ollama | llamacpp | openai_compat
timeout_s = 20
explanation_ttl_s = 300
anomaly_ttl_s = 3600

[llm.config]
# fields depend on provider
```

`auto` probes localhost in order: Ollama (11434), llama.cpp (8080), LM Studio (1234). First success wins. Override the probe list via `[llm.config] probe_order = ["http://..."]`.

`ollama` requires `{ base_url, model }`.
`llamacpp` requires `{ base_url }`; `model` optional.
`openai_compat` requires `{ base_url, model }`; `api_key` optional.

Omit the entire `[llm]` block to disable the advisor.

## `[notifications]`

```toml
[notifications]

[notifications.web_push]
vapid_public        = "..."
vapid_private_path  = "/keys/vapid-private.pem"
vapid_subject       = "mailto:you@example.com"

[notifications.mqtt]
host             = "broker.local"
port             = 1883
username         = null
password         = null
discovery_prefix = "homeassistant"
publish_enabled  = true
subscribe_enabled = false

[notifications.ntfy]
base_url   = "https://ntfy.sh"
topic      = "your-private-topic"
auth_token = null

[notifications.slack]
webhook_url = "https://hooks.slack.com/services/..."

[notifications.email]
smtp_host    = "smtp.example.com"
smtp_port    = 587
username     = "..."
password     = "..."
from_address = "localsky@example.com"
to_address   = "you@example.com"
starttls     = true
```

Each section is optional. Omit to disable that channel.

## `[engine]`

```toml
[engine]
capture_efficiency       = 0.70     # not read by the watering engine (see below)
session_rain_defer_in    = 0.10
soak_minutes             = 30
interleave_cycles        = true     # water other zones during soak pauses; turn off for well/low-recovery supplies
et0_method               = "auto"   # not read at all (see below)

[engine.skip_rules]
already_wet_in              = 0.05   # 1.3 mm
rain_now_in_hr              = 0.01   # 0.25 mm/hr
rain_next_4h_skip_in        = 0.10   # 2.5 mm
rain_3day_factor            = 1.5
heat_advisory_temp_f        = 95.0   # 35 C
heat_advisory_humidity_pct  = 60.0
heat_advisory_dry_days      = 2
wind_forecast_slack_mph     = 5.0    # 8 km/h
max_wind_mph                = 10.0   # 16 km/h
min_temp_f                  = 38.0   # 3.3 C
rain_skip_in                = 0.25   # 6.4 mm
frost_skip_soil_f           = 35.0   # 1.7 C
```

All values match v0.1 hardcoded constants, with two exceptions. See [skip-rules.md](skip-rules.md) for what each skip threshold does.

`capture_efficiency` and `et0_method` are accepted and validated so an existing config still loads, but neither reaches the watering decision. The soil projection and the zone math panel use a fixed 0.70 capture factor, and the ET0 path always runs the automatic method (Penman-Monteith when the inputs are there, otherwise ASCE-simplified, otherwise Hargreaves-Samani). Editing either changes nothing about when or how long a zone waters. `capture_efficiency` has one live reader left: the tuning report's measured-sprinkler-rate check, on a zone with a soil probe bound, divides the probe's rise by it, so it does move a recommendation you can then choose to apply.

`interleave_cycles` waters other zones during a zone's cycle-and-soak pauses instead of idling through them, shortening the morning sequence. Default on; turn it off on installs fed by a well or low-recovery pump, where the idle soak gaps double as supply recovery time (the setup wizard's water-supply question sets this for you). One valve still runs at a time and soaks are minimums that may stretch, never shrink; details in [irrigation-engine.md](irrigation-engine.md#cycle-interleaving). `interleave_cycles` and `soak_minutes` hot-reload with the rest of the watering policy: a change applies on the next scheduler tick (the next morning's plan), no restart needed. Both, plus the seasonal water-budget dial, are editable on the Engine settings page.

### Watering restrictions

Rules from your water authority, municipality, or homeowners' association live under `[engine]` as a list. Empty list (the default) means no restrictions are enforced. When multiple restrictions are active, the engine ANDs them all; the strictest wins.

Example: a Florida water-district rule, keyed to the daylight-saving switch:

```toml
[[engine.watering_restrictions]]
id = "sjrwmd_dst"
name = "SJRWMD daylight-saving rule"
enabled = true                      # default: true
effective = { kind = "dst_only" }   # all_year | dst_only | standard_only | date_range
allowed_weekdays_odd  = [3, 6]      # 0 = Sunday .. 6 = Saturday; empty = no parity gate
allowed_weekdays_even = [4, 0]
forbidden_hour_start = 10           # inclusive start of the no-watering window (local hour)
forbidden_hour_end   = 16           # exclusive end
max_minutes_per_zone = 60           # optional per-session cap; min of all active caps wins
```

Example: an Australian-style summer stage restriction (no watering 10:00-16:00, December 1 to March 31, even-numbered houses Tuesday/Saturday, odd-numbered Wednesday/Sunday):

```toml
[[engine.watering_restrictions]]
id = "summer_stage2"
name = "Stage 2 summer restrictions"
effective = { kind = "date_range", start_month = 12, start_day = 1, end_month = 3, end_day = 31 }
allowed_weekdays_even = [2, 6]
allowed_weekdays_odd  = [3, 0]
forbidden_hour_start = 10
forbidden_hour_end   = 16
```

`effective` decides when the rule applies: `all_year`, `dst_only`, `standard_only` (the complement), or `date_range` with `start_month`/`start_day`/`end_month`/`end_day` (wraparound ranges like Nov 15 to Feb 28 work). `dst_only` uses **US** daylight-saving dates (2nd Sunday of March to 1st Sunday of November); outside the US, use `date_range` for seasonal windows. The odd/even weekday gates only do anything when `[deployment]` sets `address_parity = "odd"` or `"even"`; the default `"not_applicable"` makes parity gates a no-op.

## `[[manual_schedules]]`

Fixed weekday-and-time schedules that coexist with the smart engine. Each schedule fires one zone:

```toml
[[manual_schedules]]
id = "back_yard_mwf"
name = "Back yard, Mon/Wed/Fri early"
zone_slug = "back_yard"        # must match a key under [zones]
enabled = true                 # default: true
weekdays = [1, 3, 5]           # 0 = Sunday .. 6 = Saturday; empty = never fires
start_hour = 5                 # local time, 0..23
start_minute = 30              # 0..59
duration_minutes = 20
mode = "override"              # override (default) | floor
```

- `override` (default): while an enabled override schedule applies to a zone that day, smart-irrigation dispatch for that zone is suppressed. The zone's smart plan for that day is zero and the detail panel reads "Scheduled 0 min"; the engine's other figures still show.
- `floor`: the schedule fires AND the smart engine may add more runs that week if the weekly water balance says the zone still needs water. The scheduled run's water counts against the weekly target like any other run, and it resets the session-spacing clock, so a schedule firing as often as the zone's own `sessions_per_week` cadence leaves the engine no day to add on. See [Manual schedules](schedules.md).

Manual schedules respect watering restrictions exactly like smart runs do: a blocked dispatch is skipped with the reason logged to run history.

## `[auth]`

Authentication policy. Identity itself (accounts, sessions, `lsk_` API tokens) lives in the SQLite database, not in this file; this block only sets the policy. Full walkthrough: [Authentication](authentication.md).

```toml
[auth]
mode = "disabled"          # disabled (default) | required
session_ttl_days = 30      # rolling browser-session lifetime
trusted_networks = []      # CIDRs that skip auth while mode = "required", e.g. ["10.0.0.0/24"]
trusted_proxies = []       # CIDRs of YOUR reverse proxies; makes X-Forwarded-For believable
# proxy_auth_header = "X-Auth-Request-Email"  # identity header an authenticating proxy stamps
# proxy_auth_allow = ["you@example.com"]      # allowed values (case-insensitive); empty = any non-empty value
```

Configs without an `[auth]` block behave exactly as before (no login). With `mode = "required"`, static assets, `/api/v1/info`, and the `/ingest/*` receivers stay public; everything else needs a session or a Bearer token.

`proxy_auth_header` names the identity header an authenticating reverse proxy (for example oauth2-proxy's `X-Auth-Request-Email`) stamps on requests it has already logged in. It is honored only when the request's direct peer is inside `trusted_proxies`, and it vouches the caller as an authenticated operator on the privileged config/backup routes in both auth modes. `proxy_auth_allow` optionally restricts which header values qualify. The proxy must strip or overwrite the header on client traffic. Full walkthrough: [Authentication](authentication.md#running-behind-an-authenticating-reverse-proxy).

## `[network]`

```toml
[network]
mdns_enabled = true   # default: true
```

Announces `_localsky._tcp` via mDNS so the Home Assistant integration and LAN clients can discover the instance. Announce-only; needs host networking under Docker to be visible beyond the container.

## `[updates]`

```toml
[updates]
check_enabled = false   # default: false
```

Off by default; nothing phones home. When enabled (restart required), LocalSky polls the project version manifest at `localsky.io/latest.json` about once a day (the running version travels in the User-Agent, nothing per-install) and serves the comparison at `GET /api/v1/updates`. Nothing self-updates; `docker pull` stays the upgrade mechanism. See [Upgrading LocalSky](upgrading.md#update-notifications).

## `[persistence]`

Local-history retention knobs for the SQLite database. Both default to sensible values; set them only if disk is tight.

```toml
[persistence]
retention_days      = 90   # default: 90
runs_retention_days = 0    # default: 0 (keep forever)
```

- `retention_days`: days of raw `sensor_history` readings to keep. Rows older than this are pruned opportunistically as new readings arrive. `0` disables pruning (keep everything forever).
- `runs_retention_days`: days of run / skip / decision history to keep. `0` (the default) keeps everything forever, which is what makes year-over-year trends in History possible. Set a cap only if disk is genuinely tight.

## `[ui]`

Server-side UI presentation defaults. These set the baseline; per-browser choices persist in each device's localStorage and win over them once a user changes something.

```toml
[ui.radar]
providers      = []                          # default: [] (Auto, region-smart set)
default_layers = ["precip", "nexrad", "..."] # overlays on for a browser with no saved preference
```

- `ui.radar.providers`: the radar tile providers offered in the layer menu, by catalog id (see `radar_catalog::providers()`). Empty (the default) means Auto: the region-smart recommended set for your station location. Non-empty means exactly this menu, in this order; any catalog provider is allowed anywhere, so you can deliberately switch on an out-of-region source to compare.
- `ui.radar.default_layers`: which overlays are enabled by default for a browser that has no stored preference yet. Accepts provider ids and feature ids from the radar catalog. Once a user toggles layers, their per-browser choice persists in localStorage and wins over this list.

## Env var interpolation

Anywhere a string field appears, you can interpolate environment variables via `${NAME}`. Useful for secrets:

```toml
[notifications.web_push]
vapid_public  = "${VAPID_PUBLIC}"
vapid_private_path = "${VAPID_PRIVATE_PATH}"
```

Escape with `$${literal}` if you need a literal `${...}` in the value.

## Validation

`PUT /api/v1/config` validates structurally (serde decode) and semantically:

- `schema_version` must equal or be less than what the binary supports
- Source ids and controller ids must be unique
- Exactly one controller can have `default = true` (zero is allowed only when `[[controllers]]` is empty)
- Each zone's `controller_id` must reference a configured controller
- `lat` in `[-90, 90]`, `lon` in `[-180, 180]`

Bad PUTs return 422 with the specific failure; on-disk file is untouched.

## Migrations

On boot, the migration runner replays any database migrations the file has not seen yet. Schema bumps live in [src/persistence/migrations/](../src/persistence/migrations/) as numbered SQL files, each applied in its own transaction and recorded in the `schema_migrations` table. The config file's own `schema_version` is currently `1`; older configs gain new fields via defaults, and a config newer than the binary is refused at load. Details: [Upgrading LocalSky](upgrading.md#what-happens-on-first-boot-after-an-upgrade).

LocalSky records a config snapshot on every save. Each successful write (a settings `PUT`, a raw-TOML save, or the wizard apply) first copies the previous on-disk `localsky.toml` to `<config_dir>/snapshots/<unix_ts>.toml`, keeping the newest 20 and pruning older ones. To list and restore them:

```bash
# List available snapshots (newest first).
curl http://localhost:8090/api/v1/config/snapshots
# -> {"snapshots":[{"ts":1765400000,"applied_at_epoch":1765400000,"schema_version":1,"note":null}, ...]}

# Roll back to one. The snapshot is validated before the swap, and the
# current config is snapshotted first so the rollback is itself reversible.
curl -X POST -H 'Content-Type: application/json' \
    -d '{"ts": 1765400000}' \
    http://localhost:8090/api/v1/config/rollback
```

`POST /api/v1/config/rollback` also accepts the legacy `?to=<ts>` query form. A rollback hot-reloads the restored config into the running engine just like a normal save. Snapshots cover config only; keep [backup bundles](backup-restore.md) for full config-plus-database history.

## Programmatic schema

The JSON Schema is published at runtime: `GET /api/v1/config/schema`. The settings UI uses it to generate form widgets and to validate input client-side. Schemars-derived, so it tracks the Rust struct definitions exactly.

## Backup + restore

Covered in full in [Backup, restore, and recovery](backup-restore.md). The short version: all persistent state is `/data/localsky.toml` plus `/data/irrigation.db`, and `GET /api/v1/backup` hands you both as one consistent `.tar.gz` (also available as the Download backup button under Settings -> Advanced).

## Optional analytics for public instances

LocalSky never sends telemetry. If you run a *public* instance (a demo,
a showcase) and want to measure visits with your own analytics tool,
set all of these and the app shell renders one script tag; leave them
unset (the default) and nothing is loaded or sent, ever:

```bash
LOCALSKY_ANALYTICS_SRC=/stats/u.js            # your tracker script URL
LOCALSKY_ANALYTICS_WEBSITE_ID=<your-site-id>  # data-website-id value
LOCALSKY_ANALYTICS_HOST_URL=                  # optional data-host-url
```
