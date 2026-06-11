// Tokio task that binds UDP 50222 (Tempest hub broadcast port), parses
// every incoming packet, and feeds the shared store. Runs forever in
// the background; failures are logged and retried with backoff.

use crate::tempest::packets::{ObsSt, RapidWindOb, StrikeEvent, TempestPacket};
use crate::tempest::state::TempestStore;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;

pub fn spawn_listener(store: Arc<TempestStore>) {
    tokio::spawn(async move {
        loop {
            match listen(store.clone()).await {
                Ok(()) => {
                    tracing::warn!("UDP listener returned cleanly; respawning");
                }
                Err(e) => {
                    tracing::error!("UDP listener error: {e:?}; retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    });
}

async fn listen(store: Arc<TempestStore>) -> std::io::Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:50222").await?;
    sock.set_broadcast(true)?;
    tracing::info!("listening for Tempest UDP on {}", sock.local_addr()?);

    let mut buf = vec![0u8; 4096];
    loop {
        let (n, _peer) = sock.recv_from(&mut buf).await?;
        let slice = &buf[..n];
        match serde_json::from_slice::<TempestPacket>(slice) {
            Ok(pkt) => apply(&store, pkt),
            Err(e) => {
                if let Ok(text) = std::str::from_utf8(slice) {
                    tracing::debug!("unparseable packet ({} bytes): {}, {}", n, e, text);
                }
            }
        }
    }
}

fn apply(store: &TempestStore, pkt: TempestPacket) {
    match pkt {
        TempestPacket::ObsSt {
            serial_number,
            hub_sn,
            obs,
            ..
        } => {
            for row in &obs {
                if let Some(parsed) = ObsSt::from_array(row) {
                    store.apply_obs(&serial_number, &hub_sn, &parsed);
                }
            }
        }
        TempestPacket::RapidWind { ob, .. } => {
            if let Some(p) = RapidWindOb::from_array(&ob) {
                store.apply_rapid_wind(&p);
            }
        }
        TempestPacket::EvtStrike { evt, .. } => {
            if let Some(p) = StrikeEvent::from_array(&evt) {
                store.apply_strike(&p);
            }
        }
        TempestPacket::DeviceStatus { voltage, .. } => {
            store.apply_battery(voltage);
        }
        _ => {}
    }
}
