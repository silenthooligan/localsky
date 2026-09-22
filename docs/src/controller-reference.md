# Controller configuration examples

Use Settings → Devices for normal setup. These examples document adapter fields for configuration tooling. Substitute your own identifiers and credentials; verify supported hardware before enabling watering.


## LocalSky integration

```toml
[[controllers]]
id = "os_main"
default = true
enabled = true
kind = "opensprinkler_direct"
[controllers.config]
host = "192.0.2.10"
port = 80
password_md5 = "<md5 of plaintext password>"
poll_interval_s = 10
```

## Home Assistant service call (legacy continuity)

```toml
[[controllers]]
id = "ha_main"
default = true
enabled = true
kind = "ha_service_call"
[controllers.config]
base_url = "http://homeassistant.local:8123"
bearer_token = "${HA_LONG_LIVED_TOKEN}"
start_service = "script.os_zone_toggle"
stop_service = "opensprinkler.stop"
[controllers.config.zone_entity_map]
back_yard = "switch.back_yard_zone"
front_yard = "switch.front_yard_zone"
```

## Rachio Gen 2/3

```toml
[[controllers]]
id = "rachio_main"
default = true
enabled = true
kind = "rachio"
[controllers.config]
api_token = "${RACHIO_API_TOKEN}"
device_id = "..."        # Rachio device id; the Test button can discover it
poll_interval_s = 120    # optional; 60..=3600, default 120
[controllers.config.zone_uuid_map]
back_yard = "..."        # Rachio zone UUID; filled by Scan zones
```

## Hunter Hydrawise

```toml
[[controllers]]
id = "hydrawise_main"
default = true
enabled = true
kind = "hydrawise"
[controllers.config]
api_key = "${HYDRAWISE_API_KEY}"
controller_id = 0            # controller serial / id
[controllers.config.zone_relay_map]
back_yard = 1                # Hydrawise relay_id
```

## Orbit B-hyve

```toml
[[controllers]]
id = "bhyve_main"
default = true
enabled = true
kind = "bhyve"
[controllers.config]
email = "${BHYVE_EMAIL}"
password = "${BHYVE_PASSWORD}"
device_id = "..."            # from the account's /v1/devices list
[controllers.config.zone_station_map]
back_yard = 1                # B-hyve station number (1-based)
```

## Rain Bird

```toml
[[controllers]]
id = "rainbird_main"
default = true
enabled = true
kind = "rainbird"
[controllers.config]
email = "${RAINBIRD_EMAIL}"
password = "${RAINBIRD_PASSWORD}"
controller_id = "..."                       # from the account's controller list
base_url = "https://rdz-rest.rainbird.com"  # default; override only if the host changes
[controllers.config.zone_station_map]
back_yard = 1                               # Rain Bird station number (1-based)
```

## DryRun (no-op)

```toml
[[controllers]]
id = "dry"
default = true
kind = "dry_run"
[controllers.config]
simulate_runs = true   # write fake completed runs into history for dashboard population
```

## Binding precedence

A zone’s `controller_station` is the explicit binding. The controller’s legacy zone map supplies the documented fallback where no usable explicit binding exists. Do not infer a station from a similar name.

[Controller setup](controllers.md) · [Configuration reference](configuration.md)
