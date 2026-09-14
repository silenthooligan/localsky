// SSR binary entry. The boot is `localsky::boot`, one function per
// phase; this file calls them in order and serves the result.

// recursion_limit is per-crate. The SSR route tree that once overflowed
// the default budget while compiling this BINARY (three release builds
// failed before the attribute was added here) now monomorphizes in the
// lib, in `boot::api`, under lib.rs's own copy; this one stays so a deep
// tree landing in the bin again fails the same way it did then, not
// mysteriously. Compile-time only, no runtime cost.
#![recursion_limit = "512"]

#[cfg(feature = "ssr")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use anyhow::Context;
    use localsky::boot;

    let logging = boot::logging::init();
    let storage = boot::storage::open()?;
    let config = boot::config::load(&storage).await?;
    if let Some(restore) = &storage.restore {
        restore.complete()?;
        tracing::info!("verified restore activated; database and configuration loaded");
    }
    let stores = boot::stores::build(&storage, &config).await;
    let control = boot::control::start(&storage, &config, &stores).await;
    let sources = boot::sources::start(&storage, &config, &stores).await;
    let boot::api::Served {
        app,
        addr,
        announcement,
    } = boot::api::build(&storage, &config, &stores, &control, &sources, logging)?;

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}: is another service holding this port?"))?;
    let bound = listener.local_addr().context("read bound HTTP address")?;
    tracing::info!("localsky listening on http://{bound}");
    if let Some(announcement) = announcement {
        announcement.start(bound);
    }
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .context("axum serve loop exited unexpectedly")?;
    Ok(())
}

#[cfg(not(feature = "ssr"))]
pub fn main() {
    // The WASM client is built via `lib.rs::hydrate`; this stub is here so
    // the same binary target compiles cleanly with the `hydrate` feature.
}
