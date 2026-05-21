# Configuration Reference

LocalSky's configuration is a single TOML file at `/data/localsky.toml`. The first-run wizard writes it; the settings UI edits it; every PUT to `/api/config` snapshots the previous version before atomically writing the new one. Schema lives in [src/config/schema.rs](../src/config/schema.rs).

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
```

Every section except `deployment` is optional (zero-source / zero-controller configs are valid for first boots before the wizard has been completed). `schema_version` is required; the migration runner uses it to apply schema changes between releases.

## `[deployment]`

```toml
[deployment]
location = { lat = 28.5, lon = -81.4, elevation_m = 30 }
units = "imperial"
timezone = "America/New_York"
display_name = "My Yard"
```

- `location.lat` / `location.lon` - required, decimal degrees
- `location.elevation_m` - optional, used by FAO-56 net-radiation
- `units` - `"imperial"` (default) or `"metric"`. Per-field overrides live in browser localStorage, not here
- `timezone` - optional IANA name. Null derives from lat/lon at boot
- `display_name` - surfaces in the MQTT discovery node_id (slugified) and the dashboard title

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

Supported `kind` values: `tempest_udp`, `tempest_ws`, `open_meteo`, `ecowitt_local`, `nws`, `openweather`, `pirate_weather`, `met_norway`, `ambient_weather`, `ha_passthrough`, `demo_replay`. See [src/config/schema.rs](../src/config/schema.rs) `SourceKind` enum for per-kind config fields.

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

Supported `kind` values: `opensprinkler_direct`, `ha_service_call`, `esphome_native`, `rachio`, `dry_run`.

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
already_wet_in              = 0.05
rain_now_in_hr              = 0.01
rain_next_4h_skip_in        = 0.10
rain_3day_factor            = 1.5
heat_advisory_temp_f        = 95.0
heat_advisory_humidity_pct  = 60.0
heat_advisory_dry_days      = 2
wind_forecast_slack_mph     = 5.0
max_wind_mph                = 10.0
min_temp_f                  = 38.0
rain_skip_in                = 0.25
frost_skip_soil_f           = 35.0
```

All values match v0.1 hardcoded constants. See [skip-rules.md](skip-rules.md) for what each one does.

## Env var interpolation

Anywhere a string field appears, you can interpolate environment variables via `${NAME}`. Useful for secrets:

```toml
[notifications.web_push]
vapid_public  = "${VAPID_PUBLIC}"
vapid_private_path = "${VAPID_PRIVATE_PATH}"
```

Escape with `$${literal}` if you need a literal `${...}` in the value.

## Validation

`/api/config` validates structurally (serde decode) and semantically:

- `schema_version` must equal or be less than what the binary supports
- Source ids and controller ids must be unique
- Exactly one controller can have `default = true` (zero is allowed only when `[[controllers]]` is empty)
- Each zone's `controller_id` must reference a configured controller
- `lat` in `[-90, 90]`, `lon` in `[-180, 180]`

Bad PUTs return 422 with the specific failure; on-disk file is untouched.

## Migration + rollback

On boot, the runner replays any unapplied migrations from `schema_migrations`. Schema bumps live in [src/persistence/migrations/](../src/persistence/migrations/) as numbered SQL files.

Every PUT snapshots the previous config into `config_snapshots` (M0002) with retention of 20 versions. Roll back via:

```
POST /api/config/rollback?to=<version>
```

Always reachable, even when the engine is in a `degraded` state (no valid controller, no enabled sources). The rollback endpoint never validates the target; if you saved a broken config, you can restore it. Use the safety net responsibly.

## Programmatic schema

The JSON Schema is published at runtime: `GET /api/config/schema`. The settings UI uses it to generate form widgets and to validate input client-side. Schemars-derived, so it tracks the Rust struct definitions exactly.
