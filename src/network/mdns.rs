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
use std::sync::Arc;

use mdns_sd::{ServiceDaemon, ServiceInfo};

pub const SERVICE_TYPE: &str = "_localsky._tcp.local.";

/// Spawn the announcer. Re-registers when the auth policy flips so the
/// TXT record stays truthful. Failures log and give up quietly: mDNS is
/// a convenience, never load-bearing.
pub fn spawn(port: u16, auth_rt: Option<Arc<crate::auth::AuthRuntime>>) {
    tokio::spawn(async move {
        let daemon = match ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "mdns: daemon failed to start; discovery announce disabled");
                return;
            }
        };

        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| "localsky".into());
        let instance_name = format!("LocalSky ({hostname})");
        let host_fqdn = format!("{hostname}.local.");

        let mut last_auth: Option<bool> = None;
        loop {
            let auth_required = auth_rt
                .as_ref()
                .map(|rt| rt.policy.load().required)
                .unwrap_or(false);
            if last_auth != Some(auth_required) {
                let mut txt = HashMap::new();
                txt.insert("version".to_string(), env!("CARGO_PKG_VERSION").to_string());
                txt.insert("api_prefix".to_string(), "/api/v1".to_string());
                if let Some(uuid) = crate::instance::get() {
                    txt.insert("uuid".to_string(), uuid.to_string());
                }
                txt.insert(
                    "auth".to_string(),
                    if auth_required {
                        "required"
                    } else {
                        "disabled"
                    }
                    .to_string(),
                );
                // enable_addr_auto: the daemon fills in every interface
                // address, which is what a multi-homed host wants.
                match ServiceInfo::new(SERVICE_TYPE, &instance_name, &host_fqdn, (), port, txt)
                    .map(|si| si.enable_addr_auto())
                {
                    Ok(si) => {
                        if let Err(e) = daemon.register(si) {
                            tracing::warn!(error = %e, "mdns: register failed");
                        } else {
                            tracing::info!(
                                service = SERVICE_TYPE,
                                port,
                                auth = auth_required,
                                "mdns: announcing"
                            );
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "mdns: service info build failed"),
                }
                last_auth = Some(auth_required);
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}
