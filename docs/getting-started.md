# Getting Started with LocalSky

This guide takes you from "no LocalSky installed" to "watching real weather and managing real zones" in about 15 minutes. Two paths: **Demo mode** if you just want to see the UI, and **Real install** if you have hardware.

## Prerequisites

LocalSky is delivered as a Docker image. Anywhere Docker runs, LocalSky runs:

- Linux (any distro; native Docker)
- macOS (Docker Desktop, OrbStack, or colima)
- Windows (Docker Desktop with WSL2 backend)
- Synology / QNAP NAS (Container Manager)
- Raspberry Pi 4 or 5 (64-bit OS, multi-arch image ships arm64)
- Unraid, Proxmox containers, TrueNAS Scale

You do not need a Linux box, a server room, or a dedicated machine. A workstation that's powered on most of the day works fine; LocalSky runs in ~30 MB resident memory.

What you do need:

- About 200 MB of disk for the image + a few hundred KB for the SQLite database
- A free port (8090 by default; remap at the docker run layer if taken)
- (Optional) An always-on host if you want irrigation to dispatch on schedule

## Demo mode (no hardware required)

```bash
docker run -d \
  --name localsky \
  -p 8090:8090 \
  -e LOCALSKY_DEMO=1 \
  ghcr.io/silenthooligan/localsky:latest
```

Open http://localhost:8090. The dashboard renders with simulated weather and an in-memory dry-run controller. Every actionable button shows what it would have done but never fires anything. Useful for:

- Exploring the UI before committing to a hardware setup
- Showcasing LocalSky to friends or in a presentation
- Running screenshots for documentation
- Verifying a Docker image build before deploying it

The demo data loops on a synthetic Florida summer day at 10× wall-clock rate. No external network calls except the Leaflet stylesheet for the radar map.

## Real install

### What you need

- Docker (see Prerequisites above)
- Your latitude and longitude
- (Optional) An irrigation controller. See [docs/controllers.md](controllers.md) for the supported list. Without one, LocalSky becomes a hyperlocal weather dashboard with no actionable irrigation; that's a fine starting point.
- (Optional) An LLM endpoint for the advisor. Ollama on the same host is the easiest path; see [docs/llm.md](llm.md).

### Install

```bash
mkdir -p /opt/localsky/data
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v /opt/localsky/data:/data \
  -e LOCALSKY_V2=1 \
  ghcr.io/silenthooligan/localsky:latest
```

`/opt/localsky/data` (or wherever you point the `-v` mount) is where the config file and SQLite database live. Adjust the host path to fit your filesystem.

`LOCALSKY_V2=1` opts into the new wizard + settings UI; without it, you'd be on the legacy single-config path that requires hand-editing env vars.

Visit http://localhost:8090. The dashboard redirects you to `/setup` because there's no config file yet.

### First-run wizard

Eight steps; none take more than a minute.

1. **Welcome** - accept the Apache-2.0 license. Telemetry defaults off.
2. **Location** - latitude + longitude in decimal degrees. Elevation optional; improves FAO-56 ET₀. Timezone optional too (derives from lat/lon at boot).
3. **Sources** - informational. Auto-creates a Tempest UDP listener (in case you have one) + Open-Meteo forecast. Full editor under `/settings/sources` post-wizard.
4. **Controllers** - informational. Auto-detects HA env vars and creates an HA-service-call controller if present; otherwise add one under `/settings/controllers`.
5. **Zones** - informational. Configure under `/settings/zones`.
6. **LLM** - pick a provider, or leave at "Auto" or "None".
7. **Notifications** - Web Push, MQTT, ntfy, Slack (all independent + optional).
8. **Review** - click Save and finish. Settings write to `/data/localsky.toml` atomically with a snapshot to history.

### After the wizard

Everything is editable under `/settings`. See [docs/configuration.md](configuration.md) for the field-by-field reference.

## Standalone vs Home Assistant integration

> **TL;DR**: LocalSky is a complete native product, not an HA add-on. Smart Irrigation and Irrigation Unlimited are no longer required; LocalSky's engine does what they did. HA can still play a role (Mode 2 or 3 below) but is never a dependency. Deep version: [docs/standalone.md](standalone.md).

LocalSky has three operating modes. Pick the one that fits your stack.

### Mode 1: Standalone (no Home Assistant)

LocalSky talks directly to your irrigation hardware. No HA install required, no HA running, no MQTT broker.

Setup:
1. Run the install command above without setting `HA_URL` or `MQTT_HOST`.
2. In the wizard's Controllers step, configure your direct-controlled controller (OpenSprinkler is the canonical example).
3. Done. LocalSky's dashboard becomes your irrigation surface; the engine drives zones directly.

What this gets you:
- Weather dashboard
- Engine-driven irrigation with full ET / soil / skip-rule logic
- Controller HAL handles dispatch
- Push notifications via Web Push (browser only)
- Optional LLM advisor

What you give up: HA's broader sensor + automation ecosystem. If you don't have HA today, you don't need it.

### Mode 2: Outbound to HA (LocalSky publishes, HA consumes)

LocalSky talks to your controller directly, AND publishes its state via MQTT discovery so existing HA dashboards see `sensor.localsky_*` entities automatically. No HA YAML required.

Setup:
1. Same install command.
2. Configure your controller directly under `/settings/controllers`.
3. Under `/settings/notifications`, set the MQTT broker host (your existing HA broker, or any other).
4. HA auto-discovers `sensor.localsky_<zone>_bucket_mm`, `sensor.localsky_<zone>_et_today_mm`, `sensor.localsky_<zone>_planned_seconds`, `binary_sensor.localsky_zone_<zone>_running`, and `sensor.localsky_verdict_today`.

This is the recommended mode for users who use HA for the rest of their home but want LocalSky to own irrigation.

### Mode 3: HA-driven (legacy continuity)

LocalSky's controller dispatches through HA service calls instead of directly. Useful when you already run Smart Irrigation + Irrigation Unlimited + OpenSprinkler HACS through HA and don't want to re-plumb.

Setup:
1. Pass `HA_URL` and `HA_LONG_LIVED_TOKEN` env vars to the container.
2. In the wizard's Controllers step, pick `ha_service_call`. Map your LocalSky zones to HA entity ids.
3. LocalSky reads HA state via `/api/states`, dispatches runs via `/api/services/<domain>/<service>`.

This is the path for upgrading an existing HA-driven irrigation setup without losing automations.

## Remote reachability

LocalSky listens on `0.0.0.0:8090` inside the container by default. Several ways to reach it from outside the LAN:

### Tailscale (easiest)

Install Tailscale on the host running Docker. Connect your devices to the same tailnet. Visit `http://<host-tailscale-ip>:8090` from anywhere. No port forwarding, no DNS, no TLS cert; the tailnet does WireGuard between your devices and authenticates via your identity provider.

```bash
# On the Docker host
curl -fsSL https://tailscale.com/install.sh | sh
tailscale up
```

The dashboard works through Tailscale exactly as on localhost.

### Reverse proxy with TLS (production)

Front LocalSky with Caddy, nginx, or Traefik. Get a free Let's Encrypt cert. Expose the proxy port (443) to the internet.

Caddy example:

```caddy
localsky.example.com {
    reverse_proxy localhost:8090
}
```

Add basic auth or oauth2-proxy if you want authentication. LocalSky doesn't enforce auth itself; the proxy layer is where you add it.

### Cloudflare Tunnel

`cloudflared tunnel` exposes LocalSky via a Cloudflare-managed edge without opening any ports. Works behind CGNAT and on networks that don't allow inbound connections.

```bash
docker run -d \
  --name cloudflared \
  --restart unless-stopped \
  cloudflare/cloudflared:latest \
  tunnel --no-autoupdate run --token YOUR_TUNNEL_TOKEN
```

### Local LAN only

Without any of the above, the dashboard is reachable from any device on the same LAN at `http://<host-lan-ip>:8090`. Add an mDNS / Avahi entry for nicer URLs (`http://localsky.local:8090`).

### Mobile PWA from a remote URL

The Web Push functionality works through any of the reachability options above. Subscribe per device once the dashboard is loaded. The service worker handles offline reads of cached snapshots so the dashboard stays usable when the device is off-network.

## Irrigation controllers

The full list of supported controllers and their integration shape lives in [docs/controllers.md](controllers.md). Short version:

- **OpenSprinkler** (firmware 2.1.9+) - the ideal controller. Direct HTTP API on the LAN, no cloud, $130-180 hardware.
- **OpenSprinkler Pi** - same protocol as the boxed version; runs on a Raspberry Pi
- **Home Assistant service call** - works with any HA-driven irrigation integration (opensprinkler HACS, irrigation_unlimited, rachio, esphome sprinkler component, hubitat sprinkler, etc.)
- **ESPHome sprinkler component** (planned) - DIY ESP32-based controllers
- **Rachio** Gen 2 / 3 (planned) - cloud API, $130-250 hardware
- **DryRun** - no-op for testing + demos

LocalSky's controller HAL is a Rust trait; adding new adapters takes ~100-200 lines. See [CONTRIBUTING.md](../CONTRIBUTING.md).

## Optional: sensors

LocalSky's engine is fully functional without any sensors beyond the weather sources. Adding sensors unlocks additional logic:

| Sensor type | Unlocks |
|---|---|
| Soil moisture (Ecowitt WH51 / WH52, Aqara, Sonoff) | Per-zone saturation skip, soil-moisture projection, smarter dry-out detection |
| Soil temperature | Soil-frost skip rule (catches the "cold soil + sprinkler = frozen lawn" case better than air temp alone) |
| Rain gauge (separate from weather station) | Improves rain-today accumulation accuracy |
| Lightning detector | Powers the lightning panel + safety skip during active storms |
| Flow meter (on controller) | Validates actual delivered water vs. computed mm depth |

The dashboard renders cleanly without any of these; sensor tiles show empty states with "Connect a sensor to unlock soil-saturation rules" affordances. Once a source provides the data, the tile lights up and additional skip rules activate. The engine never blocks on missing sensor data - weather + ET-based math is the always-on baseline.

## Optional: Local LLM

LocalSky's advisor produces plain-English explanations of why today's verdict is what it is. Three setup paths from easiest to most flexible:

### Ollama (recommended)

If you have Ollama running on the same host:

```bash
ollama pull llama3.2:3b-instruct
```

LocalSky's "Auto" provider probes `localhost:11434` and detects Ollama within seconds. Models with tool support (llama3.1-8b, qwen2.5-7b) give richer responses on a workstation; phi3-mini-Q4 runs comfortably on a Pi 4.

### llama.cpp

Run `llama-server` on the same host listening on `localhost:8080`. Auto-detect picks it up the same way.

### Any OpenAI-compatible endpoint

Anything that speaks `/v1/chat/completions`: OpenAI, LM Studio, vLLM, TGI, Anthropic-compatible shims, etc. Set `LLM_PROVIDER=openai_compat`, `LLM_BASE_URL=https://...`, `LLM_MODEL=...`, `LLM_API_KEY=...`.

## Troubleshooting

- **Dashboard says "no zones"**: the wizard hasn't been run, or the zone editor was skipped. Visit `/setup` or `/settings/zones`.
- **Verdict shows "(weather rules only; soil rules offline)"**: a soil moisture probe isn't reporting. Check the source under `/settings/sources`.
- **LLM advisor is grayed out**: provider is unreachable. Visit `/settings/llm`.
- **MQTT discovery isn't creating entities in HA**: HA's MQTT integration needs the broker connected (Settings → Devices & Services → MQTT → Configure). Discovery topics live under `homeassistant/<component>/<your-deployment-slug>/...`.
- **Container won't start on Raspberry Pi**: confirm 64-bit OS (`uname -m` should report `aarch64`). 32-bit Pi OS is not supported.

## Next steps

- [docs/standalone.md](standalone.md): full no-HA setup including MQTT-based sensor ingestion
- [docs/api.md](api.md): REST endpoints + SSE streams for configs and data
- [docs/controllers.md](controllers.md): every supported controller in depth
- [docs/irrigation-engine.md](irrigation-engine.md): FAO-56 math driving verdicts
- [docs/grass-species.md](grass-species.md): species catalog
- [docs/skip-rules.md](skip-rules.md): every rule in the ladder
- [docs/configuration.md](configuration.md): field-by-field config reference
- [docs/MIGRATION.md](MIGRATION.md): internal Aperture Labs operator path
