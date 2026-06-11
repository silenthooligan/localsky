# Configuration reference

LocalSky's configuration is a single TOML file at `/data/localsky.toml`. The first-run wizard writes it; the settings UI edits it; every `PUT /api/v1/config` validates and then writes it atomically (write to a temp file, rename). Schema lives in [src/config/schema.rs](../src/config/schema.rs).

This document is the field-by-field reference. The wizard ([docs/getting-started.md](getting-started.md)) is the conversational walkthrough; this is the lookup table.

## Top-level structure

```toml
schema_version = 1

[deployment]
[features]
[[sources]]
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

Supported `kind` values: `tempest_udp`, `tempest_ws`, `open_meteo`, `ecowitt_local`, `ecowitt_gw_poll`, `davis_wll`, `nws`, `openweather`, `pirate_weather`, `met_norway`, `ambient_weather`, `netatmo`, `yolink`, `lacrosse`, `tuya_cloud`, `ha_passthrough`, `mqtt`, `http_webhook`, `demo_replay`. See [src/config/schema.rs](../src/config/schema.rs) `SourceKind` enum for per-kind config fields.

Two kinds deserve a callout because they accept data from anything:

- `mqtt` subscribes to broker topics (Tasmota, ESPHome, Zigbee2MQTT, any raw publisher). Config: `broker_host`, `broker_port` (default 1883), optional `username`/`password`, and a `subscriptions` list mapping each topic to a weather field with optional scale/offset.
- `http_webhook` accepts JSON POSTs at a path you choose under `/ingest/` from anything that can speak HTTP (Arduino, a Pi script, a commercial gateway). Config: `path`, optional shared-secret `token` (sent as the `X-LocalSky-Token` header or `?token=` query parameter), and a `fields` mapping list.

`priority` matters when multiple sources report the same field. Convention: 100 = LAN station; 50 = forecast model; 10 = fallback.

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

Supported `kind` values: `opensprinkler_direct`, `ha_service_call`, `esphome_native`, `rachio`, `hydrawise`, `bhyve`, `rainbird`, `mqtt_command`, `dry_run`.

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
controller_id = "os_main"
controller_station = "1"         # 1-based for OS; entity_id for HA / ESPHome
soil_sensor_id = null            # optional; engine uses modeled bucket when absent
target_min_pct_soil = 30.0
saturation_pct_soil = 70.0
photo_url = null
```

`species` enum: `st_augustine`, `bermuda`, `zoysia`, `bahia`, `centipede`, `kentucky_bluegrass`, `tall_fescue`, `perennial_ryegrass`, `ornamental_shrubs`, `vegetable_garden`, `drip_xeriscape`, `other`. See [grass-species.md](grass-species.md).

`soil_texture` enum: `sand`, `loamy_sand`, `sandy_loam`, `loam`, `silt_loam`, `clay_loam`, `clay`. See [soil-textures.md](soil-textures.md).

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
capture_efficiency       = 0.70
session_rain_defer_in    = 0.10
soak_minutes             = 30
et0_method               = "auto"   # auto | penman_monteith | asce_simplified | hargreaves_samani | source_native

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

All values match v0.1 hardcoded constants. See [skip-rules.md](skip-rules.md) for what each one does.

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

- `override` (default): while an enabled override schedule applies to a zone that day, smart-irrigation dispatch for that zone is suppressed. The smart math still computes for visibility.
- `floor`: the schedule fires AND the smart engine may add more water if its deficit math justifies it. Useful for minimum-coverage requirements; can overwater if the scheduled run already covers the deficit.

Manual schedules respect watering restrictions exactly like smart runs do: a blocked dispatch is skipped with the reason logged to run history.

## `[auth]`

Authentication policy. Identity itself (accounts, sessions, `lsk_` API tokens) lives in the SQLite database, not in this file; this block only sets the policy. Full walkthrough: [Authentication](authentication.md).

```toml
[auth]
mode = "disabled"          # disabled (default) | required
session_ttl_days = 30      # rolling browser-session lifetime
trusted_networks = []      # CIDRs that skip auth while mode = "required", e.g. ["192.168.1.0/24"]
```

Configs without an `[auth]` block behave exactly as before (no login). With `mode = "required"`, static assets, `/api/v1/info`, and the `/ingest/*` receivers stay public; everything else needs a session or a Bearer token.

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

Off by default; nothing phones home. When enabled (restart required), LocalSky polls the GitHub releases API about once a day and serves the comparison at `GET /api/v1/updates`. Nothing self-updates; `docker pull` stays the upgrade mechanism. See [Upgrading LocalSky](upgrading.md#update-notifications).

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

A config rollback endpoint exists (`POST /api/v1/config/rollback?to=<version>`, snapshot list at `GET /api/v1/backup/snapshots`), but this beta does not record config snapshots on save yet, so it always returns 404. Keep backup bundles as your config history for now.

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
