# Contributing to LocalSky

LocalSky combines a weather app, an irrigation planner, and device control. Changes should make those parts easier to understand and reliable on real installations.

## Report a problem

Use the [issue form](https://github.com/silenthooligan/localsky/issues/new/choose). Include the server version from `/api/v1/info`, installation type, relevant hardware, the action you took, and what happened.

For a failed request, include the status, error code, request ID, and timestamp. Review logs and diagnostics for credentials or personal information before posting. Report security vulnerabilities through the [private reporting process](SECURITY.md).

## Development setup

The repository's [CI workflow](.github/workflows/ci.yml) is the reference for the tested toolchain and checks. Tool versions and verified downloads are recorded in [.github/build-tools.json](.github/build-tools.json).

For local UI development, install Rust, the `wasm32-unknown-unknown` target, and the listed version of `cargo-leptos`. Then:

```bash
git clone https://github.com/silenthooligan/localsky.git
cd localsky
LOCALSKY_DEMO=1 cargo leptos watch
```

Use demo mode and isolated test data while developing. Keep a development instance disconnected from real irrigation controllers.

## Find the code

| Area | Directory |
|---|---|
| Watering math, rules, and catalogs | `src/engine/` |
| Shared weather data and selection | `src/weather/` |
| Weather adapters | `src/sources/` |
| Controller adapters | `src/controllers/` |
| Device contracts | `src/ports/` |
| Configuration | `src/config/` |
| Database and migrations | `src/persistence/` |
| API | `src/api/` |
| Web app | `src/components/` |
| Product guide | `docs/src/` |

## Validate a change

Use checks appropriate to the changed behavior:

```bash
cargo fmt --all -- --check
cargo clippy --no-default-features --features ssr --all-targets -- -D warnings
cargo test --locked --no-default-features --features ssr --tests
```

UI changes also need the hydration build and browser checks. CI exercises supported native architectures and builds the production image.

For documentation, regenerate the connector exports:

```bash
python3 .github/scripts/build-doc-assets.py
python3 -m unittest discover -s .github/scripts -p 'test_doc_*.py' -v
```

The Documentation workflow builds the guide and checks links, accessibility, mobile layout, and API examples.

## Device and science changes

A weather adapter must report capabilities, timestamps, units, and unavailable readings accurately. A successful poll must not make an old observation appear fresh.

A controller adapter must distinguish command acceptance from reported state and support bounded shutoff behavior. Test failures and uncertain outcomes, not only successful requests.

Changes to plant, soil, or evapotranspiration calculations need a source for the method or coefficient and a test demonstrating the intended behavior. Distinguish a published method from a locally chosen default.

## Open a pull request

Describe the problem, the resulting behavior, and the checks you ran. Keep the scope reviewable. Include screenshots for visible changes and migration notes when stored data or API responses change.

Product prose should be direct and specific. Explain what works, what a user should do next, and any relevant limit without marketing claims the implementation cannot support.

[Release automation](.github/RELEASING.md) · [Code of conduct](CODE_OF_CONDUCT.md)
