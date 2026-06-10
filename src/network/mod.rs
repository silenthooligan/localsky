// Network presence. mdns.rs announces _localsky._tcp on the LAN so
// integration clients (the HACS config flow's zeroconf step, future
// mobile apps) discover the instance without typing an IP.

pub mod mdns;
