# Upgrading LocalSky

LocalSky ships as a single Docker image. Upgrading means pulling a newer image and recreating the container. Everything that matters lives in `/data` (your config file and the SQLite database), so the container itself is disposable: stop it, remove it, start a new one on the same volume, and you are back where you were, on the new version.

## Back up first

Before any upgrade, download a backup bundle. It takes one click (Settings -> Advanced -> Download backup) or one command:

```bash
curl -fL -o localsky-backup.tar.gz http://localhost:8090/api/v1/backup
```

If you enabled [authentication](authentication.md), add `-H "Authorization: Bearer lsk_..."` with an API token. See [Backup, restore, and recovery](backup-restore.md) for everything the bundle contains and how to restore it. A pre-upgrade backup is also your downgrade path, so do not skip it.

## Choosing a tag

The image is published at `ghcr.io/silenthooligan/localsky`:

- **Pinned version** (`ghcr.io/silenthooligan/localsky:v0.2.0-beta.1`): you decide exactly when to move and what release notes apply. Recommended while LocalSky is in beta.
- **`:latest`**: always points at the newest release. Convenient, but a routine `docker compose pull` can move you across versions without you reading the release notes first.

Either way, read the release notes on GitHub before upgrading. Releases that change the database or config schema say so explicitly.

## The upgrade

With plain `docker run` (matching the install command from [Quick start](getting-started.md)):

```bash
docker pull ghcr.io/silenthooligan/localsky:latest
docker stop localsky && docker rm localsky
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v /opt/localsky/data:/data \
  ghcr.io/silenthooligan/localsky:latest
```

With Docker Compose:

```bash
docker compose pull
docker compose up -d
```

Removing the container does not touch `/data`. Your config, run history, sensor history, and login accounts all survive the recreate.

Auto-updaters (Watchtower, Diun notifications, Renovate on a pinned compose file) work fine with this image. Pair them with a scheduled backup if you let them act unattended.

## What happens on first boot after an upgrade

1. **Database migrations run.** LocalSky keeps a chain of numbered SQLite migrations (M0001 through M0009 as of this release) and records each applied one in a `schema_migrations` table. On boot it applies only the ones your database has not seen yet. Each migration runs inside a single transaction, so a failure rolls back cleanly rather than leaving a half-migrated database. Skipping releases is fine: the chain applies in order, however many versions you jumped.
2. **The config file loads.** `/data/localsky.toml` carries a `schema_version` field (currently `1`). Fields added by newer releases are filled with documented defaults when missing from an older file, and unknown leftover fields are ignored, so old configs keep loading.
3. **The app comes up** at the same address with the same data, zones, and history.

No manual migration steps. If a migration fails, the error appears in `docker logs localsky` with the migration version that failed.

**Ownership is handled for you.** LocalSky runs as the non-root user uid 10001, and the container fixes the ownership of `/data` to that user at startup. Upgrading from an older version that ran as root (and left root-owned files in the volume) needs no manual `chown`; the only requirement is that `/data` stays writable (not mounted read-only). If you front LocalSky with a reverse proxy, set `trusted_proxies` so it sees the real client IP (see [Authentication](authentication.md)).

## Downgrading and rollback

Rolling back the image is the same recreate dance with an older tag:

```bash
docker stop localsky && docker rm localsky
docker run -d \
  --name localsky \
  --restart unless-stopped \
  -p 8090:8090 \
  -v /opt/localsky/data:/data \
  ghcr.io/silenthooligan/localsky:v0.2.0-beta.1
```

Two things to know:

- **Database migrations are not reversed.** An older binary simply ignores migration entries it does not know about. That often works, but if the release you are leaving changed table shapes, the older code may misread them. The supported downgrade path is to restore the backup you took before upgrading (see [restore](backup-restore.md#restoring)).
- **A config from the future is refused.** If a newer release ever bumps `schema_version` above what the running binary supports, the loader refuses it with `refusing to load a config newer than this binary` and LocalSky boots as if unconfigured rather than guessing. Restore the pre-upgrade `localsky.toml` from your backup (or re-upgrade). As of this release `schema_version` is still `1`, so this cannot bite you yet.

There is also a config rollback endpoint (`POST /api/v1/config/rollback?to=<version>`), but in this beta nothing records config snapshots yet, so it always answers 404. Treat backup bundles as your rollback mechanism for now.

## Update notifications

LocalSky never updates itself and phones nowhere by default. Two opt-in ways to hear about new releases:

**Server-side check.** Add to `/data/localsky.toml` and restart the container:

```toml
[updates]
check_enabled = true   # default: false
```

When enabled, LocalSky polls the project version manifest at `localsky.io/latest.json` about once a day (a plain GET; the running version travels in the User-Agent, nothing per-install) and serves the result at:

```bash
curl http://localhost:8090/api/v1/updates
```

```json
{
  "current": "0.2.0-beta.1",
  "latest": "v0.2.0",
  "update_available": true,
  "release_url": "https://github.com/silenthooligan/localsky/releases/tag/v0.2.0",
  "checked_at_epoch": 1765432100,
  "check_enabled": true
}
```

The first check happens about a minute after boot; until then `latest` is null. Wire `update_available` into whatever notifies you (Home Assistant REST sensor, Uptime Kuma keyword, a cron + curl).

**Per-device check.** Settings -> Advanced -> "Check for new LocalSky releases" makes your browser (not the server) fetch `localsky.io/latest.json`, at most once per 24 hours, and shows the result inline. It is stored per device and discloses that device's IP to the `localsky.io` server, which the toggle's help text says outright.

## Upgrading from v0.1

v0.1 installs are adopted in place; point the v0.2 container at the same `/data`:

- An existing `irrigation.db` that predates the migration runner is detected on first boot. The legacy `runs` table is rebuilt into the current schema with every historical row preserved (your watering history carries forward), and existing web push subscriptions are kept as-is.
- `/data/localsky.toml`, if the wizard already wrote one, loads unchanged: `schema_version = 1` then is `schema_version = 1` now.
- New v0.2 surfaces ([authentication](authentication.md), the `/api/v1/*` API prefix, backup endpoints) start in their defaults: auth stays disabled until you create an owner account, and the old bare `/api/*` paths still work for existing clients.

Take a copy of `/data` before the first v0.2 boot anyway. The runs-table rebuild is one-way, and a 30-second `tar czf localsky-v01.tar.gz -C /opt/localsky data` is cheap insurance.
