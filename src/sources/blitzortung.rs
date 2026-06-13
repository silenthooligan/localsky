// Blitzortung.org community lightning feed (opt-in, display-only).
//
// Connects to the same public websocket firehose the blitzortung.org
// map uses (wss://ws1/ws2/ws7/ws8.blitzortung.org, verified live
// 2026-06-12), subscribes with the fixed token {"a":111}, LZW-decodes
// each text frame into one strike JSON, keeps strikes within
// `radius_mi` of the station, and feeds them into the TempestStore
// lightning ring buffer tagged source="blitzortung". The existing
// snapshot + SSE + radar plumbing then carries them with zero new
// endpoints: /api/snapshot hydrates the map layer's backlog and
// /api/v1/stream pushes live updates. Tempest and Blitzortung strikes
// coexist in the buffer; no dedupe (different detection networks).
//
// Boundaries, straight from blitzortung.org's terms (contact page,
// captured 2026-06-12):
//   - private, non-commercial use; data is CC BY-SA 4.0 and the UI
//     must show visible attribution wherever strikes render
//   - NEVER a storm-warning/safety feature and NEVER an automation
//     input. The engine does not read lightning_recent today; keep it
//     that way (detection delay alone is 3-8 s plus transport).
//   - default OFF: double opt-in (entry enabled + config enabled)
//   - LocalSky never rebroadcasts strike data from project servers
//
// Protocol notes (all verified live): one text frame per strike
// (~1 KB compressed, ~7/s globally), no server-side geo filter, no
// heartbeat; the firehose itself is the liveness signal, so >60 s of
// frame silence means the connection is dead even if TCP disagrees.
// The host set churns across their web-client releases, hence config
// over constants. The subscription message is a fixed token; nothing
// identifying is ever sent (no User-Agent, no Origin, no account).
//
// Like ecowitt_gw_poll, this is intentionally NOT a `WeatherSource`:
// that trait's run loop only gets the merge bus, and strikes feed the
// TempestStore display buffer instead. main.rs spawns it directly and
// it runs until the process exits (same contract as the refreshers).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use futures::{SinkExt, StreamExt};
use rand::RngExt;
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::config::schema::{default_blitzortung_hosts, BlitzortungConfig};
use crate::sources::bus_recorder::SourceLastSeen;
use crate::tempest::packets::{StrikeEvent, STRIKE_SOURCE_BLITZORTUNG};
use crate::tempest::state::TempestStore;

/// Fixed subscription token the blitzortung.org web client sends after
/// connect. Carries no identity and is the entire handshake.
const SUBSCRIBE_MSG: &str = "{\"a\":111}";

/// The global strike rate never drops anywhere near zero, so a minute
/// without a single frame means the connection is dead.
const FRAME_SILENCE: Duration = Duration::from_secs(60);

/// Accepted strikes are batched and applied at most this often, so a
/// storm sitting on top of the station doesn't swap + broadcast a full
/// snapshot for every individual strike.
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// Jittered exponential reconnect backoff bounds (the SSE-loop
/// discipline used by the HACS coordinator and radar.js). The cap is
/// deliberately long: the network is volunteer-run with no SLA, and a
/// quiet stale layer beats hammering donated servers.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// Decode one Blitzortung LZW text frame. This is the well-known
/// variant shared by their JS map client and the HA integration, and
/// it operates on Unicode CODE POINTS, not bytes: the dictionary
/// starts empty with the next code at 256, the first char passes
/// through, chars < 256 are literals, codes >= 256 look up the
/// dictionary with the classic not-yet-defined-code fallback of
/// prev + prev[0], and after each emit dict[next++] = prev + first
/// char of the emitted entry. Real frames contain chars >= 256 by
/// design (e.g. U+0109 inline), so any byte-oriented decoder breaks.
///
/// Multi-byte UTF-8 inside the original payload decodes to one char
/// per byte (a Latin-1-style mojibake), but that can only occur inside
/// sig[] station names, which LocalSky never reads; every field we
/// parse is plain ASCII JSON.
pub fn decode_frame(frame: &str) -> String {
    let mut chars = frame.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };
    let mut out = String::with_capacity(frame.len() * 4);
    out.push(first);
    // Dictionary indexed by (code - 256), built as we emit.
    let mut dict: Vec<String> = Vec::new();
    let mut prev = first.to_string();
    for c in chars {
        let code = c as usize;
        let entry = if code < 256 {
            c.to_string()
        } else {
            match dict.get(code - 256) {
                Some(s) => s.clone(),
                None => {
                    // Code referenced before definition: cScSc pattern.
                    let mut s = prev.clone();
                    s.extend(prev.chars().next());
                    s
                }
            }
        };
        out.push_str(&entry);
        let mut next = prev;
        next.extend(entry.chars().next());
        dict.push(next);
        prev = entry;
    }
    out
}

/// The subset of a decoded strike frame LocalSky uses. serde skips
/// unknown fields by default, which is exactly the memory strip the
/// firehose needs: the 20-40 entry `sig` station array (and alt/pol/
/// mds/mcg/region/delay) is never deserialized, let alone buffered.
#[derive(Debug, Clone, Deserialize)]
pub struct RawStrike {
    /// Nanoseconds since the Unix epoch.
    #[serde(default)]
    pub time: i64,
    pub lat: f64,
    pub lon: f64,
}

/// Great-circle distance in km (haversine, R = 6371 km). Plenty
/// accurate for a keep/drop radius check.
pub fn haversine_km(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dp = (lat2 - lat1).to_radians();
    let dl = (lon2 - lon1).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dl / 2.0).sin().powi(2);
    2.0 * 6371.0 * a.sqrt().asin()
}

/// Radius filter + shape adapter: a decoded strike inside `radius_km`
/// of the station becomes a tagged StrikeEvent for the shared
/// lightning buffer; anything farther is dropped on the floor.
/// `now_epoch` backstops a frame with a missing/garbage timestamp.
pub fn to_strike_event(
    raw: &RawStrike,
    station_lat: f64,
    station_lon: f64,
    radius_km: f64,
    now_epoch: i64,
) -> Option<StrikeEvent> {
    let distance_km = haversine_km(station_lat, station_lon, raw.lat, raw.lon);
    if distance_km > radius_km {
        return None;
    }
    let time_epoch = if raw.time > 0 {
        raw.time / 1_000_000_000
    } else {
        now_epoch
    };
    Some(StrikeEvent {
        time_epoch,
        distance_km,
        energy: 0,
        source: STRIKE_SOURCE_BLITZORTUNG.to_string(),
        lat: Some(raw.lat),
        lon: Some(raw.lon),
    })
}

/// The configured host list, or the schema defaults when the operator
/// emptied it (an empty list would otherwise mean "enabled but can
/// never connect", which is never what anyone wants).
pub fn effective_hosts(configured: &[String]) -> Vec<String> {
    if configured.is_empty() {
        default_blitzortung_hosts()
    } else {
        configured.to_vec()
    }
}

/// Spawn the feed. No-op unless the config-level opt-in is set (the
/// caller already filters on the entry-level `enabled`). Runs until
/// the process exits, like the other main.rs-spawned loops.
pub fn spawn(
    id: String,
    config: BlitzortungConfig,
    store: Arc<TempestStore>,
    station: (f64, f64),
    last_seen: Option<SourceLastSeen>,
) {
    if !config.enabled {
        info!(
            source_id = %id,
            "blitzortung: configured but not opted in (config.enabled=false); not connecting"
        );
        return;
    }
    tokio::spawn(run(id, config, store, station, last_seen));
}

async fn run(
    id: String,
    config: BlitzortungConfig,
    store: Arc<TempestStore>,
    station: (f64, f64),
    last_seen: Option<SourceLastSeen>,
) {
    let hosts = effective_hosts(&config.hosts);
    let radius_km = config.radius_mi.max(0.0) * 1.609_344;
    // Shuffled start so a fleet of instances doesn't pile onto ws1.
    let mut host_idx = rand::rng().random_range(0..hosts.len());
    let mut backoff = BACKOFF_MIN;
    let mut was_streaming = true; // first failure logs at warn
    info!(
        source_id = %id,
        hosts = hosts.len(),
        radius_mi = config.radius_mi,
        "blitzortung community lightning feed enabled (display-only layer)"
    );
    loop {
        let url = &hosts[host_idx % hosts.len()];
        let mut frames: u64 = 0;
        let err = connect_and_stream(
            url,
            &id,
            &store,
            station,
            radius_km,
            last_seen.as_ref(),
            &mut frames,
        )
        .await
        .unwrap_err(); // the stream loop only returns by failing
        if frames > 0 {
            // The connection was healthy before it died; reconnect fast.
            backoff = BACKOFF_MIN;
            was_streaming = true;
        }
        // Degrade quietly (volunteer-run network, outages are normal):
        // warn once on the streaming -> down transition, debug after.
        if was_streaming {
            warn!(source_id = %id, host = %url, error = %format!("{err:#}"),
                  "blitzortung connection lost; rotating host with backoff");
        } else {
            debug!(source_id = %id, host = %url, error = %format!("{err:#}"),
                   "blitzortung still unreachable");
        }
        was_streaming = false;
        host_idx = host_idx.wrapping_add(1);
        let jitter_ms = rand::rng().random_range(0..=(backoff.as_millis() as u64 / 2).max(1));
        tokio::time::sleep(backoff + Duration::from_millis(jitter_ms)).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One connection lifetime: connect, subscribe, then decode frames
/// until the stream errors, closes, or goes silent. Always returns
/// Err describing why the connection ended; accepted-but-unflushed
/// strikes are applied before returning so none are lost across a
/// reconnect.
async fn connect_and_stream(
    url: &str,
    id: &str,
    store: &Arc<TempestStore>,
    station: (f64, f64),
    radius_km: f64,
    last_seen: Option<&SourceLastSeen>,
    frames: &mut u64,
) -> anyhow::Result<()> {
    let (mut ws, _) = connect_async(url).await.context("websocket connect")?;
    ws.send(Message::Text(SUBSCRIBE_MSG.into()))
        .await
        .context("send subscription")?;
    debug!(source_id = %id, host = %url, "blitzortung connected + subscribed");
    let mut pending: Vec<StrikeEvent> = Vec::new();
    let mut last_flush = tokio::time::Instant::now();
    loop {
        let msg = match tokio::time::timeout(FRAME_SILENCE, ws.next()).await {
            Err(_) => {
                flush(store, &mut pending);
                anyhow::bail!(
                    "no frames for {}s (global feed never goes quiet; treating as dead)",
                    FRAME_SILENCE.as_secs()
                );
            }
            Ok(None) => {
                flush(store, &mut pending);
                anyhow::bail!("stream closed by server");
            }
            Ok(Some(Err(e))) => {
                flush(store, &mut pending);
                return Err(anyhow::Error::from(e).context("stream error"));
            }
            Ok(Some(Ok(m))) => m,
        };
        let Message::Text(txt) = msg else {
            continue; // ping/pong/binary
        };
        *frames += 1;
        // Every decoded frame proves liveness for /api/health, even
        // when no strike lands inside the radius for hours.
        if let Some(ls) = last_seen {
            ls.record(id, chrono::Utc::now().timestamp());
        }
        match serde_json::from_str::<RawStrike>(&decode_frame(&txt)) {
            Ok(raw) => {
                if let Some(evt) = to_strike_event(
                    &raw,
                    station.0,
                    station.1,
                    radius_km,
                    chrono::Utc::now().timestamp(),
                ) {
                    pending.push(evt);
                }
            }
            // Unparseable frame = protocol drift (unversioned feed).
            // Log quietly and keep reading; one bad frame is not a
            // reason to drop a healthy connection.
            Err(e) => debug!(source_id = %id, error = %e, "blitzortung frame did not parse"),
        }
        if !pending.is_empty() && last_flush.elapsed() >= FLUSH_INTERVAL {
            flush(store, &mut pending);
            last_flush = tokio::time::Instant::now();
        }
    }
}

fn flush(store: &Arc<TempestStore>, pending: &mut Vec<StrikeEvent>) {
    if pending.is_empty() {
        return;
    }
    store.apply_strikes(pending);
    pending.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LZW compressor mirroring decode_frame (and the JS client's
    /// encoder): emits single chars literally and dictionary strings
    /// as code points 256+, in dictionary-insertion order. Used to
    /// produce wire-shaped fixtures from captured plaintext.
    fn encode_lzw(input: &str) -> String {
        use std::collections::HashMap;
        fn emit(out: &mut String, w: &str, dict: &HashMap<String, u32>) {
            let mut it = w.chars();
            let first = it.next().expect("emit on empty block");
            if it.next().is_none() {
                out.push(first);
            } else {
                out.push(char::from_u32(dict[w]).expect("code collides with surrogate range"));
            }
        }
        let mut dict: HashMap<String, u32> = HashMap::new();
        let mut next = 256u32;
        let mut out = String::new();
        let mut w = String::new();
        for c in input.chars() {
            let mut wc = w.clone();
            wc.push(c);
            if w.is_empty() || dict.contains_key(&wc) {
                w = wc;
            } else {
                emit(&mut out, &w, &dict);
                dict.insert(wc, next);
                next += 1;
                w = c.to_string();
            }
        }
        if !w.is_empty() {
            emit(&mut out, &w, &dict);
        }
        out
    }

    /// Full decoded frame captured live from ws1.blitzortung.org on
    /// 2026-06-12 ~15:42 UTC (strike near Mount Athos, Greece; the
    /// 40-entry sig[] station array truncated to two entries).
    const CAPTURED_FRAME_PLAINTEXT: &str = concat!(
        "{\"time\":1781278922596972800,\"lat\":40.220814,\"lon\":23.953156,",
        "\"alt\":0,\"pol\":0,\"mds\":11712,\"mcg\":184,\"status\":2,\"region\":9,",
        "\"delay\":4.1,\"lonc\":0,\"latc\":0,\"sig\":[",
        "{\"sta\":1656,\"time\":417772,\"lat\":38.530085,\"lon\":24.038119,\"alt\":0,\"status\":12},",
        "{\"sta\":2731,\"time\":737367,\"lat\":41.443523,\"lon\":26.572418,\"alt\":0,\"status\":12}",
        "]}"
    );

    #[test]
    fn decoder_round_trips_captured_strike_frame() {
        let wire = encode_lzw(CAPTURED_FRAME_PLAINTEXT);
        // A real frame must exercise dictionary codes (chars >= 256;
        // live frames show e.g. U+0109 inline), otherwise this test
        // would only cover the literal passthrough path.
        assert!(
            wire.chars().any(|c| c as u32 >= 256),
            "fixture too short to produce dictionary codes"
        );
        assert!(wire.chars().count() < CAPTURED_FRAME_PLAINTEXT.chars().count());
        assert_eq!(decode_frame(&wire), CAPTURED_FRAME_PLAINTEXT);
    }

    #[test]
    fn decoded_frame_parses_with_sig_stripped() {
        let wire = encode_lzw(CAPTURED_FRAME_PLAINTEXT);
        let raw: RawStrike = serde_json::from_str(&decode_frame(&wire)).unwrap();
        assert_eq!(raw.time, 1_781_278_922_596_972_800);
        assert!((raw.lat - 40.220814).abs() < 1e-9);
        assert!((raw.lon - 23.953156).abs() < 1e-9);
    }

    #[test]
    fn decoder_handles_not_yet_defined_code() {
        // Hand-built vector for the cScSc fallback: code 256 is used
        // in the same step that defines it, so the decoder must emit
        // prev + prev[0]. "a" + chr(256) + "a" decodes to "aaaa".
        let wire = format!("a{}a", char::from_u32(256).unwrap());
        assert_eq!(decode_frame(&wire), "aaaa");
        // And the encoder produces exactly that wire form.
        assert_eq!(encode_lzw("aaaa"), wire);
    }

    #[test]
    fn decoder_tolerates_empty_and_single_char_frames() {
        assert_eq!(decode_frame(""), "");
        assert_eq!(decode_frame("{"), "{");
    }

    #[test]
    fn haversine_matches_known_distances() {
        // 1 degree of longitude on the equator is ~111.19 km.
        assert!((haversine_km(0.0, 0.0, 0.0, 1.0) - 111.19).abs() < 0.5);
        // Zero distance.
        assert!(haversine_km(28.5, -81.4, 28.5, -81.4) < 1e-9);
    }

    #[test]
    fn radius_filter_keeps_near_drops_far() {
        let radius_km = 100.0 * 1.609_344; // the 100 mi default
        let now = 1_781_278_900;
        // ~55 km north of an Orlando station: kept, tagged, located.
        let near = RawStrike {
            time: 1_781_278_922_596_972_800,
            lat: 29.0,
            lon: -81.4,
        };
        let evt = to_strike_event(&near, 28.5, -81.4, radius_km, now).unwrap();
        assert_eq!(evt.source, "blitzortung");
        assert_eq!(evt.lat, Some(29.0));
        assert_eq!(evt.lon, Some(-81.4));
        assert!((evt.distance_km - 55.6).abs() < 1.0);
        // Nanosecond feed time collapses to whole seconds.
        assert_eq!(evt.time_epoch, 1_781_278_922);
        assert_eq!(evt.energy, 0);
        // The Memphis strike from the same capture session: ~900+ km
        // away, dropped.
        let far = RawStrike {
            time: 1_781_278_899_926_667_000,
            lat: 35.221895,
            lon: -90.015792,
        };
        assert!(to_strike_event(&far, 28.5, -81.4, radius_km, now).is_none());
    }

    #[test]
    fn missing_frame_time_falls_back_to_now() {
        let raw = RawStrike {
            time: 0,
            lat: 28.6,
            lon: -81.4,
        };
        let evt = to_strike_event(&raw, 28.5, -81.4, 200.0, 1_700_000_000).unwrap();
        assert_eq!(evt.time_epoch, 1_700_000_000);
    }

    #[test]
    fn config_serde_defaults_are_opt_in() {
        // The load-bearing default: a blitzortung entry whose config
        // omits `enabled` must deserialize to enabled=false, so adding
        // the entry alone never connects to the volunteer servers.
        let entry: crate::config::schema::SourceEntry = serde_json::from_value(serde_json::json!({
            "id": "blitz",
            "kind": "blitzortung",
            "config": {},
        }))
        .unwrap();
        let crate::config::schema::SourceKind::Blitzortung(cfg) = &entry.source else {
            panic!("expected blitzortung kind");
        };
        assert!(!cfg.enabled, "blitzortung must default to opted OUT");
        assert!((cfg.radius_mi - 100.0).abs() < 1e-9);
        assert_eq!(cfg.hosts, default_blitzortung_hosts());
        assert_eq!(cfg.hosts.len(), 4);
        assert!(cfg.hosts.iter().all(|h| h.starts_with("wss://")));
    }

    #[test]
    fn effective_hosts_falls_back_to_defaults() {
        assert_eq!(effective_hosts(&[]), default_blitzortung_hosts());
        let custom = vec!["wss://example.invalid/".to_string()];
        assert_eq!(effective_hosts(&custom), custom);
    }
}
