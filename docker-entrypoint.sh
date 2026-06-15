#!/bin/sh
# Fix-permissions-then-drop entrypoint.
#
# The container starts as root only long enough to make the writable mounts
# owned by the app uid, then drops to the non-root user (uid 10001) via gosu to
# run LocalSky. This makes the non-root container work on ANY volume shape with
# no operator action: a fresh bind mount (host-root-owned), a named volume, or
# an upgrade from a volume that was root-owned by an earlier (root) container.
# The app process itself runs unprivileged for its entire life; root exists only
# for the brief chown + drop.
#
# If the container is started as a non-root user already (e.g. compose sets
# `user:`), the chown is skipped and the app runs as that user as-is.
set -e

APP_UID=10001
APP_GID=10001

if [ "$(id -u)" = "0" ]; then
    # Writable data dir: ensure the app can own + write it. Idempotent.
    chown -R "$APP_UID:$APP_GID" /data 2>/dev/null || true
    # Keys dir is usually mounted read-only; chown is best-effort (a read-only
    # mount is fine as long as the key file is already app-readable).
    chown -R "$APP_UID:$APP_GID" /keys 2>/dev/null || true
    exec gosu "$APP_UID:$APP_GID" "$@"
fi

exec "$@"
