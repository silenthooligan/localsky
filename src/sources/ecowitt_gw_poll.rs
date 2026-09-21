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
// Both channel readings and outdoor weather publish through SourceBus.
// The bus recorder persists the native keys used by zone bindings, while
// the snapshot bridge arbitrates the outdoor weather fields.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, info, warn};

use crate::config::schema::EcowittGwPollConfig;
use crate::persistence::sensor_history::Reading;
use crate::ports::weather_source::{SourceBus, SourceEvent, WeatherField};
use crate::sources::poll::ReachabilityLatch;

/// Parse the leading numeric portion of an Ecowitt `val` string. The gateway
/// appends units and symbols ("56%", "3.13 mph", "0.0 in", "71.6"); we take
/// the leading sign/digits/decimal run and parse that.
fn parse_num(s: &str) -> Option<f64> {
    parse_num_with_unit(s).map(|(v, _)| v)
}

/// Like `parse_num` but also returns the trailing unit token (everything after
/// the number, degree-glyph-stripped + trimmed). The gateway reports values in
/// its configured display units and suffixes most of them ("3.13 mph",
/// "29.93 inHg", "5.0 mm"), so the suffix tells us whether to convert to
/// canonical imperial. `None` unit = no suffix (e.g. outdoor temp "22.0").
fn parse_num_with_unit(s: &str) -> Option<(f64, Option<String>)> {
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
    let value = head.parse::<f64>().ok()?;
    let rest = trimmed[end..].replace('\u{00b0}', "");
    let unit = rest.trim();
    let unit = (!unit.is_empty()).then(|| unit.to_string());
    Some((value, unit))
}

/// Map an Ecowitt sensor_history key to the WeatherField CLASS whose unit
/// conversion applies (temp/pressure/wind/rain). Returns None for keys that
/// are unitless or already canonical (soil moisture %, EC, humidity, UV,
/// solar, battery) so they pass through untouched.
fn ecowitt_field(key: &str) -> Option<WeatherField> {
    use WeatherField::*;
    if key == "dewpointf" || key.contains("temp") {
        Some(AirTempF)
    } else if key.starts_with("barom") {
        Some(PressureInHg)
    } else if key.contains("wind") || key == "maxdailygust" {
        Some(WindMph)
    } else if key.contains("rain") {
        Some(RainTodayIn)
    } else {
        None
    }
}

/// Convert an Ecowitt reading to canonical imperial given the gateway's
/// reported unit for that key. Non-convertible keys pass through.
fn convert(key: &str, value: f64, unit: Option<&str>) -> f64 {
    match ecowitt_field(key) {
        Some(field) => crate::sources::units::to_canonical(field, value, unit),
        None => value,
    }
}

/// Map an OUTDOOR weather sensor_history key to its global WeatherField for the
/// merge bus, so the snapshot bridge can populate the dashboard + HA weather
/// entities. Soil channels, indoor (tempinf/humidityin), and per-channel
/// (temp{ch}f/humidity{ch}) keys return None: they're history-only, not the
/// global current-conditions snapshot. Values are already canonical imperial.
fn weather_field_for_key(key: &str) -> Option<WeatherField> {
    use WeatherField::*;
    Some(match key {
        "tempf" => AirTempF,
        "dewpointf" => DewPointF,
        "humidity" => RhPct,
        "windspeedmph" => WindMph,
        "windgustmph" => WindGustMph,
        "baromabsin" => PressureInHg,
        "dailyrainin" => RainTodayIn,
        "rainratein" => RainIntensityInHr,
        "solarradiation" => SolarWm2,
        "uv" => UvIndex,
        "solarradiation_lux" => Illuminance,
        _ => return None,
    })
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

    // Gateway temperature display unit, from the wh25 block's `unit` field
    // ("F"/"C"). This is the gateway-global temp unit and the only signal for
    // the outdoor common_list temps (which carry no per-value suffix). Defaults
    // to imperial (None -> to_canonical passes through).
    let temp_unit: Option<String> = body
        .get("wh25")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|w| w.get("unit"))
        .and_then(Value::as_str)
        .map(|s| s.to_string());

    // common_list, outdoor temp / humidity / wind / solar / uv.
    if let Some(arr) = body.get("common_list").and_then(Value::as_array) {
        for item in arr {
            let (Some(id), Some(val)) = (
                item.get("id").and_then(Value::as_str),
                item.get("val").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let (Some(key), Some((v, val_unit))) = (common_key(id), parse_num_with_unit(val)) {
                // Wind carries its own suffix (mph/m·s⁻¹/km·h⁻¹); outdoor temp
                // has no suffix, so fall back to the gateway temp unit.
                let unit = val_unit.or_else(|| {
                    matches!(key, "tempf" | "dewpointf")
                        .then(|| temp_unit.clone())
                        .flatten()
                });
                push(key.to_string(), convert(key, v, unit.as_deref()));
            }
        }
    }

    // rain block, daily/rate/etc. (suffix tells us in vs mm).
    if let Some(arr) = body.get("rain").and_then(Value::as_array) {
        for item in arr {
            let (Some(id), Some(val)) = (
                item.get("id").and_then(Value::as_str),
                item.get("val").and_then(Value::as_str),
            ) else {
                continue;
            };
            if let (Some(key), Some((v, unit))) = (rain_key(id), parse_num_with_unit(val)) {
                push(key.to_string(), convert(key, v, unit.as_deref()));
            }
        }
    }

    // wh25, the gateway's own indoor temp / humidity / pressure block.
    if let Some(arr) = body.get("wh25").and_then(Value::as_array) {
        if let Some(item) = arr.first() {
            let wh25_unit = item.get("unit").and_then(Value::as_str);
            if let Some(v) = item
                .get("intemp")
                .and_then(Value::as_str)
                .and_then(parse_num)
            {
                push("tempinf".to_string(), convert("tempinf", v, wh25_unit));
            }
            if let Some(v) = item
                .get("inhumi")
                .and_then(Value::as_str)
                .and_then(parse_num)
            {
                push("humidityin".to_string(), v);
            }
            // Absolute pressure preferred; fall back to relative. The val
            // carries the unit suffix (inHg vs hPa).
            if let Some((v, u)) = item
                .get("abs")
                .or_else(|| item.get("rel"))
                .and_then(Value::as_str)
                .and_then(parse_num_with_unit)
            {
                push(
                    "baromabsin".to_string(),
                    convert("baromabsin", v, u.as_deref()),
                );
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
                    let key = format!("soiltemp{ch}f");
                    let v = convert(&key, v, item.get("unit").and_then(Value::as_str));
                    push(key, v);
                }
                if let Some(v) = item.get("ec").and_then(Value::as_str).and_then(parse_num) {
                    push(format!("soilec{ch}"), v);
                }
                if let Some(v) = item
                    .get("battery")
                    .and_then(Value::as_str)
                    .and_then(parse_num)
                {
                    // Ecowitt soil battery is a 0-5 level; publish as % (×20)
                    // so HA's battery device_class + the low-battery alert read
                    // in familiar units.
                    push(format!("soilbatt{ch}"), (v * 20.0).clamp(0.0, 100.0));
                }
            }
        }
    }

    // ch_temp / ch_aisle, WH31 temp+humidity channels. Best-effort extras.
    for block in ["ch_temp", "ch_aisle"] {
        if let Some(arr) = body.get(block).and_then(Value::as_array) {
            for item in arr {
                let Some(ch) = item.get("channel").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(v) = item.get("temp").and_then(Value::as_str).and_then(parse_num) {
                    let key = format!("temp{ch}f");
                    let v = convert(&key, v, item.get("unit").and_then(Value::as_str));
                    push(key, v);
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

/// Parse /get_cli_soilad into calibrated `soilmoistureN` readings using the
/// per-channel dry/wet AD endpoints. Channels with no calibration entry are
/// skipped (their livedata humidity value is kept). moisture% is clamped 0..100.
pub fn parse_soilad(
    body: &Value,
    source_id: &str,
    epoch: i64,
    calibration: &std::collections::BTreeMap<String, crate::config::schema::SoilAdCalibration>,
) -> Vec<Reading> {
    let mut out = Vec::new();
    let Some(arr) = body.as_array() else {
        return out;
    };
    for c in arr {
        let Some(ch) = c.get("ch").and_then(Value::as_str) else {
            continue;
        };
        let Some(cal) = calibration.get(ch) else {
            continue;
        };
        let Some(ad) = c.get("nowAd").and_then(Value::as_str).and_then(parse_num) else {
            continue;
        };
        let span = (cal.ad_wet - cal.ad_dry).abs().max(1.0);
        let pct = ((ad - cal.ad_dry) / span * 100.0).clamp(0.0, 100.0);
        out.push(Reading {
            epoch,
            source_id: source_id.to_string(),
            key: format!("soilmoisture{ch}"),
            value: pct,
        });
    }
    out
}

/// Replace only channels actually returned by the calibration endpoint.
/// Partial calibration must preserve uncalibrated probes and probes whose
/// raw AD was absent from this response.
fn apply_calibrated_readings(readings: &mut Vec<Reading>, calibrated: Vec<Reading>) {
    readings.retain(|r| !calibrated.iter().any(|c| c.key == r.key));
    readings.extend(calibrated);
}

/// Spawn the poll loop. Runs until the process exits: `spawn` receives no
/// `ShutdownSignal` (main.rs creates the source shutdown watch after these
/// spawns, same contract as the Tempest/forecast refreshers), so there is
/// nothing to select on. A `None` history store makes this a no-op (nothing
/// to write to).
///
/// This is a hand-rolled loop rather than `sources::poll::run_polling` for
/// one reason the primitive cannot spell: the gateway's embedded HTTP server
/// has brief busy windows (its own periodic cloud uploads), so a lone failed
/// cycle must keep the PRIOR reachability verdict, neither online nor
/// offline, and `Poll` only knows true/false. The pieces the primitive owns
/// are still shared: the reachability edges go through `ReachabilityLatch`
/// (transitions only, both directions, first verdict always reported) and
/// the per-cycle fetch is wrapped in `metrics::observe_fetch`.
/// The gateway's own HTTP endpoint, polled on the LAN. It carries the
/// per-zone soil probes, which is why it exists: those ride the bus as
/// keyed readings under the channel names a zone binds to
/// (`source:<id>:soilmoisture<N>`), and its outdoor weather rides as
/// ordinary fields.
pub struct EcowittGwPoll {
    id: String,
    config: EcowittGwPollConfig,
    priority: i32,
}

impl EcowittGwPoll {
    pub fn new(id: String, config: EcowittGwPollConfig, priority: i32) -> Self {
        Self {
            id,
            config,
            priority,
        }
    }
}

#[async_trait::async_trait]
impl crate::ports::weather_source::WeatherSource for EcowittGwPoll {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self) -> crate::ports::weather_source::SourceCaps {
        crate::ports::weather_source::SourceCaps {
            // A live poll of a real station on the LAN.
            live_current: true,
            // What `parse_livedata` actually produces. The per-zone soil
            // channels are not WeatherFields; they ride as keyed readings.
            fields: [
                WeatherField::AirTempF,
                WeatherField::DewPointF,
                WeatherField::RhPct,
                WeatherField::WindMph,
                WeatherField::WindGustMph,
                WeatherField::PressureInHg,
                WeatherField::RainTodayIn,
                WeatherField::RainIntensityInHr,
                WeatherField::SolarWm2,
                WeatherField::UvIndex,
                WeatherField::Illuminance,
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        }
    }

    fn priority(&self, _field: WeatherField) -> i32 {
        self.priority
    }

    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        mut shutdown: crate::ports::weather_source::ShutdownSignal,
    ) -> anyhow::Result<()> {
        let id = self.id.clone();
        let config = self.config.clone();
        let url = format!("http://{}/get_livedata_info", config.host);
        let soilad_url = format!("http://{}/get_cli_soilad", config.host);
        let interval = Duration::from_secs(config.poll_interval_s.max(5) as u64);
        let bus = Some(bus);
        {
            info!(source_id = %id, host = %config.host, interval_s = config.poll_interval_s,
                  "ecowitt_gw_poll started");
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut latch = ReachabilityLatch::new();
            let mut failed_cycles: u32 = 0;
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = shutdown.changed() => return Ok(()),
                }
                // Retry once in-cycle (a busy-window blip usually clears in 2s),
                // and treat only TWO consecutive failed cycles as a real outage:
                // a lone blip neither warns nor flips source health (it used to
                // flap unreachable/reachable pairs into the log every few
                // minutes). One fetch metric per cycle, after the retry.
                match crate::metrics::observe_fetch(&id, fetch_with_retry(&url)).await {
                    Ok(body) => {
                        failed_cycles = 0;
                        if let Some(bus) = &bus {
                            crate::sources::poll::report_diagnostic(bus, &id, None);
                        }
                        if report(&mut latch, bus.as_ref(), &id, true) {
                            info!(source_id = %id, "ecowitt_gw_poll reachable");
                        }
                        let epoch = chrono::Utc::now().timestamp();
                        let mut readings = parse_livedata(&body, &id, epoch);
                        // Native calibrated soil: when dry/wet endpoints are
                        // configured, read the raw AD and recompute moisture so it
                        // matches a calibrated source-of-truth, replacing the
                        // gateway's own % from livedata.
                        if !config.soil_calibration.is_empty() {
                            match fetch(&soilad_url).await {
                                Ok(soilad) => {
                                    let cal =
                                        parse_soilad(&soilad, &id, epoch, &config.soil_calibration);
                                    apply_calibrated_readings(&mut readings, cal);
                                }
                                Err(error) => {
                                    let failure = crate::diagnostics::from_anyhow(
                                        &error,
                                        "Ecowitt gateway soil calibration",
                                    );
                                    warn!(source_id = %id, %failure, "calibration read failed; retaining reported moisture");
                                    if let Some(bus) = &bus {
                                        crate::sources::poll::report_diagnostic(
                                            bus,
                                            &id,
                                            Some(failure),
                                        );
                                    }
                                }
                            }
                        }
                        if readings.is_empty() {
                            let failure = crate::failure::Failure::new(
                                crate::failure::FailureCode::MissingField,
                                "Ecowitt gateway livedata",
                            )
                            .with_field("readings");
                            if let Some(bus) = &bus {
                                crate::sources::poll::report_diagnostic(bus, &id, Some(failure));
                            }
                            continue;
                        }
                        // Publish the outdoor weather subset onto the merge bus so
                        // the snapshot bridge populates the dashboard + HA weather
                        // entities (soil/channel keys stay history-only). Values are
                        // already canonical imperial. live_current=true: it's a live
                        // local poll of a real station.
                        if let Some(bus) = &bus {
                            let fields: Vec<(WeatherField, f64)> = readings
                                .iter()
                                .filter_map(|r| weather_field_for_key(&r.key).map(|f| (f, r.value)))
                                .collect();
                            if !fields.is_empty() {
                                let _ = bus.send(SourceEvent::Observation {
                                    source_id: id.clone(),
                                    fields,
                                    at_epoch: epoch,
                                });
                            }
                        }
                        // Every reading, under the gateway's own channel name, so
                        // a zone bound to `source:<id>:soilmoisture2` keeps the
                        // key it was bound to. The bus recorder persists them.
                        if let Some(bus) = &bus {
                            for r in &readings {
                                let _ = bus.send(SourceEvent::KeyedReading {
                                    source_id: id.clone(),
                                    key: r.key.clone(),
                                    value: r.value,
                                    at_epoch: r.epoch,
                                });
                            }
                        }
                        debug!(source_id = %id, readings = readings.len(), "ecowitt_gw_poll published");
                    }
                    Err(e) => {
                        let e = crate::diagnostics::from_anyhow(&e, "Ecowitt gateway poll");
                        if let Some(bus) = &bus {
                            crate::sources::poll::report_diagnostic(bus, &id, Some(e.clone()));
                        }
                        failed_cycles = failed_cycles.saturating_add(1);
                        if failed_cycles == 1 {
                            // One failed cycle (after the retry): quiet. The next
                            // poll decides whether this is real.
                            debug!(source_id = %id, error = %e,
                               "ecowitt_gw_poll: poll cycle failed once; retrying next tick");
                        } else if report(&mut latch, bus.as_ref(), &id, false) {
                            warn!(source_id = %id, error = %e, failed_cycles,
                              "ecowitt_gw_poll unreachable");
                        }
                    }
                }
            }
        }
    }
}

/// One reachability verdict through the shared latch, which publishes only
/// the edges: a gateway that answers every poll says so once, not every
/// thirty seconds. Returns whether this verdict was an edge, so the caller
/// logs on the same terms.
fn report(
    latch: &mut ReachabilityLatch,
    bus: Option<&SourceBus>,
    source_id: &str,
    reachable: bool,
) -> bool {
    match bus {
        Some(bus) => latch.report(bus, source_id, reachable),
        None => latch.observe(reachable),
    }
}

/// `fetch`, retried once after a 2s pause. The gateway's embedded HTTP
/// server has brief busy windows, so a single failed attempt usually
/// succeeds on the second try.
async fn fetch_with_retry(url: &str) -> anyhow::Result<Value> {
    match fetch(url).await {
        Ok(body) => Ok(body),
        Err(first) => {
            let first = crate::diagnostics::from_anyhow(&first, "Ecowitt gateway livedata");
            tokio::time::sleep(Duration::from_secs(2)).await;
            match fetch(url).await {
                Ok(body) => {
                    tracing::debug!(%first, recovered = true, "Ecowitt gateway retry recovered");
                    Ok(body)
                }
                Err(error) => Err(crate::failure::Failure::attempts(
                    "Ecowitt gateway retry",
                    vec![
                        first,
                        crate::diagnostics::from_anyhow(&error, "Ecowitt gateway livedata"),
                    ],
                )
                .into()),
            }
        }
    }
}

/// Fetch the gateway's local-API JSON through an SSRF-hardened client.
///
/// Not `net::client`: the gateway host comes from config (`config.host`) and
/// this poller runs always-on in the background, so it routes outbound
/// through net::safe_fetch (defense in depth): the forbidden-target filter
/// rejects loopback/metadata/link-local/multicast, the resolved IP is pinned
/// (anti DNS-rebinding) and redirects are disabled. RFC1918/ULA stay allowed
/// because the GW1100/GW2000 lives on the LAN, so legitimate Ecowitt polling
/// is unaffected. The 8s budget matches the previous persistent client.
async fn fetch(url: &str) -> anyhow::Result<Value> {
    let (client, safe_url) =
        crate::net::safe_fetch::build_safe_client(url, Duration::from_secs(8)).await?;
    let resp = client.get(safe_url).send().await?.error_for_status()?;
    Ok(crate::net::safe_fetch::read_json_capped::<Value>(resp).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_soilad_calibrates_from_raw_ad() {
        use crate::config::schema::SoilAdCalibration;
        // Real /get_cli_soilad shape; ch1 AD 724 with HA's dry/wet 502/1442
        // must yield 23.6% (matches Home Assistant exactly).
        let body = json!([
            {"ch": "1", "name": "Back Yard Soil", "soilVal": "19", "nowAd": "724"},
            {"ch": "2", "name": "Front", "nowAd": "913"},
        ]);
        let mut cal = std::collections::BTreeMap::new();
        cal.insert(
            "1".to_string(),
            SoilAdCalibration {
                ad_dry: 502.0,
                ad_wet: 1442.0,
            },
        );
        let out = parse_soilad(&body, "ecowitt_gw", 100, &cal);
        assert_eq!(out.len(), 1, "only ch1 has a calibration entry");
        assert_eq!(out[0].key, "soilmoisture1");
        assert!(
            (out[0].value - 23.6).abs() < 0.1,
            "expected ~23.6%, got {}",
            out[0].value
        );
    }

    #[test]
    fn partial_calibration_keeps_every_other_probe() {
        let mut readings = parse_livedata(&sample(), "gw", 100);
        let calibrated = vec![Reading {
            source_id: "gw".into(),
            key: "soilmoisture1".into(),
            epoch: 100,
            value: 23.6,
        }];
        apply_calibrated_readings(&mut readings, calibrated);
        assert_eq!(val(&readings, "soilmoisture1"), Some(23.6));
        assert_eq!(val(&readings, "soilmoisture2"), Some(38.0));
        assert_eq!(
            readings.iter().filter(|r| r.key == "soilmoisture1").count(),
            1
        );
        let before = readings.len();
        apply_calibrated_readings(&mut readings, Vec::new());
        assert_eq!(readings.len(), before);
    }

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
    fn converts_metric_gateway_to_imperial() {
        // A gateway configured for metric: outdoor temp has no suffix (the unit
        // is taken from the wh25 block), wind/rain/pressure carry metric
        // suffixes, channel temps declare their own unit.
        let body = json!({
            "common_list": [
                {"id": "0x02", "val": "22.0"},
                {"id": "0x03", "val": "12.0"},
                {"id": "0x0B", "val": "10.0 km/h"}
            ],
            "rain": [
                {"id": "0x10", "val": "25.4 mm"}
            ],
            "wh25": [{"intemp": "20.0", "unit": "C", "inhumi": "57%", "abs": "1013.0 hPa"}],
            "ch_ec": [{"channel": "1", "humidity": "30%", "temp": "25.0", "unit": "C", "ec": "40 uS/cm"}]
        });
        let rs = parse_livedata(&body, "gw", 1);
        let approx = |key: &str, want: f64| {
            let g = val(&rs, key).unwrap_or_else(|| panic!("missing {key}"));
            assert!((g - want).abs() < 0.05, "{key}: {g} != {want}");
        };
        approx("tempf", 71.6); // 22 C  (from wh25 unit)
        approx("dewpointf", 53.6); // 12 C
        approx("windspeedmph", 6.21); // 10 km/h
        approx("dailyrainin", 1.0); // 25.4 mm
        approx("tempinf", 68.0); // 20 C
        approx("baromabsin", 29.91); // 1013 hPa
        approx("soiltemp1f", 77.0); // 25 C
                                    // Unitless channels untouched.
        assert_eq!(val(&rs, "soilmoisture1"), Some(30.0));
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
