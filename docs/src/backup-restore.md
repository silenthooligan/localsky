# Backup, restore, and recovery

Everything LocalSky knows lives in the `/data` directory you mounted at install time. Back that up and you can rebuild a working instance on any machine in minutes.

## What is in /data

| File | What it holds |
|---|---|
| `localsky.toml` | Your entire configuration: location, sources, controllers, zones, schedules, restrictions, notification channels |
| `localsky.ledger.toml` | LocalSky's own record beside the config: which config migrations have run, which forecast authorities it seeded, the Home Assistant helper migration. Not for editing |
| `irrigation.db` | The SQLite database: run history, sensor history, verdict history, decision traces, web push subscriptions, and (when auth is enabled) accounts, sessions, and API tokens |
| `irrigation.db-wal`, `irrigation.db-shm` | SQLite write-ahead-log sidecars; present while the container runs |
| `*.restore`, `irrigation.db.restore-state.json` | Pending restore files and the durable record that identifies a complete staged set; preserve them if startup reports an interrupted restore |
| `localsky.toml.restore-hot-apply.pending` | Present during a config-only restore; a leftover means the apply did not finish and startup requires recovery |
| `*.pre-restore.<transaction>` | Prior live files retained during restore activation, including the old database's journal sidecars |
| `localsky.toml.draft` | First-run wizard progress, if you saved mid-wizard; deleted when the wizard finishes |
| `instance-id` | A stable random identity used for mDNS and Home Assistant pairing |
| `site/photos/` | Zone photos uploaded through the zone editor |

The database runs in WAL mode, so SQLite can recover interrupted database transactions. Replacing the config, ledger, and database is a separate operation: an interrupted restore refuses startup until you recover a complete set.

## Built-in backup (recommended)

LocalSky can produce a consistent backup bundle while running: a `.tar.gz` containing `localsky.toml`, `localsky.ledger.toml` (the server-owned migration and seeding record beside it), a point-in-time copy of `irrigation.db` (made with SQLite's `VACUUM INTO`, safe against concurrent writes), and a small `manifest.json` recording the version and timestamp.

**From the UI:** Settings -> Advanced -> Backup and restore -> **Download backup**.

**From the command line:**

```bash
curl -fL -OJ http://localhost:8090/api/v1/backup
# saves localsky-backup-<version>-<timestamp>.tar.gz
```

If [authentication](authentication.md) is enabled (`[auth] mode = "required"`), pass an API token:

```bash
curl -fL -OJ -H "Authorization: Bearer lsk_yourtoken" \
  http://localhost:8090/api/v1/backup
```

That curl line drops straight into cron for nightly backups. Keep a few generations and store them off the machine that runs LocalSky.

> **The bundle contains real secrets.** So that it restores onto a fresh machine without you re-typing everything, `localsky.toml` is included **full fidelity**: your Home Assistant token, MQTT and SMTP passwords, OpenSprinkler password hash, LLM API key, and any webhook URLs are all in the file. The download endpoint is privileged (only an authenticated session, an API token, or a trusted-network/loopback caller can fetch it, even when auth is set to disabled), but the resulting `.tar.gz` is a credential once it leaves the box. Store it somewhere secure and encrypted, and treat it like a password. (The on-screen config views, by contrast, redact secrets.)

Deliberately **not** in the bundle:

- The web push VAPID private key (wherever `VAPID_PRIVATE_KEY_PATH` points). A casually shared backup should not leak a signing key; copy it separately if you use web push.
- `instance-id`. Restoring a bundle onto new hardware mints a new identity on purpose.
- Zone photos (`/data/site/photos/`). Copy that directory yourself if the photos matter to you.

## Offline alternative

No API needed; plain files work too.

**While running** (WAL mode makes a SQLite-aware copy safe):

```bash
# Bind mount, as in the install docs:
sqlite3 /opt/localsky/data/irrigation.db \
  ".backup '/backup/localsky/irrigation-$(date +%F).db'"
cp /opt/localsky/data/localsky.toml /backup/localsky/localsky-$(date +%F).toml
cp /opt/localsky/data/localsky.ledger.toml /backup/localsky/localsky-$(date +%F).ledger.toml

# Named volume instead? The files live under Docker's volume root:
sqlite3 /var/lib/docker/volumes/localsky-data/_data/irrigation.db \
  ".backup '/backup/localsky/irrigation-$(date +%F).db'"
```

**Cold copy** (simplest, brief downtime):

```bash
docker stop localsky
tar czf localsky-backup-$(date +%F).tar.gz -C /opt/localsky data
docker start localsky
```

A cold `tar` of the whole directory captures everything, including the wizard draft, instance id, and photos.

## Scheduled backups (automatic)

The best backup is the one you do not have to remember. LocalSky can write a bundle to a local directory on an interval and keep the newest few, off by default and enabled with one environment variable:

```bash
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v /opt/localsky/data:/data \
  -e LOCALSKY_AUTO_BACKUP_HOURS=24 \    # interval in hours; unset or 0 disables
  ghcr.io/silenthooligan/localsky:latest
```

Bundles are written as `localsky-backup-<epoch>.tar.gz` in `LOCALSKY_BACKUP_DIR` (default `/data/backups`, so they live inside your mounted volume), in the exact same format as the API bundle above, so they restore through the same flow. Two more optional knobs:

- `LOCALSKY_BACKUP_DIR`: where bundles are written (default `/data/backups`).
- `LOCALSKY_BACKUP_KEEP`: how many newest bundles to retain; older ones are pruned (default `7`).

These bundles contain **real secrets** (like every backup), and by default land inside `/data`, so keep the volume protected. For off-box durability, point `LOCALSKY_BACKUP_DIR` at a mounted path that is itself backed up, or copy the directory out on your own schedule. A scheduled backup you have never restored is still only hope: run through [Test your restore](#test-your-restore) once.

## Restoring

### From a backup bundle

**From the UI:** Settings -> Advanced -> Backup and restore -> **Restore from bundle**, then pick the `.tar.gz`.

**From the command line:**

```bash
curl -f -X POST \
  -F bundle=@localsky-backup-0.7.1-20260703-020000.tar.gz \
  http://localhost:8090/api/v1/backup/restore
docker restart localsky
```

What the restore does, exactly:

- **Validate every uploaded part before changing live or staged files.** Config and ledger must parse and the config must be supported by this release. A database must pass SQLite integrity checks, match a supported LocalSky migration history and schema, and successfully run pending migrations on a disposable copy. Malformed uploads and unsupported or inconsistent databases are rejected; the original uploaded database bytes stay unchanged.
- **Stage database-bearing restores for restart.** The supplied config, ledger, and database become `localsky.toml.restore`, `localsky.ledger.toml.restore`, and `irrigation.db.restore`. A synced `irrigation.db.restore-state.json` records their paths, hashes, and which optional parts are absent. It moves from `publishing` to `ready` only after the complete set is published. A DB-only restore replaces any earlier pending set without inheriting its stale config or ledger stages.
- **Hold new watering.** Accepting a database-bearing restore, including DB-only, latches the shared restart hold before staging changes. Manual runs and overrides cannot bypass it, and another settings save cannot clear it. Already-running controller timers may finish; the hold does not stop an active valve. Use Stop if you need to stop current watering. The response reports `restart_required` and its reasons.
- **Verify and activate at boot.** Before opening the database or registering controllers, LocalSky verifies the complete `ready` set and its compatibility again, then records `applying`. Prior config, ledger, and database files are retained as `.pre-restore.<transaction>` copies; the old database's WAL, SHM and rollback-journal files move with its recovery copy. The marker clears only after all replacements succeed, the database opens, and the config loads. Any failure refuses startup.

These are separate file replacements, **not an atomic multi-file restore**. Ordinary staging errors attempt to restore the previous stages. Power loss, process termination, or an uncertain rollback can leave a partial set; the durable marker prevents a new process from running against it. An interrupted `publishing` or `applying` marker, mismatched files, or an older release's unmarked `.restore` files require [manual recovery](#a-restore-was-interrupted).

A **config-only** upload applies through the normal config and runtime path. Threshold-only changes can take effect immediately. Changes to startup connections or deployment settings require restart and hold new watering. During the apply, `localsky.toml.restore-hot-apply.pending` protects the config/ledger pair; it clears only after saving both and publishing the runtime change. A failed or interrupted apply leaves recovery required rather than claiming the previous config was restored.

You can also restore pieces individually: `-F config=@localsky.toml` applies a config, while `-F db=@irrigation.db` stages only a database. Read the response's `restart_required` and `restart_reasons` fields after either request. A disconnected HTTP client does not cancel an already accepted restore; check its state before submitting another.

### From plain file copies

```bash
docker stop localsky
cp /backup/localsky/irrigation-2026-06-01.db /opt/localsky/data/irrigation.db
rm -f /opt/localsky/data/irrigation.db-wal /opt/localsky/data/irrigation.db-shm /opt/localsky/data/irrigation.db-journal
cp /backup/localsky/localsky-2026-06-01.toml /opt/localsky/data/localsky.toml
cp /backup/localsky/localsky-2026-06-01.ledger.toml /opt/localsky/data/localsky.ledger.toml
docker start localsky
```

This example assumes no interrupted restore or pending stages; otherwise follow [manual recovery](#a-restore-was-interrupted) first. Keep the config and its ledger together. Remove stale `-wal`, `-shm` and `-journal` sidecars only when replacing the database with a self-contained SQLite backup; preserve the original files elsewhere first. The restore endpoint checks whether an older database can migrate to the current release before accepting it. Plain file copying bypasses that upload validation.

## Test your restore

A restore test needs a separate data directory and an instance that cannot reach your controllers. Demo mode rejects privileged restore requests, so use a normal instance with networking disabled. The following test exposes no host port; access it through `docker exec`. Select the image version you intend to restore into and provide any environment variables referenced by your config.

```bash
restore_test_dir=$(mktemp -d /tmp/localsky-restore-test.XXXXXX)
docker run -d --name localsky-test --network none \
  -v "$restore_test_dir:/data" \
  -e LOCALSKY_SMART_DRY_RUN=1 \
  -e LLM_ADVISOR_DISABLED=1 \
  ghcr.io/silenthooligan/localsky:VERSION_YOU_ARE_TESTING

# Wait until the fresh process responds, then upload from inside its network namespace:
docker exec localsky-test curl -f http://127.0.0.1:8090/api/v1/health
docker cp localsky-backup-....tar.gz localsky-test:/tmp/restore-bundle.tar.gz
docker exec localsky-test curl -f -X POST \
  -F bundle=@/tmp/restore-bundle.tar.gz \
  http://127.0.0.1:8090/api/v1/backup/restore
docker restart localsky-test
docker logs localsky-test
docker exec localsky-test curl -f http://127.0.0.1:8090/api/v1/health
```

Check the startup log for successful restore activation, then use `docker exec` and the read-only config, irrigation snapshot, and history endpoints to check your zones, settings, and history. After restart, authentication follows the restored config and database; provide a valid restored API token if required. Unreachable sources and controllers are expected with networking disabled. This proves restore and loading, not physical device operation. Keep the network disabled and do not issue run actions. Remove the test container when finished; its isolated data directory remains available for inspection:

```bash
docker rm -f localsky-test
printf 'Test data retained at %s\n' "$restore_test_dir"
```

## Recovery patterns

### "I broke my config and the UI still loads"

Settings -> Advanced -> **Raw TOML editor** edits `/data/localsky.toml` directly and validates before saving. Or push a known-good config file without restoring the database:

```bash
curl -f -X POST -F config=@localsky-good.toml \
  http://localhost:8090/api/v1/backup/restore
```

Config saves retain the previous document in the config directory's `snapshots/` folder, keeping the newest 20. List them at `GET /api/v1/config/snapshots` or `GET /api/v1/backup/snapshots`, and restore one with `POST /api/v1/config/rollback` and JSON `{"ts": <snapshot timestamp>}`. Rollback uses the normal validation and runtime apply path; inspect its restart requirement. These config snapshots do not replace database backups.

### "A restore was interrupted"

If the log reports an incomplete restore, repeated restarts will not finish or undo it automatically. LocalSky refuses to register controllers or start schedulers against an uncertain set. Recover with the process stopped:

1. Preserve the complete data directory, startup error, marker, `.restore` stages, `.pre-restore.<transaction>` files and any `.restore.previous-<pid>-<sequence>` copies. For a config-only failure, preserve `localsky.toml.restore-hot-apply.pending` too. Do not delete a marker merely to bypass the startup check.
2. Choose one complete, verified recovery set: a known-good backup, or the matching pre-restore files. A partially activated restore can contain a mixture of old and new live files; do not select each file independently by its timestamp. A database recovery copy may depend on the WAL journal archived beside it.
3. Restore the selected config, its ledger, and database together while LocalSky is stopped. Keep that database's own journal files when recovering a WAL-based copy; exclude unrelated journals when restoring a self-contained backup. Validate the config and database with the intended LocalSky version in an isolated instance before returning it to service.
4. Only after the selected live set is complete and verified, archive the obsolete marker and pending stages outside their watched paths. Do not fabricate a `ready` marker or edit its hashes to make a partial set pass. Unmarked `.restore` files left by an older release need this same deliberate recovery.
5. Start LocalSky and check the startup log, health, configuration, zones and history. A fresh process clears the runtime restart hold; successful loading and the expected bindings still need verification before resuming watering.

### "Nothing loads at all"

Edit the file from the host (bind mount: `/opt/localsky/data/localsky.toml`) or via the container:

```bash
docker exec localsky cat /data/localsky.toml > /tmp/broken.toml
# fix /tmp/broken.toml in your editor
docker cp /tmp/broken.toml localsky:/data/localsky.toml
docker restart localsky
```

Worst case, move the file aside and rerun the first-run wizard; the database (and all history) is untouched by config problems.

### "The database is corrupted"

Crashes mid-write are handled automatically by WAL recovery. For real filesystem-level corruption:

```bash
docker stop localsky
mv /opt/localsky/data/irrigation.db /opt/localsky/data/irrigation.db.bad
rm -f /opt/localsky/data/irrigation.db-wal /opt/localsky/data/irrigation.db-shm
docker start localsky
```

Boot creates a fresh database via the migration chain. Your config, zones, sources, and controllers are all preserved (they live in `localsky.toml`); run history starts over unless you restore a database backup instead.

### "I want to move to a new machine"

```bash
# Old host
docker stop localsky
tar czf localsky-move.tar.gz -C /opt/localsky data

# New host
mkdir -p /opt/localsky
tar xzf localsky-move.tar.gz -C /opt/localsky
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v /opt/localsky/data:/data \
  ghcr.io/silenthooligan/localsky:latest
```

A full directory copy carries everything, identity included, so Home Assistant pairings and push subscriptions follow you. If you used the API bundle instead, the new host gets a fresh identity and excludes the VAPID key by design: re-pair the HACS integration and re-enable push notifications on your devices afterward.

## Related pages

- [Upgrading LocalSky](upgrading.md): always back up before an upgrade; restoring is the supported downgrade path
- [Configuration reference](configuration.md): every field in `localsky.toml`
- [Authentication](authentication.md): creating the `lsk_` API tokens used in the curl examples
