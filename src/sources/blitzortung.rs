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
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use serde::Deserialize;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use crate::config::schema::{
    default_blitzortung_hosts, BlitzortungConfig, BlitzortungMqtt, BlitzortungTransport,
};
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

/// Parse the three fields LocalSky uses from one decoded strike record.
///
/// Fast path: strict serde, which is valid for the `complete` /
/// `lzw_complete` topics and today's WebSocket frames. Fallback: a
/// tolerant head extractor for the `core` / `lzw_core` topics, whose
/// records are NOT valid JSON (`"mdecs":undefined` plus a `"region",N`
/// comma-for-colon), so serde rejects them. The fallback reads only the
/// record head, before any `sig[` station array, so a station's own
/// lat/lon/time can never be captured by mistake, and it parses the raw
/// `time` as an i64 to keep full nanosecond precision (an f64 would lose
/// the low digits the dedup key depends on).
pub fn parse_strike(s: &str) -> Option<RawStrike> {
    if let Ok(raw) = serde_json::from_str::<RawStrike>(s) {
        return Some(raw);
    }
    extract_raw_strike(s)
}

fn extract_raw_strike(s: &str) -> Option<RawStrike> {
    // Truncate at the first '[' so the sig[] station array (each entry of
    // which carries its own "lat"/"lon"/"time") is out of reach.
    let head = match s.find('[') {
        Some(i) => &s[..i],
        None => s,
    };
    let lat = find_f64_field(head, "\"lat\":")?;
    let lon = find_f64_field(head, "\"lon\":")?;
    let time = find_i64_field(head, "\"time\":").unwrap_or(0);
    Some(RawStrike { time, lat, lon })
}

/// Byte offset just past `key`'s colon, but only where `key` sits at the
/// start of a top-level field (immediately after `{` or `,`). This skips
/// look-alikes such as `"latc":`/`"blat":` and any occurrence of the key
/// inside a string value.
fn field_value_start(s: &str, key: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = s[from..].find(key) {
        let at = from + rel;
        match s[..at].chars().next_back() {
            Some('{') | Some(',') => return Some(at + key.len()),
            _ => from = at + key.len(),
        }
    }
    None
}

fn find_f64_field(s: &str, key: &str) -> Option<f64> {
    parse_leading_f64(&s[field_value_start(s, key)?..])
}

fn find_i64_field(s: &str, key: &str) -> Option<i64> {
    parse_leading_i64(&s[field_value_start(s, key)?..])
}

/// Parse a leading JSON number (optional sign, fraction, exponent),
/// consuming only the numeric prefix. Rejects trailing junk by
/// construction and, unlike a `"lat":(-?\d+(?:\.\d+)?)` regex, does not
/// silently drop an exponent (which would read `1.2e-3` as `1.2`).
fn parse_leading_f64(s: &str) -> Option<f64> {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut end = 0;
    if matches!(b.first(), Some(b'-') | Some(b'+')) {
        end += 1;
    }
    let mut saw_digit = false;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
        saw_digit = true;
    }
    if end < b.len() && b[end] == b'.' {
        end += 1;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
            saw_digit = true;
        }
    }
    if !saw_digit {
        return None;
    }
    if end < b.len() && (b[end] == b'e' || b[end] == b'E') {
        let mut e = end + 1;
        if e < b.len() && matches!(b[e], b'-' | b'+') {
            e += 1;
        }
        let mut exp_digit = false;
        while e < b.len() && b[e].is_ascii_digit() {
            e += 1;
            exp_digit = true;
        }
        if exp_digit {
            end = e;
        }
    }
    t[..end].parse::<f64>().ok()
}

/// Parse a leading signed integer (the nanosecond `time`), consuming only
/// the digit prefix. Kept separate from the float parser so the 19-digit
/// nanosecond value round-trips exactly as an i64.
fn parse_leading_i64(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut end = 0;
    if matches!(b.first(), Some(b'-') | Some(b'+')) {
        end += 1;
    }
    let digits_start = end;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return None;
    }
    t[..end].parse::<i64>().ok()
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
    // Reject non-finite or out-of-range coordinates before they reach the
    // map. The tolerant `core` parser and the unversioned feed can both
    // yield a garbage coordinate; the radius check alone would still keep
    // a wrong value that happens to land near the station.
    if !raw.lat.is_finite()
        || !raw.lon.is_finite()
        || !(-90.0..=90.0).contains(&raw.lat)
        || !(-180.0..=180.0).contains(&raw.lon)
    {
        return None;
    }
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
        // Raw nanosecond time is the dedup key (see StrikeEvent::id). 0
        // when the feed omitted it, which also disables dedup for that
        // strike (it cannot be identified across refinements anyway).
        id: raw.time.max(0),
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
    // Miles -> km once; both transports share the local radius filter.
    let radius_km = config.radius_mi.max(0.0) * 1.609_344;
    match config.transport {
        BlitzortungTransport::WebSocket => {
            run_websocket(id, &config, store, station, radius_km, last_seen).await
        }
        BlitzortungTransport::Mqtt => {
            run_mqtt(id, &config, store, station, radius_km, last_seen).await
        }
    }
}

/// Legacy public web-map firehose: rotate ws1/ws2/ws7/ws8 on failure
/// with jittered exponential backoff.
async fn run_websocket(
    id: String,
    config: &BlitzortungConfig,
    store: Arc<TempestStore>,
    station: (f64, f64),
    radius_km: f64,
    last_seen: Option<SourceLastSeen>,
) {
    let hosts = effective_hosts(&config.hosts);
    // Shuffled start so a fleet of instances doesn't pile onto ws1.
    let mut host_idx = rand::rng().random_range(0..hosts.len());
    let mut backoff = BACKOFF_MIN;
    let mut was_streaming = true; // first failure logs at warn
    info!(
        source_id = %id,
        hosts = hosts.len(),
        radius_mi = config.radius_mi,
        "blitzortung community lightning feed enabled (WebSocket transport, display-only layer)"
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

/// Dedicated Blitzortung MQTT broker: a single durable subscription with
/// the same jittered backoff discipline (no host rotation; one broker).
async fn run_mqtt(
    id: String,
    config: &BlitzortungConfig,
    store: Arc<TempestStore>,
    station: (f64, f64),
    radius_km: f64,
    last_seen: Option<SourceLastSeen>,
) {
    let mqtt = &config.mqtt;
    let mut backoff = BACKOFF_MIN;
    let mut was_streaming = true;
    info!(
        source_id = %id,
        host = %mqtt.host,
        port = mqtt.port,
        topic = %mqtt.topic,
        radius_mi = config.radius_mi,
        "blitzortung community lightning feed enabled (MQTT transport, display-only layer)"
    );
    loop {
        let mut frames: u64 = 0;
        let err = connect_and_stream_mqtt(
            mqtt,
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
            backoff = BACKOFF_MIN;
            was_streaming = true;
        }
        if was_streaming {
            warn!(source_id = %id, host = %mqtt.host, error = %format!("{err:#}"),
                  "blitzortung mqtt connection lost; backing off");
        } else {
            debug!(source_id = %id, host = %mqtt.host, error = %format!("{err:#}"),
                   "blitzortung mqtt still unreachable");
        }
        was_streaming = false;
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
        // Decode the LZW frame, then parse + radius-filter on the shared
        // path (serde fast path, tolerant fallback). WebSocket frames are
        // the `complete` shape so serde succeeds; the fallback only ever
        // engages if the unversioned feed drifts to the `core` shape.
        if !ingest_text(&decode_frame(&txt), station, radius_km, &mut pending) {
            // Unparseable frame = protocol drift. Log quietly and keep
            // reading; one bad frame is not a reason to drop a healthy
            // connection.
            debug!(source_id = %id, "blitzortung frame did not parse");
        }
        if !pending.is_empty() && last_flush.elapsed() >= FLUSH_INTERVAL {
            flush(store, &mut pending);
            last_flush = tokio::time::Instant::now();
        }
    }
}

/// Parse one decoded strike record and, when it falls inside the radius,
/// queue it. Returns false only if the record did not parse at all, so
/// the caller can log protocol drift. Shared by both transports.
fn ingest_text(
    text: &str,
    station: (f64, f64),
    radius_km: f64,
    pending: &mut Vec<StrikeEvent>,
) -> bool {
    let Some(raw) = parse_strike(text) else {
        return false;
    };
    if let Some(evt) = to_strike_event(
        &raw,
        station.0,
        station.1,
        radius_km,
        chrono::Utc::now().timestamp(),
    ) {
        pending.push(evt);
    }
    true
}

/// One MQTT connection lifetime: connect, subscribe on ConnAck, then
/// ingest published strikes until the event loop errors or goes silent.
/// Always returns Err describing why it ended; unflushed strikes are
/// applied first so none are lost across a reconnect.
async fn connect_and_stream_mqtt(
    cfg: &BlitzortungMqtt,
    id: &str,
    store: &Arc<TempestStore>,
    station: (f64, f64),
    radius_km: f64,
    last_seen: Option<&SourceLastSeen>,
    frames: &mut u64,
) -> anyhow::Result<()> {
    let mut opts = MqttOptions::new(format!("localsky-{id}"), &cfg.host, cfg.port);
    opts.set_keep_alive(Duration::from_secs(30));
    if !cfg.username.is_empty() {
        opts.set_credentials(&cfg.username, &cfg.password);
    }
    // `complete` strikes reach a few KB; give the incoming buffer headroom
    // above rumqttc's small default so a large payload is never truncated.
    opts.set_max_packet_size(512 * 1024, 512 * 1024);
    let (client, mut eventloop) = AsyncClient::new(opts, 64);
    // Topics carrying "lzw" are LZW-compressed; the plain topics are not.
    let is_lzw = cfg.topic.contains("lzw");
    let mut pending: Vec<StrikeEvent> = Vec::new();
    let mut last_flush = tokio::time::Instant::now();
    loop {
        let event = match tokio::time::timeout(FRAME_SILENCE, eventloop.poll()).await {
            Err(_) => {
                flush(store, &mut pending);
                anyhow::bail!(
                    "no frames for {}s (global feed never goes quiet; treating as dead)",
                    FRAME_SILENCE.as_secs()
                );
            }
            Ok(Err(e)) => {
                flush(store, &mut pending);
                return Err(anyhow::Error::from(e).context("mqtt eventloop"));
            }
            Ok(Ok(event)) => event,
        };
        match event {
            // Subscribe on (re)connect so a reconnect re-subscribes.
            Event::Incoming(Packet::ConnAck(_)) => {
                client
                    .subscribe(&cfg.topic, QoS::AtMostOnce)
                    .await
                    .context("mqtt subscribe")?;
                debug!(source_id = %id, topic = %cfg.topic, "blitzortung mqtt connected + subscribing");
            }
            Event::Incoming(Packet::Publish(p)) => {
                *frames += 1;
                // Every message proves liveness for /api/health, even when
                // no strike lands inside the radius for hours.
                if let Some(ls) = last_seen {
                    ls.record(id, chrono::Utc::now().timestamp());
                }
                let payload = String::from_utf8_lossy(&p.payload);
                let text = if is_lzw {
                    decode_frame(&payload)
                } else {
                    payload.into_owned()
                };
                if !ingest_text(&text, station, radius_km, &mut pending) {
                    debug!(source_id = %id, "blitzortung mqtt message did not parse");
                }
                if !pending.is_empty() && last_flush.elapsed() >= FLUSH_INTERVAL {
                    flush(store, &mut pending);
                    last_flush = tokio::time::Instant::now();
                }
            }
            _ => {}
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
    fn parse_strike_tolerates_core_invalid_json() {
        // The `core` topic is NOT valid JSON: a bare `undefined` and a
        // `"region",N` comma-for-colon. serde rejects it; the tolerant
        // fallback must still recover time/lat/lon at full ns precision.
        let core = r#"{"time":1782914975455557600,"lat":51.243883,"lon":12.150345,"alt":0,"pol":0,"mdecs":undefined,"mcg":171,"status":2,"region",9}"#;
        assert!(serde_json::from_str::<RawStrike>(core).is_err());
        let raw = parse_strike(core).expect("tolerant fallback parses core");
        assert_eq!(raw.time, 1_782_914_975_455_557_600);
        assert!((raw.lat - 51.243883).abs() < 1e-9);
        assert!((raw.lon - 12.150345).abs() < 1e-9);
    }

    #[test]
    fn parse_strike_uses_serde_fast_path_for_complete() {
        let complete = r#"{"time":7,"lat":51.24,"lon":12.15,"alt":0,"pol":0,"mds":9,"mcg":1,"status":2,"region":9,"sig":[{"sta":1,"time":2,"lat":50.9,"lon":11.0,"alt":2,"status":12}]}"#;
        let raw = parse_strike(complete).unwrap();
        assert_eq!(raw.time, 7);
        assert!((raw.lat - 51.24).abs() < 1e-9);
        assert!((raw.lon - 12.15).abs() < 1e-9);
    }

    #[test]
    fn extractor_reads_head_never_nested_sig_station() {
        // If the fallback ever runs on a complete-shaped record (serde
        // rejected it), it must read the strike's own coords, never a
        // sig[] station's (which follow the first '['). Station here is
        // 50.94/11.09 with time 269883; the strike is 51.24/12.15.
        let with_sig = r#"{"time":1782914975455557600,"lat":51.243883,"lon":12.150345,"alt":0,"mds":9,"status":2,"region":9,"sig":[{"sta":1,"time":269883,"lat":50.947643,"lon":11.092339,"alt":253,"status":12}]}"#;
        let raw = extract_raw_strike(with_sig).unwrap();
        assert!((raw.lat - 51.243883).abs() < 1e-9);
        assert!((raw.lon - 12.150345).abs() < 1e-9);
        assert_eq!(raw.time, 1_782_914_975_455_557_600);
    }

    #[test]
    fn extractor_handles_scientific_notation_and_key_lookalikes() {
        // Scientific notation must NOT truncate to 1.2 (a naive
        // `\d+(\.\d+)?` regex would drop the exponent).
        let sci = r#"{"time":5,"lat":1.2e-3,"lon":-0.0}"#;
        let raw = extract_raw_strike(sci).unwrap();
        assert!((raw.lat - 0.0012).abs() < 1e-12);
        assert!(raw.lon.abs() < 1e-12);
        // A look-alike key preceding the real one must not be captured.
        let lookalike = r#"{"time":5,"latitude":9.9,"lat":51.2,"lon":12.1}"#;
        let raw = extract_raw_strike(lookalike).unwrap();
        assert!((raw.lat - 51.2).abs() < 1e-9);
    }

    #[test]
    fn out_of_range_coordinates_are_dropped() {
        // A garbage coordinate that still lands "near" the station must
        // not survive: range check rejects it before the radius check.
        let bad = RawStrike {
            time: 1_781_278_922_596_972_800,
            lat: 999.0,
            lon: 12.0,
        };
        assert!(to_strike_event(&bad, 40.0, 23.0, 200.0, 1_781_278_900).is_none());
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
