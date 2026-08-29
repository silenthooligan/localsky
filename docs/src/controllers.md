# Irrigation Controllers

LocalSky's `IrrigationController` port abstracts the act of firing valves. The same engine output (zone X for Y seconds) dispatches to any supported controller. Pick the one that fits your hardware.

## Supported controllers

| Controller | Path | Cloud required? | Scan zones | Hardware cost (US$) | Status |
|---|---|---|---|---|---|
| **OpenSprinkler** (boxed) | Direct HTTP on LAN | No | Yes | 130-180 | Shipped |
| **OpenSprinkler Pi** | Direct HTTP on LAN | No | Yes | ~80 (Pi) + relay board | Shipped |
| **[DIY / ESP32](diy-controllers.md)** (HTTP) | Direct HTTP on LAN | No | Yes | 5-40 ESP32 + valves | Shipped |
| **[DIY / ESP32](diy-controllers.md)** (MQTT) | MQTT (ESPHome, Tasmota, Z2M) | No | No (zone map by hand) | 5-40 ESP32 + valves | Shipped |
| **Home Assistant service call** | HA REST | No (HA local) | No (entity map by hand) | Whatever HA drives | Shipped |
| **Rachio** Gen 2/3 | Rachio cloud API | Yes | Yes | 130-250 | Shipped |
| **Hunter Hydrawise** | Hydrawise cloud API | Yes | No (relay id by hand) | 130-300 | Shipped |
| **Orbit B-hyve** | B-hyve cloud API | Yes | No (station number by hand) | 80-150 | Shipped |
| **Rain Bird** | Rain Bird cloud API | Yes | No (station number by hand) | 100-300 | Shipped |
| **DryRun** | No-op | No | Yes (3 sample zones) | None | Shipped |

Prices are US retail; availability and cost vary by region. Rachio, B-hyve, Hydrawise, and Rain Bird are sold mostly through North American retail; OpenSprinkler and ESP32 hardware ship worldwide, which makes them the natural picks outside North America too. All four cloud controllers are offered in the controller picker as of 0.7.

The **Scan zones** column says whether LocalSky can ask the controller for its own zone list. Where it can, you never type an id: the zone editor's **Controller station** field lists the controller's zones by the controller's name for each and stores its id, and a scan in the controller editor ends in a table where you say which of your zones each one fires. Where it cannot, you enter the id yourself, which is a relay id, a station number, or a Home Assistant entity_id depending on the controller.

Only four kinds can be asked: OpenSprinkler, Rachio, the DIY HTTP board, and the simulated controller. Hydrawise, B-hyve and Rain Bird have no zone-discovery endpoint, and Home Assistant, MQTT and ESPHome are not probed at all.

> Rolling your own with an ESP32 or another relay board? See [DIY & ESP32 controllers](diy-controllers.md) for the two supported paths (a small HTTP contract, or MQTT) with copy-and-flash reference firmware, beginner to advanced.

## OpenSprinkler (the ideal)

OpenSprinkler is LocalSky's reference controller for one reason: it speaks a documented HTTP API on the LAN with no cloud dependency. No telemetry to a vendor, no account required, no app subscription. The hardware is open-source (schematic + firmware) and the protocol has been stable for years.

### Hardware options

- **OpenSprinkler 3.x boxed** (24 stations, US$180), the canonical choice for an outdoor enclosure.
- **OpenSprinkler 3.x bare PCB** (US$130), DIY mount.
- **OpenSprinkler Pi**: a Pi HAT + relay board. Cheaper if you have a spare Pi.
- **OpenSprinkler OSPi-Plus**: newer board, more I/O.

Firmware 2.1.9 or newer is required.

### LocalSky integration

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

The first-run wizard or `/settings/controllers` does this for you. The `password_md5` is computed client-side at config time; the plaintext never leaves your browser.

### What LocalSky uses

- `GET /jc` for status (zone states, water level %, rain sensor, firmware version)
- `GET /cm` for manual station start/stop
- `GET /cv` for stop-all
- `GET /jl` for run-history backfill

LocalSky never touches the program/schedule storage on the OS device. Schedules live in LocalSky's engine; the controller is just a valve-firing API.

### Where OpenSprinkler shines

- Direct LAN control means no cloud lag, no service outages, no app required
- Detailed status JSON (water level, rain sensor, flow meter, per-station runtime)
- Native run-history endpoint enables LocalSky's restart-recovery + audit
- Active open-source community

### Where OpenSprinkler falls short

- HTTP only (no TLS by default; put it behind a reverse proxy if you must expose it)
- MD5 password (legacy crypto; not a deal-breaker on a LAN but not great)
- 24-station boxed limit (chain a slave for more)

## Home Assistant service call (legacy continuity)

If you already drive irrigation through Home Assistant, OpenSprinkler integration, Irrigation Unlimited, Rachio HACS, ESPHome sprinkler, LocalSky can dispatch through HA service calls without replumbing anything.

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

LocalSky's payload to HA is normalized: `{"entity_id": "<from map>", "duration_s": <seconds>, "minutes": <float>}`. Your HA-side script or service template picks the field it understands.

Use cases:
- Migrating from an HA-driven irrigation setup without re-wiring schedules
- Using a controller LocalSky has no native adapter for yet, when a Home Assistant integration for it already exists
- Wanting irrigation runs to flow through HA's automation engine for additional logic

## ESP32 / DIY (ESPHome, Tasmota, custom)

An ESP32 with a relay board is a smart irrigation controller for ~$15-40 in parts. LocalSky drives it two ways, both first-class and both covered in detail on the [DIY & ESP32 controllers](diy-controllers.md) page:

- **MQTT** (`mqtt_command`): for boards that already speak MQTT (ESPHome, Tasmota, Zigbee2MQTT, a bare relay). Optional state/availability/flow readback. The bundled ESPHome reference firmware uses this path.
- **HTTP** (`http_generic`): for a self-contained board with no broker. LocalSky polls a small REST contract, so Test connection + Scan zones work in the wizard. A copy-and-flash ESP32 Arduino sketch ships in `examples/http/`.

> A native ESPHome protobuf adapter (`esphome_native`) is scaffolded but not yet built, so it is not offered in the controller picker. Use MQTT or HTTP above for ESPHome hardware today.

## Cloud controllers (Rachio, Hydrawise, B-hyve, Rain Bird)

Four vendor controllers are driven natively through their own clouds; all ship in 0.7 and appear in the controller picker under "Cloud account". Each authenticates with your vendor account (an API token, or account email + password) and maps LocalSky zone slugs to that controller's zones/stations. Put secrets in env vars and interpolate them with `${...}` so they never sit in the config in cleartext.

### How a zone binds

A zone's binding is the **Controller station** field on the zone itself: the controller's own id for the valve that zone fires. Everything else is a way of filling it in.

- **Zone editor** (Settings, then Zones): pick the controller's zone from the list where the controller can be asked, or enter its id where it cannot. This is the field the binding lives in.
- **Controller editor** (Settings, then Devices): **Scan zones** lists what the controller reports and gives each one a dropdown of your zones. Choosing there writes the same field on each zone you pick. It binds existing zones and never creates one.
- **Setup wizard**: Scan zones and import. Imported zones are created with the controller's id already in **Controller station**.

Names on the two sides are free to differ. A Rachio zone called "Front Lawn" can fire a LocalSky zone called "Front Yard", because what is stored is the id, not the name. LocalSky keeps the controller's name alongside it as a label so the zone card can say which valve it is wired to, but nothing dispatches on that label.

### The controller's own zone map, and why it is still there

Each cloud kind, Home Assistant and MQTT also hold a `zone_*_map` inside the controller's config (`zone_uuid_map`, `zone_relay_map`, `zone_station_map`, `zone_entity_map`, `zone_command_map`). It is keyed by zone slug and is still read at dispatch as a fallback, so a config written before the picker existed keeps working untouched.

When both exist for the same zone, the zone's own **Controller station** wins, provided its value is an id that controller kind understands. A value the kind's parser rejects is ignored with a log line naming the zone, and whatever the map held stays in place. **The parsers differ, and it matters:** Rachio accepts only a zone UUID, so a station number there is ignored and the mapped UUID survives. Hydrawise, B-hyve and Rain Bird address zones by a NUMBER, so a number is accepted and replaces what their map held. Home Assistant accepts an entity_id and nothing else, so a leftover station number is ignored rather than sent to HA as a valve name.

On startup, LocalSky copies a map entry into any zone of that controller whose **Controller station** is empty and whose slug the map covers. That moves an existing binding onto the zone where you can see and change it, and it never overwrites a station you set. The map is left in place.

**MQTT is the exception.** An MQTT zone's binding is a command topic plus its payloads, which is more than a single id can carry, so MQTT zones bind in `zone_command_map` only and **Controller station** is ignored for them.

You do **not** need Home Assistant for any of the cloud controllers; the native adapter talks to the vendor cloud directly. (Driving one through HA with `ha_service_call` is still an option if you already do that.)

### When a zone will not start

The error text now names which of these it is. Read it: it is the fastest answer, and it is what to paste into a bug report.

**A station number on a Rachio zone.** Rachio addresses zones by UUID, never by station number, so a number in **Controller station** is not an id it recognizes. It is now ignored and the scanned UUID is what dispatches, where it used to replace the scanned UUID silently and Rachio rejected every start. (On Hydrawise, B-hyve and Rain Bird a number IS the vendor's id and still overrides the scanned map, so clear it there unless it is the vendor's own number.)

**The zone is not bound to anything.** Either its **Controller station** is empty, or it holds an id this controller cannot use, and the controller's zone map has no entry under its slug either, so there is nothing to fire. The zone card marks this one **Unbound** and the config check says so before you try to run it.

The second case is easy to reach and hard to spot: move a zone from one brand of controller to another and its old id stays in the field. A Rachio zone UUID means nothing to a Hydrawise, which addresses zones by relay number, and a station number means nothing to a Rachio. The check names the value and what that controller expects. (Switching a zone's controller in the editor now clears the field for you; this catches configs written before that.)

The fix is one step: open the zone in Settings, then Zones, and set **Controller station**. Where the controller can be asked, the field lists its zones by name; where it cannot, enter the id (a relay id, a station number, or an entity_id). The error body lists what that controller can currently fire, so you can see exactly which of your zones is missing from it.

**Do not rename the zone to make the two match.** That was the old advice and it is wrong: a zone's slug is permanent, because its run history, its soil sensor channel, its nine Home Assistant entity ids, its retained MQTT discovery topics and its `/zones/<slug>` links are all stored under it. Renaming trades a binding you can fix in one field for orphaned history you cannot. The slug field is read-only in the editor for that reason, and the raw TOML editor refuses a zone-key rename.

Clearing **Controller station** does not fix a mismatch either: an empty station is skipped, which leaves the zone exactly as unbound as before.

**The daily API budget is spent.** Cloud controllers cap how many requests an account may make per day, and LocalSky's status polling shares that budget with the vendor's own app and anything else on the same token. A rate-limited controller now says so, with the allowance its last response reported. Raising `poll_interval_s` spends fewer requests.

Whichever it is, the reason appears in three places: in the message on the button you pressed (the Zones page Run, or Test run on the zone card in Settings), in the container log at the moment of the attempt, and in the response body of `POST /api/v1/irrigation/action` if you are looking at the browser's network tab. Include that text in a bug report; it carries the vendor's own status and message.

A separate thing that is **not** a failure: after a run is accepted, the zone can keep reading idle for a while. A cloud controller reports its state on a throttle (Rachio: 60s at the fastest, 120s by default), which is longer than the window the Zones page waits before saying something. When that happens the message says the controller accepted the change and how often it reports state, rather than implying the run failed.

### Rachio Gen 2/3

Uses a Rachio API token (Rachio app: Account Settings, "Get API key"). The device id can be left empty: the Test button resolves your account's first device and offers to fill it in. Zones bind by picking them in the zone editor or in the controller editor's bind table, so the TOML below is the end state, not something to type. The `zone_uuid_map` shown is the fallback described above; a zone bound in the editor carries its UUID in `controller_station` instead.

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

Facts to know about the Rachio path:

- **Rate limit.** Rachio's cloud allows roughly 1700 API requests per day per token. LocalSky polls live status at most every `poll_interval_s` seconds (default 120, floor 60; each poll is two API calls) and serves the cached snapshot between polls, which fits the budget with room for dispatch. The controller Test result shows the cloud's remaining daily request count when Rachio reports it.
- **Stopping one zone stops the device.** The public Rachio API has no per-zone stop; the only stop operation halts all watering on the device. LocalSky says so whenever it happens (the Stop button's confirmation, the logs), and its bookkeeping matches: every run the stop ended is recorded at its real length. The shutoff backstop also verifies with the cloud before enforcing a deadline on Rachio: a zone that already closed on its own timer is released without any stop, and the widened enforcement grace absorbs normal cloud latency, so a multi-zone morning is never cut short by the previous zone's deadline. Rachio's own on-device timer remains the primary shutoff: a started zone always closes itself at the requested duration.
- **Run length cap.** A single zone start is capped at 3 hours by the API; longer requests are clamped.
- **Live state.** Running state and remaining time come from the cloud's current-schedule endpoint, so runs LocalSky starts are observed and recorded in History like any other controller's.
- **Deferred: webhooks and history backfill.** Rachio supports push webhooks (exact start/stop events, rain-sensor state) but they need a URL the cloud can reach, which most LocalSky installs do not expose; polling is the default. Event-history backfill rides the same future webhook work, so the adapter reports no flow meter, no rain sensor, and no history query today rather than pretending.

### Hunter Hydrawise

Uses a Hydrawise API key. `controller_id` scopes commands when your account has more than one controller; map each zone slug to its Hydrawise relay id.

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

### Orbit B-hyve

Signs in with your B-hyve account email and password. `device_id` (from the account's device list) scopes commands; map each zone slug to its B-hyve station number (1-based).

Like Rachio, B-hyve's cloud has no per-station stop: stopping one zone halts all watering on the device, and LocalSky says so when it happens.

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

### Rain Bird

Signs in with your Rain Bird account email and password. `controller_id` comes from your account's controller list; map each zone slug to its Rain Bird station number (1-based). `base_url` defaults to the production endpoint and only needs setting if Rain Bird rotates hosts.

Like Rachio, the Rain Bird cloud has no per-station stop: stopping one zone halts the whole controller, and LocalSky says so when it happens.

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

For testing, demos, and CI. DryRun records intent (with optional simulated runs that write to the SQLite history) but never fires anything.

```toml
[[controllers]]
id = "dry"
default = true
kind = "dry_run"
[controllers.config]
simulate_runs = true   # write fake completed runs into history for dashboard population
```

`LOCALSKY_DEMO=1` env auto-creates this controller.

## Multi-controller setups

The `ControllerRegistry` supports any number of controllers. Use cases:

- **Primary + backup**: production OS device + DryRun for safety during config changes
- **Geographic split**: front-yard OS + back-yard ESPHome on different LAN subnets
- **HA-bridged + direct**: legacy HA-driven zones + new direct-controlled zones in the same deployment

Per-zone `controller_id` in `ZoneConfig` picks which controller fires that zone. Exactly one controller must have `default = true`; new zones inherit that.

## Editing and renaming controllers

Controller IDs are editable, even after zones are linked. When you rename a controller (in Settings > Devices), every zone that points to it migrates to the new id automatically, so there are no dangling references and no manual fixup. The default controller flag migrates the same way: change which controller is the default and new unassigned zones inherit it.

## Adding a new controller

Open `src/controllers/<name>.rs`, implement the `IrrigationController` trait:

```rust
#[async_trait]
impl IrrigationController for MyController {
    fn id(&self) -> &str { &self.id }
    fn supports(&self) -> ControllerCaps { ... }
    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> { ... }
    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> { ... }
    async fn stop_all(&self) -> ControllerResult<()> { ... }
    async fn status(&self) -> ControllerResult<ControllerStatus> { ... }
    async fn run_history(&self, since_epoch: i64) -> ControllerResult<Vec<RunRecord>> { ... }
}
```

Add a variant to `ControllerKind` in `src/config/schema.rs`. Wire construction in `src/runtime.rs::build_controllers`. ~100-200 lines total.

See `src/controllers/dry_run.rs` for the minimal example, `src/controllers/opensprinkler_direct.rs` for a full HTTP-API integration.
