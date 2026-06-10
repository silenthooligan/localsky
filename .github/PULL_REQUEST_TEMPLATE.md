## What

<!-- One or two sentences. What does this PR change? -->

## Why

<!-- Link the issue, or describe the user-facing reason. Skip restating the diff. -->

## How to test

<!-- The minimum someone needs to verify this works. e.g. -->
<!-- 1. `LOCALSKY_DEMO=1 cargo leptos serve` -->
<!-- 2. Visit /irrigation -->
<!-- 3. Verify ... -->

## Checklist

- [ ] `cargo fmt` clean
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- [ ] `cargo test` passes
- [ ] No em dashes in user-facing prose (READMEs, docs, commit messages)
- [ ] If touching a new external system: ports/adapters boundary preserved
- [ ] If touching public API: doc + version bump under `/api/v1` policy
