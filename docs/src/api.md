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
- [Radar map data](#radar-map-data)
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

### Migration notes

**1.24.0** (a zone binds to a controller zone by id). Additive only; no existing response shape changes. `ZoneConfig` gains `controller_zone_name` in `GET`/`PUT /api/config` and the config schema: the controller's own name for the bound zone (`null` when the binding was typed by hand or predates this release). It is a **display label**. Nothing dispatches on it, nothing keys on it, and a stale value cannot mis-actuate. `controller_station` keeps its shape and its meaning and additionally becomes `#[serde(default)]`, so a hand-written config that omits it parses instead of failing.

The behavior change is in what fills `controller_station`. It is now the binding a user picks, and a controller's own `zone_*_map` is the fallback that keeps a pre-existing config watering unchanged. On load, LocalSky copies a map entry into any zone of that controller whose `controller_station` is empty and whose slug the map covers, so an install bound only through the map ends up with the same binding visible on the zone. It never overwrites a non-empty station and never rewrites or removes the map. It is idempotent, and the next config write persists the copied value.

`ha_service_call` now reads `controller_station` and overlays it onto `zone_entity_map`, which it never did before: an entity id in that field used to be silently ignored. A value that is not entity-id shaped (`domain.object_id`) is warn-skipped rather than sent to Home Assistant, so the legacy v0.1 station numbers stay inert. `mqtt_command` is explicitly exempt (its per-zone value is a command struct a string cannot carry) and `esphome_native` still builds nothing.

New validation warning `zone_unbound`: a zone whose controller exists, whose station is empty, and whose controller's zone map has no entry under its slug. A **warning**, never an error, so no previously loadable config becomes unloadable. It is honest per kind rather than uniform: `mqtt_command` ignores the station field entirely, so only its `zone_command_map` counts; `dry_run` accepts any slug and is never unbound; and `esphome_native` reports the separate `zone_controller_not_built` warning, because its adapter is not constructed and neither binding fires.

Its companion `zone_station_unparseable` covers the case that looks bound and is not: `controller_station` holds a value the controller kind cannot use (a Rachio UUID on a Hydrawise zone, a station number on a Rachio zone, an OpenSprinkler `0`, a bare number where Home Assistant needs an entity id), and no zone map entry covers the zone either. Dispatch already ignored such a value; now the config check names it, says what that kind expects, and stops counting the zone as bound. The check calls the same per-kind parsers `build_controllers` binds with, so it cannot disagree with dispatch. A zone whose controller map still covers it keeps watering and is not reported.

BOTH whole-config write paths, `PUT /api/config` and `PUT /api/config/raw`, now refuse a save that drops a zone key and adds another in the same write with `422 zone_key_renamed`, because a zone's slug keys its history, its overrides, its in-flight run ledger, its tuning dismissals, its soil channel, its Home Assistant entity ids and its retained MQTT discovery topics. When the removal and the addition really are unrelated zones, either save them as two separate writes or repeat the one write with `?allow_zone_key_change=1` (`=true`, `=yes`, `=on`, and a bare `?allow_zone_key_change` are accepted too). The unknown-zone `400` body's `hint` no longer suggests renaming a zone to match a map key; it points at the zone's **Controller station** field and says why renaming is not the fix.

An operator upgrading a Home Assistant install should check the boot log once. A zone that carries BOTH a station value and a `zone_entity_map` entry now dispatches to the station value, and the log names every zone whose target moved.

**No HACS integration change is required.** The integration reads `/api/v1/info`, the sensor manifest, the streams, the action endpoint and the session endpoint, and gates on the API **major** only. It never reads `/api/config`, so an additive `ZoneConfig` field is invisible to it.

**1.23.0** (controller failures become distinguishable). `POST /api/v1/irrigation/action` used to answer `502 Bad Gateway` for every controller failure except an unknown zone (`400`) and an unsupported operation (`501`), so a rejected credential, an exhausted daily request budget, a vendor rejecting the zone id, and a transport failure were one indistinguishable status. The mapping is now `424 Failed Dependency` for a rejected *controller* credential, `429` for a rate-limited controller, `400` for an unknown zone, `501` for an unsupported operation, and `502` for the failures that really are upstream or transport (the controller returned an error, the request never completed, the controller is offline, or the adapter failed to initialize). **Not `401`:** on every LocalSky endpoint `401` means *this deploy's* authentication failed, and clients act on it accordingly (the HACS integration starts a reauthentication flow), so a revoked vendor key must never borrow that status. **Branch on `code`, not on the status.** Every error body now carries a stable `code`: `zone_unknown`, `controller_auth_failed`, `controller_rate_limited`, `controller_unsupported`, `controller_unreachable`. A client that treats any non-2xx as a failure needs no change; one that pinned `502` as *the* controller-failure status must widen to the set above. No HACS integration change is required. Additive alongside it: the `429` body carries `rate_limit_remaining` (what the controller's **last response** reported, as a string, `null` when it reported none, never a zero standing in for unknown); the `400` body carries `mapped_zones`, the controller's zone-map keys, so a zone-slug mismatch is diagnosable from the error alone; the `400`, `424`, and `429` bodies carry a `hint` naming the fix; the unknown-zone `error` text now reads `zone "<slug>" is not mapped to a zone on this controller` instead of `zone unknown: <slug>` (it was never a parseable code); and a successful `run`, `stop`, or `stop_all` response gains `confirm_within_s`, how many seconds that controller can take to *report* the change (`null` when it reads state on demand), so a client can say a change was accepted and confirmation is still pending rather than implying a dispatch failed when its own confirmation window is shorter than the controller's poll interval.

**1.22.0** (Rachio first-class). Additive only; no existing response shape changes. `RachioConfig` gains `poll_interval_s` (seconds between live status polls against the Rachio cloud, `60..=3600`, `null` = the 120s default; values outside the band fail validation) and `base_url` (`null` = the production endpoint) in `GET`/`PUT /api/config`. `POST /api/v1/wizard/test_controller` for a `rachio` entry adds `discovered_device` (`{ device_id, name, device_count }` when the posted entry carried an API token but no device id, so the form can offer the resolved id; `null` otherwise) and `rate_limit_remaining` (the cloud's reported remaining daily request budget, `null` when the header was absent). `POST /api/v1/wizard/scan_zones` and `test_controller` now restore redacted-secret sentinels from the stored config by entry id (the `PUT /api/config` pattern) and answer `400 unmatched_redacted_secret` when no stored value matches, so probing an existing cloud controller works without retyping its secret. When a stored secret is restored, the probe's transport fields (`base_url`, `host`, ports) are pinned to the stored entry's values; a probe that changes them alongside a redacted secret answers `400 transport_field_mismatch` (save the address change first, or re-enter the secret). The manual `stop` action's response gains `scope` (`"zone"` or `"device"`) plus a `note` when the controller has no per-zone stop and the whole device was stopped. The new `ControllerCaps.per_zone_stop` bit is internal (capability structs never ride the wire).

**1.21.0** (weekly water balance). Additive only; no existing response shape changes. Each `water_budgets[]` row in the irrigation snapshot gains the settled balance terms: `observed_rain_mm` + `observed_rain_source` (`gauge` | `radar` | `model_archive` | `none`), `applied_mm` (gross irrigation over the trailing 7 days, union-clustered watering evidence), `forecast_credit_mm` + `forecast_credit_source` (`bias_forecast` | `none`), `bias_multiplier` + `bias_sample_count` (the current month's forecast-bias correction; 1.0 with the sample count when under-trained), and `remaining_sessions`. The existing fields keep their meaning: `today_seconds` is still the actual seconds to water today (the external HA automation contract), `expected_rain_mm` keeps its historical wire scaling (probability-weighted 7-day forward forecast in mm times the 0.7 capture factor; informational, the balance itself subtracts only the bias-corrected credit up to the next session), `needed_mm` is now the balance remainder, and `mm_per_session` is the per-remaining-session gross depth. Session sizing no longer multiplies by the heat multiplier or divides by capture efficiency. `GET /api/v1/irrigation/history` run rows gain `source` and `status`. The tuning report's `zones[]` gain `dismissed` + `dismissed_fields`, and two privileged endpoints manage silencing: `POST /api/v1/irrigation/tuning/dismiss` `{zone_slug, field, recommendation_id, kind: "snooze" | "permanent"}` (a snooze keys the exact recommendation id and expires after 30 days; a permanent dismissal keys the zone + field and survives value drift) and `POST /api/v1/irrigation/tuning/undismiss` `{zone_slug, field}`. A dismissed or snoozed suggestion is stripped inside the report's ranked pick server-side (the zone's next-ranked suggestion, if any, surfaces instead), so counts and the weekly push go quiet with it. The `open_meteo` source's `past_days` config is now honored by the fetch (clamped 1..=7; default 3), and the `forecast_observations` ledger records each day's observed rain as a day-max with an `observed_source` tag.

**1.20.0** (per-zone run limit). Additive only. `ZoneConfig` gains `max_run_minutes` (whole minutes, `5..=360`, `null` = the 60 minute default) in `GET`/`PUT /api/config` and the config schema; the value hot-reloads on save (no restart). The tuning report can now recommend `max_run_minutes` (its `suggested_value` is in minutes), and `POST /api/v1/config/zones/apply` accepts the field alongside the existing set (`soil_texture`, `precip_rate_mm_hr` plus `precip_rate_source`, `root_depth_mm`, `mad_pct_override`, `weekly_budget_in`, `sessions_per_week`); out-of-band values answer `422`. A config write that raises a zone's limit past 60 minutes emits a Web Push notice (tag `cap-raised-<slug>`, deep link `/zones/<slug>`) to subscribed devices after the save. No existing response shape changes.

**1.19.0** (tuning report). Additive only; no existing response shape changes. `GET /api/v1/irrigation/tuning?days=N` (clamp 7..=30, default 14) returns the per-zone results-based tuning report: `{ generated_epoch, window_days, zones: [ { slug, display_name, status, lines, recommendation } ], scorecard }`, at most one recommendation per zone, each carrying the target config field, current and suggested values as JSON (`null` clears an override), companion fields the apply writes alongside (a measured precipitation rate also stamps `precip_rate_source`), the plain-language headline, the evidence lines, and a stable `id`. The scorecard's `scored_days` / `confirmed_days` cover forecast rain skips and are **null** until at least 3 such days could be judged; reactive rain skips (rain already falling or on the ground) ride the additive `reactive_days` / `reactive_line` as a plain count (the 1.18.0 honest-unknowns register; never a zero sentinel). `POST /api/v1/config/zones/apply` writes one recommendation through the validated config path; it is privileged like every config write, regenerates the recommendation server-side, and answers `409` when the supplied `id` no longer derives from current data. Like the other history reads, `/irrigation/tuning` mounts only when the history database is available. No HACS integration change is required.

**1.18.0** (honest unknowns). Several fields whose zero doubled as "no data" are now **nullable**, and a handful of manifest entities are capability-gated. Nullable (each still always present; `null` is the documented unknown value, and a client that treated the old `0` as a real reading was already reading a defect): `tempest.pop_pct` and `tempest.leaf_wetness_pct` (null until a configured source writes them), `irrigation.water_level_pct` (null when the controller does not report a level; previously a fabricated `100` on native installs and `0` on HA installs with no entity), `forecast.eto_today_mm` (null when no source/forecast/native compute produced one; the flat `5.0` fallback no longer publishes), `forecast.temp_max_today_f` / `temp_min_today_f` / `humidity_mean_today_pct` (now resolved from the live forecast first, legacy HA sensors second, null when neither exists), and the precipitation probabilities (`skip_check.rain_tomorrow_prob_pct`, `forecast.rain_tomorrow_prob_pct`, `seven_day_verdicts[].precip_probability_max`), which are null when the forecast provider reports no probability series; the probability-weighted rollups now take probability-less rain at full value instead of zeroing it. Additive alongside these: `irrigation.water_level_capable` (whether the active controller reports a water level). Manifest schema is **1.4**: `pop_pct`, `wet_bulb_f`, `wind_lull_mph`, `rain_in_last_min`, `illuminance_lx`, `water_level_pct`, and the per-zone soil moisture/temperature/EC/battery descriptors now publish only when the install actually has the backing source, station, controller capability, or soil probe, so installs without the hardware stop growing dead entities. The HACS integration already renders `null` as unavailable; no integration change is required.

**1.17.0** (lightning). `tempest.lightning_avg_dist_mi` is now **nullable**: it is `null` whenever the reporting interval detected no strikes, where it previously carried the station's bare `0`. On a distance channel that `0` read as a strike directly overhead, so a client filtering on `distance < 10` saw a phantom storm between strikes, and the obvious guard `distance > 0` dropped real readings. The field is still always present, and `null` is the documented unknown value. If you want a distance that persists between strikes, read the new `last_strike_distance_mi` (also exposed as a sensor descriptor in the manifest) instead of the interval average. Separately, `lightning_strikes_last_hour` now decays as strikes age out of the hour rather than holding the last storm's total until the next strike; a trigger of the form "strikes above 0" re-arms on its own once it reaches 0.

### `GET /api/v1/info`

Returns the running service version, the API contract version, and the mount prefix. Hit it first when probing a LocalSky instance. Always public, even when authentication is required.

```json
{
  "service": "localsky",
  "service_version": "0.7.0",
  "api_version": "1.15.0",
  "api_prefix": "/api/v1",
  "license": "Apache-2.0",
  "repository": "https://github.com/silenthooligan/localsky",
  "dry_run": false,
  "demo": false,
  "auth_required": true,
  "uuid": "1f0a4c2e-9b7d-4e21-a3c5-08d2f6b7e914",
  "has_irrigation": true,
  "nerd_mode_default": false
}
```

- `auth_required` tells a client whether it must present credentials before touching anything else. Integration clients (the HACS integration) read this on probe and prompt for an API token.
- `uuid` is the stable per-install id, also broadcast in the mDNS TXT record (`_localsky._tcp.`), so clients can dedupe an instance across IP or hostname changes.
- `dry_run` and `demo` flag instances running with `LOCALSKY_SMART_DRY_RUN=1` or `LOCALSKY_DEMO=1`.
- `has_irrigation` is true when any controller or zone is configured; a weather-only install reads false, and the UI hides the irrigation navigation on it.
- `nerd_mode_default` is the server-configured `features.nerd_mode_default`; the UI seeds Simple vs Nerd presentation from it.

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
| `/metrics` | Prometheus exposition endpoint. Aggregate operational counters only (verdict mix, refresh and degraded counts, controller/cloud error counts, last-fetch latency); no secrets, config, or PII. Firewall it at the proxy if you do not want it public |
| `/docs/*` | The bundled handbook (served from the image), so in-app help works pre-login and on a fresh install. Static pages, no secrets |
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

These serve the dashboard's primary data. Both REST (one-shot) and SSE (push-on-change) variants exist for every snapshot type. All SSE feeds emit events named `snapshot`. The weather (`/api/v1/stream`) and irrigation (`/api/v1/irrigation/stream`) feeds send a keep-alive every 15 seconds; the forecast feed (`/api/v1/forecast/stream`) sends one every 30 seconds.

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
  "lightning_avg_dist_mi": null,
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

### `GET /api/v1/config/snapshots`

The on-disk config snapshot history, newest first. Every save snapshots the previous `localsky.toml` (newest 20 kept). Returns `{ "snapshots": [ { "ts", "applied_at_epoch", "schema_version", "note" }, ... ] }`. (`GET /api/v1/backup/snapshots` returns the same history.)

### `POST /api/v1/config/rollback`

Restore a previous snapshot. Body `{ "ts": <snapshot ts> }` (the legacy `?to=<ts>` query is also accepted). The snapshot is validated before the swap, the current config is snapshotted first so the rollback is itself reversible, and the restored config hot-reloads. Reachable even when the engine is degraded; use it to recover from a bad config push.

```bash
curl -X POST -H 'Authorization: Bearer lsk_...' \
    -H 'Content-Type: application/json' \
    -d '{"ts": 1765400000}' \
    http://localhost:8090/api/v1/config/rollback
```

### `POST /api/v1/config/zones/apply`

Write one [tuning report](#get-apiv1irrigationtuningdays14) recommendation through the validated config path. Body: `{ "zone_slug", "recommendation_id", "field", "value", "window_days" }`, echoing the recommendation as served. `window_days` is the window the report was fetched at (clamped 7..30; absent = the default 14): the server re-derives the zone's recommendation at that window against the exact config it is about to mutate, inside the config write lock, and answers `409 { "error": "stale_recommendation" }` when the claim no longer derives, so a stale page can never write an outdated value. A client viewing a non-default window MUST echo the report's `window_days` or its applies can 409 indefinitely. Companion fields ride server-side (a measured `precip_rate_mm_hr` also stamps `precip_rate_source = "measured"`). The mutation runs the same validation as `PUT /config` (`422` with the structured report on failure), snapshots the previous config, saves, hot-reloads the runtime, and returns `{ "applied", "zone", "field", "old_value", "new_value", "saved", "validation", "restart_required", "restart_reasons" }`. Privileged like every config write.

```bash
curl -X POST http://localhost:8090/api/v1/config/zones/apply \
    -H 'Content-Type: application/json' \
    -H 'Authorization: Bearer lsk_...' \
    -d '{"zone_slug":"back_yard","recommendation_id":"8f2c41a09b6d13e7","field":"precip_rate_mm_hr","value":18.0,"window_days":14}'
```

### `GET /api/v1/config/raw` and `PUT /api/v1/config/raw`

Read and write the raw TOML text instead of the JSON projection, for operators who prefer editing `localsky.toml` directly through the Settings raw editor.

### `GET /api/v1/config/field_sources`

The dataset behind the Data sources page: the user-facing fields with a per-field picker (`user_fields`), every enabled source with the fields it can provide plus its tier (`device` / `cloud`), data nature, and region priority (`sources`), the saved per-field pins and ordered chains (`overrides`, `field_source_chains`), the forecast-capable candidates and the saved pin (`forecast_candidates`, `forecast_provider`), and a `region_label` for the "Automatic (region default)" tag. A field absent from both `overrides` and `field_source_chains` uses the automatic region order (sort that field's candidates by `region_priority` descending). This is the read side of the chain editor; writes go through the normal config PUT.

### `GET /api/v1/config/source_catalog`

The honest cloud-source catalog behind the cloud weather panel: one entry per cloud weather kind (highest honesty first), each carrying the static facts (data nature per field, key tier, real-time / localization / watering-risk copy, honesty and irrigation ranks), the live current-field list, region recommendation flags, whether the kind is already configured, and a live `status` computed by the same taxonomy as [`/api/v1/health`](#get-apiv1health) (`active` / `watching` / `standby` / `falling_through` / `offline`). Top-level shape: `{ "lat": ..., "lon": ..., "cloud_sources": [ ... ] }`.

## Wizard endpoints

Used during first-run; always mounted, and **public only until the first account exists** (see [Public paths](#public-paths)). The dashboard routes to `/setup` when no `/data/localsky.toml` exists.

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/wizard/draft` | GET / PUT / DELETE | Read, save, or discard the wizard draft |
| `/api/v1/wizard/apply` | POST | Validate the draft and write it as the live config |
| `/api/v1/wizard/state` | GET | Wizard progress state |
| `/api/v1/wizard/seed_current` | POST | Seed the draft from the current live config (re-running the wizard) |
| `/api/v1/wizard/test_source` | POST | `{ "source": <SourceEntry> }`; structural validation of the entry. No live probe per kind yet: receiver sources confirm via live readings on the Sensors hub, polled sources within one cycle after apply |
| `/api/v1/wizard/test_controller` | POST | `{ "controller": <ControllerEntry> }`; live connect + status read. Returns `{ ok, reachable, master_enabled, water_level_pct, zone_count, firmware }`, `502` if unreachable, `422` if unsupported. Rachio entries add `discovered_device` (the account's first device, resolved when the entry has a token but no device id) and `rate_limit_remaining`; redacted secrets in the posted entry are restored from the stored config by entry id (`400 unmatched_redacted_secret` when unresolvable) |
| `/api/v1/wizard/test_llm` | POST | `{ "llm": <LlmConfig> }`; live probe of the configured LLM provider |
| `/api/v1/wizard/scan_zones` | POST | `{ "controller": <ControllerEntry> }`; zone discovery for controllers that support it. Returns `{ "zones": [ { "station_id", "name" } ] }`. Callers use it to offer the controller's own zones as choices: the zone editor's station picker, the controller editor's bind table, and the setup wizard's zone import. A redacted secret in the posted entry is restored from the stored config by entry id (`400 unmatched_redacted_secret` when no stored value matches). `422 controller_unsupported` when the kind is not probeable at all (`mqtt_command`, `ha_service_call`, `esphome_native`); `502 zone_scan_failed` when the controller is unreachable, rejects the credential, is rate limited, **or has no zone-discovery endpoint** (`hydrawise`, `bhyve`, `rainbird`, whose detail reads "operation not supported by this controller"). A client cannot tell "this kind cannot enumerate" from "this controller is offline" by status alone, so gate on the kind before calling: only `rachio`, `opensprinkler_direct`, `http_generic` and `dry_run` can enumerate |
| `/api/v1/wizard/probe_soil` | POST | `{ "host": "<gateway host>", "source_id": "..." }` (`source_id` optional); reads an Ecowitt gateway's live soil channels off its local API so the Sensors step can offer them for zone binding. `422` for an empty or non-LAN host, `502` if unreachable |
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
{ "kind": "set_global_override", "mode": "run" }
{ "kind": "set_zone_override", "zone": "back_yard", "mode": "skip" }
```

Notes:

- `run` is clamped server-side to 7200 seconds (2 hours) regardless of what the client sends.
- `set_threshold` accepts only the known keys `max_wind_mph`, `min_temp_f`, `rain_skip_in`.
- `set_override_tomorrow` takes `"none" | "skip" | "run"` (a one-day, HA-helper override).
- `set_pause_until` with `epoch: 0` clears the vacation pause (same as `clear_pause_until`).
- `set_global_override` takes `"auto" | "skip" | "run"`. It is a **sticky** global override, LocalSky-native (its own state, no nightly reset): `"run"` forces watering past the skip conditions, `"skip"` force-skips, `"auto"` follows the engine. It stays in effect until you change it.
- `set_zone_override` takes a `zone` slug plus `"auto" | "skip" | "run"`, the same sticky semantics scoped to one zone. **A zone override beats the global override**; `"auto"` clears the zone override so the zone falls back to the global override, then the engine verdict.
- The two override actions are always handled by LocalSky's own state (a small SQLite store) whenever a persistence DB is mounted, so they work on both standalone and Home-Assistant-sourced deployments.

A successful `run`, `stop`, or `stop_all` dispatched through a configured controller answers `200` with `{ "ok": true, "dispatched": "controller:<id>", ... }` plus `confirm_within_s` (1.23.0): how many seconds the dispatching controller can take to *report* the change, or `null` when it reads state on demand. A cloud controller polls its vendor on a throttle, so a change can be accepted well before it reads back; treat `200` as confirmation that the controller took the command, and `confirm_within_s` as how long the state may lag. (The legacy Home-Assistant-service-call path, used only on an HA-sourced deploy with no controller configured, omits the field; treat an absent value the same as `null`.)

A failed dispatch answers `{ "error": "<what happened>", "code": "<stable discriminator>" }` plus, for the recoverable ones, a `hint` naming the fix. **Branch on `code`**, which is stable across status changes:

| Status | `code` | Means |
|---|---|---|
| `400` | `zone_unknown` | The zone is not mapped to a zone on the controller. The body carries `mapped_zones`, the controller's zone-map keys, so a slug mismatch is visible directly |
| `424` | `controller_auth_failed` | The controller rejected its own credential. Deliberately not `401`, which on any endpoint means *this deploy's* authentication failed |
| `429` | `controller_rate_limited` | The controller is rate limiting. The body carries `rate_limit_remaining`, what its last response reported (`null` when that response reported none) |
| `501` | `controller_unsupported` | The controller does not support the operation |
| `502` | `controller_unreachable` | The controller returned an error, was unreachable, is offline, or its adapter failed to initialize. The `error` string carries the upstream status and body for a cloud controller |
| `503` | (none) | No controller registry, or no controller configured |

A cloud controller's zone map is keyed by the slugified vendor zone name, while dispatch looks it up by the LocalSky zone slug, and nothing forces the two to agree. That is what `mapped_zones` exists for: on a `zone_unknown` it shows exactly which keys the controller can dispatch, next to the slug that missed. The lookup is deliberately exact, with no name-similarity fallback, because guessing which valve a near-match meant risks opening the wrong one.

> `run_sequence_now` was removed along with Irrigation Unlimited support. The action still deserializes so an old client gets a clear **`410 Gone`** (`{"error": "run_sequence_now was removed along with Irrigation Unlimited support; use per-zone Run instead"}`) rather than a parse error. Use a per-zone `run` instead.

### `GET /api/v1/irrigation/history?days=30`

Run history window, counted backward from now. `days` defaults to 30 and clamps to 1..365.

```json
{
  "from_epoch": 1762808000,
  "to_epoch": 1765400000,
  "runs": [
    { "zone": "back_yard", "start_epoch": 1765320000, "duration_s": 600, "skip_reason": null, "source": "ha_refresher", "status": "completed" }
  ]
}
```

Rows with a non-null `skip_reason` are skip events rather than completed runs. `source` and `status` (1.21.0, additive) carry the row's provenance so clients can reduce watering minutes the way the engine does: watering evidence is `status = "completed"` with source `ha_refresher`, `manual`, or `manual:<id>`; `dry_run` (and `dry_run:<id>`) rows are pretend water and `smart_morning` rows are skip markers. A manually started run appears twice (the request row and the observed hardware activity); minute totals should cluster overlapping rows rather than sum them, which is exactly what the app's own charts do.

### `GET /api/v1/irrigation/decisions?days=30`

Verdict-transition history: one record per change of the skip-check verdict, so you can answer "did we actually skip on day X, and why" weeks later. Same `days` parameter semantics as `/history`.

### `GET /api/v1/irrigation/export?days=365&format=csv`

Portable history export. `format=csv` (the default) streams the run/skip events as `timestamp_utc,zone,event,duration_s,reason` rows; `format=json` returns the full `{ from_epoch, to_epoch, runs, decisions }` structure. `days` defaults to 365 and clamps to 1..3650. Served with a `Content-Disposition: attachment` header, so a browser hit downloads a file.

### `GET /api/v1/irrigation/accuracy?days=30`

The forecast-accuracy scoreboard: one row per local day pairing that morning's verdict with the rain that actually fell, plus the matched/scored tally. `days` defaults to 30 and clamps to 1..365. Like `/history` and `/decisions`, this mounts only when the history database is available.

### `GET /api/v1/irrigation/tuning?days=14`

The per-zone tuning report: a window of recorded outcomes reduced to at most one plain-language recommendation per zone, plus the install-wide forecast-skip scorecard. `days` defaults to 14 and clamps to 7..30. Like `/history` and `/decisions`, this mounts only when the history database is available. Read-only; the write side is [`POST /api/v1/config/zones/apply`](#post-apiv1configzonesapply).

```json
{
  "generated_epoch": 1787500000,
  "window_days": 14,
  "zones": [
    {
      "slug": "back_yard",
      "display_name": "Back Yard",
      "status": "recommendation",
      "lines": ["Watered 5 time(s) in the last 14 days."],
      "recommendation": {
        "id": "8f2c41a09b6d13e7",
        "field": "precip_rate_mm_hr",
        "current_value": null,
        "suggested_value": 18.0,
        "companion_fields": [{ "field": "precip_rate_source", "value": "measured" }],
        "headline": "Set this zone's sprinkler rate to the measured 18.0 mm/hr; runs are planned as if it were 38.0 mm/hr.",
        "evidence": ["Median rate backed out of 3 clean watering events: 18.0 mm/hr vs the configured 38.0 mm/hr (53% apart)."],
        "confidence": "medium"
      }
    }
  ],
  "scorecard": {
    "window_days": 30,
    "scored_days": 4,
    "confirmed_days": 3,
    "min_scored_days": 3,
    "line": "Skipped 4 days for forecast rain in the last 30; rain came 3 of 4.",
    "reactive_days": 2,
    "reactive_line": "Skipped 2 day(s) for rain already falling or on the ground in the last 30."
  }
}
```

- `status` is `recommendation`, `ok`, or `insufficient_data`; `lines` carries the cadence line, the water-balance term lines (observed rain, applied irrigation, forecast credit, each with its source rung), and each check's specific not-enough-data state.
- `current_value` / `suggested_value` are JSON values; `null` as a suggestion means "clear the override" (restore the default).
- `scorecard.scored_days` / `confirmed_days` cover FORECAST rain skips only (rain expected within 4 hours, tomorrow rain, 3-day rain) and are `null` until at least `min_scored_days` such days could be judged. Reactive rain skips (rain already falling or already on the ground) confirm themselves, so they are never scored; they ride `reactive_days` / `reactive_line` as a separate count (`null` / empty until one exists).
- Applying a recommendation from a non-default window must echo the report's `window_days` (see the apply endpoint below): window-dependent checks derive different suggestions at different windows.
- `dismissed: true` (1.21.0) marks a zone with at least one snoozed or dismissed suggestion. The silenced suggestion is skipped inside the ranked pick, so a lower-ranked suggestion may still occupy `recommendation`; the last entry of `lines` is the muted annotation, and `dismissed_fields` names the silenced config fields for the undismiss call.

### `POST /api/v1/irrigation/tuning/dismiss` and `POST /api/v1/irrigation/tuning/undismiss`

Silence or restore one zone's tuning recommendation (1.21.0). Dismiss body: `{ "zone_slug", "field", "recommendation_id", "kind" }` where `kind` is `"snooze"` (silences the exact `recommendation_id` for 30 days; a suggestion whose value later drifts to a new id returns immediately) or `"permanent"` (keys the zone + field and survives value drift). Undismiss body: `{ "zone_slug", "field" }`; answers `{ "removed": N }`. Both are privileged like `POST /config/zones/apply`. Silencing is per-suggestion and total for that suggestion: the report strips it server-side inside the ranked pick (the zone's next-ranked suggestion, if any, surfaces instead), so every count-based surface and the weekly notification go quiet with it; a report whose every suggestion is silenced sends no weekly push. Mount only when the history database is available.

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

## Radar map data

Server-side data services for the radar map's overlay layers. Canonical prefix only (`/api/v1/radar/*`, no legacy `/api` alias). All three are built from upstream feeds with server-side caching, so map panning does not hammer the upstreams; on an upstream failure they return `502` and the frontend degrades the layer silently.

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/radar/windgrid?bbox=minLon,minLat,maxLon,maxLat` | GET | Wind field for the leaflet-velocity layer: a grib2json-style two-record array (U then V components in m/s) over an 8x8 grid clamped to the bbox. Cached about 30 minutes |
| `/api/v1/radar/precip?bbox=minLon,minLat,maxLon,maxLat` | GET | Short-range precipitation nowcast grid: 8 future 15-minute frames (the next 2 hours) of mm-per-15-min values over the same 8x8 grid, plus a `max_mm` scale hint. Cached about 15 minutes |
| `/api/v1/radar/tropical` | GET | Basin-aware tropical cyclone GeoJSON, normalized from the NHC/CPHC, JMA, and JTWC feeds into one FeatureCollection (positions, tracks, forecast tracks, cones) plus a per-agency `sources` health array. Cached 10 minutes |

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
  "version": "0.7.0",
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
      "status": "active"
    }
  ],
  "controllers": [
    { "id": "opensprinkler", "kind": "opensprinkler_direct", "default": true, "enabled": true }
  ],
  "ha": { "env_configured": true, "reachable": true, "snapshot_source": "standalone" }
}
```

Per-source `status` reflects each source's role in the live per-field merge, not a raw age bucket. It is one of:

- `active`: the source currently owns at least one live reading (it is the winning provider for a field right now).
- `watching`: reachable and quiet. It fetched fine but has nothing to report this cycle (a dry or no-coverage rain authority), or it is a reachable non-owner whose reading is currently held only by a lower-or-equal-priority source. It should be winning and simply has nothing to add yet.
- `standby`: reachable and owns nothing because a strictly higher-priority source currently owns the reading(s) it could provide. It is ready to take over if that source goes quiet.
- `falling_through`: it previously owned a reading, has since gone stale past that reading's freshness window, and another source has taken over (the backup chain handled it). Reserved: current releases do not yet assert this state (prior ownership is not tracked), so a source in that situation reads `standby` or `watching` instead; treat it as part of the vocabulary, not a status to alert on.
- `offline`: no successful fetch and no observation for the hard-offline window (about 30 minutes), or never seen. **This is the only status that marks the instance `degraded`;** `watching`, `standby`, and `falling_through` are all calm (the fall-through chain working as designed).

`last_seen_epoch` and `stale_for_s` remain on the wire as diagnostics but no longer drive the status; the taxonomy above is computed from reachability plus live per-field ownership. On an auth-required instance, **anonymous** callers get a trimmed liveness-only body: no `sources`, `controllers`, or `ha` detail, so Docker healthchecks keep working without leaking topology.

When `config_present` is false the server is in wizard mode; the dashboard redirects to `/setup`.

### `GET /api/v1/updates`

Release check status: `{ current, latest, update_available, release_url, checked_at_epoch, check_enabled }`. The background check only runs when `[updates] check_enabled` is set; otherwise `latest` stays null. When enabled it fetches the project version manifest at `localsky.io/latest.json` daily; the running version travels in the request User-Agent, nothing per-install.

### `GET /api/v1/location`

The configured map center (lat/lon/zoom) for the radar, from `deployment.location` in the config, falling back to the `WEATHER_APP_LAT`/`WEATHER_APP_LON` env vars.

### `GET /api/v1/location/timezone?lat=<lat>&lon=<lon>`

Offline IANA timezone lookup for a coordinate.

### `GET /api/v1/location/elevation?lat=<lat>&lon=<lon>`

Elevation lookup for a coordinate via the Open-Meteo elevation API, returning `{ "elevation_m": <meters> }` (the same unit as the config's `deployment.location.elevation_m`, which the wizard prefills from it). Returns `502` on an upstream or parse failure; the wizard falls back to manual entry.

### `GET /metrics`

Prometheus exposition endpoint (`text/plain; version=0.0.4`), served at the origin root (not under `/api/v1`) and always public. Aggregate operational counters only: verdict mix, refresh and degraded counts, controller and cloud error counts, last-fetch latency. No secrets, config, or PII; firewall it at the proxy if you do not want it exposed.

## System

`POST /api/v1/system/restart` restarts LocalSky from inside the app
(privileged: same authentication bar as a config write). Body is optional:
`{"force": true}` overrides the active-watering guard, which otherwise
answers `409 watering_in_progress` naming the running zones. Responds
`202 {"mode": "supervisor" | "exit"}`: under the Home Assistant add-on the
Supervisor restarts the add-on; everywhere else the process exits cleanly
and the container/service restart policy relaunches it (every documented
install runs `--restart unless-stopped` or equivalent).

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
