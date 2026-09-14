// The Tempest hub's UDP broadcast, as an ordinary weather source.
//
// It used to write the live store directly, which is why the store
// carried two arbiters for the same fields and a mutex whose only job was
// keeping this path from racing the bus bridge. It publishes on the bus
// now like everything else, and the readings a station reports that no
// cloud does (the wind lull, the three-second wind, the battery, the
// precipitation type) are ordinary fields rather than a special case.
//
// The socket is CONFIGURED, not unconditional, and the supervisor
// re-reads the config rather than binding once at boot. The port is
// exclusive within a network namespace: this socket sets neither
// SO_REUSEADDR nor SO_REUSEPORT. That is fine when LocalSky is the only
// listener, and fine on a separate host, because a hub BROADCASTS and
// every host on the LAN receives every packet independently. It is not
// fine when LocalSky shares a host with another Tempest consumer,
// typically Home Assistant's WeatherFlow integration, because whoever
// binds second gets EADDRINUSE. Since the retry never gives up, it would
// also take the port the moment the other side let go, which from the
// other side looks like LocalSky stealing the station. So an operator who
// removes the source in the UI gets the port back, without a restart and
// without editing TOML.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::config::schema::TempestUdpConfig;
use crate::ports::config_store::ConfigStore;
use crate::ports::weather_source::{
    ShutdownSignal, SourceBus, SourceCaps, SourceEvent, WeatherField, WeatherSource,
};
use crate::tempest::listener::{set_status, ListenerStatus};
use crate::tempest::packets::{ObsSt, RapidWindOb, StrikeEvent, TempestPacket};

/// How often the supervisor re-reads the config.
const CONFIG_POLL: Duration = Duration::from_secs(15);

pub struct TempestUdp {
    id: String,
    /// The config as built. Used as-is when no config store is wired
    /// (tests); otherwise the supervisor's first read replaces it.
    boot_config: TempestUdpConfig,
    /// Re-read on an interval so removing the source in the UI, or
    /// pointing it at a different hub, takes effect without a restart.
    /// Adding one takes a restart, like every other source: this task
    /// exists only because a source entry did at boot.
    config_store: Option<Arc<dyn ConfigStore>>,
    priority: i32,
    /// The serials last announced, so identity is published on the first
    /// packet and on a change rather than on every one.
    announced: Mutex<Option<(String, String)>>,
}

impl TempestUdp {
    pub fn new(
        id: String,
        boot_config: TempestUdpConfig,
        priority: i32,
        config_store: Option<Arc<dyn ConfigStore>>,
    ) -> Self {
        Self {
            id,
            boot_config,
            config_store,
            priority,
            announced: Mutex::new(None),
        }
    }

    /// The enabled `tempest_udp` source as the config has it now, or the
    /// boot copy when nothing is wired to re-read.
    async fn wanted(&self) -> Option<TempestUdpConfig> {
        let Some(store) = self.config_store.as_ref() else {
            return Some(self.boot_config.clone());
        };
        let cfg = store.load().await.ok()?;
        wanted_config(&cfg, &self.id)
    }

    /// Resolves when the configured source differs from `have`.
    async fn config_changed(&self, have: TempestUdpConfig) {
        loop {
            tokio::time::sleep(CONFIG_POLL).await;
            if self.wanted().await.as_ref() != Some(&have) {
                return;
            }
        }
    }

    async fn listen(&self, cfg: &TempestUdpConfig, bus: &SourceBus) -> std::io::Result<()> {
        // Honors bind_addr, which the schema has offered since it was
        // written and nothing read. An operator sharing a host with
        // another consumer can bind a specific interface instead of
        // rebuilding a container.
        let sock = UdpSocket::bind(cfg.bind_addr.as_str()).await?;
        sock.set_broadcast(true)?;
        tracing::info!(
            source_id = %self.id,
            bind_addr = %cfg.bind_addr,
            hub_serial = cfg.hub_serial.as_deref().unwrap_or("any"),
            "listening for Tempest UDP on {}",
            sock.local_addr()?
        );
        set_status(ListenerStatus::Listening {
            bind_addr: sock
                .local_addr()
                .map(|a| a.to_string())
                .unwrap_or_else(|_| cfg.bind_addr.clone()),
            hub_serial: cfg.hub_serial.clone(),
        });
        // A bound socket is a reachable source even before the hub's next
        // packet, so a quiet minute reads as watching rather than offline.
        let _ = bus.send(SourceEvent::Reachability {
            source_id: self.id.clone(),
            reachable: true,
        });

        let mut buf = vec![0u8; 4096];
        loop {
            let (n, _peer) = sock.recv_from(&mut buf).await?;
            let slice = &buf[..n];
            match serde_json::from_slice::<TempestPacket>(slice) {
                Ok(pkt) => self.publish(pkt, cfg, bus).await,
                Err(e) => {
                    if let Ok(text) = std::str::from_utf8(slice) {
                        tracing::debug!("unparseable packet ({} bytes): {}, {}", n, e, text);
                    }
                }
            }
        }
    }

    async fn publish(&self, pkt: TempestPacket, cfg: &TempestUdpConfig, bus: &SourceBus) {
        // One hub, if the operator named one. On a street where two hubs
        // are in broadcast range, LocalSky used to merge both, silently,
        // because nothing read this setting.
        if let Some(hub) = pkt.hub_sn() {
            if !hub_matches(cfg, hub) {
                tracing::trace!(hub_sn = hub, "packet from another hub; ignored");
                return;
            }
        }
        match pkt {
            TempestPacket::ObsSt {
                serial_number,
                hub_sn,
                obs,
                ..
            } => {
                self.announce(&serial_number, &hub_sn, bus).await;
                for row in &obs {
                    if let Some(parsed) = ObsSt::from_array(row) {
                        // Sent even when every reading in the row decoded
                        // as absent, ON PURPOSE, and unlike the rapid_wind
                        // arm below. An empty field list changes nothing in
                        // the live store (it returns before it stamps any
                        // freshness), but it does advance this source's
                        // `last_seen`, which is the only thing separating a
                        // hub whose sensor module has died (talking, and
                        // reporting nothing) from a hub that is unplugged.
                        // Those are different faults with different fixes
                        // and the status row has to say which. What makes
                        // that claim trustworthy is the row's TIME, and
                        // `epoch_at` bounds it to a plausible window.
                        let _ = bus.send(SourceEvent::Observation {
                            source_id: self.id.clone(),
                            fields: observation_fields(&parsed),
                            at_epoch: parsed.time_epoch,
                        });
                    }
                }
            }
            TempestPacket::RapidWind { ob, .. } => {
                if let Some(p) = RapidWindOb::from_array(&ob) {
                    // Same omit-rather-than-guess rule as the full
                    // observation: a sample with no number in it is not a
                    // calm three seconds.
                    let mut fields = Vec::new();
                    if let Some(mps) = p.speed_mps {
                        fields.push((WeatherField::RapidWindMph, crate::units::ms_to_mph(mps)));
                    }
                    if let Some(deg) = p.direction_deg {
                        fields.push((WeatherField::RapidWindBearingDeg, deg));
                    }
                    // A sample with nothing in it is not an observation of
                    // anything. Publishing it empty is a no-op in the store
                    // but not a free one: the bridge clones the snapshot and
                    // takes both owner mutexes before it reaches
                    // `if !touched { return; }`, and this arm fires 20 times
                    // a minute. The obs_st arm above is what keeps the hub's
                    // `last_seen` advancing, so the omission cannot make a
                    // reachable station read offline.
                    if !fields.is_empty() {
                        let _ = bus.send(SourceEvent::Observation {
                            source_id: self.id.clone(),
                            fields,
                            at_epoch: p.time_epoch,
                        });
                    }
                }
            }
            TempestPacket::EvtStrike { evt, .. } => {
                if let Some(p) = StrikeEvent::from_array(&evt) {
                    let _ = bus.send(SourceEvent::Strikes {
                        source_id: self.id.clone(),
                        strikes: vec![p],
                    });
                }
            }
            TempestPacket::DeviceStatus { voltage, .. } => {
                let _ = bus.send(SourceEvent::Observation {
                    source_id: self.id.clone(),
                    fields: vec![(WeatherField::BatteryV, voltage)],
                    at_epoch: crate::timefmt::now_epoch(),
                });
            }
            _ => {}
        }
    }

    /// Publish which hardware is talking, on the first packet and on a
    /// change. The footer names the station from it and "a station is
    /// present" reads off it.
    async fn announce(&self, station: &str, hub: &str, bus: &SourceBus) {
        let pair = (station.to_string(), hub.to_string());
        let mut announced = self.announced.lock().await;
        if announced.as_ref() == Some(&pair) {
            return;
        }
        *announced = Some(pair);
        let _ = bus.send(SourceEvent::Identity {
            source_id: self.id.clone(),
            station_serial: station.to_string(),
            hub_serial: hub.to_string(),
        });
    }
}

fn wanted_config(cfg: &crate::config::Config, id: &str) -> Option<TempestUdpConfig> {
    cfg.sources.iter().find_map(|s| {
        if !s.enabled || s.id != id {
            return None;
        }
        match &s.source {
            crate::config::schema::SourceKind::TempestUdp(t) => Some(t.clone()),
            _ => None,
        }
    })
}

/// True when this packet's hub is the one the operator configured.
///
/// `hub_serial` is the second field the schema has offered since it was
/// written and nothing read. On a street where two hubs are in broadcast
/// range, LocalSky merged both, silently.
fn hub_matches(cfg: &TempestUdpConfig, hub_sn: &str) -> bool {
    match cfg.hub_serial.as_deref() {
        None | Some("") => true,
        Some(want) => want.eq_ignore_ascii_case(hub_sn),
    }
}

/// One observation packet as bus fields, in the units the merge speaks.
///
/// A field the packet did not carry is OMITTED rather than sent as a
/// zero. The live store takes whatever arrives verbatim and stamps a
/// per-field live epoch for it, so a fabricated zero reads downstream as
/// a fresh, non-degraded station reading: a null air temperature
/// published as 32 F hard-skips every zone on the freeze gate ("Freeze
/// risk now (32F < 38F)") and writes that into History, the push and the
/// advisor prompt, and a null humidity published as 0 % is a measurement
/// the station never took, standing in the heat index and the HA sensors
/// (ET0 comes from the forecast day, not from this reading). An omitted
/// field claims nothing, so the forecast fills it and the decision is
/// honestly flagged degraded.
///
/// Rain is the last MINUTE's fall, not a since-midnight total: the store
/// integrates it on the deployment's calendar.
///
/// The lightning distance is the one channel where the STORE, not this
/// function, owns the no-reading encoding: `live_store` keeps
/// `lightning_avg_dist_mi` only while the value is above zero, because 0
/// miles on a distance channel would read as overhead. So a quiet minute
/// (a reported count of zero) publishes no distance at all, and a minute
/// that did detect strikes always publishes one.
fn observation_fields(obs: &ObsSt) -> Vec<(WeatherField, f64)> {
    use WeatherField as F;
    let mut fields = Vec::new();
    if let Some(c) = obs.air_temp_c {
        fields.push((F::AirTempF, crate::units::c_to_f(c)));
    }
    if let Some(rh) = obs.rh_pct {
        fields.push((F::RhPct, rh));
    }
    // Dew point is DERIVED and needs both parents. Computing it from one
    // of them plus a stand-in would smuggle the missing reading back in
    // under a different field name.
    if let (Some(c), Some(rh)) = (obs.air_temp_c, obs.rh_pct) {
        fields.push((
            F::DewPointF,
            crate::units::c_to_f(crate::weather::dew_point_c(c, rh)),
        ));
    }
    if let Some(mb) = obs.pressure_mb {
        fields.push((F::PressureInHg, crate::units::hpa_to_inhg(mb)));
    }
    if let Some(mps) = obs.wind_avg_mps {
        fields.push((F::WindMph, crate::units::ms_to_mph(mps)));
    }
    if let Some(mps) = obs.wind_gust_mps {
        fields.push((F::WindGustMph, crate::units::ms_to_mph(mps)));
    }
    if let Some(mps) = obs.wind_lull_mps {
        fields.push((F::WindLullMph, crate::units::ms_to_mph(mps)));
    }
    if let Some(deg) = obs.wind_dir_deg {
        fields.push((F::WindBearingDeg, deg));
    }
    if let Some(v) = obs.solar_w_m2 {
        fields.push((F::SolarWm2, v));
    }
    if let Some(v) = obs.uv_index {
        fields.push((F::UvIndex, v));
    }
    if let Some(lx) = obs.illuminance_lx {
        fields.push((F::Illuminance, lx));
    }
    if let Some(mm) = obs.rain_mm_last_min {
        fields.push((F::RainLastMinIn, crate::units::mm_to_in(mm)));
        fields.push((F::RainIntensityInHr, crate::units::mm_to_in(mm * 60.0)));
    }
    if let Some(kind) = obs.precip_type {
        fields.push((F::PrecipType, f64::from(kind)));
    }
    if let Some(v) = obs.battery_v {
        fields.push((F::BatteryV, v));
    }
    if let Some(count) = obs.lightning_strike_count_last_min {
        fields.push((F::LightningCount, f64::from(count)));
        // A minute that detected nothing has no average distance, so a
        // quiet minute sends none. A minute that DID detect strikes sends
        // one either way: the packet's own zero when the distance slot was
        // null, which `live_store`'s `(v > 0.0).then_some(v)` turns back
        // into "no reading". Omitting it there instead would leave the
        // PREVIOUS minute's distance in the snapshot beside this minute's
        // fresh count, which is the stale-reading-as-current failure this
        // path exists to remove.
        if count > 0 {
            let mi = obs
                .lightning_avg_dist_km
                .map_or(0.0, crate::units::km_to_mi);
            fields.push((F::LightningDistanceMi, mi));
        }
    }
    fields
}

#[async_trait]
impl WeatherSource for TempestUdp {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> SourceCaps {
        use WeatherField as F;
        SourceCaps {
            live_current: true,
            fields: HashSet::from([
                F::AirTempF,
                F::DewPointF,
                F::RhPct,
                F::PressureInHg,
                F::WindMph,
                F::WindGustMph,
                F::WindLullMph,
                F::WindBearingDeg,
                F::RapidWindMph,
                F::RapidWindBearingDeg,
                F::SolarWm2,
                F::UvIndex,
                F::Illuminance,
                F::RainLastMinIn,
                F::RainIntensityInHr,
                F::PrecipType,
                F::BatteryV,
                F::LightningCount,
                F::LightningDistanceMi,
            ]),
            ..Default::default()
        }
    }

    fn priority(&self, field: WeatherField) -> i32 {
        if self.capabilities().fields.contains(&field) {
            self.priority
        } else {
            i32::MIN
        }
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: ShutdownSignal,
    ) -> anyhow::Result<()> {
        let mut current: Option<TempestUdpConfig> = None;
        loop {
            if *shutdown.borrow() {
                let _ = bus.send(SourceEvent::Reachability {
                    source_id: self.id.clone(),
                    reachable: false,
                });
                return Ok(());
            }
            let wanted = self.wanted().await;
            match (&wanted, &current) {
                (None, Some(_)) => {
                    tracing::info!("Tempest UDP source removed or disabled; releasing the port");
                    current = None;
                    set_status(ListenerStatus::NotConfigured);
                }
                (Some(w), c) if Some(w) != c.as_ref() => {
                    tracing::info!(bind_addr = %w.bind_addr, "Tempest UDP source configured");
                    current = wanted.clone();
                }
                _ => {}
            }

            let Some(cfg) = current.clone() else {
                set_status(ListenerStatus::NotConfigured);
                tokio::select! {
                    _ = tokio::time::sleep(CONFIG_POLL) => {}
                    _ = shutdown.changed() => return Ok(()),
                }
                continue;
            };

            // Listen until the socket errors or the config changes under
            // us. `select!` drops the socket on the config branch, which
            // is what actually frees the port.
            tokio::select! {
                r = self.listen(&cfg, &bus) => match r {
                    Ok(()) => tracing::warn!("UDP listener returned cleanly; respawning"),
                    Err(e) => {
                        let _ = bus.send(SourceEvent::Reachability {
                            source_id: self.id.clone(),
                            reachable: false,
                        });
                        // EADDRINUSE means another program on this host
                        // already holds the port. That is a different
                        // problem from a broken socket and it has a
                        // different fix, so it gets its own state and its
                        // own words rather than a generic error.
                        let detail = e.to_string();
                        if e.kind() == std::io::ErrorKind::AddrInUse {
                            tracing::error!(
                                bind_addr = %cfg.bind_addr,
                                "{} is already in use by another program on this host. A \
                                 Tempest hub BROADCASTS, so LocalSky and another consumer \
                                 can both receive it from separate hosts, but they cannot \
                                 share the port on one host. Retrying every 5s.",
                                cfg.bind_addr
                            );
                            set_status(ListenerStatus::AddressInUse {
                                bind_addr: cfg.bind_addr.clone(),
                                detail,
                            });
                        } else {
                            tracing::error!("UDP listener error: {e:?}; retrying in 5s");
                            set_status(ListenerStatus::Error {
                                bind_addr: cfg.bind_addr.clone(),
                                detail,
                            });
                        }
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                },
                () = self.config_changed(cfg.clone()) => {
                    let _ = bus.send(SourceEvent::Reachability {
                        source_id: self.id.clone(), reachable: false,
                    });
                    tracing::info!("Tempest UDP config changed; rebinding");
                }
                _ = shutdown.changed() => {
                    let _ = bus.send(SourceEvent::Reachability {
                        source_id: self.id.clone(), reachable: false,
                    });
                    return Ok(());
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_reload_keeps_the_adapter_bound_to_its_source_id() {
        let mut cfg = crate::config::Config::default();
        cfg.sources = vec![
            serde_json::from_value(serde_json::json!({
                "id": "other", "enabled": true, "priority": 100,
                "kind": "tempest_udp", "config": {"bind_addr": "127.0.0.1:50221", "hub_serial": "HB-OTHER"}
            })).unwrap(),
            serde_json::from_value(serde_json::json!({
                "id": "mine", "enabled": true, "priority": 100,
                "kind": "tempest_udp", "config": {"bind_addr": "127.0.0.1:50222", "hub_serial": "HB-MINE"}
            })).unwrap(),
        ];
        let selected = wanted_config(&cfg, "mine").unwrap();
        assert_eq!(selected.hub_serial.as_deref(), Some("HB-MINE"));
        cfg.sources[1].enabled = false;
        assert!(wanted_config(&cfg, "mine").is_none());
        cfg.sources.pop();
        assert!(wanted_config(&cfg, "mine").is_none());
    }

    /// A minute from a healthy station: every reading these assertions
    /// care about is present, which is what makes the omissions in the
    /// tests further down mean something.
    fn obs() -> ObsSt {
        ObsSt {
            time_epoch: 1_700_000_000,
            air_temp_c: Some(20.0),
            rh_pct: Some(50.0),
            pressure_mb: Some(1013.0),
            wind_avg_mps: Some(3.0),
            wind_gust_mps: Some(5.0),
            wind_lull_mps: Some(1.0),
            wind_dir_deg: Some(180.0),
            rain_mm_last_min: Some(0.254),
            lightning_strike_count_last_min: Some(0),
            ..Default::default()
        }
    }

    fn row(json: &str) -> Vec<serde_json::Value> {
        serde_json::from_str(json).expect("a valid JSON array")
    }

    /// A row's time is judged against THIS HOST's clock (see
    /// `tempest::packets::epoch_at`), so a wire fixture has to stamp
    /// itself. `rest` is the row from slot 1 on, comma and all.
    fn row_at(epoch: i64, rest: &str) -> Vec<serde_json::Value> {
        row(&format!("[{epoch}{rest}]"))
    }

    /// One rapid_wind packet off the wire, `ob` being the sample from
    /// slot 1 on.
    fn rapid_wind(epoch: i64, ob: &str) -> TempestPacket {
        let json = format!(
            r#"{{"type":"rapid_wind","serial_number":"ST-1","hub_sn":"HB-1","ob":[{epoch}{ob}]}}"#
        );
        serde_json::from_str(&json).expect("a valid rapid_wind packet")
    }

    fn value_of(fields: &[(WeatherField, f64)], want: WeatherField) -> Option<f64> {
        fields.iter().find(|(f, _)| *f == want).map(|(_, v)| *v)
    }

    /// The packet's per-minute rain reaches the bus as the MINUTE's fall,
    /// never as a since-midnight total: the store integrates it, and a
    /// per-minute value mapped to the daily field is the single most
    /// damaging misconfiguration available (it is recorded as measured
    /// gauge evidence and outranks the rain archive for a week).
    #[test]
    fn rain_is_published_as_the_last_minutes_fall_and_as_a_rate() {
        let fields = observation_fields(&obs());
        let by = |want: WeatherField| value_of(&fields, want).expect("field present");
        assert!((by(WeatherField::RainLastMinIn) - 0.01).abs() < 1e-6);
        assert!((by(WeatherField::RainIntensityInHr) - 0.6).abs() < 1e-6);
        assert!(
            !fields.iter().any(|(f, _)| *f == WeatherField::RainTodayIn),
            "the station reports minutes; the store owns the accumulation"
        );
    }

    /// A quiet minute reports zero strikes, and zero miles on a distance
    /// channel means overhead, not "none": the distance rides along only
    /// when the minute actually detected something.
    #[test]
    fn a_quiet_minute_carries_no_lightning_distance() {
        let quiet = observation_fields(&obs());
        assert!(!quiet
            .iter()
            .any(|(f, _)| *f == WeatherField::LightningDistanceMi));
        let mut stormy = obs();
        stormy.lightning_strike_count_last_min = Some(3);
        stormy.lightning_avg_dist_km = Some(8.0);
        let fields = observation_fields(&stormy);
        let d = value_of(&fields, WeatherField::LightningDistanceMi)
            .expect("a minute with strikes carries the distance");
        assert!((d - crate::units::km_to_mi(8.0)).abs() < 1e-9);
    }

    /// A truncated or partly-null row can report strikes and no distance.
    /// The count publishes, so the distance has to publish too: the store
    /// keeps `lightning_avg_dist_mi` only while it is above zero, so the
    /// packet's zero is what CLEARS it. Omit it and an earlier minute's
    /// miles stay in the snapshot next to this minute's fresh count.
    #[test]
    fn strikes_with_no_distance_clear_the_previous_minutes_distance() {
        let mut blind = obs();
        blind.lightning_strike_count_last_min = Some(3);
        blind.lightning_avg_dist_km = None;
        let fields = observation_fields(&blind);
        assert_eq!(
            value_of(&fields, WeatherField::LightningCount),
            Some(3.0),
            "the strikes it did count still publish"
        );
        assert_eq!(
            value_of(&fields, WeatherField::LightningDistanceMi),
            Some(0.0),
            "zero is the store's clear, not a distance of nothing"
        );
    }

    /// The whole packet path, from wire bytes to bus fields: a Tempest
    /// whose temp/RH module has failed keeps broadcasting a well-formed
    /// obs_st with `null` in those two slots. Nothing may be published
    /// for them. Published as zeros they arrive as a fresh, non-degraded
    /// 32 F and 0 % RH: the freeze gate hard-skips every zone with
    /// "Freeze risk now (32F < 38F)" on a July morning, and the 0 % RH
    /// stands in the heat index and the HA sensors as a measurement the
    /// station never took. Omitted, they fall through to the forecast
    /// fill and the decision reads degraded.
    #[test]
    fn a_null_reading_publishes_no_field_at_all() {
        let parsed = ObsSt::from_array(&row_at(
            crate::timefmt::now_epoch(),
            ",0.18,0.22,0.27,144,6,1017.57,null,null,328,0.03,3,0.0,0,0,0,2.41,1",
        ))
        .expect("a row with a time is still an observation");
        let fields = observation_fields(&parsed);
        let has = |want: WeatherField| fields.iter().any(|(f, _)| *f == want);
        assert!(
            !has(WeatherField::AirTempF),
            "a null air temperature must not reach the freeze gate as 32 F"
        );
        assert!(
            !has(WeatherField::RhPct),
            "a null humidity must not reach the heat index as 0 %"
        );
        assert!(
            !has(WeatherField::DewPointF),
            "a derived field cannot outlive the readings it is derived from"
        );
        // Everything the packet DID carry still reaches the bus: this is
        // an omission, not a dropped packet.
        assert!(has(WeatherField::PressureInHg));
        assert!(has(WeatherField::WindMph));
        assert!(has(WeatherField::BatteryV));
        assert!(has(WeatherField::PrecipType));
    }

    /// The inverse, and the reason the fix is not "treat zero as
    /// missing": a station reporting zero is reporting. Freezing
    /// mornings happen, and 0 C must still reach the gate as 32 F.
    #[test]
    fn a_reported_zero_is_still_published() {
        let parsed = ObsSt::from_array(&row_at(
            crate::timefmt::now_epoch(),
            ",0,0,0,0,6,1013,0,0,0,0,0,0,0,0,0,0,1",
        ))
        .expect("decodes");
        let fields = observation_fields(&parsed);
        assert_eq!(value_of(&fields, WeatherField::AirTempF), Some(32.0));
        assert_eq!(value_of(&fields, WeatherField::RhPct), Some(0.0));
        assert_eq!(value_of(&fields, WeatherField::WindMph), Some(0.0));
        assert_eq!(value_of(&fields, WeatherField::LightningCount), Some(0.0));
    }

    /// A row that is short rather than null-filled is the same story: the
    /// slots it never reached publish nothing.
    #[test]
    fn a_truncated_row_publishes_only_what_it_carried() {
        let parsed =
            ObsSt::from_array(&row_at(crate::timefmt::now_epoch(), ",0.18,0.22")).expect("decodes");
        let fields = observation_fields(&parsed);
        let has = |want: WeatherField| fields.iter().any(|(f, _)| *f == want);
        assert!(has(WeatherField::WindMph));
        assert!(has(WeatherField::WindLullMph));
        assert!(!has(WeatherField::AirTempF));
        assert!(!has(WeatherField::PressureInHg));
        assert!(!has(WeatherField::BatteryV));
        assert!(
            !has(WeatherField::LightningCount),
            "a row that never reported a count has not reported a quiet minute"
        );
    }

    /// The three-second sample fires 20 times a minute. One with no
    /// number in it publishes NOTHING: an empty field list is a no-op in
    /// the store, but the bridge still clones the snapshot and takes both
    /// owner mutexes to discover that. The obs_st arm is what keeps this
    /// source's `last_seen` advancing, so nothing reads it as offline for
    /// the omission.
    #[tokio::test]
    async fn a_rapid_wind_sample_with_nothing_in_it_publishes_nothing() {
        let cfg = TempestUdpConfig {
            bind_addr: "0.0.0.0:50222".into(),
            hub_serial: None,
        };
        let src = TempestUdp::new("tempest".into(), cfg.clone(), 100, None);
        let (tx, mut rx) = tokio::sync::broadcast::channel::<SourceEvent>(16);
        let t = crate::timefmt::now_epoch();

        src.publish(rapid_wind(t, ",null,null"), &cfg, &tx).await;
        assert!(
            rx.try_recv().is_err(),
            "a sample with no number in it is not an observation"
        );

        // One real number is still a sample, and publishes only itself.
        src.publish(rapid_wind(t, ",2.0,null"), &cfg, &tx).await;
        let evt = rx.try_recv().expect("a sample with a speed publishes");
        match evt {
            SourceEvent::Observation {
                fields, at_epoch, ..
            } => {
                assert_eq!(at_epoch, t);
                assert_eq!(fields.len(), 1, "only the reading it carried");
                assert_eq!(fields[0].0, WeatherField::RapidWindMph);
            }
            other => panic!("expected an Observation, got {other:?}"),
        }
    }

    #[test]
    fn a_named_hub_filters_every_other_hub() {
        let mut cfg = TempestUdpConfig {
            bind_addr: "0.0.0.0:50222".into(),
            hub_serial: None,
        };
        assert!(hub_matches(&cfg, "HB-1"), "unset takes any hub");
        cfg.hub_serial = Some("HB-1".into());
        assert!(hub_matches(&cfg, "hb-1"), "serials are case-insensitive");
        assert!(!hub_matches(&cfg, "HB-2"));
    }
}
