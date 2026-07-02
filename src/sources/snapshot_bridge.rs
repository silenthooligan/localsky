// Bridges the merged source bus into the live TempestStore snapshot.
//
// The web dashboard SSE, GET /api/snapshot, and every HA weather entity read
// the SAME TempestStore snapshot. Historically only Tempest UDP / demo /
// Blitzortung wrote it, so a user on Ecowitt / Open-Meteo / NWS / HA-passthrough
// / Davis / Ambient saw an EMPTY dashboard even though their source was happily
// publishing on the bus. This consumer carries those bus observations into the
// snapshot via TempestStore::apply_source_fields, so every source populates the
// dashboard + HA entities.
//
// CRITICAL containment: only sources whose SourceCaps.live_current is true
// stamp the snapshot's last_packet_epoch. A forecast source (NWS/Open-Meteo/
// MetNorway/PirateWeather) populates the DISPLAY fields but must NOT be treated
// as a live station by the irrigation engine's resolve_current_conditions, or
// forecast numbers get fed into a real run/skip decision. The live_current map
// is built from each source's own capabilities(); an id we can't classify
// defaults to false (conservative: never claim station-liveness we're unsure of).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::{debug, info};

use crate::ports::weather_source::SourceEvent;
use crate::tempest::state::TempestStore;

/// Spawn the bus -> snapshot bridge. Runs for the process lifetime.
pub fn spawn(
    mut rx: broadcast::Receiver<SourceEvent>,
    store: Arc<TempestStore>,
    live_current: HashMap<String, bool>,
) {
    tokio::spawn(async move {
        info!(
            sources = live_current.len(),
            "snapshot bridge started (non-Tempest sources -> live snapshot)"
        );
        loop {
            match rx.recv().await {
                Ok(SourceEvent::Observation {
                    source_id,
                    fields,
                    at_epoch,
                }) => {
                    let lc = live_current.get(&source_id).copied().unwrap_or(false);
                    store.apply_source_fields(&fields, at_epoch, lc, &source_id);
                    debug!(
                        source_id = %source_id,
                        fields = fields.len(),
                        live_current = lc,
                        "snapshot bridge applied source fields"
                    );
                }
                // KeyedReading (zone-bound soil channels) + Reachability are not
                // global snapshot fields; the bus_recorder / health layer own them.
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!(
                        skipped = n,
                        "snapshot bridge lagged; some observations dropped"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("snapshot bridge: bus closed, exiting");
                    break;
                }
            }
        }
    });
}
