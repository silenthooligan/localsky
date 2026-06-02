# LocalSky

**Hyperlocal weather on your hardware. Smart irrigation when you want it.**

LocalSky is two products in one Docker container.

A **self-hosted weather dashboard** that reads your Tempest or Ecowitt station over the LAN, merges Open-Meteo and NWS forecasts with per-field provenance, and renders the result in a fast installable PWA with built-in NEXRAD radar and lightning. Useful on its own, even if you never irrigate anything.

A **smart irrigation engine** that pairs the same weather data with peer-reviewed agronomy (FAO-56 reference ET, USDA soil textures, species-aware Kc curves, a 17-rule skip ladder) and drives OpenSprinkler, ESPHome, or Home Assistant. Optional. Off until you wire a controller.

This site is the operator's manual. The dashboard, settings UI, and first-run wizard are designed to keep you out of YAML and out of the terminal for day-to-day use. The chapters here exist for when you want to understand exactly what the engine is doing, swap a sensor source, calibrate a zone, or wire LocalSky into the rest of your stack.

## Where to start

- New install: jump to **[Quick start](getting-started.md)** for the docker run and the first-run wizard walkthrough.
- Weather-only user: the wizard's "Controllers" step accepts an empty list. The irrigation surfaces disappear and LocalSky runs as a pure weather product.
- No Home Assistant: **[Standalone mode](standalone.md)** covers sensors via MQTT, Ecowitt LAN, and HTTP webhooks.
- Existing HA user: **[Home Assistant integration](hacs.md)** covers the outbound MQTT discovery path and the legacy Smart Irrigation + Irrigation Unlimited passthrough.

## Where things live

| What you want to know | Chapter |
|---|---|
| What weather sources LocalSky can read | [Sensors](sensors.md) |
| How the engine decides whether to water | [Irrigation engine](irrigation-engine.md) + [Skip rules](skip-rules.md) |
| Which grass species the catalog supports | [Grass species catalog](grass-species.md) |
| Which soil textures the catalog supports | [Soil texture catalog](soil-textures.md) |
| Which controllers LocalSky drives | [Controllers](controllers.md) |
| Every config option | [Configuration reference](configuration.md) |
| Every REST + SSE endpoint | [REST + SSE API](api.md) |
| How the UI is supposed to feel | [UX journey](ux-journey.md) |
| Upgrade from v0.1 | [Migration from v0.1](migration.md) |

## Two ways to run it

LocalSky is designed to work well in either configuration:

- **Standalone**: a self-contained service that talks directly to your weather sensors (and optionally to your irrigation controller). Add sensors over MQTT, Ecowitt LAN POST, or HTTP webhooks.
- **Alongside Home Assistant**: an outbound MQTT discovery publisher auto-creates `sensor.localsky_*` entities in HA. A passthrough adapter is also available for existing Smart Irrigation + Irrigation Unlimited setups.

Both modes are first-class. Pick the one that fits your stack.

Everything runs on your own hardware. Open-Meteo is the only optional outbound call, and it can be swapped for NWS or any other compatible forecast source.

## Project links

- Source: [github.com/silenthooligan/localsky](https://github.com/silenthooligan/localsky)
- HACS integration: [github.com/silenthooligan/localsky-hacs](https://github.com/silenthooligan/localsky-hacs)
- Issues + discussions: same repos
- License: [Apache-2.0](https://github.com/silenthooligan/localsky/blob/main/LICENSE)
