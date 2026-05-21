# UX Journey: install, upgrade, change

This document audits LocalSky's operator experience across every state transition: first install, version upgrades, configuration changes, hardware changes, recovery from misconfiguration, and migration between modes. Each section names the gaps + how LocalSky handles them.

## First-run install

### What the operator sees

1. `docker run` returns. Container is up.
2. Visit `http://<host>:8090`. Server detects no `/data/localsky.toml`. Redirects to `/setup/welcome`.
3. Eight-step wizard ([docs/getting-started.md](getting-started.md#first-run-wizard)). Each step's "Save and finish later" link writes a draft to `/data/localsky.toml.draft`. The wizard is resumable across restarts.
4. Step 8 (Review) presents the final summary + a single "Save and finish" button. POSTs `/api/wizard/apply`:
   - Validates the draft
   - Writes `/data/localsky.toml` atomically (write to .tmp, fsync, rename)
   - Records a snapshot in `config_snapshots` (version 1)
   - Deletes the draft file
5. Server re-mounts normal routes. Dashboard appears at `/`.

### Gaps + how LocalSky handles them

| Gap | Handling |
|---|---|
| Browser refresh mid-wizard | Draft is persisted server-side after each step; refresh resumes at the same step with same values |
| Container restart mid-wizard | Same: draft survives restart |
| User closes tab and comes back days later | Draft still there. The wizard banner on the dashboard ("Resume setup") invites resumption |
| Wizard finishes but config validation fails | Apply returns 422 with the specific field error inline; on-disk file untouched; draft preserved |
| First boot has no location entered | Lat/lon default to (0.0, 0.0); validation flags "null island"; user can't advance past the Location step until corrected |
| User doesn't accept the license | Apply refuses with `LicenseNotAccepted` error |

### What still needs work

- **Geocode helper from address text**: server-side proxy to Nominatim exists at `/api/wizard/geocode`, but the wizard UI's Location step currently shows raw lat/lon inputs only. The address-to-lat/lon flow is plumbed but not wired into the form yet. Tracked.
- **Map picker for location**: Leaflet is already loaded for the radar panel. The wizard could re-use it as a click-to-pick interface. Planned.
- **Test buttons in Sources / Controllers / LLM steps**: the API endpoints exist (`POST /api/wizard/test_source` etc.) but return 501 in v0.1; the per-adapter test logic lands as each adapter graduates from planned to tested.

## Upgrades

### What the operator does

```bash
docker pull ghcr.io/silenthooligan/localsky:latest
docker stop localsky && docker rm localsky
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v /opt/localsky/data:/data \
  -e LOCALSKY_V2=1 \
  ghcr.io/silenthooligan/localsky:latest
```

Or for users on `:latest`-pinned compose files, `docker compose pull && docker compose up -d`. Watchtower / Diun / Renovate all work.

### What happens inside

1. Container starts. Reads `/data/localsky.toml`. `schema_version` field decides what migration path applies.
2. Migration runner ([src/persistence/runner.rs](../src/persistence/runner.rs)) opens the SQLite DB. Applies any new migrations in order (one per release that needed schema changes). Records each in the `schema_migrations` table; idempotent if you rerun.
3. Config loader checks `cfg.schema_version`. If lower than `CURRENT_SCHEMA_VERSION` (currently 1), runs the registered migration chain (one function per version bump). Writes the migrated config back.
4. Runtime composition root constructs registries from the (now-current) Config. If any source/controller type has been removed in the new release, those entries are skipped with a warn log.
5. Boot completes. Dashboard at the same URL, same data, same zones.

### Gaps + how LocalSky handles them

| Gap | Handling |
|---|---|
| New config schema version released | `config/migrate.rs` runs the v-to-v+1 chain. Operator does nothing |
| New DB schema version released | `persistence/runner.rs` applies the new SQL migrations idempotently |
| Operator skips multiple versions | Migrations are chained; v0.2 → v0.5 runs M0006, M0007, ... M0012 in order |
| New release adds a required config field | Old configs missing the field are accepted: serde fills with the default declared in the schema |
| New release removes a deprecated field | Serde with `#[serde(default)]` on every field means missing-from-disk is fine; extra-on-disk is silently ignored. The next save drops the deprecated field |
| Operator downgrades to a previous version | If the persisted config has `schema_version > CURRENT_SCHEMA_VERSION` of the binary, the loader refuses with `SchemaTooNew { found, known }`. Operator must either re-upgrade or restore an older snapshot via `POST /api/config/rollback` |
| Migration partially succeeds | Each migration runs in a single transaction. Partial application is impossible: rusqlite commits or rolls back |
| DB lock from prior process | Idempotent: rerunning boot picks up where the previous attempt left off |

### What still needs work

- **CHANGELOG breaking-change section** is required for every release that bumps `CURRENT_SCHEMA_VERSION`. Currently part of the release-process docs but not yet enforced by CI.
- **Automatic snapshot before migration**: today, schema migrations apply directly. A pre-migration snapshot would let operators roll back a botched migration. Planned.

## Configuration changes

### What the operator does

Edit a value in `/settings/<section>`. Click Save. Settings UI POSTs the change to `/api/config` (or PATCH-style mutates one section + PUTs the whole config back, depending on the page).

### What happens inside

1. Server validates the incoming Config (serde decode + semantic validation).
2. If valid: snapshot the previous Config into `config_snapshots` (retention 20), write the new TOML atomically.
3. Broadcast a `ConfigEvent` over the runtime's `tokio::sync::broadcast` channel. Interested subsystems re-read their slice:
   - `ZoneUpdated(slug)` → engine picks up new Kc/soil/MAD on next tick
   - `SourceAdded(id)` → registry spawns a new adapter task
   - `ControllerChanged(id)` → registry swaps the pointer atomically; in-flight runs complete on the old controller
   - `LlmChanged(...)` → advisor reconfigures on next call (cache TTL respects the new config)
4. If invalid: return 422 with structured field-by-field errors. On-disk file untouched.

### Gaps + how LocalSky handles them

| Gap | Handling |
|---|---|
| User saves an invalid config | 422 + inline field errors; settings UI surfaces each next to the relevant field |
| Two users edit simultaneously | Last write wins (no optimistic-locking version on PUT today); both writes get snapshotted so neither is lost from history |
| User removes a source another zone depends on | Validator rejects: "zone X references source Y which is not configured" |
| User removes the default controller | Validator rejects: "at least one controller must have default = true" |
| User pushes a config that crashes the engine | Engine errors are caught at the tick boundary; the previous tick's snapshot stays in place until the next valid one. UI shows a yellow "engine error: ..." banner |
| User wants to undo a config change | `POST /api/config/rollback?to=<version>` restores any snapshot from the last 20. Reachable even when the engine is degraded |
| Operator wants to script a change | `/api/config` GET → mutate → PUT roundtrip works from curl / Python / shell |

### What still needs work

- **Optimistic locking on PUT**: a `version` field in the Config wire format would let the server reject stale writes from simultaneous edits. Today it's last-write-wins.
- **Hot-reload broadcast**: the broadcast channel is plumbed but not yet wired to every subsystem. Currently sources + controllers re-read; engine tick re-reads. LLM advisor doesn't yet (next call picks up).
- **Per-section dirty-state UX**: the settings UI saves the entire Config block on each PUT. A per-section PATCH endpoint would let only-this-section pages save without touching unrelated fields. Not blocking but would tighten the UX.

## Hardware changes

### Adding a new sensor

The flow:

1. Connect the sensor's physical hardware (battery, Zigbee pair, network plug).
2. Decide the path: direct LAN adapter (e.g. Tempest), MQTT subscribe (Tasmota / ESPHome / Zigbee2MQTT), Ecowitt POST receiver, or HTTP webhook ([docs/standalone.md](standalone.md#sensor-ingestion-without-home-assistant)).
3. Add a source entry under `/settings/sources`. Optionally test the connection via the "Test" button.
4. Save. The Runtime spawns a new source task; observations start flowing immediately.
5. If the sensor is per-zone (soil moisture, soil temp), reference it from the zone editor: `ZoneConfig.soil_sensor_id`.

### Swapping controllers

The flow:

1. Add the new controller under `/settings/controllers` alongside the old one. Test connection.
2. Per-zone: update `ZoneConfig.controller_id` to point to the new one. Save.
3. When confident, mark the new controller as `default = true` (the validator enforces exactly one default).
4. Optionally delete the old controller entry. Or leave it as a standby.

Zero downtime: in-flight runs complete on whichever controller they started on. New runs dispatch through the new default.

### Swapping weather sources

Same pattern: add the new source, configure it, mark it preferred by setting a higher `priority`, test, then remove the old source if desired. The merge engine handles overlap automatically: the source with the highest priority for each WeatherField wins.

### Replacing physical hardware (e.g. dead Tempest, new Ecowitt)

1. Add the new source first. Confirm data is flowing in the live dashboard (the merge engine shows provenance per field; you'll see "via ecowitt_new").
2. Remove the dead source. Verify no field reverts to a stale value.
3. If you have run history attributed to the old source ID, leave the entry disabled instead of deleting to preserve attribution.

### Adding a new zone

1. `/settings/zones` → Add.
2. Species + soil texture + sprinkler PR + controller mapping.
3. Save. Engine starts tracking the bucket from "fully wet" assumption (depletion_mm = 0); the operator can adjust by observation.

### Removing a zone

1. `/settings/zones` → delete row.
2. Confirm. The zone disappears from the dashboard immediately. Run history for the zone is preserved (the runs table just stops getting new entries for that slug).

### Gaps + how LocalSky handles them

| Gap | Handling |
|---|---|
| Sensor disconnects unexpectedly | Source-side: the merge engine detects "field not seen in N minutes" and demotes that source for that field. Dashboard shows the older value with a "stale" badge |
| Controller goes offline | Status badge flips red. Manual zone-run buttons return "controller offline" error. Scheduled runs queue with `status='intended'` and dispatch when the controller returns |
| Operator pulls the SD card mid-run | Restart picks up `runs.status='running'` rows older than 5 min, polls the controller, marks `aborted` with the actual end-time from controller telemetry (when supported) |
| Wrong source supplies a field | Lower the priority or disable. Merge engine picks the next source down. Provenance display reveals which source is currently winning each field |
| Operator wants to test a config change non-destructively | `POST /api/config/preview` runs validation against a candidate config without writing. Settings UI plans to expose this as a "Validate" button |

### What still needs work

- **Sensor disconnect detection** isn't surfaced explicitly in the UI today. Per-source last-seen timestamps are stored in `sensor_history`; a panel exposing them is planned.
- **Mid-run controller swap protection**: today, deleting the controller a zone references will validate-reject. But a less-obvious case is changing the controller_id on a zone while the zone is running. The right behavior is to refuse the change until the run completes; today's validator allows it.
- **Zone delete confirmation**: the settings UI plans a "delete this zone?" modal with a "downloads run history first" affordance. Planned.

## Mode migration (e.g. standalone → outbound HA → HA-driven)

LocalSky's three modes are runtime-switchable:

- **Standalone → outbound HA (Mode 2)**: set `notifications.mqtt.host`. LocalSky starts publishing discovery topics. No other change. HA auto-creates entities.
- **Standalone → HA-driven (Mode 3)**: add a `kind = "ha_service_call"` controller; mark it default. Zone runs now dispatch through HA.
- **HA-driven → standalone**: add a direct-control controller (e.g. `opensprinkler_direct`); mark it default. Optionally delete the HA controller.

In all cases, zone runs in flight complete on whichever controller started them. Configuration changes flow through the same hot-reload mechanism as other config edits.

### Gaps

| Gap | Handling |
|---|---|
| Mode transitions are step-wise; no "convert mode 1 to mode 2" wizard | Documented as a checklist; auto-converter is planned |
| HA passthrough → MQTT subscribe migration | Both work simultaneously; the merge engine prefers higher-priority sources. Operator removes the old one once confidence is established |

## Recovery patterns

### "I broke my config and now nothing loads"

```bash
docker exec -it localsky cat /data/localsky.toml > /tmp/config-broken.toml
# Edit /tmp/config-broken.toml to fix the issue
docker exec -i localsky tee /data/localsky.toml < /tmp/config-broken.toml
docker restart localsky
```

Or via the API: `POST /api/config/rollback?to=<previous_version>`. The rollback endpoint is always reachable even when the engine is degraded.

### "My data dir is corrupted"

LocalSky's SQLite uses WAL mode + synchronous=NORMAL. Crashes mid-write produce a rolled-back state; the next boot recovers.

If the DB file is irrecoverable (filesystem-level corruption):

```bash
docker stop localsky
mv /opt/localsky/data/irrigation.db /opt/localsky/data/irrigation.db.bak
docker start localsky
```

The migration runner re-creates a fresh DB. Config + zones + sources + controllers are preserved (those live in localsky.toml). Run history is lost; the new DB starts fresh.

### "I want to migrate to a new host"

```bash
# Old host
docker stop localsky
tar czf localsky-backup.tar.gz -C /opt/localsky data

# New host
scp localsky-backup.tar.gz newhost:
ssh newhost
mkdir -p /opt/localsky
tar xzf ~/localsky-backup.tar.gz -C /opt/localsky
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v /opt/localsky/data:/data \
  -e LOCALSKY_V2=1 \
  ghcr.io/silenthooligan/localsky:latest
```

Done. All state (config, zones, runs history, push subscriptions, sensor history) is in `/data`.

### "I want to clone production to a staging instance"

Same as host migration but pointed at a different host + port. The two instances can publish to different MQTT topic namespaces (set `deployment.display_name` differently); both can read from the same HA broker / Tempest hub without conflict.

## Summary: what to look at before publishing

The UX gaps surfaced by this audit, in priority order:

1. **Wire address-to-lat/lon geocoding into the Location wizard step** (server endpoint exists; UI input doesn't call it yet)
2. **Test buttons in Sources / Controllers / LLM steps** (501 → real)
3. **Sensor list editor in `/settings/sources`** (with add/remove/test affordances)
4. **Controller list editor in `/settings/controllers`** (with scan-zones + test-fire)
5. **Zone list editor in `/settings/zones`** (with species picker that uses the grass-species images + calibration modal for catch-cup measurement)
6. **Optimistic-locking version on PUT /api/config** (so simultaneous edits don't silently overwrite)
7. **Pre-migration snapshot** (one extra config_snapshots row when schema_version bumps; gives clean rollback even for schema migrations)
8. **Sensor last-seen panel** (per-source freshness display)
9. **Zone delete confirmation modal** (with run-history download)
10. **Map picker for the Location step** (Leaflet click-to-pick)

None of these are launch-blocking; the wizard + settings paths all work end-to-end today. They're polish items between v0.1 and v0.2.

## Cross-references

- [docs/getting-started.md](getting-started.md): the conversational first-run walkthrough
- [docs/configuration.md](configuration.md): field-by-field config reference
- [docs/api.md](api.md): REST endpoints used by every operation in this doc
- [docs/MIGRATION.md](MIGRATION.md): operator playbook for moving from internal to public deployment
- [docs/standalone.md](standalone.md): full no-HA path including hardware change scenarios
