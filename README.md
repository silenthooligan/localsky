<h1 align="center">LocalSky</h1>

<p align="center">
  <strong>Local-first weather and irrigation for hyperlocal homes.</strong><br>
  Tempest, Ecowitt, OpenSprinkler, Home Assistant. No cloud required.
</p>

<p align="center">
  <a href="LICENSE"><img alt="License: Apache-2.0" src="https://img.shields.io/badge/License-Apache_2.0-3b82f6.svg"></a>
  <a href="https://www.rust-lang.org/"><img alt="Built with Rust" src="https://img.shields.io/badge/Built_with-Rust-d97706.svg"></a>
  <a href="https://leptos.dev/"><img alt="Built with Leptos" src="https://img.shields.io/badge/Built_with-Leptos-9333ea.svg"></a>
  <a href="https://github.com/silenthooligan/localsky/releases"><img alt="Status" src="https://img.shields.io/badge/Status-Alpha-f59e0b.svg"></a>
</p>

<p align="center">
  <img src="docs/assets/screenshots/dashboard-desktop.png" alt="LocalSky desktop dashboard" width="92%">
</p>

LocalSky is a single-binary, single-image, full-stack PWA that combines hyperlocal weather observations, FAO-56 reference evapotranspiration, grass- and soil-aware irrigation scheduling, and an optional local LLM advisor.

## Two ways to run it

LocalSky is designed to work well in either configuration:

- **Standalone.** A self-contained service that talks directly to your weather sensors and irrigation controller. Add sensors over MQTT, Ecowitt LAN POST, or HTTP webhooks. See [docs/standalone.md](docs/standalone.md).
- **Alongside Home Assistant.** An outbound MQTT discovery publisher auto-creates `sensor.localsky_*` entities in HA. A passthrough adapter is also available for existing Smart Irrigation + Irrigation Unlimited setups. Both modes are first-class; pick the one that fits your stack.

Everything runs on your own hardware. Open-Meteo is the only optional outbound call, and it can be swapped for NWS or any other compatible forecast source.

## Screenshots

<p align="center">
  <img src="docs/assets/screenshots/irrigation-desktop.png" alt="Irrigation dashboard with 7-day verdict strip, next-run card, and live forecast intelligence" width="92%"><br>
  <em>Irrigation page: 7-day verdict strip, next-run card, full skip-rule breakdown, water budget, and live forecast intelligence</em>
</p>

<p align="center">
  <img src="docs/assets/screenshots/radar-desktop.png" alt="Live radar with RainViewer precipitation, NEXRAD reflectivity, satellite IR, and Tempest lightning rings" width="92%"><br>
  <em>Live radar: RainViewer precipitation, IEM NEXRAD reflectivity, satellite IR, Tempest lightning rings, layer toggles, legend, playback</em>
</p>

<p align="center">
  <img src="docs/assets/screenshots/zone-controls-desktop.png" alt="Manual zone controls with idle/running badges and 10/30/60-minute run buttons per zone" width="92%"><br>
  <em>Manual zone controls: idle / running badge per zone, planned / today / bucket readouts, and 10 / 30 / 60-minute quick-run buttons. Running zones swap to a single red STOP.</em>
</p>

<table>
  <tr>
    <td align="center" width="50%">
      <img src="docs/assets/screenshots/settings-skip-rules.png" alt="Settings page editing the 17-rule skip ladder thresholds" width="100%"><br>
      <em>Override every threshold in the skip ladder: rain, wind, freeze, soil frost, heat advisory. Defaults shown inline; engine picks up new values on the next tick.</em>
    </td>
    <td align="center" width="50%">
      <img src="docs/assets/screenshots/wizard-zones.png" alt="First-run wizard showing the 12-species grass catalog with Kc, root depth, and MAD" width="100%"><br>
      <em>First-run wizard: full 12-species grass catalog with Kc range, root depth, and MAD per species. Drop photos in <code>public/grass-species/</code> to populate the cards.</em>
    </td>
  </tr>
</table>

<table>
  <tr>
    <td align="center" width="50%">
      <img src="docs/assets/screenshots/mobile-zone-detail.png" alt="Mobile zone detail with FAO-56 math reveal and 14-day sparkline" width="62%"><br>
      <em>Mobile zone detail: status, 14-day sparkline, FAO-56 math chain, run history</em>
    </td>
    <td align="center" width="50%">
      <img src="docs/assets/screenshots/mobile-irrigation.png" alt="Mobile irrigation main view" width="62%"><br>
      <em>Mobile irrigation page: next-run hero, advisor card, forecast breakdown</em>
    </td>
  </tr>
</table>

## Features

### Engine

- **Native FAO-56 Penman-Monteith** reference ET with ASCE-EWRI 2005 simplified and Hargreaves-Samani 1985 fallbacks
- **Single-bucket water balance** with TAW / RAW / MAD per zone; depletion-driven scheduling
- **Cycle-and-soak** infiltration splitter that respects soil texture and slope
- **12-species grass catalog** with monthly Kc curves (St. Augustine, Bermuda, Zoysia, Bahia, Centipede, KBG, TTTF, PRG, plus ornamental shrubs, vegetable garden, drip / xeriscape)
- **7-class USDA soil texture catalog** with field capacity, wilting point, available water, and slope-graded infiltration
- **17-rule skip ladder** with configurable thresholds: rain now, rain next 4 h, probability-weighted 3-day and 7-day rollups, freeze, soil saturation, soil frost, heat advisory, high wind
- **7-day forward verdict strip** - the same engine that decides today, projected forward

### Sources, controllers, and integrations

- **Multi-source weather merge** with per-field provenance (Tempest UDP local, Open-Meteo, NWS, Ecowitt LAN, plus generic MQTT subscribe and HTTP webhook sources)
- **Multi-controller HAL**: OpenSprinkler direct, HA service call, ESPHome native (community), Rachio cloud (planned), DryRun for demo and tests
- **Home Assistant optional**: outbound MQTT discovery auto-creates `sensor.localsky_*` entities; inbound passthrough is opt-in
- **Standalone sensor paths** (no HA needed): MQTT subscribe with JSON-path extraction, Ecowitt gateway local POST, and a generic HTTP webhook ingester for ESPHome or custom scripts
- **Local LLM advisor**: Ollama auto-detect, llama.cpp, OpenAI-compatible (LM Studio, vLLM, any private gateway)

### UI and operability

- **Installable PWA** on iOS, Android, and desktop with VAPID-signed push notifications
- **First-run wizard** + in-app settings; no editing config files by hand
- **Versioned JSON schema** published at `/api/config/schema` for the settings UI
- **Atomic config writes** with snapshot-before-write retention and always-reachable rollback endpoint
- **Versioned SQLite migrations** with engine-replay history (stored verdict inputs replay through the current rules)
- **Demo mode** (`LOCALSKY_DEMO=1`) ships with synthetic Tempest / forecast / irrigation streams so the UI is fully populated out of the box
- **Dark glass-morphism design** with claymorphic accents - full mobile + desktop parity

## Quick start

```bash
docker run -d \
  --name localsky \
  -p 8090:8090 \
  -v localsky-data:/data \
  -e LOCALSKY_DEMO=1 \
  ghcr.io/silenthooligan/localsky:latest
```

Visit <http://localhost:8090>. The `LOCALSKY_DEMO=1` flag boots with simulated data so you can explore the UI before connecting any hardware.

For a real install, drop `LOCALSKY_DEMO`, mount your config volume, and visit `/setup` - the first-run wizard walks you through location, weather sources, controllers, and zones. See [docs/getting-started.md](docs/getting-started.md) for a full walkthrough.

## Hardware compatibility

| Category | Device | Status |
|---|---|---|
| Weather | Tempest WeatherFlow (UDP LAN) | Tested |
| Weather | Open-Meteo / NWS | Tested |
| Weather | Tempest Cloud WebSocket | Planned |
| Weather | Ecowitt GW1100 / GW2000 LAN | Planned |
| Soil | Ecowitt WH51 / WH52 (via GW1x00) | Tested |
| Soil | Any MQTT-published soil sensor | Tested |
| Controller | OpenSprinkler firmware 2.1.9+ | Tested |
| Controller | OpenSprinkler Pi | Tested |
| Controller | HA Irrigation Unlimited passthrough | Tested |
| Controller | ESPHome sprinkler component | Community |
| Controller | Rachio Gen 2 / 3 cloud | Planned |
| LLM | Ollama (any tool-capable model) | Tested |
| LLM | llama.cpp HTTP server | Community |
| LLM | OpenAI / Anthropic compatible | Tested |
| LLM | LM Studio, vLLM | Community |
| Push | Web Push (VAPID) | Tested |
| Push | ntfy.sh / Slack webhook | Planned |

Promote to **Tested** only when CI fixture or maintainer-confirmed run exists.

## Documentation

Full docs live in [`docs/`](docs/) and are built into an mdBook for online viewing. Start here:

- [Getting started](docs/getting-started.md) - install, prerequisites, and first-run walkthrough
- [Standalone mode](docs/standalone.md) - the full no-Home-Assistant path
- [Controllers](docs/controllers.md) - OpenSprinkler deep-dive plus alternatives
- [Sensors](docs/sensors.md) - what each sensor type unlocks
- [Configuration reference](docs/configuration.md) - every `localsky.toml` field
- [API reference](docs/api.md) - REST + SSE endpoints, JSON shapes
- [Irrigation engine](docs/irrigation-engine.md) - FAO-56 walkthrough with citations
- [Grass species](docs/grass-species.md) and [soil textures](docs/soil-textures.md)
- [Skip rules](docs/skip-rules.md) - every rule in the ladder, explained
- [HACS integration](docs/hacs.md) - Home Assistant Community Store roadmap
- [UX journey](docs/ux-journey.md) - first-run, upgrades, hardware changes, config changes
- [Launch checklist](docs/launch-checklist.md) - gating criteria for the 0.1.0 cut

## Architecture at a glance

LocalSky is a Rust + Leptos full-stack PWA. The SSR server is a single statically-linked binary; the WASM client hydrates the streamed HTML. The internal layout follows a ports-and-adapters shape so every external system is swappable.

```
engine/        FAO-56 ET0, water balance, cycle-and-soak, skip rules - pure functions, no I/O
ports/         WeatherSource, IrrigationController, LlmProvider, NotificationSink, ConfigStore
sources/       Tempest UDP, Open-Meteo, Ecowitt LAN, MQTT subscribe, HTTP webhook, demo replay
controllers/   OpenSprinkler direct, HA service call, DryRun
llm/providers/ Ollama, OpenAI-compatible (covers LM Studio, vLLM, llama.cpp /v1, private gateways)
ha/            Optional MQTT discovery publisher + opt-in REST passthrough
persistence/   Hand-rolled SQLite migrations, runs / sensor_history / verdict_history / config_snapshots
api/           Axum routes for snapshot, irrigation, forecast, config, wizard, health, LLM
components/    Leptos UI primitives plus the irrigation, forecast, weather, and settings surfaces
```

## Roadmap

**0.1** (initial public release): demo mode, OpenSprinkler direct, Tempest UDP, Open-Meteo, MQTT subscribe, Ecowitt local, Ollama and OpenAI-compatible LLM, first-run wizard, full settings UI, mobile parity.

**0.2**: ESPHome sprinkler controller, Tempest cloud WebSocket source, NWS source, Ambient Weather source, ntfy + Slack notification sinks, hosted demo site.

**0.3**: Rachio cloud controller, Pirate Weather source, MET Norway source, HACS publishing for the inbound HA integration, telemetry opt-in.

## Acknowledgements

LocalSky stands on the shoulders of decades of agronomy and meteorology research, and on the work of a vibrant open-source community:

- FAO Irrigation and Drainage Paper No. 56 (Allen et al., 1998)
- ASCE-EWRI Standardized Reference Evapotranspiration (2005)
- UF/IFAS Extension publications on Florida turfgrass species (ENH6, ENH8, ENH11, ENH19, ENH62, ENH1115)
- USDA NRCS National Irrigation Guide (Part 652)
- The Home Assistant Smart Irrigation and Irrigation Unlimited integrations, whose deployments helped shape this engine
- Open-Meteo, RainViewer, Leaflet, Leptos, rumqttc, Axum, tokio

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, code style, and the PR workflow. New grass species, soil textures, weather sources, and irrigation controllers are particularly welcome.

Security disclosures: see [SECURITY.md](SECURITY.md).

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
