// Ecowitt gateway local-API poller (Phase E1 of the device-parity effort).
//
// The push counterpart (`ecowitt_local`) waits for the gateway to POST to
// LocalSky, which contends with Home Assistant for the gateway's single
// "Customized" push destination. This POLLS the gateway's read-only local
// HTTP API instead:
//
//   GET http://<host>/get_livedata_info
//
// GW1100 / GW2000 firmware serves a JSON blob with the live readings of
// every attached sensor. Polling it doesn't touch the push slot, so HA's
// Ecowitt integration keeps working unchanged while LocalSky reads the same
// hardware natively. The parsed readings are written to `sensor_history`
// keyed exactly the way the push ingest keys them (`soilmoisture1..N`,
// `tempf`, `humidity`, ...), so a zone's `soil_sensor_id` of
// `source:<id>:soilmoisture1` resolves identically whether the gateway is
// pushed or polled.
//
// This is intentionally NOT a `WeatherSource`: that trait's run loop only
// gets the merge bus, and the value here is the soil channels, which flow
// through sensor_history + `resolve_soil_pct`, not the (not-yet-live) merge
// engine. It's spawned directly from main.rs like the Tempest/forecast
// refreshers.

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::config::schema::EcowittGwPollConfig;
use crate::persistence::sensor_history::Reading;
use crate::persistence::SensorHistoryStore;

/// Parse the leading numeric portion of an Ecowitt `val` string. The gateway
/// appends units and symbols ("56%", "3.13 mph", "0.0 in", "71.6"); we take
/// the leading sign/digits/decimal run and parse that.
fn parse_num(s: &str) -> Option<f64> {
    let trimmed = s.trim();
    let end = trimmed
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+'))
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    let head = &trimmed[..end];
    if head.is_empty() {
        return None;
    }
    head.parse::<f64>().ok()
}

/// Map a `common_list` Ecowitt id code to the sensor_history key the push
/// ingest would use. Ecowitt firmware reports these as hex strings; a few
/// firmwares use bare decimals, so we match both forms. Unknown ids are
/// dropped (best-effort: the soil channels are the load-bearing part).
fn common_key(id: &str) -> Option<&'static str> {
    match id.to_ascii_lowercase().as_str() {
        "0x02" | "2" => Some("tempf"),
        "0x03" | "3" => Some("dewpointf"),
        "0x07" | "7" => Some("humidity"),
        "0x0b" | "11" => Some("windspeedmph"),
        "0x0c" | "12" => Some("windgustmph"),
        "0x19" | "25" => Some("maxdailygust"),
        "0x15" => Some("solarradiation_lux"),
        "0x16" => Some("solarradiation"),
        "0x17" => Some("uv"),
        _ => None,
    }
}

/// Map a `rain` block id to its sensor_history key.
fn rain_key(id: &str) -> Option<&'static str> {
    match id.to_ascii_lowercase().as_str() {
        "0x0e" | "14" => Some("rainratein"),
        "0x10" | "16" => Some("dailyrainin"),
        "0x11" | "17" => Some("weeklyrainin"),
        "0x13" | "19" => Some("yearlyrainin"),
        _ => None,
    }
}

/// Parse one `/get_livedata_info` body into sensor_history readings. Pure +
/// defensive: every block is optional and malformed entries are skipped, so
/// a firmware that omits or renames a block degrades gracefully rather than
/// failing the whole poll.
pub fn parse_livedata(body: &Value, source_id: &str, epoch: i64) -> Vec<Reading> {
    let mut out = Vec::new();
    let mut push = |key: String, value: f64| {
        out.push(Reading {
            epoch,
            source_id: source_id.to_string(),
            key,
            value,
        });
    };

    // common_list — outdoor temp / humidity / wind / solar / uv.
    if let Some(arr) = body.get("common_list").and_then(Value::as_array) {
        for item in arr {
            let (Some(id), Some(val)) = (
                item.get("id").and_then(Value::as_str),
                item.get("val").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let (Some(key), Some(v)) = (common_key(id), parse_num(val)) {
                push(key.to_string(), v);
            }
        }
    }

    // rain block — daily/rate/etc.
    if let Some(arr) = body.get("rain").and_then(Value::as_array) {
        for item in arr {
            let (Some(id), Some(val)) = (
                item.get("id").and_then(Value::as_str),
                item.get("val").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let (Some(key), Some(v)) = (rain_key(id), parse_num(val)) {
                push(key.to_string(), v);
            }
        }
    }

    // wh25 — the gateway's own indoor temp / humidity / pressure block.
    if let Some(arr) = body.get("wh25").and_then(Value::as_array) {
        if let Some(item) = arr.first() {
            if let Some(v) = item
                .get("intemp")
                .and_then(Value::as_str)
                .and_then(parse_num)
            {
                push("tempinf".to_string(), v);
            }
            if let Some(v) = item
                .get("inhumi")
                .and_then(Value::as_str)
                .and_then(parse_num)
            {
                push("humidityin".to_string(), v);
            }
            // Absolute pressure preferred; fall back to relative.
            if let Some(v) = item
                .get("abs")
                .or_else(|| item.get("rel"))
                .and_then(Value::as_str)
                .and_then(parse_num)
            {
                push("baromabsin".to_string(), v);
            }
        }
    }

    // Soil-moisture probes. Two firmware shapes: classic `ch_soil` (WH51)
    // and `ch_ec` (the newer EC soil sensors, which this firmware uses).
    // Both expose `humidity` as the moisture %; we key it `soilmoistureN`
    // either way so `source:<id>:soilmoistureN` resolves regardless of probe
    // type. `ch_ec` additionally carries soil temp + EC, which we record so
    // the engine can use them later (EC-aware skip rules, salt flushing).
    for block in ["ch_soil", "ch_ec"] {
        if let Some(arr) = body.get(block).and_then(Value::as_array) {
            for item in arr {
                let Some(ch) = item.get("channel").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(v) = item
                    .get("humidity")
                    .and_then(Value::as_str)
                    .and_then(parse_num)
                {
                    push(format!("soilmoisture{ch}"), v);
                }
                if let Some(v) = item.get("temp").and_then(Value::as_str).and_then(parse_num) {
                    push(format!("soiltemp{ch}f"), v);
                }
                if let Some(v) = item.get("ec").and_then(Value::as_str).and_then(parse_num) {
                    push(format!("soilec{ch}"), v);
                }
            }
        }
    }

    // ch_temp / ch_aisle — WH31 temp+humidity channels. Best-effort extras.
    for block in ["ch_temp", "ch_aisle"] {
        if let Some(arr) = body.get(block).and_then(Value::as_array) {
            for item in arr {
                let Some(ch) = item.get("channel").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(v) = item.get("temp").and_then(Value::as_str).and_then(parse_num) {
                    push(format!("temp{ch}f"), v);
                }
                if let Some(v) = item
                    .get("humidity")
                    .and_then(Value::as_str)
                    .and_then(parse_num)
                {
                    push(format!("humidity{ch}"), v);
                }
            }
        }
    }

    out
}

/// Spawn the poll loop. Runs until the process exits (no per-source shutdown
/// signal, same contract as the Tempest/forecast refreshers in main.rs).
/// A `None` history store makes this a no-op (nothing to write to).
pub fn spawn(id: String, config: EcowittGwPollConfig, history: Option<SensorHistoryStore>) {
    let Some(history) = history else {
        warn!(source_id = %id, "ecowitt_gw_poll: no sensor_history store; poller disabled");
        return;
    };
    let client = Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .expect("reqwest client construction");
    let url = format!("http://{}/get_livedata_info", config.host);
    let interval = Duration::from_secs(config.poll_interval_s.max(5) as u64);

    tokio::spawn(async move {
        info!(source_id = %id, host = %config.host, interval_s = config.poll_interval_s,
              "ecowitt_gw_poll started");
        let mut tick = tokio::time::interval(interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_ok: Option<bool> = None;
        loop {
            tick.tick().await;
            match fetch(&client, &url).await {
                Ok(body) => {
                    if last_ok != Some(true) {
                        info!(source_id = %id, "ecowitt_gw_poll reachable");
                        last_ok = Some(true);
                    }
                    let epoch = chrono::Utc::now().timestamp();
                    let readings = parse_livedata(&body, &id, epoch);
                    if readings.is_empty() {
                        debug!(source_id = %id, "ecowitt_gw_poll: no parseable readings");
                        continue;
                    }
                    let n = readings.len();
                    if let Err(e) = history.insert_many(readings).await {
                        warn!(source_id = %id, error = %e, "ecowitt_gw_poll sensor_history write failed");
                    } else {
                        debug!(source_id = %id, readings = n, "ecowitt_gw_poll recorded");
                    }
                }
                Err(e) => {
                    if last_ok != Some(false) {
                        warn!(source_id = %id, error = %e, "ecowitt_gw_poll unreachable");
                        last_ok = Some(false);
                    }
                }
            }
        }
    });
}

async fn fetch(client: &Client, url: &str) -> anyhow::Result<Value> {
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.json::<Value>().await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        // Abridged GW2000 /get_livedata_info, real-world shape.
        json!({
            "common_list": [
                {"id": "0x02", "val": "71.6"},
                {"id": "0x07", "val": "56%"},
                {"id": "0x03", "val": "54.9"},
                {"id": "0x0B", "val": "3.13 mph"},
                {"id": "0x17", "val": "6"}
            ],
            "rain": [
                {"id": "0x0E", "val": "0.0 in/Hr"},
                {"id": "0x10", "val": "0.24 in"}
            ],
            "ch_soil": [
                {"channel": "1", "name": "Back Yard", "battery": "5", "humidity": "45%"},
                {"channel": "2", "name": "Front", "battery": "4", "humidity": "38%"}
            ],
            "ch_temp": [
                {"channel": "1", "temp": "68.0", "unit": "F", "humidity": "50%"}
            ]
        })
    }

    fn val(rs: &[Reading], key: &str) -> Option<f64> {
        rs.iter().find(|r| r.key == key).map(|r| r.value)
    }

    #[test]
    fn parse_num_strips_units() {
        assert_eq!(parse_num("56%"), Some(56.0));
        assert_eq!(parse_num("3.13 mph"), Some(3.13));
        assert_eq!(parse_num("0.0 in/Hr"), Some(0.0));
        assert_eq!(parse_num("-4.2 F"), Some(-4.2));
        assert_eq!(parse_num("71.6"), Some(71.6));
        assert_eq!(parse_num("--"), None);
        assert_eq!(parse_num(""), None);
    }

    #[test]
    fn parses_soil_channels_to_push_keys() {
        let rs = parse_livedata(&sample(), "ecowitt_gw", 1000);
        assert_eq!(val(&rs, "soilmoisture1"), Some(45.0));
        assert_eq!(val(&rs, "soilmoisture2"), Some(38.0));
        // Same key scheme as the push ingest -> source:ecowitt_gw:soilmoisture1.
        assert!(rs
            .iter()
            .all(|r| r.source_id == "ecowitt_gw" && r.epoch == 1000));
    }

    #[test]
    fn parses_common_and_rain() {
        let rs = parse_livedata(&sample(), "gw", 1);
        assert_eq!(val(&rs, "tempf"), Some(71.6));
        assert_eq!(val(&rs, "humidity"), Some(56.0));
        assert_eq!(val(&rs, "dewpointf"), Some(54.9));
        assert_eq!(val(&rs, "windspeedmph"), Some(3.13));
        assert_eq!(val(&rs, "uv"), Some(6.0));
        assert_eq!(val(&rs, "dailyrainin"), Some(0.24));
        assert_eq!(val(&rs, "rainratein"), Some(0.0));
    }

    #[test]
    fn parses_wh31_temp_channel() {
        let rs = parse_livedata(&sample(), "gw", 1);
        assert_eq!(val(&rs, "temp1f"), Some(68.0));
        assert_eq!(val(&rs, "humidity1"), Some(50.0));
    }

    #[test]
    fn tolerates_missing_blocks() {
        // A firmware that only sends ch_soil must still parse it.
        let body = json!({ "ch_soil": [{"channel": "3", "humidity": "60%"}] });
        let rs = parse_livedata(&body, "gw", 1);
        assert_eq!(val(&rs, "soilmoisture3"), Some(60.0));
        assert_eq!(rs.len(), 1);
    }

    #[test]
    fn empty_body_yields_nothing() {
        let rs = parse_livedata(&json!({}), "gw", 1);
        assert!(rs.is_empty());
    }

    #[test]
    fn parses_real_ec_soil_gateway() {
        // Exact shape of a dedicated EC-soil gateway:
        // no common_list weather, a wh25 indoor block, and ch_ec probes.
        let body = json!({
            "common_list": [],
            "wh25": [{"intemp": "92.8", "unit": "F", "inhumi": "57%", "abs": "29.93 inHg", "rel": "29.93 inHg"}],
            "ch_ec": [
                {"channel": "1", "name": "Back Yard Soil", "battery": "5", "humidity": "32%", "temp": "79.2", "unit": "F", "ec": "40 uS/cm"},
                {"channel": "3", "name": "Side Yard Soil", "battery": "5", "humidity": "65%", "temp": "82.0", "unit": "F", "ec": "170 uS/cm"}
            ]
        });
        let rs = parse_livedata(&body, "ecowitt_gw", 1);
        // The load-bearing bit: EC probes surface as soilmoistureN, so a
        // zone's source:ecowitt_gw:soilmoisture1 resolves.
        assert_eq!(val(&rs, "soilmoisture1"), Some(32.0));
        assert_eq!(val(&rs, "soilmoisture3"), Some(65.0));
        // Soil temp + EC recorded too.
        assert_eq!(val(&rs, "soiltemp1f"), Some(79.2));
        assert_eq!(val(&rs, "soilec3"), Some(170.0));
        // Indoor block.
        assert_eq!(val(&rs, "tempinf"), Some(92.8));
        assert_eq!(val(&rs, "humidityin"), Some(57.0));
        assert_eq!(val(&rs, "baromabsin"), Some(29.93));
    }
}
