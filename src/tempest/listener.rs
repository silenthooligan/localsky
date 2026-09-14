// What the LAN listener is doing, for the health endpoint and the UI.
//
// The socket itself is a weather source now (`sources::tempest_udp`),
// like every other reading LocalSky takes. This is the state it reports
// while it runs, and it stays here because /api/v1/health and the setup
// wizard have always read it here.
//
// Why it is worth reporting at all: a Tempest station is read over the
// network, and only one program per machine can read it. That is an
// ordinary thing to run into and an awful thing to diagnose, because the
// symptom is an empty station panel.

use arc_swap::ArcSwap;
use std::sync::OnceLock;

pub use crate::tempest::status::ListenerStatus;

fn status_cell() -> &'static ArcSwap<ListenerStatus> {
    static CELL: OnceLock<ArcSwap<ListenerStatus>> = OnceLock::new();
    CELL.get_or_init(|| ArcSwap::from_pointee(ListenerStatus::NotConfigured))
}

/// The listener's current state, for the health endpoint and the UI.
pub fn listener_status() -> ListenerStatus {
    (**status_cell().load()).clone()
}

pub fn set_status(s: ListenerStatus) {
    status_cell().store(std::sync::Arc::new(s));
}
