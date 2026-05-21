# API Reference

LocalSky exposes a REST + SSE API mounted at `/api`. Every endpoint is documented here. Use it for tooling, scripts, dashboards, or integrating with anything that speaks HTTP.

## Authentication

LocalSky doesn't enforce auth at the application layer. If you need it, front the service with a reverse proxy (Caddy basic auth, nginx + oauth2-proxy, Cloudflare Access, Tailscale ACLs). The proxy is the right boundary because LocalSky shouldn't store user credentials.

CORS is locked to same-origin by default. Edit `/settings/advanced` to whitelist additional origins if you need cross-origin access.

## Snapshot endpoints (read-only)

These serve the dashboard's primary data. Both REST (one-shot) and SSE (push-on-change) variants exist for every snapshot type.

### `GET /api/snapshot`

Current Tempest weather snapshot. Returns the merged live observation set: air temp, humidity, wind, solar, lightning, rain.

```json
{
  "air_temp_f": 87.2,
  "humidity_pct": 65.0,
  "wind_mph": 4.5,
  "wind_gust_mph": 8.1,
  "wind_bearing_deg": 218,
  "solar_w_m2": 712.3,
  "uv_index": 7.5,
  "pressure_in_hg": 30.05,
  "rain_today_in": 0.00,
  "rain_intensity_in_hr": 0.00,
  "lightning_count_last_3h": 0,
  "battery_volts": 2.78,
  "observed_at_epoch": 1700000000
}
```

### `GET /api/stream`

Server-Sent Events feed; one event per snapshot mutation. Use from a browser or any SSE client:

```javascript
const es = new EventSource('/api/stream');
es.addEventListener('snapshot', (e) => {
    const snap = JSON.parse(e.data);
    // ...
});
```

Keep-alive every 15 seconds.

### `GET /api/irrigation/snapshot`

Current irrigation state: per-zone bucket / running status / last-run / planned next-run, plus the merged verdict and the 7-day forward strip.

```json
{
  "ha_reachable": true,
  "verdict": "run",
  "reason": "",
  "zones": [
    {
      "name": "Back Yard",
      "slug": "back_yard",
      "running": false,
      "bucket_mm": -12.3,
      "planned_run_seconds": 1200,
      "last_run_epoch": 1700000000,
      "math": { ... }
    }
  ],
  "skip_check": { ... },
  "forecast": { ... },
  "seven_day_verdicts": [ ... ],
  "soil_forecasts": [ ... ],
  "water_budgets": [ ... ]
}
```

### `GET /api/irrigation/stream`

SSE feed for irrigation state. Same event shape as `/api/stream` but emits on irrigation-snapshot changes.

### `GET /api/forecast/snapshot`

Daily and hourly Open-Meteo forecast slice currently in use. Returns the source's last successful fetch.

### `GET /api/forecast/stream`

SSE feed for forecast snapshot changes.

## Configuration endpoints

Only mounted when `LOCALSKY_V2=1` is set or `/data/localsky.toml` exists.

### `GET /api/config`

Current Config as JSON. Secrets are not redacted from the JSON wire today (the trade-off documented in [SECURITY.md](../SECURITY.md)); treat the endpoint as you would the on-disk TOML.

### `GET /api/config/schema`

JSON Schema generated from the Config struct via `schemars`. Use this from any tool that wants to render config forms or validate user input client-side.

```bash
curl http://localhost:8090/api/config/schema | jq '.properties.deployment'
```

### `PUT /api/config`

Replace the entire config. Body is a JSON object matching the schema. Server:

1. Validates structurally (serde decode)
2. Validates semantically (unique ids, exactly one default controller, lat/lon in range, etc.)
3. Snapshots the previous config into `config_snapshots` (retention 20)
4. Atomically writes `/data/localsky.toml` (write to .tmp, fsync, rename)
5. Notifies the runtime via the broadcast bus so hot-reload kicks in

Returns 200 + the new `ConfigVersion` on success; 422 + structured error on validation failure (on-disk file untouched).

```bash
curl -X PUT http://localhost:8090/api/config \
    -H 'Content-Type: application/json' \
    -d @new-config.json
```

### `POST /api/config/preview`

Dry-run validation. Body: `{ "candidate": <Config JSON> }`. Server runs the same validation pipeline as PUT but returns the result without writing.

```json
{
  "ok": true,
  "errors": []
}
```

Useful for client-side "validate before save" flows.

### `POST /api/config/rollback?to=<version>`

Restore a previous snapshot. The endpoint is always reachable even when the engine is in a `degraded` state (no enabled sources, no default controller). Use it to recover from a bad config push.

```bash
curl -X POST 'http://localhost:8090/api/config/rollback?to=12'
```

Returns 200 + the restored Config on success; 404 if the version doesn't exist.

## Wizard endpoints

Used during first-run; mounted only when no `/data/localsky.toml` exists (or `LOCALSKY_V2=1` overrides).

### `GET /api/wizard/draft`

Current draft, or a fresh default if none exists. Returns:

```json
{
  "current_step": "welcome",
  "config": { ... },
  "license_accepted": false,
  "telemetry_choice": null,
  "last_updated_epoch": 1700000000
}
```

### `PUT /api/wizard/draft`

Save the draft. Body: a full `WizardDraft` object. Server writes atomically.

### `DELETE /api/wizard/draft`

Clear the draft (cancel + restart the wizard).

### `POST /api/wizard/apply`

Finalize: validate the draft, write `/data/localsky.toml` via the FileConfigStore, drop the draft. The runtime's setup-gate middleware re-mounts normal routes after this returns.

Returns 200 + ConfigVersion on success; 422 + WizardError otherwise.

### `POST /api/wizard/test_source`

Body: `{ "source": <SourceEntry> }`. Attempts a connect + read against the given source. Returns capability + reachability report.

(Stubbed in v0.1; full implementation lands alongside the per-kind adapter in a follow-up.)

### `POST /api/wizard/test_controller`

Body: `{ "controller": <ControllerEntry> }`. Attempts a connect + status read against the given controller.

### `POST /api/wizard/scan_zones`

Body: `{ "controller": <ControllerEntry> }`. For controllers that support discovery (OpenSprinkler, ESPHome), returns the list of detected zones so the UI can pre-populate the zone editor.

### `GET /api/wizard/geocode?q=<address>`

Server-side proxy to Nominatim with the required User-Agent. Returns up to 5 candidates:

```json
[
  {
    "display_name": "Orlando, Florida, USA",
    "lat": "28.5383",
    "lon": "-81.3792"
  }
]
```

## Irrigation control endpoints

### `POST /api/irrigation/action`

Dispatch a controller action. Body shape varies by `kind`:

```json
{ "kind": "run", "zone": "back_yard", "seconds": 600 }
{ "kind": "stop", "zone": "back_yard" }
{ "kind": "stop_all" }
{ "kind": "run_now" }
{ "kind": "set_threshold", "name": "max_wind_mph", "value": 12.0 }
{ "kind": "set_paused", "value": true }
```

Server clamps zone runs to `max_duration_s` (default 7200). Returns 200 on success, 422 with the controller's error otherwise.

### `GET /api/irrigation/history?from=<epoch>&to=<epoch>`

Run history window. Returns up to 1000 rows ordered by start_epoch ASC.

```json
{
  "from_epoch": 1699913600,
  "to_epoch": 1700000000,
  "runs": [
    { "zone_slug": "back_yard", "start_epoch": 1699920000, "duration_s": 600, "skip_reason": null, "status": "completed" }
  ]
}
```

### `GET /api/irrigation/explanation`

Latest LLM-generated plain-English explanation of today's verdict. Cache TTL 5 minutes.

### `GET /api/irrigation/anomalies`

Latest LLM-generated anomaly list. Cache TTL 1 hour. Returns:

```json
{
  "anomalies": [
    {
      "severity": "warn",
      "type": "soil_moisture_drift",
      "description": "Back yard moisture has dropped 18% in 24h, faster than ETc alone predicts."
    }
  ]
}
```

## Web Push endpoints

### `GET /api/push/vapid-key`

Public VAPID key for browser subscription. Returns the key as a base64url string, or 503 if push is not configured.

### `POST /api/push/subscribe`

Body: `PushSubscription` JSON from the browser's `pushManager.subscribe()`. Server stores it.

### `POST /api/push/unsubscribe`

Body: `{ "endpoint": "..." }`. Removes the row.

## Health + meta

### `GET /api/health`

Liveness + readiness. Returns:

```json
{
  "status": "ok",
  "config_present": true,
  "version": "0.2.0-alpha.1",
  "sources_reachable": 2,
  "controllers_reachable": 1,
  "uptime_s": 1234
}
```

When `config_present` is false the server is in wizard mode; the dashboard redirects to `/setup`.

## Service worker + PWA

### `GET /sw.js`

Service worker script. Version interpolated server-side from `CARGO_PKG_VERSION` so every deploy bumps the SW version.

### `GET /manifest.webmanifest`

PWA manifest. Static.

## Client tooling

A minimal Python client to round-trip the config:

```python
import requests, json

base = 'http://localhost:8090'
cfg = requests.get(f'{base}/api/config').json()

# tweak something
cfg['engine']['skip_rules']['max_wind_mph'] = 12.0

r = requests.put(f'{base}/api/config', json=cfg)
if r.status_code == 200:
    print('saved version', r.json()['version'])
else:
    print('rejected:', r.json())
```

JavaScript / shell / Rust clients follow the same shape.
