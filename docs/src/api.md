# API reference

LocalSky exposes a REST + SSE API mounted at **`/api/v1/`** (canonical) and **`/api/`** (legacy alias). New clients should target `/api/v1/*`; the bare `/api/*` paths exist for backwards compatibility with v0.1 and will be removed in a future major release. A few newer endpoint families (`/api/v1/backup`, `/api/v1/updates`) exist only under `/api/v1`.

**On this page**

- [Versioning](#versioning)
- [Authentication](#authentication)
- [Snapshot endpoints](#snapshot-endpoints-read-only)
- [Configuration endpoints](#configuration-endpoints)
- [Wizard endpoints](#wizard-endpoints)
- [Irrigation control endpoints](#irrigation-control-endpoints)
- [Devices](#devices)
- [Sensors and weather history](#sensors-and-weather-history)
- [Web Push endpoints](#web-push-endpoints)
- [Zone photos](#zone-photos)
- [Ingest endpoints](#ingest-endpoints)
- [Health and meta](#health-and-meta)
- [Backup and restore](#backup-and-restore)
- [Service worker and PWA](#service-worker-and-pwa)
- [Client tooling](#client-tooling)

## Versioning

The `/api/v1` namespace is the stable contract. Version semantics:

- **major** (`v1` -> `v2`): breaking change to any response shape or required field. Both versions ship in parallel during the deprecation window.
- **minor**: additive field on a response, or new endpoint. No bump to the path prefix; integrators can rely on extra fields being ignorable.
- **patch**: data-correctness fix with no shape change.

The shape of each `/api/v1/*` GET response is locked at build time by `insta` snapshot tests in `src/api/snapshot_tests.rs`. Any change that mutates the JSON body fails CI until a maintainer acknowledges the diff, which is the moment `api_version` gets bumped.

### `GET /api/v1/info`

Returns the running service version, the API contract version, and the mount prefix. Hit it first when probing a LocalSky instance. Always public, even when authentication is required.

```json
{
  "service": "localsky",
  "service_version": "0.2.0-beta.1",
  "api_version": "1.6.0",
  "api_prefix": "/api/v1",
  "license": "Apache-2.0",
  "repository": "https://github.com/silenthooligan/localsky",
  "dry_run": false,
  "demo": false,
  "auth_required": true,
  "uuid": "1f0a4c2e-9b7d-4e21-a3c5-08d2f6b7e914"
}
```

- `auth_required` tells a client whether it must present credentials before touching anything else. Integration clients (the HACS integration) read this on probe and prompt for an API token.
- `uuid` is the stable per-install id, also broadcast in the mDNS TXT record (`_localsky._tcp.`), so clients can dedupe an instance across IP or hostname changes.
- `dry_run` and `demo` flag instances running with `LOCALSKY_SMART_DRY_RUN=1` or `LOCALSKY_DEMO=1`.

## Authentication

LocalSky ships built-in authentication (API 1.6.0+). It is policy-driven: `[auth] mode = "disabled"` (the default for upgraded installs) leaves every endpoint open, `mode = "required"` gates everything except the public set below. See the [Authentication guide](authentication.md) for setup, accounts, and `trusted_networks`.

### Credentials

When auth is required, the middleware accepts credentials in this order:

1. **`Authorization: Bearer lsk_...`**: a long-lived API token created under Settings, then Account. This is what integrations (HACS, scripts, dashboards) should use.
2. **`?access_token=lsk_...`**: the same API token as a query parameter, accepted **only on paths ending in `/stream`** (browser `EventSource` cannot set headers). It is ignored everywhere else.
3. **Session cookie**: `localsky_session=lss_...`, set by `POST /api/v1/auth/login`. `HttpOnly`, `SameSite=Lax`, marked `Secure` when the request arrived over HTTPS (detected via `X-Forwarded-Proto`). Lifetime is `session_ttl_days`.

Requests from a `trusted_networks` CIDR skip credentials entirely; read [how the client address is determined](authentication.md#x-forwarded-for-and-trusted-networks) before relying on this.

Unauthenticated outcomes: HTML `GET`s are redirected (302) to `/login`; API calls get `401` with body `{"error": "unauthorized"}` and a `WWW-Authenticate: Bearer realm="localsky"` header.

### Public paths

These are exempt from authentication, straight from the middleware's exemption table:

| Path | Why it is public |
|---|---|
| `/pkg/*`, `/sw.js` | Compiled hydration assets and the service worker; browsers fetch these without credentials, so gating them breaks the app |
| Root-level static files (`/favicon.ico`, `/manifest.webmanifest`, and any single-segment path ending in `.svg .png .ico .webmanifest .woff2 .woff .css .js .map .txt`) | Browsers fetch manifests and icons without credentials. Uploaded photos under `/site/photos/*` stay protected |
| `/api/v1/info`, `/api/info` | Pairing probe; carries `auth_required` so clients know to ask for a token |
| `/login`, `/api/v1/auth/status`, `/api/v1/auth/login`, `/api/v1/auth/setup` (and the `/api/auth/*` aliases) | The way in. `setup` only succeeds while zero accounts exist |
| `/ingest/*`, `/api/v1/ingest/*` | Weather hardware (Ecowitt consoles, webhook devices) cannot authenticate. See [what to expose through a proxy](reverse-proxy.md#what-to-expose) |
| `/api/v1/health`, `/api/health` | Always reachable for Docker healthchecks, but anonymous callers get a trimmed liveness-only body (no source, controller, or HA detail) |
| `/setup`, `/setup/*`, `/api/v1/wizard/*`, `/api/wizard/*` | Only until the first account exists, so `docker run` -> browser -> wizard works; locked once setup completes |

Everything else, including every other `/api/v1/*` endpoint, the dashboard pages, and `/site/photos/*`, requires credentials.

### Cross-origin behavior

LocalSky sends no CORS headers, so browsers block cross-origin reads of the API by default; call it from the same origin or from server-side code. Additionally, when auth is required, any non-GET request whose `Origin` header disagrees with the `Host` header is rejected with `403` (CSRF hardening alongside the `SameSite=Lax` cookie). Non-browser clients send no `Origin` header and pass.

### Auth endpoints

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/auth/status` | GET | `{ mode, setup_complete, authenticated }`; always public |
| `/api/v1/auth/setup` | POST | Create the first owner account `{username, password}`; 409 once one exists |
| `/api/v1/auth/login` | POST | Sign in `{username, password}`; sets the session cookie |
| `/api/v1/auth/logout` | POST | Clear the session |
| `/api/v1/auth/session` | GET | Current user (401 when anonymous and auth is required) |
| `/api/v1/auth/tokens` | GET / POST | List / create API tokens (`{name}` -> `{token}`, shown exactly once) |
| `/api/v1/auth/tokens/{id}` | DELETE | Revoke a token |

Login and setup are rate limited to 10 attempts per minute per client address.

## Snapshot endpoints (read-only)

These serve the dashboard's primary data. Both REST (one-shot) and SSE (push-on-change) variants exist for every snapshot type. All SSE feeds emit events named `snapshot` with a keep-alive every 15 seconds.

### `GET /api/v1/snapshot`

Current Tempest weather snapshot, the merged live observation set:

```json
{
  "last_packet_epoch": 1765400000,
  "air_temp_f": 87.2,
  "feels_like_f": 91.4,
  "dew_point_f": 71.3,
  "wet_bulb_f": 75.1,
  "rh_pct": 65.0,
  "pressure_inhg": 30.05,
  "pressure_trend_inhg": [30.02, 30.03, 30.05],
  "wind_lull_mph": 1.2,
  "wind_avg_mph": 4.5,
  "wind_gust_mph": 8.1,
  "wind_dir_deg": 218.0,
  "rapid_wind_mph": 5.0,
  "rapid_wind_dir": 220.0,
  "illuminance_lx": 80500.0,
  "uv_index": 7.5,
  "solar_w_m2": 712.3,
  "rain_in_last_min": 0.0,
  "rain_in_today": 0.0,
  "rain_intensity_in_hr": 0.0,
  "precip_type": 0,
  "lightning_count_last_min": 0,
  "lightning_strikes_last_hour": 0,
  "lightning_recent": [],
  "lightning_avg_dist_mi": 0.0,
  "last_strike_distance_mi": null,
  "last_strike_epoch": null,
  "battery_v": 2.78,
  "battery_pct": 92.0,
  "station_serial": "ST-00012345",
  "hub_serial": "HB-00067890"
}
```

### `GET /api/v1/stream`

Server-Sent Events feed; one event per snapshot mutation. Use from a browser or any SSE client:

```javascript
const es = new EventSource('/api/v1/stream');
es.addEventListener('snapshot', (e) => {
    const snap = JSON.parse(e.data);
    // ...
});
```

External SSE consumers on an auth-required instance append `?access_token=lsk_...`.

### `GET /api/v1/irrigation/snapshot`

Current irrigation state. Top-level fields:

```json
{
  "last_refresh_epoch": 1765400000,
  "ha_reachable": true,
  "tempest_last_seen_epoch": 1765399990,
  "forecast_last_seen_epoch": 1765398000,
  "next_run_epoch": 1765432800,
  "next_run_total_minutes": 62,
  "master_enable": true,
  "iu_enabled": true,
  "iu_suspended": false,
  "water_level_pct": 100.0,
  "zones": [ { "..." : "per-zone status, bucket, planned and last run, math" } ],
  "skip_check": { "...": "today's verdict inputs and result" },
  "forecast": { "...": "the forecast slice the engine used" },
  "seven_day_verdicts": [ ],
  "soil_forecasts": [ ],
  "water_budgets": [ ],
  "pause_until_epoch": 0,
  "override_tomorrow": "none",
  "override_helpers_present": true,
  "decision_trace": { "...": "why the verdict is what it is" },
  "zone_verdicts": [ ]
}
```

### `GET /api/v1/irrigation/stream`

SSE feed for irrigation state. Same event mechanics as `/api/v1/stream` but emits on irrigation-snapshot changes.

### `GET /api/v1/forecast/snapshot`

Daily and hourly Open-Meteo forecast slice currently in use. Returns the source's last successful fetch.

### `GET /api/v1/forecast/stream`

SSE feed for forecast snapshot changes.

### `GET /api/v1/forecast/bias`

The learned per-month forecast bias multiplier, available once enough observations have been recorded.

## Configuration endpoints

Always mounted. Until the wizard writes `/data/localsky.toml`, `GET /api/v1/config` returns the env-compat-synthesized baseline (lat/lon from env vars, default sources, no controllers configured).

### `GET /api/v1/config`

Current config as JSON, with secrets redacted. Every known secret-bearing string (API keys, bearer tokens, controller passwords, and similar) is replaced with the sentinel `***redacted***` on the wire. The PUT handler accepts the sentinel back and preserves the stored value, so a GET-edit-PUT round trip never needs to know the real secrets.

### `GET /api/v1/config/schema`

JSON Schema generated from the Config struct via `schemars`. Use this from any tool that wants to render config forms or validate user input client-side.

```bash
curl http://localhost:8090/api/v1/config/schema | jq '.properties.deployment'
```

### `PUT /api/v1/config`

Replace the entire config. Body is a JSON object matching the schema. The server validates structurally (serde decode) and semantically, snapshots the previous config (retention: last 20 versions), writes `/data/localsky.toml`, and hot-reloads the runtime.

Returns `200` with `{ "saved": <version info>, "validation": <report> }` on success (the report can carry non-blocking warnings); `422` with `{ "error": "config_invalid", "validation": <report> }` on validation failure (the on-disk file is untouched).

```bash
curl -X PUT http://localhost:8090/api/v1/config \
    -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer lsk_...' \
    -d @new-config.json
```

### `GET /api/v1/config/validate`

Structured validation report (errors + warnings) for the config as currently on disk. Returns an empty report with a note when no config exists yet (wizard pending).

### `POST /api/v1/config/preview`

Dry-run validation. Body: `{ "candidate": <Config JSON> }`. Runs validation and returns `{ "ok": true|false, "errors": [...] }` without writing anything. Useful for client-side "validate before save" flows.

### `POST /api/v1/config/rollback?to=<version>`

Restore a previous snapshot. Reachable even when the engine is degraded; use it to recover from a bad config push.

```bash
curl -X POST -H 'Authorization: Bearer lsk_...' \
    'http://localhost:8090/api/v1/config/rollback?to=12'
```

### `GET /api/v1/config/raw` and `PUT /api/v1/config/raw`

Read and write the raw TOML text instead of the JSON projection, for operators who prefer editing `localsky.toml` directly through the Settings raw editor.

## Wizard endpoints

Used during first-run; always mounted, and **public only until the first account exists** (see [Public paths](#public-paths)). The dashboard routes to `/setup` when no `/data/localsky.toml` exists.

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/wizard/draft` | GET / PUT / DELETE | Read, save, or discard the wizard draft |
| `/api/v1/wizard/apply` | POST | Validate the draft and write it as the live config |
| `/api/v1/wizard/state` | GET | Wizard progress state |
| `/api/v1/wizard/seed_current` | POST | Seed the draft from the current live config (re-running the wizard) |
| `/api/v1/wizard/test_source` | POST | `{ "source": <SourceEntry> }`; structural validation of the entry. No live probe per kind yet: receiver sources confirm via live readings on the Sensors hub, polled sources within one cycle after apply |
| `/api/v1/wizard/test_controller` | POST | `{ "controller": <ControllerEntry> }`; live connect + status read. Returns `{ ok, reachable, master_enabled, water_level_pct, zone_count, firmware }`, `502` if unreachable, `422` if unsupported |
| `/api/v1/wizard/test_llm` | POST | `{ "llm": <LlmConfig> }`; live probe of the configured LLM provider |
| `/api/v1/wizard/scan_zones` | POST | `{ "controller": <ControllerEntry> }`; zone discovery for controllers that support it, pre-populates the zone editor |
| `/api/v1/wizard/discover` | GET | One LAN sweep: passive Tempest, Ecowitt broadcast, OpenSprinkler probe |
| `/api/v1/wizard/geocode?q=<address>` | GET | Server-side proxy to Nominatim with the required User-Agent |

`geocode` returns up to 5 candidates:

```json
[
  {
    "display_name": "Orlando, Florida, USA",
    "lat": "28.5383",
    "lon": "-81.3792"
  },
  {
    "display_name": "Cambridge, Cambridgeshire, England, United Kingdom",
    "lat": "52.2053",
    "lon": "0.1218"
  }
]
```

## Irrigation control endpoints

### `POST /api/v1/irrigation/action`

Dispatch a controller action. The body is a tagged enum; shape varies by `kind`:

```json
{ "kind": "run", "zone": "back_yard", "seconds": 600 }
{ "kind": "stop", "zone": "back_yard" }
{ "kind": "stop_all" }
{ "kind": "set_threshold", "key": "max_wind_mph", "value": 12.0 }
{ "kind": "toggle", "key": "irrigation_pause", "on": true }
{ "kind": "set_pause_until", "epoch": 1765500000 }
{ "kind": "clear_pause_until" }
{ "kind": "set_override_tomorrow", "mode": "skip" }
{ "kind": "run_sequence_now" }
```

Notes:

- `run` is clamped server-side to 7200 seconds (2 hours) regardless of what the client sends.
- `set_threshold` accepts only the known keys `max_wind_mph`, `min_temp_f`, `rain_skip_in`.
- `set_override_tomorrow` takes `"none" | "skip" | "run"`.
- `set_pause_until` with `epoch: 0` clears the vacation pause (same as `clear_pause_until`).
- `run_sequence_now` triggers the full irrigation sequence immediately, bypassing the skip-check.

### `GET /api/v1/irrigation/history?days=30`

Run history window, counted backward from now. `days` defaults to 30 and clamps to 1..365.

```json
{
  "from_epoch": 1762808000,
  "to_epoch": 1765400000,
  "runs": [
    { "zone": "back_yard", "start_epoch": 1765320000, "duration_s": 600, "skip_reason": null }
  ]
}
```

Rows with a non-null `skip_reason` are skip events rather than completed runs.

### `GET /api/v1/irrigation/decisions?days=30`

Verdict-transition history: one record per change of the skip-check verdict, so you can answer "did we actually skip on day X, and why" weeks later. Same `days` parameter semantics as `/history`.

### `POST /api/v1/irrigation/simulate`

What-if evaluation of the skip-check against a supplied scenario, without touching hardware.

### `GET /api/v1/irrigation/shadow/snapshot` and `GET /api/v1/irrigation/shadow/diff`

Shadow mode: the native (standalone) snapshot built alongside the Home Assistant one for comparison. Empty unless `shadow_native` is enabled.

### `GET /api/v1/irrigation/explanation`

Latest LLM-generated plain-English explanation of today's verdict. Cached for 5 minutes.

### `GET /api/v1/irrigation/anomalies`

Latest LLM-generated anomaly list. Cached for 1 hour.

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

## Devices

### `GET /api/v1/devices`

Every gateway, hub, controller, and cloud account LocalSky knows about, each with the sensors or zones it provides (the MA-style device view). Sorted by id.

### `GET /api/v1/devices/discover`

Broadcast LAN discovery (Ecowitt gateways today). Listens for about 3 seconds and returns the gateways found, each with a suggested host the UI pre-fills into an `ecowitt_gw_poll` source.

## Sensors and weather history

These endpoints are mounted only when the history database is available (it is, in any normal Docker deployment with `/data` mounted).

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/sensors/soil` | GET | Soil-moisture channels for the zone picker |
| `/api/v1/sensors/discovered` | GET | Every relevant entity LocalSky can see, grouped by role (HA entities as `ha:<entity_id>`, local POST channels as `source:<src>:<key>`) |
| `/api/v1/sensors/manifest` | GET | Declarative entity inventory for the HACS integration |
| `/api/v1/weather/history?hours=24` | GET | Recent observed-weather series (oldest to newest) for the headline fields; powers the dashboard sparklines |
| `/api/v1/weather/readings` | GET | Recent raw readings from the sensor-history table |

## Web Push endpoints

### `GET /api/v1/push/vapid-key`

Public VAPID key for browser subscription. Returns `{ "public_key": "<base64url>" }`, or `503` with `{ "error": "vapid not configured" }` when no keypair is loaded. See [Notifications](notifications.md) for key generation.

### `POST /api/v1/push/subscribe`

Body: the `PushSubscription` JSON from the browser's `pushManager.subscribe()` (`{ endpoint, keys: { p256dh, auth } }`). Idempotent upsert; returns `{ "ok": true }`.

### `POST /api/v1/push/unsubscribe`

Body: `{ "endpoint": "..." }`. Returns `{ "ok": true, "removed": <n> }`.

Both subscribe endpoints return `503` if the history database was not openable at startup.

## Zone photos

### `POST /api/v1/zones/photo`

Multipart upload, field name `file`. Accepts `jpg`, `jpeg`, `png`, `gif`, `webp` up to 10 MB (SVG is rejected because it can carry script). Returns `{ "url": "/site/photos/...", "filename": "..." }`. The served photos under `/site/photos/*` require authentication.

## Ingest endpoints

Push-style sensor receivers. Mounted at `/ingest/*` and `/api/v1/ingest/*`, and **unauthenticated by design** because the posting hardware cannot hold credentials; per-source path secrets are the mitigation. Do not expose these to the internet: see [what to expose](reverse-proxy.md#what-to-expose).

| Endpoint | Method | Purpose |
|---|---|---|
| `/ingest/ecowitt` | POST | Ecowitt console "custom upload" receiver (form-encoded) |
| `/ingest/webhook/{id}` | POST | Generic HTTP webhook receiver for the configured webhook source `{id}` |

Both return `200` on successful parse so misconfigured downstreams do not trigger retry storms on the device.

## Health and meta

### `GET /api/v1/health`

Liveness + readiness, always reachable. Authenticated (or auth-disabled) callers get the full structured body:

```json
{
  "status": "ok",
  "config_present": true,
  "version": "0.2.0-beta.1",
  "schema_version": 1,
  "uptime_s": 1234,
  "subsystems": { "config_store": "ok", "persistence": "ok" },
  "sources": [
    {
      "id": "tempest",
      "kind": "tempest_udp",
      "enabled": true,
      "last_seen_epoch": 1765399990,
      "stale_for_s": 12,
      "status": "fresh"
    }
  ],
  "controllers": [
    { "id": "opensprinkler", "kind": "opensprinkler_direct", "default": true, "enabled": true }
  ],
  "ha": { "env_configured": true, "reachable": true, "snapshot_source": "standalone" }
}
```

Per-source `status` is `"fresh"` (seen within 5 minutes), `"stale"` (5 minutes to 1 hour), or `"offline"` (over 1 hour, or never). On an auth-required instance, **anonymous** callers get a trimmed liveness-only body: no `sources`, `controllers`, or `ha` detail, so Docker healthchecks keep working without leaking topology.

When `config_present` is false the server is in wizard mode; the dashboard redirects to `/setup`.

### `GET /api/v1/updates`

Release check status: `{ current, latest, update_available, release_url, checked_at_epoch, check_enabled }`. The background check only runs when `[updates] check_enabled` is set; otherwise `latest` stays null. When enabled it fetches the project version manifest at `localsky.io/latest.json` daily; the running version travels in the request User-Agent, nothing per-install.

### `GET /api/v1/location`

The configured map center (lat/lon/zoom) for the radar, from `deployment.location` in the config, falling back to the `WEATHER_APP_LAT`/`WEATHER_APP_LON` env vars.

### `GET /api/v1/location/timezone?lat=<lat>&lon=<lon>`

Offline IANA timezone lookup for a coordinate.

## Backup and restore

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/backup` | GET | tar.gz bundle: `localsky.toml` + a consistent copy of the database + manifest. Deliberately excludes the VAPID private key directory |
| `/api/v1/backup/restore` | POST | Multipart restore (`bundle`, or bare `config` / `db`); the database swaps in at next boot |
| `/api/v1/backup/snapshots` | GET | Config snapshot history feeding `POST /api/v1/config/rollback` |

## Service worker and PWA

### `GET /sw.js`

Service worker script. Version interpolated server-side from `CARGO_PKG_VERSION` so every deploy bumps the SW version. Always public.

### `GET /manifest.webmanifest`

PWA manifest. Static and always public.

## Client tooling

A minimal Python client to round-trip the config:

```python
import requests

base = 'http://localhost:8090'
headers = {'Authorization': 'Bearer lsk_...'}  # omit if auth is disabled

cfg = requests.get(f'{base}/api/v1/config', headers=headers).json()
# Secret fields arrive as "***redacted***"; leave them unchanged and
# the server preserves the stored values on PUT.

cfg['engine']['skip_rules']['max_wind_mph'] = 12.0

r = requests.put(f'{base}/api/v1/config', json=cfg, headers=headers)
if r.status_code == 200:
    print('saved', r.json()['saved'])
else:
    print('rejected:', r.json())
```

JavaScript / shell / Rust clients follow the same shape.
