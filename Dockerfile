# Multi-stage build for the Leptos full-stack weather app.
# Stage layout (BuildKit):
#   toolchain -> src -> gate     (CI quality gate: fmt/clippy/test/hydrate)
#                    -> builder  (release SSR binary + WASM bundle + docs)
#   runtime (void-base) ships only the binary, site/, hash.txt, docs.
#
# CI runs `docker build --target gate .` BEFORE the release build, so the
# gate and the image build share the toolchain layers AND the two BuildKit
# cache mounts below (cargo registry + target). On a warm runner neither
# job re-downloads a toolchain or recompiles dependencies; only this crate
# rebuilds. The cache lives in the runner daemon's BuildKit state; the
# workflow prunes it back to a budget after each build.

FROM rust:slim-trixie@sha256:5c6f46a6e4472ab1ca7ba7d494e6677f2f219ebc02f32025d3986f057635ec9c AS toolchain

RUN apt-get update && apt-get install -y \
        pkg-config libssl-dev curl wget build-essential \
    && rm -rf /var/lib/apt/lists/*

# cargo-binstall for fast cargo-leptos install (avoids OOM on source build).
# Arch-aware: the build runs natively on both amd64 and arm64 runners, so
# the bootstrap binary must match the build machine.
#
# Supply-chain pin: fetch a SPECIFIC release, never /latest/download. An
# unpinned /latest meant the bootstrap binary could change under us between
# builds (a silent supply-chain surface on a tool we run with full build-time
# privileges). Bump CARGO_BINSTALL_VERSION deliberately when updating.
ARG CARGO_BINSTALL_VERSION=v1.20.0
RUN arch="$(uname -m)" \
    && wget -q "https://github.com/cargo-bins/cargo-binstall/releases/download/${CARGO_BINSTALL_VERSION}/cargo-binstall-${arch}-unknown-linux-musl.tgz" \
    && tar -xf "cargo-binstall-${arch}-unknown-linux-musl.tgz" \
    && cp cargo-binstall /usr/local/cargo/bin \
    && rm "cargo-binstall-${arch}-unknown-linux-musl.tgz" cargo-binstall

# Pin cargo-leptos to the line that targets leptos 0.8 (this repo's version),
# instead of letting binstall pull whatever is newest. A future cargo-leptos
# major could change the build contract or default versions under us; the ^0.3
# constraint keeps builds reproducible while still taking patch fixes. Bump
# deliberately alongside a leptos major.
RUN cargo binstall cargo-leptos --version "^0.3" -y
# mdbook builds the bundled documentation (docs/ -> docs/book) that the
# server serves same-origin at /docs. Pinned to a version compatible with
# docs/book.toml (mdbook >= 0.5; the book uses [output.html] search/fold/
# print + preprocessor.links/index, all stable in the 0.5 line). binstall
# fetches the prebuilt release binary, no source compile.
RUN cargo binstall mdbook --version "^0.5" -y
RUN rustup target add wasm32-unknown-unknown
# The gate stage runs fmt + clippy; the slim image's minimal profile may not
# carry them. Idempotent when they are already present.
RUN rustup component add clippy rustfmt

# Pin the dart-sass version cargo-leptos pulls. 1.86.0's binary
# bundle ships a broken extracted dart launcher (`dart: not found`)
# in current builds; 1.99.0 has a working one. Cargo-leptos itself
# nudged toward this version in its install warning.
ENV LEPTOS_SASS_VERSION=1.99.0

WORKDIR /build

# ── Sources (shared by gate + builder) ──
FROM toolchain AS src
# Copy Cargo.lock so the build is reproducible. Without it, cargo would
# re-resolve every transitive on every build; a tachys 0.2.x patch with a
# hydration regression once shipped this way and the WASM panicked on
# first paint. Pinning the lockfile keeps SSR + WASM on the exact set the
# repo was tested with.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY style ./style
COPY public ./public
# Documentation sources for the in-app /docs server (rendered in builder).
COPY docs ./docs

# ── Quality gate (CI: docker build --target gate) ──
# fmt + clippy + the ssr test suite + the hydrate/wasm check, all against the
# same cache mounts the release build uses, so the whole gate runs warm. One
# RUN so a failure stops the build with that step's output.
FROM src AS gate
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo fmt --check \
    && cargo clippy --no-default-features --features ssr --all-targets -- -D warnings \
    && INSTA_UPDATE=no cargo test --no-default-features --features ssr --lib \
    && cargo check --no-default-features --features hydrate --target wasm32-unknown-unknown \
    && echo gate-ok > /gate-ok

# ── Release build ──
FROM src AS builder

# Commit sha for the service-worker cache namespace. option_env!("GIT_SHA")
# in src/sw.rs reads this at compile time so every deploy emits a byte-different
# /sw.js, which is what forces browsers to install the new SW and nuke the old
# caches (otherwise the SW version is a static "-dev" and clients freeze on
# stale WASM). Passed as a --build-arg by the CI build workflow. Lives in this
# stage only so the sha churn never touches the gate stage's layers.
ARG GIT_SHA=dev
ENV GIT_SHA=${GIT_SHA}

# NOTE (build memory): the front-end wasm-release compile (opt-level=z) peaks
# above 16GB and OOM-kills on an under-provisioned runner ("cannot allocate
# memory" / SIGKILL). CI pins this build to runner-210 (32GB); the wasm-release
# profile uses thin LTO + 16 codegen units to keep the peak in check. If this
# step starts OOM-ing again as the crate grows, raise the runner's RAM rather
# than lowering opt-level (which would balloon the PWA bundle).
#
# target/ is a cache mount: artifacts persist across builds (deps stay
# compiled; only this crate rebuilds), but nothing inside the mount is part
# of the image, so everything the runtime stage needs is copied OUT to
# /build/out in the same RUN. target/site is regenerated from scratch each
# build (cheap assembly) so stale content-hashed pkg files from earlier
# builds cannot accumulate in the cache and leak into the image.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    rm -rf target/site \
    && cargo leptos build --release \
    && mkdir -p /build/out \
    && cp target/release/localsky /build/out/localsky \
    && cp -r target/site /build/out/site \
    && HASHTXT="$(find /build/target -name hash.txt -print -quit)" \
    && test -n "$HASHTXT" \
    && echo "hash.txt found at: $HASHTXT" \
    && cp "$HASHTXT" /build/out/hash.txt \
    && echo "=== hash.txt contents ===" && cat /build/out/hash.txt

# Render the bundled docs AFTER the app build so a docs change alone does
# not invalidate the (slow) cargo build layer above. Output lands in
# /build/docs/book and is copied into the site root in the runtime stage.
# The {{LOCALSKY_VERSION}} token in docs/src is substituted from Cargo.toml
# first, so the docs banner always tracks the exact version being built
# (introduction.md used to carry a hand-bumped literal that went stale).
RUN VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2) \
    && grep -rl '{{LOCALSKY_VERSION}}' docs/src \
       | xargs -r sed -i "s/{{LOCALSKY_VERSION}}/${VERSION}/g" \
    && mdbook build docs

# ── Runtime ──
FROM debian:trixie-slim

# ca-certificates + OpenSSL 3 libs for outbound HTTPS, curl for the
# healthcheck, tzdata for local-time rendering, and gosu for the
# fix-perms-then-drop entrypoint (docker-entrypoint.sh chowns /data and /keys
# as root, then drops to 10001:10001). The uid:10001 app user is created here
# (the public base has no pre-baked app user).
# The upgrade pulls Debian security fixes the base tag has not been rebuilt
# with yet; without it the trivy gate fails on OS packages whose fixed
# versions already sit in the security repo.
RUN apt-get update && apt-get upgrade -y --no-install-recommends \
    && apt-get install -y --no-install-recommends \
        ca-certificates libssl3 curl tzdata gosu \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --user-group --no-create-home --shell /usr/sbin/nologin localsky

WORKDIR /app
# Artifacts are staged at /build/out by the builder stage: its target dir is a
# BuildKit cache mount, so nothing under /build/target exists in the image
# layers at COPY time. These paths MUST track the internal Dockerfile's
# runtime-stage COPYs.
COPY --from=builder --chown=10001:10001 /build/out/localsky /app/localsky
COPY --from=builder --chown=10001:10001 /build/out/site /app/site
# hash.txt MUST sit next to the binary, leptos reads it from
# current_exe().parent()/hash.txt to map /pkg names to their hashed forms.
COPY --from=builder --chown=10001:10001 /build/out/hash.txt /app/hash.txt
# Bundled documentation, served same-origin at /docs (LEPTOS_SITE_ROOT=
# "site" -> /app/site, the docs ServeDir roots at <site_root>/docs).
# Placed after the site COPY so it lands inside the served static root.
COPY --from=builder --chown=10001:10001 /build/docs/book /app/site/docs

# /data and /keys are volume mounts. The entrypoint chowns them to the app uid
# at startup and drops to the non-root user, so any volume shape (fresh bind
# mount, named volume, or an upgrade from a root-owned volume) just works with
# no operator action.
RUN mkdir -p /data /keys && chown -R 10001:10001 /data /keys

ENV LEPTOS_SITE_ADDR="0.0.0.0:8090"
ENV LEPTOS_SITE_ROOT="site"
# Emit content-hashed /pkg URLs (reads /app/hash.txt). No compile-time fallback
# in leptos_config, must be set here or names go hashless and 404 the hashed
# files on disk.
ENV LEPTOS_HASH_FILES="true"
ENV RUST_LOG="info"

# Fix-perms-then-drop: the container starts as root, the entrypoint chowns the
# writable mounts, then gosu-drops to uid 10001 to run the app unprivileged.
# No USER directive here on purpose; the entrypoint owns the privilege drop.
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
EXPOSE 8090
EXPOSE 50222/udp

# /api/v1/info is the cheapest stable endpoint; returns service +
# api_version metadata. start-period gives the SSR boot + initial source
# warmup time before the first failure counts.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl --fail --silent --show-error --max-time 4 \
        http://127.0.0.1:8090/api/v1/info > /dev/null || exit 1

CMD ["/app/localsky"]
