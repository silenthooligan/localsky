# Troubleshooting

This page is keyed by symptom. Find the thing that looks wrong, follow the steps. When in doubt, start with the first section: almost every problem shows its face in the logs or the health endpoint before it shows anywhere else.

## Logs and health first

### Read the logs

```bash
docker logs -f localsky
```

Log verbosity is controlled by the standard `RUST_LOG` environment variable (the server uses `tracing` with an env filter; if `RUST_LOG` is unset it defaults to `info`). To get engine, source, and controller detail without drowning in HTTP transport noise:

```bash
docker run ... -e RUST_LOG=info,localsky=debug ...
```

Restart the container after changing it.

### Ask the health endpoint

```bash
curl -s http://localhost:8090/api/v1/health | jq
```

What the fields mean:

- `status` is a three-step ladder:
  - `wizard`: no config file exists yet. Visit `/setup`.
  - `ok`: config loaded and every enabled source is reporting.
  - `degraded`: the config file exists but failed to load, **or** at least one enabled source is offline.
- `sources[]`: one entry per configured source with `last_seen_epoch`, `stale_for_s`, and a `status` of `fresh`, `stale`, or `offline`. For live sources (stations, soil sensors) the windows are: fresh under 5 minutes, stale from 5 minutes to 1 hour, offline past 1 hour (or never seen). Polled forecast sources (Open-Meteo, NWS, OpenWeather, Pirate Weather, MET Norway, Netatmo) refresh on a roughly 30 minute cadence, so they get wider windows: fresh under 65 minutes, offline past 3 hours.
- `controllers[]`: id, kind, whether it is the default, and whether it is enabled.
- `ha`: the Home Assistant relationship in both directions: `env_configured` (HA_URL set), `reachable` (last HA poll succeeded), `snapshot_source` (`standalone` or `home_assistant`), `mqtt_discovery` (outbound MQTT publishing on), `hacs_last_seen_epoch` and `hacs_streaming` (whether the Home Assistant integration has fetched the manifest or is holding a live event stream right now).

If authentication is enabled and you call `/api/v1/health` without credentials, you get a trimmed body: `status`, `config_present`, `version`, `uptime_s`, and `subsystems` only. Sources, controllers, and the `ha` block are removed so an anonymous probe cannot map your network. Docker healthchecks and uptime monitors keep working either way.

### Compose healthcheck

The image ships a built-in `HEALTHCHECK` that curls `http://127.0.0.1:8090/api/v1/info` every 30 seconds. If you move LocalSky off port 8090, override it in compose:

```yaml
services:
  localsky:
    # ...
    healthcheck:
      test: ["CMD", "curl", "--fail", "--silent", "--max-time", "4", "http://127.0.0.1:8091/api/v1/info"]
      interval: 30s
      timeout: 5s
      start_period: 30s
      retries: 3
```

`/api/v1/info` is the cheapest liveness probe. Use `/api/v1/health` instead if you want your monitor to alert on `degraded`, not just on dead.

## Install and first boot

### Container exits immediately with a bind error

The log will end with a line like:

```
bind 0.0.0.0:8090: is another service holding this port?
```

Something else on the host already owns the port. Either free it, or move LocalSky:

```bash
docker run ... -e LEPTOS_SITE_ADDR=0.0.0.0:8091 -p 8091:8091 ...
```

This bites most often with `network_mode: host`, where the container shares the host's port space directly (no `-p` remapping is possible). Pick a free port via `LEPTOS_SITE_ADDR` and remember to override the healthcheck (above).

### Wizard cannot save, or history is missing, with permission errors in the logs

The app runs as the non-root user uid 10001, and the container fixes the ownership of `/data` to that user on every startup, so a normal bind mount or named volume needs no manual `chown`. If you still see permission errors (the wizard cannot save `localsky.toml`, or history is disabled with a logged SQLite open failure), the cause is almost always one of:

- **`/data` is mounted read-only.** The container cannot fix ownership of, or write to, a read-only mount. Mount `/data` read-write.
- **You overrode the entrypoint** (for example `user: "0:0"` plus a separate non-root step, or a custom `entrypoint:`). Remove the override and let the image manage the privilege drop.

As a last resort you can pre-own the host directory yourself: `sudo chown -R 10001:10001 /opt/localsky/data`.

### Low-power hardware

- Raspberry Pi 4/5: the image ships arm64, but the OS must be 64-bit. `uname -m` should report `aarch64`. 32-bit Pi OS is not supported.
- LocalSky idles around 30 MB resident, so nothing special is needed beyond that. The SQLite database sees light write traffic (run rows, sensor samples), which is fine on an SD card, though an SSD never hurts.

## Weather sources

### Tempest station shows no data

The Tempest hub broadcasts UDP packets on port 50222 to your LAN's broadcast address. Docker's default bridge networking does not deliver broadcast traffic into a container, so a bridge-networked LocalSky never hears the hub even though everything looks configured. Run with host networking:

```yaml
services:
  localsky:
    network_mode: host
```

To confirm packets are actually arriving on the host:

```bash
sudo tcpdump -i any -c 3 udp port 50222
```

If tcpdump sees packets and LocalSky still shows nothing, check the source is enabled under Settings, then Sources, and watch `docker logs` for parse errors.

### Ecowitt discovery finds nothing

Discovery works by sending a broadcast datagram on UDP 46000 and listening about 3 seconds for gateway replies. Two requirements:

1. Host networking (same broadcast limitation as Tempest above).
2. The gateway must be on the same subnet as the LocalSky host.

If discovery still comes back empty, skip it and add the gateway manually: create an `ecowitt_gw_poll` source under Settings, then Sources, and enter the gateway's IP address. Alternatively, point the gateway's own custom upload (WSView Plus or the console UI) at LocalSky's receiver: protocol Ecowitt, path `/ingest/ecowitt`, your LocalSky host and port.

### A source went stale: what happens to watering?

Nothing dramatic, by design. When an enabled source crosses the offline threshold:

- `/api/v1/health` flips to `degraded`.
- A dismissable banner appears at the top of the UI naming the offline source(s), with a link to the Sensors hub. Dismissing it snoozes that exact set of sources for the session; a new failure re-raises it.
- The engine keeps deciding from the freshest data it has. Field merging picks the highest-priority source with a recent observation (ties broken by recency), and rain totals take the max across sources so one dead gauge cannot mask real rain. Sensor-dependent extras (soil-saturation skip, for example) sit out while their probe is silent; the weather and ET math stays on.

## Controllers

### Controller was offline when watering should have started

Runs do not queue. When the morning scheduler dispatches a zone and the controller call fails, LocalSky logs a warning (`smart morning: controller dispatch failed`), abandons the rest of that zone's segments, and moves on to the next zone. There is no retry later in the day; the next attempt is tomorrow's window. Check `docker logs` around your dispatch time and fix the controller's reachability (power, IP change, password).

Different case: if **LocalSky itself** was down through the morning window, it catches up at boot. Within a 2 hour grace period after the planned finish time it dispatches a late run (if the verdict is still "run"); past that, it records a skipped row with the reason "Missed dispatch window (LocalSky offline)" so the history stays honest.

### Zone is running but the dashboard disagrees (or vice versa)

The dashboard's view of controller state comes from a poll loop that refreshes roughly every 10 seconds (with backoff during outages), so a few seconds of lag is normal. If the disagreement persists:

- Check the `controllers` block in `/api/v1/health`: is the controller enabled, and is one marked `default`?
- Runs started from the controller's own app or front panel show up via the status poll, but they were not planned by LocalSky and may not appear in its run history the way engine-dispatched runs do.

### Verify wiring with the DryRun controller

Before trusting a new setup with real valves, add a controller of kind `dry_run`. Every dispatch is logged (`dry_run: would have run zone ...`) instead of actuated, and with `simulate_runs` enabled it writes completed rows to the runs table so the dashboard and history render exactly as they would for real hardware. The wizard's zone scan against a DryRun controller returns sample zones (Front Lawn, Back Lawn, Garden Beds) so you can rehearse the full add, test, scan, import flow with zero hardware. See [Controllers](controllers.md).

## Watering decisions

### Why did my zone skip today?

Every skip is recorded per zone with its reason. Open the zone's skip breakdown in the UI, or look at the run history. The full explanation of each threshold lives in [Skip thresholds explained](skip-breakdown.md), and the reporting views in [History and reporting](history.md).

### Lots of skips in the first week

Expected. Each zone's soil bucket starts full (zero depletion, soil assumed at field capacity). The engine will not water until evapotranspiration draws the bucket down past the allowed depletion for your soil and species, which typically takes days. If you know the soil is actually dry on day one, run the zones manually once; the engine accounts for the applied water and the model converges from there.

## Auth and reverse proxy

### Locked out of the owner account

Short version (full procedure in [Authentication](authentication.md)): stop the container, delete the identity rows from the SQLite database, restart, and re-run account creation:

```bash
sqlite3 /opt/localsky/data/irrigation.db \
  "DELETE FROM auth_sessions; DELETE FROM api_tokens; DELETE FROM users;"
```

Physical access to the data volume is the trust anchor, same as Home Assistant.

### Page loads but is frozen: nothing clicks, behind a proxy auth gate

Classic symptom of an external auth gate (oauth2-proxy, Authelia, Caddy forward_auth) swallowing the app's compiled assets. Browsers fetch `/pkg/*` (the WASM bundle) and `/sw.js` (the service worker) without credentials, the gate answers with a 302 to its login page instead of the file, and hydration dies silently: you see server-rendered HTML, but no JavaScript behavior. Exempt `/pkg/*` and `/sw.js` from the gate. Examples in [Reverse proxy and HTTPS](reverse-proxy.md). LocalSky's own built-in auth already exempts these paths.

### Home Assistant integration logs 401s

The API token it was given has been revoked or replaced. The integration starts its reauthentication flow automatically on the next 401: Home Assistant raises a repair/reauth prompt. Create a fresh token in LocalSky (Settings, then Account, then Create token) and paste it into the prompt. Tokens are shown in plaintext exactly once.

## Home Assistant

### No LocalSky entities in HA

- The integration is installed via HACS as a custom repository; if you only installed HACS itself, the LocalSky integration is not there yet. See [Home Assistant integration](hacs.md).
- The config flow needs a reachable LocalSky URL and, on auth-enabled instances, an API token (`lsk_...`).
- Zeroconf discovery (the config flow finding LocalSky by itself) relies on LocalSky's mDNS announce (`_localsky._tcp`), which only reaches the LAN when LocalSky runs with host networking. With bridge networking, just enter the URL manually.

### Duplicate entities

You have both publishing paths on at once: MQTT discovery (LocalSky publishing to your broker) and the HACS integration (HA polling LocalSky) each create their own set of `localsky` entities. Pick one. To keep the integration, turn off MQTT publishing under Settings, then Notifications, and delete the leftover MQTT device in HA (Settings, Devices & Services, MQTT).

### Entities unavailable, but LocalSky is still watering

Expected, and it is the point of standalone operation: the engine and scheduler run inside LocalSky and do not depend on HA being up. Unavailable entities only mean HA cannot currently see LocalSky's state. The one exception is controllers of kind `ha_service_call`, which dispatch through HA and do need it reachable.
