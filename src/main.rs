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
    use std::future::IntoFuture;

    let logging = boot::logging::init();
    let storage = boot::storage::open()
        .map_err(|e| localsky::diagnostics::from_anyhow(&e, "startup storage and recovery"))?;
    let config = boot::config::load(&storage)
        .await
        .map_err(|e| localsky::diagnostics::from_anyhow(&e, "startup configuration"))?;
    if let Some(restore) = &storage.restore {
        restore
            .complete()
            .map_err(|e| localsky::diagnostics::from_error(&e, "startup recovery completion"))?;
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
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = shutdown_rx.await;
    })
    .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result.context("axum serve loop exited unexpectedly")?,
        signal = shutdown_signal() => {
            signal?;
            // Refuse new dispatches during the drain. Keep the durable active-run
            // ledger intact so normal startup recovery still reconciles it.
            control.registry.restart_hold().latch(vec!["LocalSky is shutting down".into()]);
            tracing::info!("shutdown requested; draining HTTP requests");
            let _ = shutdown_tx.send(());
            // SSE clients stay connected indefinitely. Give ordinary requests a
            // bounded drain, then close streams before Supervisor's kill timeout.
            match tokio::time::timeout(std::time::Duration::from_secs(3), &mut server).await {
                Ok(result) => result.context("HTTP shutdown failed")?,
                Err(_) => tracing::info!("closing remaining event streams for shutdown"),
            }
        }
    }
    Ok(())
}

#[cfg(feature = "ssr")]
async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

#[cfg(not(feature = "ssr"))]
pub fn main() {
    // The WASM client is built via `lib.rs::hydrate`; this stub is here so
    // the same binary target compiles cleanly with the `hydrate` feature.
}
