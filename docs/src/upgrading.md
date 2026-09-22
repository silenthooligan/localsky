# Updates and rollback

Back up, update the server, then update the Home Assistant companion if you use it. Read the [release notes](https://github.com/silenthooligan/localsky/releases) for the version you are installing.

## Before updating

Download a bundle from **Settings > Advanced > Backup and restore**. Keep it outside the LocalSky host. Copy zone photos and the Web Push private key separately if you need them; the bundle excludes both.

Record your current image tag and deployment settings. For predictable upgrades, pin a released image such as `ghcr.io/silenthooligan/localsky:v{{LOCALSKY_VERSION}}`.

## Docker Compose

Change the image tag in your Compose file, preserving the existing volume, network, ports, and environment. Then:

```bash
docker compose pull
docker compose up -d
docker compose logs --tail=100 localsky
```

For a container created with `docker run`, recreate it with your original options and the new image. Reuse the same persistent data mount. Do not replace a host-network installation with a bridge-network example if it receives local broadcasts.

## Home Assistant OS

Take an HA backup, update the **LocalSky app**, and inspect its log. Open LocalSky and verify the server version. Update the optional **HACS integration** afterward.

The app hosts the server. The HACS integration connects HA to that server; updating one does not update the other.

## Verify the update

Check **Settings > About** or `GET /api/v1/info`. Confirm:

- Configured sources are reporting with plausible observation times.
- Zones remain bound to the correct controller stations.
- Today's recorded activity and the next watering plan are available.
- HA entities reconnect, if used.

A liveness check alone does not prove that sources or controllers work.

## Config and database migrations

LocalSky migrates supported older data at startup. Config schema version and migration records are maintained separately in `localsky.toml` and `localsky.ledger.toml`; keep the pair together. SQLite migrations apply in order.

A migration failure is a reason to preserve the files and investigate the logged version and error. Do not delete the migration ledger or edit schema numbers to bypass it.

## Roll back

Use the previous image **with the backup made before the upgrade**. Database migrations are not reversed by changing an image tag, and newer configuration may not load in an older server.

Stop watering, stop the service, preserve the current data, and restore a complete compatible set. The [recovery guide](backup-restore.md) covers validation and pending-restore markers.

For a settings mistake, config snapshots may be enough. They restore configuration only; they do not roll back the image or database.

## Update notifications

Automatic installation is not built into LocalSky. Server-side release checks are optional:

```toml
[updates]
check_enabled = true
```

This checks the public version manifest about daily. The request includes the running version in its User-Agent. The browser's update toggle is a separate, per-device preference.

[Backup and restore](backup-restore.md) · [Troubleshooting](troubleshooting.md)
