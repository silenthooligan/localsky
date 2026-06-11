# LocalSky

> These docs track LocalSky v0.2.0-beta.1.

**Hyperlocal weather on your hardware. Smart irrigation when you want it.**

LocalSky is two products in one Docker container.

A **self-hosted weather dashboard** that reads your weather station over the LAN (Tempest, Ecowitt, Ambient Weather, Davis, and more), merges Open-Meteo with regional forecast sources (NWS in the US, MET Norway, OpenWeather, Pirate Weather) with per-field provenance, and renders the result in a fast installable PWA with built-in radar (RainViewer worldwide, IEM NEXRAD in the US) and lightning. Useful on its own, even if you never irrigate anything.

A **smart irrigation engine** that pairs the same weather data with peer-reviewed agronomy (FAO-56 reference ET, USDA soil textures, species-aware Kc curves, a 17-rule skip ladder) and drives OpenSprinkler, Rachio, Rain Bird, Hydrawise, B-hyve, or any valve reachable over MQTT or Home Assistant. Optional. Off until you wire a controller.

This site is the operator's manual. The dashboard, settings UI, and first-run wizard are designed to keep you out of YAML and out of the terminal for day-to-day use. The chapters here exist for when you want to understand exactly what the engine is doing, swap a sensor source, calibrate a zone, or wire LocalSky into the rest of your stack.

## Where to start

- New install: jump to **[Quick start](getting-started.md)** for the docker run and the first-run wizard walkthrough.
- Weather-only user: the wizard's "Controllers" step can be skipped. The irrigation surfaces disappear and LocalSky runs as a pure weather product.
- No Home Assistant: **[Standalone mode](standalone.md)** covers sensors via MQTT, Ecowitt LAN, and HTTP webhooks.
- Existing HA user: **[Home Assistant integration](hacs.md)** covers the LocalSky integration for HA (installed through HACS). It discovers LocalSky on your network and brings live weather, every zone and its valve, forecasts, and run/stop/pause controls into HA as native entities and services.

## Where things live

| What you want to know | Chapter |
|---|---|
| What weather sources LocalSky can read | [Weather and soil sensors](sensors.md) |
| How the engine decides whether to water | [Irrigation engine](irrigation-engine.md) + [Skip rules in depth](skip-rules.md) |
| Which grass species the catalog supports | [Grass species catalog](grass-species.md) |
| Which soil textures the catalog supports | [Soil texture catalog](soil-textures.md) |
| Which controllers LocalSky drives | [Irrigation controllers](controllers.md) |
| Every config option | [Configuration reference](configuration.md) |
| Every REST + SSE endpoint | [REST + SSE API](api.md) |
| Upgrade from v0.1 | [Upgrading LocalSky](upgrading.md) |
| Something broke | [Troubleshooting](troubleshooting.md) |
| Quick answers | [FAQ](faq.md) |

## Two ways to run it

LocalSky is designed to work well in either configuration:

- **Standalone**: a self-contained service that talks directly to your weather sensors (and optionally to your irrigation controller). Add sensors over MQTT, Ecowitt LAN POST, or HTTP webhooks.
- **Alongside Home Assistant**: install the LocalSky integration from HACS and HA finds LocalSky on your network by itself. HA gets native entities and controls (live weather, zones, valves, forecasts, run/stop/pause); LocalSky owns irrigation scheduling and actuation. An MQTT discovery publisher is also available for setups that prefer MQTT.

Both modes are first-class. Pick the one that fits your stack.

Everything runs on your own hardware. The only outbound calls are the ones you opt into: public forecast sources (Open-Meteo, NWS, and others) and any cloud-backed controller you connect (Rachio, B-hyve, Hydrawise). A LAN-only setup with a local controller makes none.

## Project links

- Source: [github.com/silenthooligan/localsky](https://github.com/silenthooligan/localsky)
- HACS integration: [github.com/silenthooligan/localsky-hacs](https://github.com/silenthooligan/localsky-hacs)
- Issues + discussions: same repos
- License: [Apache-2.0](https://github.com/silenthooligan/localsky/blob/main/LICENSE)
