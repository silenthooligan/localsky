// Bridges forecast-capable bus sources into the shared ForecastStore.
//
// Historically the ForecastStore had exactly ONE writer: a hardcoded Open-Meteo
// refresher, spawned unconditionally. A user who configured NWS / OpenWeather /
// PirateWeather / Met.no got their forecast IGNORED. This consumer makes the
// forecast source-agnostic: every forecast-capable source emits
// SourceEvent::Forecast on the bus, and this bridge picks the highest-priority
// fresh source's snapshot, so the user's CHOSEN forecast provider drives the
// forecast (Open-Meteo becomes the lowest-priority default + failover).
//
// Arbitration is source-level (not per-field): forecast snapshots are whole
// daily/hourly arrays from one provider, so mixing fields across providers would
// produce an incoherent forecast. The current owner is kept until a
// higher-or-equal-priority source emits, or the owner goes stale (a source that
// stops refreshing must not pin the forecast forever).

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::forecast::store::ForecastStore;
use crate::ports::weather_source::SourceEvent;

/// How long the current forecast owner is trusted before a lower-priority
/// source may take over. Forecast refreshes run ~30 min, so 90 min tolerates a
/// couple of missed refreshes before failing over to a backup provider.
const FORECAST_OWNER_STALE_SECS: i64 = 90 * 60;

/// Spawn the bus -> ForecastStore bridge. `priority` maps each forecast
/// source's id to its `priority(ForecastDaily)`; an id we can't classify
/// defaults to 0 (below any real forecast source).
///
/// HOT-RELOAD: `priority` is a shared, swappable handle, not a boot-time
/// clone. The config-apply path (PUT /api/config + wizard apply) recomputes
/// `runtime::forecast_priority_map` from the freshly-saved config and
/// arc-swaps it in, so reassigning the forecast provider (or repinning
/// `forecast_provider`) re-ranks the bridge on the very next forecast emit
/// with no restart. Each event re-`load()`s the current map; a stale owner
/// still fails over by the same staleness rule, so a re-rank takes effect at
/// the next refresh of the newly-winning source (within one forecast cycle).
pub fn spawn(
    mut rx: broadcast::Receiver<SourceEvent>,
    store: Arc<ForecastStore>,
    priority: Arc<ArcSwap<HashMap<String, i32>>>,
) {
    tokio::spawn(async move {
        info!(
            sources = priority.load().len(),
            "forecast bridge started (forecast sources -> ForecastStore)"
        );
        // (owner_id, owner_priority, owner_last_epoch)
        let mut owner: Option<(String, i32, i64)> = None;
        loop {
            match rx.recv().await {
                Ok(SourceEvent::Forecast {
                    source_id,
                    snapshot,
                    at_epoch,
                }) => {
                    // Re-read the live priority map each event so a hot-reloaded
                    // forecast-provider pin / re-rank is honored immediately.
                    let priority = priority.load();
                    let prio = priority.get(&source_id).copied().unwrap_or(0);
                    let take = match &owner {
                        None => true,
                        // The owner refreshing itself always wins.
                        Some((oid, _, _)) if *oid == source_id => true,
                        // A strictly-higher source wins; a stale owner is
                        // yielded. Strict `>` so two equal-priority sources
                        // (e.g. OpenWeather=PirateWeather=50) don't flip-flop
                        // ownership on every refresh. The owner's priority is
                        // re-read from the LIVE map (not the value captured when
                        // it took ownership) so a hot-reloaded re-rank that
                        // demotes the incumbent lets a now-higher source take
                        // over on its next emit instead of waiting for a stale
                        // window.
                        Some((oid, oprio, oepoch)) => {
                            let oprio_live = priority.get(oid).copied().unwrap_or(*oprio);
                            prio > oprio_live
                                || at_epoch.saturating_sub(*oepoch) > FORECAST_OWNER_STALE_SECS
                        }
                    };
                    if take {
                        // Guarantee a provenance label even if a producer left
                        // it blank, so the UI never falls back to a hardcoded
                        // provider name.
                        let mut snapshot = snapshot;
                        if snapshot.source_label.is_empty() {
                            snapshot.source_label = source_id.clone();
                        }
                        store.store(snapshot);
                        owner = Some((source_id.clone(), prio, at_epoch));
                        debug!(source_id = %source_id, prio, "forecast bridge stored forecast");
                    } else {
                        debug!(source_id = %source_id, prio, "forecast bridge kept higher-priority owner");
                    }
                }
                // Observation / KeyedReading / Reachability are handled by the
                // snapshot bridge + bus recorder, not here.
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!(
                        skipped = n,
                        "forecast bridge lagged; some forecasts dropped"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("forecast bridge: bus closed, exiting");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forecast::snapshot::{DailyEntry, ForecastSnapshot};

    fn snap(tz: &str) -> ForecastSnapshot {
        ForecastSnapshot {
            timezone: tz.to_string(),
            source_reachable: true,
            daily: vec![DailyEntry::default()],
            ..Default::default()
        }
    }

    fn forecast(id: &str, tz: &str, at: i64) -> SourceEvent {
        SourceEvent::Forecast {
            source_id: id.to_string(),
            snapshot: snap(tz),
            at_epoch: at,
        }
    }

    // Drive the arbitration logic directly (no task) by replicating the decision
    // so the priority/staleness rules are unit-tested deterministically.
    fn decide(owner: &Option<(String, i32, i64)>, prio: i32, id: &str, at: i64) -> bool {
        match owner {
            None => true,
            Some((oid, _, _)) if oid == id => true,
            Some((_, oprio, oepoch)) => {
                prio > *oprio || at.saturating_sub(*oepoch) > FORECAST_OWNER_STALE_SECS
            }
        }
    }

    #[test]
    fn higher_priority_source_takes_over() {
        let owner = Some(("open_meteo".to_string(), 40, 1_000));
        assert!(
            decide(&owner, 60, "nws", 1_100),
            "NWS(60) beats Open-Meteo(40)"
        );
    }

    #[test]
    fn lower_priority_source_is_ignored_while_owner_fresh() {
        let owner = Some(("nws".to_string(), 60, 1_000));
        assert!(
            !decide(&owner, 40, "open_meteo", 1_100),
            "Open-Meteo(40) must not displace a fresh NWS(60)"
        );
    }

    #[test]
    fn stale_owner_yields_to_anyone() {
        let owner = Some(("nws".to_string(), 60, 1_000));
        let at = 1_000 + FORECAST_OWNER_STALE_SECS + 1;
        assert!(
            decide(&owner, 40, "open_meteo", at),
            "a stale NWS owner fails over to Open-Meteo"
        );
    }

    #[test]
    fn owner_refreshing_itself_always_wins() {
        let owner = Some(("nws".to_string(), 60, 1_000));
        assert!(decide(&owner, 60, "nws", 1_100));
    }

    #[test]
    fn equal_priority_other_source_does_not_thrash() {
        // OpenWeather owns; PirateWeather (same priority 50) must NOT displace it
        // while fresh, or the two would flip-flop every refresh.
        let owner = Some(("openweather".to_string(), 50, 1_000));
        assert!(!decide(&owner, 50, "pirate", 1_100));
    }

    #[test]
    fn first_forecast_is_always_taken() {
        let _ = forecast("x", "UTC", 1); // exercise the helper
        assert!(decide(&None, 0, "anything", 1));
    }
}
