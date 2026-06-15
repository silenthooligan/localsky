# Backup, restore, and recovery

Everything LocalSky knows lives in the `/data` directory you mounted at install time. Back that up and you can rebuild a working instance on any machine in minutes.

## What is in /data

| File | What it holds |
|---|---|
| `localsky.toml` | Your entire configuration: location, sources, controllers, zones, schedules, restrictions, notification channels |
| `irrigation.db` | The SQLite database: run history, sensor history, verdict history, decision traces, web push subscriptions, and (when auth is enabled) accounts, sessions, and API tokens |
| `irrigation.db-wal`, `irrigation.db-shm` | SQLite write-ahead-log sidecars; present while the container runs |
| `localsky.toml.draft` | First-run wizard progress, if you saved mid-wizard; deleted when the wizard finishes |
| `instance-id` | A stable random identity used for mDNS and Home Assistant pairing |
| `site/photos/` | Zone photos uploaded through the zone editor |

The database runs in WAL mode, so even an unclean shutdown rolls back to a consistent state on the next boot.

## Built-in backup (recommended)

LocalSky can produce a consistent backup bundle while running: a `.tar.gz` containing `localsky.toml`, a point-in-time copy of `irrigation.db` (made with SQLite's `VACUUM INTO`, safe against concurrent writes), and a small `manifest.json` recording the version and timestamp.

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

## Restoring

### From a backup bundle

**From the UI:** Settings -> Advanced -> Backup and restore -> **Restore from bundle**, then pick the `.tar.gz`.

**From the command line:**

```bash
curl -f -X POST \
  -F bundle=@localsky-backup-0.2.0-beta.1-20260610-020000.tar.gz \
  http://localhost:8090/api/v1/backup/restore
docker restart localsky
```

What the restore does, exactly:

- The **config** is validated first (a broken file is rejected with a 422 and changes nothing), then applied immediately.
- The **database** is not swapped live. It is staged next to the real one as `irrigation.db.restore`; on the next container start, LocalSky moves the current database aside (kept as `irrigation.db.pre-restore.<timestamp>`, so a restore is reversible) and swaps the staged one in. That is why the response says `"restart_required": true` whenever a database was uploaded.

You can also restore the pieces individually: `-F config=@localsky.toml` applies just a config (no restart needed), `-F db=@irrigation.db` stages just a database.

### From plain file copies

```bash
docker stop localsky
cp /backup/localsky/irrigation-2026-06-01.db /opt/localsky/data/irrigation.db
rm -f /opt/localsky/data/irrigation.db-wal /opt/localsky/data/irrigation.db-shm
cp /backup/localsky/localsky-2026-06-01.toml /opt/localsky/data/localsky.toml
docker start localsky
```

Remove the `-wal`/`-shm` sidecars when replacing the database file; stale ones belong to the old database. Restoring a database from an older release is fine: boot replays whatever schema migrations it is missing.

## Test your restore

A backup you have never restored is a hope, not a backup. Five minutes proves yours works, without touching production:

```bash
mkdir -p /tmp/localsky-restore-test
docker run -d --name localsky-test \
  -p 8091:8090 \
  -v /tmp/localsky-restore-test:/data \
  -e LOCALSKY_DEMO=1 \
  ghcr.io/silenthooligan/localsky:latest

# Push the bundle into the test instance, then restart to swap the DB in:
curl -f -X POST -F bundle=@localsky-backup-....tar.gz \
  http://localhost:8091/api/v1/backup/restore
docker restart localsky-test
```

Open `http://localhost:8091` and check that your zones, settings, and run history are all there. `LOCALSKY_DEMO=1` keeps the test instance's live data paths switched off, so it will not poll your weather sources, and weather shown is synthetic; it exists only to prove the bundle restores. Even so, the restored config names your real irrigation controller, so do not press run buttons on the test instance. Tear it down when satisfied:

```bash
docker rm -f localsky-test && rm -rf /tmp/localsky-restore-test
```

## Recovery patterns

### "I broke my config and the UI still loads"

Settings -> Advanced -> **Raw TOML editor** edits `/data/localsky.toml` directly and validates before saving. Or push a known-good config file without restoring the database:

```bash
curl -f -X POST -F config=@localsky-good.toml \
  http://localhost:8090/api/v1/backup/restore
```

A note on config snapshots: the database has a snapshot table and a `POST /api/v1/config/rollback?to=<version>` endpoint (snapshots listed at `GET /api/v1/backup/snapshots`), but in this beta saves do not record snapshots yet, so the list stays empty and rollback returns 404. Until that lands, your backup bundles are the config history.

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
