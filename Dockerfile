# Multi-stage build for the Leptos full-stack weather app.
# Stage 1 compiles the SSR binary + WASM client via cargo-leptos.
# Stage 2 ships only the binary, the static `site/` bundle, and CA roots.

FROM rust:slim-bookworm@sha256:b5f842fac1e3b4ff718a652a8e0173b62d9403ec826ef4998880b9347db30684 AS builder

RUN apt-get update && apt-get install -y \
        pkg-config libssl-dev curl wget build-essential \
    && rm -rf /var/lib/apt/lists/*

# cargo-binstall for fast cargo-leptos install (avoids OOM on source build).
RUN wget https://github.com/cargo-bins/cargo-binstall/releases/latest/download/cargo-binstall-x86_64-unknown-linux-musl.tgz \
    && tar -xvf cargo-binstall-x86_64-unknown-linux-musl.tgz \
    && cp cargo-binstall /usr/local/cargo/bin \
    && rm cargo-binstall-x86_64-unknown-linux-musl.tgz cargo-binstall

RUN cargo binstall cargo-leptos -y
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

# Commit sha for the service-worker cache namespace. option_env!("GITEA_SHA")
# in src/sw.rs reads this at compile time so every deploy emits a byte-different
# /sw.js, which is what forces browsers to install the new SW and nuke the old
# caches (otherwise the SW version is a static "-dev" and clients freeze on
# stale WASM). Passed as a --build-arg by .gitea/workflows/build.yml.
ARG GITEA_SHA=dev
ENV GITEA_SHA=${GITEA_SHA}

RUN cargo leptos build --release

# ── Runtime ──
FROM debian:bookworm-slim@sha256:66117fe525ba266a4d9de1dc238fa9b9d2fe78ff9d0836b8348d133e836f39b5

RUN apt-get update && apt-get install -y \
        ca-certificates libssl3 curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 localsky \
    && useradd --system --uid 10001 --gid 10001 --home-dir /app --shell /usr/sbin/nologin localsky

WORKDIR /app
COPY --from=builder --chown=10001:10001 /build/target/release/localsky /app/localsky
COPY --from=builder --chown=10001:10001 /build/target/site /app/site

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
