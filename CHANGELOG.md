# Changelog

All notable changes to LocalSky are documented here. Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Versions follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0-beta.1] - 2026-06-22

### Added

- First-class DIY / ESP32 irrigation controller support, no boxed controller required:
  - New "DIY (HTTP)" controller: drive any board over a small documented HTTP/REST contract (`GET /status`, `GET /zones`, `POST /zone/{id}/run|stop`, `POST /stop_all`, optional bearer token). Fully pollable, so status readback, zone discovery, and the setup wizard's "Test connection" / "Scan zones" all work. Selectable in the setup wizard and Settings > Controllers.
  - The MQTT controller now supports optional state readback: per-zone `state_topic` (with `state_on_payload`), plus a controller-level `availability_topic` and `flow_topic`. With these set, the board's real running state, online/offline status, and live flow feed the dashboard. Command-only (fire-and-forget) behavior is unchanged when they're omitted.
  - Reference firmware in `examples/`: an ESPHome config for the MQTT path and an ESP32 Arduino sketch for the HTTP path, plus a "DIY & ESP32 controllers" documentation page spanning beginner (copy-and-flash) to advanced (raw contract).
- Sticky irrigation overrides: set a global, or per-zone, Auto / Skip / Force decision that persists until you clear it (instead of a one-shot skip). Surfaced on the zones cards and the controls panel.
- New `sensor.localsky_wind_gust_forecast` exposing the day's forecast peak wind gust (Open-Meteo). A wind-shadowed station under-reports gusts, so the forecast feeds the high-wind irrigation skip and is available for Home Assistant automations.

### Fixed

- PWA reliability: `/pkg` WASM and JS assets are now content-hashed and the service worker is push-only, self-cleaning stale caches. This fixes the class of bug where a phone could load a stale asset pair and render the desktop layout or fail to hydrate after an upgrade.

### Changed

- DIY ESP32 boards via ESPHome / Tasmota now go through the MQTT or DIY (HTTP) controller. The non-functional `esphome_native` option (its backend is not yet built) is no longer offered in the controller picker, so a saved controller can no longer silently fail to water.

## [0.4.0-beta.3] - 2026-06-14

A security fixes and hardening release. Upgrading is recommended, especially for instances reachable beyond a trusted LAN.

### Changed

- Home Assistant integration links updated for the renamed `localsky-ha` repository.

### Upgrade notes

- Behind a reverse proxy, set `trusted_proxies` so LocalSky sees the real client IP.

## [0.4.0-beta.2] - 2026-06-14

This release builds out the irrigation and sensor side and makes the whole product easy to set up and learn: flow metering, a first-class sensors experience, point-and-click setup for every data source, documentation built into the app, and contextual help on every screen.

### Features

- Sensors view: a first-class Sensors page showing every gateway and probe with live readings, battery, and signal, with one place to bind a probe to a zone.
- Guided sensor setup: the first-run wizard discovers a gateway's probes and binds them to your zones in a single step.
- Flow metering: LocalSky reads a controller's flow meter and shows live GPM during a run. A clear capable / connected / live distinction means it only reports a meter you actually have.
- Soil sensors wired end to end: labeled forms for Ecowitt and MQTT soil, and an MQTT probe bound to a zone now feeds the engine's skip decisions directly.
- Point-and-click setup for every data source: adding or editing any weather or sensor source (host, port, URL, tokens, API keys, model, poll cadence) is now a labeled form, with the raw JSON kept as an advanced escape hatch.
- LibreWXR radar and smarter forecast sourcing: LibreWXR joins the catalog as a region-aware default radar provider and a forecast source, alongside an Open-Meteo precipitation-forecast layer.
- Documentation built in: the full handbook ships inside the app and opens same-origin at /docs, so it matches your exact build and works offline or on a LAN with no public domain.
- Help on every screen: a question-mark popover with a short explainer and a "Read full doc" link now sits on every complex screen, with new pages for radar, restrictions, advanced settings, the devices hub, and manual schedules. The controller picker links straight to the controller docs.
- Show and hide on secret fields: API keys, tokens, and passwords have a reveal toggle so you can confirm what you pasted.

### Bug fixes

- Setup wizard now completes on a fresh install: license acceptance is saved with the draft, so the toggle keeps its state across steps and the final step no longer rejects an accepted license.
- Setup wizard notification choices (Web Push, MQTT, ntfy, Slack) are carried into the saved configuration instead of being dropped before the final step.
- Flow is no longer reported as present when no meter is connected; the reading now reflects the real device signal.
- OpenWeather sources save correctly (they previously failed to persist).
- The radar Layers panel sizes to its content so settings stay visible, and opens and closes more smoothly.

### Security

- Source credentials (app_key, client_secret, refresh_token) are redacted from the config API instead of returned in cleartext. OAuth client IDs are shown as the public identifiers they are.

### API

- Contract 1.11.0: additive `GET /api/v1/sensors/inventory` (gateways, soil probes, flow).

## [0.4.0-beta.1] - 2026-06-13

Live Radar grows from a single precipitation layer into a full weather map: choose your imagery providers, overlay national alerts and worldwide tropical systems, add community lightning and wind flow, and manage all of it from one Layers panel.

### Features

- Radar provider catalog with a Settings > Radar control: pick which imagery providers the map offers (Auto chooses the best regional source, or define a custom menu) and which layers start on. Sources are region-aware, and your layer choices persist per browser
- National Weather Service alert overlay: severity-colored warning polygons (red extreme, orange severe), tap any polygon for the headline, refreshed every couple of minutes
- Worldwide tropical cyclone tracking: hurricanes, typhoons, and cyclones normalized from the responsible agencies (NOAA NHC/CPHC, JMA, JTWC) into position markers, track lines, and forecast cones; empty when the basins are quiet
- Choose your national forecast model: the weather model behind your forecast is now configurable, from a built-in catalog of national and global models
- Wind flow layer: animated particle flow of current 10 m winds over the visible map, warmer colors for stronger wind, refetched as you pan
- Opt-in Blitzortung community lightning strikes, off by default
- Layers panel: one Layers chip opens a drawer of Imagery and Overlays, each with a toggle, an expandable legend, and source attribution; it overlays the map without resizing it and replaces the old legend rail and layer control. A stacked-layers icon, accent outline, and active-count badge make the picker unmistakable, and a footer link jumps straight to Settings > Radar
- API contract 1.10.0: additive radar endpoints for the tropical-cyclone feed, the wind grid, and the forecast-model catalog

### Bug fixes

- Outbound National Weather Service requests now identify LocalSky by its project URL instead of a personal contact, per the NWS API policy

## [0.3.0-beta.2] - 2026-06-11

### Features

- Serve under a URL prefix: LocalSky honors the `X-Ingress-Path` header from prefix-stripping reverse proxies, so it runs correctly behind a subpath, while direct port access keeps working unchanged

### Bug fixes

- Fresh installs no longer show four phantom irrigation zones; zones come from your configuration (the wizard), and a pristine instance starts empty
- Web Push subscriptions work on fresh installs (the subscription table was only created on databases carried over from v1)

## [0.3.0-beta.1] - 2026-06-11

### Features

- History run log: per-day rows for every start, duration, and skip, with day watered totals
- Run log search (zone, reason, watered/skipped), 7/30/90/All range chips, and a month jump; the log fetches its own window so All is genuinely all
- Dated x-axes on History and zone charts (oldest to newest), zero-floored y-axis
- Rule manager: enable/disable, reorder, delete; template farm with six curated starting points
- Built-in skip gates are operator-controllable per gate behind a warning acknowledgement; control and legal gates stay locked; the trace marks gates disabled by operator
- Segmented On|Off toggle pills on gates and rules
- History retention setting (`persistence.runs_retention_days`, default keep forever) with daily prune
- Soil probe fault detection: 24h without a valid reading surfaces in `/api/health` (degraded), the health banner, and a one-time push naming the zone
- Per-zone verdict enforcement at dispatch: zones whose own verdict says skip are logged with the reason and not watered
- Operator-opt-in analytics tag (`LOCALSKY_ANALYTICS_*` env, off by default)
- Demo mode seeds 30 days of synthetic run history
- API contract 1.8.0: additive `soil_probe_faults` on snapshot and health

### Bug fixes

- Rain today reads the native Tempest daily accumulator (local-midnight rollover, restart-safe reseed, persisted to history); the HA WeatherFlow per-minute precipitation entity is no longer misread as a daily total
- Days-since-rain takes the min of the regional model and the station's own observed history
- The yard-wide saturation gate names zones missing soil readings instead of going silently inapplicable
- Scheduler no longer double-records completed runs
- Title/subtitle spacing normalized across page headers, panels, and gate rows

## [0.2.0-beta.1] - 2026-06-10

The v2 burndown. Lays a ports-and-adapters foundation underneath the existing v0.1 deployment without changing observable behavior, plus the standalone, UI, and ops work to make LocalSky a viable open-source product.

### Added

#### Launch hardening (auth, identity, discovery, ops)

- Built-in authentication: owner account (argon2id), browser sessions, show-once API tokens for integrations; `[auth]` policy block with `mode` (default `disabled` on upgrades, `required` for new wizard installs that create an account), rolling `session_ttl_days`, and `trusted_networks` CIDRs. Login page, wizard Account step, Settings Account section with token management. Static assets, `/api/v1/info`, ingest receivers, and liveness stay public; anonymous health is trimmed to liveness-only
- Stable instance identity (`/data/instance-id`) surfaced in `/api/v1/info` (`uuid`) and announced over mDNS as `_localsky._tcp.local.` with version + auth TXT records (config-gated via `[network] mdns_enabled`), enabling Home Assistant zeroconf discovery
- Timezone inference: offline lat/lon to IANA lookup autofills the wizard, persists on apply, and backs `GET /api/v1/location/timezone`; the wizard Location step now persists to the draft and gained address search via the Nominatim proxy
- Config validation (`GET /api/v1/config/validate`): structured errors/warnings with stable codes; errors block wizard apply and `PUT /api/config`
- One-sweep network discovery for the wizard (`GET /api/wizard/discover`): passive Tempest detection, Ecowitt UDP broadcast probe, OpenSprinkler LAN sweep; Scan-my-network panels with one-click Add on the Sources + Controllers steps
- Backup + restore: `GET /api/v1/backup` (tar.gz of config + consistent database copy + manifest), `POST /api/v1/backup/restore` (validated config applies live; database stages and swaps at next boot), snapshot listing, and Download/Restore controls under Settings Advanced
- Opt-in update check (`[updates] check_enabled`): daily GitHub releases poll behind `GET /api/v1/updates`; off by default, no telemetry
- Wizard honors its skip promises: skipped Sources synthesize Tempest UDP + Open-Meteo defaults, controllers are optional (weather-only installs are first-class), and DryRun controllers are fully testable with sample zone discovery
- API contract 1.6.0: additive `auth_required` + `uuid` on `/api/v1/info`; new `/api/v1/auth/*` endpoint family

#### Devices + Home Assistant parity

- Device model: a unified registry of gateways, controllers, cloud services, and the HA bridge, each grouping the sensors or zones it provides. `GET /api/v1/devices` and a Devices settings panel
- Native Ecowitt gateway support without Home Assistant: a local-API poller (`ecowitt_gw_poll`, reads `/get_livedata_info` and records soil/weather to history; handles both `ch_soil` and `ch_ec` probes) alongside the existing push receiver
- Native LAN gateway discovery: Ecowitt UDP broadcast (`GET /api/v1/devices/discover`) with a "Discover gateways" button; multi-homed hosts are probed per interface with computed subnet broadcasts
- Home Assistant device import over the WebSocket API (device + entity registries), scoped to weather/soil/irrigation-relevant devices

#### Standalone runtime (Home Assistant optional)

- Native pause / one-day override persisted locally so a no-HA deploy can be paused (HA helpers no longer required)
- Config-fed per-zone weekly water budgets so any configured zone gets a run-time, not just a fixed set
- Configurable Home Assistant controller entity prefix (`deployment.ha_sprinkler_prefix`) so the HA path works for any controller naming, not one hardcoded deployment

#### Engine (was SI + IU, now native)

- FAO-56 Penman-Monteith reference ET0 with ASCE-EWRI 2005 simplified variant and Hargreaves-Samani 1985 fallback when only temp range + lat + DOY are available
- Single-bucket soil water balance with TAW/RAW/MAD per zone; depletion-driven scheduling
- Infiltration-aware cycle-and-soak splitter that respects per-soil + per-slope infiltration rates
- 12-species grass catalog with monthly Kc piecewise curves and UF/IFAS citations (St. Augustine, Bermuda, Zoysia, Bahia, Centipede, KBG, TTTF, PRG, plus ornamental shrubs, vegetable garden, drip / xeriscape)
- 7-class USDA soil texture catalog (FAO-56 Table 19 + USDA NRCS Part 652) with FC, WP, AW, and slope-graded infiltration
- 17-rule skip ladder extracted to `engine::skip_rules`; v0.1 hardcoded constants exposed as `SkipRuleParams` config fields whose defaults preserve previous verdicts exactly
- 7-day verdict strip and water-budget projections derived from native engine

#### Configuration

- Full TOML schema (deployment, features, sources, controllers, zones, llm, notifications, engine params, mqtt, webpush)
- `${VAR}` env interpolation; `env_compat` layer synthesizes a v2 Config from legacy env vars so existing deployments boot unchanged
- Versioned migrations between schema revisions
- Atomic writes (tmp + fsync + rename) with snapshot-before-write into `config_snapshots` (20-version retention) and always-reachable `/api/config/rollback`
- Recursive secret redaction with sentinel + unredact roundtrip on PUT so `/api/config` never leaks tokens (4 unit tests)
- Validation: target_min < saturation, area > 0, precip in (0, 200], no whitespace in ids, lat/lon ranges; structured error responses

#### First-run wizard + settings UI

- 8-step first-run wizard: Welcome -> Location -> Sources -> Controllers -> Zones -> LLM -> Notifications -> Review + Apply
- Wizard REST endpoints (`/api/wizard/draft`, `/apply`, `/test_source`, `/test_controller`, `/scan_zones`, `/geocode`)
- Settings UI under `/settings/*` with editors for location, sources (list + add/remove/test), controllers (list + add/remove/test), zones (list + per-zone form), LLM, notifications, advanced/engine
- `<SetupGate/>` redirects to `/setup/welcome` until `/data/localsky.toml` exists

#### Persistence

- Hand-rolled migration runner with versioned SQL files and `assert_monotonic()` per-PR unit test
- `runs` evolved to v2 (status, controller_id, source, et0_mm, etc) via table-rebuild migration; DB-backed in-flight run state (no in-memory loss across restarts)
- `sensor_history` time-series store with `(epoch, source_id, key)` PK and per-source freshness query
- `verdict_history` with `inputs_json` for engine replay against historical conditions
- `config_snapshots` retention trigger; `push_subscriptions` moved out of `push/store.rs` into persistence layer

#### Controllers HAL

- `IrrigationController` port with three adapters at launch: DryRun (demo + tests), OpenSprinklerDirect (HTTP API, MD5 auth), HaServiceCall (legacy continuity)
- Arc-swap controller registry; hot-reload swaps the default mid-session without interrupting in-flight runs
- `ControllerCaps` declared per adapter (flow_meter, rain_sensor, master_valve, multi_zone_parallel, history_query, remote_program_upload)
- LAN discovery in the wizard: an HTTP /24 sweep finds OpenSprinkler controllers. (LocalSky advertises itself over mDNS for clients to find; it does not browse mDNS for controllers.)

#### Sources + standalone sensors

- `WeatherSource` port + `MergedSnapshot` with per-field provenance `{value, source_id, observed_at}`
- Per-field merge policies (max for rain/wind, min for low temp, configurable priority for ET0)
- DemoReplay synthetic source; TempestUdp (LAN); OpenMeteo (with `et0_fao_evapotranspiration` query)
- **Standalone sensor paths** (no HA required):
  - **MQTT subscribe source**: connects to any broker, wildcards + JSON path + scale/offset; pairs cleanly with the outbound publisher
  - **Ecowitt local source**: POST receiver at `/ingest/ecowitt` for GW1100/GW2000 gateways
  - **HTTP webhook source**: generic JSON POST at `/ingest/webhook/<id>` for ESPHome, custom integrations, scripts

#### LLM provider abstraction

- `LlmProvider` port with two adapters: OllamaProvider (native `/api/chat`) and OpenaiCompatProvider (`/v1/chat/completions`, covers OpenAI, Anthropic-compat shims, vLLM, LM Studio, llama.cpp `/v1`, and any private gateway)
- Boot-time `auto_detect` probes `localhost:11434`, `:8080`, `:1234`; first success wins
- `Advisor` accepts `Arc<dyn LlmProvider>`; TTL cache and prompts unchanged; cache key includes model so swap invalidates cleanly

#### HA bridge (optional, not required)

- MQTT discovery publisher: HA users get auto-created `sensor.localsky_*`, `binary_sensor.localsky_zone_*_running`, `switch.localsky_zone_*_run_now` without LocalSky reading HA
- Outbound publish skips entities tagged `attribution = "LocalSky"` on inbound MQTT to prevent feedback cycles
- Legacy `HaServiceCall` controller for v0.1 continuity

#### Health, observability, demo

- `/api/health` reports per-source freshness (fresh/stale/offline) + per-controller summary + DB + LLM
- `LOCALSKY_DEMO=1`: synthetic data feeder populates TempestStore, IrrigationStore, ForecastStore so the dashboard renders fully without any real hardware; cycling verdicts; 7-day forecast variety; 4 demo zones

#### UI

- Mobile parity polish: zone math reveal + 14-day daily-totals sparkline on `/irrigation/zone/:slug`
- Design system primitives (`<Panel/>`, `<Card/>`, `<Sheet/>`, `<Toggle/>`, `<Slider/>`, `<SegmentedControl/>`, `<FormField/>`, `<EmptyState/>`)
- `<Sheet/>` is viewport-aware (bottom-sheet mobile, centered modal desktop)

#### Open-source readiness

- Apache-2.0 license, NOTICE with citations, `.env.example`, expanded `.gitignore`
- Public README, CONTRIBUTING, SECURITY, CODE_OF_CONDUCT, this CHANGELOG
- Docs site under `docs/` covering getting-started, standalone, controllers, sensors, irrigation-engine, grass-species, soil-textures, skip-rules, configuration, api, ux-journey, hacs

### Internal

- 157 unit tests covering engine math, persistence migrations, controllers, sources, LLM providers, config redaction, and engine inputs
- `cargo check --features ssr`: zero warnings; `cargo check --features hydrate --target wasm32-unknown-unknown`: zero warnings
- All v0.1 behavior paths still work; v2 modules are additive

## [0.1.0] - Internal release

Initial homelab deployment. Tempest UDP weather, Open-Meteo forecast, Home Assistant Smart Irrigation + Irrigation Unlimited integration, OpenAI-compatible LLM advisor, four hardcoded zones, glass-morphism PWA UI. Never publicly released.
