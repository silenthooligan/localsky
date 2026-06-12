# Getting Started with LocalSky

This guide takes you from "no LocalSky installed" to "watching real weather and managing real zones" in about 15 minutes. Two paths: **Demo mode** if you just want to see the UI, and **Real install** if you have hardware.

> **Docker is the preferred way to run LocalSky**, and it is what this guide covers. That said, several platforms are supported, and if you run Home Assistant OS there is a convenience option: LocalSky also installs as a [Home Assistant app](home-assistant-app.md) in one click, wizard and all, using the exact same image. (Home Assistant OS and Supervised installs only; the app store does not exist on HA Container/Core, so those use the Docker install below.)
>
> [![Add the LocalSky app repository to my Home Assistant](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fsilenthooligan%2Flocalsky-apps)

## Prerequisites

LocalSky is delivered as a Docker image. Anywhere Docker runs, LocalSky runs:

- Linux (any distro; native Docker)
- macOS (Docker Desktop, OrbStack, or colima)
- Windows (Docker Desktop with WSL2 backend)
- Synology / QNAP NAS (Container Manager)
- Raspberry Pi 4 or 5 (64-bit OS, multi-arch image ships arm64)
- Unraid, Proxmox, TrueNAS Scale

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

The demo data loops on a synthetic humid-subtropical summer day at 10× wall-clock rate. No external network calls except the Leaflet stylesheet for the radar map.

When you're ready for the real thing, remove the demo container with `docker rm -f localsky` and follow the install below. The demo container was started without a volume mount, so nothing it generated persists on disk.

## Real install

### What you need

- Docker (see Prerequisites above)
- Your latitude and longitude
- (Optional) An irrigation controller. See [docs/controllers.md](controllers.md) for the supported list. Without one, LocalSky becomes a hyperlocal weather dashboard with no actionable irrigation; that's a fine starting point.
- (Optional) An LLM endpoint for the advisor. Ollama on the same host is the easiest path; see [docs/llm.md](llm.md).

### Install

```bash
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -p 50222:50222/udp \
  -v localsky-data:/data \
  ghcr.io/silenthooligan/localsky:latest
```

`localsky-data` is a named Docker volume that holds the config file (`/data/localsky.toml`) and the SQLite database. Docker creates it on first run and it survives container upgrades.

> **Prefer a bind mount?** The container runs as uid 10001, not root. If you mount a host directory instead (`-v /opt/localsky/data:/data`), first run `sudo chown -R 10001:10001 /opt/localsky/data`, or start the container with `--user 0:0`.

> **Networking for LAN weather stations.** On Linux, `--network host` is recommended: WeatherFlow Tempest hubs broadcast on UDP port 50222, and the wizard's network discovery (Tempest and Ecowitt broadcasts, OpenSprinkler subnet sweep) needs to see your LAN. With host networking, drop the `-p` flags; LocalSky listens on port 8090 directly. The bridged alternative shown above (`-p 8090:8090 -p 50222:50222/udp`) works too, but LAN broadcasts may not cross the bridge, so discovery can miss devices.

### Docker Compose

The same install as a `docker-compose.yml`:

```yaml
services:
  localsky:
    image: ghcr.io/silenthooligan/localsky:latest
    container_name: localsky
    restart: unless-stopped
    # Recommended on Linux so Tempest UDP broadcasts and network
    # discovery reach the container. Remove the ports: block if you
    # uncomment this.
    # network_mode: host
    ports:
      - "8090:8090"
      - "50222:50222/udp"
    environment:
      - TZ=America/New_York  # your IANA timezone, e.g. Europe/Berlin, Australia/Sydney
    volumes:
      - localsky-data:/data
    healthcheck:
      test: ["CMD", "curl", "-fsS", "http://127.0.0.1:8090/api/v1/health"]
      interval: 30s
      timeout: 5s
      start_period: 30s
      retries: 3

volumes:
  localsky-data:
```

Once the container is up, open http://localhost:8090/setup to start the first-run wizard. A fresh install does not redirect automatically, so go to `/setup` directly.

### First-run wizard

Nine steps; none take more than a minute. Three of them (AI advisor, Notifications, Account) are optional, and the progress strip renders them as hollow dots.

1. **Welcome**: what LocalSky is and the Apache-2.0 license acknowledgement. No telemetry, no analytics, no email signup.
2. **Your location**: search for your address (built-in geocoding) or enter latitude and longitude directly. Elevation improves the FAO-56 ET₀ math; the timezone autofills from an offline dataset whenever lat/lon change.
3. **Weather**: add weather and sensor sources with the same editor used in the Sensors hub. A one-click network scan finds Tempest and Ecowitt hardware on your LAN. Skipping is fine; sources can be added any time afterward.
4. **Controller**: add your irrigation controller with the same editor as Settings, test it live against the real hardware, and scan it for zones. Scanned stations can be imported as zone stubs.
5. **Zones**: explains LocalSky's zone model and shows the grass-species gallery so you pick the right species. Zone editing itself lives under `/settings/zones` after the wizard; zones imported from a controller scan arrive there pre-populated.
6. **AI advisor** (optional): pick an LLM provider, or None. You can test the connection live before finishing. See [llm.md](llm.md).
7. **Notifications** (optional): Web Push, MQTT, ntfy, Slack. All independent; none required.
8. **Account** (optional): create the owner account (username plus a password stored as an argon2id hash). The account is created immediately and you are signed in on that browser; finishing setup switches authentication to required. Skipping leaves auth disabled. See [authentication.md](authentication.md).
9. **Review & apply**: a per-section summary with edit links back into each step. Save and finish writes the config and sends you to the dashboard.

### After the wizard

Everything is editable under `/settings`. See [docs/configuration.md](configuration.md) for the field-by-field reference.

## Standalone vs Home Assistant integration

> **TL;DR**: LocalSky is a complete native product, not an HA add-on. Smart Irrigation and Irrigation Unlimited are no longer required; LocalSky's engine does what they did. HA can still play a role (paths 2 to 4 below) but is never a dependency. Deep version: [docs/standalone.md](standalone.md).

LocalSky has four integration paths. Pick the one that fits your stack.

### Path 1: Standalone (the default)

LocalSky talks directly to your irrigation hardware. No HA install required, no MQTT broker.

Setup:
1. Run the install command above.
2. In the wizard's Controller step, add your direct-controlled controller (OpenSprinkler is the canonical example) and test it.
3. Done. LocalSky's dashboard becomes your irrigation surface; the engine drives zones directly.

What this gets you:
- Weather dashboard
- Engine-driven irrigation with full ET / soil / skip-rule logic
- Controller HAL handles dispatch
- Push notifications via Web Push (browser only)
- Optional LLM advisor

What you give up: HA's broader sensor + automation ecosystem. If you don't have HA today, you don't need it.

### Path 2: HACS integration (recommended for HA users)

Install the LocalSky integration through HACS. It polls LocalSky's REST API and creates native HA entities, driven by LocalSky's entity manifest (`/api/v1/sensors/manifest`), so new zones and sources show up in HA automatically with no MQTT broker and no YAML. LocalSky still owns the irrigation engine and talks to the controller itself; HA gets a live, read-and-act view.

Full walkthrough: [docs/hacs.md](hacs.md).

### Path 3: MQTT discovery (when a broker already runs)

LocalSky talks to your controller directly, AND publishes its state via MQTT discovery so HA dashboards see `sensor.localsky_*` entities automatically. An alternative to the HACS integration when you already run a broker; do not enable both, or you get duplicate entities.

Setup:
1. Same install command; configure your controller under `/settings/controllers`.
2. Under Settings > Notifications, set the MQTT broker host, port, credentials, and discovery prefix, and leave publishing enabled.
3. Settings > Home Assistant shows whether discovery is currently publishing.
4. HA auto-discovers the entities once its MQTT integration is connected to the same broker.

### Path 4: HA service-call controller (valves only HA can reach)

LocalSky's controller dispatches through HA service calls instead of directly. Useful when you already run an HA-driven irrigation integration (opensprinkler HACS, irrigation_unlimited, and similar) and don't want to re-plumb, or when only HA can reach the valves.

Setup:
1. In the wizard's Controller step (or `/settings/controllers`), pick the `ha_service_call` controller type.
2. Give it your HA base URL and a long-lived access token, and map your LocalSky zone slugs to HA entity ids. The start and stop services are configurable (defaults target an OpenSprinkler-style setup).
3. LocalSky dispatches runs via HA's `/api/services/<domain>/<service>` API.

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

LocalSky ships built-in authentication: an owner account (password stored as an argon2id hash) plus API tokens for integrations. New installs that create the owner account in the wizard's Account step finish with auth mode set to `required`; installs that skip that step default to `mode = "disabled"` in the `[auth]` config section. Proxy-level auth (basic auth, oauth2-proxy) is optional defense in depth on top of that, not a substitute. Details: [docs/authentication.md](authentication.md).

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

- **OpenSprinkler** (firmware 2.1.9+), the ideal controller. Direct HTTP API on the LAN, no cloud, US$130-180 hardware (US pricing; varies by region).
- **OpenSprinkler Pi**: same protocol as the boxed version; runs on a Raspberry Pi
- **Home Assistant service call**: works with any HA-driven irrigation integration (opensprinkler HACS, irrigation_unlimited, rachio, esphome sprinkler component, hubitat sprinkler, etc.)
- **ESPHome sprinkler component** (planned), DIY ESP32-based controllers
- **Rachio** Gen 2 / 3 (planned), cloud API, US$130-250 hardware
- **DryRun**: no-op for testing + demos

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

The dashboard renders cleanly without any of these; sensor tiles show empty states with "Connect a sensor to unlock soil-saturation rules" affordances. Once a source provides the data, the tile lights up and additional skip rules activate. The engine never blocks on missing sensor data, weather + ET-based math is the always-on baseline.

## Optional: Local LLM

LocalSky's advisor produces plain-English explanations of why today's verdict is what it is. It is entirely optional: point it at any OpenAI-compatible endpoint, a local Ollama or llama.cpp instance, or nothing at all. Setup, provider options, and model recommendations live in [docs/llm.md](llm.md).

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
- [docs/migration.md](migration.md): internal Aperture Labs operator path
