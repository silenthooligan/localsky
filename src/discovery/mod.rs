// Native LAN device discovery (Phase E2 of the device-parity effort).
//
// Finds hardware on the local network without going through Home Assistant,
// so a gateway can be onboarded in LocalSky's own UI (the Music-Assistant
// "it just appears" experience). Today: Ecowitt gateways via their UDP
// broadcast protocol. The host may be multi-homed (the host may have a NIC
// on both the app subnet and the sensor subnet); discovery broadcasts on
// every interface so a gateway on any attached subnet is found.

pub mod ecowitt;
pub mod opensprinkler;

pub use ecowitt::{discover_ecowitt, DiscoveredGateway};
