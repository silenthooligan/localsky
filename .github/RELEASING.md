# Release automation

Pushing a version tag starts one `release` workflow:

1. Check the tag, source commit, Cargo version and changelog.
2. Run the complete CI matrix, including release-tool regression tests.
3. Build amd64 and arm64 images on native runners. Start each image with no
   network and temporary data, check health/pages/assets, then scan that exact
   image with Trivy.
4. Combine the verified image digests and promote GHCR
   and the configured Docker Hub mirror.
5. Publish the GitHub release. This job depends on all previous jobs succeeding.

PRs run the same native image builds, startup checks and scans without publishing.
Release candidates are pushed by digest with BuildKit provenance; version and
Latest tags are created only after both architectures pass their checks.
No polling script waits for unrelated workflow runs. The release run contains
the checks and artifacts that authorized its publication.

## Tool downloads

`.github/build-tools.json` pins wasm-bindgen, Binaryen and Dart Sass archives for
both architectures, along with cargo-leptos. Before compilation, the shared
`build-tools` action verifies the Cargo.lock wasm-bindgen version, downloads with
bounded retries and falls back to GitHub's asset API if the release URL fails.
Cached archives are checked against the committed SHA-256 on every use. A hash
mismatch fails the build. Executables are freshly extracted and version-checked.

The Docker build uses the normal repository Dockerfile with a generated build-only
tool COPY/PATH addition and an exact cargo-leptos pin. Runtime instructions and
application sources are unchanged. If the toolchain insertion points change,
preparation stops with an explicit error. Update both architecture assets and
checksums when upgrading tools or the wasm-bindgen lockfile entry.

## Recovery

Use **Re-run failed jobs** on the release run. Successful jobs remain available;
image digest and validation artifacts are retained for 30 days. GitHub reruns
the original commit. Transient download/API errors are retried, but failed tests,
startup checks, hash validation and security scans still block publication.

For a fresh attempt of a tag that contains this workflow:

```sh
gh workflow run release.yml --ref vX.Y.Z
```

Dispatching a branch is rejected. Already-published releases are explicit no-ops:
no rebuild, image retagging, note replacement or Latest change. An older version
cannot move Latest backwards. Never move a published version tag to obtain newer
workflow code. Historical failed runs remain an accurate record; a later recovery
does not rewrite their results.

To exercise the whole dependency chain on a branch without publishing:

```sh
gh workflow run release.yml --ref main -f verify_only=true
```

Verification mode always runs the source and image checks, even if the package
version is already released. It disables image promotion and release creation.

No LocalSky version bump is needed for workflow-only maintenance.
