# Multi-stage build for the Leptos full-stack weather app.
# Stage 1 compiles the SSR binary + WASM client via cargo-leptos.
# Stage 2 ships only the binary, the static `site/` bundle, and CA roots.

FROM rust:slim-trixie@sha256:082a5849a6870672b5f7a5bf4eddc71723fce38756fd834a0d734a5306a310ab AS builder

RUN apt-get update && apt-get install -y \
        pkg-config libssl-dev curl wget build-essential \
    && rm -rf /var/lib/apt/lists/*

# cargo-binstall for fast cargo-leptos install (avoids OOM on source build).
# Arch-aware: the build runs natively on both amd64 and arm64 runners, so
# the bootstrap binary must match the build machine.
RUN arch="$(uname -m)" \
    && wget -q "https://github.com/cargo-bins/cargo-binstall/releases/latest/download/cargo-binstall-${arch}-unknown-linux-musl.tgz" \
    && tar -xf "cargo-binstall-${arch}-unknown-linux-musl.tgz" \
    && cp cargo-binstall /usr/local/cargo/bin \
    && rm "cargo-binstall-${arch}-unknown-linux-musl.tgz" cargo-binstall

RUN cargo binstall cargo-leptos -y
# mdbook builds the bundled documentation (docs/ -> docs/book) that the
# server serves same-origin at /docs. Pinned to a version compatible with
# docs/book.toml (mdbook >= 0.5; the book uses [output.html] search/fold/
# print + preprocessor.links/index, all stable in the 0.5 line). binstall
# fetches the prebuilt release binary, no source compile.
RUN cargo binstall mdbook --version "^0.5" -y
RUN rustup target add wasm32-unknown-unknown

# Pin the dart-sass version cargo-leptos pulls. 1.86.0's binary
# bundle ships a broken extracted dart launcher (`dart: not found`)
# in current builds; 1.99.0 has a working one. Cargo-leptos itself
# nudged toward this version in its install warning.
ENV LEPTOS_SASS_VERSION=1.99.0

WORKDIR /build
# Copy Cargo.lock so the build is reproducible. Without it, cargo would
# re-resolve every transitive on every build; a tachys 0.2.x patch with a
# hydration regression once shipped this way and the WASM panicked on
# first paint. Pinning the lockfile keeps SSR + WASM on the exact set the
# repo was tested with.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY style ./style
COPY public ./public
# Documentation sources for the in-app /docs server. `mdbook build docs`
# (after the leptos build below) renders docs/src/*.md -> docs/book.
COPY docs ./docs

# Commit sha for the service-worker cache namespace. option_env!("GIT_SHA")
# in src/sw.rs reads this at compile time so every deploy emits a byte-different
# /sw.js, which is what forces browsers to install the new SW and nuke the old
# caches (otherwise the SW version is a static "-dev" and clients freeze on
# stale WASM). Passed as a --build-arg by the CI build workflow.
ARG GIT_SHA=dev
ENV GIT_SHA=${GIT_SHA}

RUN cargo leptos build --release

# Render the bundled docs AFTER the app build so a docs change alone does
# not invalidate the (slow) cargo build cache layer above. Output lands
# in /build/docs/book and is copied into the site root in the runtime
# stage so the server serves it at /docs.
RUN mdbook build docs

# ── Runtime ──
FROM debian:trixie-slim@sha256:4e401d95de7083948053197a9c3913343cd06b706bf15eb6a0c3ccd26f436a0e

RUN apt-get update && apt-get install -y \
        ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 localsky \
    && useradd --system --uid 10001 --gid 10001 --home-dir /app --shell /usr/sbin/nologin localsky

WORKDIR /app
COPY --from=builder --chown=10001:10001 /build/target/release/localsky /app/localsky
COPY --from=builder --chown=10001:10001 /build/target/site /app/site
# Bundled documentation, served same-origin at /docs (LEPTOS_SITE_ROOT=
# "site" -> /app/site, the docs ServeDir roots at <site_root>/docs).
# Placed after the site COPY so it lands inside the served static root.
COPY --from=builder --chown=10001:10001 /build/docs/book /app/site/docs

# /data and /keys are volume mounts; document the uid the container expects.
# Bind-mount hosts should chown 10001:10001 or pass --user 0:0 to override.
RUN mkdir -p /data /keys && chown -R 10001:10001 /data /keys

ENV LEPTOS_SITE_ADDR="0.0.0.0:8090"
ENV LEPTOS_SITE_ROOT="site"
ENV RUST_LOG="info"

USER 10001:10001
EXPOSE 8090
EXPOSE 50222/udp

# /api/v1/info is the cheapest stable endpoint; returns service +
# api_version metadata. start-period gives the SSR boot + initial source
# warmup time before the first failure counts.
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
    CMD curl --fail --silent --show-error --max-time 4 \
        http://127.0.0.1:8090/api/v1/info > /dev/null || exit 1

CMD ["/app/localsky"]
