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
#
# Volume the container can't chown (Synology/QNAP NFS exports with root_squash
# or "map all users" squash root and forbid chowning to an arbitrary uid): the
# best-effort chown fails, so we detect that /data still isn't writable as the
# target uid and fall back to running as the volume's ACTUAL owner instead of
# EACCES-looping. Operators can pin the uid:gid explicitly with PUID/PGID.
set -e

APP_UID="${PUID:-10001}"
APP_GID="${PGID:-10001}"

if [ "$(id -u)" = "0" ]; then
    # Writable data dir: ensure the app can own + write it. Idempotent.
    chown -R "$APP_UID:$APP_GID" /data 2>/dev/null || true
    # Keys dir is usually mounted read-only; chown is best-effort (a read-only
    # mount is fine as long as the key file is already app-readable).
    chown -R "$APP_UID:$APP_GID" /keys 2>/dev/null || true

    # If /data still isn't writable as the chosen uid (squashed/mapped NFS),
    # run as the directory's real owner so writes succeed instead of failing.
    if ! gosu "$APP_UID:$APP_GID" test -w /data 2>/dev/null; then
        OWNER_UID="$(stat -c '%u' /data 2>/dev/null || echo "$APP_UID")"
        OWNER_GID="$(stat -c '%g' /data 2>/dev/null || echo "$APP_GID")"
        # Never fall back to root: a root-owned, unwritable /data (read-only
        # mount, or an export we can't chown) would otherwise run the whole app
        # as uid 0, defeating the non-root + cap_drop model. Every legitimate
        # squash/mapped case lands on a non-zero owner, so this loses nothing.
        if [ -z "$OWNER_UID" ] || [ "$OWNER_UID" = "0" ]; then
            echo "localsky: /data is not writable as ${APP_UID}:${APP_GID} and its owner is root; refusing to run as root. Fix the mount (mount rw, or chown to a non-root uid) or set PUID/PGID." >&2
        else
            echo "localsky: /data not writable as ${APP_UID}:${APP_GID} (squashed/mapped NFS?); running as its owner ${OWNER_UID}:${OWNER_GID}. Set PUID/PGID to override." >&2
            APP_UID="$OWNER_UID"
            APP_GID="$OWNER_GID"
        fi
    fi

    exec gosu "$APP_UID:$APP_GID" "$@"
fi

exec "$@"
