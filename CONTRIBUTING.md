# Contributing to LocalSky

Thank you for considering a contribution. LocalSky is built for homeowners by homeowners, and the codebase tries to stay legible enough that a curious operator can read it end to end. Contributions that keep that property are the most valuable.

## Development setup

You need:

- Rust stable (1.75 or newer; check `rust-toolchain.toml` when added)
- `cargo-leptos` for the WASM hydration build:

  ```bash
  cargo install cargo-leptos --locked
  ```
- (Optional) Docker for the production-shape build

Clone and run:

```bash
git clone https://github.com/silenthooligan/localsky.git
cd localsky
cargo leptos watch
```

The dev server lives at http://localhost:8090.

For demo mode (no external dependencies):

```bash
LOCALSKY_DEMO=1 cargo leptos watch
```

## Code layout

The engine is a ports-and-adapters Rust workspace:

- `src/ports/` defines the trait surface (`WeatherSource`, `IrrigationController`, `LlmProvider`, `NotificationSink`, `ConfigStore`)
- `src/engine/` is pure logic: FAO-56 ET, water balance, cycle-and-soak, skip rules, grass + soil catalogs
- `src/config/` is the typed schema + TOML loader + first-run wizard
- `src/persistence/` is SQLite with versioned migrations
- `src/sources/` is the weather adapter set
- `src/controllers/` is the irrigation adapter set
- `src/llm/providers/` is the LLM adapter set
- `src/components/` is the Leptos UI (shared SSR + WASM hydrate)

## Style

- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before pushing.
- Default to no comments unless a hidden constraint or non-obvious invariant warrants one.
- Don't write em dashes in user-facing prose (README, CHANGELOG, commit subjects). Use commas, periods, parens.
- Tests use `tokio::test` for async paths and `Connection::open_in_memory()` for SQLite paths so the suite is hermetic.

## Adding a weather source

Implement the `WeatherSource` trait in `src/sources/<name>.rs`. Capability advertisement and per-field priority matter for the merge engine. See `src/sources/demo_replay.rs` for a minimal example. Add an enum variant to `SourceKind` in `src/config/schema.rs` so the config plane can express your source.

## Adding an irrigation controller

Implement `IrrigationController` in `src/controllers/<name>.rs`. See `src/controllers/dry_run.rs` for a minimal example and `src/controllers/opensprinkler_direct.rs` for a full HTTP-API integration. Add a variant to `ControllerKind` in the schema.

## Adding a grass species or soil texture

Edit `src/engine/species_catalog.rs` or `src/engine/soil_catalog.rs`. New entries must cite a public source (UF/IFAS, FAO-56, USDA NRCS, or peer-reviewed). Open a PR with the citation in the comment block.

## Pull requests

- Open against `main`.
- Title in conventional-commits style: `feat(scope): summary`, `fix(scope): summary`, `docs:`, `test:`, `refactor:`, etc.
- Include a test plan when relevant.
- One concern per PR; small, reviewable diffs land faster than sweeping refactors.

## Reporting bugs

Use the bug template under [Issues](../../issues/new/choose). Include:

- Version (`localsky --version` or check `/about` in the UI)
- OS + architecture
- Controller and source types you have configured
- Steps to reproduce
- Relevant log lines (redact secrets)

## Code of conduct

Be excellent to each other. See [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md).
