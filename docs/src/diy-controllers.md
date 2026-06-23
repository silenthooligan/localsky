# DIY & ESP32 controllers

You do not need a boxed controller. If you have an ESP32 (or any board that can
switch a relay and talk to your network), LocalSky can drive it as a
first-class controller, same engine, same verdict, same dashboard. There are
two supported paths; pick the one that fits how your board already works.

| Path | Controller kind | Board needs | You get back |
|---|---|---|---|
| **HTTP / REST** | `http_generic` | a tiny HTTP server | full status, zone discovery, wizard "test connection" |
| **MQTT** | `mqtt_command` | an MQTT client | optional state, availability, and flow readback |

Both run entirely on your LAN, no cloud, no account. LocalSky always owns the
watering decision and the run duration; the board just opens and closes valves.

## Path 1: the HTTP contract (`http_generic`)

Implement these five endpoints on your board and LocalSky polls + commands it
exactly like a boxed controller. An optional bearer token is sent as
`Authorization: Bearer <token>` on every request when you set `bearer_token`.

| Method & path | Body | Purpose |
|---|---|---|
| `GET /status` | (none) | current state (see shape below) |
| `GET /zones` | (none) | zone list for the wizard's "scan zones" |
| `POST /zone/{id}/run` | `{"seconds": 600}` | start zone `{id}` for N seconds |
| `POST /zone/{id}/stop` | (none) | stop zone `{id}` |
| `POST /stop_all` | (none) | stop every zone |

Success is any HTTP `2xx`. Return `401` for a bad token. `{id}` is whatever
string your board uses (`"1"`, `"back_yard"`, ...); it's what you put in each
zone's **controller station** field.

`GET /status` response (only `zones` is required; everything else is optional):

```json
{
  "firmware": "1.0.0",
  "zones": [
    { "id": "1", "running": true, "remaining_s": 120 },
    { "id": "2", "running": false }
  ],
  "flow_gpm": 3.5,
  "rain": false
}
```

A board that includes `flow_gpm` is telling LocalSky a flow meter is wired in;
omit it if you have none. `GET /zones` returns `{"zones":[{"id":"1","name":"Back Yard"}]}`.

LocalSky config:

```toml
[[controllers]]
id = "diy"
default = true
kind = "http_generic"

[controllers.config]
base_url = "http://192.0.2.50"
# bearer_token = "optional-shared-secret"
poll_interval_s = 10
```

Set each zone's `controller_id = "diy"` and its `controller_station` to the
board's zone id. In the setup wizard, **Test connection** hits `GET /status`
and **Scan zones** imports `GET /zones`, just like OpenSprinkler.

**Contract notes for firmware authors:**

- `seconds` is a positive integer. LocalSky caps a single run at 7200s (2h)
  before sending, but your board should enforce its own max-runtime watchdog
  too, so a lost network or server can never leave a valve open. The reference
  sketch in `examples/http/` does this.
- `run`, `stop`, and `stop_all` are `POST`s. LocalSky sends a JSON body
  (`{"seconds":N}` for run, `{}` for stop / stop_all); accept and ignore an
  empty or `{}` body on stop.
- Security: set `bearer_token` and check it on the board (constant-time compare
  if you can). On an untrusted segment, terminate TLS in front of the board;
  LocalSky pins the resolved IP and follows no redirects.
- Forward-compatible: your board may include extra fields in `/status` (for
  example a `contract_version`); LocalSky ignores fields it doesn't recognize.

## Path 2: MQTT with state readback (`mqtt_command`)

If your board already speaks MQTT (ESPHome, Tasmota, Zigbee2MQTT, a bare
relay), use `mqtt_command`. LocalSky publishes an on/off payload per zone, and
LocalSky owns the shutoff timer. That alone is "fire-and-forget" control.

Add a `state_topic` per zone (and optionally a controller `availability_topic`
and `flow_topic`) and the board's reported state flows back into the
dashboard, the HA-native MQTT convention most firmware already publishes:

```toml
[[controllers]]
id = "diy"
default = true
kind = "mqtt_command"

[controllers.config]
broker_host = "192.0.2.10"
availability_topic = "localsky-irrig/status"     # "online" / "offline" (LWT)
flow_topic = "localsky-irrig/sensor/flow_gpm/state"

[controllers.config.zone_command_map.back_yard]
topic       = "localsky-irrig/switch/zone_1/command"   # LocalSky -> board
state_topic = "localsky-irrig/switch/zone_1/state"     # board -> LocalSky
# on_payload / off_payload default to "ON" / "OFF"
# state_on_payload defaults to on_payload; matching is case-insensitive
```

Without a `state_topic`, LocalSky reports running state from its own run log.
With it, the dashboard reflects what the board actually says.

### A note on state payloads (plain vs JSON)

State readback compares the **whole** `state_topic` payload against
`state_on_payload` (case-insensitive), and parses the **whole** `flow_topic`
payload as a number. So point these at topics that publish a *plain* value, not
JSON:

- **ESPHome** publishes plain `ON` / `OFF` on its state topic, this works out of the box (it's what the reference firmware in `examples/esphome/` uses).
- **Tasmota** publishes plain `ON` / `OFF` on `stat/<device>/POWER`, point `state_topic` there (not the JSON `tele/<device>/STATE`):
  ```toml
  [controllers.config.zone_command_map.back_yard]
  topic            = "cmnd/garage/POWER1"
  on_payload       = "1"
  off_payload      = "0"
  state_topic      = "stat/garage/POWER1"
  state_on_payload = "ON"
  ```
- **Zigbee2MQTT** command works (publish to `zigbee2mqtt/<name>/set`), but its
  *state* is JSON (`{"state":"ON"}` on `zigbee2mqtt/<name>`), which the
  whole-payload match can't read yet. Leave `state_topic` unset for Z2M relays,
  control still works; LocalSky just reports running state from its own run log.

Per-field JSON extraction for state topics is planned; until then use a plain
state topic where one exists.

## Reference firmware

Two copy-and-flash starting points ship in the repo, one per path:

- **MQTT path:** [`examples/esphome/`](https://github.com/silenthooligan/localsky/tree/main/examples/esphome),
  an ESP32 relay board wired over MQTT with per-zone state, LWT availability, and
  an optional flow sensor. ESPHome speaks MQTT natively, so this is the smoothest
  beginner on-ramp. Edit the GPIO pins, drop in your Wi-Fi/MQTT secrets, and `esphome run`.
- **HTTP path:** [`examples/http/`](https://github.com/silenthooligan/localsky/tree/main/examples/http),
  a single ESP32 Arduino sketch implementing the five-endpoint contract above
  (plus optional bearer auth). Flash it, point the `http_generic` controller at
  the board's IP, and Test connection + Scan zones work end to end. The README
  there includes a `curl` script to exercise the contract from your laptop.

Pick MQTT if you already run a broker or ESPHome; pick HTTP if you want a
self-contained board with no broker and the richest wizard experience.
