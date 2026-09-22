#!/bin/sh
# Pre-merge UI gate: boot the freshly built image on an EMPTY volume, walk
# the first hour with Playwright (the /setup redirect, the wizard's happy
# path with the rules step, a zone edit), then check accessibility, editor
# interactions, override confirmation and controller scan/binding persistence.
#
#   tests/e2e/pre-merge.sh <image>
#
# Everything runs through docker so it works the same on the DooD runner
# and on a developer box: the app container and a Playwright container
# share a private network. The Playwright image is Microsoft's, at the
# exact @playwright/test version the lock file installs, or the browsers
# inside it are the wrong revision.
set -eu
IMG="${1:?image}"
NET=localsky-e2e-net
APP=localsky-e2e
DEMO=localsky-e2e-demo
E2E_DIR=$(cd "$(dirname "$0")" && pwd)
PW_VERSION=$(sed -n '/"node_modules\/@playwright\/test"/,/}/s/.*"version": *"\([0-9.]*\)".*/\1/p' "$E2E_DIR/package-lock.json" | head -1)
[ -n "$PW_VERSION" ] || { echo "could not read the @playwright/test version from package-lock.json"; exit 1; }
PW_IMG="mcr.microsoft.com/playwright:v${PW_VERSION}-noble"

cleanup() {
  docker rm -f "$APP" >/dev/null 2>&1 || true
  docker rm -f "$DEMO" >/dev/null 2>&1 || true
  docker rm -f localsky-e2e-pw >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
}
fail() {
  echo "PRE-MERGE E2E FAIL: $1"
  docker logs --tail 80 "$APP" 2>&1 || true
  cleanup
  exit 1
}
cleanup
docker network create "$NET" >/dev/null
docker run -d --name "$APP" --network "$NET" \
  --tmpfs /data \
  -e LEPTOS_SITE_ADDR=0.0.0.0:8090 \
  -e HISTORY_DB_PATH=/data/irrigation.db \
  "$IMG" >/dev/null
up=0
for _ in $(seq 1 45); do
  if docker exec "$APP" curl -fsS --max-time 4 http://127.0.0.1:8090/api/v1/info >/dev/null 2>&1; then up=1; break; fi
  sleep 2
done
[ "$up" = 1 ] || fail "image never served /api/v1/info within 90s"

# Visuals target the same newly built image with a separate synthetic dataset.
# The public demo can intentionally remain on an approved older image.
docker run -d --name "$DEMO" --network "$NET" --tmpfs /data \
  -e LEPTOS_SITE_ADDR=0.0.0.0:8090 -e LOCALSKY_DEMO=1 \
  -e HISTORY_DB_PATH=/data/irrigation.db "$IMG" >/dev/null
up=0
for _ in $(seq 1 45); do
  if docker exec "$DEMO" curl -fsS --max-time 4 http://127.0.0.1:8090/api/v1/info >/dev/null 2>&1; then up=1; break; fi
  sleep 2
done
[ "$up" = 1 ] || fail "synthetic image never served /api/v1/info within 90s"

# The suite, in order: the fresh-install walk configures the instance, then
# axe runs on the configured pages (two invocations, because Playwright
# orders files by name and axe must see the configured install). The
# specs are COPIED into the Playwright container rather than bind-mounted:
# on the docker-outside-of-docker runner the checkout lives inside the
# job container, and a bind mount would resolve on the host, where it
# does not exist (the same reason the smoke step curls from inside the
# app container). The report is copied back out for the upload step.
# --ipc=host for chromium's shared memory.
# The stock browser image does not include the DejaVu families selected by
# the visual fixture. Install and verify them before comparing baselines.
PW=localsky-e2e-pw
docker rm -f "$PW" >/dev/null 2>&1 || true
docker create --name "$PW" --network "$NET" --ipc=host -w /e2e \
  -e CI=true -e FRESH_INSTALL=1 -e BASE_URL="http://${APP}:8090" \
  -e DEMO_BASE_URL="http://${DEMO}:8090" \
  "$PW_IMG" sh -c 'apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends fonts-dejavu-core >/dev/null \
    && test "$(fc-match -f "%{family}" "DejaVu Sans")" = "DejaVu Sans" \
    && test "$(fc-match -f "%{family}" "DejaVu Sans Mono")" = "DejaVu Sans Mono" \
    && npm ci --no-audit --no-fund >/dev/null \
    && npx playwright test fresh-install.spec.ts --reporter=list --retries=0 --forbid-only \
    && npx playwright test axe.spec.ts editor-drawer.spec.ts intent-safety.spec.ts forecast-tracks.spec.ts rachio-scan.spec.ts zone-bind.spec.ts --reporter=list --retries=0 --forbid-only \
    && BASE_URL="$DEMO_BASE_URL" npx playwright test smoke.spec.ts radar.spec.ts demo.spec.ts --reporter=list --retries=0 --forbid-only --update-snapshots=none' >/dev/null
for f in package.json package-lock.json playwright.config.ts "$E2E_DIR"/*.spec.ts; do
  case "$f" in /*) src="$f" ;; *) src="$E2E_DIR/$f" ;; esac
  docker cp "$src" "$PW:/e2e/$(basename "$src")"
done
docker cp "$E2E_DIR/fixtures" "$PW:/e2e/fixtures"
docker cp "$E2E_DIR/smoke.spec.ts-snapshots" "$PW:/e2e/smoke.spec.ts-snapshots"
status=0
docker start -a "$PW" || status=$?
rm -rf "$E2E_DIR/playwright-report" "$E2E_DIR/test-results"
docker cp "$PW:/e2e/playwright-report" "$E2E_DIR/playwright-report" >/dev/null 2>&1 || true
docker cp "$PW:/e2e/test-results" "$E2E_DIR/test-results" >/dev/null 2>&1 || true
docker rm -f "$PW" >/dev/null 2>&1 || true
[ "$status" = 0 ] || fail "playwright reported failures (exit $status)"
echo "PRE-MERGE E2E OK: persisted wizard, accessibility, interactions, actual render and fixed visual baselines"
cleanup
