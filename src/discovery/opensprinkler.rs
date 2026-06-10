// OpenSprinkler LAN probe. Sweeps the RFC1918 /24 of every local
// interface on ports 8080 and 80 with GET /jo (controller options).
// An OpenSprinkler answers with JSON carrying "fwv" (firmware) when
// unauthenticated access is allowed, or {"result":<code>} when a
// password is required; either shape identifies the device. Explicitly
// user-initiated (the wizard's Scan button), bounded concurrency, short
// per-host timeout, so a full sweep returns in a few seconds.

use std::net::Ipv4Addr;
use std::time::Duration;

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct DiscoveredController {
    /// "opensprinkler".
    pub vendor: String,
    pub ip: String,
    pub port: u16,
    /// Firmware version when the controller answered unauthenticated.
    pub firmware: Option<String>,
    /// True when /jo demanded a password (still an OpenSprinkler).
    pub password_required: bool,
}

/// Candidate /24 subnets from local interface addresses (private ranges
/// only; never sweep public space).
fn local_subnets() -> Vec<Ipv4Addr> {
    let mut nets = Vec::new();
    if let Ok(ifaces) = if_addrs::get_if_addrs() {
        for iface in ifaces {
            if iface.is_loopback() {
                continue;
            }
            if let std::net::IpAddr::V4(ip) = iface.addr.ip() {
                if ip.is_private() {
                    let o = ip.octets();
                    let base = Ipv4Addr::new(o[0], o[1], o[2], 0);
                    if !nets.contains(&base) {
                        nets.push(base);
                    }
                }
            }
        }
    }
    nets
}

async fn probe(client: reqwest::Client, ip: Ipv4Addr, port: u16) -> Option<DiscoveredController> {
    let url = format!("http://{ip}:{port}/jo");
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    if let Some(fwv) = v.get("fwv") {
        return Some(DiscoveredController {
            vendor: "opensprinkler".into(),
            ip: ip.to_string(),
            port,
            firmware: Some(fwv.to_string()),
            password_required: false,
        });
    }
    if v.get("result").map(|r| r.is_number()) == Some(true)
        && v.as_object().map(|o| o.len()) == Some(1)
    {
        return Some(DiscoveredController {
            vendor: "opensprinkler".into(),
            ip: ip.to_string(),
            port,
            firmware: None,
            password_required: true,
        });
    }
    None
}

/// Sweep the local /24s. Returns every OpenSprinkler-shaped responder.
pub async fn discover_opensprinkler(per_host_timeout: Duration) -> Vec<DiscoveredController> {
    use futures::stream::{self, StreamExt};

    let client = match reqwest::Client::builder().timeout(per_host_timeout).build() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut targets = Vec::new();
    for net in local_subnets() {
        let o = net.octets();
        for host in 1..=254u8 {
            let ip = Ipv4Addr::new(o[0], o[1], o[2], host);
            targets.push((ip, 8080u16));
            targets.push((ip, 80u16));
        }
    }

    let found: Vec<DiscoveredController> = stream::iter(targets)
        .map(|(ip, port)| {
            let client = client.clone();
            async move { probe(client, ip, port).await }
        })
        .buffer_unordered(64)
        .filter_map(|r| async move { r })
        .collect()
        .await;

    // One entry per IP: prefer the 8080 hit (OpenSprinkler's default).
    let mut deduped: Vec<DiscoveredController> = Vec::new();
    for c in found {
        if let Some(existing) = deduped.iter_mut().find(|e| e.ip == c.ip) {
            if existing.port != 8080 && c.port == 8080 {
                *existing = c;
            }
        } else {
            deduped.push(c);
        }
    }
    deduped
}
