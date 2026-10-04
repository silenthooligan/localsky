#!/bin/sh
# Fresh-install gate: boot the image on an EMPTY volume and prove the front
# door is the wizard. No demo mode, no config, no env location.
#
#   tests/e2e/fresh-install.sh <image>
#
# Asserts, from inside the container (the image ships curl):
#   GET /                -> 302 with Location: /setup
#   GET /setup           -> 200
#   GET /api/v1/health   -> config_present=false, location_configured=false
#   GET /api/v1/info     -> location_configured=false
set -eu
IMG="${1:?image}"
NAME=localsky-fresh
MODE="${2:-plain}"
if [ "$MODE" = haos ]; then
  set -- -e SUPERVISOR_TOKEN=test-only -e HA_TOKEN=test-only -e HA_URL=http://127.0.0.1:9 -e VAPID_PUBLIC_KEY=test-only
else
  set --
fi
docker rm -f "$NAME" >/dev/null 2>&1 || true
docker run -d --name "$NAME" \
  --tmpfs /data \
  -e LEPTOS_SITE_ADDR=0.0.0.0:8090 \
  -e HISTORY_DB_PATH=/data/irrigation.db \
  "$@" \
  "$IMG" >/dev/null
fail() {
  echo "FRESH-INSTALL FAIL: $1"
  docker logs --tail 60 "$NAME" 2>&1 || true
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  exit 1
}
c() { docker exec "$NAME" curl -sS --max-time 6 "$@"; }
up=0
for _ in $(seq 1 45); do
  if c -f http://127.0.0.1:8090/api/v1/info >/dev/null 2>&1; then up=1; break; fi
  sleep 2
done
[ "$up" = 1 ] || fail "image never served /api/v1/info within 90s"

head=$(c -o /dev/null -w '%{http_code} %{redirect_url}' http://127.0.0.1:8090/)
case "$head" in
  "302 "*"/setup") ;;
  *) fail "GET / on an empty volume answered '$head', expected 302 to /setup" ;;
esac
code=$(c -o /dev/null -w '%{http_code}' http://127.0.0.1:8090/setup)
[ "$code" = 200 ] || fail "GET /setup answered $code"
health=$(c http://127.0.0.1:8090/api/v1/health)
echo "$health" | grep -q '"config_present":false' || fail "health lacks config_present=false: $health"
echo "$health" | grep -q '"location_configured":false' || fail "health lacks location_configured=false: $health"
c http://127.0.0.1:8090/api/v1/info | grep -q '"location_configured":false' || fail "info lacks location_configured=false"
echo "FRESH-INSTALL OK: / -> 302 /setup, wizard serves, health says why"
docker exec "$NAME" test ! -f /data/localsky.toml || fail "fresh $MODE install synthesized a legacy yard"
# Supervisor sends SIGTERM. A connected event stream must not prevent clean exit.
docker exec -d "$NAME" curl -sN --max-time 30 http://127.0.0.1:8090/api/v1/stream -o /tmp/shutdown-stream
sleep 1
docker stop --time 8 "$NAME" >/dev/null
[ "$(docker inspect "$NAME" --format '{{.State.ExitCode}}')" = 0 ] || fail "SIGTERM did not exit cleanly"
echo "SHUTDOWN OK: $MODE install exits zero with an event stream connected"
docker rm -f "$NAME" >/dev/null 2>&1 || true
if [ "$MODE" = plain ]; then sh "$0" "$IMG" haos; fi
