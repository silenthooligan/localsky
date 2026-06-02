// Ecowitt gateway UDP discovery.
//
// Ecowitt gateways (GW1100 / GW2000 / WittBoy consoles) answer a broadcast
// "CMD_BROADCAST" datagram on UDP 46000 with their MAC, IP, local API port,
// and a model/firmware string. The request is a fixed 5-byte frame; the
// reply is parsed below. Layout verified against GW1100B firmware.
//
// Request:  FF FF 12 03 15   (header, cmd=0x12, size=0x03, checksum=cmd+size)
// Reply:    FF FF 12 <size:2> <mac:6> <ip:4> <port:2> <name_len:1> <name..> <cksum:1>

use std::net::Ipv4Addr;
use std::time::Duration;

use serde::Serialize;
use tokio::net::UdpSocket;
use tracing::debug;

/// A gateway found on the LAN. `mac` is the stable identity used to dedup
/// the same device against an HA-imported copy in Phase F.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscoveredGateway {
    /// Vendor family. "ecowitt" for now; the field future-proofs the API.
    pub vendor: String,
    /// Colon-separated uppercase MAC (e.g. "00:11:22:33:44:55").
    pub mac: String,
    pub ip: String,
    /// Gateway local-API port (from the reply; informational).
    pub port: u16,
    /// Model + firmware string (e.g. "GW1100B-WIFI4455 V2.4.5").
    pub model: String,
    /// Pre-built `ecowitt_gw_poll` source config the "Add" button can use.
    pub suggested_host: String,
}

const ECOWITT_PORT: u16 = 46000;
const DISCOVERY_REQUEST: [u8; 5] = [0xFF, 0xFF, 0x12, 0x03, 0x15];

/// Parse one CMD_BROADCAST reply into a `DiscoveredGateway`. Returns None on
/// any malformation (wrong header/cmd, truncated) so a stray datagram on the
/// port can't crash discovery.
pub fn parse_reply(buf: &[u8]) -> Option<DiscoveredGateway> {
    // Minimum: header(2)+cmd(1)+size(2)+mac(6)+ip(4)+port(2)+name_len(1) = 18.
    if buf.len() < 18 || buf[0] != 0xFF || buf[1] != 0xFF || buf[2] != 0x12 {
        return None;
    }
    let mac = buf[5..11]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":");
    let ip = Ipv4Addr::new(buf[11], buf[12], buf[13], buf[14]).to_string();
    let port = u16::from_be_bytes([buf[15], buf[16]]);
    let name_len = buf[17] as usize;
    let name_end = 18usize.checked_add(name_len)?;
    if name_end > buf.len() {
        return None;
    }
    let model = String::from_utf8_lossy(&buf[18..name_end])
        .trim_matches(char::from(0))
        .trim()
        .to_string();
    Some(DiscoveredGateway {
        vendor: "ecowitt".to_string(),
        mac,
        ip: ip.clone(),
        port,
        model,
        suggested_host: ip,
    })
}

/// The subnet-directed broadcast for an interface, computed from ip+netmask.
/// We can't rely on the kernel `brd` field: the host's NICs may be
/// configured `scope global` with no broadcast address, so if-addrs reports
/// `broadcast: None` and a naive skip would never probe the sensor subnet.
/// `ip | !mask` always yields the right directed broadcast.
fn subnet_broadcast(ip: Ipv4Addr, mask: Ipv4Addr) -> Option<Ipv4Addr> {
    let m = u32::from(mask);
    if m == 0 {
        return None; // /0 -> global broadcast; not a real subnet to probe
    }
    Some(Ipv4Addr::from(u32::from(ip) | !m))
}

/// Broadcast the Ecowitt discovery request on every IPv4 interface and
/// collect replies for `timeout`. Each interface is probed concurrently — a
/// dual-homed host has several NICs and doing them sequentially would
/// multiply the timeout. A single 255.255.255.255 send only leaves the
/// default-route NIC and would miss a gateway on a secondary subnet, so we
/// bind per interface and send to that interface's directed broadcast.
/// Deduped by MAC.
pub async fn discover_ecowitt(timeout: Duration) -> Vec<DiscoveredGateway> {
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(e) => {
            debug!(error = %e, "discovery: get_if_addrs failed");
            return Vec::new();
        }
    };

    let mut probes = Vec::new();
    for iface in ifaces {
        if iface.is_loopback() {
            continue;
        }
        let if_addrs::IfAddr::V4(v4) = iface.addr else {
            continue;
        };
        let Some(bcast) = subnet_broadcast(v4.ip, v4.netmask) else {
            continue;
        };
        probes.push(probe_interface(iface.name, v4.ip, bcast, timeout));
    }

    let mut found: Vec<DiscoveredGateway> = Vec::new();
    for list in futures::future::join_all(probes).await {
        for gw in list {
            if !found.iter().any(|g| g.mac == gw.mac) {
                found.push(gw);
            }
        }
    }
    found
}

/// Probe one interface: bind, broadcast, and drain replies until `timeout`.
async fn probe_interface(
    name: String,
    bind_ip: Ipv4Addr,
    bcast: Ipv4Addr,
    timeout: Duration,
) -> Vec<DiscoveredGateway> {
    let mut found: Vec<DiscoveredGateway> = Vec::new();
    let sock = match UdpSocket::bind((bind_ip, 0)).await {
        Ok(s) => s,
        Err(e) => {
            debug!(iface = %name, error = %e, "discovery: bind failed");
            return found;
        }
    };
    if sock.set_broadcast(true).is_err() {
        return found;
    }
    if let Err(e) = sock
        .send_to(&DISCOVERY_REQUEST, (bcast, ECOWITT_PORT))
        .await
    {
        debug!(iface = %name, error = %e, "discovery: send failed");
        return found;
    }
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = [0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, sock.recv_from(&mut buf)).await {
            Ok(Ok((n, _src))) => {
                if let Some(gw) = parse_reply(&buf[..n]) {
                    debug!(iface = %name, mac = %gw.mac, ip = %gw.ip, "discovered ecowitt gateway");
                    if !found.iter().any(|g| g.mac == gw.mac) {
                        found.push(gw);
                    }
                }
            }
            _ => break,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    // A CMD_BROADCAST reply built from parts (documentation MAC + TEST-NET
    // address per RFC 5737), exercising the exact wire layout.
    fn synth_reply() -> Vec<u8> {
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        let ip = [192, 0, 2, 61];
        let port: u16 = 0xafc8;
        let name = b"GW1100B-WIFI4455 V2.4.5";
        let mut v = vec![0xFF, 0xFF, 0x12, 0x00, 0x27]; // header, cmd, size
        v.extend_from_slice(&mac);
        v.extend_from_slice(&ip);
        v.extend_from_slice(&port.to_be_bytes());
        v.push(name.len() as u8);
        v.extend_from_slice(name);
        v.push(0x00); // checksum (parser does not validate it)
        v
    }

    #[test]
    fn parses_gw1100b_reply() {
        let gw = parse_reply(&synth_reply()).expect("parses");
        assert_eq!(gw.mac, "00:11:22:33:44:55");
        assert_eq!(gw.ip, "192.0.2.61");
        assert_eq!(gw.port, 0xafc8);
        assert_eq!(gw.model, "GW1100B-WIFI4455 V2.4.5");
        assert_eq!(gw.suggested_host, "192.0.2.61");
        assert_eq!(gw.vendor, "ecowitt");
    }

    #[test]
    fn subnet_broadcast_from_ip_and_mask() {
        // A NIC configured `scope global` with no kernel brd: compute it.
        assert_eq!(
            super::subnet_broadcast(
                "192.0.2.81".parse().unwrap(),
                "255.255.255.0".parse().unwrap()
            ),
            Some("192.0.2.255".parse().unwrap())
        );
        assert_eq!(
            super::subnet_broadcast("10.1.2.3".parse().unwrap(), "255.255.0.0".parse().unwrap()),
            Some("10.1.255.255".parse().unwrap())
        );
        // /0 has no meaningful subnet broadcast.
        assert_eq!(
            super::subnet_broadcast("1.2.3.4".parse().unwrap(), "0.0.0.0".parse().unwrap()),
            None
        );
    }

    #[test]
    fn rejects_non_ecowitt_or_truncated() {
        assert!(parse_reply(&[0x00, 0x01, 0x02]).is_none());
        assert!(
            parse_reply(&[0xFF, 0xFF, 0x99, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).is_none()
        );
        // Claims a 200-byte name but the buffer is short.
        let mut bad = synth_reply();
        bad[17] = 200;
        assert!(parse_reply(&bad).is_none());
    }
}
