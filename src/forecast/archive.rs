//! Archive committed store snapshots. Database work never delays a provider
//! fetch or forecast arbitration; failed writes retry from the stores.
use super::ForecastStore;
use crate::persistence::forecast_archive::{ForecastArchiveStore, RETENTION_DAYS};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub fn spawn(store: Arc<ForecastStore>, archive: ForecastArchiveStore) {
    let mut merged = store.subscribe();
    let mut tracks = store.tracks.subscribe();
    tokio::spawn(async move {
        let mut recorded: BTreeMap<String, (i64, String)> = BTreeMap::new();
        let mut prune_at = 0_i64;
        loop {
            let mut snapshots = store
                .tracks
                .snapshots()
                .into_iter()
                .map(|(id, model, snapshot)| (id, Some(model), snapshot))
                .collect::<Vec<_>>();
            snapshots.push(("merged".into(), None, store.snapshot().as_ref().clone()));
            for (id, model, snapshot) in snapshots {
                let identity = (snapshot.last_refresh_epoch, snapshot.source_label.clone());
                if snapshot.last_refresh_epoch <= 0 || recorded.get(&id) == Some(&identity) {
                    continue;
                }
                match archive.record(&id, model.as_deref(), &snapshot).await {
                    Ok(_) => {
                        recorded.insert(id, identity);
                    }
                    Err(error) => {
                        tracing::warn!(error = %crate::diagnostics::from_anyhow(&error, "forecast archive write"), track = %id, "forecast archive write failed; will retry")
                    }
                }
            }
            let now = chrono::Utc::now().timestamp();
            if now >= prune_at {
                match archive.prune_older_than(now - RETENTION_DAYS * 86400).await {
                    Ok(_) => prune_at = now + 86400,
                    Err(error) => {
                        tracing::warn!(error = %crate::diagnostics::from_anyhow(&error, "forecast archive retention"), "forecast archive retention failed; will retry")
                    }
                }
            }
            tokio::select! {
                result = merged.changed() => { if result.is_err() { break; } }
                result = tracks.changed() => { if result.is_err() { break; } }
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
        }
    });
}
