# Backup, restore, and recovery

Keep a recoverable copy of configuration and history outside the LocalSky host. Use the built-in bundle for routine backups, and a stopped-service copy when you need the complete data directory and instance identity.

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
curl -f -OJ http://localhost:8090/api/v1/backup
# saves localsky-backup-<version>-<timestamp>.tar.gz
```

Backup endpoints are privileged. For a script, send an [API token](authentication.md):

```bash
curl -f -OJ -H "Authorization: Bearer lsk_yourtoken" \
  http://localhost:8090/api/v1/backup
```

A scheduler can call this endpoint with its token stored as a secret. Retain multiple generations outside the LocalSky host.

> **Backups contain credentials.** Configuration is included with its real secrets so it can be restored. Store bundles securely and do not attach them to public issue reports.

Deliberately **not** in the bundle:

- The web push VAPID private key (wherever `VAPID_PRIVATE_KEY_PATH` points). A casually shared backup should not leak a signing key; copy it separately if you use web push.
- `instance-id`. Restoring a bundle onto new hardware mints a new identity on purpose.
- Zone photos (`/data/site/photos/`). Copy that directory yourself if the photos matter to you.

## Full data-directory copy

Stop LocalSky before copying its complete data directory. Include the database and its own journal sidecars, configuration and ledger, instance identity, and photos. Copy any Web Push key stored outside that directory separately.

A raw copy of a running SQLite database can miss committed WAL data. Use the built-in bundle for online backups instead. Keep the original directory until a restore has been tested.

## Scheduled backups (automatic)

LocalSky can write backup bundles on an interval. Add these settings to your existing deployment:

```yaml
environment:
  LOCALSKY_AUTO_BACKUP_HOURS: "24"
  LOCALSKY_BACKUP_KEEP: "7"
  LOCALSKY_BACKUP_DIR: /data/backups
```

Bundles are written as `localsky-backup-<epoch>.tar.gz` in `LOCALSKY_BACKUP_DIR` (default `/data/backups`, so they live inside your mounted volume), in the exact same format as the API bundle above, so they restore through the same flow. Two more optional knobs:

- `LOCALSKY_BACKUP_DIR`: where bundles are written (default `/data/backups`).
- `LOCALSKY_BACKUP_KEEP`: how many newest bundles to retain; older ones are pruned (default `7`).

These bundles contain **real secrets** (like every backup), and by default land inside `/data`, so keep the volume protected. For off-box durability, point `LOCALSKY_BACKUP_DIR` at a mounted path that is itself backed up, or copy the directory out on your own schedule. Verify a scheduled bundle with [Test your restore](#test-your-restore) once.

## Restoring

### From a backup bundle

**From the UI:** Settings -> Advanced -> Backup and restore -> **Restore from bundle**, then pick the `.tar.gz`.

**From the command line:**

```bash
curl -f -X POST \
  -F bundle=@localsky-backup.tar.gz \
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

Stop LocalSky and preserve the current directory first. Restore the selected config, its matching ledger, and database as one recovery set. A self-contained SQLite backup must not be combined with unrelated WAL, SHM, or rollback-journal files. A cold database copy may need its own journals.

Copying files bypasses upload validation. Validate the selected set in an isolated instance before reconnecting it to controllers. If pending stages or restore markers exist, follow [interrupted restore recovery](#a-restore-was-interrupted) first.

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

### Configuration or database will not load

Preserve the full directory and startup error. If only configuration is damaged, recover a validated config snapshot with its ledger. For database corruption, restore a known-good compatible backup; keep the damaged database and journals for diagnosis.

Creating a fresh database discards history and account data. It is a deliberate reset, not a routine repair. Do not remove journal files or configuration to make an unexplained startup error disappear.

### Move to another host

Use a stopped-service copy of the full data directory to retain instance identity and photos. A built-in bundle deliberately excludes them; a new host restored from that bundle needs HA pairing reviewed and photos copied separately.

Copy the Web Push signing key separately and preserve its configured path if you want existing subscriptions to remain usable. Start the new instance in isolation, verify its configuration and history, then retire the old scheduler before connecting the replacement to controllers.

## Related pages

- [Upgrading LocalSky](upgrading.md): always back up before an upgrade; restoring is the supported downgrade path
- [Configuration reference](configuration.md): every field in `localsky.toml`
- [Authentication](authentication.md): creating the `lsk_` API tokens used in the curl examples
