# Multi-stage build for the Leptos full-stack weather app.
# Stage 1 compiles the SSR binary + WASM client via cargo-leptos.
# Stage 2 ships only the binary, the static `site/` bundle, and CA roots.

FROM rust:slim-bookworm AS builder

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

RUN cargo leptos build --release

# ── Runtime ──
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
        ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /build/target/release/localsky /app/localsky
COPY --from=builder /build/target/site /app/site

ENV LEPTOS_SITE_ADDR="0.0.0.0:8090"
ENV LEPTOS_SITE_ROOT="site"
ENV RUST_LOG="info"

EXPOSE 8090
EXPOSE 50222/udp
CMD ["/app/localsky"]
