# Troubleshooting

Start with the failing component and its error detail. A running server can still have an unavailable source, an unbound zone, or a controller that did not complete a command.

## Capture useful evidence

Open **Settings > About** for the version and inspect the relevant device status. For Docker:

```bash
docker logs --since 15m localsky
curl -i http://localhost:8090/api/v1/info
```

Use `GET /api/v1/health` for health detail; anonymous responses are reduced. `?strict=1` returns 503 when overall status is not healthy. Supply a bearer token for privileged diagnostics.

For a report, include the action, timestamp and timezone, LocalSky version, installation type, stable error code, and request ID. The diagnostics endpoint collects health, recent logs, redacted configuration, and decision context. Review the bundle for personal addresses and identifiers before sharing it.

[API error reference](api-errors.md) · [Source error codes](source-errors.md)

## Home Assistant returns HTTP 500

A 500 from HA's `/api/states` is an upstream failure, not proof that a mapped entity is missing. LocalSky 0.9.2 can fall back to reading individually mapped entities within the same request deadline.

If the mapped reading now works, the fallback recovered that input. It does not identify or repair the cause of HA's bulk-endpoint failure. Check HA's logs at the same timestamp for the exception and integration involved. Include LocalSky's operation, upstream status, error code, and request ID when reporting continued failures.

For a single unavailable entity, verify the exact entity ID, its current state, unit, and mapping in LocalSky. Do not map a preceding-minute rain reading as a daily total; use **Rain last minute (accumulate today)**.

## Tempest has no readings

The local Tempest path listens for UDP broadcasts on port 50222. Confirm the source is enabled, the hub is on a reachable broadcast network, and Docker uses host networking where necessary.

If HA already owns the local listener on the same host, use [HA passthrough](hacs.md#use-home-assistant-weather-sensors) or move ownership deliberately. Disabling or removing LocalSky's native source releases its listener within about 15 seconds.

A cloud Tempest connection is a separate path with its own credentials and internet requirement.

## Ecowitt discovery finds nothing

Local discovery needs broadcast reachability to the gateway. Enter the gateway IP manually for a polling source if discovery cannot cross your network. For custom uploads, verify the destination host, port, protocol, and path against [sensor setup](sensors.md).

## Watering did not run

Open **History > Daily log** for the recorded decision, then **Watering decisions** for current evidence. A missing historical record is not a recorded skip.

Check the zone's binding, pause and override state, permitted watering days, forecast coverage, soil evidence, and controller health. A configured probe that is missing or untrusted can hold its zone. Missing required forecast evidence can hold automatic watering.

A request being accepted does not establish that a physical valve opened. Verify device feedback and the run outcome. See [controllers](controllers.md) and [skip reasons](skip-breakdown.md).

## Water ran despite rain later

Compare the recorded decision with what was known at dispatch: recent measured rain, earlier watering, soil demand, and the forecast issued before the run. Later rainfall does not by itself prove that the earlier decision ignored rain.

The forecast archive preserves received forecasts; history preserves decisions and runs. Use both to distinguish a forecast miss from stale input, missing history, an incorrect zone rate, or a planning defect. Include those timestamps in a report.

## A valve may still be open

Use **Stop** and verify the valve physically. If LocalSky cannot reach the controller, use the controller's own stop or shut off the water supply. Preserve the command error and controller logs for diagnosis.

Controller timers and LocalSky's shutoff retries reduce risk, but a successful network response cannot guarantee a mechanically closed valve.

## Setup cannot save

Check that `/data` is persistent and writable. The container normally runs as uid 10001 and prepares the volume at startup. NAS mappings may require explicit `PUID` and `PGID` matching the share owner. Inspect the startup error before changing permissions; do not make the entire volume world-writable.

## Port already in use

Change the host port mapping for bridge networking. With host networking, change `LEPTOS_SITE_ADDR` to a free listen port and update any healthcheck or client URL that refers to it.

## Login or live updates fail through a proxy

Check HTTPS forwarding, `trusted_proxies`, and SSE buffering. Test from a signed-out browser. Follow the matching examples in [reverse proxy setup](reverse-proxy.md).

## Restore or startup failed

Preserve the data directory and the full startup error. Repeated restarts do not repair an incomplete restore. Follow [backup and recovery](backup-restore.md#a-restore-was-interrupted); do not delete the marker to force startup.
