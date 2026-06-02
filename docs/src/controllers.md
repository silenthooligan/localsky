# Irrigation Controllers

LocalSky's `IrrigationController` port abstracts the act of firing valves. The same engine output (zone X for Y seconds) dispatches to any supported controller. Pick the one that fits your hardware.

## Supported controllers

| Controller | Path | Cloud required? | Hardware cost | Status in v0.1 |
|---|---|---|---|---|
| **OpenSprinkler** (boxed) | Direct HTTP on LAN | No | $130-180 | Tested |
| **OpenSprinkler Pi** | Direct HTTP on LAN | No | ~$80 (Pi) + relay board | Tested |
| **Home Assistant service call** | HA REST | No (HA local) | Whatever HA drives | Tested |
| **ESPHome sprinkler** | ESPHome native API | No | $5-40 ESP32 + valves | Community / planned |
| **Rachio** Gen 2/3 | Rachio cloud API | Yes | $130-250 | Planned |
| **Hunter Hydrawise** | Cloud API | Yes | $130-300 | Community / planned |
| **B-hyve** | Cloud API | Yes | $80-150 | Community / planned |
| **DryRun** | No-op | No | None | Tested |

## OpenSprinkler (the ideal)

OpenSprinkler is LocalSky's reference controller for one reason: it speaks a documented HTTP API on the LAN with no cloud dependency. No telemetry to a vendor, no account required, no app subscription. The hardware is open-source (schematic + firmware) and the protocol has been stable for years.

### Hardware options

- **OpenSprinkler 3.x boxed** (24 stations, $180), the canonical choice for an outdoor enclosure.
- **OpenSprinkler 3.x bare PCB** ($130), DIY mount.
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
- Using a controller LocalSky doesn't have a native adapter for (Hunter, B-hyve via HA), but the HA integration does
- Wanting irrigation runs to flow through HA's automation engine for additional logic

## ESPHome sprinkler (community / planned)

ESPHome's `sprinkler` component turns an ESP32 with a relay board into a smart irrigation controller for ~$15-40 total parts cost. The native API (protobuf over TCP) is documented.

```toml
[[controllers]]
id = "esp_irrigation"
default = true
kind = "esphome_native"
[controllers.config]
host = "192.0.2.20"
port = 6053
password = "${ESP_API_PASSWORD}"
[controllers.config.zone_entity_map]
back_yard = "switch.back_yard_valve"
front_yard = "switch.front_yard_valve"
```

Status: trait scaffolded, native adapter implementation deferred. Until then, run the ESPHome device under HA and use the `ha_service_call` controller. Track progress at the relevant GitHub issue.

## Rachio Gen 2/3 (planned)

Rachio is cloud-tethered but well-documented. The v1 API takes a bearer token and exposes zone start, zone stop, schedule query.

```toml
[[controllers]]
id = "rachio_main"
default = true
kind = "rachio"
[controllers.config]
api_token = "${RACHIO_API_TOKEN}"
device_id = "..."
[controllers.config.zone_uuid_map]
back_yard = "..."  # Rachio zone UUID
```

Status: schema variant exists, adapter implementation deferred. Until then, drive your Rachio through HA's Rachio integration and use `ha_service_call`.

## Hunter Hydrawise / B-hyve / others (community)

Both speak cloud APIs that HA integrations exist for. The LocalSky path until native adapters exist: drive them through HA + `ha_service_call`.

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
