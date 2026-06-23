# ESPHome reference firmware for DIY irrigation

`localsky-irrigation.yaml` turns an ESP32 relay board into a first-class
LocalSky controller. LocalSky handles all the watering decisions, run
durations, and shutoff; the board just switches relays and reports state.

## Flash it

1. Install ESPHome (`pip install esphome` or the Home Assistant add-on).
2. Create `secrets.yaml` next to the YAML:
   ```yaml
   wifi_ssid: "YourWiFi"
   wifi_password: "..."
   mqtt_broker: "192.0.2.10"
   mqtt_username: "localsky"
   mqtt_password: "..."
   ```
3. Edit the `substitutions:` block (relay GPIOs, flow pin + pulses-per-gallon).
4. `esphome run localsky-irrigation.yaml`

## Wire it into LocalSky

Add this controller to LocalSky (Settings > Controllers, or `localsky.toml`).
Topics are derived from the board `name` (`localsky-irrig`) and each switch's
name (`Zone 1` -> `zone_1`):

```toml
[[controllers]]
id = "diy"
default = true
kind = "mqtt_command"

[controllers.config]
broker_host = "192.0.2.10"
broker_port = 1883
username = "localsky"
password = "..."
availability_topic = "localsky-irrig/status"
flow_topic = "localsky-irrig/sensor/flow_gpm/state"

[controllers.config.zone_command_map.back_yard]
topic       = "localsky-irrig/switch/zone_1/command"
state_topic = "localsky-irrig/switch/zone_1/state"

[controllers.config.zone_command_map.front_yard]
topic       = "localsky-irrig/switch/zone_2/command"
state_topic = "localsky-irrig/switch/zone_2/state"
```

Then set each zone's `controller_id = "diy"`. With `state_topic` set, LocalSky
reads the board's real on/off state back; `availability_topic` drives the
online/offline indicator; `flow_topic` feeds live gallons/min into the snapshot.

## Prefer plain HTTP?

If you'd rather run your own firmware (Arduino/ESP-IDF) and have LocalSky poll
it, implement the five-endpoint HTTP contract and use the `http_generic`
controller kind instead. See the DIY & ESP32 controllers documentation page.
