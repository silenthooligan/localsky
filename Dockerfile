# Multi-stage build for the Leptos full-stack weather app.
# Stage 1 compiles the SSR binary + WASM client via cargo-leptos.
# Stage 2 ships only the binary, the static `site/` bundle, and CA roots.

FROM rust:slim-trixie@sha256:3b05f7c617a200c41c3506097f0d15fc193a1c93bfd8f141007b47cac8f95d3c AS builder

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

# hash-files emits content-hashed /pkg names + a hash.txt manifest. leptos reads
# that manifest at runtime from current_exe().parent()/hash.txt (next to the
# binary), NOT the site root, so stage it at a known path. Fail loudly if it's
# missing: shipping hashed files without the manifest makes the HTML emit
# hashless URLs that 404 the WASM.
RUN HASHTXT="$(find /build/target -name hash.txt -print -quit)" \
    && { [ -n "$HASHTXT" ] || HASHTXT="$(find /build -name hash.txt -print -quit)"; } \
    && test -n "$HASHTXT" \
    && echo "hash.txt found at: $HASHTXT" \
    && cp "$HASHTXT" /build/hash.txt \
    && echo "=== hash.txt contents ===" && cat /build/hash.txt

# Render the bundled docs AFTER the app build so a docs change alone does
# not invalidate the (slow) cargo build cache layer above. Output lands
# in /build/docs/book and is copied into the site root in the runtime
# stage so the server serves it at /docs.
RUN mdbook build docs

# ── Runtime ──
FROM debian:trixie-slim@sha256:4e401d95de7083948053197a9c3913343cd06b706bf15eb6a0c3ccd26f436a0e

RUN apt-get update && apt-get install -y \
        ca-certificates libssl3 curl gosu \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 localsky \
    && useradd --system --uid 10001 --gid 10001 --home-dir /app --shell /usr/sbin/nologin localsky

WORKDIR /app
COPY --from=builder --chown=10001:10001 /build/target/release/localsky /app/localsky
COPY --from=builder --chown=10001:10001 /build/target/site /app/site
# hash.txt MUST sit next to the binary; leptos reads it from
# current_exe().parent()/hash.txt to map /pkg names to their hashed forms.
COPY --from=builder --chown=10001:10001 /build/hash.txt /app/hash.txt
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
ENV RUST_LOG="info"
# Emit content-hashed /pkg URLs (reads /app/hash.txt). No compile-time fallback
# in leptos_config; must be set here or names go hashless and 404 the hashed
# files on disk.
ENV LEPTOS_HASH_FILES="true"

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
