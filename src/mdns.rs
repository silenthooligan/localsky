// mDNS announce: register `_localsky._tcp.local.` with TXT metadata so
// LAN clients (HACS zeroconf, mobile apps) find the instance and know
// how to talk to it before the first HTTP request:
//
//   version       crate version
//   api_prefix    "/api/v1"
//   uuid          stable instance id (instance.rs)
//   auth          "required" | "disabled" (refreshed when policy flips)
//
// Announce-only (we never browse), enabled by default via
// [network].mdns_enabled. Docker note: requires host networking (the
// compose file already uses network_mode: host for Tempest UDP); under
// bridged networking the announce stays inside the container's netns
// and discovery falls back to manual host entry, same caveat Music
// Assistant documents.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use mdns_sd::{IfKind, IfPredicate, ServiceDaemon, ServiceInfo};
use sha2::{Digest, Sha256};

pub const SERVICE_TYPE: &str = "_localsky._tcp.local.";

/// Constructed during routing, started only after the HTTP listener binds.
/// Keeping the policy here avoids starting a discovery task for a failed boot.
pub struct Announcement {
    auth_rt: Option<Arc<crate::auth::AuthRuntime>>,
}

impl Announcement {
    pub fn new(auth_rt: Option<Arc<crate::auth::AuthRuntime>>) -> Self {
        Self { auth_rt }
    }

    pub fn start(self, bound: SocketAddr) {
        if !advertisable_bind(bound) {
            tracing::debug!(%bound, "mdns: local-only or unusable listener; discovery disabled");
            return;
        }
        let Some(uuid) = crate::instance::get().filter(|id| !id.is_empty()) else {
            tracing::warn!("mdns: instance identity unavailable; discovery disabled");
            return;
        };
        tokio::spawn(async move {
            let daemon = match ServiceDaemon::new() {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!(error = %e, "mdns: daemon failed to start; discovery disabled");
                    return;
                }
            };
            let mut last_auth: Option<bool> = None;
            loop {
                let auth_required = self
                    .auth_rt
                    .as_ref()
                    .map(|rt| rt.policy.load().required)
                    .unwrap_or(false);
                if last_auth != Some(auth_required) {
                    match service_info(bound, uuid, auth_required) {
                        Ok(info) => {
                            if let Err(e) = daemon.register(info) {
                                tracing::warn!(error = %e, "mdns: register failed");
                            } else {
                                tracing::info!(service = SERVICE_TYPE, %bound, auth = auth_required, "mdns: announcing");
                                last_auth = Some(auth_required);
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "mdns: service info build failed"),
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
    }
}

fn is_loopback(ip: IpAddr) -> bool {
    ip.is_loopback()
        || matches!(ip, IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()))
}

fn advertisable_bind(bound: SocketAddr) -> bool {
    bound.port() != 0
        && !is_loopback(bound.ip())
        && !bound.ip().is_multicast()
        && !matches!(bound.ip(), IpAddr::V4(v4) if v4.is_broadcast())
}

/// Wildcard listeners advertise their own address family only. A concrete
/// listener can advertise exactly its bound address, never the host's other NICs.
fn accepts_interface(bound: IpAddr, candidate: IpAddr) -> bool {
    if is_loopback(candidate) || candidate.is_unspecified() || candidate.is_multicast() {
        return false;
    }
    if bound.is_unspecified() {
        bound.is_ipv4() == candidate.is_ipv4()
    } else {
        bound == candidate
    }
}

fn service_info(
    bound: SocketAddr,
    uuid: &str,
    auth_required: bool,
) -> mdns_sd::Result<ServiceInfo> {
    // HOSTNAME can be missing, duplicated across containers, or invalid as a
    // DNS label. The persisted instance id distinguishes installations, while
    // the port keeps cloned data on a second listener from sharing DNS records.
    let digest = hex::encode(Sha256::digest(uuid.as_bytes()));
    let identity = format!("{}-{}", &digest[..20], bound.port());
    let instance_name = format!("LocalSky ({identity})");
    let host_fqdn = format!("localsky-{identity}.local.");
    let mut txt = HashMap::new();
    txt.insert("version".to_string(), env!("CARGO_PKG_VERSION").to_string());
    txt.insert("api_prefix".to_string(), "/api/v1".to_string());
    txt.insert("uuid".to_string(), uuid.to_string());
    txt.insert(
        "auth".to_string(),
        if auth_required {
            "required"
        } else {
            "disabled"
        }
        .to_string(),
    );
    let addresses: Vec<IpAddr> = if bound.ip().is_unspecified() {
        Vec::new()
    } else {
        vec![bound.ip()]
    };
    let mut info = ServiceInfo::new(
        SERVICE_TYPE,
        &instance_name,
        &host_fqdn,
        addresses.as_slice(),
        bound.port(),
        txt,
    )?;
    if bound.ip().is_unspecified() {
        info = info.enable_addr_auto();
    }
    info.set_interfaces(vec![IfKind::Predicate(IfPredicate::new(
        move |interface| accepts_interface(bound.ip(), interface.ip()),
    ))]);
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_fixtures_cannot_advertise_lan_endpoints() {
        for bind in [
            "127.0.0.1:18190",
            "127.8.9.10:18090",
            "[::1]:8090",
            "[::ffff:127.0.0.1]:8090",
            "0.0.0.0:0",
        ] {
            assert!(!advertisable_bind(bind.parse().unwrap()), "{bind}");
        }
        assert!(advertisable_bind("198.51.100.81:8090".parse().unwrap()));
    }

    #[test]
    fn identity_names_are_stable_and_isolate_instances_and_ports() {
        let bound = "0.0.0.0:8090".parse().unwrap();
        let first = service_info(bound, "live-instance", false).unwrap();
        let second = service_info(bound, "fixture-instance", false).unwrap();
        let auth_change = service_info(bound, "live-instance", true).unwrap();
        let cloned_data =
            service_info("0.0.0.0:18090".parse().unwrap(), "live-instance", false).unwrap();
        assert_ne!(first.get_hostname(), second.get_hostname());
        assert_ne!(first.get_fullname(), second.get_fullname());
        assert_ne!(first.get_hostname(), cloned_data.get_hostname());
        assert_eq!(first.get_hostname(), auth_change.get_hostname());
        assert_eq!(first.get_fullname(), auth_change.get_fullname());
        assert_eq!(first.get_property_val_str("uuid"), Some("live-instance"));
        assert_eq!(first.get_property_val_str("api_prefix"), Some("/api/v1"));
        assert_eq!(first.get_property_val_str("auth"), Some("disabled"));
        assert_eq!(auth_change.get_property_val_str("auth"), Some("required"));
        let label = first.get_hostname().split('.').next().unwrap();
        assert!(label.len() <= 63);
        assert!(label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }

    #[test]
    fn concrete_listener_publishes_only_its_address_without_auto_expansion() {
        let bound: SocketAddr = "198.51.100.81:8090".parse().unwrap();
        let info = service_info(bound, "live-instance", false).unwrap();
        assert!(!info.is_addr_auto());
        assert_eq!(info.get_addresses().len(), 1);
        assert!(info.get_addresses().contains(&bound.ip()));
        assert!(accepts_interface(bound.ip(), bound.ip()));
        assert!(!accepts_interface(
            bound.ip(),
            "203.0.113.81".parse().unwrap()
        ));
    }

    #[test]
    fn wildcard_discovery_excludes_loopback_and_unbound_address_family() {
        let v4: SocketAddr = "0.0.0.0:8090".parse().unwrap();
        assert!(service_info(v4, "live-instance", false)
            .unwrap()
            .is_addr_auto());
        assert!(accepts_interface(v4.ip(), "198.51.100.81".parse().unwrap()));
        assert!(!accepts_interface(v4.ip(), "127.0.0.1".parse().unwrap()));
        assert!(!accepts_interface(v4.ip(), "2001:db8::1".parse().unwrap()));
        let v6: IpAddr = "::".parse().unwrap();
        assert!(accepts_interface(v6, "2001:db8::1".parse().unwrap()));
        assert!(!accepts_interface(v6, "::1".parse().unwrap()));
        assert!(!accepts_interface(v6, "198.51.100.81".parse().unwrap()));
    }
}
