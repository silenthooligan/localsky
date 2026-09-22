# Configuration and administration API

These operations manage the instance. Keep them separate from read-only dashboard or AI connectors. Paths use `/api/v1` unless stated otherwise.

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

Replace the entire config. Body is a JSON object matching the schema. The server validates structurally (serde decode) and semantically, snapshots the previous config (retention: last 20 versions), writes `/data/localsky.toml`, and applies supported runtime changes. Check `restart_required` and `restart_reasons` for changes that need a restart.

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

Write one [tuning report](api-irrigation.md#export-and-review) recommendation through the validated config path. Body: `{ "zone_slug", "recommendation_id", "field", "value", "window_days" }`, echoing the recommendation as served. `window_days` is the window the report was fetched at (clamped 7..30; absent = the default 14): the server re-derives the zone's recommendation at that window against the exact config it is about to mutate, inside the config write lock, and answers `409 { "error": "stale_recommendation" }` when the claim no longer derives, so a stale page can never write an outdated value. A client viewing a non-default window MUST echo the report's `window_days` or its applies can 409 indefinitely. Companion fields ride server-side (a measured `precip_rate_mm_hr` also stamps `precip_rate_source = "measured"`). The mutation runs the same validation as `PUT /config` (`422` with the structured report on failure), snapshots the previous config, saves, hot-reloads the runtime, and returns `{ "applied", "zone", "field", "old_value", "new_value", "saved", "validation", "restart_required", "restart_reasons" }`. Privileged like every config write.

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

The cloud-source catalog behind the cloud weather panel: one entry per cloud weather kind (highest honesty first), each carrying the static facts (data nature per field, key tier, real-time / localization / watering-risk copy, honesty and irrigation ranks), the live current-field list, region recommendation flags, whether the kind is already configured, and a live `status` computed by the same taxonomy as [`/api/v1/health`](api-errors.md) (`active` / `watching` / `standby` / `falling_through` / `offline`). Top-level shape: `{ "lat": ..., "lon": ..., "cloud_sources": [ ... ] }`.

## Wizard endpoints

Used during first-run; always mounted, and **public only until the first account exists** (see [Public paths](authentication.md#public-endpoints)). The dashboard routes to `/setup` when no `/data/localsky.toml` exists.

| Endpoint | Method | Purpose |
|---|---|---|
| `/api/v1/wizard/draft` | GET / PUT / DELETE | Read, save, or discard the wizard draft |
| `/api/v1/wizard/apply` | POST | Validate the draft and write it as the live config |
| `/api/v1/wizard/state` | GET | Wizard progress state |
| `/api/v1/wizard/seed_current` | POST | Seed the draft from the current live config (re-running the wizard) |
| `/api/v1/wizard/test_source` | POST | Deprecated since 0.9.0. `{ "source": <SourceEntry> }`; structural validation only, answers ok for any well-formed entry. Receiver sources confirm via live readings on the Sensors hub, polled sources within one cycle after apply |
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
| `/api/v1/backup` | GET | tar.gz bundle: `localsky.toml` + its ledger + a consistent database copy + manifest. Deliberately excludes the VAPID private key directory |
| `/api/v1/backup/restore` | POST | Multipart restore (`bundle`, or bare `config` / `db`); the database swaps in at next boot |
| `/api/v1/backup/snapshots` | GET | Config snapshot history feeding `POST /api/v1/config/rollback` |

[Authentication](authentication.md) · [Backup and restore](backup-restore.md) · [Error handling](api-errors.md)
