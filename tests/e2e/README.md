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
