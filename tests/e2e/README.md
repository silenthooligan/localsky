# LocalSky UI smoke tests (Playwright)

The Rust CI proves the WASM UI **compiles**. These tests prove it **renders**:
they load the five pages a real user lives in, assert each shows content and a
nav (so it did not hydrate blank or fall through to NotFound), fail on any
uncaught console error or WASM panic, and capture a baseline screenshot so
visual regressions are caught instead of silently shipped.

Pages covered: `/` (weather home), `/irrigation`, `/zones`, `/history`,
`/settings`.

## Run it

Point at any LocalSky instance running with `LOCALSKY_DEMO=1` (deterministic
synthetic data, so screenshots are stable). Default target is a local demo
container on `:8091`.

```bash
cd tests/e2e
npm install
npm run install-browsers          # one-time: chromium + deps

# against a local demo container (docker run -e LOCALSKY_DEMO=1 -p 8091:8091 ...)
npm test

# against the live post-deploy canary
BASE_URL=https://demo.localsky.io npm test

# refresh screenshot baselines after an intentional UI change
npm test -- --update-snapshots
```

First run records baseline screenshots under `__screenshots__/`; commit them.
Later runs diff against those baselines and fail on layout drift beyond a small
threshold. The forecast values and clock jitter between runs, so the baselines
are about layout, not exact pixels (`maxDiffPixelRatio`).

## How it fits CI

The operator's CI runs this suite as a **post-deploy canary** against the demo
instance (which tracks prod on `:latest`), on a schedule and on demand. It is not a pre-merge gate because that would need a full Rust build in
CI; the canary catches "the last deploy broke a page" within minutes, which is
the failure this is meant to stop.

## The pre-merge gate (fresh install, wizard, zone edit, axe)

`fresh-install.spec.ts` and `axe.spec.ts` run in the build workflow against the
freshly built image booted on an EMPTY volume (`tests/e2e/pre-merge.sh <image>`,
which needs only docker). The first walks the /setup redirect, the wizard's
happy path including the watering-rules step, and a zone edit from settings;
the second runs axe-core (WCAG 2.x A/AA) on the five pages of the install the
wizard just configured and fails on any violation. `fresh-install.spec.ts` is
skipped unless `FRESH_INSTALL=1`, so the daily demo canary never runs it.

To run them by hand against a local build:

```sh
# an empty data dir, hashed assets like the image
mkdir -p /tmp/e2e-data
CONFIG_PATH=/tmp/e2e-data/localsky.toml HISTORY_DB_PATH=/tmp/e2e-data/irrigation.db LEPTOS_SITE_ADDR=127.0.0.1:18090 LEPTOS_SITE_ROOT=target/site LEPTOS_HASH_FILES=true ./target/release/localsky &
cd tests/e2e
FRESH_INSTALL=1 BASE_URL=http://127.0.0.1:18090 npx playwright test fresh-install.spec.ts
BASE_URL=http://127.0.0.1:18090 npx playwright test axe.spec.ts
```

The specs wait for `html[data-hydrated="true"]`, which the app sets once the
WASM has attached its handlers; typing before that goes into a page nobody is
listening to.

The fixed visual suite freezes external image artwork and waits for application
content independently of third-party tile network idle. Radar checks retain their
own layer and tile assertions. Intent tests cover overnight session grouping and
keyboard access to the restore picker; all writes in those tests are intercepted.

The fixed visual suite covers desktop and phone layouts with the same synthetic
inputs. It also checks page width, all mobile navigation hit targets, and text
containment inside zone summary tiles. Review actual captures before changing
any baseline; the private capture helper removes only its copied references so
small differences cannot leave an older screenshot masquerading as a fresh one.
